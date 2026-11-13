//! The operator routes over a real operator server: the cordon's status codes,
//! its idempotence, the state report and the states no cordon applies to;
//! the release-all's counts.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use b2bua::release_all::ReleasedAll;
use b2bua::repl::ReadinessSource;
use probe_http::AdminServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::*;

async fn request(addr: SocketAddr, method: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

/// The response's body, after its head.
fn body(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or("")
}

async fn server(readiness: &Readiness, calls: usize) -> AdminServer {
    let release_all: ReleaseAllFn = Arc::new(ReleasedAll::default);
    server_releasing(readiness, calls, release_all).await
}

async fn server_releasing(
    readiness: &Readiness,
    calls: usize,
    release_all: ReleaseAllFn,
) -> AdminServer {
    let routes = admin_routes(
        readiness.clone(),
        Arc::new(move || calls),
        Arc::new(|| Some(Vec::new())),
        release_all,
    );
    AdminServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap()
}

/// A node whose gates never open.
struct NeverReady;

impl ReadinessSource for NeverReady {
    fn all_bootstrapped(&self) -> bool {
        false
    }
    fn all_current(&self) -> bool {
        false
    }
}

/// The report `GET /admin/cordon` answers with `state`, `cordoned` and
/// `calls`: the serving process's pid with them.
fn report_of(state: &str, cordoned: bool, calls: usize) -> serde_json::Value {
    serde_json::json!({
        "state": state, "cordoned": cordoned, "calls": calls, "pid": std::process::id()
    })
}

/// The GET names the process that serves the route, so an operator holding
/// a pid can tell whether the worker answering is that process.
#[tokio::test]
async fn cordon_and_uncordon_answer_204_twice_and_get_reports_the_state_the_calls_and_the_pid() {
    let readiness = Readiness::always_ready();
    let s = server(&readiness, 3).await;
    let get = request(s.addr(), "GET", "/admin/cordon").await;
    assert!(get.starts_with("HTTP/1.1 200 OK") && get.contains("application/json"), "{get}");
    let report: serde_json::Value = serde_json::from_str(body(&get)).unwrap();
    assert_eq!(report, report_of("ready", false, 3));

    for _ in 0..2 {
        let post = request(s.addr(), "POST", "/admin/cordon").await;
        assert!(post.starts_with("HTTP/1.1 204 No Content"), "{post}");
        assert!(body(&post).is_empty() && !post.contains("Content-Length"), "{post}");
    }
    let report: serde_json::Value =
        serde_json::from_str(body(&request(s.addr(), "GET", "/admin/cordon").await)).unwrap();
    assert_eq!(report, report_of("cordoned", true, 3));

    for _ in 0..2 {
        let delete = request(s.addr(), "DELETE", "/admin/cordon").await;
        assert!(delete.starts_with("HTTP/1.1 204 No Content"), "{delete}");
    }
    assert_eq!(readiness.state(), b2bua::repl::ReadinessState::Ready, "uncordoned");
}

/// A query string changes no route.
#[tokio::test]
async fn the_cordon_routes_answer_with_a_query_string() {
    let readiness = Readiness::always_ready();
    let s = server(&readiness, 0).await;
    let post = request(s.addr(), "POST", "/admin/cordon?by=operator").await;
    assert!(post.starts_with("HTTP/1.1 204 No Content"), "{post}");
    assert!(readiness.is_cordoned());
    let get = request(s.addr(), "GET", "/admin/cordon?pretty").await;
    assert!(get.starts_with("HTTP/1.1 200 OK"), "{get}");
}

#[tokio::test]
async fn another_method_is_405_and_another_admin_path_404() {
    let s = server(&Readiness::always_ready(), 0).await;
    let put = request(s.addr(), "PUT", "/admin/cordon").await;
    assert!(put.starts_with("HTTP/1.1 405 Method Not Allowed"), "{put}");
    assert!(put.contains("Allow: GET, POST, DELETE\r\n"), "{put}");
    let other = request(s.addr(), "POST", "/admin/other").await;
    assert!(other.starts_with("HTTP/1.1 404 Not Found"), "{other}");
}

/// A NotReady node takes no cordon; it has none to lift. A draining node
/// takes no cordon and keeps the one it was given.
#[tokio::test]
async fn a_cordon_is_refused_409_on_a_not_ready_and_on_a_draining_node() {
    let not_ready = Readiness::new(Arc::new(NeverReady));
    let s = server(&not_ready, 0).await;
    let post = request(s.addr(), "POST", "/admin/cordon").await;
    assert!(post.starts_with("HTTP/1.1 409 Conflict"), "{post}");
    assert_eq!(body(&post), r#"{"state":"not-ready"}"#);
    assert!(request(s.addr(), "DELETE", "/admin/cordon").await.starts_with("HTTP/1.1 204"));

    let draining = Readiness::always_ready();
    let s = server(&draining, 0).await;
    assert!(request(s.addr(), "POST", "/admin/cordon").await.starts_with("HTTP/1.1 204"));
    draining.set_draining();
    for method in ["POST", "DELETE"] {
        let r = request(s.addr(), method, "/admin/cordon").await;
        assert!(r.starts_with("HTTP/1.1 409 Conflict"), "{method}: {r}");
        assert_eq!(body(&r), r#"{"state":"draining"}"#, "{method}");
    }
    let report: serde_json::Value =
        serde_json::from_str(body(&request(s.addr(), "GET", "/admin/cordon").await)).unwrap();
    assert_eq!(report, report_of("draining", true, 0));
}

/// `POST /admin/release-all` runs the release once per request and answers
/// what it reached; another method runs nothing.
#[tokio::test]
async fn release_all_answers_its_counts_and_runs_once_per_post() {
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = runs.clone();
    let release_all: ReleaseAllFn = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        ReleasedAll { released: 2, aborted: 1 }
    });
    let s = server_releasing(&Readiness::always_ready(), 3, release_all).await;
    let post = request(s.addr(), "POST", "/admin/release-all").await;
    assert!(post.starts_with("HTTP/1.1 200 OK") && post.contains("application/json"), "{post}");
    let reached: serde_json::Value = serde_json::from_str(body(&post)).unwrap();
    assert_eq!(reached, serde_json::json!({"released": 2, "aborted": 1}));
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let get = request(s.addr(), "GET", "/admin/release-all").await;
    assert!(get.starts_with("HTTP/1.1 405 Method Not Allowed"), "{get}");
    assert!(get.contains("Allow: POST\r\n"), "{get}");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "a GET releases nothing");
}

/// `GET /admin/calls` names every live call by its ref and the Call-IDs of
/// its legs, a-leg first: what an observer joins a call it placed to.
#[tokio::test]
async fn the_live_calls_are_listed_with_their_legs_call_ids() {
    let live: LiveCallsFn = Arc::new(|| {
        Some(vec![LiveCall { call_ref: "c1".into(), call_ids: vec!["a@x".into(), "b@y".into()] }])
    });
    let routes = admin_routes(
        Readiness::always_ready(),
        Arc::new(|| 1),
        live,
        Arc::new(ReleasedAll::default),
    );
    let s = AdminServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap();
    let get = request(s.addr(), "GET", "/admin/calls").await;
    assert!(get.starts_with("HTTP/1.1 200 OK"), "{get}");
    let listed: serde_json::Value = serde_json::from_str(body(&get)).unwrap();
    assert_eq!(
        listed,
        serde_json::json!({"calls": [{"call_ref": "c1", "call_ids": ["a@x", "b@y"]}]})
    );
    let post = request(s.addr(), "POST", "/admin/calls").await;
    assert!(post.starts_with("HTTP/1.1 405 Method Not Allowed"), "{post}");
}

/// A worker whose calls cannot be read answers 503, never an empty list an
/// observer would take for "no call left".
#[tokio::test]
async fn unreadable_calls_answer_503() {
    let routes = admin_routes(
        Readiness::always_ready(),
        Arc::new(|| 0),
        Arc::new(|| None),
        Arc::new(ReleasedAll::default),
    );
    let s = AdminServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap();
    let get = request(s.addr(), "GET", "/admin/calls").await;
    assert!(get.starts_with("HTTP/1.1 503"), "{get}");
}
