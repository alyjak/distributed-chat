//! Defines IO with a chat peer.
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::stream::{SplitSink, SplitStream};
use tokio;
use tokio::codec::{Framed, Decoder};
use tokio::net::TcpStream;
use tokio::prelude::{Async, Future, Poll, Stream};
use tokio::io::{stderr, AsyncWrite};

use message::{Message, MsgClient, MsgCodec, MsgTyp, NACKKind};
use terminal::TermClient;

#[derive(Debug)]
pub struct SockMap {
    pub inner: HashMap<SocketAddr, Box<SplitSink<Framed<TcpStream, MsgCodec>>>>,
}

/// PeerMap is a map in the Client that holds channels to the peers and I/O preferences the client
/// holds for each known peer.
#[derive(Debug)]
pub struct PeerMap{
    pub inner: HashMap<SocketAddr, Box<PeerConfig>>
}

/// UsernameMap holds peer usernames in order to map them to their addresses
#[derive(Debug)]
pub struct UserMap {
    pub inner: HashMap<Bytes, SocketAddr>
}

pub type SockMapAccessor = Arc<Mutex<SockMap>>;
pub type PeerMapAccessor = Arc<Mutex<PeerMap>>;
pub type UserMapAccessor = Arc<Mutex<UserMap>>;

#[derive(Clone, Debug, PartialEq)]
pub enum PeerVolume {
    /// Write out all messages, including ACK, NACK, etc.
    Loud,
    /// Write out all broadcast and directed messages.
    Normal,
    /// Only write directed messages
    Quiet,
    /// Do not write any messages from this peer.
    Mute,
}

/// PeerConfig stores client preferences about a peer.
#[derive(Debug)]
pub struct PeerConfig {
    /// Remote address for the peer.
    pub addr: SocketAddr,
    /// Active username for this address
    pub username: Bytes,
    /// The clients configured verbosity for this peer
    pub vol: PeerVolume,
}

impl PeerConfig {
    pub fn new(addr: SocketAddr) -> PeerConfig {
        PeerConfig {
            addr,
            username: Bytes::new(), // Empty name by default
            vol: PeerVolume::Normal,
        }
    }
}

/// Drains a TCP socket, sets up a sink to transmit with the socket as well.
#[derive(Debug)]
pub struct PeerChannel {
    pub stream: SplitStream<Framed<TcpStream, MsgCodec>>,
    /// Needed to find our configuration in the peer_map
    addr: SocketAddr,
    /// Needed in order to insert client name into To field on ACK/NACKs
    client: Bytes,
    peer_map: PeerMapAccessor,
    sock_map: SockMapAccessor,
    /// Once we find the username an address is using, we update user_map accordingly
    user_map: UserMapAccessor,
}

impl PeerChannel {
    pub fn new(sock: TcpStream,
               addr: SocketAddr,
               client: Bytes,
               peer_map: PeerMapAccessor,
               sock_map: SockMapAccessor,
               user_map: UserMapAccessor) -> PeerChannel {
        let (sink, stream) = MsgCodec::new(addr.clone()).framed(sock).split();
        let mutex = sock_map.clone();
        let mut lock = mutex.lock().unwrap();
        let s_map = &mut lock.inner;
        s_map.insert(addr, Box::new(sink));
        PeerChannel {
            stream,
            addr,
            client,
            peer_map,
            sock_map,
            user_map,
        }
    }
}

impl Drop for PeerChannel {
    fn drop(&mut self) {
        let mut lock = self.sock_map.lock().unwrap();
        let map = &mut lock.inner;
        map.remove(&self.addr);
    }
}

impl Future for PeerChannel {
    type Item = ();
    type Error = io::Error;

    /// Drain any queued messages from our peer.
    fn poll(&mut self) -> Poll<(), Self::Error> {
        let stream = &mut self.stream;
        loop {
            let _ = match stream.poll() {
                Ok(Async::Ready(Some(msg_or_err))) => handle_message(
                    msg_or_err,
                    self.client.clone(),
                    self.addr.clone(),
                    self.peer_map.clone(),
                    self.sock_map.clone(),
                    self.user_map.clone()),
                Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "error polling socket")),
            };
        }
    }
}

fn handle_message(msg_or_err: Result<Message, String>,
                  client: Bytes,
                  addr: SocketAddr,
                  peer_map: PeerMapAccessor,
                  sock_map: SockMapAccessor,
                  user_map: UserMapAccessor) -> Result<(), io::Error> {
    // Since ACKs don't get ack'd, there's no point in having msgnums, therefore ACKs are not built
    // with the builder, so their msgnums are all set to 0.
    let mut ack = Message {
        typ: MsgTyp::ACK,
        msgnum: 0_u8,
        from: client.clone(),
        to: Bytes::from(addr.clone().to_string().into_bytes()),
        content: Bytes::from(&[0_u8][..]),
        crc: 0u32,
    };
    match msg_or_err {
        Ok(msg) => {
            ack.content = Bytes::from(&[msg.msgnum][..]);
            let mut update_usermap = false;
            {
                let mut peer_lock = peer_map.lock().unwrap();
                // PeerChannel is created in main, which ensures peer_map has an entry for the
                // addr before initialization of the peerchannel
                let p_map = &mut peer_lock.inner;
                let peer_conf = p_map.get_mut(&addr).unwrap();
                if peer_conf.vol == PeerVolume::Mute {
                    ack.typ = MsgTyp::NACK(NACKKind::Blocked);
                } else {
                    let term = TermClient::new(
                        client.clone(), sock_map.clone(), peer_map.clone(), user_map.clone(), msg.clone()
                    );
                    tokio::spawn(term);
                }

                // Check if the user map needs updating
                //
                // TODO: This is a bad api -- this is going to try and lock and view the map
                // for every message. Also, peers can change their usernames such that an
                // address gets a new user, or there can be a username clash between two
                // addresses.
                if peer_conf.username != msg.from {
                    peer_conf.username = msg.from.clone();
                    update_usermap = true;
                }
            }
            if update_usermap {
                let mut u_lock = user_map.lock().unwrap();
                let u_map = &mut u_lock.inner;
                u_map.insert(msg.from.clone(), addr.clone());
            }
            match msg.typ {
                // Don't ACK ACKs or Disconnects
                MsgTyp::ACK | MsgTyp::NACK(_) | MsgTyp::Disconnect => {},
                _ => {
                    let (_, crc) = ack.raw();
                    ack.crc = crc;
                    let peer = MsgClient::new(ack, addr.clone(), sock_map.clone());
                    tokio::spawn(peer);
                }
            };
        },
        Err(message) => {
            ack.typ = MsgTyp::NACK(NACKKind::ParseError);
            let (_, crc) = ack.raw();
            ack.crc = crc;
            let peer = MsgClient::new(ack, addr.clone(), sock_map.clone());
            tokio::spawn(peer);
            stderr().poll_write(
                format!("Error parsing message from {}. Error: {}",
                        addr, message).as_bytes()
            )?;
        }
    };
    Ok(())
}
