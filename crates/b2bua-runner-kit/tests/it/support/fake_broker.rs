//! A scripted AMQP 0-9-1 broker on a loopback port: enough of the protocol
//! for one publisher (handshake, channel, `confirm.select`, queue declare,
//! `basic.publish`), each publish answered by the next scripted [`Reply`],
//! else the current one, and the fault modes a real broker shows (a stalled
//! handshake, flow control, a socket that stops being read, a connection
//! ended between publishes).

use std::collections::VecDeque;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amq_protocol::frame::{gen_frame, parse_frame, AMQPContentHeader, AMQPFrame, WriteContext};
use amq_protocol::protocol::{basic, channel, confirm, connection, queue, AMQPClass};
use amq_protocol::types::{FieldTable, LongString, ShortString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Notify};

/// How the broker answers the next publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ack,
    /// No ack until every `n`th publish, then one `multiple=true` ack
    /// covering it and every earlier unacked publish.
    AckMultipleEvery(u64),
    /// An ack sent after this delay, the connection meanwhile serving others.
    AckAfter(Duration),
    Nack,
    /// `basic.return` (312 NO_ROUTE) then the ack, as for an unroutable
    /// mandatory publish.
    Return,
    /// No answer, ever.
    Silent,
    /// The broker closes the socket.
    Hangup,
    /// The broker stops reading the socket, keeping it open.
    StopReading,
}

/// How the broker treats a new connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Serve,
    /// TCP accepted, the AMQP protocol header never answered.
    StallHandshake,
    /// Served, `connection.blocked` sent just before the declare-ok, and no
    /// publish of the connection answered: a broker under a resource alarm.
    ServeBlocked,
}

/// How the broker ends a served connection between publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleEnd {
    /// The socket's write side shut down (a FIN), the read side read until
    /// the client ends its own.
    Fin,
    /// `connection.close` with this reply code, the socket closed on the
    /// client's `close-ok` or at the [`CLOSE_TIMEOUT`].
    Close(u16),
    /// `channel.close` with this reply code on the channel the client opened,
    /// the connection kept.
    CloseChannel(u16),
}

/// How long the broker waits for the client's `close-ok` before it closes
/// the socket, as a broker's close handshake timeout. The client acts on the
/// close when it reads it, and its `close-ok` may never reach the wire.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(100);

/// `connection.close`'s reply code for a broker shutting down.
pub const CONNECTION_FORCED: u16 = 320;
/// The reply code of a clean close, which is no error.
pub const REPLY_SUCCESS: u16 = 200;

#[derive(Default)]
pub struct Counters {
    /// TCP connections accepted.
    pub accepted: AtomicUsize,
    /// Connections the client closed or shut down (EOF or reset seen), the
    /// broker having sent no `connection.close`.
    pub ended_by_client: AtomicUsize,
    /// Connections the broker sent `connection.close` on, counted when they
    /// end: at the client's `close-ok`, EOF or reset, or at the close timeout.
    pub closed_by_broker: AtomicUsize,
    /// Channels the broker sent `channel.close` on, counted at the client's
    /// `close-ok`.
    pub channels_closed_by_broker: AtomicUsize,
    /// Queue declares answered.
    pub declared: AtomicUsize,
    /// Publishes received whole (method, header, body).
    pub published: AtomicUsize,
}

/// The replies publishes take: the script's head while one is queued, the
/// current reply otherwise.
struct Replies {
    current: Reply,
    script: VecDeque<Reply>,
}

impl Replies {
    fn next(&mut self) -> Reply {
        self.script.pop_front().unwrap_or(self.current)
    }
}

pub struct FakeBroker {
    pub addr: SocketAddr,
    pub counters: Arc<Counters>,
    reply: Arc<Mutex<Replies>>,
    admit: Arc<Mutex<Admit>>,
    /// Every served connection's order to end itself between publishes.
    idle_ends: broadcast::Sender<IdleEnd>,
}

impl FakeBroker {
    pub async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counters = Arc::new(Counters::default());
        let reply = Arc::new(Mutex::new(Replies { current: reply, script: VecDeque::new() }));
        let admit = Arc::new(Mutex::new(Admit::Serve));
        let idle_ends = broadcast::Sender::new(8);
        let (c, r, a, e) = (counters.clone(), reply.clone(), admit.clone(), idle_ends.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                c.accepted.fetch_add(1, Ordering::SeqCst);
                let mode = *a.lock().unwrap();
                tokio::spawn(serve(socket, mode, c.clone(), r.clone(), e.subscribe()));
            }
        });
        Self { addr, counters, reply, admit, idle_ends }
    }

    /// Shuts down the write side of every connection accepted so far (the
    /// client reads a FIN), each then read until the client ends its side.
    pub fn hang_up_idle(&self) {
        let _ = self.idle_ends.send(IdleEnd::Fin);
    }

    /// Sends `connection.close` with `reply_code` on every connection accepted
    /// so far, as a broker shutting down does; each socket closes on the
    /// client's `close-ok` or at the close timeout.
    pub fn close_idle(&self, reply_code: u16) {
        let _ = self.idle_ends.send(IdleEnd::Close(reply_code));
    }

    /// Sends `channel.close` with `reply_code` on the open channel of every
    /// connection accepted so far, each connection kept open.
    pub fn close_idle_channel(&self, reply_code: u16) {
        let _ = self.idle_ends.send(IdleEnd::CloseChannel(reply_code));
    }

    pub fn url(&self) -> String {
        format!("amqp://guest:guest@{}/%2f", self.addr)
    }

    /// Sets the reply of every publish no script entry answers.
    pub fn reply(&self, reply: Reply) {
        self.reply.lock().unwrap().current = reply;
    }

    /// Queues `replies` for the next publishes the broker reads, one each in
    /// order, whenever they arrive.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.reply.lock().unwrap().script.extend(replies);
    }

    pub fn admit(&self, admit: Admit) {
        *self.admit.lock().unwrap() = admit;
    }

    pub fn count(&self, pick: impl Fn(&Counters) -> &AtomicUsize) -> usize {
        pick(&self.counters).load(Ordering::SeqCst)
    }
}

fn encode(frame: &AMQPFrame) -> Vec<u8> {
    let mut buf = vec![0u8; 1 << 16];
    let ctx = gen_frame(frame)(WriteContext::from(Cursor::new(&mut buf[..]))).expect("frame fits");
    let (_, len) = ctx.into_inner();
    buf.truncate(len as usize);
    buf
}

fn method(ch: u16, class: AMQPClass) -> Vec<u8> {
    encode(&AMQPFrame::Method(ch, class))
}

/// Reads until the client ends the connection, counting it.
async fn drain_until_end(mut rd: tokio::net::tcp::OwnedReadHalf, counters: &Counters) {
    let mut buf = [0u8; 4096];
    loop {
        match rd.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    counters.ended_by_client.fetch_add(1, Ordering::SeqCst);
}

async fn serve(
    socket: TcpStream,
    admit: Admit,
    counters: Arc<Counters>,
    reply: Arc<Mutex<Replies>>,
    mut idle_ends: broadcast::Receiver<IdleEnd>,
) {
    let (mut rd, mut wr) = socket.into_split();
    if admit == Admit::StallHandshake {
        drain_until_end(rd, &counters).await;
        drop(wr);
        return;
    }
    // One writer task: replies and delayed acks share the socket in order,
    // until a FIN shuts its side down.
    let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let fin = Arc::new(Notify::new());
    let fin_ordered = fin.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                bytes = out_rx.recv() => match bytes {
                    Some(bytes) if wr.write_all(&bytes).await.is_ok() => {}
                    _ => break,
                },
                () = fin_ordered.notified() => {
                    let _ = wr.shutdown().await;
                    break;
                }
            }
        }
    });

    let mut header = [0u8; 8];
    if rd.read_exact(&mut header).await.is_err() {
        counters.ended_by_client.fetch_add(1, Ordering::SeqCst);
        return;
    }
    let _ = out.send(method(
        0,
        AMQPClass::Connection(connection::AMQPMethod::Start(connection::Start {
            version_major: 0,
            version_minor: 9,
            server_properties: FieldTable::default(),
            mechanisms: LongString::from("PLAIN AMQPLAIN"),
            locales: LongString::from("en_US"),
        })),
    ));

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut tag: u64 = 0;
    // The publish being received: its channel, routing key and the body bytes
    // still due (unknown until its header).
    let mut publishing: Option<(u16, String, Option<u64>)> = None;
    // Set once the broker sent `connection.close`: when it closes the socket.
    let mut close_by: Option<tokio::time::Instant> = None;
    // The channel the client opened last.
    let mut open_channel: Option<u16> = None;
    loop {
        let close_timeout = async {
            match close_by {
                Some(by) => tokio::time::sleep_until(by).await,
                None => std::future::pending().await,
            }
        };
        let n = tokio::select! {
            read = rd.read(&mut chunk) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            },
            Ok(end) = idle_ends.recv() => {
                match end {
                    IdleEnd::Fin => fin.notify_one(),
                    IdleEnd::Close(reply_code) => {
                        let _ = out.send(method(
                            0,
                            AMQPClass::Connection(connection::AMQPMethod::Close(connection::Close {
                                reply_code,
                                reply_text: ShortString::from("broker forced connection closure"),
                                class_id: 0,
                                method_id: 0,
                            })),
                        ));
                        close_by = Some(tokio::time::Instant::now() + CLOSE_TIMEOUT);
                    }
                    IdleEnd::CloseChannel(reply_code) => {
                        if let Some(ch) = open_channel {
                            let _ = out.send(method(
                                ch,
                                AMQPClass::Channel(channel::AMQPMethod::Close(channel::Close {
                                    reply_code,
                                    reply_text: ShortString::from("broker closed the channel"),
                                    class_id: 0,
                                    method_id: 0,
                                })),
                            ));
                        }
                    }
                }
                continue;
            }
            () = close_timeout => {
                counters.closed_by_broker.fetch_add(1, Ordering::SeqCst);
                return;
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Ok((rest, frame)) = parse_frame(buf.as_slice()) {
            let consumed = buf.len() - rest.len();
            buf.drain(..consumed);
            match frame {
                AMQPFrame::Method(0, AMQPClass::Connection(m)) => match m {
                    connection::AMQPMethod::StartOk(_) => {
                        let _ = out.send(method(
                            0,
                            AMQPClass::Connection(connection::AMQPMethod::Tune(connection::Tune {
                                channel_max: 2047,
                                frame_max: 131_072,
                                heartbeat: 0,
                            })),
                        ));
                    }
                    connection::AMQPMethod::Open(_) => {
                        let _ = out.send(method(
                            0,
                            AMQPClass::Connection(connection::AMQPMethod::OpenOk(
                                connection::OpenOk {},
                            )),
                        ));
                    }
                    connection::AMQPMethod::Close(_) => {
                        let _ = out.send(method(
                            0,
                            AMQPClass::Connection(connection::AMQPMethod::CloseOk(
                                connection::CloseOk {},
                            )),
                        ));
                    }
                    connection::AMQPMethod::CloseOk(_) => {
                        counters.closed_by_broker.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                    _ => {}
                },
                AMQPFrame::Method(ch, AMQPClass::Channel(channel::AMQPMethod::Open(_))) => {
                    open_channel = Some(ch);
                    let _ = out.send(method(
                        ch,
                        AMQPClass::Channel(channel::AMQPMethod::OpenOk(channel::OpenOk {})),
                    ));
                }
                AMQPFrame::Method(ch, AMQPClass::Channel(channel::AMQPMethod::Close(_))) => {
                    let _ = out.send(method(
                        ch,
                        AMQPClass::Channel(channel::AMQPMethod::CloseOk(channel::CloseOk {})),
                    ));
                }
                AMQPFrame::Method(_, AMQPClass::Channel(channel::AMQPMethod::CloseOk(_))) => {
                    counters.channels_closed_by_broker.fetch_add(1, Ordering::SeqCst);
                }
                AMQPFrame::Method(ch, AMQPClass::Confirm(confirm::AMQPMethod::Select(_))) => {
                    let _ = out.send(method(
                        ch,
                        AMQPClass::Confirm(confirm::AMQPMethod::SelectOk(confirm::SelectOk {})),
                    ));
                }
                AMQPFrame::Method(ch, AMQPClass::Queue(queue::AMQPMethod::Declare(d))) => {
                    counters.declared.fetch_add(1, Ordering::SeqCst);
                    if admit == Admit::ServeBlocked {
                        let _ = out.send(method(
                            0,
                            AMQPClass::Connection(connection::AMQPMethod::Blocked(
                                connection::Blocked { reason: ShortString::from("low on memory") },
                            )),
                        ));
                    }
                    let _ = out.send(method(
                        ch,
                        AMQPClass::Queue(queue::AMQPMethod::DeclareOk(queue::DeclareOk {
                            queue: d.queue.clone(),
                            message_count: 0,
                            consumer_count: 0,
                        })),
                    ));
                }
                AMQPFrame::Method(ch, AMQPClass::Basic(basic::AMQPMethod::Publish(p))) => {
                    publishing = Some((ch, p.routing_key.to_string(), None));
                }
                AMQPFrame::Header(_, h) => {
                    if let Some((_, _, due)) = publishing.as_mut() {
                        *due = Some(h.body_size);
                    }
                }
                AMQPFrame::Body(_, data) => {
                    if let Some((_, _, Some(due))) = publishing.as_mut() {
                        *due = due.saturating_sub(data.len() as u64);
                    }
                }
                _ => {}
            }
            // A publish is whole once its header came and its body is complete.
            if let Some((ch, rk, Some(0))) = publishing.as_ref() {
                let (ch, rk) = (*ch, rk.clone());
                publishing = None;
                tag += 1;
                counters.published.fetch_add(1, Ordering::SeqCst);
                let now = if admit == Admit::ServeBlocked {
                    Reply::Silent
                } else {
                    reply.lock().unwrap().next()
                };
                match answer(now, ch, tag, &rk, &out) {
                    Flow::Continue => {}
                    Flow::Hangup => return,
                    Flow::StopReading => {
                        // Keep the socket open and unread until the client ends it.
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
    let ended =
        if close_by.is_some() { &counters.closed_by_broker } else { &counters.ended_by_client };
    ended.fetch_add(1, Ordering::SeqCst);
}

enum Flow {
    Continue,
    Hangup,
    StopReading,
}

fn ack(ch: u16, tag: u64) -> Vec<u8> {
    method(
        ch,
        AMQPClass::Basic(basic::AMQPMethod::Ack(basic::Ack { delivery_tag: tag, multiple: false })),
    )
}

fn answer(reply: Reply, ch: u16, tag: u64, rk: &str, out: &mpsc::UnboundedSender<Vec<u8>>) -> Flow {
    match reply {
        Reply::Ack => {
            let _ = out.send(ack(ch, tag));
        }
        Reply::AckMultipleEvery(n) => {
            if tag.is_multiple_of(n) {
                let _ = out.send(method(
                    ch,
                    AMQPClass::Basic(basic::AMQPMethod::Ack(basic::Ack {
                        delivery_tag: tag,
                        multiple: true,
                    })),
                ));
            }
        }
        Reply::AckAfter(delay) => {
            let out = out.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = out.send(ack(ch, tag));
            });
        }
        Reply::Nack => {
            let _ = out.send(method(
                ch,
                AMQPClass::Basic(basic::AMQPMethod::Nack(basic::Nack {
                    delivery_tag: tag,
                    multiple: false,
                    requeue: false,
                })),
            ));
        }
        Reply::Return => {
            let body = b"returned".to_vec();
            let mut bytes = method(
                ch,
                AMQPClass::Basic(basic::AMQPMethod::Return(basic::Return {
                    reply_code: 312,
                    reply_text: ShortString::from("NO_ROUTE"),
                    exchange: ShortString::from(""),
                    routing_key: ShortString::from(rk),
                })),
            );
            bytes.extend(encode(&AMQPFrame::Header(
                ch,
                AMQPContentHeader {
                    class_id: 60,
                    body_size: body.len() as u64,
                    properties: basic::AMQPProperties::default(),
                },
            )));
            bytes.extend(encode(&AMQPFrame::Body(ch, body)));
            bytes.extend(ack(ch, tag));
            let _ = out.send(bytes);
        }
        Reply::Silent => {}
        Reply::Hangup => return Flow::Hangup,
        Reply::StopReading => return Flow::StopReading,
    }
    Flow::Continue
}
