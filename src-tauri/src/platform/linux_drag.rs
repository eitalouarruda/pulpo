//! Linux: arrastrar y soltar dentro de una página.
//!
//! CEF, hospedado como ventana hija, no consigue arrancar el arrastre nativo
//! de X11: la página recibe `dragstart` y enseguida `dragend`, sin `dragover`
//! ni `drop` (mover tarjetas en Jira, reordenar la barra lateral...). Se le pide
//! a Chromium que entregue el arrastre por el DevTools Protocol
//! (`Input.setInterceptDrags`) y Pulpo lo conduce: sigue el puntero con
//! `XQueryPointer` y manda `dragOver` hasta que se suelta el botón, y entonces
//! `drop` (o `dragCancel` si se suelta fuera de la página).
//!
//! Cubre los arrastres que empiezan y terminan en la misma página.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::Webview;
use tauri_runtime_cef::cef::{ImplBrowser, ImplBrowserHost};
use tauri_runtime_cef::{allocate_devtools_message_id, DevToolsProtocol, WebviewCefExt};
use x11_dl::xlib;

use super::linux::{send, UiThread};

/// Cada cuánto se mira el puntero (unos 60 Hz).
const POLL: Duration = Duration::from_millis(16);
/// Un arrastre que no termina (se perdió el botón) se cancela.
const MAX_DRAG: Duration = Duration::from_secs(120);

type Browser = Arc<Mutex<Option<UiThread>>>;

/// Arrastre interceptado que espera la escala de la página (`devicePixelRatio`)
/// para pasar los píxeles de X11 a píxeles CSS.
struct Pending {
    ratio_id: i32,
    xid: xlib::Window,
    data: Value,
}

/// Activa la intercepción de arrastres en `webview`. Bloquea hasta que el
/// runtime registra el observador, así que no se llama desde el hilo principal.
pub fn enable_drag(webview: &Webview) -> Result<(), String> {
    let browser: Browser = Arc::default();
    {
        let browser = browser.clone();
        webview
            .with_cef_webview(move |handle| {
                *browser.lock().unwrap() = Some(UiThread(handle.browser()))
            })
            .map_err(|e| e.to_string())?;
    }
    let pending: Mutex<Option<Pending>> = Mutex::default();
    let observer_browser = browser.clone();
    // El observador corre en el hilo de UI de CEF: ahí se puede tocar el `Browser`.
    webview
        .on_dev_tools_protocol(move |message| match message {
            DevToolsProtocol::Message(raw) => {
                let Some(data) = intercepted(&raw) else {
                    return;
                };
                let xid = observer_browser
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|b| b.0.host())
                    .map(|host| host.window_handle() as xlib::Window)
                    .filter(|xid| *xid != 0);
                let (Some(xid), Ok(ratio_id)) = (xid, allocate_devtools_message_id()) else {
                    return dispatch(&observer_browser, "dragCancel", (0.0, 0.0), &data);
                };
                *pending.lock().unwrap() = Some(Pending {
                    ratio_id,
                    xid,
                    data,
                });
                send(
                    &observer_browser,
                    json!({
                        "id": ratio_id,
                        "method": "Runtime.evaluate",
                        "params": { "expression": "devicePixelRatio", "returnByValue": true },
                    }),
                );
            }
            DevToolsProtocol::MethodResult {
                message_id, result, ..
            } => {
                let mut slot = pending.lock().unwrap();
                if slot.as_ref().is_none_or(|p| p.ratio_id != message_id) {
                    return;
                }
                let Pending { xid, data, .. } = slot.take().unwrap();
                let ratio = serde_json::from_slice::<Value>(&result)
                    .ok()
                    .and_then(|r| r["result"]["value"].as_f64())
                    .filter(|r| r.is_finite() && *r > 0.0)
                    .unwrap_or(1.0);
                let browser = observer_browser.clone();
                std::thread::spawn(move || follow(&browser, xid, ratio, &data));
            }
            DevToolsProtocol::Event { .. } => {}
        })
        .map_err(|e| e.to_string())?;
    send(
        &browser,
        json!({ "method": "Input.setInterceptDrags", "params": { "enabled": true } }),
    );
    Ok(())
}

/// Los datos del arrastre, si `raw` es un `Input.dragIntercepted`.
fn intercepted(raw: &[u8]) -> Option<Value> {
    if !raw.starts_with(br#"{"method":"Input.dragIntercepted""#) {
        return None;
    }
    let mut message: Value = serde_json::from_slice(raw).ok()?;
    Some(message["params"]["data"].take()).filter(|d| d.is_object())
}

fn dispatch(browser: &Browser, kind: &str, (x, y): (f64, f64), data: &Value) {
    send(
        browser,
        json!({
            "method": "Input.dispatchDragEvent",
            "params": { "type": kind, "x": x, "y": y, "data": data },
        }),
    );
}

/// Sigue el puntero sobre la ventana `xid` hasta que se suelta el botón.
fn follow(browser: &Browser, xid: xlib::Window, ratio: f64, data: &Value) {
    let Some(pointer) = Pointer::open(xid) else {
        return dispatch(browser, "dragCancel", (0.0, 0.0), data);
    };
    let started = Instant::now();
    let mut last: Option<(f64, f64)> = None;
    loop {
        let Some(state) = pointer.query() else {
            return dispatch(browser, "dragCancel", last.unwrap_or_default(), data);
        };
        let position = (state.x as f64 / ratio, state.y as f64 / ratio);
        if !state.pressed {
            if !state.inside || last.is_none() {
                return dispatch(browser, "dragCancel", position, data);
            }
            if last != Some(position) {
                dispatch(browser, "dragOver", position, data);
            }
            return dispatch(browser, "drop", position, data);
        }
        if started.elapsed() > MAX_DRAG {
            return dispatch(browser, "dragCancel", position, data);
        }
        if state.inside && last != Some(position) {
            let kind = if last.is_none() {
                "dragEnter"
            } else {
                "dragOver"
            };
            dispatch(browser, kind, position, data);
            last = Some(position);
        }
        std::thread::sleep(POLL);
    }
}

/// Conexión propia con el servidor X para leer el puntero sobre una ventana.
struct Pointer {
    xlib: xlib::Xlib,
    display: *mut xlib::Display,
    xid: xlib::Window,
}

struct PointerState {
    /// Posición relativa a la ventana, en píxeles físicos.
    x: i32,
    y: i32,
    pressed: bool,
    inside: bool,
}

impl Pointer {
    fn open(xid: xlib::Window) -> Option<Self> {
        let xlib = xlib::Xlib::open().ok()?;
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        (!display.is_null()).then_some(Self { xlib, display, xid })
    }

    fn query(&self) -> Option<PointerState> {
        let (mut root, mut child) = (0, 0);
        let (mut root_x, mut root_y, mut x, mut y) = (0, 0, 0, 0);
        let mut mask = 0;
        let mut attributes = std::mem::MaybeUninit::<xlib::XWindowAttributes>::uninit();
        unsafe {
            // `False`: el puntero está en otra pantalla.
            if (self.xlib.XQueryPointer)(
                self.display,
                self.xid,
                &mut root,
                &mut child,
                &mut root_x,
                &mut root_y,
                &mut x,
                &mut y,
                &mut mask,
            ) == 0
            {
                return None;
            }
            if (self.xlib.XGetWindowAttributes)(self.display, self.xid, attributes.as_mut_ptr())
                == 0
            {
                return None;
            }
        }
        // XGetWindowAttributes rellena la estructura entera cuando responde.
        let attributes = unsafe { attributes.assume_init() };
        Some(PointerState {
            x,
            y,
            pressed: mask & xlib::Button1Mask != 0,
            inside: (0..attributes.width).contains(&x) && (0..attributes.height).contains(&y),
        })
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        unsafe { (self.xlib.XCloseDisplay)(self.display) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_intercepted_drag() {
        let raw = br#"{"method":"Input.dragIntercepted","params":{"data":{"items":[{"mimeType":"text/plain","data":"x"}],"dragOperationsMask":-1}}}"#;
        let data = intercepted(raw).unwrap();
        assert_eq!(data["items"][0]["mimeType"], "text/plain");
        assert!(intercepted(br#"{"method":"Fetch.requestPaused","params":{}}"#).is_none());
        assert!(intercepted(br#"{"id":4,"result":{}}"#).is_none());
    }
}
