use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout};

use meersh_value::Value;
use meersh_value::protocol::{Decoder, RS};

use crate::{Result, ShellError};

/// A lazy stream of values. Items are results so that a failure in the middle of a stream,
/// for example a failed LLM batch, reaches the user instead of silently ending the stream.
pub type ValueStream = Box<dyn Iterator<Item = Result<Value>> + Send>;

/// What flows between two pipeline stages.
pub enum PipelineData {
    Empty,
    Value(Value),
    Stream(ValueStream),
    /// Raw bytes from an external program. They turn into values only when a builtin
    /// consumes them. Between two externals they stay an OS pipe.
    Bytes(ByteStream),
}

pub enum ByteSource {
    Child(ChildStdout),
    Reader(Box<dyn Read + Send>),
}

pub struct ByteStream {
    pub source: ByteSource,
    /// Every process upstream of these bytes. They are reaped when the stream ends.
    pub children: Vec<Child>,
    /// The read end of the object channel on file descriptor 3 (protocol v2).
    pub objects: Option<File>,
}

impl PipelineData {
    pub fn stream(iter: impl Iterator<Item = Result<Value>> + Send + 'static) -> Self {
        PipelineData::Stream(Box::new(iter))
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, PipelineData::Empty | PipelineData::Value(Value::Nothing))
    }

    /// Iterates the input as values. A list yields its items. Bytes yield objects if the
    /// producer spoke the object protocol and lines of text otherwise.
    pub fn into_stream(self) -> ValueStream {
        match self {
            PipelineData::Empty | PipelineData::Value(Value::Nothing) => Box::new(std::iter::empty()),
            PipelineData::Value(Value::List(items)) => Box::new(items.into_iter().map(Ok)),
            PipelineData::Value(v) => Box::new(std::iter::once(Ok(v))),
            PipelineData::Stream(s) => s,
            PipelineData::Bytes(b) => b.into_values(),
        }
    }

    /// Collects the input into one value. Streams become lists and text becomes a string.
    pub fn into_value(self) -> Result<Value> {
        match self {
            PipelineData::Empty => Ok(Value::Nothing),
            PipelineData::Value(v) => Ok(v),
            PipelineData::Stream(s) => Ok(Value::List(s.collect::<Result<_>>()?)),
            PipelineData::Bytes(b) => b.into_value(),
        }
    }
}

impl ByteStream {
    pub fn from_reader(reader: impl Read + Send + 'static) -> Self {
        ByteStream { source: ByteSource::Reader(Box::new(reader)), children: vec![], objects: None }
    }

    fn split(self) -> (Box<dyn Read + Send>, Vec<Child>) {
        let reader: Box<dyn Read + Send> = match (self.source, self.objects) {
            (ByteSource::Child(out), Some(objects)) => choose_channel(out, objects),
            (ByteSource::Child(out), None) => Box::new(out),
            (ByteSource::Reader(r), _) => r,
        };
        (reader, self.children)
    }

    /// Reads everything. Used by `save` and by externals that receive bytes from a builtin.
    pub fn into_reader(self) -> impl Read + Send {
        let (reader, children) = self.split();
        ReapOnDrop { reader: Some(BufReader::new(reader)), children }
    }

    fn into_values(self) -> ValueStream {
        let (reader, children) = self.split();
        let mut reader = BufReader::new(reader);
        let objects = reader.fill_buf().is_ok_and(|b| b.first() == Some(&RS));
        let guard = ReapOnDrop { reader: Some(reader), children };
        if objects { Box::new(Objects { guard, decoder: Decoder::new() }) } else { Box::new(Lines { guard }) }
    }

    fn into_value(self) -> Result<Value> {
        let (reader, children) = self.split();
        let mut reader = BufReader::new(reader);
        if reader.fill_buf().is_ok_and(|b| b.first() == Some(&RS)) {
            let guard = ReapOnDrop { reader: Some(reader), children };
            let values = Objects { guard, decoder: Decoder::new() }.collect::<Result<Vec<_>>>()?;
            return Ok(Value::List(values));
        }
        let mut bytes = Vec::new();
        let mut guard = ReapOnDrop { reader: Some(reader), children };
        guard.reader.as_mut().expect("reader is present until drop").read_to_end(&mut bytes)?;
        Ok(match String::from_utf8(bytes) {
            Ok(mut s) => {
                if s.ends_with('\n') {
                    s.pop();
                }
                Value::String(s)
            }
            Err(e) => Value::Binary(e.into_bytes()),
        })
    }
}

/// Protocol v2. A meersh aware program writes objects to file descriptor 3 and keeps its
/// standard output for humans. It must write to descriptor 3 before it writes to stdout.
/// Whichever channel has data first decides. Objects go down the pipeline and the text of an
/// object program goes to stderr, so it stays visible without mixing into the table.
fn choose_channel(stdout: ChildStdout, mut objects: File) -> Box<dyn Read + Send> {
    use std::os::fd::AsFd;

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let objects_first = {
        let mut fds = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN), PollFd::new(objects.as_fd(), PollFlags::POLLIN)];
        poll(&mut fds, PollTimeout::NONE).is_ok() && fds[1].revents().is_some_and(|r| !r.is_empty())
    };
    if objects_first {
        let mut first = vec![0u8; 8192];
        if let Ok(n) = objects.read(&mut first)
            && n > 0
        {
            first.truncate(n);
            std::thread::spawn(move || {
                let mut stdout = stdout;
                let _ = std::io::copy(&mut stdout, &mut std::io::stderr());
            });
            return Box::new(std::io::Cursor::new(first).chain(objects));
        }
    }
    Box::new(stdout)
}

/// Closes the pipe before waiting, so a producer blocked on a full pipe gets `SIGPIPE`
/// when a consumer like `first 5` stops reading early.
struct ReapOnDrop {
    reader: Option<BufReader<Box<dyn Read + Send>>>,
    children: Vec<Child>,
}

impl Read for ReapOnDrop {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.as_mut().map_or(Ok(0), |r| r.read(buf))
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        self.reader.take();
        for child in &mut self.children {
            let _ = child.wait();
        }
    }
}

struct Lines {
    guard: ReapOnDrop,
}

impl Iterator for Lines {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.guard.reader.as_mut()?;
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) => {
                while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    line.pop();
                }
                Some(Ok(Value::String(String::from_utf8_lossy(&line).into_owned())))
            }
            Err(e) => Some(Err(e.into())),
        }
    }
}

struct Objects {
    guard: ReapOnDrop,
    decoder: Decoder,
}

impl Iterator for Objects {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let reader = self.guard.reader.as_mut()?;
            let mut chunk = Vec::new();
            match reader.read_until(RS, &mut chunk) {
                Ok(0) => return None,
                Ok(_) => {
                    if chunk.last() == Some(&RS) {
                        chunk.pop();
                    }
                    if chunk.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match self.decoder.decode(&chunk) {
                        Ok(Some(value)) => return Some(Ok(value)),
                        Ok(None) => continue,
                        Err(e) => return Some(Err(ShellError::Other(e))),
                    }
                }
                Err(e) => return Some(Err(e.into())),
            }
        }
    }
}
