use std::io;
use std::iter;
use std::fmt;
use std::error::Error;
use std::result::Result;
use futures::{future, Future, stream, Stream, BoxFuture};
use futures::sync::mpsc;
use futures::Sink;
use tokio::net::TcpStream;
use tokio::io::{write_all, AsyncRead, AsyncWrite};
use protocol::{Resp, Array, BulkStr, decode_resp, DecodeError, resp_to_buf};
use super::command::{CmdReplySender, CmdReplyReceiver, CommandResult, Command, new_command_task};

pub trait RespHandler {
    fn handle_resp(&mut self, sender: CmdReplySender);
}

pub struct Session {
}

impl Session {
    pub fn new() -> Self {
        Session{}
    }
}

impl RespHandler for Session {
    fn handle_resp(&mut self, reply_sender: CmdReplySender) {
        let reply = Resp::Bulk(BulkStr::Str(String::from("done").into_bytes()));
        reply_sender.send(Ok(reply)).unwrap();
    }
}

pub fn handle_conn<H>(handler: H, sock: TcpStream) -> impl Future<Item = (), Error = SessionError> + Send
   where H: RespHandler + Send + 'static
{
    let (reader, writer) = sock.split();
    let reader = io::BufReader::new(reader);

    let (tx, rx) = mpsc::channel(1024);

    let reader_handler = handle_read(handler, reader, tx);
    let writer_handler = handle_write(writer, rx);

    let handler = reader_handler.select(writer_handler)
        .then(move |_| {
            println!("Connection closed.");
            Result::Ok::<(), SessionError>(())
        });
    handler
}

fn handle_read<H, R>(handler: H, reader: R, tx: mpsc::Sender<CmdReplyReceiver>) -> impl Future<Item = (), Error = SessionError> + Send
    where R: AsyncRead + io::BufRead + Send + 'static, H: RespHandler + Send + 'static
{
    let reader_stream = stream::iter_ok(iter::repeat(()));
    let handler = reader_stream.fold((handler, tx, reader), move |(handler, tx, reader), _| {
        decode_resp(reader)
            .then(|res| {
                let fut : BoxFuture<_, SessionError> = match res {
                    Ok((reader, resp)) => {
                        let (reply_sender, reply_receiver) = new_command_task(Command::new(resp));

                        let mut handler = handler;
                        handler.handle_resp(reply_sender);

                        let sendFut = tx.send(reply_receiver)
                            .map(move |tx| (handler, tx, reader))
                            .map_err(|e| {
                                println!("rx closed");
                                SessionError::Canceled
                            });
                        Box::new(sendFut)
                    },
                    Err(DecodeError::InvalidProtocol) => {
                        let (reply_sender, reply_receiver) = new_command_task(Command::new(Resp::Arr(Array::Nil)));

                        let reply = Resp::Error(String::from("Err invalid protocol").into_bytes());
                        reply_sender.send(Ok(reply)).unwrap();

                        let sendFut = tx.send(reply_receiver)
                            .map_err(|e| {
                                println!("rx closed");
                                SessionError::Canceled
                            })
                            .and_then(move |_tx| future::err(SessionError::InvalidProtocol));
                        Box::new(sendFut)
                    },
                    Err(DecodeError::Io(e)) => {
                        println!("io error: {:?}", e);
                        Box::new(future::err(SessionError::Io(e)))
                    },
                };
                fut
            })
    });
    handler.map(|_| ())
}

fn handle_write<W>(writer: W, rx: mpsc::Receiver<CmdReplyReceiver>) -> impl Future<Item = (), Error = SessionError> + Send
    where W: AsyncWrite + Send + 'static
{
    let handler = rx
        .map_err(|e| SessionError::Canceled)
        .fold(writer, |writer, reply_receiver| {
            reply_receiver.wait_response()
                .map_err(|e| SessionError::Other)
                .then(|res| {
                    let fut : BoxFuture<_, SessionError> = match res {
                        Ok(resp) => {
                            // TODO: Implement encode and use it
                            let mut buf = vec![];
                            resp_to_buf(&mut buf, resp);
                            println!("encode result {:?}", String::from_utf8(buf.clone()));
                            let writeFut = write_all(writer, buf)
                                .map(move |(writer, _)| writer)
                                .map_err(SessionError::Io);
                            Box::new(writeFut)
                        },
                        Err(e) => {
                            // TODO: display error here
                            let writeFut = write_all(writer, "-Err cmd error\r\n")
                                .map(move |(writer, _)| writer)
                                .map_err(SessionError::Io);
                            Box::new(writeFut)
                        },
                    };
                    fut
                })
        });
    handler.map(|_| ())
}

#[derive(Debug)]
pub enum SessionError {
    Io(io::Error),
    InvalidProtocol,
    Canceled,
    Other,  // TODO: remove this
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for SessionError {
    fn description(&self) -> &str {
        "session error"
    }

    fn cause(&self) -> Option<&Error> {
        match self {
            SessionError::Io(err) => Some(err),
            _ => None,
        }
    }
}
