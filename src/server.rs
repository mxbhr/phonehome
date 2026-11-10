//! Serves the interface and the current snapshot, on the loopback interface only.
//!
//! A deliberately small HTTP/1.1 server: GET only, one short-lived thread per
//! connection, every response closes the connection.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct Asset {
    path: &'static str,
    mime: &'static str,
    body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $mime:literal, $file:literal) => {
        Asset {
            path: $path,
            mime: $mime,
            body: include_bytes!(concat!("../web/", $file)),
        }
    };
}

const ASSETS: &[Asset] = &[
    asset!("/", "text/html; charset=utf-8", "index.html"),
    asset!("/style.css", "text/css; charset=utf-8", "style.css"),
    asset!("/app.js", "text/javascript; charset=utf-8", "app.js"),
    asset!("/demo.js", "text/javascript; charset=utf-8", "demo.js"),
    asset!(
        "/vendor/d3-array.min.js",
        "text/javascript; charset=utf-8",
        "vendor/d3-array.min.js"
    ),
    asset!(
        "/vendor/d3-geo.min.js",
        "text/javascript; charset=utf-8",
        "vendor/d3-geo.min.js"
    ),
    asset!(
        "/vendor/topojson-client.min.js",
        "text/javascript; charset=utf-8",
        "vendor/topojson-client.min.js"
    ),
    asset!(
        "/vendor/countries-110m.json",
        "application/json",
        "vendor/countries-110m.json"
    ),
    asset!(
        "/vendor/timezones.json",
        "application/json",
        "vendor/timezones.json"
    ),
    asset!(
        "/fonts/bricolage.woff2",
        "font/woff2",
        "fonts/bricolage.woff2"
    ),
];

/// The latest snapshot, serialised once per poll and shared with request handlers.
pub type SharedState = Arc<Mutex<String>>;

const MAX_HEADER_BYTES: u64 = 16 * 1024;

pub fn bind(port: u16) -> anyhow::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))
}

pub fn serve(listener: TcpListener, state: SharedState) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    for stream in listener.incoming().flatten() {
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let _ = handle(stream, port, &state);
        });
    }
}

fn handle(mut stream: TcpStream, port: u16, state: &SharedState) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let mut reader = BufReader::new(stream.try_clone()?.take(MAX_HEADER_BYTES));
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let mut host = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                host = value.trim().to_string();
            }
        }
    }

    // A hostile web page can point its own domain at 127.0.0.1 (DNS rebinding)
    // and read this API from the browser. Only answer to our own names.
    if !host_allowed(&host, port) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"forbidden");
    }
    if method != "GET" {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"method not allowed",
        );
    }

    let path = target.split('?').next().unwrap_or("/");
    if path == "/api/state" {
        let body = state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "{}".to_string());
        return respond(&mut stream, "200 OK", "application/json", body.as_bytes());
    }
    match ASSETS.iter().find(|a| a.path == path) {
        Some(asset) => respond(&mut stream, "200 OK", asset.mime, asset.body),
        None => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: &str, mime: &str, body: &[u8]) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn host_allowed(host: &str, port: u16) -> bool {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .iter()
    .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(port: u16, raw: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(raw.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn start() -> u16 {
        let listener = bind(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(String::from(r#"{"apps":[]}"#)));
        thread::spawn(move || serve(listener, state));
        port
    }

    #[test]
    fn only_answers_to_loopback_names() {
        assert!(host_allowed("127.0.0.1:4747", 4747));
        assert!(host_allowed("localhost:4747", 4747));
        assert!(!host_allowed("evil.example:4747", 4747));
        assert!(!host_allowed("127.0.0.1:9999", 4747));
        assert!(!host_allowed("", 4747));
    }

    #[test]
    fn serves_the_interface_and_the_state() {
        let port = start();
        let page = request(
            port,
            &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(page.starts_with("HTTP/1.1 200 OK"));
        assert!(page.contains("<title>phonehome</title>"));

        let state = request(
            port,
            &format!("GET /api/state?t=1 HTTP/1.1\r\nhost: localhost:{port}\r\n\r\n"),
        );
        assert!(state.ends_with(r#"{"apps":[]}"#));
    }

    #[test]
    fn refuses_rebinding_writes_and_unknown_paths() {
        let port = start();
        let rebound = request(
            port,
            &format!("GET /api/state HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n"),
        );
        assert!(rebound.starts_with("HTTP/1.1 403"));
        let post = request(
            port,
            &format!("POST /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(post.starts_with("HTTP/1.1 405"));
        let missing = request(
            port,
            &format!("GET /../Cargo.toml HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(missing.starts_with("HTTP/1.1 404"));
    }
}
