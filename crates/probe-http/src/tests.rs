//! The probe server's routes over a real socket.

use super::*;
use std::sync::atomic::{AtomicU8, Ordering};

fn routes(body: &'static str, ready: ReadyFn) -> ProbeRoutes {
    ProbeRoutes {
        metrics: Arc::new(move || body.to_string()),
        ready,
        heap: None,
        vrrp: None,
        admin: None,
    }
}
fn always(state: ProbeState) -> ReadyFn {
    Arc::new(move || state)
}

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

#[tokio::test]
async fn serves_metrics_body_from_the_closure() {
    let server = ProbeServer::start(
        "127.0.0.1:0".parse().unwrap(),
        routes("# TYPE demo counter\ndemo 1\n", always(ProbeState::Ready)),
    )
    .await
    .unwrap();
    let resp = get(server.addr(), "/metrics").await;
    assert!(resp.contains("200 OK"));
    assert!(resp.contains("# TYPE demo counter"));
}

#[tokio::test]
async fn healthz_without_a_liveness_is_always_ok() {
    let server = ProbeServer::start(
        "127.0.0.1:0".parse().unwrap(),
        routes("", always(ProbeState::Draining)),
    )
    .await
    .unwrap();
    // Liveness is process-up, independent of readiness — ok even while draining.
    assert!(get(server.addr(), "/healthz").await.contains("200 OK"));
}

#[tokio::test]
async fn healthz_follows_the_liveness_a_server_is_started_with() {
    let live = Arc::new(AtomicU8::new(1));
    let reads = live.clone();
    let server = ProbeServer::start_live(
        "127.0.0.1:0".parse().unwrap(),
        routes("", always(ProbeState::Ready)),
        Arc::new(move || reads.load(Ordering::SeqCst) == 1),
    )
    .await
    .unwrap();
    assert!(get(server.addr(), "/healthz").await.contains("200 OK"));
    live.store(0, Ordering::SeqCst);
    let r = get(server.addr(), "/healthz").await;
    assert!(r.contains("503") && r.contains("stalled"), "{r}");
    assert!(get(server.addr(), "/ready").await.contains("200 OK"), "readiness is apart");
}

#[tokio::test]
async fn readyz_reflects_the_three_states_on_both_path_names() {
    // A flip-able source: 0=NotReady, 1=Ready, 2=Draining.
    let st = Arc::new(AtomicU8::new(1));
    let st2 = st.clone();
    let ready: ReadyFn = Arc::new(move || match st2.load(Ordering::SeqCst) {
        1 => ProbeState::Ready,
        2 => ProbeState::Draining,
        _ => ProbeState::NotReady,
    });
    let server =
        ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes("", ready)).await.unwrap();

    for path in ["/readyz", "/ready"] {
        st.store(1, Ordering::SeqCst);
        let r = get(server.addr(), path).await;
        assert!(r.contains("200 OK") && r.contains("ready"), "{path} ready: {r}");

        st.store(0, Ordering::SeqCst);
        let r = get(server.addr(), path).await;
        assert!(r.contains("503") && r.contains("not-ready"), "{path} not-ready: {r}");

        st.store(2, Ordering::SeqCst);
        let r = get(server.addr(), path).await;
        assert!(r.contains("503") && r.contains("draining"), "{path} draining: {r}");
    }
}

#[tokio::test]
async fn serve_on_runs_the_accept_loop_on_a_pre_bound_listener() {
    // The caller binds (and may retry-wait on) the listener itself; the
    // server only accepts on it — same routes, same behaviour.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server =
        ProbeServer::serve_on(listener, routes("pre-bound\n", always(ProbeState::Ready))).unwrap();
    assert!(get(server.addr(), "/metrics").await.contains("pre-bound"));
    assert!(get(server.addr(), "/healthz").await.contains("200 OK"));
}

#[tokio::test]
async fn serve_on_live_fails_healthz_on_a_pre_bound_listener_while_not_live() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = ProbeServer::serve_on_live(
        listener,
        routes("", always(ProbeState::Ready)),
        Arc::new(|| false),
    )
    .unwrap();
    let r = get(server.addr(), "/healthz").await;
    assert!(r.contains("503") && r.contains("stalled"), "{r}");
}

/// `/vrrp` passes only while the server is live and its own condition
/// holds; readiness plays no part.
#[tokio::test]
async fn vrrp_is_liveness_and_its_own_condition() {
    let (live, holds) = (Arc::new(AtomicU8::new(1)), Arc::new(AtomicU8::new(0)));
    let (live_reads, holds_reads) = (live.clone(), holds.clone());
    let routes = ProbeRoutes {
        vrrp: Some(Arc::new(move || holds_reads.load(Ordering::SeqCst) == 1)),
        admin: None,
        ..routes("", always(ProbeState::NotReady))
    };
    let server = ProbeServer::start_live(
        "127.0.0.1:0".parse().unwrap(),
        routes,
        Arc::new(move || live_reads.load(Ordering::SeqCst) == 1),
    )
    .await
    .unwrap();
    let r = get(server.addr(), "/vrrp").await;
    assert!(r.contains("503") && r.contains("not-ready"), "{r}");
    holds.store(1, Ordering::SeqCst);
    assert!(get(server.addr(), "/vrrp").await.contains("200 OK"));
    live.store(0, Ordering::SeqCst);
    let r = get(server.addr(), "/vrrp").await;
    assert!(r.contains("503") && r.contains("stalled"), "{r}");
}

#[tokio::test]
async fn vrrp_is_404_on_a_server_given_none() {
    let server =
        ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes("", always(ProbeState::Ready)))
            .await
            .unwrap();
    assert!(get(server.addr(), "/vrrp").await.contains("404 Not Found"));
}

async fn request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

/// The operator routes receive the method and the path, query included, and
/// their replies are framed: a body with its length, a `204` with neither.
#[tokio::test]
async fn admin_routes_receive_the_method_and_path_and_their_replies_are_framed() {
    let mut r = routes("", always(ProbeState::Ready));
    r.admin = Some(Arc::new(|method: &str, path: &str| match (method, path) {
        ("POST", "/admin/x") => Some(AdminReply::no_content()),
        ("GET", "/admin/x?y=1") => Some(AdminReply::json(200, r#"{"y":1}"#.into())),
        _ => None,
    }));
    let server = ProbeServer::start("127.0.0.1:0".parse().unwrap(), r).await.unwrap();
    let post = request(server.addr(), "POST", "/admin/x").await;
    assert_eq!(post, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    let get = request(server.addr(), "GET", "/admin/x?y=1").await;
    assert_eq!(
        get,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\
         Connection: close\r\n\r\n{\"y\":1}"
    );
    assert!(request(server.addr(), "DELETE", "/admin/x").await.contains("404 Not Found"));
}

#[tokio::test]
async fn admin_paths_are_404_on_a_server_given_none() {
    let server =
        ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes("", always(ProbeState::Ready)))
            .await
            .unwrap();
    assert!(request(server.addr(), "POST", "/admin/cordon").await.contains("404 Not Found"));
}

#[tokio::test]
async fn unknown_path_is_404() {
    let server =
        ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes("", always(ProbeState::Ready)))
            .await
            .unwrap();
    assert!(get(server.addr(), "/nope").await.contains("404 Not Found"));
}

#[tokio::test]
async fn fully_idle_connection_is_cut() {
    // A connection that sends nothing is dropped after the idle window; the
    // client observes EOF (read returns 0). Short window so the test is fast.
    let server = ProbeServer::start_with(
        "127.0.0.1:0".parse().unwrap(),
        routes("", always(ProbeState::Ready)),
        Duration::from_millis(150),
    )
    .await
    .unwrap();
    let mut s = TcpStream::connect(server.addr()).await.unwrap();
    // Send nothing. Within ~the idle window the server closes the connection.
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .expect("server should have cut the idle connection, not hung")
        .unwrap();
    assert_eq!(n, 0, "fully-idle connection should be closed (EOF)");
}

#[tokio::test]
async fn a_connection_that_sends_a_request_is_not_cut() {
    // The cut is "fully idle only": a real request completes even with a
    // tiny idle window, because the window only guards the wait for bytes.
    let server = ProbeServer::start_with(
        "127.0.0.1:0".parse().unwrap(),
        routes("ok-body\n", always(ProbeState::Ready)),
        Duration::from_millis(150),
    )
    .await
    .unwrap();
    assert!(get(server.addr(), "/metrics").await.contains("ok-body"));
}
