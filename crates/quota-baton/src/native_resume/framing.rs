//! Bounded stdio frames for the owned native dispatcher. No payload logging.
//! A failed write poisons the writer: partial delivery must never be retried.

use std::io::{BufRead, Write};

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Read,
    Bound,
    Truncated,
    InvalidFrame,
    WriteUncertain,
    Closed,
}

/// LF framing; an optional CR before LF is stripped. EOF between frames is
/// normal. EOF inside a frame is refused, even if its JSON happens to be valid.
pub(crate) fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, Error> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().map_err(|_| Error::Read)?;
        if chunk.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(Error::Truncated)
            };
        }
        let end = chunk.iter().position(|b| *b == b'\n');
        let take = end.unwrap_or(chunk.len());
        // The extra byte allows a CRLF delimiter at the maximum payload size.
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES + 1 {
            return Err(Error::Bound);
        }
        frame.extend_from_slice(&chunk[..take]);
        reader.consume(take + usize::from(end.is_some()));
        if end.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            validate(&frame)?;
            return Ok(Some(frame));
        }
    }
}

fn validate(frame: &[u8]) -> Result<(), Error> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(Error::Bound);
    }
    if frame.is_empty() || frame.iter().any(|b| matches!(*b, b'\r' | b'\n')) {
        return Err(Error::InvalidFrame);
    }
    Ok(())
}

/// The dispatcher owns this value exclusively. No Clone or inner-writer escape.
pub(crate) struct FrameWriter<W> {
    inner: W,
    closed: bool,
}

impl<W: Write> FrameWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            closed: false,
        }
    }

    pub(crate) fn send(&mut self, frame: &[u8]) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if let Err(error) = validate(frame) {
            self.closed = true;
            return Err(error);
        }
        // Poison before the first byte. Only a complete frame + delimiter +
        // flush restores usability; any error is an uncertain native command.
        self.closed = true;
        self.inner
            .write_all(frame)
            .and_then(|()| self.inner.write_all(b"\n"))
            .and_then(|()| self.inner.flush())
            .map_err(|_| Error::WriteUncertain)?;
        self.closed = false;
        Ok(())
    }
}
