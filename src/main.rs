extern crate bytes;
#[macro_use]
extern crate clap;
extern crate assignment;
extern crate futures;
extern crate tokio;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::codec::Decoder;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;

use assignment::message::{MsgBuilder, MsgClient, MsgTyp};

use assignment::peer::{
    BuilderAccessor, PeerChannel, PeerConfig, PeerMap, PeerMapAccessor, SockMap, SockMapAccessor,
    UserMap, UserMapAccessor,
};
use assignment::terminal::{StdioSocket, TerminalCodec, HELP};

fn main() -> Result<(), Box<std::error::Error>> {
    let matches = clap_app!(
        myapp =>
            (name: "test-client")
            (version: "0.1")
            (author: "Andrew Lyjak <andrew.lyjak@gmail.com>")
            (about: "An IRC-like peer to peer message client programmed for the POA Network hiring test.")
            (@arg ADDRESS: +required "The hostname:port to bind to.")
            (@arg NAME: +required "The username to use while connected.")
    ).get_matches();

    let self_addr = matches.value_of("ADDRESS").unwrap().parse().unwrap();
    print!(
        "Welcome {}, binding to address: {} and listening for messages\n",
        matches.value_of("NAME").unwrap(),
        self_addr
    );
    print!("{}\n\n> ", HELP);
    io::stdout().flush()?;

    // Our three global variables used between the threads to control connection
    // states.
    let peer_ref_username = Bytes::from(&matches.value_of("NAME").unwrap()[..]);
    let peer_ref_sock_map = Arc::new(Mutex::new(SockMap {
        inner: HashMap::new(),
    }));
    let peer_ref_peer_map = Arc::new(Mutex::new(PeerMap {
        inner: HashMap::new(),
    }));
    let peer_ref_user_map = Arc::new(Mutex::new(UserMap {
        inner: HashMap::new(),
    }));
    let peer_ref_bldr_mut = Arc::new(Mutex::new(MsgBuilder::new(peer_ref_username.clone())));

    // This is ugly but the only way I figured out to get variables moved into the closures
    let term_ref_username = peer_ref_username.clone();
    let term_ref_sock_map = peer_ref_sock_map.clone();
    let term_ref_peer_map = peer_ref_peer_map.clone();
    let term_ref_user_map = peer_ref_user_map.clone();
    let term_ref_bldr_mut = peer_ref_bldr_mut.clone();

    // Listener receives incoming connections
    let listener = TcpListener::bind(&self_addr)?;
    // Listen for incoming messages, decode them, filter them through
    // peer-specific configuration, and send to stdout.
    let peer_server = listener
        .incoming()
        .for_each(move |socket| {
            init_channel(
                socket,
                peer_ref_username.clone(),
                peer_ref_peer_map.clone(),
                peer_ref_sock_map.clone(),
                peer_ref_user_map.clone(),
                peer_ref_bldr_mut.clone(),
            )
        }).map_err(|err| {
            println!("Error accepting incoming connection: {}", err);
        });

    // Terminal decodes/encodes terminal input/output into messages or commands. Commands configure
    // peer-specific configuration or output message configuration. Messages are sent to peers based
    // on output message configuration. Messages are sent through an open socket if it is available,
    // otherwise a new socket is opened.
    let (_sink, cmdmsg_stream) = TerminalCodec::new(
        term_ref_sock_map.clone(),
        term_ref_peer_map.clone(),
        term_ref_user_map.clone(),
        term_ref_bldr_mut.clone(),
    ).framed(StdioSocket::new())
    .split();
    let term_server = cmdmsg_stream
        .for_each(move |(msg, addrs)| {
            let s_mutex = term_ref_sock_map.clone();
            let s_lock = s_mutex.lock().unwrap();
            let s_map = &s_lock.inner;
            for addr in addrs {
                if !s_map.contains_key(&addr) {
                    // This is ugly but the only way I figured out to get variables moved into the
                    // closures
                    let chan_ref_username = term_ref_username.clone();
                    let chan_ref_sock_map = term_ref_sock_map.clone();
                    let chan_ref_peer_map = term_ref_peer_map.clone();
                    let chan_ref_user_map = term_ref_user_map.clone();
                    let chan_ref_bldr_mut = term_ref_bldr_mut.clone();

                    let connection = TcpStream::connect(&addr)
                        .and_then(move |socket| {
                            init_channel(
                                socket,
                                chan_ref_username.clone(),
                                chan_ref_peer_map.clone(),
                                chan_ref_sock_map.clone(),
                                chan_ref_user_map.clone(),
                                chan_ref_bldr_mut.clone(),
                            )
                        }).map_err(|err| {
                            println!("Error negotiating outgoing connection: {}", err);
                        });
                    tokio::spawn(connection);
                }
                let client = MsgClient::new(msg.clone(), addr, term_ref_sock_map.clone());
                tokio::spawn(client);
            }
            if msg.typ == MsgTyp::Disconnect {
                // TODO: how do I close out nicely? Need to issue rt.shutdown_now() from inside a
                // closure..
                panic!("This is a stubbed out, sloppy execution of a termination command")
            } else {
                Ok(())
            }
        }).map_err(|err| {
            println!("Error accepting terminal input: {}", err);
        });

    // Create the runtime
    let mut rt = Runtime::new().unwrap();

    // Spawn the server task
    rt.spawn(term_server);
    rt.spawn(peer_server);

    rt.shutdown_on_idle().wait().unwrap();
    Ok(())
}

fn init_channel(
    sock: TcpStream,
    username: Bytes,
    peer_map: PeerMapAccessor,
    sock_map: SockMapAccessor,
    user_map: UserMapAccessor,
    bldr_mut: BuilderAccessor,
) -> Result<(), io::Error> {
    let addr = sock.peer_addr().unwrap();
    // Get or insert peer into peer_map
    {
        // minimize the scope of the peer_map lock
        let p_mutex = peer_map.clone();
        let mut p_lock = p_mutex.lock().unwrap();
        let p_map = &mut p_lock.inner;
        if !p_map.contains_key(&addr) {
            p_map.insert(addr.clone(), Box::new(PeerConfig::new(addr.clone())));
        }
    }
    let peer = PeerChannel::new(
        sock,
        addr.clone(),
        username.clone(),
        peer_map.clone(),
        sock_map.clone(),
        user_map.clone(),
        bldr_mut.clone(),
    ).map_err(|e| println!("Error parsing socket: {}", e));
    tokio::spawn(peer);
    Ok(())
}
