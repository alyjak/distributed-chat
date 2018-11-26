//! Provides async methods for sending messages and application commands from a terminal
//! session.
//!
//! Implements the terminal, client, and associated data structures
//!

use std::io::{self, Stdin, Stdout, Write};
use std::net::SocketAddr;
use std::str::{from_utf8, FromStr, SplitWhitespace};

use bytes::{BufMut, Bytes, BytesMut, IntoBuf};
use futures::stream::{SplitSink, SplitStream};
use tokio::codec::{Decoder, Encoder, Framed};
use tokio::io::{stderr, stdout};
use tokio::prelude::*;

use message::{Message, MsgTyp};
use peer::{
    BuilderAccessor, PeerConfig, PeerMapAccessor, PeerVolume, SockMapAccessor, UserMapAccessor,
};

pub const HELP: &str = "
Usage:

Sending messages:
  Three return presses sends a message, this allows you to compose
  paragraphs before sending. ([defect], the terminal UI makes it so you can't
  edit them though).

Commands:
  /connect <list of addresses>
    Open a connection to each address and send them a connection message.
  /msg [pm|private|private_message, bc|broadcast, bbc|blind_broadcast]
    The next message will be sent to either a configured private message
    group or broadcast. Blind broadcast doesn't publish the set of recipients,
    while normal broadcast does. Default is broadcast\n
  /to <list of usernames and/or addresses>
    Configures who private messages will be sent to. Default is an empty list.
  /volume [debug|loud, normal|default, quiet, block|mute] <usernames and/or addresses>
    Configures what messages to print from list of addresses/usernames
    - loud: prints every message received.
    - normal: prints broadcast, private, and connection messages.
    - quiet: prints private messages only
    - mute: blocks all messages and sends a NACK to the sender to let them know\n
  /whos_here?
    Print out a list of connected peers.
  /quit
    Send disconnect messages to all open connections and terminate the application.
  /help
    Print this help message.
";

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
    builder_mut: BuilderAccessor,
}

/// Polls stdin for commands and messages. Commands are performed as side effects. Messages are sent
/// using a futures Stream.
impl TerminalCodec {
    pub fn new(
        sock_map: SockMapAccessor,
        peer_map: PeerMapAccessor,
        user_map: UserMapAccessor,
        builder_mut: BuilderAccessor,
    ) -> TerminalCodec {
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
            builder_mut,
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
                return Err(format!(
                    "Peer {} cannot be parsed as a username or address",
                    line
                ));
            }
        }
        Ok(addrs)
    }

    fn parse_command(
        &mut self,
        cmdargs: &mut SplitWhitespace,
    ) -> Result<Option<(Message, Vec<SocketAddr>)>, String> {
        match &cmdargs.next().unwrap().to_lowercase()[..] {
            "/connect" => {
                let peers = self.parse_userlist(cmdargs)?;
                let mut builder = self.builder_mut.lock().unwrap();
                Ok(Some((
                    builder.build(MsgTyp::Connect, into_bytes(&peers), Bytes::new()),
                    peers,
                )))
            }
            "/msg" => {
                let msgtyp = match cmdargs.next() {
                    None => return Err("Insufficient arguments".to_string()),
                    Some(msgtyp) => match &msgtyp.trim().to_lowercase()[..] {
                        "pm" | "private" | "private_message" => MsgTyp::PM,
                        "bc" | "broadcast" => MsgTyp::BC,
                        "bbc" | "blind_broadcast" => MsgTyp::BBC,
                        _ => return Err(format!("Unknown message type {}", msgtyp)),
                    },
                };
                self.typ = msgtyp;
                Ok(None)
            }
            "/to" => {
                self.to = self.parse_userlist(cmdargs)?;
                Ok(None)
            }
            "/volume" => {
                let setting = match cmdargs.next() {
                    None => return Err("Insufficient arguments".to_string()),
                    Some(vol) => match &vol.trim().to_lowercase()[..] {
                        "debug" | "loud" => PeerVolume::Loud,
                        "normal" | "default" => PeerVolume::Normal,
                        "quiet" => PeerVolume::Quiet,
                        "block" | "mute" => PeerVolume::Mute,
                        _ => return Err(format!("Unknown volume setting {}", vol)),
                    },
                };
                let peers = self.parse_userlist(cmdargs)?;
                {
                    let mut lock = self.peer_map.lock().unwrap();
                    let map = &mut lock.inner;
                    for addr in peers {
                        if !map.contains_key(&addr) {
                            map.insert(addr, Box::new(PeerConfig::new(addr)));
                        }
                        map.get_mut(&addr).unwrap().vol = setting.clone();
                    }
                }
                Ok(None)
            }
            "/whos_here?" | "/here?" | "/whos_here" | "/here" => {
                let s_lock = self.sock_map.lock().unwrap();
                let s_map = &s_lock.inner;
                let p_lock = self.peer_map.lock().unwrap();
                let p_map = &p_lock.inner;
                let peervec: Vec<String> = s_map
                    .keys()
                    .map(|key| {
                        let b_username = match &p_map.get(key) {
                            None => Bytes::from(&"unknown: dangling connection"[..]),
                            Some(conf) => conf.username.clone(),
                        };
                        let username = match from_utf8(&b_username) {
                            Err(_) => "unknown: not unicode",
                            Ok(name) => name,
                        };
                        format!("{}@{}", username, key)
                    }).collect();
                let mut out = stdout();
                out.write_buf(
                    &mut format!("Connected Peers:\n\t{}\n\n", peervec.join("\n\t")).into_buf(),
                ).map_err(|e| format!("{:?}", e))?;
                Ok(None)
            }
            "/quit" | "/exit" => {
                // Broadcast a disconnect message to let peers know we're quitting
                // only need to send to open connections
                let s_lock = self.sock_map.lock().unwrap();
                let s_map = &s_lock.inner;
                let connected_peers = s_map.keys().map(|k| k.clone()).collect();
                let mut builder = self.builder_mut.lock().unwrap();
                Ok(Some((
                    builder.build(MsgTyp::Disconnect, Bytes::new(), Bytes::new()),
                    connected_peers,
                )))
            }
            "/help" => {
                let mut out = stdout();
                out.write_buf(&mut format!("{}", HELP).into_buf())
                    .map_err(|e| format!("{:?}", e))?;
                out.poll_flush().map_err(|e| format!("{:?}", e))?;
                Ok(None)
            }
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
                        .map(|(idx, _)| idx + 1)
                    {
                        None => {
                            self.idx = buf.len();
                            return Ok(None);
                        }
                        Some(idx) => {
                            self.line_buf.clear();
                            if self.line_buf.remaining_mut() <= idx {
                                self.line_buf.reserve(idx);
                            }
                            let line = buf.split_to(idx);

                            self.idx = 0;
                            self.line_buf.put(line);
                            self.state = ParseState::ParseLine;
                        }
                    };
                }
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
                        }
                        Err(_) => {
                            // Allow non-unicode content in a message
                            self.msg_buf.put(&self.line_buf[..]);
                            // GO back to looking for content -- need a cmd or three newlines to
                            // release a frame.
                            self.state = ParseState::Init;
                        }
                    };
                }
                ParseState::ParseMsg => {
                    let len = self.msg_buf.len();
                    if len >= 3 && &self.msg_buf[len - 3..] == b"\n\n\n" {
                        // Message terminated, time to send it.
                        self.msg_buf.truncate(len - 3); // remove the triple newline control sequence

                        let recipients = match self.typ {
                            MsgTyp::BC | MsgTyp::BBC => {
                                // Broadcast to all connected peers
                                let s_lock = self.sock_map.lock().unwrap();
                                let s_map = &s_lock.inner;
                                s_map.keys().map(|k| k.clone()).collect()
                            }
                            _ => self.to.clone(),
                        };

                        let mut builder = self.builder_mut.lock().unwrap();
                        let msg = builder.build(
                            self.typ.clone(),
                            into_bytes(&recipients),
                            self.msg_buf.clone().freeze(),
                        );
                        self.msg_buf.clear();
                        self.state = ParseState::Init;
                        let mut out = stdout();
                        out.write_buf(&mut format!("\rTransmitting {}\n> ", self.typ).into_buf())?;
                        out.poll_flush()?;
                        return Ok(Some((msg, recipients)));
                    } else {
                        // Message is not terminated, go back to finding lines
                        // print a "we're in the middle of a message" caret
                        let mut out = stdout();
                        out.write_buf(&mut format!("+ ").into_buf())?;
                        out.poll_flush()?;

                        self.state = ParseState::Init;
                    }
                }
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
                        Ok(val) => {
                            let mut out = stdout();
                            out.write_buf(&mut format!("\rok\n> ").into_buf())?;
                            out.poll_flush()?;
                            if let Some((msg, recipients)) = val {
                                return Ok(Some((msg, recipients)));
                            };
                        }
                        Err(err) => {
                            let mut out = stderr();
                            out.write_buf(
                                &mut format!("\rCommand failed: {}\n> ", err).into_buf(),
                            )?;
                            out.poll_flush()?;
                        }
                    }
                }
            }
        }
    }
}

impl Encoder for TerminalCodec {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let volume = {
            let u_lock = self.user_map.lock().unwrap();
            let u_map = &u_lock.inner;
            match u_map.get(&item.from) {
                None => PeerVolume::Normal,
                Some(addr) => {
                    let p_lock = self.peer_map.lock().unwrap();
                    let p_map = &p_lock.inner;
                    match p_map.get(&addr) {
                        None => PeerVolume::Normal,
                        Some(ref conf) => conf.vol.clone(),
                    }
                }
            }
        };
        match volume {
            PeerVolume::Loud => {
                dst.put(format!("\r{}\n", item).as_bytes());
                match item.typ {
                    MsgTyp::ACK | MsgTyp::NACK(_) => {
                        let msgnum = item.content[0] as u8;
                        let builder = self.builder_mut.lock().unwrap();
                        match builder.ping_map.get(&msgnum) {
                            None => {
                                dst.put(
                                    format!(
                                        "Unknown roundtrip msgtime for msgnum {}, there is \
                                         no ping_map entry for that message.\n",
                                        msgnum
                                    ).as_bytes(),
                                );
                            }
                            Some(time) => {
                                match time.elapsed() {
                                    Ok(elapsed) => {
                                        dst.put(
                                            format!(
                                                "Roundtrip time for msgnum {} was {:?}.\n",
                                                msgnum, elapsed
                                            ).as_bytes(),
                                        );
                                    }
                                    Err(e) => {
                                        dst.put(
                                            format!(
                                                "Error computing roundtrip time for msgnum \
                                                 {}: Er: {:?}.\n",
                                                msgnum, e
                                            ).as_bytes(),
                                        );
                                    }
                                };
                            }
                        };
                    }
                    _ => {}
                };
            }
            PeerVolume::Normal => {
                match item.typ {
                    MsgTyp::PM
                    | MsgTyp::BC
                    | MsgTyp::BBC
                    | MsgTyp::Connect
                    | MsgTyp::Disconnect => {
                        dst.put(format!("{}\n", item).as_bytes());
                    }
                    _ => {}
                };
            }
            PeerVolume::Quiet => match item.typ {
                MsgTyp::PM => {
                    dst.put(format!("{}\n", item).as_bytes());
                }
                _ => {}
            },
            PeerVolume::Mute => {}
        };
        let mut out = stdout();
        // TODO: do I want this do while loop? it will steal the thread until its done. The other
        // option is to submit a future to complete the flush.
        while let Async::NotReady = out.poll_flush()? {}
        Ok(())
    }
}

pub struct TermClient {
    msg: Message,
    pub sink: SplitSink<Framed<StdioSocket, TerminalCodec>>,
    pub stream: SplitStream<Framed<StdioSocket, TerminalCodec>>,
}

impl TermClient {
    pub fn new(
        sock_map: SockMapAccessor,
        peer_map: PeerMapAccessor,
        user_map: UserMapAccessor,
        builder_mut: BuilderAccessor,
        msg: Message,
    ) -> TermClient {
        let (sink, stream) = TerminalCodec::new(sock_map, peer_map, user_map, builder_mut)
            .framed(StdioSocket::new())
            .split();
        TermClient { msg, sink, stream }
    }
}

impl Future for TermClient {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), Self::Error> {
        // Wait for our connection to finish initializing
        match self.sink.start_send(self.msg.clone()) {
            Err(_) => return Err(()),
            _ => {}
        };
        match self.sink.poll_complete() {
            Ok(Async::NotReady) => {
                task::current().notify();
                Ok(Async::NotReady)
            }
            Ok(val) => Ok(val),
            Err(_) => Err(()),
        }
    }
}
