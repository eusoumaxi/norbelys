//! A stream wrapper that bounds what an IMAP server can make the client hold in memory.
//!
//! `async-imap` reads a literal (`{n}` followed by `n` bytes) into memory as the server declares
//! it, so a hostile or broken server could declare a gigabyte. This wrapper reads the decrypted
//! bytes before the parser does and fails the session as soon as a literal declaration exceeds
//! the configured bound or a protocol line (outside literals) grows past [`MAX_LINE`]. Literal
//! content is skipped byte for byte, so a message body may contain braces freely, and a
//! declaration split across reads is still caught.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::Stream;

/// The longest protocol line outside a literal: room for a `SEARCH` answer listing tens of
/// thousands of UIDs.
pub(super) const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug)]
struct Guard {
    max_literal: usize,
    line: Vec<u8>,
    literal_remaining: usize,
}

fn exceeded() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "an IMAP response exceeds its bound",
    )
}

impl Guard {
    fn check(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &byte in bytes {
            if self.literal_remaining > 0 {
                self.literal_remaining -= 1;
                continue;
            }
            if self.line.len() >= MAX_LINE {
                return Err(exceeded());
            }
            self.line.push(byte);
            if byte != b'\n' {
                continue;
            }
            // A literal declaration ends a protocol line: `{123}` or the non-synchronising
            // `{123+}`, right before the CRLF.
            if let Some(line) = self.line.strip_suffix(b"\r\n")
                && let Some(line) = line.strip_suffix(b"}")
                && let Some(start) = line.iter().rposition(|byte| *byte == b'{')
            {
                let digits = line.get(start + 1..).ok_or_else(exceeded)?;
                let digits = digits.strip_suffix(b"+").unwrap_or(digits);
                let size = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|digits| digits.parse::<usize>().ok())
                    .ok_or_else(exceeded)?;
                if size > self.max_literal {
                    return Err(exceeded());
                }
                self.literal_remaining = size;
            }
            self.line.clear();
        }
        Ok(())
    }
}

/// The wrapped stream: bytes are checked by the guard before the parser sees them.
#[derive(Debug)]
pub(super) struct Bounded {
    inner: Box<dyn Stream>,
    guard: Guard,
}

impl Bounded {
    /// Wraps `inner`, allowing literals of at most `max_literal` bytes.
    pub(super) fn new(inner: Box<dyn Stream>, max_literal: usize) -> Self {
        Self {
            inner,
            guard: Guard {
                max_literal,
                line: Vec::new(),
                literal_remaining: 0,
            },
        }
    }
}

impl AsyncRead for Bounded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut bytes = [0; 8192];
        let limit = output.remaining().min(bytes.len());
        let mut input = ReadBuf::new(bytes.get_mut(..limit).ok_or_else(exceeded)?);
        ready!(Pin::new(&mut self.inner).poll_read(cx, &mut input))?;
        self.guard.check(input.filled())?;
        output.put_slice(input.filled());
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Bounded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Guard, MAX_LINE};

    fn guard(max_literal: usize) -> Guard {
        Guard {
            max_literal,
            line: Vec::new(),
            literal_remaining: 0,
        }
    }

    /// A literal declared above the bound fails the session before the parser reserves memory
    /// for it, even when the declaration arrives split across reads.
    #[test]
    fn an_oversized_literal_is_refused_however_it_is_split() {
        let declaration = b"* 1 FETCH (BODY[] {1073741824}\r\n";
        for chunk_size in 1..=declaration.len() {
            let mut guard = guard(1_024);
            let refused = declaration
                .chunks(chunk_size)
                .any(|chunk| guard.check(chunk).is_err());
            assert!(refused, "chunks of {chunk_size}");
        }
        assert!(
            guard(1_024)
                .check(b"* 1 FETCH (BODY[] {99999999999999999999999}\r\n")
                .is_err()
        );
    }

    /// Literal content is skipped byte for byte, so a message body may contain anything that
    /// looks like a declaration; protocol lines outside literals are bounded too.
    #[test]
    fn literal_content_is_not_read_as_protocol_and_lines_are_bounded() {
        let mut bounded = guard(1_024);
        let content = b"{999999999999}\r\n";
        bounded
            .check(format!("* 1 FETCH (BODY[] {{{}}}\r\n", content.len()).as_bytes())
            .expect("a small literal");
        bounded.check(content).expect("content is skipped");
        bounded
            .check(b")\r\nA1 OK done\r\n")
            .expect("protocol resumes");
        assert_eq!(bounded.literal_remaining, 0);
        assert!(guard(1_024).check(&vec![b'x'; MAX_LINE + 1]).is_err());
    }
}
