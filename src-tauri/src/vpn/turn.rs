//! Servidor TURN local (RFC 5766) sobre TCP para encaminhar pacotes WebRTC
//! UDP através do túnel WireGuard (`smoltcp`), sem drivers de kernel e sem vazar IP real.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use zeroize::Zeroize;

use super::config::is_blocked_ipv4_core;
use super::tunnel::{TunnelHandle, UdpDatagram};

pub const TURN_VIRTUAL_IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
pub const TURN_VIRTUAL_PORT: u16 = 3478;
pub const TURN_VIRTUAL_HOST: &str = "turn.pulpo.internal";

const REALM: &str = "pulpo";
const NONCE: &str = "pulpo-turn-nonce";
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const DEFAULT_LIFETIME: u32 = 600;
const MAX_STUN_FRAME_LEN: usize = 16 * 1024;
const MAX_UDP_PAYLOAD_LEN: usize = 65507;
const MAX_TURN_FORWARD_BYTES: usize = 65400;
const MAX_TURN_AUTH_FAILURES: u8 = 5;
const TURN_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

const MSG_BINDING_REQUEST: u16 = 0x0001;
const MSG_BINDING_RESPONSE: u16 = 0x0101;
const MSG_ALLOCATE_REQUEST: u16 = 0x0003;
const MSG_ALLOCATE_RESPONSE: u16 = 0x0103;
const MSG_ALLOCATE_ERROR: u16 = 0x0113;
const MSG_REFRESH_REQUEST: u16 = 0x0004;
const MSG_REFRESH_RESPONSE: u16 = 0x0104;
const MSG_REFRESH_ERROR: u16 = 0x0114;
const MSG_SEND_INDICATION: u16 = 0x0016;
const MSG_DATA_INDICATION: u16 = 0x0017;
const MSG_CREATE_PERMISSION_REQUEST: u16 = 0x0008;
const MSG_CREATE_PERMISSION_RESPONSE: u16 = 0x0108;
const MSG_CREATE_PERMISSION_ERROR: u16 = 0x0118;
const MSG_CHANNEL_BIND_REQUEST: u16 = 0x0009;
const MSG_CHANNEL_BIND_RESPONSE: u16 = 0x0109;
const MSG_CHANNEL_BIND_ERROR: u16 = 0x0119;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;

#[derive(Clone)]
pub struct TurnCredentials {
    pub username: String,
    pub password: String,
    key: [u8; 16],
}

impl std::fmt::Debug for TurnCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCredentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Drop for TurnCredentials {
    fn drop(&mut self) {
        self.password.zeroize();
        self.key.zeroize();
    }
}

impl TurnCredentials {
    pub fn generate() -> Self {
        let username = format!("u{}", random_hex_64());
        let password = format!("p{}{}", random_hex_64(), random_hex_64());
        Self::with_credentials(&username, &password)
    }

    pub fn with_credentials(username: &str, password: &str) -> Self {
        let auth_str = format!("{username}:{REALM}:{password}");
        let key = md5(auth_str.as_bytes());
        Self {
            username: username.to_string(),
            password: password.to_string(),
            key,
        }
    }
}

fn random_hex_64() -> String {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable for TURN credentials");
    format!("{:016x}", u64::from_ne_bytes(bytes))
}

pub fn webrtc_patch_script(creds: &TurnCredentials) -> String {
    let url = format!("turn:{TURN_VIRTUAL_IP}:{TURN_VIRTUAL_PORT}?transport=tcp");
    let url_json = serde_json::to_string(&url).unwrap_or_default();
    let user_json = serde_json::to_string(&creds.username).unwrap_or_default();
    let pass_json = serde_json::to_string(&creds.password).unwrap_or_default();

    format!(
        r#"(() => {{
  if (window.top !== window.self) return;
  if (window.__pulpoWebrtcTurnPatched) return;
  window.__pulpoWebrtcTurnPatched = true;
  const pulpoServer = {{
    urls: [{url_json}],
    username: {user_json},
    credential: {pass_json}
  }};
  const rewriteConfig = (cfg) => {{
    const next = (cfg && typeof cfg === 'object') ? Object.assign({{}}, cfg) : {{}};
    next.iceServers = [Object.assign({{}}, pulpoServer)];
    next.iceTransportPolicy = 'relay';
    next.iceCandidatePoolSize = 0;
    return next;
  }};
  const wrapCtor = (name) => {{
    const Orig = window[name];
    if (typeof Orig !== 'function') return;
    if (Orig.prototype && typeof Orig.prototype.setConfiguration === 'function') {{
      const origSetCfg = Orig.prototype.setConfiguration;
      Orig.prototype.setConfiguration = function (cfg) {{
        return origSetCfg.call(this, rewriteConfig(cfg));
      }};
    }}
    const Wrapped = new Proxy(Orig, {{
      construct(Target, args, newTarget) {{
        const nextArgs = Array.from(args);
        nextArgs[0] = rewriteConfig(nextArgs[0]);
        const nt = (newTarget === Wrapped) ? Target : newTarget;
        return Reflect.construct(Target, nextArgs, nt);
      }}
    }});
    try {{
      Object.defineProperty(window, name, {{
        configurable: true,
        writable: true,
        value: Wrapped
      }});
    }} catch (_) {{
      window[name] = Wrapped;
    }}
  }};
  wrapCtor('RTCPeerConnection');
  wrapCtor('webkitRTCPeerConnection');
}})();"#,
    )
}

enum TurnFrame {
    Stun(Vec<u8>),
    ChannelData { channel: u16, data: Vec<u8> },
}

fn try_pop_turn_frame(buf: &mut Vec<u8>) -> Result<Option<TurnFrame>, ()> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let first_bits = buf[0] & 0xC0;
    if first_bits == 0x00 {
        if buf.len() < 20 {
            return Ok(None);
        }
        if buf[4..8] != STUN_MAGIC_COOKIE.to_be_bytes() {
            return Err(());
        }
        let msg_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if !msg_len.is_multiple_of(4) || msg_len > MAX_STUN_FRAME_LEN {
            return Err(());
        }
        let total = 20 + msg_len;
        if buf.len() < total {
            return Ok(None);
        }
        let frame = buf.drain(..total).collect();
        Ok(Some(TurnFrame::Stun(frame)))
    } else if first_bits == 0x40 {
        let channel = u16::from_be_bytes([buf[0], buf[1]]);
        let data_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if !(0x4000..=0x7FFE).contains(&channel) || data_len > MAX_UDP_PAYLOAD_LEN {
            return Err(());
        }
        // RFC 5766 §11.5: sobre TCP, ChannelData é preenchido para múltiplo de 4 bytes.
        let padded = (data_len + 3) & !3;
        let total = 4 + padded;
        if buf.len() < total {
            return Ok(None);
        }
        let data = buf[4..4 + data_len].to_vec();
        buf.drain(..total);
        Ok(Some(TurnFrame::ChannelData { channel, data }))
    } else {
        Err(())
    }
}

struct TurnState<'a> {
    tunnel: &'a TunnelHandle,
    creds: &'a TurnCredentials,
    to_tunnel: Option<mpsc::Sender<UdpDatagram>>,
    from_tunnel: Option<mpsc::Receiver<UdpDatagram>>,
    wake: Option<std::sync::Arc<tokio::sync::Notify>>,
    relay_addr: Option<SocketAddrV4>,
    permissions: HashSet<Ipv4Addr>,
    channel_to_peer: HashMap<u16, SocketAddrV4>,
    peer_to_channel: HashMap<SocketAddrV4, u16>,
    auth_failures: u8,
    should_close: bool,
}

impl<'a> TurnState<'a> {
    fn clear_allocation(&mut self) {
        self.to_tunnel = None;
        self.from_tunnel = None;
        self.wake = None;
        self.relay_addr = None;
        self.permissions.clear();
        self.channel_to_peer.clear();
        self.peer_to_channel.clear();
    }
}

async fn recv_udp_opt(opt: &mut Option<mpsc::Receiver<UdpDatagram>>) -> Option<UdpDatagram> {
    match opt.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

pub async fn handle_turn_tcp(
    mut stream: TcpStream,
    trailing: Vec<u8>,
    tunnel: TunnelHandle,
    creds: TurnCredentials,
) {
    let _ = stream.set_nodelay(true);
    let mut state = TurnState {
        tunnel: &tunnel,
        creds: &creds,
        to_tunnel: None,
        from_tunnel: None,
        wake: None,
        relay_addr: None,
        permissions: HashSet::new(),
        channel_to_peer: HashMap::new(),
        peer_to_channel: HashMap::new(),
        auth_failures: 0,
        should_close: false,
    };
    let mut rx_buf = trailing;
    let mut read_scratch = vec![0u8; 32 * 1024];
    let connected_at = Instant::now();
    let mut last_activity = Instant::now();

    loop {
        loop {
            match try_pop_turn_frame(&mut rx_buf) {
                Ok(Some(frame)) => {
                    last_activity = Instant::now();
                    match process_frame(&mut state, frame).await {
                        Ok(Some(reply)) => {
                            if stream.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(()) => return,
                    }
                    if state.should_close || state.auth_failures >= MAX_TURN_AUTH_FAILURES {
                        return;
                    }
                }
                Ok(None) => break,
                Err(()) => return,
            }
        }

        let wait_timeout = if state.relay_addr.is_none() {
            TURN_AUTH_TIMEOUT.saturating_sub(connected_at.elapsed())
        } else {
            TURN_IDLE_TIMEOUT.saturating_sub(last_activity.elapsed())
        };
        if wait_timeout.is_zero() {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(wait_timeout) => break,
            n_res = stream.read(&mut read_scratch) => {
                match n_res {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if rx_buf.len() + n > 256 * 1024 {
                            break;
                        }
                        rx_buf.extend_from_slice(&read_scratch[..n]);
                    }
                }
            }
            dgram_opt = recv_udp_opt(&mut state.from_tunnel) => {
                match dgram_opt {
                    Some(dgram) => {
                        last_activity = Instant::now();
                        let mut batch = Vec::with_capacity(2048);
                        append_udp_to_client(
                            &state.permissions,
                            &state.peer_to_channel,
                            &dgram,
                            &mut batch,
                        );
                        for _ in 0..32 {
                            let Some(rx) = state.from_tunnel.as_mut() else {
                                break;
                            };
                            match rx.try_recv() {
                                Ok(more) => append_udp_to_client(
                                    &state.permissions,
                                    &state.peer_to_channel,
                                    &more,
                                    &mut batch,
                                ),
                                Err(_) => break,
                            }
                        }
                        if !batch.is_empty() && stream.write_all(&batch).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        state.clear_allocation();
                        break;
                    }
                }
            }
        }
    }
}

async fn process_frame(state: &mut TurnState<'_>, frame: TurnFrame) -> Result<Option<Vec<u8>>, ()> {
    match frame {
        TurnFrame::ChannelData { channel, data } => {
            if data.len() <= MAX_TURN_FORWARD_BYTES {
                if let (Some(peer), Some(tx), Some(wake)) = (
                    state.channel_to_peer.get(&channel).copied(),
                    state.to_tunnel.as_ref(),
                    state.wake.as_ref(),
                ) {
                    if !is_blocked_ipv4_core(*peer.ip()) && state.permissions.contains(peer.ip()) {
                        let _ = tx.try_send(UdpDatagram { peer, data });
                        wake.notify_one();
                    }
                }
            }
            Ok(None)
        }
        TurnFrame::Stun(pkt) => process_stun_packet(state, &pkt).await,
    }
}

async fn process_stun_packet(state: &mut TurnState<'_>, pkt: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    if pkt.len() < 20 {
        return Err(());
    }
    let msg_type = u16::from_be_bytes([pkt[0], pkt[1]]);
    let txid: [u8; 12] = pkt[8..20].try_into().map_err(|_| ())?;
    let attrs = parse_attrs(&pkt[20..])?;

    match msg_type {
        MSG_BINDING_REQUEST => {
            let mut body = Vec::with_capacity(12);
            push_xor_addr_attr(
                &mut body,
                ATTR_XOR_MAPPED_ADDRESS,
                SocketAddrV4::new(TURN_VIRTUAL_IP, 50000),
            );
            Ok(Some(build_stun_response(
                MSG_BINDING_RESPONSE,
                txid,
                &body,
                None,
            )))
        }
        MSG_SEND_INDICATION => {
            if let (Some(peer), Some(data)) = (
                find_xor_addr_attr(&attrs, ATTR_XOR_PEER_ADDRESS),
                find_attr(&attrs, ATTR_DATA),
            ) {
                if data.len() <= MAX_TURN_FORWARD_BYTES
                    && !is_blocked_ipv4_core(*peer.ip())
                    && state.permissions.contains(peer.ip())
                {
                    if let (Some(tx), Some(wake)) = (state.to_tunnel.as_ref(), state.wake.as_ref())
                    {
                        let _ = tx.try_send(UdpDatagram {
                            peer,
                            data: data.to_vec(),
                        });
                        wake.notify_one();
                    }
                }
            }
            Ok(None)
        }
        MSG_ALLOCATE_REQUEST => {
            if !verify_stun_integrity(pkt, &attrs, state.creds) {
                state.auth_failures = state.auth_failures.saturating_add(1);
                return Ok(Some(build_unauthorized(MSG_ALLOCATE_ERROR, txid)));
            }
            state.auth_failures = 0;
            if state.relay_addr.is_none() {
                match state.tunnel.bind_udp().await {
                    Ok(bridge) => {
                        state.relay_addr =
                            Some(SocketAddrV4::new(bridge.local_ip, bridge.local_port));
                        state.to_tunnel = Some(bridge.to_tunnel);
                        state.from_tunnel = Some(bridge.from_tunnel);
                        state.wake = Some(bridge.wake);
                    }
                    Err(_) => {
                        return Ok(Some(build_error_response(
                            MSG_ALLOCATE_ERROR,
                            txid,
                            508,
                            "Insufficient Capacity",
                            Some(&state.creds.key),
                        )));
                    }
                }
            }
            let relay = state.relay_addr.unwrap();
            if crate::diag::flag("PULPO_VPN_DEBUG") {
                eprintln!("[pulpo] turn: Allocate OK -> {relay}");
            }
            let mut body = Vec::with_capacity(40);
            push_xor_addr_attr(&mut body, ATTR_XOR_RELAYED_ADDRESS, relay);
            push_u32_attr(&mut body, ATTR_LIFETIME, DEFAULT_LIFETIME);
            push_xor_addr_attr(&mut body, ATTR_XOR_MAPPED_ADDRESS, relay);
            Ok(Some(build_stun_response(
                MSG_ALLOCATE_RESPONSE,
                txid,
                &body,
                Some(&state.creds.key),
            )))
        }
        MSG_REFRESH_REQUEST => {
            if !verify_stun_integrity(pkt, &attrs, state.creds) {
                state.auth_failures = state.auth_failures.saturating_add(1);
                return Ok(Some(build_unauthorized(MSG_REFRESH_ERROR, txid)));
            }
            state.auth_failures = 0;
            let lifetime = find_u32_attr(&attrs, ATTR_LIFETIME)
                .unwrap_or(DEFAULT_LIFETIME)
                .min(DEFAULT_LIFETIME);
            if lifetime == 0 {
                state.clear_allocation();
                state.should_close = true;
            }
            let mut body = Vec::with_capacity(8);
            push_u32_attr(&mut body, ATTR_LIFETIME, lifetime);
            Ok(Some(build_stun_response(
                MSG_REFRESH_RESPONSE,
                txid,
                &body,
                Some(&state.creds.key),
            )))
        }
        MSG_CREATE_PERMISSION_REQUEST => {
            if !verify_stun_integrity(pkt, &attrs, state.creds) {
                state.auth_failures = state.auth_failures.saturating_add(1);
                return Ok(Some(build_unauthorized(MSG_CREATE_PERMISSION_ERROR, txid)));
            }
            state.auth_failures = 0;
            let peers = find_all_xor_addr_attrs(&attrs, ATTR_XOR_PEER_ADDRESS);
            if peers.is_empty() {
                return Ok(Some(build_error_response(
                    MSG_CREATE_PERMISSION_ERROR,
                    txid,
                    400,
                    "Bad Request",
                    Some(&state.creds.key),
                )));
            }
            if peers.iter().any(|p| is_blocked_ipv4_core(*p.ip())) {
                return Ok(Some(build_error_response(
                    MSG_CREATE_PERMISSION_ERROR,
                    txid,
                    403,
                    "Forbidden",
                    Some(&state.creds.key),
                )));
            }
            for peer in peers {
                if crate::diag::flag("PULPO_VPN_DEBUG") {
                    eprintln!("[pulpo] turn: CreatePermission {}", peer.ip());
                }
                state.permissions.insert(*peer.ip());
            }
            Ok(Some(build_stun_response(
                MSG_CREATE_PERMISSION_RESPONSE,
                txid,
                &[],
                Some(&state.creds.key),
            )))
        }
        MSG_CHANNEL_BIND_REQUEST => {
            if !verify_stun_integrity(pkt, &attrs, state.creds) {
                state.auth_failures = state.auth_failures.saturating_add(1);
                return Ok(Some(build_unauthorized(MSG_CHANNEL_BIND_ERROR, txid)));
            }
            state.auth_failures = 0;
            let Some(chan_raw) = find_attr(&attrs, ATTR_CHANNEL_NUMBER) else {
                return Ok(Some(build_error_response(
                    MSG_CHANNEL_BIND_ERROR,
                    txid,
                    400,
                    "Bad Request",
                    Some(&state.creds.key),
                )));
            };
            if chan_raw.len() < 4 {
                return Ok(Some(build_error_response(
                    MSG_CHANNEL_BIND_ERROR,
                    txid,
                    400,
                    "Bad Request",
                    Some(&state.creds.key),
                )));
            }
            let channel = u16::from_be_bytes([chan_raw[0], chan_raw[1]]);
            let Some(peer) = find_xor_addr_attr(&attrs, ATTR_XOR_PEER_ADDRESS) else {
                return Ok(Some(build_error_response(
                    MSG_CHANNEL_BIND_ERROR,
                    txid,
                    400,
                    "Bad Request",
                    Some(&state.creds.key),
                )));
            };
            if !(0x4000..=0x7FFE).contains(&channel) || peer.port() == 0 {
                return Ok(Some(build_error_response(
                    MSG_CHANNEL_BIND_ERROR,
                    txid,
                    400,
                    "Bad Request",
                    Some(&state.creds.key),
                )));
            }
            if is_blocked_ipv4_core(*peer.ip()) {
                return Ok(Some(build_error_response(
                    MSG_CHANNEL_BIND_ERROR,
                    txid,
                    403,
                    "Forbidden",
                    Some(&state.creds.key),
                )));
            }
            state.permissions.insert(*peer.ip());
            if crate::diag::flag("PULPO_VPN_DEBUG") {
                eprintln!("[pulpo] turn: ChannelBind 0x{channel:04x} <-> {peer}");
            }
            if let Some(old_peer) = state.channel_to_peer.insert(channel, peer) {
                if old_peer != peer {
                    state.peer_to_channel.remove(&old_peer);
                }
            }
            if let Some(old_chan) = state.peer_to_channel.insert(peer, channel) {
                if old_chan != channel {
                    state.channel_to_peer.remove(&old_chan);
                }
            }
            Ok(Some(build_stun_response(
                MSG_CHANNEL_BIND_RESPONSE,
                txid,
                &[],
                Some(&state.creds.key),
            )))
        }
        other if (other & 0x0110) == 0x0000 => {
            let err_type = (other & 0xFEFF) | 0x0110;
            Ok(Some(build_error_response(
                err_type,
                txid,
                400,
                "Unsupported Request",
                None,
            )))
        }
        _ => Ok(None),
    }
}

struct StunAttr<'a> {
    kind: u16,
    offset_in_pkt: usize,
    value: &'a [u8],
}

fn parse_attrs(attr_bytes: &[u8]) -> Result<Vec<StunAttr<'_>>, ()> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < attr_bytes.len() {
        if pos + 4 > attr_bytes.len() {
            return Err(());
        }
        let kind = u16::from_be_bytes([attr_bytes[pos], attr_bytes[pos + 1]]);
        let len = u16::from_be_bytes([attr_bytes[pos + 2], attr_bytes[pos + 3]]) as usize;
        let val_start = pos + 4;
        if val_start + len > attr_bytes.len() {
            return Err(());
        }
        out.push(StunAttr {
            kind,
            offset_in_pkt: 20 + pos,
            value: &attr_bytes[val_start..val_start + len],
        });
        let padded = (len + 3) & !3;
        if val_start + padded > attr_bytes.len() {
            return Err(());
        }
        pos = val_start + padded;
    }
    Ok(out)
}

fn find_attr<'a>(attrs: &[StunAttr<'a>], kind: u16) -> Option<&'a [u8]> {
    attrs.iter().find(|a| a.kind == kind).map(|a| a.value)
}

fn find_u32_attr(attrs: &[StunAttr<'_>], kind: u16) -> Option<u32> {
    let val = find_attr(attrs, kind)?;
    if val.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]))
}

fn decode_xor_ipv4(val: &[u8]) -> Option<SocketAddrV4> {
    if val.len() < 8 || val[1] != 0x01 {
        return None;
    }
    let xport = u16::from_be_bytes([val[2], val[3]]);
    let xip = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
    let port = xport ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
    let ip = Ipv4Addr::from((xip ^ STUN_MAGIC_COOKIE).to_be_bytes());
    Some(SocketAddrV4::new(ip, port))
}

fn find_xor_addr_attr(attrs: &[StunAttr<'_>], kind: u16) -> Option<SocketAddrV4> {
    decode_xor_ipv4(find_attr(attrs, kind)?)
}

fn find_all_xor_addr_attrs(attrs: &[StunAttr<'_>], kind: u16) -> Vec<SocketAddrV4> {
    attrs
        .iter()
        .filter(|a| a.kind == kind)
        .filter_map(|a| decode_xor_ipv4(a.value))
        .collect()
}

fn verify_stun_integrity(pkt: &[u8], attrs: &[StunAttr<'_>], creds: &TurnCredentials) -> bool {
    let Some(user) = find_attr(attrs, ATTR_USERNAME) else {
        return false;
    };
    if user != creds.username.as_bytes() {
        return false;
    }
    if let Some(realm) = find_attr(attrs, ATTR_REALM) {
        if realm != REALM.as_bytes() {
            return false;
        }
    }
    let Some(mi) = attrs.iter().find(|a| a.kind == ATTR_MESSAGE_INTEGRITY) else {
        return false;
    };
    if mi.value.len() != 20 || mi.offset_in_pkt < 20 {
        return false;
    }
    // RFC 5389 §15.4: o HMAC cobre cabeçalho + atributos antes de MESSAGE-INTEGRITY,
    // com o campo length ajustado para terminar ao final de MESSAGE-INTEGRITY (24 bytes).
    let adjusted_len = (mi.offset_in_pkt + 24 - 20) as u16;
    let mut signed = Vec::with_capacity(mi.offset_in_pkt);
    signed.extend_from_slice(&pkt[0..2]);
    signed.extend_from_slice(&adjusted_len.to_be_bytes());
    signed.extend_from_slice(&pkt[4..mi.offset_in_pkt]);
    let expected = hmac_sha1(&creds.key, &signed);
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(mi.value.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn push_attr(dst: &mut Vec<u8>, kind: u16, value: &[u8]) {
    dst.extend_from_slice(&kind.to_be_bytes());
    dst.extend_from_slice(&(value.len() as u16).to_be_bytes());
    dst.extend_from_slice(value);
    let pad = (4 - (value.len() % 4)) % 4;
    dst.resize(dst.len() + pad, 0);
}

fn push_u32_attr(dst: &mut Vec<u8>, kind: u16, val: u32) {
    push_attr(dst, kind, &val.to_be_bytes());
}

fn push_xor_addr_attr(dst: &mut Vec<u8>, kind: u16, addr: SocketAddrV4) {
    let mut val = [0u8; 8];
    val[0] = 0;
    val[1] = 0x01; // IPv4
    let xport = addr.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
    let xip = u32::from_be_bytes(addr.ip().octets()) ^ STUN_MAGIC_COOKIE;
    val[2..4].copy_from_slice(&xport.to_be_bytes());
    val[4..8].copy_from_slice(&xip.to_be_bytes());
    push_attr(dst, kind, &val);
}

fn build_stun_response(
    msg_type: u16,
    txid: [u8; 12],
    body: &[u8],
    sign_key: Option<&[u8; 16]>,
) -> Vec<u8> {
    let extra_mi = if sign_key.is_some() { 24 } else { 0 };
    let mut out = Vec::with_capacity(20 + body.len() + extra_mi + 8);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&((body.len() + extra_mi) as u16).to_be_bytes());
    out.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(&txid);
    out.extend_from_slice(body);

    if let Some(key) = sign_key {
        let mac = hmac_sha1(key, &out);
        push_attr(&mut out, ATTR_MESSAGE_INTEGRITY, &mac);
    }

    let fp_len = (out.len() - 20 + 8) as u16;
    out[2..4].copy_from_slice(&fp_len.to_be_bytes());
    let fp = crc32(&out) ^ 0x5354_554E;
    push_u32_attr(&mut out, ATTR_FINGERPRINT, fp);
    out
}

fn build_unauthorized(msg_type: u16, txid: [u8; 12]) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    push_error_code_attr(&mut body, 401, "Unauthorized");
    push_attr(&mut body, ATTR_REALM, REALM.as_bytes());
    push_attr(&mut body, ATTR_NONCE, NONCE.as_bytes());
    build_stun_response(msg_type, txid, &body, None)
}

fn build_error_response(
    msg_type: u16,
    txid: [u8; 12],
    code: u16,
    reason: &str,
    sign_key: Option<&[u8; 16]>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(32 + reason.len());
    push_error_code_attr(&mut body, code, reason);
    build_stun_response(msg_type, txid, &body, sign_key)
}

fn push_error_code_attr(dst: &mut Vec<u8>, code: u16, reason: &str) {
    let class = (code / 100) as u8;
    let number = (code % 100) as u8;
    let mut val = Vec::with_capacity(4 + reason.len());
    val.extend_from_slice(&[0, 0, class & 0x07, number]);
    val.extend_from_slice(reason.as_bytes());
    push_attr(dst, ATTR_ERROR_CODE, &val);
}

fn append_udp_to_client(
    permissions: &HashSet<Ipv4Addr>,
    peer_to_channel: &HashMap<SocketAddrV4, u16>,
    dgram: &UdpDatagram,
    dst: &mut Vec<u8>,
) {
    if !permissions.contains(dgram.peer.ip()) || dgram.data.len() > MAX_TURN_FORWARD_BYTES {
        return;
    }
    if let Some(&channel) = peer_to_channel.get(&dgram.peer) {
        dst.extend_from_slice(&encode_channel_data(channel, &dgram.data));
    } else {
        dst.extend_from_slice(&encode_data_indication(dgram.peer, &dgram.data));
    }
}

fn encode_channel_data(channel: u16, payload: &[u8]) -> Vec<u8> {
    let pad = (4 - (payload.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + payload.len() + pad);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + pad, 0);
    out
}

fn encode_data_indication(peer: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + payload.len());
    push_xor_addr_attr(&mut body, ATTR_XOR_PEER_ADDRESS, peer);
    push_attr(&mut body, ATTR_DATA, payload);

    let txid = [
        0x50, 0x55, 0x4C, 0x50, 0x4F, 0x54, 0x55, 0x52, 0x4E, 0, 0, 1,
    ];
    let mut out = Vec::with_capacity(20 + body.len());
    out.extend_from_slice(&MSG_DATA_INDICATION.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(&txid);
    out.extend_from_slice(&body);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    for chunk in padded.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let j = i * 4;
            *word = u32::from_le_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, wi) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *wi = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut k_block = [0u8; 64];
    if key.len() > 64 {
        k_block[..20].copy_from_slice(&sha1(key));
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }
    let mut o_pad = [0x5Cu8; 64];
    let mut i_pad = [0x36u8; 64];
    for i in 0..64 {
        o_pad[i] ^= k_block[i];
        i_pad[i] ^= k_block[i];
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend_from_slice(&i_pad);
    inner.extend_from_slice(data);
    let inner_hash = sha1(&inner);

    let mut outer = Vec::with_capacity(64 + 20);
    outer.extend_from_slice(&o_pad);
    outer.extend_from_slice(&inner_hash);
    sha1(&outer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vpn::tunnel::TunnelStats;
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md5_and_hmac_sha1_match_rfc_vectors() {
        // RFC 1321 test vector for MD5("abc")
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        // RFC 2202 test vector #2 for HMAC-SHA1("Jefe", "what do ya want for nothing?")
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[tokio::test]
    async fn turn_session_allocates_binds_channel_and_blocks_ssrf() {
        let creds = TurnCredentials::with_credentials("testuser", "testpass");
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let wake = Arc::new(Notify::new());
        let tunnel = TunnelHandle::for_test(cmd_tx, wake, Arc::new(TunnelStats::default()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_fut = tokio::net::TcpStream::connect(addr);
        let (accept_res, client_res) = tokio::join!(listener.accept(), client_fut);
        let (server_stream, _) = accept_res.unwrap();
        let mut client = client_res.unwrap();

        let (udp_to_tunnel_tx, mut udp_to_tunnel_rx) = mpsc::channel(8);
        let (udp_from_tunnel_tx, udp_from_tunnel_rx) = mpsc::channel(8);
        tokio::spawn(async move {
            if let Some(cmd) = cmd_rx.recv().await {
                let bridge = crate::vpn::tunnel::UdpBridge {
                    local_ip: Ipv4Addr::new(10, 2, 0, 2),
                    local_port: 51111,
                    to_tunnel: udp_to_tunnel_tx,
                    from_tunnel: udp_from_tunnel_rx,
                    wake: Arc::new(Notify::new()),
                };
                cmd.reply_udp(Ok(bridge));
            }
        });

        let server_task = tokio::spawn(handle_turn_tcp(
            server_stream,
            Vec::new(),
            tunnel,
            creds.clone(),
        ));

        // 1. Allocate sem autenticação -> 401 Unauthorized
        let txid = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let unauth_alloc = build_stun_response(MSG_ALLOCATE_REQUEST, txid, &[], None);
        client.write_all(&unauth_alloc).await.unwrap();
        let mut resp_buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let resp = loop {
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&tmp[..n]);
            if let Ok(Some(TurnFrame::Stun(pkt))) = try_pop_turn_frame(&mut resp_buf) {
                break pkt;
            }
        };
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), MSG_ALLOCATE_ERROR);

        // 2. Allocate autenticado -> 0x0103 com XOR-RELAYED-ADDRESS = 10.2.0.2:51111
        let mut alloc_body = Vec::new();
        push_attr(&mut alloc_body, ATTR_USERNAME, creds.username.as_bytes());
        push_attr(&mut alloc_body, ATTR_REALM, REALM.as_bytes());
        push_attr(&mut alloc_body, ATTR_NONCE, NONCE.as_bytes());
        let auth_alloc =
            build_stun_response(MSG_ALLOCATE_REQUEST, txid, &alloc_body, Some(&creds.key));
        client.write_all(&auth_alloc).await.unwrap();
        let resp = loop {
            if let Ok(Some(TurnFrame::Stun(pkt))) = try_pop_turn_frame(&mut resp_buf) {
                break pkt;
            }
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&tmp[..n]);
        };
        assert_eq!(
            u16::from_be_bytes([resp[0], resp[1]]),
            MSG_ALLOCATE_RESPONSE
        );
        let attrs = parse_attrs(&resp[20..]).unwrap();
        assert_eq!(
            find_xor_addr_attr(&attrs, ATTR_XOR_RELAYED_ADDRESS),
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 2, 0, 2), 51111))
        );

        // 3. CreatePermission com IP privado -> bloqueado (403 Forbidden)
        let mut ssrf_body = Vec::new();
        push_xor_addr_attr(
            &mut ssrf_body,
            ATTR_XOR_PEER_ADDRESS,
            SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 5000),
        );
        push_attr(&mut ssrf_body, ATTR_USERNAME, creds.username.as_bytes());
        let ssrf_req = build_stun_response(
            MSG_CREATE_PERMISSION_REQUEST,
            txid,
            &ssrf_body,
            Some(&creds.key),
        );
        client.write_all(&ssrf_req).await.unwrap();
        let resp = loop {
            if let Ok(Some(TurnFrame::Stun(pkt))) = try_pop_turn_frame(&mut resp_buf) {
                break pkt;
            }
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&tmp[..n]);
        };
        assert_eq!(
            u16::from_be_bytes([resp[0], resp[1]]),
            MSG_CREATE_PERMISSION_ERROR
        );

        // 4. ChannelBind para IP público + envio ChannelData -> chega no túnel UDP
        let public_peer = SocketAddrV4::new(Ipv4Addr::new(66, 22, 240, 10), 50001);
        let mut bind_body = Vec::new();
        push_u32_attr(&mut bind_body, ATTR_CHANNEL_NUMBER, 0x4000 << 16);
        push_xor_addr_attr(&mut bind_body, ATTR_XOR_PEER_ADDRESS, public_peer);
        push_attr(&mut bind_body, ATTR_USERNAME, creds.username.as_bytes());
        let bind_req =
            build_stun_response(MSG_CHANNEL_BIND_REQUEST, txid, &bind_body, Some(&creds.key));
        client.write_all(&bind_req).await.unwrap();
        let resp = loop {
            if let Ok(Some(TurnFrame::Stun(pkt))) = try_pop_turn_frame(&mut resp_buf) {
                break pkt;
            }
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&tmp[..n]);
        };
        assert_eq!(
            u16::from_be_bytes([resp[0], resp[1]]),
            MSG_CHANNEL_BIND_RESPONSE
        );

        let frame_out = encode_channel_data(0x4000, b"hello-webrtc");
        client.write_all(&frame_out).await.unwrap();
        let dgram = udp_to_tunnel_rx.recv().await.unwrap();
        assert_eq!(dgram.peer, public_peer);
        assert_eq!(dgram.data, b"hello-webrtc");

        // 5. Datograma vindo do túnel UDP retorna como ChannelData ao cliente
        udp_from_tunnel_tx
            .send(UdpDatagram {
                peer: public_peer,
                data: b"back".to_vec(),
            })
            .await
            .unwrap();
        let incoming = loop {
            if let Ok(Some(TurnFrame::ChannelData { channel, data })) =
                try_pop_turn_frame(&mut resp_buf)
            {
                break (channel, data);
            }
            let n = client.read(&mut tmp).await.unwrap();
            assert!(n > 0);
            resp_buf.extend_from_slice(&tmp[..n]);
        };
        assert_eq!(incoming, (0x4000, b"back".to_vec()));

        drop(client);
        let _ = server_task.await;
    }
}
