//! Provides async methods for sending messages and application commands from a terminal
//! session.
//!
//! Implements the terminal, client, and associated data structures
//!

use std::io::{self, Write, Stdin, Stdout};
use std::net::SocketAddr;
use std::str::{FromStr, SplitWhitespace, from_utf8};

use bytes::{BufMut, Bytes, BytesMut, IntoBuf};
use futures::stream::{SplitSink, SplitStream};
use tokio::codec::{Decoder, Encoder, Framed};
use tokio::io::{stdout, stderr};
use tokio::prelude::*;

use message::{Message, MsgBuilder, MsgTyp};
use peer::{SockMapAccessor, PeerConfig, PeerMapAccessor, PeerVolume, UserMapAccessor};

fn into_bytes(peers: &Vec<SocketAddr>) -> Bytes {
    let peervec: Vec<String> = peers.iter().map(|socket| socket.to_string()).collect();
    Bytes::from(peervec.join(",").as_bytes())
}

#[derive(Debug)]
enum ParseState {
    Init,
    ParseLine,
    ParseCmd,
    ParseMsg,
}

#[derive(Debug)]
pub struct StdioSocket {
    stdin: Stdin,
    stdout: Stdout,
}

impl StdioSocket {
    pub fn new() -> StdioSocket {
        StdioSocket {
            stdin: io::stdin(),
            stdout: io::stdout(),
        }
    }
}

impl Read for StdioSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdin.read(buf)
    }
}

impl AsyncRead for StdioSocket {
    unsafe fn prepare_uninitialized_buffer(&self, _: &mut [u8]) -> bool {
        false
    }
}

impl Write for StdioSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdout.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }
}

impl AsyncWrite for StdioSocket {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

/// Terminal wraps stdin and stdout. Peers send messages into the other end of the peer2stdout mpsc
/// buffer. Commands and messages from stdin go to the Client, which then either processes the
/// command or sends a message to the addressed peers, according to the parsed stdin contents.
#[derive(Debug)]
pub struct TerminalCodec {
    typ: MsgTyp,
    to: Vec<SocketAddr>,
    bytebuf: BytesMut,
    state: ParseState,
    idx: usize,
    line_buf: BytesMut,
    msg_buf: BytesMut,
    sock_map: SockMapAccessor,
    peer_map: PeerMapAccessor,
    user_map: UserMapAccessor,
    builder: MsgBuilder,
}

/// Polls stdin for commands and messages. Commands are performed as side effects. Messages are sent
/// using a futures Stream.
impl TerminalCodec {
    pub fn new(username: Bytes,
               sock_map: SockMapAccessor,
               peer_map: PeerMapAccessor,
               user_map: UserMapAccessor) -> TerminalCodec {
        TerminalCodec {
            typ: MsgTyp::BC,
            to: vec![],
            bytebuf: BytesMut::with_capacity(1024),
            state: ParseState::Init,
            idx: 0,
            line_buf: BytesMut::with_capacity(1024),
            msg_buf: BytesMut::with_capacity(1024),
            sock_map,
            peer_map,
            user_map,
            builder: MsgBuilder::new(username),
        }
    }

    fn parse_userlist(&self, userlist: &mut SplitWhitespace) -> Result<Vec<SocketAddr>, String> {
        let mut addrs: Vec<SocketAddr> = vec![];
        let lock = self.user_map.lock().unwrap();
        let map = &lock.inner;
        for line in userlist {
            // This may be either a fully qualified socketaddress, or a username
            let an_addr = SocketAddr::from_str(line);
            let name = Bytes::from(line.as_bytes());
            if map.contains_key(&name) {
                addrs.push(map.get(&name).unwrap().clone());
            } else if an_addr.is_ok() {
                addrs.push(an_addr.unwrap());
            } else {
                return Err(format!("Peer {} cannot be parsed as a username or address", line));
            }
        }
        Ok(addrs)
    }

    fn parse_command(&mut self, cmdargs: &mut SplitWhitespace) -> Result<Option<(Message, Vec<SocketAddr>)>, String> {
        match &cmdargs.next().unwrap().to_lowercase()[..] {
            "/msg" => {
                let msgtyp = match cmdargs.next() {
                    None => return Err("Insufficient arguments".to_string()),
                    Some(msgtyp) => {
                        match &msgtyp.trim().to_lowercase()[..] {
                            "pm" | "private" | "private_message" => MsgTyp::PM,
                            "bc" | "broadcast" => MsgTyp::BC,
                            "bbc" | "blind_broadcast" => MsgTyp::BBC,
                            _ => return Err(format!("Unknown message type {}", msgtyp)),
                        }
                    }
                };
                self.typ = msgtyp;
                Ok(None)
            },
            "/volume" => {
                let setting = match cmdargs.next() {
                    None => return Err("Insufficient arguments".to_string()),
                    Some(vol) => {
                        match &vol.trim().to_lowercase()[..] {
                            "debug" | "loud" => PeerVolume::Loud,
                            "normal" | "default" => PeerVolume::Normal,
                            "quiet" => PeerVolume::Quiet,
                            "block" | "mute" => PeerVolume::Mute,
                            _ => return Err(format!("Unknown volume setting {}", vol)),
                        }
                    },
                };
                let peers = self.parse_userlist(cmdargs)?;
                {
                    let mut lock = self.peer_map.lock().unwrap();
                    let map = &mut lock.inner;
                    for addr in peers {
                        if !map.contains_key(&addr) {
                            map.insert(
                                addr,
                                Box::new(PeerConfig::new(addr))
                            );
                        }
                        map.get_mut(&addr).unwrap().vol = setting.clone();
                    }
                }
                Ok(None)
            },
            "/connect" => {
                let peers = self.parse_userlist(cmdargs)?;
                let builder = &mut self.builder;
                Ok(Some(
                    (builder.build(
                        MsgTyp::Connect,
                        into_bytes(&peers),
                        Bytes::new()),
                     peers)
                ))
            },
            "/to" => {
                self.to = self.parse_userlist(cmdargs)?;
                Ok(None)
            },
            "/quit" | "/exit" => {
                // Broadcast a disconnect message to let peers know we're quitting
                // only need to send to open connections
                let connected_peers = self.sock_map
                    .lock()
                    .unwrap()
                    .inner
                    .keys()
                    .map(|k| k.clone())
                    .collect();
                Ok(Some(
                    (self.builder.build(
                        MsgTyp::Disconnect,
                        Bytes::new(),
                        Bytes::new()),
                     connected_peers)
                ))
            },
            _ => Err(format!("Unknown command {:?}", cmdargs)),
        }
    }
}

/// Implement polling stdin/stdout as a Stream so that it occurs for the entire duration of the
/// executor.
impl Decoder for TerminalCodec {
    type Item = (Message, Vec<SocketAddr>);
    type Error = io::Error;

    /// Poll stdin for new input and send it out for the Client future to process, also
    /// drain peer2stdout if it has content.
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                ParseState::Init => {
                    match buf[self.idx..]
                        .windows(1)
                        .enumerate()
                        .find(|(_, byte)| byte == b"\n")
                        .map(|(idx, _)| idx)
                    {
                        None => {
                            self.idx = buf.len();
                            return Ok(None)
                        },
                        Some(idx) => {
                            self.line_buf.clear();
                            if self.line_buf.remaining_mut() <= idx {
                                self.line_buf.reserve(idx+1);
                            }
                            let line = buf.split_to(idx);
                            self.idx = 0;
                            self.line_buf.put(line);
                            self.state = ParseState::ParseLine;
                        },
                    };
                },
                ParseState::ParseLine => {
                    // If we have a newline, now check to see if the first line is a
                    // command. Commands are all strings, so try and read the line as a string. If
                    // that fails, its a message. If it succeeds, strip leading/trailing whitespace
                    // and then see if the first char is the command initiator '/' in honor of IRC.

                    let linebytes = Bytes::from(&self.line_buf[..]);
                    match from_utf8(&linebytes[..]) {
                        Ok(string) => {
                            let line = string.trim();
                            if line.starts_with("/") {
                                self.line_buf.clear();
                                self.line_buf.put(line.into_buf());
                                self.state = ParseState::ParseCmd;
                                // TODO: If we have an unfinished message --- what do we do? keep
                                // buffering it? That's what I'll do for now
                            } else {
                                self.msg_buf.put(line.into_buf());
                                self.msg_buf.put("\n".into_buf());
                                self.state = ParseState::ParseMsg;
                            }
                        },
                        Err(_) => {
                            // Allow non-unicode content in a message
                            self.msg_buf.put(&self.line_buf[..]);
                            // GO back to looking for content -- need a cmd or three newlines to
                            // release a frame.
                            self.state = ParseState::Init;
                        }
                    };
                },
                ParseState::ParseMsg => {
                    let len = self.msg_buf.len();
                    if len >= 3 && &self.msg_buf[len - 3 ..] == b"\n\n\n" {
                        // Message terminated, time to send it.
                        self.msg_buf.truncate(len - 3); // remove the triple newline control sequence
                        let msg = self.builder.build(
                            self.typ.clone(),
                            into_bytes(&self.to),
                            self.msg_buf.clone().freeze(),
                        );
                        self.msg_buf.clear();
                        self.state = ParseState::Init;
                        return Ok(Some((msg, self.to.clone())));
                    } else {
                        // Message is not terminated, go back to finding lines
                        self.state = ParseState::Init;
                    }
                },
                ParseState::ParseCmd => {
                    // can unwrap because this is only entered via ParseLine, which checks that the
                    // buf is valid unicode.
                    let linebytes = Bytes::from(&self.line_buf[..]);
                    let line = from_utf8(&linebytes[..]).unwrap();
                    let mut cmdargs = line.split_whitespace();
                    // Since ParseLine checks for a leading '/', unwrap will work
                    self.line_buf.clear();
                    self.state = ParseState::Init;
                    let result = self.parse_command(&mut cmdargs);
                    match result {
                        Ok(Some((_msg, _recipients))) => {
                            stdout().poll_write(b"Command accepted")?;
                        },
                        Ok(None) => {}
                        Err(err) => {
                            stderr().poll_write(format!("Command failed: {}", err).as_bytes())?;
                        }
                    }
                },
            }
        };
    }
}

impl Encoder for TerminalCodec {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut volume = PeerVolume::Normal;
        if let Some(addr) = self.user_map.lock().unwrap().inner.get(&item.from) {
            volume = self.peer_map.lock().unwrap().inner.get(addr).unwrap().vol.clone();
        }
        match volume {
            PeerVolume::Loud => {
                dst.put(format!("{}", item).as_bytes());
                match item.typ {
                    MsgTyp::ACK | MsgTyp::NACK(_) => {
                        let msgnum = item.content[0] as u8;
                        match self.builder.ping_map.get(&msgnum) {
                            None => {
                                dst.put(format!("Unknown roundtrip msgtime for msgnum {}, there is
                                                 no ping_map entry for that message.", msgnum).as_bytes());
                            },
                            Some(time) => {
                                match time.elapsed() {
                                    Ok(elapsed) => {
                                        dst.put(format!("Roundtrip time for msgnum {} = {:?}.",
                                                        msgnum, elapsed).as_bytes());
                                    },
                                    Err(e) => {
                                        dst.put(format!("Error computing roundtrip time for msgnum
                                                         {}: Er: {:?}.", msgnum, e).as_bytes());
                                    },
                                };
                            },
                        };
                    },
                    _ => {},
                };
            },
            PeerVolume::Normal => {
                match item.typ {
                    MsgTyp::PM | MsgTyp::BC | MsgTyp::BBC => {
                        dst.put(format!("{}", item).as_bytes());
                    },
                    _ => {}
                };
            },
            PeerVolume::Quiet => {
                match item.typ {
                    MsgTyp::PM => {
                        dst.put(format!("{}", item).as_bytes());
                    },
                    _ => {}
                }
            },
            PeerVolume::Mute => {},
        };
        Ok(())
    }
}


pub struct TermClient {
    msg: Message,
    pub sink: SplitSink<Framed<StdioSocket, TerminalCodec>>,
    pub stream: SplitStream<Framed<StdioSocket, TerminalCodec>>,
}

impl TermClient {
    pub fn new(user: Bytes,
               sock_map: SockMapAccessor,
               peer_map: PeerMapAccessor,
               user_map: UserMapAccessor,
               msg: Message) -> TermClient {
        let (sink, stream) = TerminalCodec::new(user.clone(), sock_map, peer_map, user_map)
            .framed(StdioSocket::new())
            .split();
        TermClient {
            msg,
            sink,
            stream,
        }
    }
}

impl Future for TermClient{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), Self::Error> {
        // Wait for our connection to finish initializing
        match self.sink.start_send(self.msg.clone()) {
            Err(_) => return Err(()),
            _ => {},
        };
        match self.sink.poll_complete() {
            Ok(val) => Ok(val),
            Err(_) => Err(())
        }
    }
}
