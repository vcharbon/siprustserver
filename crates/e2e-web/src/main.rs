//! `e2e-web` — the E2E test-management website (ADR-0018 Phase I).
//!
//! ```sh
//! cargo run -p e2e-web                       # serves ./e2e on 127.0.0.1:8378
//! cargo run -p e2e-web -- --port 9000 --e2e-dir path/to/e2e
//! ```

use std::path::PathBuf;

fn main() {
    let mut e2e_dir = PathBuf::from("e2e");
    let mut runs_root: Option<PathBuf> = None;
    let mut port: u16 = 8378;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--e2e-dir" => e2e_dir = PathBuf::from(args.next().expect("--e2e-dir needs a value")),
            "--runs-root" => {
                runs_root = Some(PathBuf::from(args.next().expect("--runs-root needs a value")))
            }
            "--port" => port = args.next().expect("--port needs a value").parse().expect("port"),
            other => {
                eprintln!("usage: e2e-web [--e2e-dir <dir>] [--runs-root <dir>] [--port <p>]");
                panic!("unknown argument {other:?}");
            }
        }
    }
    let runs_root = runs_root.unwrap_or_else(|| e2e_dir.join("runs"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let app = e2e_web::router(e2e_dir.clone(), runs_root.clone());
        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        eprintln!(
            "e2e-web: serving {} (runs: {}) on http://{addr}/campaigns",
            e2e_dir.display(),
            runs_root.display()
        );
        axum::serve(listener, app).await.expect("serve");
    });
}
