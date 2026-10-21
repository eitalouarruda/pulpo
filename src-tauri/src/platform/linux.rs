//! Linux: Chromium (CEF). El bloqueo de red usa el dominio `Fetch` del
//! DevTools Protocol: cada petición se pausa, se consulta el motor de Brave y
//! se continúa o se cancela con `BlockedByClient`.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blocker::Blocker;
use tauri::Webview;

use crate::services::Theme;
use tauri_runtime_cef::cef;
use tauri_runtime_cef::{allocate_devtools_message_id, DevToolsProtocol, WebviewCefExt};

/// El `Browser` de CEF solo se toca en el hilo de UI de CEF, que es donde
/// corren tanto `with_cef_webview` como los observadores del DevTools Protocol.
pub(super) struct UiThread(pub(super) cef::Browser);
unsafe impl Send for UiThread {}

/// Lo que hace falta de un `Fetch.requestPaused`.
struct RequestPaused {
    request_id: String,
    url: String,
    resource_type: String,
}

impl RequestPaused {
    /// Lee el mensaje crudo sin parsearlo entero: en los POST viene el cuerpo
    /// (`postData`), que puede traer escapes UTF-16 sueltos que Chromium emite
    /// pero que no son JSON válido. Si el mensaje no se leyera, la petición
    /// quedaría pausada para siempre y la página colgada.
    ///
    /// Dentro de un valor de texto las comillas van escapadas (`\"`), así que
    /// `"clave":"` solo aparece como clave real del mensaje.
    fn from_raw(raw: &[u8]) -> Option<Self> {
        let raw = std::str::from_utf8(raw).ok()?;
        if !raw.starts_with(r#"{"method":"Fetch.requestPaused""#) {
            return None;
        }
        Some(Self {
            request_id: string_field(raw, "requestId")?,
            // Sin URL legible se deja pasar: nunca se deja una petición colgada.
            url: string_field(raw, "url").unwrap_or_default(),
            resource_type: string_field(raw, "resourceType").unwrap_or_default(),
        })
    }
}

/// Primer valor de texto de `key` en `raw`.
fn string_field(raw: &str, key: &str) -> Option<String> {
    let start = raw.find(&format!(r#""{key}":""#))? + key.len() + 3;
    let bytes = raw.as_bytes();
    let mut end = start + 1;
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'"' => return serde_json::from_str(&raw[start..=end]).ok(),
            _ => end += 1,
        }
    }
    None
}

/// Engancha el bloqueo de red y carga `url`. Bloquea el hilo actual hasta que
/// termina la carga inicial en blanco y `Fetch.enable` responde, así que no se
/// llama desde el hilo principal.
///
/// `initial_load` recibe un aviso cuando termina esa carga inicial: si se
/// navega antes, la carga en blanco que el runtime hace al crear el webview
/// pisa la navegación.
pub fn attach(
    webview: &Webview,
    initial_load: mpsc::Receiver<()>,
    blocker: Option<Arc<Blocker>>,
    url: &str,
    theme: Theme,
    vpn: bool,
) -> tauri::Result<()> {
    if initial_load.recv_timeout(Duration::from_secs(10)).is_err() {
        eprintln!("[pulpo] {url}: la carga inicial no terminó; se navega igual");
    }
    if vpn {
        // Antes de navegar: si no se puede, la app no se carga (la IP real se vería).
        proxy_only_webrtc(webview)?;
    }
    if let Err(e) = set_theme(webview, theme) {
        eprintln!("[pulpo] {url}: no se pudo fijar el tema: {e}");
    }
    if let Err(e) = super::enable_drag(webview) {
        eprintln!("[pulpo] {url}: arrastrar y soltar no disponible: {e}");
    }
    if let Some(blocker) = blocker.filter(|_| !crate::diag::flag("PULPO_NO_NETFILTER")) {
        if let Err(e) = enable_blocking(webview, blocker, url.to_string()) {
            eprintln!("[pulpo] bloqueo de red desactivado en {url}: {e}");
        }
    }
    webview.navigate(url.parse().expect("url del servicio"))
}

/// WebRTC solo por el proxy (ver `vpn::WEBRTC_POLICY`). Es una preferencia del
/// perfil, que es solo de esta app.
fn proxy_only_webrtc(webview: &Webview) -> tauri::Result<()> {
    use cef::{ImplBrowser, ImplBrowserHost, ImplPreferenceManager, ImplValue};
    let (tx, rx) = mpsc::channel();
    webview.with_cef_webview(move |handle| {
        let applied = (|| {
            let context = handle.browser().host()?.request_context()?;
            let mut value = cef::value_create()?;
            value.set_string(Some(&crate::vpn::WEBRTC_POLICY.into()));
            let mut error = cef::CefString::from("");
            let ok = context.set_preference(
                Some(&"webrtc.ip_handling_policy".into()),
                Some(&mut value),
                Some(&mut error),
            ) == 1;
            if !ok {
                eprintln!("[pulpo] vpn: no se pudo limitar WebRTC: {error}");
            }
            Some(ok)
        })();
        let _ = tx.send(applied == Some(true));
    })?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(true) => Ok(()),
        _ => Err(std::io::Error::other("WebRTC sin limitar").into()),
    }
}

/// El tema que la página ve en `prefers-color-scheme`. Con `Dark` también el
/// oscurecido automático de Chromium, para las páginas sin tema oscuro propio
/// (las que ya son oscuras no cambian). Se fija al crear el webview, antes de
/// la primera navegación, y se mantiene al navegar.
fn set_theme(webview: &Webview, theme: Theme) -> Result<(), String> {
    let value = match theme {
        // El del navegador: nada que emular.
        Theme::Auto => return Ok(()),
        Theme::Light => "light",
        Theme::Dark => "dark",
    };
    let send = |method: &str, params: serde_json::Value| {
        let message = serde_json::json!({
            "id": allocate_devtools_message_id().map_err(|e| e.to_string())?,
            "method": method,
            "params": params,
        });
        webview
            .send_dev_tools_message(message.to_string().as_bytes())
            .map_err(|e| e.to_string())
    };
    send(
        "Emulation.setEmulatedMedia",
        serde_json::json!({ "features": [{ "name": "prefers-color-scheme", "value": value }] }),
    )?;
    if theme == Theme::Dark {
        send(
            "Emulation.setAutoDarkModeOverride",
            serde_json::json!({ "enabled": true }),
        )?;
    }
    Ok(())
}

fn enable_blocking(
    webview: &Webview,
    blocker: Arc<Blocker>,
    page_url: String,
) -> Result<(), String> {
    let browser: Arc<Mutex<Option<UiThread>>> = Arc::default();
    {
        let browser = browser.clone();
        webview
            .with_cef_webview(move |handle| {
                *browser.lock().unwrap() = Some(UiThread(handle.browser()))
            })
            .map_err(|e| e.to_string())?;
    }
    let log_blocked = crate::diag::flag("PULPO_LOG_BLOCKED");
    let enable_id = allocate_devtools_message_id().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    webview
        .on_dev_tools_protocol(move |message| match message {
            // El runtime no siempre clasifica `Fetch.requestPaused` como
            // `Event`, pero el mensaje crudo llega siempre: se lee ese.
            DevToolsProtocol::Message(raw) => {
                let Some(paused) = RequestPaused::from_raw(&raw) else {
                    return;
                };
                let block = blocker.should_block(
                    &paused.url,
                    &page_url,
                    request_type(&paused.resource_type),
                );
                if block && log_blocked {
                    eprintln!("[blocked] {}", paused.url);
                }
                let reply = if block {
                    serde_json::json!({
                        "method": "Fetch.failRequest",
                        "params": { "requestId": paused.request_id, "errorReason": "BlockedByClient" },
                    })
                } else {
                    serde_json::json!({
                        "method": "Fetch.continueRequest",
                        "params": { "requestId": paused.request_id },
                    })
                };
                send(&browser, reply);
            }
            DevToolsProtocol::MethodResult { message_id, success, .. } if message_id == enable_id => {
                let _ = tx.lock().unwrap().send(success);
            }
            _ => {}
        })
        .map_err(|e| e.to_string())?;

    let enable = serde_json::json!({
        "id": enable_id,
        "method": "Fetch.enable",
        "params": { "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] },
    });
    webview
        .send_dev_tools_message(enable.to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Fetch.enable falló".into()),
        Err(_) => Err("Fetch.enable no respondió".into()),
    }
}

/// Envía un mensaje directamente al `BrowserHost`, sin pasar por el bucle de
/// eventos de Tauri. Se encola como tarea en el hilo de UI: mandarlo dentro del
/// propio observador re-entra en el runtime y lo bloquea. Sin `id` en el
/// mensaje se le asigna uno (cuando no hace falta reconocer la respuesta).
pub(super) fn send(browser: &Mutex<Option<UiThread>>, mut message: serde_json::Value) {
    if message.get("id").is_none() {
        let Ok(id) = allocate_devtools_message_id() else {
            return;
        };
        message["id"] = id.into();
    }
    let Some(browser) = browser.lock().unwrap().as_ref().map(|b| b.0.clone()) else {
        return;
    };
    task::post_send(browser, message.to_string());
}

mod task {
    use tauri_runtime_cef::cef::*;

    wrap_task! {
        struct SendDevToolsTask {
            browser: Browser,
            message: String,
        }

        impl Task {
            fn execute(&self) {
                if let Some(host) = self.browser.host() {
                    host.send_dev_tools_message(Some(self.message.as_bytes()));
                }
            }
        }
    }

    pub fn post_send(browser: Browser, message: String) {
        let mut task = SendDevToolsTask::new(browser, message);
        post_task(sys::cef_thread_id_t::TID_UI.into(), Some(&mut task));
    }
}

/// Escribe ya en disco las cookies de cada webview y espera (como mucho
/// `timeout`) a que termine. Chromium las guarda por lotes cada ~30 s, y al
/// salir para actualizar, que suele pasar a los pocos segundos de arrancar, se
/// perdían las que la página acababa de renovar al cargar (YouTube y Trello
/// rotan las de sesión): la versión nueva mandaba las viejas y el sitio cerraba
/// la sesión. No se llama desde el hilo principal.
pub fn flush_cookies(webviews: &[Webview], timeout: Duration) {
    let (tx, rx) = mpsc::channel();
    let mut pending = 0;
    for webview in webviews {
        let tx = tx.clone();
        let posted = webview.with_cef_webview(move |handle| flush::start(handle.browser(), tx));
        if posted.is_ok() {
            pending += 1;
        }
    }
    let deadline = Instant::now() + timeout;
    while pending > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if rx.recv_timeout(left).is_err() {
            eprintln!("[pulpo] cookies: {pending} perfiles sin confirmar el guardado");
            return;
        }
        pending -= 1;
    }
}

mod flush {
    use std::sync::mpsc::Sender;
    use tauri_runtime_cef::cef::*;

    /// Pide el guardado; `tx` recibe el aviso al terminar (o ya, si no se pudo).
    pub fn start(browser: Browser, tx: Sender<()>) {
        let manager = browser
            .host()
            .and_then(|host| host.request_context())
            .and_then(|context| context.cookie_manager(None));
        let flushing = manager.is_some_and(|manager| {
            let mut done = Done::new(tx.clone());
            manager.flush_store(Some(&mut done)) != 0
        });
        if !flushing {
            let _ = tx.send(());
        }
    }

    wrap_completion_callback! {
        pub struct Done {
            tx: Sender<()>,
        }

        impl CompletionCallback {
            fn on_complete(&self) {
                let _ = self.tx.send(());
            }
        }
    }
}

/// `Network.ResourceType` del DevTools Protocol → tipo de petición de adblock.
fn request_type(resource_type: &str) -> &'static str {
    match resource_type {
        "Document" => "sub_frame",
        "Stylesheet" => "stylesheet",
        "Image" => "image",
        "Media" => "media",
        "Font" => "font",
        "Script" => "script",
        "XHR" | "Fetch" | "EventSource" => "xmlhttprequest",
        "WebSocket" => "websocket",
        "Ping" | "CSPViolationReport" => "ping",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_paused_request_with_invalid_post_data() {
        // `postData` con un escape UTF-16 suelto: JSON inválido para serde.
        let raw = r#"{"method":"Fetch.requestPaused","params":{"requestId":"interception-job-7.0","request":{"url":"https://music.youtube.com/youtubei/v1/browse?a=\"b\"","method":"POST","postData":"{\"q\":\"\ud83d\",\"url\":\"x\"}"},"frameId":"F","resourceType":"XHR"}}"#;
        assert!(serde_json::from_str::<serde_json::Value>(raw).is_err());
        let paused = RequestPaused::from_raw(raw.as_bytes()).unwrap();
        assert_eq!(paused.request_id, "interception-job-7.0");
        assert_eq!(
            paused.url,
            r#"https://music.youtube.com/youtubei/v1/browse?a="b""#
        );
        assert_eq!(paused.resource_type, "XHR");
    }

    #[test]
    fn ignores_other_messages() {
        assert!(
            RequestPaused::from_raw(br#"{"method":"Page.loadEventFired","params":{}}"#).is_none()
        );
        assert!(RequestPaused::from_raw(br#"{"id":3,"result":{}}"#).is_none());
    }
}
