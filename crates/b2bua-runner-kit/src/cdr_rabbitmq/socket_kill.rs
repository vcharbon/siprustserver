//! The kill switch of one AMQP connection's socket. The client's IO loop runs
//! on a thread of its own and stops only on a socket error or a close
//! handshake, neither of which a stalled or black-holed broker ever delivers;
//! shutting the socket down is what ends it, so an abandoned connection leaves
//! neither a thread nor a socket behind.

use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};

/// Holds a clone of the connection's socket once connected; [`Self::kill`]
/// shuts it down, now or, when the socket connects after the kill, at once.
#[derive(Debug, Default)]
pub(super) struct SocketKill {
    state: Mutex<KillState>,
}

#[derive(Debug, Default)]
struct KillState {
    killed: bool,
    socket: Option<TcpStream>,
}

impl SocketKill {
    /// Registers the connected socket (a clone sharing its descriptor).
    pub(super) fn arm(&self, socket: TcpStream) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.killed {
            let _ = socket.shutdown(Shutdown::Both);
        } else {
            state.socket = Some(socket);
        }
    }

    /// Shuts the socket down in both directions; idempotent.
    pub(super) fn kill(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.killed = true;
        if let Some(socket) = state.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    /// Whether [`Self::kill`] ran.
    #[cfg(test)]
    pub(super) fn killed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).killed
    }
}

/// Kills the socket when dropped unless [`Self::disarm`]ed: a connect future
/// dropped by its timeout, or failing half-way, ends its connection.
pub(super) struct KillOnDrop(Option<Arc<SocketKill>>);

impl KillOnDrop {
    pub(super) fn new(kill: Arc<SocketKill>) -> Self {
        Self(Some(kill))
    }

    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(kill) = self.0.take() {
            kill.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// A connected pair: the client side and the peer the listener accepted.
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (client, peer)
    }

    fn peer_sees_eof(mut peer: TcpStream) -> bool {
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        matches!(peer.read(&mut [0u8; 1]), Ok(0))
    }

    #[test]
    fn a_kill_shuts_down_the_armed_socket() {
        let (client, peer) = pair();
        let kill = SocketKill::default();
        kill.arm(client.try_clone().unwrap());
        kill.kill();
        assert!(kill.killed());
        assert!(peer_sees_eof(peer), "the peer must see the connection end");
    }

    #[test]
    fn a_socket_armed_after_the_kill_is_shut_down_at_once() {
        let (client, peer) = pair();
        let kill = SocketKill::default();
        kill.kill();
        kill.arm(client.try_clone().unwrap());
        assert!(peer_sees_eof(peer));
    }

    #[test]
    fn a_dropped_guard_kills_and_a_disarmed_one_does_not() {
        let kill = Arc::new(SocketKill::default());
        KillOnDrop::new(kill.clone()).disarm();
        assert!(!kill.killed());
        drop(KillOnDrop::new(kill.clone()));
        assert!(kill.killed());
    }
}
