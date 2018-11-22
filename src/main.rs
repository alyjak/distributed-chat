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
