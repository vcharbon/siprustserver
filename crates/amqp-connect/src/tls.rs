//! The TLS a broker stream runs, built once: the system's CA roots are read
//! and parsed when it is built, never on a connect.

use std::io;

use lapin::tcp::{RustlsConnector, RustlsConnectorConfig};
use lapin::uri::{AMQPScheme, AMQPUri};

/// How a broker stream is secured: plain for `amqp`; for `amqps` one TLS
/// connector over the system's CA roots, shared by every handshake after it
/// is built. A broker's renewed certificate needs nothing; a CA bundle
/// changed on disk is read by the next process.
#[derive(Clone, Debug)]
pub struct BrokerTls(Option<RustlsConnector>);

impl BrokerTls {
    /// No TLS: what an `amqp` URI runs.
    pub const PLAIN: Self = Self(None);

    /// The TLS `uri`'s scheme asks for. `Err` when an `amqps` URI's roots do
    /// not load: no system CA certificate reads.
    pub fn for_uri(uri: &AMQPUri) -> io::Result<Self> {
        match uri.scheme {
            AMQPScheme::AMQP => Ok(Self::PLAIN),
            AMQPScheme::AMQPS => {
                let config = RustlsConnectorConfig::new_with_native_certs()?;
                Ok(Self(Some(config.connector_with_no_client_auth()?)))
            }
        }
    }

    /// The connector of an `amqps` stream; `None` for a plain one.
    pub(crate) fn connector(&self) -> Option<&RustlsConnector> {
        self.0.as_ref()
    }
}

/// Two are equal when both are plain or both run TLS: every connector is
/// built over the same system roots.
impl PartialEq for BrokerTls {
    fn eq(&self, other: &Self) -> bool {
        self.0.is_some() == other.0.is_some()
    }
}

impl Eq for BrokerTls {}
