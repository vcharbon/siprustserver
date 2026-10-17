//! The cordon routes over a real probe server: their status codes, their
//! idempotence, the state report, and the states no cordon applies to.

use std::net::SocketAddr;

use b2bua::repl::ReadinessSource;
use probe_http::{ProbeRoutes, ProbeServer, ProbeState};
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

async fn server(readiness: &Readiness, calls: usize) -> ProbeServer {
    let routes = ProbeRoutes {
        metrics: Arc::new(String::new),
        ready: Arc::new(|| ProbeState::Ready),
        heap: None,
        vrrp: None,
        admin: Some(admin_routes(readiness.clone(), Arc::new(move || calls))),
    };
    ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap()
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

#[tokio::test]
async fn cordon_and_uncordon_answer_204_twice_and_get_reports_the_state_and_the_calls() {
    let readiness = Readiness::always_ready();
    let s = server(&readiness, 3).await;
    let get = request(s.addr(), "GET", "/admin/cordon").await;
    assert!(get.starts_with("HTTP/1.1 200 OK") && get.contains("application/json"), "{get}");
    let report: serde_json::Value = serde_json::from_str(body(&get)).unwrap();
    assert_eq!(report, serde_json::json!({"state": "ready", "cordoned": false, "calls": 3}));

    for _ in 0..2 {
        let post = request(s.addr(), "POST", "/admin/cordon").await;
        assert!(post.starts_with("HTTP/1.1 204 No Content"), "{post}");
        assert!(body(&post).is_empty() && !post.contains("Content-Length"), "{post}");
    }
    let report: serde_json::Value =
        serde_json::from_str(body(&request(s.addr(), "GET", "/admin/cordon").await)).unwrap();
    assert_eq!(report, serde_json::json!({"state": "cordoned", "cordoned": true, "calls": 3}));

    for _ in 0..2 {
        let delete = request(s.addr(), "DELETE", "/admin/cordon").await;
        assert!(delete.starts_with("HTTP/1.1 204 No Content"), "{delete}");
    }
    assert_eq!(readiness.state(), b2bua::repl::ReadinessState::Ready, "uncordoned");
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
    assert_eq!(report, serde_json::json!({"state": "draining", "cordoned": true, "calls": 0}));
}
