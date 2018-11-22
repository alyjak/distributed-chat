//! Defines IO with a chat peer.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::{BufMut, Bytes, BytesMut};

use futures::sync::mpsc;
use tokio::codec::Framed;
use tokio::net::tcp::ConnectFuture;
use tokio::net::TcpStream;
use tokio::prelude::{task, Async, Future, Poll, Stream};

use message::{BytesRx, BytesTx, Message, MsgRx, MsgTx};

#[derive(Debug)]
struct SocketMap(HashMap<SocketAddr, Box<PeerChannel>>);

pub type SocketMapAccessor = Arc<Mutex<SocketMap>>;

/// Fills and Drains a TCP socket
pub struct PeerChannel {
    socket: TcpStream,
    channel2socket: Framed<TcpStream, Message>,
    peer2terminal: MsgTx,
    terminal2peer: MsgRx,
    username: Bytes,
}

impl PeerState {
    fn from_stream(username: BytesMut, ignore: bool, socket: TcpStream) -> Self {
        let (tx, rx) = mpsc::unbounded();
        PeerState {
            username,
            ignore,
            socket: ConnectionState::Connected(socket),
            address: socket.peer_addr().unwrap(),
            client2peer: rx,
            client2peerbuf: BytesMut::new(),
            peer2clientbuf: BytesMut::new(),
        }
    }

    fn from_addr(username: BytesMut, ignore: bool, addr: SocketAddr) -> Self {
        let (tx, rx) = mpsc::unbounded();
        PeerState {
            username,
            ignore,
            socket: ConnectionState::Disconnected,
            address: addr,
            client2peer: rx,
            client2peerbuf: BytesMut::new(),
            peer2clientbuf: BytesMut::new(),
        }
    }
}

impl Future for PeerState {
    type Item = ();
    type Error = io::Error;

    /// Transmit any queued data to our peer. Additionally, acquire any incoming messages from our
    /// peer.
    ///
    /// TODO: drop connection, CoDec to messages, ACK protocol, don't tx to stdout directly --
    ///       submit to client unbound buffer (wrapped in an Arc(Mutex())).
    ///
    fn poll(&mut self) -> Poll<(), Self::Error> {
        // Comment taken from tokio examples:
        //   Tokio (and futures) use cooperative scheduling without any preemption. If a task never
        //   yields execution back to the executor, then other tasks may be starved.
        //
        //   To deal with this, robust applications should not have any unbounded loops. In this
        //   example, we will read at most `LINES_PER_TICK` lines from the client on each tick.
        const LINES_PER_TICK: usize = 10;

        // Grab data from our local client thread
        for i in 0..LINES_PER_TICK {
            if i + 1 == LINES_PER_TICK {
                task::current().notify();
            }
            match self.client2peer.poll().unwrap() {
                Async::Ready(Some(v)) => {
                    self.client2peerbuf.reserve(v.len());
                    self.client2peerbuf.put(v);
                }
                _ => break,
            }
        }

        // If we have data to transmit to the peer, ensure we have an open socket
        let socket = match self.socket {
            ConnectionState::Disconnected => {
                // There's need to connect if we dont' have anything to say to the peer
                if self.client2peerbuf.is_empty() {
                    return Ok(Async::Ready(()));
                }
                self.socket = ConnectionState::Connecting(TcpStream::connect(&self.address));
                return Ok(Async::NotReady);
            }
            ConnectionState::Connecting(connectfuture) => match connectfuture.poll()? {
                Async::Ready(socket) => {
                    self.socket = ConnectionState::Connected(socket);
                    socket
                }
                Async::NotReady => return Ok(Async::NotReady),
            },
            ConnectionState::Connected(socket) => socket,
        };

        // Drain the transmit buffer into the peer's socket
        while !self.client2peerbuf.is_empty() {
            let n = try_ready!(socket.poll_write(&self.client2peerbuf));

            // As long as txbuf is not empty a successful write should never write 0 bytes.
            assert!(n > 0);

            let _ = self.client2peerbuf.split_to(n);
        }

        // decode any received data
        loop {
            self.peer2clientbuf.reserve(1024);
            let n = try_ready!(socket.read_buf(&mut self.peer2clientbuf));
            if n == 0 {
                println!("[{:?}]: {:?}", self.username, self.peer2clientbuf);
                self.peer2clientbuf.clear();
                return Ok(Async::Ready(()));
            }
        }
    }
}

// TODO: what causes drop to be automatically called? Clients should remember who they ignored
// ignore shoudl probably be in the client structure
impl Drop for PeerState {
    fn drop(&mut self) {
        self.connections.lock().unwrap().remove(&self.address);
    }
}
