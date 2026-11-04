//! The hand-over of a connected broker stream to the AMQP client.

use std::io;

use async_rs::{Runtime, Tokio};
use lapin::{uri::AMQPUri, Connection, ConnectionProperties};
use tokio::sync::Mutex;

use crate::stream::BrokerStream;

/// Opens the AMQP connection of `uri` over `stream`. Auto-recovery is off, so
/// the client's IO loop asks for a stream once and takes this one: it never
/// connects itself, and its connect step cannot fail. A URI that states no
/// heartbeat proposes [`DEFAULT_HEARTBEAT_SEC`](crate::DEFAULT_HEARTBEAT_SEC),
/// so the link never rests on a broker that disables its own.
pub async fn open_over(mut uri: AMQPUri, stream: BrokerStream) -> lapin::Result<Connection> {
    uri.query.heartbeat.get_or_insert(crate::DEFAULT_HEARTBEAT_SEC);
    let ready = Mutex::new(Some(stream));
    let connect = async move |_: AMQPUri, _: Runtime<Tokio>| -> lapin::Result<BrokerStream> {
        let taken = ready.lock().await.take();
        taken.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "the broker stream was handed over").into()
        })
    };
    let runtime = Runtime::tokio_current();
    Connection::connector(uri, runtime, connect, ConnectionProperties::default()).await
}
