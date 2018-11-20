#[macro_use]
extern crate clap;
extern crate assignment;
extern crate bytes;
#[macro_use]
extern crate futures;
extern crate tokio;

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::{BufMut, Bytes, BytesMut};

use futures::sync::mpsc;
use tokio::net::tcp::ConnectFuture;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;

/// Shorthand for the transmit half of the message channel.
type Tx = mpsc::UnboundedSender<Bytes>;

/// Shorthand for the receive half of the message channel.
type Rx = mpsc::UnboundedReceiver<Bytes>;

#[derive(Debug)]
struct PeerStateMap(HashMap<SocketAddr, Box<PeerState>>);

// Shared access to active TcpStreams
type PeerStates = Arc<Mutex<PeerStateMap>>;

#[derive(Debug)]
struct ClientMessageMap(HashMap<SocketAddr, Tx>);

#[derive(Debug)]
struct User2AddressMap(HashMap<Bytes, SocketAddr>);

#[derive(Debug)]
enum ConnectionState {
    /// There's no active socket for this connection,.
    Disconnected,
    /// We're currently initializing a TCP socket for this connection.
    Connecting(ConnectFuture),
    /// There's an active TCP socket for this connection.
    Connected(TcpStream),
}

/// PeerState stores data about an external connection. The PeerState Future implementation
/// handles receiving and transmitting messages to/from the remote peer.
#[derive(Debug)]
struct PeerState {
    /// The username of the peer,
    username: BytesMut,
    /// Active socket (if any)
    ignore: bool,
    /// Some socket used for an ongoing message transfer. Becomes None when no TX or RX is active.
    socket: ConnectionState,
    /// Remote address for the peer.
    address: SocketAddr,
    /// The rx side of the unboundedReceiver future -- used to get messages from the console client.
    client2peer: Rx,
    /// Used to buffer messages from the console client such that socket tx operations are cleaner.
    client2peerbuf: BytesMut,
    /// Used to buffer incoming messages from the peer
    peer2clientbuf: BytesMut,
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

#[derive(Debug)]
struct Terminal {
    stdin2parse: Rx,
    peer2stdout: Tx,
}

/// Polls stdin for commands and messages. Commands are performed as side effects. Messages are sent
/// using a futures Stream.
impl Terminal {
    // Note that tx and rx are NOT to the same future. Tx goes to TerminalDecoder, rx comes from
    // peers via the PeerMap.
    fn new(tx: Tx, rx: Rx) -> Terminal {
        TerminalInput {
            stdin2parse: tx,
            peer2stdout: rx,
        }
    }
}

impl Stream for Terminal {
    type Item = String;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<String>, Self::Error> {
        let mut is_ready = false;
        loop {
            let mut linebuf = vec![0; 1024];
            let n = match io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) => return Err(io::Error::new(io::ErrorKind::Other, "whut")),
            };
            linebuf.truncate(n);
            self.stdin2parse.send(linebuf).wait()?;
            is_ready = true;
        }

        // drain the stdout queue
        let mut print_carret = false;
        while !self.peer2stdout.is_empty() {
            let mut printbuf = vec![0, 1024];
            let n = try_ready!(socket.poll_write(&mut printbuf));
            printbuf.truncate(n);
            io::stdout().write_all(&printbuf);
            print_carret = true;
        }
        if print_carret == true {
            print!("\r> ");
            io::stdout().flush()?;
        }
        match is_ready {
            true => Ok(Async::Ready(())),
            false => Ok(Async::NotReady)
        }
    }
}

/// Takes in unstructured terminal input and transforms it into client-server commands or messages
/// to send to peers
struct TerminalParser {
    stdin2parse: Rx,
    parsebuf: BytesMut,
    parse2stdout: Tx,
    message_type: MessageType,
    recipients: Ref<Vec[SocketAddr]>,
    peer_map: PeerStates,
    client_config: ClientConfig,
}

impl TerminalParser {
    fn new(
        stdin2parse: Rx,
        parse2stdout: Tx,
        peer_map: PeerStates,
        client_config: ClientConfig
    ) -> TerminalParser {
        TerminalParser {
            stdin2parse,
            parsebuf: BytesMut::new(),
            parse2stdout,
            message_type: MessageType::Broadcast,
            recipients: vec![],
            peer_map,
            client_config,
        }
    }

    /// Looks at the start of parsebuf to determine if there's a command string there, if not then
    /// the buffer is a message. If there is a command, returns the command and advances the
    /// parsebuf to remove the command bytes.
    fn try_parse_cmd(&mut self, idx: usize) -> Option<()> {
        // First check for windows-style newlines
        // Increment enumerated index by 2 to capture the two return characters.
        let mut newline_idx = self.parsebuf.windows(2).enumerate()
            .find(|&(_, bytes)| bytes == b"\r\n")
            .map(|(idx, _)| idx + 2);
        // then unix
        if let None = newline_idx {
            // Check for unix style line endings now.
            // Increment index by 1 to capture the return character.
            newline_idx = self.parsebuf.windows(1).enumerate()
                .find(|&(_, byte)| byte == b"\n")
                .map(|(idx, _)| idx + 1);
        }

        if let None = newline_idx { return None; }

        // If we have a newline, now check to see if the first line is a command. Commands are all
        // strings, so try and read the line as a string. If that fails, its a message. If it
        // succeeds, strip leading/trailing whitespace and then see if the first char is the command
        // initiator '/' in honor of IRC.
        let mut rest = self.parsebuf.split_from(idx);
        let mut cmdstr = match str::from_utf8(&self.parsebuf) {
            Ok(string) => {
                string.trim();
                string
            },
            Err(e) => {
                // oops! not a string, therefore assume this is part of some fancy message
                self.parsebuf.unsplit(rest);
                return None;
            }
        };
        if !cmdstr.starts_with("/") {
            return None;
        }
        //! found a command!
        let mut cmdargs = cmdstr.split_whitespace();
        // We know there's at least one element as cmdstr starts with /
        let cmd = cmdargs.next().unwrap();
        let result = match cmd {
            "/block" => self.cmd_block(cmdargs),
            "/unblock" => self.cmd_unblock(cmdargs),
            "/connect" => self.cmd_connect(cmdargs),
            "/to" => self.cmd_set_to(cmdargs),
            "/quit" => self.cmd_quit(),
            _ => Err(format!("Unknown command {}", cmd)),
        };
        let resultstr = match result {
            Ok(ref okstr) => format!("command issued! {}", okstr),
            Err(ref errorstr) => format!("command failed: {}", errorstr),
        };
        // the rx half will only be dropped when the application is closing, which
        // occurs either through ctrl+c or the quit command that this struct issues.
        self.parse2stdout.unbounded_send(resultstr).unwrap();
        // Drop the command portion of the buffer
        self.parsebuf = rest;
        Ok(())
    }

    fn cmd_block(&mut self, &mut args: str::SplitWhitespace) -> Option<String>{
        Err("TODO".to_string())
    }

    fn cmd_unblock(&mut self, &mut args: str::SplitWhitespace) -> Option<String>{
        Err("TODO".to_string())
    }

    fn cmd_connect(&mut self, &mut args: str::SplitWhitespace) -> Option<String>{
        Err("TODO".to_string())
    }

    fn cmd_to(&mut self, &mut args: str::SplitWhitespace) -> Option<String>{
        Err("TODO".to_string())
    }

    fn cmd_quit(&mut self, &mut args: str::SplitWhitespace) -> Option<String>{
        Err("TODO".to_string())
    }

    /// Use a triple newline because then people can write paragraphs, which
    /// I would like to encourage. If I knew how to detect ctrl+enter I would prefer that over the
    /// triple enter though.
    fn try_parse_msg(&mut self) -> Option<()> {
        // Look for 3 windows style newlines
        let mut newline_idx = self.parsebuf.windows(6).enumerate()
            .find(|&(_, bytes)| bytes == b"\r\n\r\n\r\n")
            .map(|(idx, _)| idx + 6);
        // then try UNIX
        if let None = newline_idx {
            // Check for unix style line endings now.
            // Increment index by 1 to capture the return character.
            newline_idx = self.parsebuf.windows(1).enumerate()
                .find(|&(_, byte)| byte == b"\n\n\n")
                .map(|(idx, _)| idx + 3);
        }

        if let None = newline_idx { return None; }

        let msg = self.parsebuf.split_to(newline_idx);
        if msg[newline_idx - 2..] == b"\r\n" {
            msg.split_off(newline_idx - 6);
        } else {
            msg.split_off(newline_idx - 3)
        }

        let msgtype = match self.message_type {
            Message::Broadcast => b"0",
            Message::To => b"1"
        };
        let msgfrom = 
    }
}

impl Future for TerminalParser {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), Self::Error> {
        while !self.stdin2parse.is_empty() {
            self.parsebuf.reserve(1024);
            let n = try_ready!(self.stdin2parse(&self.parsebuf));
        }

        loop {
            if self.try_parse_cmd() == None {
                if self.try_parse_msg() == None {break}
            }
        }
        Async::Ready(())
    }
}


// TODO: tx isn't connected
// ignore isn't processed
// command parsing
// end to end message
// nothing is creating the unbounded future socket
fn main() {
    let matches = clap_app!(
        myapp =>
            (name: "test-client")
            (version: "0.1")
            (author: "Andrew Lyjak <andrew.lyjak@gmail.com>")
            (about: "An IRC-like peer to peer message client programmed for the POA Network hiring test.")
            (@arg ADDRESS: +required "The hostname:port to bind to.")
            (@arg NAME: +required "The username to use while connected.")
    ).get_matches();

    let self_addr = matches.value_of("ADDRESS").unwrap();
    println!(
        "Welcome {}, binding to address: {} and listening for messages",
        matches.value_of("NAME").unwrap(),
        self_addr
    );

    let peer_map = Arc::new(Mutex::new(PeerStateMap(HashMap::new())));
    let client_multiplexer = Arc::new(Mutex::new(
    let listener = TcpListener::bind(&self_addr);

    // Listen for incoming
    let chatserver = listener.incoming().for_each(move |socket| {
        // Spawn a task to process each incoming connection
        process_incoming_connection(socket, peer_map.clone());
        Ok(())
    });

    let terminal = TerminalInput::new(peer_map.clone());
    let cmdserver = tokio.spawn(
        TerminalInput::new(peer_map.clone())
            .for_each(|msg| terminal_parse(msg, peer_map.clone()))
            .map_err(|e| println!("Aborted terminal connection because of error: {}", e)),
    );

    tokio::run(chatserver);
}
