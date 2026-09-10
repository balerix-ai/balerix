//! A small HTTP stub for unit tests: captures each raw request, answers
//! with a canned status and body.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

pub fn stub_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, mpsc::Receiver<String>) {
    stub_server_n(1, status_line, body)
}

/// Answers `n` requests, one connection each, in arrival order; every raw
/// request goes down the channel.
pub fn stub_server_n(
    n: usize,
    status_line: &'static str,
    body: &'static str,
) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..n {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = sock.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(idx) = text.find("\r\n\r\n") {
                    let len: usize = text[..idx]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= idx + 4 + len {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            tx.send(String::from_utf8_lossy(&buf).to_string()).unwrap();
            write!(
                sock,
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{addr}"), rx)
}
