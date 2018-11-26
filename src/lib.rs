//! # An IRC-ish peer-to-peer chat service.
//!
//! Contains a simple client-server, which both opens a terminal interface for sending and receiving
//! messages as well as a server to listen for messages. In the terminal interface a user can:
//!
//! 1. Open up a connection to a peer.
//! 2. Broadcast messages to each connected peer.
//! 3. Send private messages to a single peer,
//! 4. Disconnect or ignore a peer,
//! 5. Accept an incoming connection request from an external peer,
//! 6. Exit the application.
//!
//! When the service is started, a help message is displayed that lists the available
//! commands. Messages are broadcast by default.
//!
//! Each client-server will ACK received messages. If the message was unpacked and processed
//! successfully, then an ACK is sent. If the message could not be processed, then a NACK is sent.
//!
//! TODO: Since each client-server has their own peer list, broadcast groups are not necessarily the
//! same set of peers. Expand the ACK protocol to: 1) share peer lists among connections, 2) add a
//! global broadcast, where a peer, after receiving a global broadcast will re-transmit the message
//! to some peer_C that it knows peer_A doesn't have in their broadcast set. Do this in a way where
//! every peer_B who has peer_C in their broadcast list doesn't all simultaneously rebroadcast to
//! peer_C.
//!
//! TODO: Replace unbounded senders with their bounded counterparts.
//!
//! Architecture:
//!
//! +---------------------+
//! | TcpListener(stream) |
//! +---------------------+
//!     ^                     /-->print                /->print                        /->print
//!            +-------------------+  +--------------------+  +---------------------------+
//!  network   |  Socket(future)   |->|  MsgClient(future) |<-|    Terminal(stream)       |
//!            |  ==============   |  |  ================= |  |    ================       |
//! --[bytes]->|MsgCodec(TcpStream)>-->--[msg|peerconfig]-->-->-.                         |
//! <-[bytes]--|ACK(msg) _/        |  +--------------------+  |  v                        |
//!            +-------------------+                          |TerminalCodec(stdin/stdout)|<-userinput
//!                                                           |                       |   |
//!            +-------------------+  +--------------------+  |                      /|   |
//! <-[bytes]--|MsgCodec(TcpStream)<--<--[msg|clientconf]--<--<--msg----------------' |   |
//! --[bytes]->|ACK(msg)_/         |  +--------------------+  +-----------------------|---+
//!            +-------------------+                                                  |
//!                                                     +-------------------+       [cmd]
//!                                      PeerConfig   <-| CmdClient(future) |        /
//!                                      ClientConfig <-|                   |<------/
//!                                                     +-------------------+
//!                                                            \-->print
extern crate byteorder;
extern crate bytes;
extern crate crc;
extern crate futures;
extern crate tokio;

pub mod message;
pub mod peer;
pub mod terminal;
