//! Linux: Chromium (CEF). El bloqueo de red usa el dominio `Fetch` del
//! DevTools Protocol: cada petición se pausa, se consulta el motor de Brave y
//! se continúa o se cancela con `BlockedByClient`.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blocker::Blocker;
use tauri::Webview;
use tauri_runtime_cef::cef;
use tauri_runtime_cef::{allocate_devtools_message_id, DevToolsProtocol, WebviewCefExt};

/// El `Browser` de CEF solo se toca en el hilo de UI de CEF, que es donde
/// corren tanto `with_cef_webview` como los observadores del DevTools Protocol.
struct UiThread(cef::Browser);
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
) -> tauri::Result<()> {
    if initial_load.recv_timeout(Duration::from_secs(10)).is_err() {
        eprintln!("[pulpo] {url}: la carga inicial no terminó; se navega igual");
    }
    if let Some(blocker) = blocker.filter(|_| !crate::diag::flag("PULPO_NO_NETFILTER")) {
        if let Err(e) = enable_blocking(webview, blocker, url.to_string()) {
            eprintln!("[pulpo] bloqueo de red desactivado en {url}: {e}");
        }
    }
    webview.navigate(url.parse().expect("url del servicio"))
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
/// propio observador re-entra en el runtime y lo bloquea.
fn send(browser: &Mutex<Option<UiThread>>, mut message: serde_json::Value) {
    let Ok(id) = allocate_devtools_message_id() else {
        return;
    };
    message["id"] = id.into();
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
