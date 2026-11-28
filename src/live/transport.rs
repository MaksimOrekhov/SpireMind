use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

pub const MAX_FRAME: usize = 256 * 1024;
const PARTIAL_TIMEOUT: Duration = Duration::from_secs(2);

pub enum Receive {
    Frame(Vec<u8>, Instant),
    Idle,
    Eof,
}

#[derive(Default)]
pub struct Frames {
    // One read buffer plus one bounded frame, regardless of sender rate.
    buffer: Vec<u8>,
    complete: Vec<u8>,
    first_byte: Option<Instant>,
}

impl Frames {
    pub fn read(&mut self, input: &mut impl Read) -> io::Result<Receive> {
        loop {
            if self
                .first_byte
                .is_some_and(|t| t.elapsed() >= PARTIAL_TIMEOUT)
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Incomplete Advisor frame",
                ));
            }
            if let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                self.append(end)?;
                self.buffer.remove(0);
                self.first_byte = None;
                let mut frame = std::mem::take(&mut self.complete);
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(Receive::Frame(frame, Instant::now()));
            }
            self.append(self.buffer.len())?;
            let mut chunk = [0; 8192];
            match input.read(&mut chunk) {
                Ok(0) if self.complete.is_empty() => return Ok(Receive::Eof),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Incomplete Advisor frame",
                    ));
                }
                Ok(n) => {
                    self.first_byte.get_or_insert_with(Instant::now);
                    self.buffer.extend(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(Receive::Idle);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn append(&mut self, length: usize) -> io::Result<()> {
        if self.complete.len() + length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Oversize Advisor frame",
            ));
        }
        self.complete.extend(self.buffer.drain(..length));
        Ok(())
    }
}
