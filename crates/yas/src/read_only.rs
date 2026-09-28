//! Making a session read-only on its way to the server.
//!
//! A relay that carries a client's bytes to a YAS server can hold that client
//! to a read-only session's passive authority without understanding anything
//! past the first frame. [`ReadOnlyIngress`] buffers the preface and the Core
//! HELLO, adds the required read-only-session extension to it ([`restrict`]),
//! and passes every later byte through unchanged. The server, not the relay,
//! then advertises and enforces the restricted catalogue, so a later frame
//! forged to confuse the relay gains nothing. `yas share`'s read-only secrets
//! and `yas server --read-only-sock` both work this way.

use core::fmt;

use crate::core::ClientHello;
use crate::prelude::*;
use crate::{Decode, Encode, Error, Extension, FrameCodec, FrameHeader, FrameLimits, PREFACE};

const READ_ONLY_TAG: u16 = crate::schema::core::CLIENT_HELLO_READ_ONLY_SESSION_EXTENSION as u16;

/// Whether `hello` asks for a read-only session.
pub fn is_read_only(hello: &ClientHello) -> bool {
    hello
        .extensions
        .0
        .iter()
        .any(|extension| extension.tag == READ_ONLY_TAG)
}

/// Make `hello` ask for a read-only session: add the required, empty
/// read-only-session extension in tag order, unless it is there already.
pub fn restrict(hello: &mut ClientHello) {
    if is_read_only(hello) {
        return;
    }
    let position = hello
        .extensions
        .0
        .partition_point(|extension| extension.tag < READ_ONLY_TAG);
    hello.extensions.0.insert(
        position,
        Extension {
            tag: READ_ONLY_TAG,
            required: true,
            value: Vec::new(),
        },
    );
}

/// Why a client's stream cannot be made read-only. The relay should close it:
/// nothing it sent may reach the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyError {
    /// The stream does not start with the native YAS preface.
    NotYas,
    /// The first frame is longer than a frame may be before negotiation.
    HelloTooLarge(u32),
    /// The first frame is not a Core HELLO request, or not a valid one.
    InvalidHello(Error),
}

impl fmt::Display for ReadOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotYas => f.write_str("not a native YAS stream (wrong preface)"),
            Self::HelloTooLarge(length) => write!(
                f,
                "a {length}-byte first frame exceeds the pre-negotiation limit"
            ),
            Self::InvalidHello(error) => write!(f, "invalid HELLO: {error}"),
        }
    }
}

impl core::error::Error for ReadOnlyError {}

/// Rewrites a client-to-server byte stream so its session is read-only.
///
/// Feed it what the client sends, in order, and send the server what it
/// returns. Once [`is_negotiated`](Self::is_negotiated), every chunk comes
/// back unchanged, so a relay may forward later bytes itself.
#[derive(Debug, Default)]
pub struct ReadOnlyIngress {
    pending: Vec<u8>,
    negotiated: bool,
}

impl ReadOnlyIngress {
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            negotiated: false,
        }
    }

    /// Whether the HELLO went through: nothing after it is looked at.
    pub const fn is_negotiated(&self) -> bool {
        self.negotiated
    }

    /// Take the next bytes the client sent. Returns what to send to the
    /// server: `None` while the HELLO is incomplete, then the preface, the
    /// rewritten HELLO and whatever followed it in one chunk, then each chunk
    /// as it came. After an error, close the stream.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, ReadOnlyError> {
        if self.negotiated {
            return Ok((!bytes.is_empty()).then(|| bytes.to_vec()));
        }
        self.pending.extend_from_slice(bytes);
        let compared = self.pending.len().min(PREFACE.len());
        if self.pending[..compared] != PREFACE[..compared] {
            return Err(ReadOnlyError::NotYas);
        }
        if self.pending.len() < PREFACE.len() + 4 {
            return Ok(None);
        }
        let length = u32::from_le_bytes(
            self.pending[PREFACE.len()..PREFACE.len() + 4]
                .try_into()
                .expect("four length bytes"),
        );
        if length > FrameLimits::pre_hello().max_wire_frame {
            return Err(ReadOnlyError::HelloTooLarge(length));
        }
        // Bounded by the pre-negotiation limit just checked.
        let hello_end = PREFACE.len() + 4 + length as usize;
        if self.pending.len() < hello_end {
            return Ok(None);
        }

        let codec = FrameCodec::pre_hello();
        let (mut frame, consumed) = codec
            .decode_stream(&self.pending[PREFACE.len()..hello_end])
            .map_err(ReadOnlyError::InvalidHello)?;
        if consumed != hello_end - PREFACE.len() {
            return Err(ReadOnlyError::InvalidHello(Error::TrailingBytes(
                hello_end - PREFACE.len() - consumed,
            )));
        }
        let request_id = frame.header.request_id.unwrap_or(0);
        if frame.header
            != FrameHeader::request(
                crate::family::CORE,
                crate::core::request_kind::HELLO,
                request_id,
            )
            || frame.header.compressed
        {
            return Err(ReadOnlyError::InvalidHello(Error::Invalid(
                "first frame (not a Core HELLO request)",
            )));
        }
        let mut hello = ClientHello::decode(&frame.payload).map_err(ReadOnlyError::InvalidHello)?;
        restrict(&mut hello);
        frame.payload = hello.encode().map_err(ReadOnlyError::InvalidHello)?;

        let mut output = Vec::with_capacity(self.pending.len() + 8);
        output.extend_from_slice(&PREFACE);
        output.extend_from_slice(
            &codec
                .encode_stream(&frame)
                .map_err(ReadOnlyError::InvalidHello)?,
        );
        output.extend_from_slice(&self.pending[hello_end..]);
        self.pending = Vec::new();
        self.negotiated = true;
        Ok(Some(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Extensions;
    use crate::core::{FamilyOffer, ReceiveLimits};

    fn hello(extensions: Extensions) -> ClientHello {
        ClientHello {
            min_minor: 0,
            max_minor: 0,
            receive: ReceiveLimits {
                max_frame: 1 << 20,
                max_decoded: 1 << 20,
                max_datagram: 0,
                max_buffered: 16 << 20,
            },
            client_instance: [7; 16],
            client_name: "test".into(),
            client_release: "0".into(),
            families: vec![FamilyOffer {
                family_id: crate::family::TERMINAL,
                versions: vec![1],
                required: false,
            }],
            codecs: Vec::new(),
            extensions,
        }
    }

    fn stream(hello: &ClientHello, header: FrameHeader) -> Vec<u8> {
        let frame = crate::Frame {
            header,
            payload: hello.encode().unwrap(),
        };
        // The pre-HELLO codec encodes nothing but a HELLO; an ordinary one
        // makes the frames a client must not open with.
        let codec = FrameCodec::new(FrameLimits::pre_hello(), []).unwrap();
        let mut bytes = PREFACE.to_vec();
        bytes.extend(codec.encode_stream(&frame).unwrap());
        bytes
    }

    fn hello_header() -> FrameHeader {
        FrameHeader::request(crate::family::CORE, crate::core::request_kind::HELLO, 1)
    }

    fn rewritten(output: &[u8]) -> (ClientHello, usize) {
        assert_eq!(&output[..PREFACE.len()], &PREFACE);
        let (frame, consumed) = FrameCodec::pre_hello()
            .decode_stream(&output[PREFACE.len()..])
            .unwrap();
        assert_eq!(frame.header, hello_header());
        (
            ClientHello::decode(&frame.payload).unwrap(),
            PREFACE.len() + consumed,
        )
    }

    #[test]
    fn a_fragmented_hello_comes_out_read_only_and_the_rest_unchanged() {
        let original = hello(Extensions::default());
        let mut bytes = stream(&original, hello_header());
        bytes.extend_from_slice(b"after the hello");
        let mut ingress = ReadOnlyIngress::new();
        let mut output = Vec::new();
        for byte in bytes.chunks(1) {
            assert!(!ingress.is_negotiated() || output.len() > PREFACE.len());
            if let Some(chunk) = ingress.push(byte).unwrap() {
                output.extend_from_slice(&chunk);
            }
        }
        assert!(ingress.is_negotiated());
        let (hello, end) = rewritten(&output);
        assert!(is_read_only(&hello));
        assert_eq!(
            hello.extensions.0,
            vec![Extension {
                tag: READ_ONLY_TAG,
                required: true,
                value: Vec::new(),
            }]
        );
        let mut expected = original.clone();
        restrict(&mut expected);
        assert_eq!(hello, expected);
        assert_eq!(&output[end..], b"after the hello");
        assert_eq!(ingress.push(b"").unwrap(), None);
        assert_eq!(ingress.push(b"more").unwrap().unwrap(), b"more");
    }

    #[test]
    fn extensions_stay_in_tag_order_and_a_read_only_hello_is_unchanged() {
        let idle = Extension {
            tag: crate::schema::core::CLIENT_HELLO_IDLE_TIMEOUT_EXTENSION as u16,
            required: false,
            value: 30u64.to_le_bytes().to_vec(),
        };
        let mut original = hello(Extensions(vec![idle.clone()]));
        let (hello_out, _) = rewritten(
            &ReadOnlyIngress::new()
                .push(&stream(&original, hello_header()))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(hello_out.extensions.0[0], idle);
        assert_eq!(hello_out.extensions.0[1].tag, READ_ONLY_TAG);

        restrict(&mut original);
        let bytes = stream(&original, hello_header());
        assert_eq!(ReadOnlyIngress::new().push(&bytes).unwrap().unwrap(), bytes);
    }

    #[test]
    fn what_is_not_a_native_hello_is_refused() {
        assert_eq!(
            ReadOnlyIngress::new().push(b"GET / HTTP/1.1\r\n"),
            Err(ReadOnlyError::NotYas)
        );

        let mut too_large = PREFACE.to_vec();
        too_large.extend_from_slice(&(FrameLimits::pre_hello().max_wire_frame + 1).to_le_bytes());
        assert!(matches!(
            ReadOnlyIngress::new().push(&too_large),
            Err(ReadOnlyError::HelloTooLarge(_))
        ));

        let not_hello = stream(
            &hello(Extensions::default()),
            FrameHeader::request(crate::family::CORE, crate::core::request_kind::HELLO + 1, 1),
        );
        assert!(matches!(
            ReadOnlyIngress::new().push(&not_hello),
            Err(ReadOnlyError::InvalidHello(_))
        ));
    }
}
