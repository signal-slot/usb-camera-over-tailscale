//! Just enough HTTP/1.1 for the snapshot server: parsing the request head of
//! a browser or `curl`, routing, and building a response. Bodies of requests
//! are ignored; every response is `Connection: close`.

/// Largest request head accepted; anything longer is answered with 431.
pub const MAX_HEAD: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Request target as sent (path and query).
    pub target: String,
}

impl Request {
    /// The path without the query string.
    pub fn path(&self) -> &str {
        self.target.split(['?', '#']).next().unwrap_or("")
    }

    /// Query parameters in order, percent-decoded. A parameter without `=`
    /// has an empty value.
    pub fn query(&self) -> Vec<(String, String)> {
        let Some((_, q)) = self.target.split_once('?') else {
            return Vec::new();
        };
        q.split('#')
            .next()
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the length of the request head, blank line included, once the
/// buffer holds a complete one.
pub fn head_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2))
}

/// Parses the request line; header fields are not needed and are skipped.
pub fn parse_request(head: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(head).ok()?;
    let line = text.lines().next()?.trim_end_matches('\r');
    let mut parts = line.split(' ').filter(|p| !p.is_empty());
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return None;
    }
    if !method.bytes().all(|b| b.is_ascii_uppercase()) || !target.starts_with('/') {
        return None;
    }
    Some(Request {
        method: method.to_string(),
        target: target.to_string(),
    })
}

/// What the server does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `GET /` (or `HEAD /`): one JPEG frame from the camera, after applying
    /// the camera settings given as query parameters.
    Snapshot {
        head_only: bool,
    },
    /// `GET /controls`: the camera's controls with their ranges, as JSON.
    Controls,
    NotFound,
    MethodNotAllowed,
}

pub fn route(req: &Request) -> Route {
    let snapshot = matches!(req.path(), "/" | "/snapshot.jpg" | "/index.html");
    let controls = req.path() == "/controls";
    match (snapshot, controls, req.method.as_str()) {
        (true, _, "GET") => Route::Snapshot { head_only: false },
        (true, _, "HEAD") => Route::Snapshot { head_only: true },
        (_, true, "GET" | "HEAD") => Route::Controls,
        (false, false, _) => Route::NotFound,
        _ => Route::MethodNotAllowed,
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// Status line and headers for a response of `len` bytes. Images are marked
/// uncacheable so that a reload always fetches a fresh frame.
pub fn response_head(status: u16, content_type: &str, len: usize) -> Vec<u8> {
    let mut h = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        reason(status)
    );
    if status == 405 {
        h.push_str("Allow: GET, HEAD\r\n");
    }
    h.push_str("\r\n");
    h.into_bytes()
}

/// A complete plain-text response (for errors).
pub fn text_response(status: u16, body: &str) -> Vec<u8> {
    let body = format!("{body}\n");
    let mut r = response_head(status, "text/plain; charset=utf-8", body.len());
    r.extend_from_slice(body.as_bytes());
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_detection() {
        assert_eq!(head_len(b"GET / HTTP/1.1\r\nHost: x\r\n"), None);
        assert_eq!(head_len(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody"), Some(27));
        assert_eq!(head_len(b"GET / HTTP/1.0\n\n"), Some(16));
    }

    #[test]
    fn request_line() {
        let r = parse_request(b"GET /?t=1 HTTP/1.1\r\nHost: cam\r\n\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.target, "/?t=1");
        assert_eq!(r.path(), "/");
        assert_eq!(route(&r), Route::Snapshot { head_only: false });
        let r = parse_request(b"HEAD /snapshot.jpg HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(route(&r), Route::Snapshot { head_only: true });
        let r = parse_request(b"POST / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(route(&r), Route::MethodNotAllowed);
        let r = parse_request(b"GET /favicon.ico HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(route(&r), Route::NotFound);
        let r = parse_request(b"GET /controls HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(route(&r), Route::Controls);
        let r = parse_request(b"PUT /controls HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(route(&r), Route::MethodNotAllowed);
        assert_eq!(parse_request(b"GET / HTTP/2\r\n\r\n"), None);
        assert_eq!(parse_request(b"GET /\r\n\r\n"), None);
        assert_eq!(parse_request(b"get / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_request(b"GET http://x/ HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_request(b"\x16\x03\x01\x00\xff"), None);
    }

    #[test]
    fn query_parameters() {
        let r = parse_request(
            b"GET /?wb=auto&zoom=200&brightness=-3&note=a%20b+c&flag HTTP/1.1\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            r.query(),
            vec![
                ("wb".to_string(), "auto".to_string()),
                ("zoom".to_string(), "200".to_string()),
                ("brightness".to_string(), "-3".to_string()),
                ("note".to_string(), "a b c".to_string()),
                ("flag".to_string(), String::new()),
            ]
        );
        assert_eq!(
            parse_request(b"GET / HTTP/1.1\r\n\r\n").unwrap().query(),
            vec![]
        );
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn responses() {
        let h = String::from_utf8(response_head(200, "image/jpeg", 12345)).unwrap();
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(h.contains("Content-Type: image/jpeg\r\n"));
        assert!(h.contains("Content-Length: 12345\r\n"));
        assert!(h.contains("Cache-Control: no-store\r\n"));
        assert!(h.ends_with("Connection: close\r\n\r\n"));
        let t = String::from_utf8(text_response(404, "not found")).unwrap();
        assert!(t.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(t.contains("Content-Length: 10\r\n"));
        assert!(t.ends_with("\r\n\r\nnot found\n"));
        let m = String::from_utf8(text_response(405, "x")).unwrap();
        assert!(m.contains("Allow: GET, HEAD\r\n"));
    }
}
