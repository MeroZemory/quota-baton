//! Staged owned-transport primitive; no launcher activation or reset authority.
#[path = "../src/native_resume/framing.rs"]
mod framing;

use framing::{Error, FrameWriter, MAX_FRAME_BYTES, read_frame};
use std::io::{self, BufReader, Cursor, Write};
use std::sync::{Arc, Mutex};

#[test]
fn fragmented_crlf_and_lf_frames_preserve_payloads_and_eof() {
    let mut input = BufReader::with_capacity(1, Cursor::new(b"{\"a\":1}\r\n{}\n"));
    assert_eq!(read_frame(&mut input), Ok(Some(b"{\"a\":1}".to_vec())));
    assert_eq!(read_frame(&mut input), Ok(Some(b"{}".to_vec())));
    assert_eq!(read_frame(&mut input), Ok(None));
}

#[test]
fn partial_eof_empty_and_embedded_delimiters_are_refused() {
    assert_eq!(read_frame(&mut Cursor::new(b"{}")), Err(Error::Truncated));
    assert_eq!(
        read_frame(&mut Cursor::new(b"\r\n")),
        Err(Error::InvalidFrame)
    );
    assert_eq!(
        read_frame(&mut Cursor::new(b"a\rb\n")),
        Err(Error::InvalidFrame)
    );
}

#[test]
fn exact_payload_bound_accepts_both_delimiters_and_refuses_oversize() {
    for delimiter in [b"\n".as_slice(), b"\r\n".as_slice()] {
        let mut input = vec![b'x'; MAX_FRAME_BYTES];
        input.extend_from_slice(delimiter);
        assert_eq!(
            read_frame(&mut Cursor::new(input)).unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );
    }
    let mut input = Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 200]);
    assert_eq!(read_frame(&mut input), Err(Error::Bound));
}

#[derive(Default)]
struct State {
    bytes: Vec<u8>,
    flushes: usize,
    calls: usize,
}

struct Sink {
    state: Arc<Mutex<State>>,
    fail_after: usize,
    fail_flush: bool,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        let left = self.fail_after.saturating_sub(state.bytes.len());
        if left == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let count = bytes.len().min(left).min(2);
        state.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().flushes += 1;
        if self.fail_flush {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}

fn sink(fail_after: usize, fail_flush: bool) -> (FrameWriter<Sink>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State::default()));
    (
        FrameWriter::new(Sink {
            state: state.clone(),
            fail_after,
            fail_flush,
        }),
        state,
    )
}

#[test]
fn every_complete_frame_is_flushed_and_writer_is_reusable() {
    let (mut writer, state) = sink(usize::MAX, false);
    writer.send(b"{\"a\":1}").unwrap();
    writer.send(b"{}").unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.bytes, b"{\"a\":1}\n{}\n");
    assert_eq!(state.flushes, 2);
}

#[test]
fn payload_delimiter_and_flush_failures_poison_without_second_write() {
    for (limit, flush) in [(2, false), (7, false), (usize::MAX, true)] {
        let (mut writer, state) = sink(limit, flush);
        assert_eq!(writer.send(b"{\"a\":1}"), Err(Error::WriteUncertain));
        let calls = state.lock().unwrap().calls;
        assert_eq!(writer.send(b"{}"), Err(Error::Closed));
        assert_eq!(state.lock().unwrap().calls, calls);
    }
}

#[test]
fn invalid_output_is_never_written_and_closes_writer() {
    let (mut writer, state) = sink(usize::MAX, false);
    assert_eq!(writer.send(b"{}\n{}"), Err(Error::InvalidFrame));
    assert_eq!(writer.send(b"{}"), Err(Error::Closed));
    assert_eq!(state.lock().unwrap().calls, 0);
}

#[test]
fn zero_write_poison_and_interrupted_continuation_are_distinct() {
    struct Zero;
    impl Write for Zero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = FrameWriter::new(Zero);
    assert_eq!(writer.send(b"{}"), Err(Error::WriteUncertain));
    assert_eq!(writer.send(b"{}"), Err(Error::Closed));

    struct InterruptedOnce<'a> {
        interrupted: bool,
        bytes: &'a mut Vec<u8>,
    }
    impl Write for InterruptedOnce<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut bytes = Vec::new();
    FrameWriter::new(InterruptedOnce {
        interrupted: false,
        bytes: &mut bytes,
    })
    .send(b"{}")
    .unwrap();
    assert_eq!(bytes, b"{}\n");
}

#[test]
fn read_error_is_a_fixed_category() {
    struct Broken;
    impl io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    assert_eq!(read_frame(&mut BufReader::new(Broken)), Err(Error::Read));
}
