//! Motor WireGuard em espaço de usuário (`boringtun` + `smoltcp`), sem drivers
//! de kernel nem alterações na rede do sistema operacional.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use boringtun::noise::errors::WireGuardError;
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use smoltcp::iface::{Config as IfaceConfig, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::dns::{self, GetQueryResultError};
use smoltcp::socket::tcp::{self, State as TcpState};
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{DnsQueryType, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch, Notify};

use super::config::{is_blocked_endpoint_ip, ValidatedConfig};

const WG_OVERHEAD_BUF: usize = 2048;
const TCP_BUF_SIZE: usize = 256 * 1024;
const TCP_CHANNEL_CAP: usize = 64;
const UDP_META_CAP: usize = 256;
const UDP_BUF_SIZE: usize = 512 * 1024;
const UDP_CHANNEL_CAP: usize = 256;
const PRIMARY_DNS_TIMEOUT: Duration = Duration::from_millis(1500);
const DNS_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_CLOSING_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DNS_CACHE_TTL: Duration = Duration::from_secs(60);
const DNS_FALLBACK_STICKY: Duration = Duration::from_secs(300);
const DNS_QUERY_SLOTS: usize = 64;
const MAX_DNS_CACHE: usize = 512;
const MAX_TCP_SOCKETS: usize = 256;
const MAX_UDP_SOCKETS: usize = 32;
const MAX_UDP_RX_BATCH: usize = 64;
const MAX_CMD_BATCH: usize = 32;
const MAX_SMOL_POLL_ITERS: usize = 64;
const MAX_RX_QUEUE: usize = 512;
const EPHEMERAL_MIN: u16 = 49152;

fn vpn_debug() -> bool {
    crate::diag::flag("PULPO_VPN_DEBUG")
}

pub struct TcpBridge {
    pub to_tunnel: mpsc::Sender<Vec<u8>>,
    pub from_tunnel: mpsc::Receiver<Vec<u8>>,
    pub wake: Arc<Notify>,
}

#[derive(Debug, Clone)]
pub struct UdpDatagram {
    pub peer: SocketAddrV4,
    pub data: Vec<u8>,
}

pub struct UdpBridge {
    pub local_ip: Ipv4Addr,
    pub local_port: u16,
    pub to_tunnel: mpsc::Sender<UdpDatagram>,
    pub from_tunnel: mpsc::Receiver<UdpDatagram>,
    pub wake: Arc<Notify>,
}

pub struct TunnelStats {
    handshake_ok: AtomicBool,
    handshake_notify: Notify,
    consecutive_failures: AtomicU32,
    started_at: Instant,
    last_rx_ms: AtomicU64,
    tx_pending_since_ms: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Default for TunnelStats {
    fn default() -> Self {
        Self {
            handshake_ok: AtomicBool::new(false),
            handshake_notify: Notify::new(),
            consecutive_failures: AtomicU32::new(0),
            started_at: Instant::now(),
            last_rx_ms: AtomicU64::new(0),
            tx_pending_since_ms: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }
}

impl TunnelStats {
    pub fn is_connected(&self) -> bool {
        self.handshake_ok.load(Ordering::Relaxed)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    pub fn record_rx(&self) {
        let ms = (self.started_at.elapsed().as_millis() as u64).max(1);
        self.last_rx_ms.store(ms, Ordering::Relaxed);
        self.tx_pending_since_ms.store(0, Ordering::Relaxed);
    }

    pub fn record_tx(&self) {
        let ms = (self.started_at.elapsed().as_millis() as u64).max(1);
        let _ =
            self.tx_pending_since_ms
                .compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Retorna `true` apenas se há tráfego de saída aguardando resposta há pelo
    /// menos `threshold`. Com o túnel ocioso (sem TX pendente), retorna `false`.
    pub fn is_stalled(&self, threshold: Duration) -> bool {
        let pending_since = self.tx_pending_since_ms.load(Ordering::Relaxed);
        if pending_since == 0 {
            return false;
        }
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        Duration::from_millis(elapsed_ms.saturating_sub(pending_since)) >= threshold
    }

    pub fn mark_disconnected(&self) {
        self.handshake_ok.store(false, Ordering::Relaxed);
    }

    fn mark_connected(&self, debug: bool) {
        self.record_rx();
        let was_connected = self.handshake_ok.swap(true, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = None;
        self.handshake_notify.notify_waiters();
        if !was_connected && debug {
            eprintln!("[pulpo] vpn: wireguard handshake completed");
        }
    }

    pub fn set_error(&self, err: impl Into<String>) {
        *self.last_error.lock().unwrap() = Some(err.into());
    }

    fn clear_error(&self) {
        *self.last_error.lock().unwrap() = None;
    }
}

pub(crate) enum TunnelCmd {
    ResolveIpv4 {
        host: String,
        reply: oneshot::Sender<Result<Vec<Ipv4Addr>, String>>,
    },
    ConnectTcp {
        remote: SocketAddr,
        reply: oneshot::Sender<Result<TcpBridge, String>>,
    },
    BindUdp {
        reply: oneshot::Sender<Result<UdpBridge, String>>,
    },
}

#[cfg(test)]
impl TunnelCmd {
    pub(crate) fn reply_udp(self, res: Result<UdpBridge, String>) {
        if let Self::BindUdp { reply } = self {
            let _ = reply.send(res);
        }
    }
}

#[derive(Clone)]
pub struct TunnelHandle {
    cmd_tx: mpsc::Sender<TunnelCmd>,
    pub wake: Arc<Notify>,
    pub stats: Arc<TunnelStats>,
}

impl TunnelHandle {
    #[cfg(test)]
    pub(crate) fn for_test(
        cmd_tx: mpsc::Sender<TunnelCmd>,
        wake: Arc<Notify>,
        stats: Arc<TunnelStats>,
    ) -> Self {
        Self {
            cmd_tx,
            wake,
            stats,
        }
    }
    pub async fn wait_handshake(&self, timeout: Duration) -> Result<(), String> {
        if self.stats.is_connected() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.stats.handshake_notify.notified();
            if self.stats.is_connected() {
                return Ok(());
            }
            match tokio::time::timeout_at(deadline, notified).await {
                Ok(()) => {
                    if self.stats.is_connected() {
                        return Ok(());
                    }
                }
                Err(_) => {
                    return Err(self
                        .stats
                        .last_error()
                        .unwrap_or_else(|| "vpn.error.handshakeTimeout".to_string()));
                }
            }
        }
    }

    pub async fn resolve_ipv4(&self, host: String) -> Result<Vec<Ipv4Addr>, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TunnelCmd::ResolveIpv4 { host, reply: tx })
            .await
            .map_err(|_| "vpn.error.stopped".to_string())?;
        self.wake.notify_one();
        tokio::time::timeout(DNS_TIMEOUT + Duration::from_secs(1), rx)
            .await
            .map_err(|_| "vpn.error.timeout".to_string())?
            .map_err(|_| "vpn.error.stopped".to_string())?
    }

    pub async fn connect_tcp(&self, remote: SocketAddr) -> Result<TcpBridge, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TunnelCmd::ConnectTcp { remote, reply: tx })
            .await
            .map_err(|_| "vpn.error.stopped".to_string())?;
        self.wake.notify_one();
        tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(1), rx)
            .await
            .map_err(|_| "vpn.error.timeout".to_string())?
            .map_err(|_| "vpn.error.stopped".to_string())?
    }

    pub async fn bind_udp(&self) -> Result<UdpBridge, String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(TunnelCmd::BindUdp { reply: tx })
            .await
            .map_err(|_| "vpn.error.stopped".to_string())?;
        self.wake.notify_one();
        tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(1), rx)
            .await
            .map_err(|_| "vpn.error.timeout".to_string())?
            .map_err(|_| "vpn.error.stopped".to_string())?
    }
}

pub fn spawn_tunnel(config: ValidatedConfig, shutdown: watch::Receiver<bool>) -> TunnelHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let wake = Arc::new(Notify::new());
    let stats = Arc::new(TunnelStats::default());
    let handle = TunnelHandle {
        cmd_tx,
        wake: wake.clone(),
        stats: stats.clone(),
    };
    tauri::async_runtime::spawn(run_tunnel(config, cmd_rx, wake, stats, shutdown));
    handle
}

struct VirtualIpDevice {
    mtu: usize,
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
}

impl VirtualIpDevice {
    fn new(mtu: usize) -> Self {
        Self {
            mtu,
            rx_queue: VecDeque::with_capacity(64),
            tx_queue: VecDeque::with_capacity(64),
        }
    }
}

struct VpnRxToken(Vec<u8>);

impl RxToken for VpnRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct VpnTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl<'a> TxToken for VpnTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let res = f(&mut buf);
        self.0.push_back(buf);
        res
    }
}

impl Device for VirtualIpDevice {
    type RxToken<'a>
        = VpnRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = VpnTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let pkt = self.rx_queue.pop_front()?;
        Some((VpnRxToken(pkt), VpnTxToken(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VpnTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

struct PendingDns {
    host: String,
    handle: dns::QueryHandle,
    using_fallback: bool,
    tried_alternate: bool,
    started_at: Instant,
    replies: Vec<oneshot::Sender<Result<Vec<Ipv4Addr>, String>>>,
}

fn finish_pending_dns(entry: PendingDns, res: Result<Vec<Ipv4Addr>, String>) {
    for reply in entry.replies {
        let _ = reply.send(res.clone());
    }
}

#[derive(Default)]
struct DnsPreference {
    prefer_fallback_until: Option<Instant>,
}

impl DnsPreference {
    fn use_fallback(&self, now: Instant) -> bool {
        self.prefer_fallback_until.is_some_and(|until| now < until)
    }

    fn mark_fallback_ok(&mut self, now: Instant) {
        self.prefer_fallback_until = Some(now + DNS_FALLBACK_STICKY);
    }

    fn mark_fallback_failed(&mut self) {
        self.prefer_fallback_until = None;
    }
}

fn create_dns_socket(server: Ipv4Addr) -> dns::Socket<'static> {
    let servers = [IpAddress::Ipv4(server)];
    let mut queries = Vec::with_capacity(DNS_QUERY_SLOTS);
    queries.resize_with(DNS_QUERY_SLOTS, || None);
    dns::Socket::new(&servers, queries)
}

struct ActiveTcp {
    local_port: u16,
    rx_tx: Option<mpsc::Sender<Vec<u8>>>,
    tx_rx: mpsc::Receiver<Vec<u8>>,
    pending_tx: Vec<u8>,
    pending_offset: usize,
    client_eof: bool,
    closing_since: Option<Instant>,
    connect_reply: Option<oneshot::Sender<Result<TcpBridge, String>>>,
    pending_bridge: Option<TcpBridge>,
    started_at: Instant,
    last_active: Instant,
}

struct ActiveUdp {
    local_port: u16,
    rx_tx: mpsc::Sender<UdpDatagram>,
    tx_rx: mpsc::Receiver<UdpDatagram>,
    pending_tx: Option<UdpDatagram>,
    closed: bool,
    last_active: Instant,
}

fn insert_dns_cache(
    cache: &mut HashMap<String, (Vec<Ipv4Addr>, Instant)>,
    key: String,
    ips: Vec<Ipv4Addr>,
) {
    if cache.len() >= MAX_DNS_CACHE && !cache.contains_key(&key) {
        cache.retain(|_, (_, at)| at.elapsed() < DNS_CACHE_TTL);
        if cache.len() >= MAX_DNS_CACHE {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
    }
    cache.insert(key, (ips, Instant::now()));
}

fn alloc_ephemeral_port(next: &mut u16, used: &HashSet<u16>) -> Option<u16> {
    let span = (u16::MAX - EPHEMERAL_MIN) as usize + 1;
    let mut candidate = (*next).max(EPHEMERAL_MIN);
    for _ in 0..span {
        let port = candidate;
        candidate = if candidate == u16::MAX {
            EPHEMERAL_MIN
        } else {
            candidate + 1
        };
        if !used.contains(&port) {
            *next = candidate;
            return Some(port);
        }
    }
    None
}

async fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, String> {
    let mut addrs = tokio::net::lookup_host(endpoint)
        .await
        .map_err(|_| "vpn.error.endpointResolve".to_string())?;
    let mut chosen = None;
    for addr in &mut addrs {
        if !is_blocked_endpoint_ip(addr.ip()) {
            if addr.is_ipv4() {
                return Ok(addr);
            }
            chosen.get_or_insert(addr);
        }
    }
    chosen.ok_or_else(|| "vpn.error.confEndpointBlocked".to_string())
}

fn random_seed() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1),
    );
    hasher.write_u32(std::process::id());
    hasher.finish() | 1
}

/// WireGuard (§5.4.6) requires transport payloads to be zero-padded to a 16-byte
/// boundary; `boringtun` leaves the plaintext unpadded, which strict gateways drop.
fn encapsulate_padded<'a>(tunn: &mut Tunn, ip_pkt: &[u8], dst: &'a mut [u8]) -> TunnResult<'a> {
    let padded_len = (ip_pkt.len() + 15) & !15;
    if padded_len > WG_OVERHEAD_BUF - 32 || padded_len == ip_pkt.len() {
        return tunn.encapsulate(ip_pkt, dst);
    }
    let mut pad = [0u8; WG_OVERHEAD_BUF];
    pad[..ip_pkt.len()].copy_from_slice(ip_pkt);
    tunn.encapsulate(&pad[..padded_len], dst)
}

fn ipv4_summary(pkt: &[u8]) -> String {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 {
        return format!("non-ipv4({}B)", pkt.len());
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    let total = u16::from_be_bytes([pkt[2], pkt[3]]);
    let proto = pkt[9];
    let src = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
    let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
    if (proto == 6 || proto == 17) && ihl >= 20 && pkt.len() >= ihl + 4 {
        let sp = u16::from_be_bytes([pkt[ihl], pkt[ihl + 1]]);
        let dp = u16::from_be_bytes([pkt[ihl + 2], pkt[ihl + 3]]);
        format!("IPv4 proto={proto} len={total} {src}:{sp} -> {dst}:{dp}")
    } else {
        format!("IPv4 proto={proto} len={total} {src} -> {dst}")
    }
}

#[allow(clippy::too_many_arguments)]
async fn flush_tx_queue(
    device: &mut VirtualIpDevice,
    peer_addr: Option<SocketAddr>,
    tunn: &mut Tunn,
    wg_buf: &mut [u8],
    udp: &UdpSocket,
    stats: &TunnelStats,
    tx_diag_count: &mut u8,
    debug: bool,
) {
    while let Some(ip_pkt) = device.tx_queue.pop_front() {
        let Some(peer) = peer_addr else {
            continue;
        };
        let log_tx = debug && *tx_diag_count < 12;
        if log_tx {
            *tx_diag_count += 1;
        }
        let summary = if log_tx {
            Some(ipv4_summary(&ip_pkt))
        } else {
            None
        };
        match encapsulate_padded(tunn, &ip_pkt, wg_buf) {
            TunnResult::WriteToNetwork(pkt) => {
                if let Some(summary) = summary {
                    eprintln!("[pulpo] vpn: tx {summary} -> wg {} bytes", pkt.len());
                }
                stats.record_tx();
                if let Err(err) = udp.send_to(pkt, peer).await {
                    if debug {
                        eprintln!("[pulpo] vpn: udp send_to({peer}) tx error: {err}");
                    }
                }
            }
            other => {
                if let Some(summary) = summary {
                    let kind = match other {
                        TunnResult::Done => "Done",
                        TunnResult::Err(_) => "Err",
                        TunnResult::WriteToTunnelV4(_, _) => "WriteToTunnelV4",
                        TunnResult::WriteToTunnelV6(_, _) => "WriteToTunnelV6",
                        TunnResult::WriteToNetwork(_) => unreachable!(),
                    };
                    eprintln!("[pulpo] vpn: tx {summary} -> encapsulate returned {kind}");
                }
            }
        }
    }
}

async fn run_tunnel(
    config: ValidatedConfig,
    mut cmd_rx: mpsc::Receiver<TunnelCmd>,
    wake: Arc<Notify>,
    stats: Arc<TunnelStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let debug = vpn_debug();
    let udp = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(sock) => sock,
        Err(err) => {
            if debug {
                eprintln!("[pulpo] vpn: failed to bind UDP socket: {err}");
            }
            stats.set_error("vpn.error.network");
            return;
        }
    };

    let seed = random_seed();
    let peer_index = ((seed as u32) & 0x00ff_ffff).max(1);
    let secret = StaticSecret::from(config.secrets.private_key);
    let peer_pub = PublicKey::from(config.peer_public_key);
    let keepalive = (config.keepalive > 0).then_some(config.keepalive);
    let mut tunn = Tunn::new(
        secret,
        peer_pub,
        config.secrets.preshared_key,
        keepalive,
        peer_index,
        None,
    );

    let start = Instant::now();
    let smol_now = || SmolInstant::from_millis(start.elapsed().as_millis() as i64);

    let mut device = VirtualIpDevice::new(config.mtu as usize);
    let mut iface_cfg = IfaceConfig::new(HardwareAddress::Ip);
    iface_cfg.random_seed = seed;
    let mut iface = Interface::new(iface_cfg, &mut device, smol_now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(
            IpAddress::Ipv4(config.tunnel_ip),
            config.tunnel_prefix,
        ));
    });
    let _ = iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 0));

    let fallback_dns_ip = if config.dns_ip == Ipv4Addr::new(1, 1, 1, 1) {
        Ipv4Addr::new(9, 9, 9, 9)
    } else {
        Ipv4Addr::new(1, 1, 1, 1)
    };
    let mut sockets = SocketSet::new(vec![]);
    let dns_handle = sockets.add(create_dns_socket(config.dns_ip));
    let fallback_dns_handle = sockets.add(create_dns_socket(fallback_dns_ip));

    let mut peer_addr: Option<SocketAddr> = match resolve_endpoint(&config.endpoint).await {
        Ok(addr) => {
            if debug {
                eprintln!(
                    "[pulpo] vpn: resolved endpoint {} -> {addr}",
                    config.endpoint
                );
            }
            Some(addr)
        }
        Err(err) => {
            if debug {
                eprintln!(
                    "[pulpo] vpn: failed to resolve endpoint {}: {err}",
                    config.endpoint
                );
            }
            stats.set_error("vpn.error.endpointResolve");
            None
        }
    };

    let mut pending_dns: Vec<PendingDns> = Vec::new();
    let mut dns_cache: HashMap<String, (Vec<Ipv4Addr>, Instant)> = HashMap::new();
    let mut dns_pref = DnsPreference::default();
    let mut active_tcps: HashMap<SocketHandle, ActiveTcp> = HashMap::new();
    let mut active_udps: HashMap<SocketHandle, ActiveUdp> = HashMap::new();
    let mut used_ports: HashSet<u16> = HashSet::new();
    let mut used_udp_ports: HashSet<u16> = HashSet::new();
    let mut next_ephemeral_port: u16 =
        EPHEMERAL_MIN + ((seed >> 16) as u16 % (u16::MAX - EPHEMERAL_MIN));
    let mut next_udp_port: u16 = EPHEMERAL_MIN + ((seed >> 32) as u16 % (u16::MAX - EPHEMERAL_MIN));
    let mut wg_buf = vec![0u8; WG_OVERHEAD_BUF];
    let mut udp_rx = vec![0u8; WG_OVERHEAD_BUF];
    let mut last_handshake_sent: Option<Instant> = None;
    let mut tx_diag_count: u8 = 0;
    let mut rx_diag_count: u8 = 0;
    let mut wg_tick = tokio::time::interval(Duration::from_millis(250));
    wg_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if *shutdown.borrow() {
            break;
        }

        // Send initial handshake once peer is resolved.
        if last_handshake_sent.is_none() {
            if let Some(peer) = peer_addr {
                match tunn.format_handshake_initiation(&mut wg_buf, false) {
                    TunnResult::WriteToNetwork(pkt) => {
                        if debug {
                            eprintln!(
                                "[pulpo] vpn: sending initial handshake ({} bytes) to {peer}",
                                pkt.len()
                            );
                        }
                        if let Err(err) = udp.send_to(pkt, peer).await {
                            if debug {
                                eprintln!("[pulpo] vpn: udp send_to({peer}) error: {err}");
                            }
                            stats.set_error("vpn.error.network");
                        }
                    }
                    TunnResult::Err(err) => {
                        if debug {
                            eprintln!("[pulpo] vpn: handshake initiation error: {err:?}");
                        }
                        stats.set_error("vpn.error.handshake");
                    }
                    _ => {}
                }
                last_handshake_sent = Some(Instant::now());
            }
        }

        // Poll smoltcp stack up to bounded iterations to drain RX/TX bursts.
        for _ in 0..MAX_SMOL_POLL_ITERS {
            let polled = iface.poll(smol_now(), &mut device, &mut sockets);
            flush_tx_queue(
                &mut device,
                peer_addr,
                &mut tunn,
                &mut wg_buf,
                &udp,
                &stats,
                &mut tx_diag_count,
                debug,
            )
            .await;
            if polled == PollResult::None && device.rx_queue.is_empty() {
                break;
            }
        }

        // Advance pending DNS queries.
        {
            let mut idx = 0;
            while idx < pending_dns.len() {
                let sock_handle = if pending_dns[idx].using_fallback {
                    fallback_dns_handle
                } else {
                    dns_handle
                };
                let query_res = sockets
                    .get_mut::<dns::Socket>(sock_handle)
                    .get_query_result(pending_dns[idx].handle);
                match query_res {
                    Ok(addrs) => {
                        let entry = pending_dns.swap_remove(idx);
                        let ipv4s: Vec<Ipv4Addr> =
                            addrs.into_iter().map(|IpAddress::Ipv4(v4)| v4).collect();
                        if ipv4s.is_empty() {
                            stats.record_failure();
                            finish_pending_dns(entry, Err("vpn.error.dnsFailed".into()));
                        } else {
                            if entry.using_fallback {
                                dns_pref.mark_fallback_ok(Instant::now());
                            }
                            stats.record_success();
                            insert_dns_cache(&mut dns_cache, entry.host.clone(), ipv4s.clone());
                            finish_pending_dns(entry, Ok(ipv4s));
                        }
                    }
                    Err(GetQueryResultError::Pending) => {
                        if !pending_dns[idx].tried_alternate
                            && pending_dns[idx].started_at.elapsed() >= PRIMARY_DNS_TIMEOUT
                        {
                            let old_handle = pending_dns[idx].handle;
                            sockets
                                .get_mut::<dns::Socket>(sock_handle)
                                .cancel_query(old_handle);
                            let next_fallback = !pending_dns[idx].using_fallback;
                            if !next_fallback {
                                dns_pref.mark_fallback_failed();
                            }
                            let alt_handle = if next_fallback {
                                fallback_dns_handle
                            } else {
                                dns_handle
                            };
                            let retry = sockets.get_mut::<dns::Socket>(alt_handle).start_query(
                                iface.context(),
                                &pending_dns[idx].host,
                                DnsQueryType::A,
                            );
                            match retry {
                                Ok(new_handle) => {
                                    pending_dns[idx].handle = new_handle;
                                    pending_dns[idx].using_fallback = next_fallback;
                                    pending_dns[idx].tried_alternate = true;
                                    if debug {
                                        let target_ip = if next_fallback {
                                            fallback_dns_ip
                                        } else {
                                            config.dns_ip
                                        };
                                        eprintln!(
                                            "[pulpo] vpn: DNS timed out, retrying via {target_ip}"
                                        );
                                    }
                                    idx += 1;
                                }
                                Err(_) => {
                                    let entry = pending_dns.swap_remove(idx);
                                    finish_pending_dns(entry, Err("vpn.error.dnsBusy".into()));
                                }
                            }
                        } else if pending_dns[idx].started_at.elapsed() >= DNS_TIMEOUT {
                            let entry = pending_dns.swap_remove(idx);
                            sockets
                                .get_mut::<dns::Socket>(sock_handle)
                                .cancel_query(entry.handle);
                            stats.record_failure();
                            if debug {
                                eprintln!("[pulpo] vpn: DNS query timed out");
                            }
                            finish_pending_dns(entry, Err("vpn.error.timeout".into()));
                        } else {
                            idx += 1;
                        }
                    }
                    Err(GetQueryResultError::Failed) => {
                        let entry = pending_dns.swap_remove(idx);
                        stats.record_failure();
                        if debug {
                            eprintln!("[pulpo] vpn: DNS query failed");
                        }
                        finish_pending_dns(entry, Err("vpn.error.dnsFailed".into()));
                    }
                }
            }
        }

        // Pump active TCP sockets <-> Tokio channels without awaiting.
        let mut to_remove = Vec::new();
        let mut recv_scratch = [0u8; 16 * 1024];
        for (&sock_handle, tcp_entry) in &mut active_tcps {
            let sock = sockets.get_mut::<tcp::Socket>(sock_handle);
            let state = sock.state();

            if let Some(reply) = tcp_entry.connect_reply.take() {
                match state {
                    TcpState::Established | TcpState::CloseWait => {
                        stats.record_success();
                        if let Some(bridge) = tcp_entry.pending_bridge.take() {
                            tcp_entry.last_active = Instant::now();
                            let _ = reply.send(Ok(bridge));
                        }
                    }
                    TcpState::Closed | TcpState::TimeWait => {
                        stats.record_failure();
                        if debug {
                            eprintln!("[pulpo] vpn: TCP connect closed/refused");
                        }
                        let _ = reply.send(Err("vpn.error.connectFailed".into()));
                        to_remove.push(sock_handle);
                        continue;
                    }
                    _ if tcp_entry.started_at.elapsed() >= CONNECT_TIMEOUT => {
                        stats.record_failure();
                        if debug {
                            eprintln!("[pulpo] vpn: TCP connect timed out");
                        }
                        sock.abort();
                        let _ = reply.send(Err("vpn.error.timeout".into()));
                        to_remove.push(sock_handle);
                        continue;
                    }
                    _ => {
                        tcp_entry.connect_reply = Some(reply);
                    }
                }
            }

            if tcp_entry.connect_reply.is_none() {
                // Drain outgoing chunks into smoltcp TX buffer.
                if sock.may_send() {
                    loop {
                        if tcp_entry.pending_offset < tcp_entry.pending_tx.len() {
                            if !sock.can_send() {
                                break;
                            }
                            let rest = &tcp_entry.pending_tx[tcp_entry.pending_offset..];
                            match sock.send_slice(rest) {
                                Ok(n) if n > 0 => {
                                    tcp_entry.pending_offset += n;
                                    tcp_entry.last_active = Instant::now();
                                    if tcp_entry.pending_offset >= tcp_entry.pending_tx.len() {
                                        tcp_entry.pending_tx.clear();
                                        tcp_entry.pending_offset = 0;
                                    }
                                }
                                _ => break,
                            }
                        } else if !tcp_entry.client_eof {
                            match tcp_entry.tx_rx.try_recv() {
                                Ok(chunk) => {
                                    tcp_entry.pending_tx = chunk;
                                    tcp_entry.pending_offset = 0;
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    tcp_entry.client_eof = true;
                                    sock.close();
                                    break;
                                }
                            }
                        } else {
                            sock.close();
                            break;
                        }
                    }
                }

                // Read incoming smoltcp RX buffer only when channel has capacity (backpressure).
                let mut aborted = false;
                while sock.can_recv() {
                    let Some(rx_tx) = tcp_entry.rx_tx.as_ref() else {
                        break;
                    };
                    match rx_tx.try_reserve() {
                        Ok(permit) => match sock.recv_slice(&mut recv_scratch) {
                            Ok(n) if n > 0 => {
                                tcp_entry.last_active = Instant::now();
                                stats.record_success();
                                permit.send(recv_scratch[..n].to_vec());
                            }
                            _ => break,
                        },
                        Err(mpsc::error::TrySendError::Full(_)) => break,
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            aborted = true;
                            break;
                        }
                    }
                }

                if aborted {
                    tcp_entry.rx_tx = None;
                    sock.abort();
                    to_remove.push(sock_handle);
                    continue;
                }

                if !sock.can_recv() && !sock.may_recv() {
                    // Remote enviou FIN (CloseWait) ou encerrou: fecha rx_tx para o proxy enviar FIN ao cliente.
                    tcp_entry.rx_tx = None;
                }

                if (tcp_entry.client_eof || tcp_entry.rx_tx.is_none())
                    && tcp_entry.closing_since.is_none()
                {
                    tcp_entry.closing_since = Some(Instant::now());
                }

                let closing_timed_out = tcp_entry
                    .closing_since
                    .is_some_and(|t| t.elapsed() >= TCP_CLOSING_TIMEOUT);
                let fully_closed = (matches!(sock.state(), TcpState::Closed | TcpState::TimeWait)
                    && !sock.can_recv())
                    || (tcp_entry.client_eof
                        && tcp_entry.pending_tx.is_empty()
                        && tcp_entry.rx_tx.is_none()
                        && !sock.may_send());
                let idle = tcp_entry.last_active.elapsed() >= TCP_IDLE_TIMEOUT;
                if fully_closed || closing_timed_out || idle {
                    if closing_timed_out || idle {
                        sock.abort();
                    }
                    to_remove.push(sock_handle);
                }
            }
        }

        for h in to_remove {
            if let Some(entry) = active_tcps.remove(&h) {
                used_ports.remove(&entry.local_port);
            }
            sockets.remove(h);
        }

        // Pump active UDP sockets <-> Tokio channels without blocking.
        let mut udp_to_remove = Vec::new();
        let mut udp_recv_scratch = [0u8; 2048];
        for (&sock_handle, udp_entry) in &mut active_udps {
            let sock = sockets.get_mut::<udp::Socket>(sock_handle);

            for _ in 0..64 {
                if udp_entry.pending_tx.is_none() && !udp_entry.closed {
                    match udp_entry.tx_rx.try_recv() {
                        Ok(dgram) => udp_entry.pending_tx = Some(dgram),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            udp_entry.closed = true;
                            break;
                        }
                    }
                }
                let Some(dgram) = udp_entry.pending_tx.as_ref() else {
                    break;
                };
                if !sock.can_send() {
                    break;
                }
                let ep = IpEndpoint::new(
                    IpAddress::Ipv4(Ipv4Address::from(dgram.peer.ip().octets())),
                    dgram.peer.port(),
                );
                if sock.send_slice(&dgram.data, ep).is_ok() {
                    udp_entry.pending_tx = None;
                    udp_entry.last_active = Instant::now();
                } else {
                    break;
                }
            }

            while sock.can_recv() {
                match sock.recv_slice(&mut udp_recv_scratch) {
                    Ok((n, meta)) => {
                        let IpAddress::Ipv4(v4) = meta.endpoint.addr;
                        let peer =
                            SocketAddrV4::new(Ipv4Addr::from(v4.octets()), meta.endpoint.port);
                        udp_entry.last_active = Instant::now();
                        stats.record_success();
                        match udp_entry.rx_tx.try_send(UdpDatagram {
                            peer,
                            data: udp_recv_scratch[..n].to_vec(),
                        }) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                udp_entry.closed = true;
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }

            if udp_entry.closed || udp_entry.last_active.elapsed() >= UDP_IDLE_TIMEOUT {
                udp_to_remove.push(sock_handle);
            }
        }

        for h in udp_to_remove {
            if let Some(entry) = active_udps.remove(&h) {
                used_udp_ports.remove(&entry.local_port);
            }
            sockets.remove(h);
        }

        // Flush any TCP/UDP packets queued during recv/send above.
        for _ in 0..MAX_SMOL_POLL_ITERS {
            let polled = iface.poll_egress(smol_now(), &mut device, &mut sockets);
            flush_tx_queue(
                &mut device,
                peer_addr,
                &mut tunn,
                &mut wg_buf,
                &udp,
                &stats,
                &mut tx_diag_count,
                debug,
            )
            .await;
            if polled == PollResult::None {
                break;
            }
        }

        let poll_wait = iface
            .poll_delay(smol_now(), &sockets)
            .map(|d| Duration::from_micros(d.total_micros()).min(Duration::from_millis(50)))
            .unwrap_or_else(|| Duration::from_millis(250));

        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = wake.notified() => {}
            _ = tokio::time::sleep(poll_wait) => {}
            _ = wg_tick.tick() => {
                if peer_addr.is_none() {
                    if let Ok(resolved) = resolve_endpoint(&config.endpoint).await {
                        if debug {
                            eprintln!(
                                "[pulpo] vpn: resolved endpoint {} -> {resolved}",
                                config.endpoint
                            );
                        }
                        peer_addr = Some(resolved);
                        stats.clear_error();
                    }
                }
                if let Some(peer) = peer_addr {
                    match tunn.update_timers(&mut wg_buf) {
                        TunnResult::WriteToNetwork(pkt) => {
                            if let Err(err) = udp.send_to(pkt, peer).await {
                                if debug {
                                    eprintln!("[pulpo] vpn: udp send_to({peer}) timer error: {err}");
                                }
                                stats.set_error("vpn.error.network");
                            }
                        }
                        TunnResult::Err(err) => {
                            if debug {
                                eprintln!("[pulpo] vpn: wireguard timer error: {err:?}");
                            }
                            stats.set_error("vpn.error.handshake");
                            stats.record_failure();
                            if matches!(err, WireGuardError::ConnectionExpired) {
                                stats.mark_disconnected();
                                last_handshake_sent = None;
                            }
                        }
                        _ => {}
                    }
                    if !stats.is_connected()
                        && last_handshake_sent
                            .is_none_or(|sent| sent.elapsed() >= Duration::from_secs(2))
                    {
                        if let TunnResult::WriteToNetwork(pkt) =
                            tunn.format_handshake_initiation(&mut wg_buf, true)
                        {
                            if debug {
                                eprintln!(
                                    "[pulpo] vpn: resending handshake ({} bytes) to {peer}",
                                    pkt.len()
                                );
                            }
                            if let Err(err) = udp.send_to(pkt, peer).await {
                                if debug {
                                    eprintln!("[pulpo] vpn: udp send_to({peer}) resend error: {err}");
                                }
                                stats.set_error("vpn.error.network");
                            }
                        }
                        last_handshake_sent = Some(Instant::now());
                    }
                }
            }
            recv = udp.recv_from(&mut udp_rx) => {
                match recv {
                    Ok((first_n, first_src)) => {
                        let mut pkt = Some((first_n, first_src));
                        for _ in 0..MAX_UDP_RX_BATCH {
                            let Some((n, src)) = pkt.take() else {
                                break;
                            };
                            if Some(src) == peer_addr {
                                let log_rx = debug && rx_diag_count < 12;
                                if log_rx {
                                    rx_diag_count += 1;
                                }
                                let res = tunn.decapsulate(Some(src.ip()), &udp_rx[..n], &mut wg_buf);
                                handle_decap(res, n, log_rx, debug, src, &mut tunn, &udp, &mut device, &stats).await;
                            } else if debug && rx_diag_count < 12 {
                                rx_diag_count += 1;
                                eprintln!(
                                    "[pulpo] vpn: rx udp {n}B from unexpected {src} (expected {peer_addr:?})"
                                );
                            }
                            match udp.try_recv_from(&mut udp_rx) {
                                Ok(next) => pkt = Some(next),
                                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(err) => {
                                    if debug {
                                        eprintln!("[pulpo] vpn: udp try_recv_from error: {err}");
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if debug {
                            eprintln!("[pulpo] vpn: udp recv_from error: {err}");
                        }
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(first) = cmd else { break; };
                let mut current_cmd = Some(first);
                let mut cmd_disconnected = false;
                for i in 0..MAX_CMD_BATCH {
                    let Some(next_cmd) = current_cmd.take() else {
                        break;
                    };
                    match next_cmd {
                        TunnelCmd::ResolveIpv4 { host, reply } => {
                            let key = host.to_ascii_lowercase();
                            let cached = dns_cache
                                .get(&key)
                                .filter(|(_, at)| at.elapsed() < DNS_CACHE_TTL)
                                .map(|(ips, _)| ips.clone());
                            if let Some(ips) = cached {
                                let _ = reply.send(Ok(ips));
                            } else if let Some(existing) =
                                pending_dns.iter_mut().find(|p| p.host == key)
                            {
                                existing.replies.push(reply);
                            } else {
                                if let Some(peer) = peer_addr {
                                    if !stats.is_connected() {
                                        if let TunnResult::WriteToNetwork(pkt) =
                                            tunn.format_handshake_initiation(&mut wg_buf, false)
                                        {
                                            let _ = udp.send_to(pkt, peer).await;
                                        }
                                    }
                                }
                                let use_fallback = dns_pref.use_fallback(Instant::now());
                                let target_handle = if use_fallback {
                                    fallback_dns_handle
                                } else {
                                    dns_handle
                                };
                                let dns_sock = sockets.get_mut::<dns::Socket>(target_handle);
                                match dns_sock.start_query(iface.context(), &host, DnsQueryType::A) {
                                    Ok(handle) => pending_dns.push(PendingDns {
                                        host: key,
                                        handle,
                                        using_fallback: use_fallback,
                                        tried_alternate: false,
                                        started_at: Instant::now(),
                                        replies: vec![reply],
                                    }),
                                    Err(_) => {
                                        let _ = reply.send(Err("vpn.error.dnsBusy".into()));
                                    }
                                }
                            }
                        }
                        TunnelCmd::ConnectTcp { remote, reply } => {
                            if active_tcps.len() >= MAX_TCP_SOCKETS {
                                let _ = reply.send(Err("vpn.error.tooManyConns".into()));
                            } else if let IpAddr::V4(v4) = remote.ip() {
                                if let Some(port) =
                                    alloc_ephemeral_port(&mut next_ephemeral_port, &used_ports)
                                {
                                    let mut sock = tcp::Socket::new(
                                        tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]),
                                        tcp::SocketBuffer::new(vec![0u8; TCP_BUF_SIZE]),
                                    );
                                    sock.set_nagle_enabled(false);
                                    let remote_ep = (v4, remote.port());
                                    if sock.connect(iface.context(), remote_ep, port).is_err() {
                                        let _ = reply.send(Err("vpn.error.connectFailed".into()));
                                    } else {
                                        used_ports.insert(port);
                                        let sock_handle = sockets.add(sock);
                                        let (rx_tx, from_tunnel) = mpsc::channel(TCP_CHANNEL_CAP);
                                        let (to_tunnel, tx_rx) = mpsc::channel(TCP_CHANNEL_CAP);
                                        let bridge = TcpBridge {
                                            to_tunnel,
                                            from_tunnel,
                                            wake: wake.clone(),
                                        };
                                        active_tcps.insert(sock_handle, ActiveTcp {
                                            local_port: port,
                                            rx_tx: Some(rx_tx),
                                            tx_rx,
                                            pending_tx: Vec::new(),
                                            pending_offset: 0,
                                            client_eof: false,
                                            closing_since: None,
                                            connect_reply: Some(reply),
                                            pending_bridge: Some(bridge),
                                            started_at: Instant::now(),
                                            last_active: Instant::now(),
                                        });
                                    }
                                } else {
                                    let _ = reply.send(Err("vpn.error.tooManyConns".into()));
                                }
                            } else {
                                let _ = reply.send(Err("vpn.error.connectFailed".into()));
                            }
                        }
                        TunnelCmd::BindUdp { reply } => {
                            if active_udps.len() >= MAX_UDP_SOCKETS {
                                let _ = reply.send(Err("vpn.error.tooManyConns".into()));
                            } else if let Some(port) =
                                alloc_ephemeral_port(&mut next_udp_port, &used_udp_ports)
                            {
                                let mut sock = udp::Socket::new(
                                    udp::PacketBuffer::new(
                                        vec![udp::PacketMetadata::EMPTY; UDP_META_CAP],
                                        vec![0u8; UDP_BUF_SIZE],
                                    ),
                                    udp::PacketBuffer::new(
                                        vec![udp::PacketMetadata::EMPTY; UDP_META_CAP],
                                        vec![0u8; UDP_BUF_SIZE],
                                    ),
                                );
                                if sock.bind(port).is_err() {
                                    let _ = reply.send(Err("vpn.error.connectFailed".into()));
                                } else {
                                    used_udp_ports.insert(port);
                                    let sock_handle = sockets.add(sock);
                                    let (rx_tx, from_tunnel) = mpsc::channel(UDP_CHANNEL_CAP);
                                    let (to_tunnel, tx_rx) = mpsc::channel(UDP_CHANNEL_CAP);
                                    let bridge = UdpBridge {
                                        local_ip: config.tunnel_ip,
                                        local_port: port,
                                        to_tunnel,
                                        from_tunnel,
                                        wake: wake.clone(),
                                    };
                                    active_udps.insert(
                                        sock_handle,
                                        ActiveUdp {
                                            local_port: port,
                                            rx_tx,
                                            tx_rx,
                                            pending_tx: None,
                                            closed: false,
                                            last_active: Instant::now(),
                                        },
                                    );
                                    let _ = reply.send(Ok(bridge));
                                }
                            } else {
                                let _ = reply.send(Err("vpn.error.tooManyConns".into()));
                            }
                        }
                    }
                    if i + 1 < MAX_CMD_BATCH {
                        match cmd_rx.try_recv() {
                            Ok(more) => current_cmd = Some(more),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => {
                                cmd_disconnected = true;
                                break;
                            }
                        }
                    }
                }
                if cmd_disconnected {
                    break;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_decap(
    res: TunnResult<'_>,
    wg_len: usize,
    log_rx: bool,
    debug: bool,
    peer: SocketAddr,
    tunn: &mut Tunn,
    udp: &UdpSocket,
    device: &mut VirtualIpDevice,
    stats: &TunnelStats,
) {
    let mut follow_buf = vec![0u8; WG_OVERHEAD_BUF];
    let mut current = res;
    let mut first = true;
    loop {
        match current {
            TunnResult::WriteToNetwork(pkt) => {
                if first {
                    // HandshakeResponse completion emits the first keepalive packet to peer.
                    stats.mark_connected(debug);
                }
                if log_rx {
                    eprintln!(
                        "[pulpo] vpn: rx wg {wg_len}B -> WriteToNetwork({}B)",
                        pkt.len()
                    );
                }
                if let Err(err) = udp.send_to(pkt, peer).await {
                    if debug {
                        eprintln!("[pulpo] vpn: udp send_to({peer}) decap error: {err}");
                    }
                }
            }
            TunnResult::WriteToTunnelV4(ip_pkt, _) => {
                stats.mark_connected(debug);
                if log_rx {
                    eprintln!(
                        "[pulpo] vpn: rx wg {wg_len}B -> WriteToTunnelV4({})",
                        ipv4_summary(ip_pkt)
                    );
                }
                if device.rx_queue.len() < MAX_RX_QUEUE {
                    device.rx_queue.push_back(ip_pkt.to_vec());
                }
            }
            TunnResult::Err(err) => {
                if debug {
                    eprintln!("[pulpo] vpn: wireguard decapsulate error from {peer}: {err:?}");
                }
                stats.set_error("vpn.error.handshake");
                break;
            }
            TunnResult::WriteToTunnelV6(ip_pkt, _) => {
                if log_rx {
                    eprintln!(
                        "[pulpo] vpn: rx wg {wg_len}B -> WriteToTunnelV6({}B)",
                        ip_pkt.len()
                    );
                }
                break;
            }
            TunnResult::Done => {
                if first {
                    stats.record_rx();
                    if log_rx {
                        eprintln!("[pulpo] vpn: rx wg {wg_len}B -> Done");
                    }
                }
                break;
            }
        }
        first = false;
        current = tunn.decapsulate(None, &[], &mut follow_buf);
    }
}

pub async fn run_connectivity_test(handle: &TunnelHandle) -> Result<u64, String> {
    let debug = vpn_debug();
    let started = Instant::now();
    if debug {
        eprintln!("[pulpo] vpn: connectivity test starting (waiting for handshake)...");
    }
    handle.wait_handshake(Duration::from_secs(6)).await?;
    if debug {
        eprintln!(
            "[pulpo] vpn: handshake ready in {}ms, resolving discord.com...",
            started.elapsed().as_millis()
        );
    }
    let ips = handle.resolve_ipv4("discord.com".into()).await?;
    let ip = ips
        .into_iter()
        .find(|ip| !super::config::is_blocked_ipv4_core(*ip))
        .ok_or("vpn.error.dnsFailed")?;
    if debug {
        eprintln!("[pulpo] vpn: connecting TCP {ip}:443 via tunnel...");
    }
    let _bridge = handle
        .connect_tcp(SocketAddr::new(IpAddr::V4(ip), 443))
        .await?;
    let ms = started.elapsed().as_millis() as u64;
    if debug {
        eprintln!("[pulpo] vpn: connectivity test succeeded in {ms}ms");
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};

    fn build_ipv4_udp_packet(
        src: Ipv4Address,
        dst: Ipv4Address,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let udp_repr = UdpRepr { src_port, dst_port };
        let ip_repr = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Udp,
            payload_len: udp_repr.header_len() + payload.len(),
            hop_limit: 64,
        };
        let total_len = ip_repr.buffer_len() + ip_repr.payload_len;
        let mut buf = vec![0u8; total_len];
        let caps = ChecksumCapabilities::default();

        let mut ip_pkt = Ipv4Packet::new_unchecked(&mut buf);
        ip_repr.emit(&mut ip_pkt, &caps);
        let mut udp_pkt = UdpPacket::new_unchecked(ip_pkt.payload_mut());
        udp_repr.emit(
            &mut udp_pkt,
            &IpAddress::Ipv4(src),
            &IpAddress::Ipv4(dst),
            payload.len(),
            |dst_payload| dst_payload.copy_from_slice(payload),
            &caps,
        );
        buf
    }

    #[test]
    fn padded_wireguard_packet_roundtrips_through_boringtun() {
        let a_sec = StaticSecret::from([11u8; 32]);
        let a_pub = PublicKey::from(&a_sec);
        let b_sec = StaticSecret::from([22u8; 32]);
        let b_pub = PublicKey::from(&b_sec);

        let mut a = Tunn::new(a_sec, b_pub, None, None, 1, None);
        let mut b = Tunn::new(b_sec, a_pub, None, None, 2, None);

        let mut buf_a = vec![0u8; WG_OVERHEAD_BUF];
        let mut buf_b = vec![0u8; WG_OVERHEAD_BUF];

        let TunnResult::WriteToNetwork(init_pkt) = a.format_handshake_initiation(&mut buf_a, false)
        else {
            panic!("expected handshake initiation");
        };
        let TunnResult::WriteToNetwork(resp_pkt) = b.decapsulate(None, init_pkt, &mut buf_b) else {
            panic!("expected handshake response");
        };
        let TunnResult::WriteToNetwork(keepalive_pkt) = a.decapsulate(None, resp_pkt, &mut buf_a)
        else {
            panic!("expected handshake keepalive");
        };
        assert!(matches!(
            b.decapsulate(None, keepalive_pkt, &mut buf_b),
            TunnResult::Done
        ));

        // 20B IPv4 + 8B UDP + 29B payload = 57B (not divisible by 16).
        let ip_pkt = build_ipv4_udp_packet(
            Ipv4Address::new(10, 2, 0, 2),
            Ipv4Address::new(10, 2, 0, 1),
            51234,
            53,
            &[0xabu8; 29],
        );
        assert_eq!(ip_pkt.len(), 57);

        let TunnResult::WriteToNetwork(wg_pkt) = encapsulate_padded(&mut a, &ip_pkt, &mut buf_a)
        else {
            panic!("expected encrypted wireguard packet");
        };
        // WireGuard header (16B) + padded plaintext (64B) + AEAD tag (16B) = 96B.
        assert_eq!(wg_pkt.len(), 32 + 64);

        let TunnResult::WriteToTunnelV4(decapped, src) = b.decapsulate(None, wg_pkt, &mut buf_b)
        else {
            panic!("expected decrypted IPv4 packet");
        };
        assert_eq!(src, Ipv4Addr::new(10, 2, 0, 2));
        assert_eq!(decapped, ip_pkt.as_slice());
    }

    #[test]
    fn smoltcp_dns_query_emits_and_parses_reply_on_slash32() {
        let mut device = VirtualIpDevice::new(1280);
        let mut iface_cfg = IfaceConfig::new(HardwareAddress::Ip);
        iface_cfg.random_seed = 0x1234_5678_9abc_def1;
        let mut iface = Interface::new(iface_cfg, &mut device, SmolInstant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::new(10, 2, 0, 2)),
                    32,
                ))
                .unwrap();
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 0))
            .unwrap();

        let dns_servers = [IpAddress::Ipv4(Ipv4Address::new(10, 2, 0, 1))];
        let mut queries = Vec::with_capacity(4);
        queries.resize_with(4, || None);
        let dns_sock = dns::Socket::new(&dns_servers, queries);
        let mut sockets = SocketSet::new(vec![]);
        let dns_handle = sockets.add(dns_sock);

        let qhandle = {
            let sock = sockets.get_mut::<dns::Socket>(dns_handle);
            sock.start_query(iface.context(), "discord.com", DnsQueryType::A)
                .expect("DNS query should start")
        };

        iface.poll(SmolInstant::from_millis(1), &mut device, &mut sockets);
        let outgoing = device
            .tx_queue
            .pop_front()
            .expect("smoltcp should emit DNS query IPv4 packet on /32");

        let ip_pkt = Ipv4Packet::new_checked(&outgoing).expect("valid outgoing IPv4");
        assert!(ip_pkt.verify_checksum());
        assert_eq!(ip_pkt.src_addr(), Ipv4Address::new(10, 2, 0, 2));
        assert_eq!(ip_pkt.dst_addr(), Ipv4Address::new(10, 2, 0, 1));
        assert_eq!(ip_pkt.next_header(), IpProtocol::Udp);

        let udp_pkt = UdpPacket::new_checked(ip_pkt.payload()).expect("valid outgoing UDP");
        assert!(udp_pkt.verify_checksum(
            &IpAddress::Ipv4(Ipv4Address::new(10, 2, 0, 2)),
            &IpAddress::Ipv4(Ipv4Address::new(10, 2, 0, 1)),
        ));
        let query_src_port = udp_pkt.src_port();
        assert_eq!(udp_pkt.dst_port(), 53);

        let udp_payload = udp_pkt.payload();
        assert!(udp_payload.len() > 12);
        let txid = [udp_payload[0], udp_payload[1]];
        let question = &udp_payload[12..];

        let mut dns_reply = Vec::with_capacity(12 + question.len() + 16);
        dns_reply.extend_from_slice(&txid);
        dns_reply.extend_from_slice(&0x8180u16.to_be_bytes()); // standard response, RD+RA
        dns_reply.extend_from_slice(&1u16.to_be_bytes()); // qdcount = 1
        dns_reply.extend_from_slice(&1u16.to_be_bytes()); // ancount = 1
        dns_reply.extend_from_slice(&0u16.to_be_bytes()); // nscount = 0
        dns_reply.extend_from_slice(&0u16.to_be_bytes()); // arcount = 0
        dns_reply.extend_from_slice(question);
        dns_reply.extend_from_slice(&[
            0xc0, 0x0c, // name pointer to question
            0x00, 0x01, // type A
            0x00, 0x01, // class IN
            0x00, 0x00, 0x00, 0x3c, // ttl 60s
            0x00, 0x04, // rdlength 4
            162, 159, 135, 232, // rdata
        ]);

        let incoming = build_ipv4_udp_packet(
            Ipv4Address::new(10, 2, 0, 1),
            Ipv4Address::new(10, 2, 0, 2),
            53,
            query_src_port,
            &dns_reply,
        );
        device.rx_queue.push_back(incoming);

        iface.poll(SmolInstant::from_millis(10), &mut device, &mut sockets);
        let sock = sockets.get_mut::<dns::Socket>(dns_handle);
        let addrs = sock
            .get_query_result(qhandle)
            .expect("DNS query should succeed");
        assert!(addrs.contains(&IpAddress::Ipv4(Ipv4Address::new(162, 159, 135, 232))));
    }

    #[test]
    fn tunnel_stats_disconnect_and_failure_tracking() {
        let stats = TunnelStats::default();
        assert!(!stats.is_connected());
        assert_eq!(stats.consecutive_failures(), 0);

        stats.record_failure();
        stats.record_failure();
        assert_eq!(stats.consecutive_failures(), 2);

        stats.mark_connected(false);
        assert!(stats.is_connected());
        stats.record_success();
        assert_eq!(stats.consecutive_failures(), 0);

        stats.mark_disconnected();
        assert!(!stats.is_connected());
    }

    #[test]
    fn dns_cache_evicts_expired_and_caps_size() {
        let mut cache = HashMap::new();
        for i in 0..MAX_DNS_CACHE {
            cache.insert(
                format!("host-{i}.test"),
                (vec![Ipv4Addr::new(1, 1, 1, 1)], Instant::now()),
            );
        }
        assert_eq!(cache.len(), MAX_DNS_CACHE);
        insert_dns_cache(
            &mut cache,
            "fresh.test".to_string(),
            vec![Ipv4Addr::new(1, 1, 1, 1)],
        );
        assert!(cache.len() <= MAX_DNS_CACHE);
        assert!(cache.contains_key("fresh.test"));
    }

    #[test]
    fn ephemeral_port_allocator_skips_in_use_ports() {
        let mut used = HashSet::new();
        used.insert(49152);
        used.insert(49153);
        let mut next = 49152;
        let chosen = alloc_ephemeral_port(&mut next, &used);
        assert_eq!(chosen, Some(49154));
        assert_eq!(next, 49155);
    }

    #[test]
    fn dns_preference_sticks_to_fallback_and_recovers() {
        let now = Instant::now();
        let mut pref = DnsPreference::default();
        assert!(!pref.use_fallback(now));

        pref.mark_fallback_ok(now);
        assert!(pref.use_fallback(now + Duration::from_secs(60)));
        assert!(!pref.use_fallback(now + DNS_FALLBACK_STICKY + Duration::from_secs(1)));

        pref.mark_fallback_failed();
        assert!(!pref.use_fallback(now));
    }

    #[test]
    fn smoltcp_remote_fin_transitions_to_close_wait_and_clears_may_recv() {
        fn pump(
            t: i64,
            c_iface: &mut Interface,
            c_dev: &mut VirtualIpDevice,
            c_socks: &mut SocketSet<'_>,
            s_iface: &mut Interface,
            s_dev: &mut VirtualIpDevice,
            s_socks: &mut SocketSet<'_>,
        ) {
            let now = SmolInstant::from_millis(t);
            for _ in 0..4 {
                c_iface.poll(now, c_dev, c_socks);
                while let Some(pkt) = c_dev.tx_queue.pop_front() {
                    s_dev.rx_queue.push_back(pkt);
                }
                s_iface.poll(now, s_dev, s_socks);
                while let Some(pkt) = s_dev.tx_queue.pop_front() {
                    c_dev.rx_queue.push_back(pkt);
                }
            }
        }

        let mut client_dev = VirtualIpDevice::new(1280);
        let mut client_cfg = IfaceConfig::new(HardwareAddress::Ip);
        client_cfg.random_seed = 0x1111_2222_3333_4444;
        let mut client_iface =
            Interface::new(client_cfg, &mut client_dev, SmolInstant::from_millis(0));
        client_iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::new(10, 2, 0, 2)),
                    32,
                ))
                .unwrap();
        });
        client_iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 0))
            .unwrap();

        let mut server_dev = VirtualIpDevice::new(1280);
        let mut server_cfg = IfaceConfig::new(HardwareAddress::Ip);
        server_cfg.random_seed = 0x5555_6666_7777_8888;
        let mut server_iface =
            Interface::new(server_cfg, &mut server_dev, SmolInstant::from_millis(0));
        server_iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::new(162, 159, 135, 232)),
                    32,
                ))
                .unwrap();
        });
        server_iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 0))
            .unwrap();

        let mut client_sockets = SocketSet::new(vec![]);
        let mut server_sockets = SocketSet::new(vec![]);

        let mut client_sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        );
        client_sock
            .connect(
                client_iface.context(),
                (Ipv4Address::new(162, 159, 135, 232), 443),
                50000,
            )
            .expect("connect");
        let ch = client_sockets.add(client_sock);

        let mut server_sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 4096]),
            tcp::SocketBuffer::new(vec![0u8; 4096]),
        );
        server_sock.listen(443).expect("listen");
        let sh = server_sockets.add(server_sock);

        pump(
            10,
            &mut client_iface,
            &mut client_dev,
            &mut client_sockets,
            &mut server_iface,
            &mut server_dev,
            &mut server_sockets,
        );
        assert_eq!(
            client_sockets.get::<tcp::Socket>(ch).state(),
            TcpState::Established
        );
        assert!(client_sockets.get::<tcp::Socket>(ch).may_recv());

        // Server envia FIN para simular encerramento remoto de conexão keep-alive.
        server_sockets.get_mut::<tcp::Socket>(sh).close();
        pump(
            20,
            &mut client_iface,
            &mut client_dev,
            &mut client_sockets,
            &mut server_iface,
            &mut server_dev,
            &mut server_sockets,
        );

        let cs = client_sockets.get::<tcp::Socket>(ch);
        assert_eq!(cs.state(), TcpState::CloseWait);
        assert!(!cs.can_recv());
        assert!(!cs.may_recv());
    }

    #[test]
    fn tunnel_stats_idle_is_not_stalled() {
        let stats = TunnelStats::default();
        assert!(!stats.is_stalled(Duration::ZERO));
        stats.record_rx();
        assert!(!stats.is_stalled(Duration::ZERO));
    }

    #[test]
    fn tunnel_stats_tx_without_rx_stalls_and_rx_clears_it() {
        let stats = TunnelStats::default();
        stats.record_tx();
        std::thread::sleep(Duration::from_millis(20));
        assert!(stats.is_stalled(Duration::from_millis(10)));
        stats.record_rx();
        assert!(!stats.is_stalled(Duration::ZERO));
    }
}
