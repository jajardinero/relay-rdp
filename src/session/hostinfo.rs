//! Identifying the server's operating system from what it sends while signing in.
//!
//! A Windows server's NTLM CHALLENGE message (MS-NLMP 2.2.1.2) carries its version. The
//! sign-in library reads but discards it, so the connection records the bytes the server
//! sends during sign-in and the challenge is found by its signature.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::store::profiles::DetectedOs;

/// Sign-in exchanges are a few kilobytes; recording stops at this size.
const MAX_RECORDED: usize = 64 << 10;
const NTLMSSP_NEGOTIATE_VERSION: u32 = 0x0200_0000;

/// Where a [`Recording`] keeps what it read; `None` stops recording.
pub type Recorded = Arc<Mutex<Option<Vec<u8>>>>;

/// A stream that keeps a copy of what is read from it.
pub struct Recording<S> {
    inner: S,
    recorded: Recorded,
}

impl<S> Recording<S> {
    pub fn new(inner: S, recorded: Recorded) -> Self {
        Self { inner, recorded }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Recording<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Some(recorded) = &mut *self.recorded.lock() {
            let read = &buf.filled()[before..];
            let room = MAX_RECORDED.saturating_sub(recorded.len());
            recorded.extend_from_slice(&read[..read.len().min(room)]);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Recording<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The operating system in the server's NTLM challenge, if it sent one with its version.
pub fn detect(recorded: &[u8]) -> Option<DetectedOs> {
    const SIGNATURE: &[u8] = b"NTLMSSP\0";
    let mut rest = recorded;
    while let Some(at) = rest
        .windows(SIGNATURE.len())
        .position(|window| window == SIGNATURE)
    {
        let message = &rest[at..];
        if let Some(os) = challenge_version(message) {
            return Some(os);
        }
        rest = &message[SIGNATURE.len()..];
    }
    None
}

/// The version in a CHALLENGE message: its flags are at 20 and its version at 48.
fn challenge_version(message: &[u8]) -> Option<DetectedOs> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            message.get(at..at + 4)?.try_into().ok()?,
        ))
    };
    if word(8)? != 2 || word(20)? & NTLMSSP_NEGOTIATE_VERSION == 0 {
        return None;
    }
    let version = message.get(48..56)?;
    let (major, minor) = (version[0], version[1]);
    let build = u16::from_le_bytes([version[2], version[3]]);
    (5..=10).contains(&major).then_some(DetectedOs::Windows {
        major,
        minor,
        build,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CHALLENGE message as Windows sends it, inside some surrounding bytes.
    fn challenge(flags: u32, version: [u8; 8]) -> Vec<u8> {
        let mut message = b"NTLMSSP\0".to_vec();
        message.extend_from_slice(&2u32.to_le_bytes()); // MessageType
        message.extend_from_slice(&[0; 8]); // TargetNameFields
        message.extend_from_slice(&flags.to_le_bytes());
        message.extend_from_slice(&[0x11; 8]); // ServerChallenge
        message.extend_from_slice(&[0; 8]); // Reserved
        message.extend_from_slice(&[0; 8]); // TargetInfoFields
        message.extend_from_slice(&version);
        let mut stream = vec![0x30, 0x82, 0x01, 0x00, 0xa1, 0x05];
        stream.extend_from_slice(&message);
        stream.extend_from_slice(&[0x02, 0x00, 0x0c, 0x00]);
        stream
    }

    const WINDOWS_11: [u8; 8] = [10, 0, 0x27, 0x58, 0, 0, 0, 15]; // build 22567

    #[test]
    fn reads_the_challenge_version() {
        let stream = challenge(0xe28a_8235, WINDOWS_11);
        let expected = DetectedOs::Windows {
            major: 10,
            minor: 0,
            build: 22567,
        };
        assert_eq!(detect(&stream), Some(expected));
    }

    #[test]
    fn ignores_other_messages_and_missing_versions() {
        assert_eq!(detect(&challenge(0xe08a_8235, WINDOWS_11)), None);
        let truncated = challenge(0xe28a_8235, WINDOWS_11);
        assert_eq!(detect(&truncated[..truncated.len() - 12]), None);
        for message_type in [1u32, 3] {
            let mut other = challenge(0xe28a_8235, WINDOWS_11);
            other[14..18].copy_from_slice(&message_type.to_le_bytes());
            assert_eq!(detect(&other), None);
        }
        assert_eq!(
            detect(&challenge(0xe28a_8235, [42, 0, 0, 0, 0, 0, 0, 15])),
            None
        );
        assert_eq!(detect(b"no sign-in here"), None);
    }

    #[test]
    fn finds_a_challenge_after_another_message() {
        let mut stream = b"NTLMSSP\0\x01\x00\x00\x00 negotiate".to_vec();
        stream.extend(challenge(0xe28a_8235, WINDOWS_11));
        assert!(detect(&stream).is_some());
    }

    #[tokio::test]
    async fn recording_keeps_what_was_read_until_stopped() {
        use tokio::io::AsyncReadExt as _;
        let recorded: Recorded = Arc::new(Mutex::new(Some(Vec::new())));
        let mut stream = Recording::new(&b"hello world"[..], Arc::clone(&recorded));
        let mut first = [0; 5];
        stream.read_exact(&mut first).await.unwrap();
        let taken = recorded.lock().take();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.unwrap();
        assert_eq!(taken.as_deref(), Some(&b"hello"[..]));
        assert_eq!(rest, b" world");
        assert!(recorded.lock().is_none());
    }
}
