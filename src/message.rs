//! Defines the message frame and codec used for the chat-server. Any new message types need to be
//! defined in this module. The Message format is defined and ordered as follows.
//!
//! HDR: 0xAA55
//! msgtype: u8
//! msgnumber: u8
//! fromlen: u32
//! from: String
//! tolen: u32
//! to: String
//! contentlen: u32
//! content: String
//! crc: u32
//!
//! CRC is calculated over everything, including the header, excluding the crc (of course!)
//!
//! The server is implemented over TCP currently, but this format could also work with UDP so long
//! as a framing buffer was wrapping the codec.
//!
//! TODO: I think this could be cleaned up in terms of memory usage.
//!

use std::{fmt, io};
use std::fmt::Display;
use std::net::SocketAddr;
use std::str;

use byteorder::NetworkEndian;
use bytes::{BufMut, Bytes, BytesMut};
use crc::crc32;
use futures::future::Either;
use futures::sync::mpsc;
use tokio::codec::{Decoder, Encoder};
use tokio::io as tio;
// its a nice pattern, that's all
pub const HDR_0: u8 = 0b10101010;
pub const HDR_1: u8 = 0b01010101;
pub const HDR: [u8; 2] = [HDR_0, HDR_1];

/// Shorthand for the two sides of the message channel used between the peer and Client.
pub type MsgRx = mpsc::UnboundedReceiver<Message>;
pub type MsgTx = mpsc::UnboundedSender<Message>;

fn get_len(buf: &mut BytesMut) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    let idx32 = NetworkEndian::read_u32(&buf[..4]);
    let idx = idx32 as usize;
    if buf.len() < idx + 4 {
        return None;
    }
    let _ = buf.split_to(4);
    Some(idx)
}

fn len_to_bytes(len: usize) -> [u8; 4] {
    let mut buf = [0; 4];
    let len32 = len as u32;
    NetworkEndian::write_u32(&mut buf, len32);
    buf
}

/// Message type. Can either broadcast to all connected peers, or send a private message to a single
/// peer
#[derive(Debug, Clone)]
pub enum MsgTyp {
    /// Send a private message to a peer or group of peers.
    PM,
    /// A broadcast message: send a message to all connected unblocked peers.
    BC,
    /// A blind broadcast message: Send a message to all connected unblocked
    /// peers but don't include peer addresses in the message.
    BBC,
    /// Request an open connection to a peer or group of peers.
    Connect,
    /// Notify a peer or group of peers that your client is disconnecting.
    Disconnect,
    /// Acknowledge receipt of a message.
    ACK,
    /// Notify a sender that there message was not processed, and the reason why.
    NACK(NACKKind),
}

/// When a NACK is received, this identifies the reason
#[derive(Debug, Clone)]
pub enum NACKKind {
    UserBlocked,
    ParseError,
    /// Identifies when a message is received with an already used message count
    Duplicate,
}

impl Display for NACKKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UserBlocked => write!(f, "blocked"),
            ParseError => write!(f, "message parse failed"),
            Duplicate => write!(f, "duplicate message"),
        }
    }
}

impl Display for MsgTyp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MsgTyp::PM => write!(f, "sent"),
            MsgTyp::BC => write!(f, "broadcast"),
            MsgTyp::BBC => write!(f, "blind broadcast"),
            MsgTyp::Connect => write!(f, "connected"),
            MsgTyp::Disconnect => write!(f, "disconnected"),
            MsgTyp::ACK => write!(f, "confirmed message"),
            MsgTyp::NACK(kind) => write!(f, "rejected message ({})", kind),
        }
    }
}

/// This is next to typ_to_byte so they can be easily checked to be inverses of one another
fn byte_to_typ(byte: u8) -> Result<MsgTyp, String> {
    match byte {
        01u8 => Ok(MsgTyp::PM),
        02u8 => Ok(MsgTyp::BC),
        03u8 => Ok(MsgTyp::BBC),
        20u8 => Ok(MsgTyp::Connect),
        21u8 => Ok(MsgTyp::Disconnect),
        40u8 => Ok(MsgTyp::ACK),
        41u8 => Ok(MsgTyp::NACK(NACKKind::Blocked)),
        42u8 => Ok(MsgTyp::NACK(NACKKind::ParseError)),
        43u8 => Ok(MsgTyp::NACK(NACKKind::Duplicate)),
        _ => Err(format!("Unknown message code {}", byte)),
    }
}

/// This is next to byte_to_typ so they can be easily checked to be inverses of one another
fn typ_to_byte(typ: MsgTyp) -> u8 {
    match typ {
        MsgTyp::PM => 01u8,
        MsgTyp::BC => 02u8,
        MsgTyp::BBC => 03u8,
        MsgTyp::Connect => 20u8,
        MsgTyp::Disconnect => 21u8,
        MsgTyp::ACK => 40u8,
        MsgTyp::NACK(NACKKind::UserBlocked) => 41u8,
        MsgTyp::NACK(NACKKind::ParseError) => 42u8,
        MsgTyp::NACK(NACKKind::Duplicate) => 43u8,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ParseState {
    Init,
    Len,
    From,
    Typ,
    MsgNum,
    To,
    Content,
    CRC,
    Done,
}

#[derive(Debug, Clone)]
pub struct Message {
    /// Message type defines the message meaning
    typ: MsgTyp,
    /// Messages are sent with monotonically incrementing counters to help identify if any got lost
    /// in transmission. The ACK protocol uses these numbers to communicate what messages were
    /// received.
    msgnum: u8,
    /// The username of the message sender (the sender's address is available via the socket)
    from: Bytes,
    /// The addresses of all recipients of the message. This information allows broadcast groups to
    /// be synchronized. Must be coercable into an array of `SocketAddr`s.
    to: Bytes,
    /// Final Content type is based on message type.
    content: Bytes,
    crc: u32,
}

impl Message {
    fn new(
        typ: MsgTyp,
        msgnum: u8,
        from: Bytes,
        to: Bytes,
        content: Bytes,
    ) -> Message {
        let mut msg = Message {typ, msgnum, from, to, content, crc: 0u32};
        let (_, crc) = msg.raw();
        msg.crc = crc;
        msg
    }

    fn raw(&mut self) -> (Bytes, u32) {
        let hdr: [u8; 4] = [HDR_0, HDR_1, typ_to_byte(self.typ), self.msgnum];
        let from = self.from.as_bytes();
        let fromlen = len_to_bytes(from.len());
        let to = self.to.as_bytes();
        let tolen = len_to_bytes(to.len());
        let content = self.content;
        let contentlen = len_to_bytes(content.len());
        let msglen = hdr.len()
            + tolen.len()
            + to.len()
            + fromlen.len()
            + from.len()
            + contentlen.len()
            + content.len();
        let raw = BytesMut::with_capacity(msglen);
        raw.put(&hdr[..]);
        raw.put(&fromlen[..]);
        raw.put(&from[..]);
        raw.put(&tolen[..]);
        raw.put(&to[..]);
        raw.put(&contentlen[..]);
        raw.put(&content[..]);

        // calculate crc and send result
        (raw.freeze(), crc32::checksum_ieee(&raw[..]))
    }

/// TODO, when there's non string content types, this needs to be expanded
impl Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.state {
            ParseState::Done => write!(f, "unparsed message"),
            _ => {
                write!(f, "[{} {}]", str::from_utf8(&self.from[..]), self.typ)?;
                match self.typ {
                    MsgTyp::ACK | MsgTyp::NACK(_) => Ok(()),
                    _ => write!(f, ": {}", str::from_utf8(&self.content[..])?)
                }
            }
        }
    }
}

pub struct MessageBuilder {
    /// We need access to the peer map in order to record timestamps of when messages were sent so
    /// that we can record roundrip latency between transmission and reception of ACKs.
    peer_map: PeerMapAccessor,
    msgnum: u8,
}

impl MessageBuilder {
    fn new(peer_map: PeerMapAccessor) -> MessageBuilder {
        MessageBuilder {
            peer_map,
            msgnum = 0u8,
        }
    }

    fn build(typ: MsgTyp, to: Bytes, content: Bytes) -> Message {
        self.msgnum = self.tx_msgnum.wrapping_add(1);
        self.ping_map.lock().insert(
            self.msgnum,
            SystemTime::now()
        );
        let msg = Message::new(typ, self.msgnum, self.username.clone(), to, content);
        msg
    }
}

/// TODO: Add timestamp of when received?
#[derive(Debug, Clone)]
pub struct MessageCodec {
    /// The socket address we're decoding from
    addr: SocketAddr,
    /// This field isn't sent, its used to control parsing
    state: ParseState,
    /// Message type defines the message meaning
    typ: Option<MsgTyp>,
    /// Messages are sent with monotonically incrementing counters to help identify if any got lost
    /// in transmission. The ACK protocol uses these numbers to communicate what messages were
    /// received.
    rx_msgnum: Option<u8>,
    /// A counter to add to transmitted messages
    tx_msgnum: u8,
    /// The username of the message sender (the sender's address is available via the socket)
    from: Option<Bytes>,
    /// The addresses of all recipients of the message. This information allows broadcast groups to
    /// be synchronized. Must be coercable into an array of `SocketAddr`s.
    to: Option<Bytes>,
    /// Final Content type is based on message type.
    content: Option<Bytes>,
    crc: Option<u32>,
};

impl MessageCodec {
    fn new(addr: SocketAddr) -> MessageCodec {
        Message {
            addr,
            state: ParseState::Init,
            peer_map,
            typ: None,
            tx_msgnum: 0,
            rx_msgnum: None,
            from: None,
            to: None,
            content: None,
            crc: None,
        }
    }

    fn clear(&mut self) {
        self.state = MsgTyp::Init;
        self.typ = None;
        self.rx_msgnum = None;
        self.from = None;
        self.to = None;
        self.content = None;
        self.crc = None;
    }

    fn parse_init(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::Init);
        let hdr_idx = buf
            .windows(2)
            .enumerate()
            .find(|&(_, byte)| byte == HDR)
            .map(|(idx, _)| idx + 2);
        match hdr_idx {
            None => Ok(false),
            Some(idx) => {
                let _ = buf.split_to(idx);
                self.state = ParseState::Typ;
                Ok(true)
            }
        }
    }

    fn parse_typ(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::Typ);
        if buf.len() < 1 {
            return Ok(false);
        }
        self.typ = Some(byte_to_typ(buf[0])?);
        let _ = buf.split_to(1);
        self.state = ParseState::MsgNum;
        Ok(true)
    }

    fn parse_msgnum(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::MsgNum);
        if buf.len() < 1 {
            return Ok(false);
        }
        self.msgnum = buf[0] as u8;
        let _ = buf.split_to(1);
        self.state = ParseState::From;
        Ok(true)
    }

    /// Parse message from and message to operate the same, so this generic function gets used by
    /// both.
    fn parse_str(&mut self, buf: &mut BytesMut) -> Result<Option<String>, String> {
        let idx = match get_len(buf) {
            None => return Ok(None),
            Some(idx) => idx,
        };
        let contentbuf = buf.split_to(idx);
        let content = match str::from_utf8(&contentbuf) {
            Ok(cont) => cont.to_string(),
            Err(err) => return Err(format!("Could not parse field as a string: {}", err)),
        };
        Ok(Some(content))
    }

    fn parse_from(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::From);
        match self.parse_str(buf)? {
            None => Ok(false),
            Some(from) => {
                self.from = Some(from);
                self.state = ParseState::To;
                Ok(true)
            }
        }
    }

    fn parse_to(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::To);
        match self.parse_str(buf)? {
            None => Ok(false),
            Some(to) => {
                self.to = Some(to);
                self.state = ParseState::Content;
                Ok(true)
            }
        }
    }

    fn parse_content(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::Content);
        let idx = match get_len(buf) {
            None => return Ok(false),
            Some(idx) => idx,
        };
        let contentbuf = buf.split_to(idx).freeze();
        self.content = Some(contentbuf);
        self.state = ParseState::CRC;
        Ok(true)
    }

    fn parse_crc(&mut self, buf: &mut BytesMut) -> Result<bool, String> {
        assert_eq!(self.state, ParseState::CRC);
        if buf.len() < 4 {
            return Ok(false);
        }
        let crcbuf = buf.split_to(4);
        let crc: u32 = NetworkEndian::read_u32(&crcbuf[..]);

        self.crc = Some(crc);
        self.state = ParseState::Done;
        Ok(true)
    }

    /// This is dynamic memory heavy :(
    ///
    /// Raw msg struct:
    /// 0xAA55<msgtyp u8><msgnumber u8><fromlen u32><from String><tolen u32><to String><contentlen u32><content String>0x0<crc32 u32>
    fn assemble(&self) -> Option<(Bytes, u32)> {
        if self.state != ParseState::Done {
            return None;
        }

        if self.typ.is_none() || self.from.is_none() || self.to.is_none() || self.content.is_none()
        {
            return None;
        }


        // We can unwrap because in order to get to ParseState::Done, we have to
        // go through ParseState::CRC, which will only occur if self.crc is set.
        let msg = Message::new(
            self.typ.unwrap().clone(),
            self.rx_msgnum.unwrap().clone(),
            self.from.unwrap().clone(),
            self.to.unwrap().clone(),
            self.content.unwrap().clone()
        );


    }
}

/// Take in a Byte encoding of a message, return a message struct, or a parsing fault. See module
/// doc for message format.
impl Decoder for MessageCodec {
    type Item = Either<Message, String>;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            let parse_result = match self.state {
                // Find the start of a message
                ParseState::Init => self.parse_init(buf),
                // get the message type
                ParseState::Typ => self.parse_typ(buf),
                // Get the sender's supplied message number (used for ACK tracking)
                ParseState::MsgNum => self.parse_msgnum(buf),
                // get the username who sent the message
                ParseState::From => self.parse_from(buf),
                ParseState::To => self.parse_to(buf),
                ParseState::Content => self.parse_content(buf),
                ParseState::CRC => self.parse_crc(buf),
                ParseState::Done => {
                    self.clear();
                    match self.assemble() {
                        None => panic!(
                            "The only way this can occur is because of a bug, because
                             ParseState::Done can only be reached once all fields are parsed
                             successfully."
                        ),
                        Some(msg) => {
                            if msg.crc == self.crc.unwrap() {
                                return Ok(Some(Either::A(msg)));
                            } else {
                                Err("CRC check failed".to_string());
                            }
                        }
                    }
                }
            };
            // True means the parse found data and made forward progress, false means there wasn't
            // sufficient data to parse, Error means the parse failed and the caller should send a
            // NACK::ParseFailure back to the message sender.
            match parse_result {
                Ok(false) => return Ok(None),
                Err(parsefault) => {
                    tio::stderr().poll_write(
                        format!("Error parsing message from {}: {}", self.addr, parsefault)
                    );
                    self.clear();
                    return Ok(Some(Either::B(parsefault)));
                }
                _ => {} // continue parsing
            }
        }
    }
}

/// Takes in a message struct, outputs a byte encoding of the message.
impl Encoder for MessageCodec {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.put(&item.raw()[..]);
        Ok(())
    }
}
