//! VPN por app con los servidores gratuitos de Urban VPN. Se usa la API de su
//! extensión de Chrome (no oficial: si la cambian, deja de funcionar), pero no
//! su código: solo sus proxies.
//!
//! Lo que hace la extensión, y aquí se repite:
//! 1. Registra un usuario anónimo y canjea su token por uno de seguridad.
//! 2. Pide la lista de países, con sus servidores proxy HTTP y una firma por
//!    servidor (vale unos 30 minutos).
//! 3. Canjea la firma del servidor elegido por un token de proxy (también
//!    unos 30 minutos), que es el usuario del proxy; la contraseña es "1".
//!
//! El motor web no sabe responder a un proxy que pide usuario y contraseña
//! (CEF no tiene dónde), así que cada país usado tiene un proxy local en
//! `127.0.0.1` sin contraseña que reenvía todo al de Urban añadiendo la
//! cabecera `Proxy-Authorization`. Las apps con ese país apuntan ahí.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use tauri::Url;

const API: &str = "https://api-pro.urban-vpn.com/rest/v1";
const COUNTRIES: &str = "https://stats.urban-vpn.com/api/rest/v2/entrypoints/countries";
const CLIENT_APP: &str = "URBAN_VPN_BROWSER_EXTENSION";
/// Un token que vence en menos de esto se renueva antes de usarlo.
const MARGIN: Duration = Duration::from_secs(120);
/// Cabeceras de una petición al proxy: más que esto no es un navegador.
const MAX_HEAD: usize = 64 * 1024;

/// WebRTC (llamadas) manda UDP directo y deja ver la IP real: con la VPN solo
/// puede ir por el proxy. Las llamadas que necesiten UDP pueden no funcionar.
#[cfg(target_os = "linux")]
pub const WEBRTC_POLICY: &str = "disable_non_proxied_udp";
#[cfg(windows)]
pub const WEBRTC_ARG: &str = "--force-webrtc-ip-handling-policy=disable_non_proxied_udp";

/// Token de seguridad de la sesión anónima, compartido por todos los países.
static SESSION: Mutex<Option<String>> = Mutex::new(None);
/// Proxy local de cada país: código ISO → puerto.
static RELAYS: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);
/// Lista de países, para no pedirla cada vez que se abre el formulario.
static LIST: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

#[derive(Deserialize)]
struct Token {
    value: String,
    #[serde(default, rename = "creationTime")]
    creation: Option<i64>,
    #[serde(default, rename = "expirationTime")]
    expiration: Option<i64>,
}

#[derive(Deserialize)]
struct CountriesResponse {
    countries: Elements<Country>,
}

#[derive(Deserialize)]
struct Elements<T> {
    #[serde(default = "Vec::new")]
    elements: Vec<T>,
}

#[derive(Deserialize)]
struct Country {
    code: Code,
    #[serde(default, rename = "accessType")]
    access: String,
    #[serde(default)]
    servers: Option<Elements<Server>>,
}

#[derive(Deserialize)]
struct Code {
    iso2: String,
}

#[derive(Deserialize)]
struct Server {
    #[serde(default, rename = "accessType")]
    access: String,
    address: Address,
    #[serde(default)]
    weight: i64,
    signature: String,
}

#[derive(Deserialize)]
struct Address {
    primary: Endpoint,
}

#[derive(Deserialize)]
struct Endpoint {
    host: String,
    port: u16,
}

impl Country {
    /// Servidores gratuitos, los de más peso primero.
    fn servers(self) -> Vec<Server> {
        if self.access != "ACCESSIBLE" {
            return Vec::new();
        }
        let mut servers: Vec<Server> = self
            .servers
            .map(|s| s.elements)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.access == "ACCESSIBLE")
            .collect();
        servers.sort_by_key(|s| std::cmp::Reverse(s.weight));
        servers
    }
}

fn post(url: &str, bearer: Option<&str>, body: serde_json::Value) -> Result<Token, String> {
    let mut request = agent().post(url).header("Accept", "application/json");
    if let Some(token) = bearer {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let text = request
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Token de seguridad: el guardado o, con `fresh` o si no hay, uno nuevo.
fn security_token(fresh: bool) -> Result<String, String> {
    let mut session = SESSION.lock().unwrap();
    if let Some(token) = session.as_ref().filter(|_| !fresh) {
        return Ok(token.clone());
    }
    let anonymous = post(
        &format!("{API}/registrations/clientApps/{CLIENT_APP}/users/anonymous"),
        None,
        serde_json::json!({ "clientApp": { "name": CLIENT_APP, "browser": "CHROME" } }),
    )?;
    let security = post(
        &format!("{API}/security/tokens/accs"),
        Some(&anonymous.value),
        serde_json::json!({ "type": "accs", "clientApp": { "name": CLIENT_APP } }),
    )?;
    *session = Some(security.value.clone());
    Ok(security.value)
}

fn fetch_countries(token: &str) -> Result<Vec<Country>, String> {
    let text = agent()
        .get(COUNTRIES)
        .header("Accept", "application/json")
        .header("X-Client-App", CLIENT_APP)
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let response: CountriesResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(response.countries.elements)
}

/// Repite `f` con una sesión nueva si falla con la guardada (venció).
fn with_session<T>(f: impl Fn(&str) -> Result<T, String>) -> Result<T, String> {
    match f(&security_token(false)?) {
        Ok(value) => Ok(value),
        Err(_) => f(&security_token(true)?),
    }
}

/// Países con servidores gratuitos (códigos ISO de dos letras), en orden.
pub fn countries() -> Result<Vec<String>, String> {
    if let Some((at, list)) = LIST.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(3600) {
            return Ok(list.clone());
        }
    }
    let mut list: Vec<String> = with_session(fetch_countries)?
        .into_iter()
        .filter(|c| c.access == "ACCESSIBLE")
        .map(|c| c.code.iso2.to_uppercase())
        .collect();
    list.sort();
    list.dedup();
    *LIST.lock().unwrap() = Some((Instant::now(), list.clone()));
    Ok(list)
}

#[tauri::command]
pub async fn vpn_countries() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(countries)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            eprintln!("[pulpo] vpn: no se pudo leer la lista de países: {e}");
            "error.vpn".to_string()
        })
}

/// El proxy de Urban que usa ahora un país, con su contraseña.
#[derive(Clone)]
struct Upstream {
    host: String,
    port: u16,
    /// Valor de `Proxy-Authorization`.
    auth: String,
    expires: Instant,
}

/// Un país en uso: su proxy de Urban, que se renueva al vencer.
struct Relay {
    code: String,
    upstream: Mutex<Option<Upstream>>,
    /// Cuántas veces falló: elige el servidor siguiente de la lista.
    failures: AtomicUsize,
}

impl Relay {
    /// El proxy actual, o uno nuevo si no hay, vence pronto o `fresh`.
    fn upstream(&self, fresh: bool) -> Result<Upstream, String> {
        let mut upstream = self.upstream.lock().unwrap();
        if let Some(current) = upstream.as_ref() {
            if !fresh && current.expires > Instant::now() + MARGIN {
                return Ok(current.clone());
            }
        }
        let next = with_session(|token| self.connect(token))?;
        *upstream = Some(next.clone());
        Ok(next)
    }

    fn connect(&self, token: &str) -> Result<Upstream, String> {
        let servers = fetch_countries(token)?
            .into_iter()
            .find(|c| c.code.iso2.eq_ignore_ascii_case(&self.code))
            .map(Country::servers)
            .unwrap_or_default();
        if servers.is_empty() {
            return Err(format!("sin servidores gratuitos en {}", self.code));
        }
        let server = &servers[self.failures.load(Ordering::Relaxed) % servers.len()];
        let proxy = post(
            &format!("{API}/security/tokens/accs-proxy"),
            Some(token),
            serde_json::json!({
                "type": "accs-proxy",
                "clientApp": { "name": CLIENT_APP },
                "signature": server.signature,
            }),
        )?;
        // La duración, no la hora de vencimiento: el reloj local puede ir distinto.
        let lifetime = match (proxy.creation, proxy.expiration) {
            (Some(start), Some(end)) if end > start => Duration::from_millis((end - start) as u64),
            _ => Duration::from_secs(600),
        };
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:1", proxy.value));
        Ok(Upstream {
            host: server.address.primary.host.clone(),
            port: server.address.primary.port,
            auth: format!("Basic {credentials}"),
            expires: Instant::now() + lifetime,
        })
    }

    fn failed(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.upstream.lock().unwrap() = None;
    }
}

/// Dirección del proxy local de `country`; lo arranca la primera vez. No
/// espera a Urban: los tokens se piden en segundo plano y, si fallan, las
/// conexiones de la app fallan (nunca salen directo).
pub fn proxy_url(country: &str) -> io::Result<Url> {
    let code = country.to_uppercase();
    let mut relays = RELAYS.lock().unwrap();
    let relays = relays.get_or_insert_with(HashMap::new);
    let port = match relays.get(&code) {
        Some(port) => *port,
        None => {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let port = listener.local_addr()?.port();
            let relay = Arc::new(Relay {
                code: code.clone(),
                upstream: Mutex::new(None),
                failures: AtomicUsize::new(0),
            });
            {
                let relay = relay.clone();
                std::thread::spawn(move || {
                    if let Err(e) = relay.upstream(false) {
                        eprintln!("[pulpo] vpn {}: {e}", relay.code);
                    }
                });
            }
            std::thread::spawn(move || {
                for client in listener.incoming().flatten() {
                    let relay = relay.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = serve(client, &relay) {
                            eprintln!("[pulpo] vpn {}: {e}", relay.code);
                        }
                    });
                }
            });
            relays.insert(code, port);
            port
        }
    };
    Ok(format!("http://127.0.0.1:{port}")
        .parse()
        .expect("url del proxy local"))
}

/// Lee la cabecera de una petición o respuesta HTTP, línea a línea.
fn read_head(reader: &mut impl BufRead) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut size = 0;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        size += read;
        if read == 0 || size > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cabecera incompleta",
            ));
        }
        if line == "\r\n" || line == "\n" {
            return Ok(lines);
        }
        lines.push(line);
    }
}

fn header_is(line: &str, names: &[&str]) -> bool {
    line.split_once(':')
        .is_some_and(|(name, _)| names.iter().any(|n| name.trim().eq_ignore_ascii_case(n)))
}

/// Atiende una conexión del motor web: `CONNECT` (HTTPS, casi todo) o una
/// petición HTTP normal.
fn serve(client: TcpStream, relay: &Relay) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut from_client = BufReader::new(client.try_clone()?);
    let head = match read_head(&mut from_client) {
        Ok(head) if !head.is_empty() => head,
        // El navegador cerró una conexión que tenía abierta de reserva.
        _ => return Ok(()),
    };
    let tunnel = head[0].starts_with("CONNECT ");
    let mut to_client = client.try_clone()?;

    // Si el proxy de Urban rechaza la contraseña (venció antes de lo esperado)
    // o no responde, se reintenta con uno nuevo.
    for attempt in 0..2 {
        let upstream = match relay.upstream(attempt > 0) {
            Ok(upstream) => upstream,
            Err(e) => {
                let _ =
                    to_client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
                return Err(io::Error::other(e));
            }
        };
        let server = match connect(&upstream) {
            Ok(server) => server,
            Err(e) => {
                eprintln!(
                    "[pulpo] vpn {}: {}:{}: {e}",
                    relay.code, upstream.host, upstream.port
                );
                relay.failed();
                continue;
            }
        };
        let mut request = head[0].clone();
        for line in &head[1..] {
            // Una conexión HTTP normal lleva una sola petición: las siguientes de
            // un keep-alive pasarían directo al proxy de Urban sin la contraseña.
            let drop: &[&str] = if tunnel {
                &["proxy-authorization"]
            } else {
                &["proxy-authorization", "proxy-connection", "connection"]
            };
            if !header_is(line, drop) {
                request.push_str(line);
            }
        }
        request.push_str(&format!("Proxy-Authorization: {}\r\n", upstream.auth));
        if !tunnel {
            request.push_str("Connection: close\r\n");
        }
        request.push_str("\r\n");
        let mut to_server = server.try_clone()?;
        to_server.write_all(request.as_bytes())?;

        let mut from_server = BufReader::new(server.try_clone()?);
        if tunnel {
            // La respuesta al CONNECT se mira antes de abrir el túnel.
            server.set_read_timeout(Some(Duration::from_secs(20)))?;
            let response = match read_head(&mut from_server) {
                Ok(response) if !response.is_empty() => response,
                _ => {
                    relay.failed();
                    continue;
                }
            };
            let status = response[0].split_whitespace().nth(1).unwrap_or("");
            if attempt == 0 && matches!(status, "407" | "403") {
                *relay.upstream.lock().unwrap() = None;
                continue;
            }
            let mut reply: String = response.concat();
            reply.push_str("\r\n");
            to_client.write_all(reply.as_bytes())?;
            server.set_read_timeout(None)?;
        }
        client.set_read_timeout(None)?;
        // Lo que el navegador ya mandó detrás de la cabecera.
        let pending = from_client.buffer().to_vec();
        to_server.write_all(&pending)?;
        let pending = from_server.buffer().to_vec();
        to_client.write_all(&pending)?;
        pipe(
            from_client.into_inner(),
            to_server,
            from_server.into_inner(),
            to_client,
        );
        return Ok(());
    }
    let _ = to_client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
    Err(io::Error::other("el proxy de Urban no responde"))
}

fn connect(upstream: &Upstream) -> io::Result<TcpStream> {
    let address = (upstream.host.as_str(), upstream.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("dirección vacía"))?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Copia en los dos sentidos hasta que uno cierra.
fn pipe(
    mut from_client: TcpStream,
    mut to_server: TcpStream,
    mut from_server: TcpStream,
    mut to_client: TcpStream,
) {
    let upload = std::thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_server);
        let _ = to_server.shutdown(Shutdown::Write);
    });
    let _ = copy(&mut from_server, &mut to_client);
    let _ = to_client.shutdown(Shutdown::Write);
    let _ = upload.join();
}

fn copy(from: &mut TcpStream, to: &mut TcpStream) -> io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = from.read(&mut buf)?;
        if read == 0 {
            return Ok(());
        }
        to.write_all(&buf[..read])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Usa la red y los servidores de Urban: `cargo test -p pulpo vpn -- --ignored`.
    #[test]
    #[ignore]
    fn sale_por_el_pais_elegido() {
        let list = countries().unwrap();
        assert!(list.contains(&"US".to_string()), "{list:?}");
        let proxy = proxy_url("us").unwrap();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .proxy(Some(ureq::Proxy::new(proxy.as_str()).unwrap()))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        for url in ["https://api.country.is/", "http://api.country.is/"] {
            let text = agent
                .get(url)
                .call()
                .unwrap()
                .body_mut()
                .read_to_string()
                .unwrap();
            assert!(text.contains(r#""country":"US""#), "{url}: {text}");
        }
    }
}
