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
//!  TcpListener
//!     v
//! +-----------+            +------------+                       +---------------------+
//! | TcpStream |<-MsgCodec->| PeerSocket |<-UnboundedMsgChannel->| PeerConfig | Client |
//! +-----------+            +------------+                       |------------|        |
//! | TcpStream |<-MsgCodec->| PeerSocket |<-UnboundedMsgChannel->| PeerConfig |        |
//! +-----------+            +------------+                       +------------+        |
//! | ...       |   ...      |  ...       |  ...                  | ...        |        |
//! +-----------+            +------------+                       +------------+--------+
//!                                                                                ^
//!                                                                UnboundedBytesChannel
//!                                                                                v
//!                                                                           +----------+
//!                                                                           | Terminal |
//!                                                                           +----------+
//!                                                                              ^     v
//!                                                                           stdin stdout

extern crate byteorder;
extern crate bytes;
extern crate crc;
#[macro_use]
extern crate futures;
extern crate tokio;

pub mod message;
pub mod peer;
pub mod terminal;

use bytes::Bytes;
use futures::sync::mpsc::{UnboundedReceiver, UnboundedSender};

// Tokio (and futures) use cooperative scheduling without any
// preemption. If a task never yields execution back to the executor,
// then other tasks may be starved.
//
// To deal with this, robust applications should not have any unbounded
// loops. In this example, we will read at most `LINES_PER_TICK` lines
// from the client on each tick.
//
// If the limit is hit, the current task is notified, informing the
// executor to schedule the task again asap.
pub const LINES_PER_TICK: usize = 10;
