//! Configuração do túnel WireGuard por app e armazenamento seguro da chave
//! privada no chaveiro do sistema.

use std::net::{IpAddr, Ipv4Addr};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const KEYRING_SERVICE: &str = "com.pulpo.app.vpn";
const KEYRING_USER_PREFIX: &str = "wg-";
pub const MAX_PROFILES: usize = 3;
pub const MAX_CONF_BYTES: usize = 16 * 1024;
pub const MIN_MTU: u16 = 1280;
pub const MAX_MTU: u16 = 1420;
pub const DEFAULT_MTU: u16 = 1380;
pub const DEFAULT_KEEPALIVE: u16 = 25;
pub const MAX_KEEPALIVE: u16 = 120;
const FALLBACK_DNS: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VpnSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<VpnProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<String>,
    #[serde(default = "default_auto_failover")]
    pub auto_failover: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<String>,
    // Campos legados mantidos apenas para desserializar configuracoes antigas.
    #[serde(default, skip_serializing)]
    pub label: Option<String>,
    #[serde(default, skip_serializing)]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing)]
    pub public_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub address: Option<String>,
    #[serde(default, skip_serializing)]
    pub dns: Option<String>,
    #[serde(default = "default_mtu", skip_serializing)]
    pub mtu: u16,
    #[serde(default = "default_keepalive", skip_serializing)]
    pub keepalive: u16,
}

const fn default_mtu() -> u16 {
    DEFAULT_MTU
}

const fn default_keepalive() -> u16 {
    DEFAULT_KEEPALIVE
}

const fn default_auto_failover() -> bool {
    true
}

impl Default for VpnSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            profiles: Vec::new(),
            active_profile_id: None,
            auto_failover: true,
            services: Vec::new(),
            label: None,
            endpoint: None,
            public_key: None,
            address: None,
            dns: None,
            mtu: DEFAULT_MTU,
            keepalive: DEFAULT_KEEPALIVE,
        }
    }
}

impl VpnSettings {
    pub fn is_default(&self) -> bool {
        !self.enabled
            && self.profiles.is_empty()
            && self.active_profile_id.is_none()
            && self.auto_failover
            && self.services.is_empty()
            && self.endpoint.is_none()
    }

    pub fn has_config(&self) -> bool {
        !self.profiles.is_empty()
            || (self.endpoint.is_some() && self.public_key.is_some() && self.address.is_some())
    }

    /// Converte uma configuracao legada de perfil unico para `profiles` +
    /// `active_profile_id`, garantindo retrocompatibilidade com `wg-default`.
    pub fn migrate_legacy(&mut self) -> bool {
        let mut changed = false;
        if self.profiles.is_empty() {
            if let (Some(endpoint), Some(public_key), Some(address)) = (
                self.endpoint.take(),
                self.public_key.take(),
                self.address.take(),
            ) {
                let id = "default".to_string();
                self.profiles.push(VpnProfile {
                    id: id.clone(),
                    label: self.label.take(),
                    endpoint,
                    public_key,
                    address,
                    dns: self.dns.take().unwrap_or_else(|| FALLBACK_DNS.to_string()),
                    mtu: self.mtu.clamp(MIN_MTU, MAX_MTU),
                    keepalive: self.keepalive.min(MAX_KEEPALIVE),
                });
                self.active_profile_id = Some(id);
                self.mtu = DEFAULT_MTU;
                self.keepalive = DEFAULT_KEEPALIVE;
                changed = true;
            }
        }
        if !self.profiles.is_empty() {
            let valid_active = self
                .active_profile_id
                .as_ref()
                .is_some_and(|id| self.profiles.iter().any(|p| &p.id == id));
            if !valid_active {
                self.active_profile_id = Some(self.profiles[0].id.clone());
                changed = true;
            }
        } else if self.active_profile_id.is_some() {
            self.active_profile_id = None;
            changed = true;
        }
        changed
    }

    pub fn active_profile(&self) -> Option<&VpnProfile> {
        self.active_profile_id
            .as_deref()
            .and_then(|id| self.profiles.iter().find(|p| p.id == id))
            .or_else(|| self.profiles.first())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VpnProfile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub endpoint: String,
    pub public_key: String,
    pub address: String,
    pub dns: String,
    pub mtu: u16,
    pub keepalive: u16,
}

pub fn generate_profile_id(existing: &[VpnProfile]) -> String {
    loop {
        let mut bytes = [0u8; 4];
        getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable for profile ID");
        let candidate = format!("p-{:08x}", u32::from_ne_bytes(bytes));
        if existing.iter().all(|p| p.id != candidate) {
            return candidate;
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WgSecrets {
    pub private_key: [u8; 32],
    #[serde(default)]
    pub preshared_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for WgSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgSecrets").finish_non_exhaustive()
    }
}

impl Drop for WgSecrets {
    fn drop(&mut self) {
        self.private_key.zeroize();
        if let Some(psk) = &mut self.preshared_key {
            psk.zeroize();
        }
    }
}

pub struct ParsedConf {
    pub profile: VpnProfile,
    pub secrets: WgSecrets,
}

#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub endpoint: String,
    pub peer_public_key: [u8; 32],
    pub tunnel_ip: Ipv4Addr,
    pub tunnel_prefix: u8,
    pub dns_ip: Ipv4Addr,
    pub mtu: u16,
    pub keepalive: u16,
    pub secrets: WgSecrets,
}

pub fn build_validated_profile(
    profile: &VpnProfile,
    secrets: WgSecrets,
) -> Result<ValidatedConfig, String> {
    validate_endpoint(&profile.endpoint)?;
    let peer_public_key = decode_key(&profile.public_key)?;
    let (tunnel_ip, tunnel_prefix) = parse_ipv4_cidr(&profile.address)?;
    let dns_ip = parse_dns_ipv4(&profile.dns)?;
    Ok(ValidatedConfig {
        endpoint: profile.endpoint.clone(),
        peer_public_key,
        tunnel_ip,
        tunnel_prefix,
        dns_ip,
        mtu: profile.mtu.clamp(MIN_MTU, MAX_MTU),
        keepalive: profile.keepalive.min(MAX_KEEPALIVE),
        secrets,
    })
}

pub fn parse_wg_conf(raw: &str, label: Option<&str>) -> Result<ParsedConf, String> {
    if raw.len() > MAX_CONF_BYTES {
        return Err("vpn.error.confTooLarge".into());
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Interface,
        Peer,
        Other,
    }

    let mut section = Section::None;
    let mut secrets = WgSecrets {
        private_key: [0u8; 32],
        preshared_key: None,
    };
    let mut has_private_key = false;
    let mut address: Option<(Ipv4Addr, u8)> = None;
    let mut dns: Option<Ipv4Addr> = None;
    let mut mtu: u16 = DEFAULT_MTU;
    let mut public_key_b64: Option<String> = None;
    let mut endpoint: Option<String> = None;
    let mut keepalive: u16 = DEFAULT_KEEPALIVE;
    let mut peer_count = 0usize;

    for raw_line in raw.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim();
            if name.eq_ignore_ascii_case("interface") {
                section = Section::Interface;
            } else if name.eq_ignore_ascii_case("peer") {
                peer_count += 1;
                if peer_count > 1 {
                    section = Section::Other;
                } else {
                    section = Section::Peer;
                }
            } else {
                section = Section::Other;
            }
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match section {
            Section::Interface => {
                if key.eq_ignore_ascii_case("PrivateKey") {
                    secrets.private_key = decode_key(val)?;
                    has_private_key = true;
                } else if key.eq_ignore_ascii_case("Address") {
                    address = Some(first_ipv4_cidr(val)?);
                } else if key.eq_ignore_ascii_case("DNS") {
                    dns = first_dns_ipv4(val);
                } else if key.eq_ignore_ascii_case("MTU") {
                    if let Ok(parsed) = val.parse::<u16>() {
                        mtu = parsed.clamp(MIN_MTU, MAX_MTU);
                    }
                }
            }
            Section::Peer => {
                if key.eq_ignore_ascii_case("PublicKey") {
                    let _ = decode_key(val)?;
                    public_key_b64 = Some(val.to_string());
                } else if key.eq_ignore_ascii_case("PresharedKey") {
                    secrets.preshared_key = Some(decode_key(val)?);
                } else if key.eq_ignore_ascii_case("Endpoint") {
                    validate_endpoint(val)?;
                    endpoint = Some(val.to_string());
                } else if key.eq_ignore_ascii_case("PersistentKeepalive") {
                    if let Ok(parsed) = val.parse::<u16>() {
                        keepalive = parsed.min(MAX_KEEPALIVE);
                    }
                }
            }
            Section::None | Section::Other => {}
        }
    }

    if !has_private_key {
        return Err("vpn.error.confInvalid".into());
    }
    let (ip, prefix) = address.ok_or("vpn.error.confInvalid")?;
    let public_key = public_key_b64.ok_or("vpn.error.confInvalid")?;
    let endpoint = endpoint.ok_or("vpn.error.confInvalid")?;
    let dns = dns.unwrap_or(FALLBACK_DNS);

    let clean_label = label.map(sanitize_label).filter(|s| !s.is_empty());

    Ok(ParsedConf {
        profile: VpnProfile {
            id: String::new(),
            label: clean_label,
            endpoint,
            public_key,
            address: format!("{ip}/{prefix}"),
            dns: dns.to_string(),
            mtu,
            keepalive,
        },
        secrets,
    })
}

fn sanitize_label(raw: &str) -> String {
    let trimmed = raw.trim();
    let no_ext = trimmed
        .strip_suffix(".conf")
        .or_else(|| trimmed.strip_suffix(".txt"))
        .unwrap_or(trimmed);
    no_ext
        .chars()
        .filter(|c| !c.is_control())
        .take(48)
        .collect::<String>()
        .trim()
        .to_string()
}

fn strip_comment(line: &str) -> &str {
    match (line.find('#'), line.find(';')) {
        (Some(a), Some(b)) => &line[..a.min(b)],
        (Some(a), None) => &line[..a],
        (None, Some(b)) => &line[..b],
        (None, None) => line,
    }
}

fn decode_key(raw: &str) -> Result<[u8; 32], String> {
    let mut bytes = Zeroizing::new(
        STANDARD
            .decode(raw.trim())
            .map_err(|_| "vpn.error.confInvalid".to_string())?,
    );
    if bytes.len() != 32 {
        return Err("vpn.error.confInvalid".into());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(out)
}

fn first_ipv4_cidr(list: &str) -> Result<(Ipv4Addr, u8), String> {
    for part in list.split(',') {
        let part = part.trim();
        if let Ok(parsed) = parse_ipv4_cidr(part) {
            return Ok(parsed);
        }
    }
    Err("vpn.error.confInvalid".into())
}

fn parse_ipv4_cidr(raw: &str) -> Result<(Ipv4Addr, u8), String> {
    let (ip_str, prefix) = match raw.trim().split_once('/') {
        Some((ip, p)) => {
            let prefix: u8 = p.trim().parse().map_err(|_| "vpn.error.confInvalid")?;
            if prefix == 0 || prefix > 32 {
                return Err("vpn.error.confInvalid".into());
            }
            (ip.trim(), prefix)
        }
        None => (raw.trim(), 32),
    };
    let ip: Ipv4Addr = ip_str.parse().map_err(|_| "vpn.error.confInvalid")?;
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.octets()[0] >= 240
    {
        return Err("vpn.error.confInvalid".into());
    }
    Ok((ip, prefix))
}

fn first_dns_ipv4(list: &str) -> Option<Ipv4Addr> {
    list.split(',').find_map(|part| parse_dns_ipv4(part).ok())
}

fn parse_dns_ipv4(raw: &str) -> Result<Ipv4Addr, String> {
    let ip: Ipv4Addr = raw.trim().parse().map_err(|_| "vpn.error.confInvalid")?;
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.octets()[0] >= 240
    {
        return Err("vpn.error.confInvalid".into());
    }
    Ok(ip)
}

pub fn validate_endpoint(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 255
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("vpn.error.confInvalid".into());
    }
    let (host, port_str) = if let Some(rest) = trimmed.strip_prefix('[') {
        let (addr, after) = rest.split_once("]:").ok_or("vpn.error.confInvalid")?;
        let ip: IpAddr = addr.parse().map_err(|_| "vpn.error.confInvalid")?;
        if is_blocked_endpoint_ip(ip) {
            return Err("vpn.error.confEndpointBlocked".into());
        }
        (addr, port_str_check(after)?)
    } else {
        let (h, p) = trimmed.rsplit_once(':').ok_or("vpn.error.confInvalid")?;
        if h.is_empty() || h.contains(':') || h.contains('/') || h.contains('@') {
            return Err("vpn.error.confInvalid".into());
        }
        if let Ok(ip) = h.parse::<IpAddr>() {
            if is_blocked_endpoint_ip(ip) {
                return Err("vpn.error.confEndpointBlocked".into());
            }
        }
        (h, port_str_check(p)?)
    };
    let _ = (host, port_str);
    Ok(())
}

fn port_str_check(raw: &str) -> Result<u16, String> {
    let port: u16 = raw.parse().map_err(|_| "vpn.error.confInvalid")?;
    if port == 0 {
        return Err("vpn.error.confInvalid".into());
    }
    Ok(port)
}

pub fn is_blocked_endpoint_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4_core(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return is_blocked_ipv4_core(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// IPs proibidos tanto para destinos do proxy CONNECT quanto para endpoints
/// literais (anti-SSRF / anti-loopback / anti-LAN).
pub fn is_blocked_ipv4_core(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        // CGNAT 100.64.0.0/10
        || (a == 100 && (64..=127).contains(&b))
        // IETF protocol assignments 192.0.0.0/24
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking 198.18.0.0/15
        || (a == 198 && (b == 18 || b == 19))
        // Documentation TEST-NET-1/2/3
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // Reserved 240.0.0.0/4
        || a >= 240
}

fn is_valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn keyring_entry_for(id: &str) -> Result<keyring::Entry, String> {
    if !is_valid_profile_id(id) {
        return Err("vpn.error.confInvalid".to_string());
    }
    let service = if crate::diag::test_home().is_some() {
        format!("{KEYRING_SERVICE}.test")
    } else {
        KEYRING_SERVICE.to_string()
    };
    let user = format!("{KEYRING_USER_PREFIX}{id}");
    keyring::Entry::new(&service, &user).map_err(|e| {
        eprintln!("[pulpo] vpn: llavero no disponible: {e}");
        "vpn.error.keyring".to_string()
    })
}

pub fn save_profile_secrets(id: &str, secrets: &WgSecrets) -> Result<(), String> {
    let payload = Zeroizing::new(
        serde_json::to_string(secrets).map_err(|_| "vpn.error.keyring".to_string())?,
    );
    keyring_entry_for(id)?.set_password(&payload).map_err(|e| {
        eprintln!("[pulpo] vpn: no se pudo guardar la clave en el llavero: {e}");
        "vpn.error.keyring".to_string()
    })
}

pub fn load_profile_secrets(id: &str) -> Result<WgSecrets, String> {
    let raw = Zeroizing::new(
        keyring_entry_for(id)?
            .get_password()
            .map_err(|_| "vpn.error.unconfigured".to_string())?,
    );
    serde_json::from_str::<WgSecrets>(&raw).map_err(|_| "vpn.error.unconfigured".to_string())
}

pub fn delete_profile_secrets(id: &str) {
    if let Ok(entry) = keyring_entry_for(id) {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CONF: &str = "\
# Proton VPN WireGuard
[Interface]
PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
Address = 10.2.0.2/32, fd00::2/128
DNS = 10.2.0.1, 2606:4700:4700::1111
MTU = 1380

[Peer]
PublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = 185.159.157.10:51820
PersistentKeepalive = 30
";

    #[test]
    fn parses_proton_conf_with_ipv6_and_comments() {
        let parsed = parse_wg_conf(SAMPLE_CONF, Some("us-free-01.conf")).unwrap();
        assert_eq!(parsed.profile.label.as_deref(), Some("us-free-01"));
        assert_eq!(parsed.profile.endpoint, "185.159.157.10:51820");
        assert_eq!(parsed.profile.address, "10.2.0.2/32");
        assert_eq!(parsed.profile.dns, "10.2.0.1");
        assert_eq!(parsed.profile.mtu, 1380);
        assert_eq!(parsed.profile.keepalive, 30);
    }

    #[test]
    fn rejects_private_endpoint() {
        let conf = SAMPLE_CONF.replace("185.159.157.10", "192.168.1.1");
        assert_eq!(
            parse_wg_conf(&conf, None).err().as_deref(),
            Some("vpn.error.confEndpointBlocked")
        );
    }

    #[test]
    fn rejects_oversized_conf() {
        let big = "a".repeat(MAX_CONF_BYTES + 1);
        assert_eq!(
            parse_wg_conf(&big, None).err().as_deref(),
            Some("vpn.error.confTooLarge")
        );
    }

    #[test]
    fn rejects_bad_key_length() {
        let conf = SAMPLE_CONF.replace("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "AAAA");
        assert_eq!(
            parse_wg_conf(&conf, None).err().as_deref(),
            Some("vpn.error.confInvalid")
        );
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_endpoint() {
        for mapped in [
            "[::ffff:127.0.0.1]:51820",
            "[::ffff:192.168.1.1]:51820",
            "[::127.0.0.1]:51820",
        ] {
            let conf = SAMPLE_CONF.replace("185.159.157.10:51820", mapped);
            assert_eq!(
                parse_wg_conf(&conf, None).err().as_deref(),
                Some("vpn.error.confEndpointBlocked"),
                "{mapped}"
            );
        }
    }

    #[test]
    fn migrates_legacy_single_profile_settings() {
        let raw = r#"{
            "enabled": true,
            "label": "pulpo-US-FREE-83",
            "endpoint": "151.243.141.165:51820",
            "public_key": "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
            "address": "10.2.0.2/32",
            "dns": "10.2.0.1",
            "mtu": 1280,
            "keepalive": 25,
            "services": ["discord"]
        }"#;
        let mut settings: VpnSettings = serde_json::from_str(raw).unwrap();
        assert!(settings.migrate_legacy());
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.profiles[0].id, "default");
        assert_eq!(settings.active_profile_id.as_deref(), Some("default"));
        assert!(settings.auto_failover);
        assert!(settings.endpoint.is_none());
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"profiles\""));
        assert!(json.contains("\"active_profile_id\""));
    }
}
