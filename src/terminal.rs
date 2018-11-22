//! Provides async methods for sending messages and application commands from a termina
//! session. Also locks stdout, forcing threads to push messages through its internal peer2stdout
//! buffer in order to print to screen. This _should_ help stability and understanding the
//! application interfaces.
//!
//! Implements the terminal, client, and associated data structures
//!

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::SocketAddr;

use bytes::{BufMut, Bytes, BytesMut};

use futures::sync::mpsc;
use tokio::net::tcp::ConnectFuture;
use tokio::net::TcpStream;
use tokio::prelude::*;

use message::{Message, MsgRx, MsgTx, MsgTyp};
use peer::SocketMapAccessor;
use LINES_PER_TICK;

/// Shorthand for the two sides of the message channel used between Terminal and Client.
pub type BytesRx = mpsc::UnboundedReceiver<Bytes>;
pub type BytesTx = mpsc::UnboundedSender<Bytes>;

/// Terminal wraps stdin and stdout. Peers send messages into the other end of the peer2stdout mpsc
/// buffer. Commands and messages from stdin go to the Client, which then either processes
/// the command or sends a message to the addressed peers, according to the parsed stdin contents.
#[derive(Debug)]
struct Terminal {
    stdin2client: BytesTx,
    peer2stdout: BytesRx,
}

/// Polls stdin for commands and messages. Commands are performed as side effects. Messages are sent
/// using a futures Stream.
impl Terminal {
    fn new(tx: BytesTx, rx: BytesRx) -> Terminal {
        Terminal {
            stdin2client: tx,
            peer2stdout: rx,
        }
    }
}

/// Implement polling stdin/stdout as a Stream so that it occurs for the entire duration of the
/// executor.
impl Stream for Terminal {
    type Item = String;
    type Error = io::Error;

    /// Poll stdin for new input and send it out for the Client future to process, also
    /// drain peer2stdout if it has content.
    fn poll(&mut self) -> Poll<Option<String>, Self::Error> {
        // flag to tell if we should trigger the terminalparser future or not.
        let mut is_ready = false;
        // The linebuf needs to be big enough to hold whatever the maximum single line that stdin
        // bufread could return.
        let stdin = io::stdin().lock();
        let mut framebuf = BytesMut::with_capacity(1024);

        for i in 0..LINES_PER_TICK {
            // Not using read_line because I dont want to convert between &mut String and BytesMut
            let peek_at_stdin = io::stdin().lock().fill_buf();
            // peek for a newline,
            let line_len = match peek_at_stdin
                .windows(1)
                .enumerate()
                .find(|(_, byte)| byte == b"\n")
                .map(|(idx, _)| idx)
            {
                // Don't send a frame without a newline
                None => break,
                Some(idx) => idx + 1, // +1 to pull in the newline char
            };

            // Get the framebuf ready for the new payload
            framebuf.clear();
            // sanity check to ensure that I understand what clear is doing
            assert!(framebuf.remaining_mut() >= 1024);

            // Ensure the framebuf to be big enough to handle the full size of the stdin line
            if framebuf.remaining_mut() <= line_len {
                framebuf.reserve(line_len);
            }
            framebuf.put(&peek_at_stdin[..line_len]);
            // Let stdin know that it should clear the line
            io::stdin().lock().collect(line_len);
            self.stdin2client
                .unbounded_send(framebuf.freeze().clone())?;
            is_ready = true;

            // If this is the last iteration, the loop will break even
            // though there could still be lines to read. Because we did
            // not reach `Async::NotReady`, we have to notify ourselves
            // in order to tell the executor to schedule the task again.
            if i + 1 == LINES_PER_TICK {
                task::current().notify();
            }
        }

        // drain the stdout queue,
        // TODO: This likely will make stdin look funky if a lot of traffic is coming in. Should
        //       capture stdin and redisplay once flushing stdout messages is completed.
        // let mut reprint_stdin = false;
        for i in 0..LINES_PER_TICK {
            match self.peer2stdout.poll()? {
                Async::Ready(Some(v)) => io::stdout().lock().write_all(&v)?,
                Async::NotReady => break,
            }
            if i + 1 == LINES_PER_TICK {
                task::current().notify();
            }
        }

        // Did we push frames onto stdin2client? If so, Async::Ready
        match is_ready {
            true => Ok(Async::Ready(())),
            false => Ok(Async::NotReady),
        }
    }
}

// PeerConfig structures follow. These are embedded in the Client

#[derive(Debug, PartialEq)]
enum ConnectionState {
    /// There's no active socket for this connection,.
    Disconnected,
    /// We're currently initializing a TCP socket for this connection.
    Connecting(Box<ConnectFuture>),
    /// There's an active TCP socket for this connection.
    Connected,
}

#[derive(Debug)]
enum PeerVerbosity {
    /// Write out all messages (ACK, NACK, etc) as well a standard.
    Debug,
    /// Write out broadcasts and directed messages
    Standard,
    /// Only write directed messages
    Quiet,
    /// Do not write any messages from this peer to stdout
    Block,
}

/// PeerConfig stores client preferences about a peer.
#[derive(Debug)]
pub struct PeerConfig {
    /// Remote address for the peer.
    address: SocketAddr,
    /// Active username for this address
    username: Bytes,
    /// The clients configured verbosity for this peer
    verbosity: PeerVerbosity,
    /// Connections can be blocked, initializing, open, or closed.
    state: ConnectionState,
    /// If there's an active socket to the peer this will buffer messages from the terminal client
    /// to that remote peer.
    client2peer: Option<MsgTx>,
    /// If there's an active socket to the peer this will buffer messages received from the remote
    /// peer intended for our terminal client.
    peer2client: Option<MsgRx>,
}

/// PeerMap is a map in the Client that holds channels to the peers and I/O preferences the client
/// holds for each known peer.
#[derive(Debug)]
struct PeerMap(HashMap<Bytes, Box<PeerConfig>>);

/// Takes in unstructured terminal input and transforms it into client-server commands or messages
/// to send to peers
struct Client {
    username: Bytes,
    stdin2client: BytesRx,
    client2stdout: BytesTx,
    parsebuf: BytesMut,
    message_type: MsgTyp,
    recipients: Box<Vec<Bytes>>,
    peermap: PeerMap,
    socketmap: SocketMapAccessor,
}

/// Client is the main processing function of the application: it multiplexes out messages to peers
/// and demultiplexes messages from peers and sends them to the client.
impl Client {
    fn new(username: String, stdin2client: BytesRx, client2stdout: BytesTx) -> Client {
        Client {
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
    /// parsebuf to remove the command bytes.
    fn try_parse_cmd(&mut self, idx: usize) -> Option<()> {
        // First check for windows-style newlines
        // Increment enumerated index by 2 to capture the two return characters.
        let mut newline_idx = self
            .parsebuf
            .windows(2)
            .enumerate()
            .find(|&(_, bytes)| bytes == b"\r\n")
            .map(|(idx, _)| idx + 2);
        // then unix
        if let None = newline_idx {
            // Check for unix style line endings now.
            // Increment index by 1 to capture the return character.
            newline_idx = self
                .parsebuf
                .windows(1)
                .enumerate()
                .find(|&(_, byte)| byte == b"\n")
                .map(|(idx, _)| idx + 1);
        }

        if let None = newline_idx {
            return None;
        }

        // If we have a newline, now check to see if the first line is a command. Commands are all
        // strings, so try and read the line as a string. If that fails, its a message. If it
        // succeeds, strip leading/trailing whitespace and then see if the first char is the command
        // initiator '/' in honor of IRC.
        let mut rest = self.parsebuf.split_from(idx);
        let mut cmdstr = match str::from_utf8(&self.parsebuf) {
            Ok(string) => {
                string.trim();
                string
            }
            Err(e) => {
                // oops! not a string, therefore assume this is part of some fancy message
                self.parsebuf.unsplit(rest);
                return None;
            }
        };
        if !cmdstr.starts_with("/") {
            return None;
        }
        // Found a command!
        let cmdargs = cmdstr.split_whitespace().collect();
        // We know there's at least one element as cmdstr starts with /
        let result = match cmdargs[0] {
            "/verbosity" => self.cmd_verbosity(&cmdargs[1..]),
            "/connect" => self.cmd_connect(&cmdargs[1..]),
            "/to" => self.cmd_set_to(&cmdargs[1..]),
            "/quit" => self.cmd_quit(),
            _ => Err(format!("Unknown command {:?}", cmdargs)),
        };
        let resultstr = match result {
            Ok(ref okstr) => format!("command issued! {}", okstr),
            Err(ref errorstr) => format!("command failed: {}", errorstr),
        };
        // the rx half will only be dropped when the application is closing, which
        // occurs either through ctrl+c or the quit command that this struct issues.
        self.client2stdout
            .unbounded_send(&resultstr[..].into_buf())
            .unwrap();
        // Drop the command portion of the buffer
        self.parsebuf = rest;
        Ok(())
    }

    fn cmd_verbosity(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_unblock(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_connect(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_to(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    fn cmd_quit(&mut self, args: &[&str]) -> Option<String> {
        Err("TODO".to_string())
    }

    // This may need to be async
    fn send_message(&self, msg: Message) {}

    fn print_message(&self, msg: Message, verbosity: PeerVerbosity) {}

    /// Use a triple newline because then people can write paragraphs, which
    /// I would like to encourage. If I knew how to detect ctrl+enter I would prefer that over the
    /// triple enter though.
    fn try_parse_msg(&mut self) -> Option<()> {
        // Look for 3 oldschool \r\n style newlines
        let mut newline_idx = self
            .parsebuf
            .windows(6)
            .enumerate()
            .find(|&(_, bytes)| bytes == b"\r\n\r\n\r\n")
            .map(|(idx, _)| idx + 6);
        // then try UNIX
        if let None = newline_idx {
            // Check for unix style line endings now.
            // Increment index by 1 to capture the return character.
            newline_idx = self
                .parsebuf
                .windows(3)
                .enumerate()
                .find(|&(_, byte)| byte == b"\n\n\n")
                .map(|(idx, _)| idx + 3);
        }

        if let None = newline_idx {
            return None;
        }

        let msg = self.parsebuf.split_to(newline_idx);
        if msg[newline_idx - 2..] == b"\r\n" {
            msg.split_off(newline_idx - 6);
        } else {
            msg.split_off(newline_idx - 3)
        }

        let msg = Message::create(
            self.message_type,
            self.username.copy(),
            self.recipients.copy(),
            msg.freeze().copy(),
        );
        match msg {
            Ok(msg) => self.send_message(msg),
            Err(e) => {
                let err_buf = &format!("Message error: {:?}", e)[..].into_buf();
                self.client2stdout.unbounded_send(err_buf).unwrap();
            }
        }
        Some(())
    }
}

impl Future for Client {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        // 1. update peermap with any socketmap elements that aren't in self.peerconfig
        {
            // scope the mutex lock
            let socket = self.socketmap.lock();
            let config_addr_set = HashSet::FromIterator(self.peermap.keys());
            let socket_addr_set = HashSet::FromIterator(socket.keys());
            // 1. ensure the config addrs that don't have sockets are in the disconnected state
            for closed in config_addr_set.difference(socket_addr_set) {}
            // 2. Open new config entries for socket addrs that don't have configs
            for new in socket_addr_set.difference(config_addr_set) {}
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
