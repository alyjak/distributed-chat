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

use std::io;
use std::str;

use byteorder::NetworkEndian;
use bytes::{BufMut, Bytes, BytesMut};
use crc::crc32;
use futures::future::Either;
use futures::sync::mpsc;
use tokio::codec::{Decoder, Encoder};
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
    BC,
    TO,
    Connect,
    Disconnect,
    ACK,
    NACK(NACKKind),
}

/// This is next to typ_to_byte so they can be easily checked to be inverses of one another
fn byte_to_typ(byte: u8) -> Result<MsgTyp, String> {
    match byte {
        01u8 => Ok(MsgTyp::BC),
        02u8 => Ok(MsgTyp::TO),
        20u8 => Ok(MsgTyp::Connect),
        21u8 => Ok(MsgTyp::Disconnect),
        40u8 => Ok(MsgTyp::ACK),
        41u8 => Ok(MsgTyp::NACK(NACKKind::Blocked)),
        42u8 => Ok(MsgTyp::NACK(NACKKind::ParseError)),
        _ => Err(format!("Unknown message code {}", byte)),
    }
}

/// This is next to byte_to_typ so they can be easily checked to be inverses of one another
fn typ_to_byte(typ: MsgTyp) -> u8 {
    match typ {
        MsgTyp::BC => 01u8,
        MsgTyp::TO => 02u8,
        MsgTyp::Connect => 20u8,
        MsgTyp::Disconnect => 21u8,
        MsgTyp::ACK => 40u8,
        MsgTyp::NACK(NACKKind::UserBlocked) => 41u8,
        MsgTyp::NACK(NACKKind::ParseError) => 42u8,
    }
}

/// When a NACK is received, this identifies the reason
#[derive(Debug, Clone)]
pub enum NACKKind {
    UserBlocked,
    ParseError,
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
    state: ParseState,
    typ: Option<MsgTyp>,
    msgnum: u8,
    from: Option<String>,
    to: Option<String>,
    content: Option<Bytes>,
    crc: Option<u32>,
}

impl Message {
    fn default() -> Message {
        Message {
            state: ParseState::Init,
            typ: None,
            msgnum: 0,
            from: None,
            to: None,
            content: None,
            crc: None,
        }
    }

    fn create(typ: MsgTyp, from: String, to: String, content: Bytes) -> Result<Message, io::Error> {
        let max = u32::max_value() as usize;
        if from.as_bytes().len() > max || to.as_bytes().len() > max || content.len() > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Maximum message string size is u32 max value. Either the from, to, or
                                       content fields exceeded that size",
            ));
        }

        let mut msg = Message::default();
        msg.state = ParseState::Done;
        msg.typ = Some(typ);
        msg.from = Some(from);
        msg.to = Some(to);
        msg.content = Some(content);
        Ok(msg)
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
            Err(_) => return Err("Could not parse field as a string".to_string()),
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

    /// like
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
    fn assemble_raw(&self) -> Option<(Bytes, u32)> {
        if self.state != ParseState::Done {
            return None;
        }

        if self.typ.is_none() || self.from.is_none() || self.to.is_none() || self.content.is_none()
        {
            return None;
        }

        let hdr: [u8; 4] = [HDR_0, HDR_1, typ_to_byte(self.typ.unwrap()), self.msgnum];
        let from = self.from.unwrap().as_bytes();
        let fromlen = len_to_bytes(from.len());
        let to = self.to.unwrap().as_bytes();
        let tolen = len_to_bytes(to.len());
        let content = self.content.unwrap();
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

        // Now calculate crc
        Some((raw.freeze(), crc32::checksum_ieee(&raw[..])))
    }
}

/// Take in a Byte encoding of a message, return a message struct, or a parsing fault. See module
/// doc for message format.
impl Decoder for Message {
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
                    match self.assemble_raw() {
                        None => panic!(
                            "The only way this can occur is because of a bug, because
                             ParseState::Done can only be reached once all fields are parsed
                             successfully."
                        ),
                        Some((_, crc)) => {
                            // We can unwrap because in order to get to ParseState::Done, we have to
                            // go through ParseState::CRC, which will only occur if self.crc is set.
                            if crc != self.crc.unwrap() {
                                return Ok(Some(Either::B("CRC check failed".to_string())));
                            }
                            return Ok(Some(Either::A(*self)));
                        }
                    }
                }
            };
            // true means the parse found data and made forward progress, false means there wasn't
            // sufficient data to parse, Error means the parse failed and the caller should send a
            // NACK::ParseFailure back to the message sender.
            match parse_result {
                Ok(false) => return Ok(None),
                Err(parsefault) => return Ok(Some(Either::B(parsefault))),
                _ => {} // continue parsing
            }
        }
    }
}

/// Takes in a message struct, outputs a byte encoding of the message.
impl Encoder for Message {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // Use the encoder to create incrementing message numbers that wrap
        self.msgnum = self.msgnum.wrapping_add(1);
        item.msgnum = self.msgnum;
        match item.assemble_raw() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot encode message, it is not fully populated!",
            )),
            Some((raw, crc)) => {
                let mut crcbuf: [u8; 4];
                NetworkEndian::write_u32(&mut crcbuf, crc);
                dst.put(&raw[..]);
                dst.put(&crcbuf[..]);
                Ok(())
            }
        }
    }
}
