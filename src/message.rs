//! This is on top of tcp, but if there was a counter in a udp header would work with UDP as well.
//!
//! Message format is intended to look something like this:
//!
//! 0xAA55<msgtype u8><msgnumber u8><msglen u32><username String>0x0<msgto,list>0x0<content>0x0<crc16 u16>
//!
//! msglen is calculated over username, msgto list, and content
//! crc is calculated over everything excluding the header (and excluding the crc of course!)
//!

use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use crc::{crc16, Hasher16};
use futures::future::Either;
use tokio::codec::{Decoder, Encoder};
// its a nice pattern, that's all
pub const HDR: u16 = 0b1010101001010101;

/// Message type. Can either broadcast to all connected peers, or send a private message to a single
/// peer
#[derive(Debug)]
pub enum MessageType {
    BC,
    TO,
    ACK,
    NACK(NACKKind),
}

/// When a NACK is received, this identifies the reason
#[derive(Debug)]
pub enum NACKKind {
    UserBlocked,
    ParseError,
}

const MESSAGEIDS: [(MessageType, u8); 5] = [
    (MessageType::BC, 0),
    (MessageType::TO, 1),
    (MessageType::ACK, 2),
    (MessageType::NACK(NACKKind::UserBlocked), 3),
    (MessageType::NACK(NACKKind::ParseError), 4),
];

#[derive(Debug, Clone)]
enum ParseState {
    Init,
    Len,
    From,
    Type,
    Count,
    To,
    Content,
    Crc,
    Done,
}

#[derive(Debug, Clone)]
//! 0xAA55<msgtype u8><msgnumber u8><msglen u32><username String>0x0<msgto,list>0x0<content>0x0<crc16 u16>
pub struct Message {
    state: ParseState,
    typ: Option<MessageType>,
    msgnum: u8,
    len: Option<u32>,
    from: Option<String>,
    to: Option<Vec<String>>,
    content: Option<Bytes>,
    crc: Option<u16>,
    raw: Option<Bytes>,
}

impl Message {
    fn default() -> Message {
        Message {
            state: ParseState::Init,
            typ: None,
            msgnum: 0,
            len: None,
            from: None,
            to: None,
            content: None,
            crc: None,
            raw: None,
        }
    }

    fn create(
        from: Bytes,
        typ: MessageType,
        number: u8,
        to: Vec<Bytes>,
        content: Bytes,
    ) -> Message {
        let mut msg = Message::default();
        msg.state = ParseState::Done;
        msg.msgnum = number;
        msg.from = Some(from);
        msg.typ = Some(typ);
        msg.to = Some(to);
        msg.content = Some(content);
        msg.len = msg.calculate_len();
        msg.crc = msg.calculate_crc();
        msg.raw = msg.assemble_raw();
        msg
    }

    fn parse_init(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_len(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_from(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_type(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_to(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_content(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn parse_crc(&mut self, buf: &mut BytesMut) -> Result<bool, String> {}

    fn calculate_len(&self) -> Option<u32> {}

    fn calculate_crc(&self) -> Option<u16> {}

    fn assemble_raw(&self) -> Option<Bytes> {}

}

/// Take in a Byte encoding of a message, return a message struct, or a parsing fault.
impl Decoder for Message {
    type Item = Either<Message, String>;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            if self.state == ParseState::Done {
                self.raw = Some(self.assemble_raw());
                if self.calculate_len() != self.len() {
                    return Ok(Some(Either::B("message length check failed")))
                }

                if self.calculate_crc() != self.crc {
                    return Ok(Some(Either::B("CRC check failed")))
                }
                return Ok(Some(Either::A(self)));
            }

            let parse_call = match self.state {
                // Find the start of a message
                ParseState::Init => self.parse_init,
                // get the expected message len
                ParseState::Len => self.parse_len,
                // get the username who sent the message
                ParseState::From => self.parse_from,
                // get the message type
                ParseState::Type => self.parse_type,
                ParseState::Count => self.parse_count,
                ParseState::To => self.parse_to(buf),
                ParseState::Content => self.parse_content,
                ParseState::Crc => self.parse_crc(buf),
                _ => return Err(io::Error("This shouldn't be possible", io::ErrorKind::Other));
            };
            // true means the parse found data and made forward progress, false means there wasn't
            // sufficient data to parse, Error means the parse failed and the caller should send a
            // NACK::ParseFailure back to the message sender.
            match parse_call(buf) {
                Ok(false) => return Ok(None),
                Err(parsefault) => return Ok(Either::B(parsefault)),
                _ => {}
            }
        }
    }
}

/// Takes in a message struct, outputs a byte encoding of the message.
impl Encoder for Message {
    type Item = Message;
    type Error = io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if !item.state == ParseState::Done {
            return Err(io::Error(
                "Message is not populated!",
                io::ErrorKind::InvalidInput,
            ));
        }
        let raw = match item.raw {
            // The unwrap here is counting on ParseState::Done to actually mean the message is fully
            // assembled.
            None => item.assemble_raw().unwrap(),
            Some(raw) => raw,
        };
        dst.put(&raw[..]);
        Ok(())
    }
}
