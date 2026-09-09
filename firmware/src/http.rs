//! HTTP server on the tailnet: `GET /` answers with one JPEG frame from the
//! camera. Requests are served one at a time on the accepting thread; the
//! netstack queues the connections that arrive meanwhile.

use crate::camera::Camera;
use adapter_core::http::{self, Route};
use anyhow::Result;
use log::{info, warn};
use std::time::Duration;
use tsnode::netstack::TcpStream;
use tsnode::node::Node;

/// A browser that opened the connection without sending a request yet.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(node: Node, camera: Camera, port: u16) -> Result<()> {
    let listener = node.listen(port);
    info!("http: listening on tailnet port {port}");
    loop {
        let Some(conn) = listener.accept(Duration::from_secs(1)) else {
            continue;
        };
        serve(&camera, conn);
    }
}

/// Reads the request head, or `None` if the client went away.
fn read_head(conn: &TcpStream) -> Option<Result<Vec<u8>, Vec<u8>>> {
    let mut head = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match conn.read(&mut buf, HEAD_TIMEOUT) {
            Ok(0) => return None,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if let Some(len) = http::head_len(&head) {
                    head.truncate(len);
                    return Some(Ok(head));
                }
                if head.len() > http::MAX_HEAD {
                    return Some(Err(http::text_response(431, "request head too large")));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Some(Err(http::text_response(408, "request timeout")));
            }
            Err(_) => return None,
        }
    }
}

fn serve(camera: &Camera, conn: TcpStream) {
    let peer = conn.peer_addr();
    let response = match read_head(&conn) {
        None => {
            conn.close();
            return;
        }
        Some(Err(r)) => r,
        Some(Ok(head)) => respond(camera, &head, &peer.to_string()),
    };
    if let Err(e) = conn.write_all(&response) {
        warn!("http: {peer}: send failed: {e}");
    }
    conn.close();
}

fn respond(camera: &Camera, head: &[u8], peer: &str) -> Vec<u8> {
    let Some(req) = http::parse_request(head) else {
        warn!("http: {peer}: not an HTTP request");
        return http::text_response(400, "bad request");
    };
    match http::route(&req) {
        Route::Snapshot { head_only } => {
            // Camera settings from the query string, before the capture.
            let applied = match camera.apply_settings(&req.query()) {
                Ok(lines) => lines,
                Err((status, msg)) => {
                    warn!("http: {peer}: {} {}: {msg}", req.method, req.target);
                    return http::text_response(status, &msg);
                }
            };
            // A changed setting shows up a frame or two later.
            let skip = if applied.is_empty() { 0 } else { 2 };
            match camera.snapshot(skip) {
                Ok(jpeg) => {
                    info!(
                        "http: {peer}: {} {} -> {} bytes",
                        req.method,
                        req.target,
                        jpeg.len()
                    );
                    let mut r = http::response_head(200, "image/jpeg", jpeg.len());
                    if !head_only {
                        r.extend_from_slice(&jpeg);
                    }
                    r
                }
                Err(e) => {
                    warn!("http: {peer}: {} {}: {e}", req.method, req.target);
                    let status = if camera.is_attached() { 504 } else { 503 };
                    http::text_response(status, &format!("{e}"))
                }
            }
        }
        Route::Controls => match camera.controls_json() {
            Ok(json) => {
                let mut r = http::response_head(200, "application/json", json.len());
                if req.method == "GET" {
                    r.extend_from_slice(json.as_bytes());
                }
                r
            }
            Err((status, msg)) => http::text_response(status, &msg),
        },
        Route::NotFound => http::text_response(404, "not found; the image is at /"),
        Route::MethodNotAllowed => http::text_response(405, "use GET"),
    }
}
