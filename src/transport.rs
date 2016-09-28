use cql_protocol::{Serialize, ParseResult, Parse, requests, responses};
use cql_protocol::requests::{Options, Startup, Query};
use cql_protocol::responses::{Error, Authenticate, Result, SetKeyspace, Prepared, SchemaChange};
use error::CqlError;
use tokio_core::io::{Io, FramedIo};
use tokio_proto::multiplex::{self, Transport, Frame};
use futures::{Async, Poll};
use std::mem;
use std::io::{self, Cursor};

pub enum Request {
    Options(requests::Options),
    Startup(requests::Startup),
    Query(requests::Query),
}

pub enum Response {
    Error(responses::Error),
    Authenticate(responses::Authenticate),
    Supported(responses::Supported),
    Result(responses::Result),
    SetKeyspace(responses::SetKeyspace),
    Prepared(responses::Prepared),
    SchemaChange(responses::SchemaChange),
}

/// Line transport
pub struct CqlTransport<T> {
    // Inner socket
    inner: T,
    // Set to true when inner.read returns Ok(0);
    done: bool,
    // Buffered read data
    rd: Vec<u8>,
    // Current buffer to write to the socket
    wr: io::Cursor<Vec<u8>>,
    // Queued requests
    cmds: Vec<Request>,
}

pub type ReqFrame = Frame<Request, (), CqlError>;
pub type RespFrame = Frame<Response, (), CqlError>;

impl<T> CqlTransport<T>
    where T: Io,
{
    pub fn new(inner: T) -> CqlTransport<T> {
        CqlTransport {
            inner: inner,
            done: false,
            rd: vec![],
            wr: io::Cursor::new(vec![]),
            cmds: vec![],
        }
    }
}

impl<T> CqlTransport<T>
    where T: Io,
{
    fn wr_is_empty(&self) -> bool {
        self.wr_remaining() == 0
    }

    fn wr_remaining(&self) -> usize {
        self.wr.get_ref().len() - self.wr_pos()
    }

    fn wr_pos(&self) -> usize {
        self.wr.position() as usize
    }

    fn wr_flush(&mut self) -> io::Result<bool> {
        // Making the borrow checker happy
        let res = {
            let buf = {
                let pos = self.wr.position() as usize;
                let buf = &self.wr.get_ref()[pos..];

                trace!("writing; remaining={:?}", buf);

                buf
            };

            self.inner.write(buf)
        };

        match res {
            Ok(mut n) => {
                n += self.wr.position() as usize;
                self.wr.set_position(n as u64);
                Ok(true)
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(false);
                }

                trace!("transport flush error; err={:?}", e);
                return Err(e)
            }
        }
    }
}

impl<T> FramedIo for CqlTransport<T>
    where T: Io,
{
    type In = ReqFrame;
    type Out = RespFrame;

    fn poll_read(&mut self) -> Async<()> {
        self.inner.poll_read()
    }

    /// Read a message from the `Transport`
    fn read(&mut self) -> Poll<RespFrame, io::Error> {
        // Not at all a smart implementation, but it gets the job done.

        // First fill the buffer
        while !self.done {
            match self.inner.read_to_end(&mut self.rd) {
                Ok(0) => {
                    self.done = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }

                    return Err(e)
                }
            }
        }

        // Try to parse some data!
        let pos;
        let ret = {
            let mut cursor = Cursor::new(&self.rd);
            let res = {
                let mut parser = Parser::new(&mut cursor);
                parser.parse_value()
            };
            pos = cursor.position() as usize;

            match res {
                Ok(val) => Ok(Async::Ready(Frame::Message(val))),
                Err(e) => e.into(),
            }
        };

        match ret {
            Ok(Async::NotReady) => {},
            _ => {
                // Data is consumed
                let tail = self.rd.split_off(pos);
                mem::replace(&mut self.rd, tail);
            }
        }

        ret
    }

    fn poll_write(&mut self) -> Async<()> {
        // Always allow writing... this isn't really the best strategy to do in
        // practice, but it is the easiest to implement in this case. The
        // number of in-flight requests can be controlled using the pipeline
        // dispatcher.
        Async::Ready(())
    }

    /// Write a message to the `Transport`
    fn write(&mut self, req: ReqFrame) -> Poll<(), io::Error> {
        match req {
            Frame::Message(cmd) => {
                // Queue the command to be written
                self.cmds.push(cmd);

                // Try to flush the write queue
                self.flush()
            },
            Frame::MessageWithBody(..) => unimplemented!(),
            Frame::Body(..) => unimplemented!(),
            Frame::Error(_) => unimplemented!(),
            Frame::Done => unimplemented!(),
        }
    }

    /// Flush pending writes to the socket
    fn flush(&mut self) -> Poll<(), io::Error> {
        loop {
            // If the current write buf is empty, try to refill it
            if self.wr_is_empty() {
                // If there are no pending commands, then all commands have
                // been fully written
                if self.cmds.is_empty() {
                    return Ok(Async::Ready(()));
                }

                // Get the next command
                let cmd = self.cmds.remove(0);

                // Queue it for writting
                self.wr = Cursor::new(cmd.get_packed_command());
            }

            // Try to write the remaining buffer
            if !try!(self.wr_flush()) {
                return Ok(Async::NotReady);
            }
        }
    }
}
