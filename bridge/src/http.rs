//! HTTP side of the bridge: the static file server for the web frontend and
//! the checks applied to WebSocket upgrade requests

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{Config, HANDSHAKE_TIMEOUT};
use crate::rooms::{DEFAULT_ROOM, valid_room_name};

/// Content Security Policy of the web frontend: scripts and connections
/// from the bridge's own origin only (WebAssembly needs 'wasm-unsafe-eval')
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; \
    worker-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; \
    form-action 'none'";

/// Response headers sent with every static file
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    // Cross-origin isolation, required for SharedArrayBuffer
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Cross-Origin-Embedder-Policy", "require-corp"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
    ("Content-Security-Policy", CSP),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-cache"),
];

/// The room named by a WebSocket request path: `/bridge` (default room) or
/// `/bridge/<room>`
pub fn room_from_path(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    match path.strip_prefix("/bridge") {
        Some("" | "/") => Some(DEFAULT_ROOM.to_string()),
        Some(rest) => {
            let name = rest.strip_prefix('/')?.trim_end_matches('/');
            valid_room_name(name).then(|| name.to_string())
        }
        None => None,
    }
}

/// Whether a WebSocket request's Origin is acceptable. Browsers always send
/// Origin; by default it must be the bridge's own origin (the Host header).
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, cfg: &Config) -> bool {
    if cfg.allow_any_origin {
        return true;
    }
    let Some(origin) = origin else {
        // Non-browser clients (tests, tools); they can send any Origin anyway
        return !cfg.require_origin;
    };
    let origin = origin.trim_end_matches('/').to_ascii_lowercase();
    if !cfg.allowed_origins.is_empty() {
        return cfg.allowed_origins.contains(&origin);
    }
    let Some(host) = host else { return false };
    let host = host.to_ascii_lowercase();
    origin == format!("http://{host}") || origin == format!("https://{host}")
}

/// The client's address: the TCP peer, or with `--trust-proxy` the address
/// the reverse proxy appended to X-Forwarded-For
pub fn client_ip(peer: SocketAddr, forwarded_for: Option<&str>, cfg: &Config) -> IpAddr {
    if cfg.trust_proxy
        && let Some(ip) = forwarded_for
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

/// Whether a new connection is a WebSocket upgrade request (peeks at the
/// request headers without consuming them)
pub async fn is_websocket_upgrade(stream: &TcpStream) -> bool {
    let peek = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.peek(&mut buf).await.ok()?;
            let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            if head.contains("\r\n\r\n") || n == buf.len() || n == 0 {
                return Some(head.contains("upgrade: websocket"));
            }
            // Headers still arriving
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, peek)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Content type by file extension
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" => "application/json",
        "css" => "text/css",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

/// Map a request path onto a file below `root`: no traversal, no hidden
/// files
pub fn resolve_static(root: &Path, request_path: &str) -> Option<PathBuf> {
    let path = request_path.split(['?', '#']).next().unwrap_or("/");
    let mut file = root.to_path_buf();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if part.starts_with('.') || part.contains('\\') || part.contains('\0') {
            return None;
        }
        file.push(part);
    }
    if file.is_dir() {
        file.push("index.html");
    }
    file.is_file().then_some(file)
}

/// Serve one static file request (HTTP/1.1, connection closed afterwards)
pub async fn serve_static(mut stream: TcpStream, root: &Path) -> Result<()> {
    let read_head = async {
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await?;
            anyhow::ensure!(n > 0 && head.len() < 16 * 1024, "bad request");
            head.extend_from_slice(&buf[..n]);
        }
        Ok(head)
    };
    let head = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head).await??;
    let head = String::from_utf8_lossy(&head);
    let mut request = head.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (request.next().unwrap_or(""), request.next().unwrap_or("/"));

    let (status, ctype, body) = if method != "GET" && method != "HEAD" {
        (
            "405 Method Not Allowed",
            "text/plain",
            b"method not allowed".to_vec(),
        )
    } else {
        match resolve_static(root, path) {
            Some(file) => ("200 OK", content_type(&file), tokio::fs::read(&file).await?),
            None => ("404 Not Found", "text/plain", b"not found".to_vec()),
        }
    };
    let mut header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in SECURITY_HEADERS {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(&body).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_paths_stay_inside_root() {
        let root = std::env::temp_dir().join(format!("snow-bridge-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("index.html"), "x").unwrap();
        std::fs::write(root.join("sub/a.js"), "y").unwrap();
        std::fs::write(root.join(".secret"), "z").unwrap();

        assert_eq!(resolve_static(&root, "/"), Some(root.join("index.html")));
        assert_eq!(
            resolve_static(&root, "/?autostart=1"),
            Some(root.join("index.html"))
        );
        assert_eq!(
            resolve_static(&root, "/sub/a.js"),
            Some(root.join("sub/a.js"))
        );
        assert_eq!(resolve_static(&root, "/../etc/passwd"), None);
        assert_eq!(resolve_static(&root, "/sub/../index.html"), None);
        assert_eq!(resolve_static(&root, "/.secret"), None, "hidden files");
        assert_eq!(resolve_static(&root, "/missing.js"), None);
        assert_eq!(content_type(Path::new("x.wasm")), "application/wasm");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rooms_from_paths() {
        assert_eq!(room_from_path("/bridge").as_deref(), Some(DEFAULT_ROOM));
        assert_eq!(room_from_path("/bridge/").as_deref(), Some(DEFAULT_ROOM));
        assert_eq!(
            room_from_path("/bridge/bolo-night?x=1").as_deref(),
            Some("bolo-night")
        );
        assert_eq!(room_from_path("/bridge/a/b"), None);
        assert_eq!(room_from_path("/bridge/../x"), None);
        assert_eq!(room_from_path("/bridgex"), None);
        assert_eq!(room_from_path("/"), None);
    }

    #[test]
    fn origins() {
        let cfg = Config::default();
        let host = Some("127.0.0.1:8080");
        assert!(
            origin_allowed(Some("http://127.0.0.1:8080"), host, &cfg),
            "same origin"
        );
        assert!(origin_allowed(Some("HTTPS://127.0.0.1:8080/"), host, &cfg));
        assert!(
            !origin_allowed(Some("https://evil.example"), host, &cfg),
            "cross origin"
        );
        assert!(
            !origin_allowed(Some("http://127.0.0.1:8080"), None, &cfg),
            "no Host"
        );
        assert!(origin_allowed(None, host, &cfg), "non-browser client");

        let strict = Config {
            require_origin: true,
            ..Config::default()
        };
        assert!(!origin_allowed(None, host, &strict));

        let listed = Config {
            allowed_origins: vec!["https://mac.example.com".into()],
            ..Config::default()
        };
        assert!(origin_allowed(
            Some("https://mac.example.com"),
            Some("internal:8080"),
            &listed
        ));
        assert!(!origin_allowed(
            Some("http://internal:8080"),
            Some("internal:8080"),
            &listed
        ));

        let any = Config {
            allow_any_origin: true,
            ..Config::default()
        };
        assert!(origin_allowed(Some("https://evil.example"), host, &any));
    }

    #[test]
    fn client_addresses() {
        let peer: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        let xff = Some("198.51.100.7, 203.0.113.9");
        assert_eq!(
            client_ip(peer, xff, &Config::default()),
            peer.ip(),
            "XFF ignored by default"
        );
        let proxied = Config {
            trust_proxy: true,
            ..Config::default()
        };
        assert_eq!(
            client_ip(peer, xff, &proxied),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(peer, Some("garbage"), &proxied), peer.ip());
    }
}
