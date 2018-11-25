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

use LINES_PER_TICK;
use message::{BytesRx, BytesTx, Message, MsgRx, MsgTx};

#[derive(Debug)]
struct SocketMap(HashMap<SocketAddr, Box<PeerChannel>>);

pub type SocketMapAccessor = Arc<Mutex<SocketMap>>;


/// Fills and Drains a TCP socket
pub struct PeerChannel {
    net_framer: Framed<TcpStream, MsgCodec>,
    /// Needed to find our configuration in the peer_map
    addr: SocketAddr,
    /// Needed in order to insert client name into To field on ACK/NACKs
    client: Bytes,
    peer_map: PeerMapAccessor,
    /// Once we find the username an address is using, we update user_map accordingly
    user_map: UserMapAccessor,
    term_framer: Framed<StdioSocket, TerminalCodec>
}

impl PeerChannel {
    fn new(sock: TcpStream,
           addr: SocketAddr,
           client: Bytes,
           peer_map: PeerMapAccessor,
           user_map: UserMapAccessork) -> PeerChannel {
        PeerChannel {
            net_framer: MsgCodec::new(addr).framed(socket),
            addr,
            client,
            peer_map,
            user_map,
            term_framer: TerminalCodec::new(peer_map.clone()).framed(StdioSocket::new())
        }
    }
}

impl Future for PeerChannel {
    type Item = ();
    type Error = io::Error;

    /// Drain any queued messages from our peer.
    fn poll(&mut self) -> Poll<(), Self::Error> {
        let (writer, reader) = self.net_framer.split();
        let drain = reader.for_each(|msg_or_err| {
            let mut ack = builder.build(
                typ: MsgTyp::ACK,
                to: self.addr,
                content: Bytes::from(&[0][..]),
            );
            match msg_or_err {
                Either::A(msg) => {
                    // PeerChannel is created in main, which ensures peer_map has an entry for the
                    // addr before initialization of the peerchannel
                    ack.content = Bytes::from(msg.msgnum);
                    let is_blocked = self.peer_map
                        .lock()
                        .unwrap()
                        .get(addr)
                        .unwrap()
                        .volume == PeerVolume::Block;
                    if is_blocked {
                        ack.typ = NACK(NACKKind::UserBlocked);
                    } else {
                        let _ = self.term_framer.poll_write(msg)?;
                    }
                    match msg.typ {
                        // Don't ACK, ack messages
                        MsgTyp::ACK | MsgTyp::NACK(_) => {},
                        _ => {
                            let (_, crc) = ack.assemble();
                            ack.crc = crc;
                            let _ = self.net_framer.poll_write(ack)?;
                        }
                    };
                },
                Either::B(_) => {
                    ack.typ = NACK(NACKKind::ParseError);
                    let (_, crc) = ack.assemble();
                    ack.crc = crc;
                    let _ = self.net_framer.poll_write(ack)?;
                }
            };
        });
        tokio::spawn(drain);
        Ok(())
    }
}

impl Drop for PeerChannel {
    fn drop(&mut self) {
        self.peer_map.lock().unwrap().remove(&self.addr);
    }
}
