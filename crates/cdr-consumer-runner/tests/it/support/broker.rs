//! A scripted AMQP 0-9-1 broker on a loopback port: enough of the protocol
//! for one consumer (handshake, channel, queue declare, `basic.consume`), and
//! the stalls a broker or the network between shows.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use amq_protocol::frame::{gen_frame, parse_frame, AMQPFrame, WriteContext};
use amq_protocol::protocol::{basic, channel, connection, queue, AMQPClass};
use amq_protocol::types::{FieldTable, LongString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How the broker treats each connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Script {
    /// Every step answered.
    Serve,
    /// TCP accepted, the AMQP protocol header never answered.
    StallHandshake,
    /// Handshake and channel served with heartbeats off, then nothing
    /// answered from the queue declare on, a close included.
    StallDeclare,
}

pub struct Broker {
    pub addr: SocketAddr,
    /// Connections the client shut down or closed (EOF or reset seen).
    ended_by_client: Arc<AtomicUsize>,
}

impl Broker {
    pub async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ended_by_client = Arc::new(AtomicUsize::new(0));
        let ended = ended_by_client.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let ended = ended.clone();
                tokio::spawn(async move {
                    serve(socket, script).await;
                    ended.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self { addr, ended_by_client }
    }

    pub fn url(&self) -> String {
        format!("amqp://guest:guest@{}/%2f", self.addr)
    }

    pub fn ended_by_client(&self) -> usize {
        self.ended_by_client.load(Ordering::SeqCst)
    }
}

fn method(ch: u16, class: AMQPClass) -> Vec<u8> {
    let mut buf = vec![0u8; 1 << 16];
    let frame = AMQPFrame::Method(ch, class);
    let ctx = gen_frame(&frame)(WriteContext::from(Cursor::new(&mut buf[..]))).expect("frame fits");
    let (_, len) = ctx.into_inner();
    buf.truncate(len as usize);
    buf
}

/// Serves `socket` as `script` says until the client ends it.
async fn serve(socket: TcpStream, script: Script) {
    let (mut rd, mut wr) = socket.into_split();
    let mut chunk = [0u8; 65536];
    if script == Script::StallHandshake {
        while matches!(rd.read(&mut chunk).await, Ok(n) if n > 0) {}
        return;
    }
    let mut header = [0u8; 8];
    if rd.read_exact(&mut header).await.is_err() {
        return;
    }
    let start = connection::Start {
        version_major: 0,
        version_minor: 9,
        server_properties: FieldTable::default(),
        mechanisms: LongString::from("PLAIN AMQPLAIN"),
        locales: LongString::from("en_US"),
    };
    let start = method(0, AMQPClass::Connection(connection::AMQPMethod::Start(start)));
    if wr.write_all(&start).await.is_err() {
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut silent = false;
    loop {
        let n = match rd.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Ok((rest, frame)) = parse_frame(buf.as_slice()) {
            let consumed = buf.len() - rest.len();
            buf.drain(..consumed);
            silent |= script == Script::StallDeclare && is_declare(&frame);
            if silent {
                continue;
            }
            if let Some(reply) = answer(frame) {
                if wr.write_all(&reply).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn is_declare(frame: &AMQPFrame) -> bool {
    matches!(frame, AMQPFrame::Method(_, AMQPClass::Queue(queue::AMQPMethod::Declare(_))))
}

/// The reply to `frame`, if any.
fn answer(frame: AMQPFrame) -> Option<Vec<u8>> {
    use connection::AMQPMethod as C;
    let AMQPFrame::Method(ch, class) = frame else { return None };
    let reply = match class {
        AMQPClass::Connection(C::StartOk(_)) => {
            let tune = connection::Tune { channel_max: 2047, frame_max: 131_072, heartbeat: 0 };
            AMQPClass::Connection(C::Tune(tune))
        }
        AMQPClass::Connection(C::Open(_)) => {
            AMQPClass::Connection(C::OpenOk(connection::OpenOk {}))
        }
        AMQPClass::Connection(C::Close(_)) => {
            AMQPClass::Connection(C::CloseOk(connection::CloseOk {}))
        }
        AMQPClass::Channel(channel::AMQPMethod::Open(_)) => {
            AMQPClass::Channel(channel::AMQPMethod::OpenOk(channel::OpenOk {}))
        }
        AMQPClass::Channel(channel::AMQPMethod::Close(_)) => {
            AMQPClass::Channel(channel::AMQPMethod::CloseOk(channel::CloseOk {}))
        }
        AMQPClass::Queue(queue::AMQPMethod::Declare(d)) => {
            AMQPClass::Queue(queue::AMQPMethod::DeclareOk(queue::DeclareOk {
                queue: d.queue,
                message_count: 0,
                consumer_count: 0,
            }))
        }
        AMQPClass::Basic(basic::AMQPMethod::Consume(c)) => {
            AMQPClass::Basic(basic::AMQPMethod::ConsumeOk(basic::ConsumeOk {
                consumer_tag: c.consumer_tag,
            }))
        }
        AMQPClass::Basic(basic::AMQPMethod::Cancel(c)) => {
            AMQPClass::Basic(basic::AMQPMethod::CancelOk(basic::CancelOk {
                consumer_tag: c.consumer_tag,
            }))
        }
        _ => return None,
    };
    Some(method(ch, reply))
}
