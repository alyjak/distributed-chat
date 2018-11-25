//! Provides async methods for sending messages and application commands from a termina
//! session. Also locks stdout, forcing threads to push messages through its internal peer2stdout
//! buffer in order to print to screen. This _should_ help stability and understanding the
//! application interfaces.
//!
//! Implements the terminal, client, and associated data structures
//!

use std::collections::{HashMap, HashSet};
use std::io::{self, Write, Stdin, Stdout};
use std::net::SocketAddr;

use bytes::{BufMut, Bytes, BytesMut};

use futures::sync::mpsc;
use tokio::net::tcp::ConnectFuture;
use tokio::net::TcpStream;
use tokio::prelude::*;

use message::{Message, MsgRx, MsgTx, MsgTyp};
use peer::SocketMapAccessor;
use LINES_PER_TICK;

enum CmdMsg {
    Cmd(CmdKind),
    Msg(Message),
}

enum CmdKind {
    Volume(SocketAddr, PeerVolume),
    Quit,
    Connect(SocketAddr),
    Type(MsgTyp),
    To(Vec<SocketAddr>),
}

enum ParseState {
    Init,
    ParseLine,
    CmdFound,
    MsgFound,
}

struct StdioSocket {
    stdin: Stdin,
    stdout: Stdout,
}

impl StdioSocket {
    fn new() -> StdioSocket {
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
/// buffer. Commands and messages from stdin go to the Client, which then either processes
/// the command or sends a message to the addressed peers, according to the parsed stdin contents.
#[derive(Debug)]
struct TerminalCodec {
    bytebuf: BytesMut,
    state: ParseState,
    idx: usize,
    line_buf: BytesMut,
    msg_buf: BytesMut,
    peer_map: PeerMapAccessor,
}

/// Polls stdin for commands and messages. Commands are performed as side effects. Messages are sent
/// using a futures Stream.
impl TerminalCodec {
    fn new(peer_map: PeerMapAccessor) -> TerminalCodec {
        TerminalCodec {
            framebuf: BytesMut::with_capacity(1024),
            state: ParseState::Init,
            buf_idx: 0,
            line_buf: ByteMut::with_capacity(1024),
            msg_buf: ByteMut::with_capacity(1024),
            peer_map,
        }
    }
}

/// Implement polling stdin/stdout as a Stream so that it occurs for the entire duration of the
/// executor.
impl Decoder for TerminalCodec {
    type Item = CmdMsg;
    type Error = io::Error;

    /// Poll stdin for new input and send it out for the Client future to process, also
    /// drain peer2stdout if it has content.
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.state {
                ParseState::Init => {
                    match buf[self.buf_idx..]
                        .windows(1)
                        .enumerate()
                        .find(|(_, byte)| byte == b"\n")
                        .map(|(idx, _)| idx)
                    {
                        None => {
                            self.buf_idx = buf.len();
                            return Ok(Async::NotReady)
                        },
                        Some(idx) => {
                            self.line_buf.clear();
                            if self.line_buf.remaining_mut() <= idx {
                                self.line_buf.reserve(idx+1);
                            }
                            let line = buf.split_to(idx);
                            self.buf_idx = 0;
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
                    match str::from_utf8(&self.line_buf[..]) {
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
                                self.msg_buf.put(Bytes::from(b"\n"));
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
                    let len = msg_buf.len()
                    if len >= 3 && self.msg_buf[len - 3 ..] == b"\n\n\n" {
                        // Message terminated, time to send it.
                        self.msg_buf.truncate(len - 3); // remove the triple newline control sequence
                        let client = MsgClient::new(
                            msg: msg_buf.freeze(),
                            self.peer_map.clone(),
                            self.sock_map.clone(),
                            self.out_conf.clone(),
                        );
                        self.msg_buf.clear();
                        self.state = ParseState::Init;
                        return Ok(Async::Ready(Some(Either::A(client))));
                    } else {
                        // Message is not terminated, go back to finding lines
                        self.state = ParseState::Init;
                    }
                },
                ParseState::ParseCmd => {
                    // can unwrap because this is only entered via ParseLine, which checks that the
                    // buf is valid unicode.
                    let line = str::from_utf8(&self.line_buf[..]);
                    let cmdargs = line.split_whitespace().iter();
                    // Since ParseLine checks for a leading '/', unwrap will work
                    let cmd = match cmdargs.next().unwrap() {
                        "/volume" => {
                            
                        }self.cmd_verbosity(&cmdargs[1..]),
                        "/connect" => self.cmd_connect(&cmdargs[1..]),
                        "/to" => self.cmd_set_to(&cmdargs[1..]),
                        "/quit" => self.cmd_quit(),
                        _ => Err(format!("Unknown command {:?}", cmdargs)),
                    }
                    self.line_buf.clear();
                    self.state = ParseState::Init;
                },
            }
        };
    }
}

impl Encoder for TerminalCodec {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let verbosity = self.peer_map.lock().get(item.from)?.verbosity;
        match verbosity {
            Debug => {
                dst.put(format!("{}", item)[..]);
                match item.typ {
                    MsgTyp::ACK | MsgTyp::NACK(_) => {
                        let msgnum = item.content.into_buf().get_u8;
                        match self.ping_map.lock()?.get(msgnum) {
                            None => {
                                dst.put(&format!("Unknown roundtrip msgtime for msgnum {}, there is
                                                 no ping_map entry for that message.", msgnum)[..]);
                            },
                            Some(time) => {
                                let roundtrip = 
                                dst.put(&format!("Roundtrip time for msgnum {} = {}.",
                                                 msgnum, time.elapsed())[..]);
                            },
                        };
                    },
                    _ => {},
                };
            },
            Standard => {
                match item.typ {
                    MsgTyp::PM | MsgTyp::BC | MsgTyp::BBC => {
                        dst.put(&format!("{}", item)[..])
                    },
                    _ => {}
                };
            },
            Quiet => {
                match item.typ {
                    MsgTyp::PM => {
                        dst.put(&format!("{}", item)[..])
                    },
                    _ => {}
                }
            },
            Block => {},
        }
    }
}

#[derive(Debug, PartialEq)]
enum PeerVolume {
    /// Write out all messages (ACK, NACK, etc) as well a standard.
    Debug,
    /// Write out broadcasts and directed messages
    Normal,
    /// Only write directed messages
    Quiet,
    /// Do not write any messages from this peer to stdout. Do not send messages to this peer.
    Block,
}

/// PeerConfig stores client preferences about a peer.
#[derive(Debug)]
pub struct PeerConfig {
    /// Remote address for the peer.
    addr: SocketAddr,
    /// Active username for this address
    name: Bytes,
    /// The clients configured verbosity for this peer
    vol: PeerVolume,
    /// One of the assignment objectives is to count the roundtrip time between sending a message
    /// and receiving an ACK. To do this we need to have a buffer to store the time sent for each
    /// message. The message protocol uses a wrapping u8 as a counter, so we know this buffer won't
    /// grow without bound, because we're indexed based on the message counter.
    ping_map: Hashmap<u8, SystemTime>,
    /// Connections can be blocked, initializing, open, or closed.
    // state: ConnectionState,
    // /// If there's an active socket to the peer this will buffer messages from the terminal client
    // /// to that remote peer.
}

impl PeerConfig {
    fn new(addr: SocketAddr) -> PeerConfig {
        SocketAddr {
            addr,
            name: Bytes::new(), // Empty name by default
            vol: PeerVolume::Standard,
            ping_map: Hashmap::new(),
        }
    }
}


/// PeerMap is a map in the Client that holds channels to the peers and I/O preferences the client
/// holds for each known peer.
#[derive(Debug)]
struct PeerMap(HashMap<SocketAddr, Box<PeerConfig>>);

/// PeerMap is a map in the Client that holds channels to the peers and I/O preferences the client
/// holds for each known peer.
#[derive(Debug)]
struct UsernameMap(HashMap<String, SocketAddr>);

/// Takes in unstructured terminal input and transforms it into client-server commands or messages
/// to send to peers.
struct MsgConfig {
    username: String,
    message_type: MsgTyp,
    recipients: Vec<SocketAddr>,
}

struct MsgClient {
    client2stdout: BytesTx,
    /// Broadcast messages are only sent to peers who are already connected or
    /// connecting and who are not blocked.
    peermap: PeerConfigAccessor,
    usernamemap: UsernameAccessor,
    socketmap: SocketMapAccessor,
}

/// Client is the main processing function of the application: it multiplexes out messages to peers
/// and demultiplexes messages from peers and sends them to the client.
impl MsgClient {
    fn new(username: String, stdin2client: BytesRx, client2stdout: BytesTx) -> Client {
        MsgClient {
            username,
            stdin2client,
            parsebuf: BytesMut::new(),
            client2stdout,
            message_type: MsgTyp::BC,
            recipients: vec![],
            client2peers: PeerMap(HashMap::new()),
        }
    }

    /// Looks at the start of parsebuf to determine if there's a command string there, if not then
    /// the buffer is a message. If there is a command, returns the command and advances the
    fn cmd_verbosity(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_connect(&mut self, args: &[&str]) -> Option<String> {
        // Need to be able to handle username clashes
        Err("TODO".to_string())
    }

    fn cmd_to(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_quit(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn send_msg(&self, msg: Message) {
        for addr in to.iter() {
            self.peermap
                .get(addr)?
                .client2peer
                .unwrap_or(return io::Error::new(
                    io::ErrorKind::ConnectionClosed,
                    "Socket to a recipient is closed"))
                .put(msg)?;
        }
    }
}

impl Future for MsgClient {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        // 1. update peermap with any socketmap elements that aren't in self.peerconfig
        { // bracket in order to scope the mutex lock
            let socket = self.socketmap.lock();
            let config_addr_set = HashSet::FromIterator(self.peermap.keys());
            let socket_addr_set = HashSet::FromIterator(socket.keys());
            // 1. ensure the config addrs that don't have sockets are in the disconnected state
            for closed_addr in config_addr_set.difference(socket_addr_set) {
                self.peermap.get(closed_addr)?.state = ConnectionState::Disconnected;
                self.socketmap.remove(closed_addr);
            }
            // 2. Open new config entries for socket addrs that don't have configs
            for new_addr in socket_addr_set.difference(config_addr_set) {
                let mut peer_socket = socket.get(new_addr)?;
                // Create sockets for peer and client messages
                let (peer_tx, peer_rx) = mpsc::unbounded();
                let (client_tx, client_rx) = mpsc::unbounded();
                self.peermap.insert(
                    new_addr,
                    PeerConfig {
                        address: new_addr,
                        username: peer_socket.username.clone(),
                        verbosity: PeerVolume::Normal,
                        state: ConnectionState::Connected,
                        client2peer: Ok(client_tx),
                        peer2client: Ok(peer_rx),
                    }
                );
                // Hook in the other side of the sockets
                peer_socket.peer2client = Ok(peer_tx);
                peer_socket.client2peer = Ok(client_rx);
                // Update the username address map
                *self.socketmap.insert(peer_socket.username.clone(), new_addr.clone());
            }
        }

        // 2. buffer terminal input
        for i in 0..LINES_PER_TICK {
            match self.stdin2client.poll()? {
                Async::Ready(Some(v)) => {
                    if v.len() > self.parsebuf.remaining_mut() {
                        self.parsebuf.reserve(v.len())
                    }
                    self.parsebuf.put(&v[..]);
                }
                Async::NotReady => break,
            }
            if i + 1 == LINES_PER_TICK {
                task::current().notify();
            }
        }
        // 3. parse buffered terminal input
        loop {
            if self.try_parse_cmd() == None {
                if self.try_parse_msg() == None {
                    break;
                }
            }
        }

        // 4. flush peer output
        for peer in self.peermap.values() {
            if let Some(msgbuf) = peer.peer2client {
                for i in 0..LINES_PER_TICK {
                    match peer.poll()? {
                        Async::Ready(Some(msg)) => self.print_message(msg, peer.verbosity),
                        Async::NotReady => break,
                    }
                    if i + 1 == LINES_PER_TICK {
                        task::current().notify();
                    }
                }
            }
        }
        Async::Ready(())
    }
}
