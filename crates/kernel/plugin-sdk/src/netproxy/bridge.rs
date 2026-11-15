//! Loopback HTTP proxy in front of SOCKS5, for Bun-based CLIs.
#![allow(
    clippy::cast_possible_truncation,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else
)]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::thread;

use url::Url;

use super::env::ProxyError;

static BRIDGES: OnceLock<Mutex<HashMap<String, SocketAddr>>> = OnceLock::new();

fn bridges() -> &'static Mutex<HashMap<String, SocketAddr>> {
    BRIDGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn listen_loopback() -> io::Result<TcpListener> {
    TcpListener::bind("127.0.0.1:0")
}

/// Loopback HTTP address that carries traffic through the SOCKS proxy `socks`.
pub(crate) fn bridge(socks: &Url) -> Result<SocketAddr, ProxyError> {
    let key = socks.to_string();
    if let Some(addr) = bridges()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .copied()
    {
        return Ok(addr);
    }
    let listener = listen_loopback()
        .map_err(|error| ProxyError(format!("HTTP bridge listen failed: {error}")))?;
    let addr = listener
        .local_addr()
        .map_err(|error| ProxyError(format!("HTTP bridge listen address failed: {error}")))?;
    if carries_to_itself(addr, socks) {
        listener
            .set_nonblocking(true)
            .map_err(|error| ProxyError(format!("HTTP bridge reuse guard failed: {error}")))?;
        drop(listener);
        return Err(ProxyError(format!(
            "no loopback port apart from the SOCKS proxy's {}",
            socks.host_str().unwrap_or("")
        )));
    }
    let socks = socks.clone();
    thread::spawn(move || accept_loop(listener, socks));
    bridges()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, addr);
    Ok(addr)
}

fn carries_to_itself(addr: SocketAddr, socks: &Url) -> bool {
    let Some(port) = socks.port_or_known_default() else {
        return false;
    };
    if port != addr.port() {
        return false;
    }
    let host = socks.host_str().unwrap_or("");
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

fn accept_loop(listener: TcpListener, socks: Url) {
    for stream in listener.incoming().flatten() {
        let socks = socks.clone();
        thread::spawn(move || serve_client(stream, &socks));
    }
}

fn serve_client(mut client: TcpStream, socks: &Url) {
    let request = match read_http_request(&mut client) {
        Some(request) => request,
        None => return,
    };
    if request.method.eq_ignore_ascii_case("CONNECT") {
        match dial_socks(socks, &request.target) {
            Ok(mut upstream) => {
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                let _ = pipe_bidirectional(&mut client, &mut upstream);
            }
            Err(error) => refuse(&mut client, &error.to_string()),
        }
        return;
    }
    if let Ok(mut upstream) = dial_socks(socks, &request.target) {
        let _ = upstream.write_all(request.head.as_bytes());
        let _ = pipe_bidirectional(&mut client, &mut upstream);
    } else {
        refuse(&mut client, "socks connect failed");
    }
}

struct HttpRequest {
    method: String,
    target: String,
    head: String,
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    stream.set_read_timeout(None).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    Some(HttpRequest {
        method,
        target,
        head: text.into_owned(),
    })
}

fn refuse(client: &mut TcpStream, message: &str) {
    let body = message.to_owned();
    let response = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes());
}

fn pipe_bidirectional(left: &mut TcpStream, right: &mut TcpStream) -> io::Result<()> {
    let mut left_read = left.try_clone()?;
    let mut right_read = right.try_clone()?;
    let mut left_write = left.try_clone()?;
    let mut right_write = right.try_clone()?;
    let one = thread::spawn(move || {
        let _ = io::copy(&mut left_read, &mut right_write);
    });
    let _ = io::copy(&mut right_read, &mut left_write);
    let _ = one.join();
    Ok(())
}

fn dial_socks(socks: &Url, target: &str) -> io::Result<TcpStream> {
    let proxy_host = socks
        .host_str()
        .ok_or_else(|| io::Error::other("socks proxy host missing"))?;
    let proxy_port = socks.port_or_known_default().unwrap_or(1080);
    let mut stream = TcpStream::connect((proxy_host, proxy_port))?;
    socks_handshake(&mut stream, socks, target)?;
    Ok(stream)
}

fn socks_handshake(stream: &mut TcpStream, socks: &Url, target: &str) -> io::Result<()> {
    let (host, port) = parse_host_port(target)?;
    let mut methods = vec![0_u8];
    if !socks.username().is_empty() {
        methods.push(2);
    }
    let mut hello = vec![5, methods.len() as u8];
    hello.extend_from_slice(&methods);
    stream.write_all(&hello)?;
    let mut reply = [0_u8; 2];
    stream.read_exact(&mut reply)?;
    if reply[0] != 5 {
        return Err(io::Error::other("not a SOCKS5 proxy"));
    }
    if reply[1] == 2 {
        let user = socks.username();
        let password = socks.password().unwrap_or("");
        if user.len() > 255 || password.len() > 255 {
            return Err(io::Error::other("user or password too long"));
        }
        let mut auth = vec![1, user.len() as u8];
        auth.extend_from_slice(user.as_bytes());
        auth.push(password.len() as u8);
        auth.extend_from_slice(password.as_bytes());
        stream.write_all(&auth)?;
        stream.read_exact(&mut reply)?;
        if reply[1] != 0 {
            return Err(io::Error::other(
                "the proxy turned the user and password away",
            ));
        }
    } else if reply[1] != 0 {
        return Err(io::Error::other("no authentication method the proxy takes"));
    }
    let mut req = vec![5, 1, 0];
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                req.push(1);
                req.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                req.push(4);
                req.extend_from_slice(&v6.octets());
            }
        }
    } else {
        if host.len() > 255 {
            return Err(io::Error::other("host name too long"));
        }
        req.push(3);
        req.push(host.len() as u8);
        req.extend_from_slice(host.as_bytes());
    }
    req.extend_from_slice(&(port.to_be_bytes()));
    stream.write_all(&req)?;
    let mut header = [0_u8; 4];
    stream.read_exact(&mut header)?;
    if header[1] != 0 {
        return Err(io::Error::other("socks connect refused"));
    }
    let skip = match header[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len)?;
            usize::from(len[0])
        }
        _ => return Err(io::Error::other("unknown socks address type")),
    };
    let mut skip_buf = vec![0_u8; skip + 2];
    stream.read_exact(&mut skip_buf)?;
    Ok(())
}

fn parse_host_port(target: &str) -> io::Result<(String, u16)> {
    if let Some((host, port)) = target.split_once(':') {
        let port = port
            .parse::<u16>()
            .map_err(|error| io::Error::other(error.to_string()))?;
        return Ok((host.to_owned(), port));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        let url = Url::parse(target).map_err(|error| io::Error::other(error.to_string()))?;
        let host = url.host_str().unwrap_or("").to_owned();
        let port = url
            .port_or_known_default()
            .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
        return Ok((host, port));
    }
    Err(io::Error::other("bad proxy target"))
}

/// Redacts credentials from `url` for error messages.
#[must_use]
pub(crate) fn redact(url: &Url) -> String {
    if url.username().is_empty() && url.password().is_none() {
        return url.to_string();
    }
    let mut out = url.clone();
    let _ = out.set_username("");
    let _ = out.set_password(None);
    out.to_string()
}
