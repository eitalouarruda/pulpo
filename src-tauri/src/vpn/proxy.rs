//! Proxy HTTP CONNECT local em `127.0.0.1:0` (fail-closed, anti-SSRF).
//! Nunca faz fallback para conexão direta.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};

use super::config::is_blocked_ipv4_core;
use super::peer_auth::is_authorized_peer;
use super::tunnel::TunnelHandle;
use super::turn::{
    handle_turn_tcp, TurnCredentials, TURN_VIRTUAL_HOST, TURN_VIRTUAL_IP, TURN_VIRTUAL_PORT,
};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_CONNS: usize = 256;

pub async fn start_proxy(
    tunnel: Arc<std::sync::RwLock<TunnelHandle>>,
    turn_creds: TurnCredentials,
    mut shutdown: watch::Receiver<bool>,
) -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("vpn.error.proxyBind:{e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("vpn.error.proxyBind:{e}"))?
        .port();

    let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        break;
                    };
                    let Ok(permit) = limit.clone().try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let handle = tunnel.read().unwrap().clone();
                    let creds = turn_creds.clone();
                    let mut conn_shutdown = shutdown.clone();
                    tauri::async_runtime::spawn(async move {
                        let _permit = permit;
                        let authorized = tauri::async_runtime::spawn_blocking(move || {
                            is_authorized_peer(peer, port)
                        })
                        .await
                        .unwrap_or(false);
                        if !authorized {
                            if crate::diag::flag("PULPO_VPN_DEBUG") {
                                eprintln!("[pulpo] vpn: rejected unauthorized proxy peer {peer}");
                            }
                            drop(stream);
                            return;
                        }
                        tokio::select! {
                            _ = handle_client(stream, handle, creds) => {}
                            _ = conn_shutdown.changed() => {}
                        }
                    });
                }
            }
        }
    });

    Ok(port)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConnectTarget {
    Host { host: String, port: u16 },
    Ipv4 { ip: Ipv4Addr, port: u16 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    BadRequest,
    MethodNotAllowed,
    Forbidden,
}

pub fn parse_connect_request(raw: &str) -> Result<ConnectTarget, ParseError> {
    let line = raw.lines().next().ok_or(ParseError::BadRequest)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or(ParseError::BadRequest)?;
    let target = parts.next().ok_or(ParseError::BadRequest)?;
    let version = parts.next().ok_or(ParseError::BadRequest)?;
    if parts.next().is_some()
        || (!version.eq_ignore_ascii_case("HTTP/1.1") && !version.eq_ignore_ascii_case("HTTP/1.0"))
    {
        return Err(ParseError::BadRequest);
    }
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err(ParseError::MethodNotAllowed);
    }
    if target.starts_with('[') {
        return Err(ParseError::Forbidden);
    }
    if target
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '@' | '/' | '?' | '#'))
    {
        return Err(ParseError::BadRequest);
    }
    let (host, port_raw) = target.rsplit_once(':').ok_or(ParseError::BadRequest)?;
    if host.is_empty() || host.contains(':') || host.len() > 253 {
        return Err(ParseError::BadRequest);
    }
    let port: u16 = port_raw.parse().map_err(|_| ParseError::BadRequest)?;
    if port == 0 {
        return Err(ParseError::BadRequest);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ip) => {
                if ip == TURN_VIRTUAL_IP && port == TURN_VIRTUAL_PORT {
                    Ok(ConnectTarget::Ipv4 { ip, port })
                } else if is_blocked_ipv4_core(ip) {
                    Err(ParseError::Forbidden)
                } else {
                    Ok(ConnectTarget::Ipv4 { ip, port })
                }
            }
            IpAddr::V6(_) => Err(ParseError::Forbidden),
        };
    }
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return Err(ParseError::Forbidden);
    }
    let lower = host.to_ascii_lowercase();
    if lower == TURN_VIRTUAL_HOST {
        if port != TURN_VIRTUAL_PORT {
            return Err(ParseError::Forbidden);
        }
    } else if lower.ends_with(".internal") {
        return Err(ParseError::Forbidden);
    }
    Ok(ConnectTarget::Host {
        host: host.to_string(),
        port,
    })
}

async fn handle_client(mut stream: TcpStream, tunnel: TunnelHandle, turn_creds: TurnCredentials) {
    let _ = stream.set_nodelay(true);
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    let header_end = match tokio::time::timeout(HEADER_TIMEOUT, async {
        loop {
            if let Some(pos) = find_header_end(&buf) {
                return Ok(pos);
            }
            if buf.len() >= MAX_HEADER_BYTES {
                return Err(());
            }
            let n = stream.read(&mut chunk).await.map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            let remaining = MAX_HEADER_BYTES - buf.len();
            buf.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    })
    .await
    {
        Ok(Ok(pos)) => pos,
        _ => {
            let _ = write_http_error(&mut stream, 400, "Bad Request").await;
            return;
        }
    };

    let Ok(header_str) = std::str::from_utf8(&buf[..header_end]) else {
        let _ = write_http_error(&mut stream, 400, "Bad Request").await;
        return;
    };

    let target = match parse_connect_request(header_str) {
        Ok(t) => t,
        Err(ParseError::MethodNotAllowed) => {
            let _ = write_http_error(&mut stream, 405, "Method Not Allowed").await;
            return;
        }
        Err(ParseError::Forbidden) => {
            let _ = write_http_error(&mut stream, 403, "Forbidden").await;
            return;
        }
        Err(ParseError::BadRequest) => {
            let _ = write_http_error(&mut stream, 400, "Bad Request").await;
            return;
        }
    };

    let is_turn_target = match &target {
        ConnectTarget::Ipv4 { ip, port } => *ip == TURN_VIRTUAL_IP && *port == TURN_VIRTUAL_PORT,
        ConnectTarget::Host { host, port } => {
            host.eq_ignore_ascii_case(TURN_VIRTUAL_HOST) && *port == TURN_VIRTUAL_PORT
        }
    };
    if is_turn_target {
        if stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let trailing = buf[header_end + 4..].to_vec();
        handle_turn_tcp(stream, trailing, tunnel, turn_creds).await;
        return;
    }

    let (ip, port) = match target {
        ConnectTarget::Ipv4 { ip, port } => (ip, port),
        ConnectTarget::Host { host, port } => match tunnel.resolve_ipv4(host).await {
            Ok(ips) => match ips.into_iter().find(|ip| !is_blocked_ipv4_core(*ip)) {
                Some(ip) => (ip, port),
                None => {
                    let _ = write_http_error(&mut stream, 403, "Forbidden").await;
                    return;
                }
            },
            Err(_) => {
                let _ = write_http_error(&mut stream, 502, "Bad Gateway").await;
                return;
            }
        },
    };

    let bridge = match tunnel
        .connect_tcp(SocketAddr::new(IpAddr::V4(ip), port))
        .await
    {
        Ok(bridge) => bridge,
        Err(_) => {
            let _ = write_http_error(&mut stream, 502, "Bad Gateway").await;
            return;
        }
    };

    if stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }

    let trailing = &buf[header_end + 4..];
    if !trailing.is_empty() && bridge.to_tunnel.send(trailing.to_vec()).await.is_err() {
        return;
    }
    bridge.wake.notify_one();

    let (mut client_read, mut client_write) = stream.into_split();
    let to_tunnel = bridge.to_tunnel;
    let wake_writer = bridge.wake.clone();
    let mut from_tunnel = bridge.from_tunnel;
    let wake_reader = bridge.wake;

    let up = async {
        let mut rbuf = vec![0u8; 32 * 1024];
        loop {
            match client_read.read(&mut rbuf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_tunnel.send(rbuf[..n].to_vec()).await.is_err() {
                        break;
                    }
                    wake_writer.notify_one();
                }
            }
        }
        drop(to_tunnel);
        wake_writer.notify_one();
    };

    let down = async {
        while let Some(data) = from_tunnel.recv().await {
            if client_write.write_all(&data).await.is_err() {
                break;
            }
            wake_reader.notify_one();
        }
        let _ = client_write.shutdown().await;
    };

    tokio::pin!(up, down);
    tokio::select! {
        _ = &mut up => {
            let _ = tokio::time::timeout(Duration::from_secs(15), &mut down).await;
        }
        _ = &mut down => {
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut up).await;
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn write_http_error(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    let msg = format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(msg.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_connect_targets() {
        assert_eq!(
            parse_connect_request("CONNECT discord.com:443 HTTP/1.1\r\nHost: discord.com:443"),
            Ok(ConnectTarget::Host {
                host: "discord.com".into(),
                port: 443
            })
        );
        assert_eq!(
            parse_connect_request("CONNECT 162.159.135.232:443 HTTP/1.1"),
            Ok(ConnectTarget::Ipv4 {
                ip: Ipv4Addr::new(162, 159, 135, 232),
                port: 443
            })
        );
        assert_eq!(
            parse_connect_request("CONNECT 192.0.2.1:3478 HTTP/1.1"),
            Ok(ConnectTarget::Ipv4 {
                ip: TURN_VIRTUAL_IP,
                port: TURN_VIRTUAL_PORT
            })
        );
        assert_eq!(
            parse_connect_request("CONNECT turn.pulpo.internal:3478 HTTP/1.1"),
            Ok(ConnectTarget::Host {
                host: TURN_VIRTUAL_HOST.into(),
                port: TURN_VIRTUAL_PORT
            })
        );
    }

    #[test]
    fn blocks_ssrf_and_non_connect_methods() {
        assert!(matches!(
            parse_connect_request("GET https://discord.com HTTP/1.1"),
            Err(ParseError::MethodNotAllowed)
        ));
        for target in [
            "CONNECT 127.0.0.1:8080 HTTP/1.1",
            "CONNECT 10.2.0.1:53 HTTP/1.1",
            "CONNECT 192.168.1.1:80 HTTP/1.1",
            "CONNECT 169.254.169.254:80 HTTP/1.1",
            "CONNECT 100.64.0.1:443 HTTP/1.1",
            "CONNECT 192.0.2.1:80 HTTP/1.1",
            "CONNECT turn.pulpo.internal:80 HTTP/1.1",
            "CONNECT secret.internal:443 HTTP/1.1",
            "CONNECT [::1]:443 HTTP/1.1",
            "CONNECT localhost:3000 HTTP/1.1",
        ] {
            assert!(
                matches!(parse_connect_request(target), Err(ParseError::Forbidden)),
                "{target}"
            );
        }
        for bad in [
            "CONNECT evil.com@discord.com:443 HTTP/1.1",
            "CONNECT discord.com/path:443 HTTP/1.1",
            "CONNECT discord.com:0 HTTP/1.1",
        ] {
            assert!(
                matches!(parse_connect_request(bad), Err(ParseError::BadRequest)),
                "{bad}"
            );
        }
    }
}
