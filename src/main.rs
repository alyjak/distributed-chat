#[macro_use]
extern crate clap;
extern crate assignment;
#[macro_use]
extern crate futures;
extern crate tokio;

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures::sync::mpsc;
use tokio::net::tcp::ConnectFuture;
use tokio::net::TcpListener;
use tokio::prelude::*;


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

    // Our three global variables used between the threads to control connection
    // states.
    let sock_map = Arc::new(Mutex::new(SockMap(HashMap::new())));
    let peer_map = Arc::new(Mutex::new(PeerMap(HashMap::new())));
    let name_map = Arc::new(Mutex::new(NameMap(HashMap::new())));
    let ping_map = Arc::new(Mutex::new(PingMap(HashMap::new())));
    let username = matches.value_of("NAME").unwrap();
    let out_conf = Arc::new(Mutex::new(OutputConfig {
        username: username.clone(),
        msg_typ: MsgTyp::Broadcast,
        to: vec![],
    }));

    // Listener receives incoming connections
    let listener = TcpListener::bind(&self_addr);
    // Listen for incoming messages, decode them, filter them through
    // peer-specific configuration, and send to stdout.
    let peer_server = listener.incoming()
        .for_each(move |socket| {
            let addr = socket.peer_addr().unwrap();
            // Get or insert peer into peer_map
            { // minimize the scope of the peer_map lock
                let p_map = peer_map.clone().lock();
                if !p_map.contains_key(addr) {
                    p_map.insert(addr, PeerConfig::new(addr));
                }
            }
            let peer = PeerChannel::new(
                socket,
                addr,
                username.clone(),
                peer_map.clone(),
                user_map.clone(),
            );
            { // Minimize the scope of the socket map lock
                let s_map = sock_map.clone().lock();
                s_map.insert(addr, peer);
            }
            tokio::spawn(peer_channel);
            Ok(())
        })
        .map_err(|err| println!("Error accepting incoming connection: {}", err));

    // Terminal decodes/encodes terminal input/output into messages or
    // commands. Commands configure peer-specific configuration or output
    // message configuration. Messages are sent to peers based on output message
    // configuration. Messages are sent through an open socket if it is
    // available, otherwise a new socket is opened.
    framed = TerminalCodec::new(peer_map.clone()).framed(StdioSocket::new());
    (msg_writer, cmdmsg_reader) = framed.split();
    cmdmsg_reader.for_each(|cmdmsg| {
        let connection = match cmdmsg {
            CmdMsg::Cmd(cmd) => {
                CmdClient{
                    cmd: cmd,
                    peer_map: peer_map.clone(),
                    name_map: name_map.clone(),
                    out_conf: out_conf.clone(),
                }
            },
            CmdMsg::Msg(msg) => {
                SenderClient{
                    username: username.clone(),
                    msg: msg,
                    peer_map: peer_map.clone(),
                    sock_map: sock_map.clone(),
                    out_conf: out_conf.clone(),
                }
            },
        };
        tokio::spawn(connection);
        Ok(())
    }).map_err(|err| {println!("Error accepting terminal input: {}", err);});

    tokio::spawn(term_server);
    tokio::run(peer_server);
}
