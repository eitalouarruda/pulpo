//! Archivos del explorador que el runtime CEF entrega a la ventana padre.
//! Se reenvían al servicio visible bajo el cursor como un arrastre de Chromium,
//! con archivos reales (sin leerlos en JS ni dar IPC a las páginas remotas).
//! WebView2 y los drops que llegan directamente a CEF usan su ruta nativa.

use std::path::PathBuf;
use std::sync::mpsc;

use serde_json::{json, Value};
use tauri::{AppHandle, DragDropEvent, Manager, PhysicalPosition, Webview, Window, WindowEvent};
use tauri_runtime_cef::{allocate_devtools_message_id, WebviewCefExt};

use crate::{placement, AppState, Placement};

/// Solo el trabajador consulta el runtime: el callback de ventana nunca espera
/// al hilo principal de Tauri/CEF.
pub fn install(window: &Window) {
    let (tx, rx) = mpsc::channel();
    let app = window.app_handle().clone();
    std::thread::spawn(move || {
        let mut drag = FileDrag::default();
        while let Ok(event) = rx.recv() {
            if let Err(e) = drag.handle(&app, event) {
                eprintln!("[pulpo] no se pudo entregar el archivo arrastrado: {e}");
                drag.cancel();
            }
        }
        drag.cancel();
    });
    window.on_window_event(move |event| {
        if let WindowEvent::DragDrop(event) = event {
            if crate::diag::flag("PULPO_LOG_DRAGS") {
                eprintln!("[pulpo] arrastre externo: {event:?}");
            }
            let _ = tx.send(event.clone());
        }
    });
}

#[derive(Default)]
struct FileDrag {
    paths: Vec<PathBuf>,
    target: Option<Webview>,
}

impl FileDrag {
    fn cancel(&mut self) {
        if let Some(webview) = self.target.take() {
            let _ = dispatch(&webview, "dragCancel", (0.0, 0.0), &self.paths);
        }
    }

    fn handle(&mut self, app: &AppHandle, event: DragDropEvent) -> Result<(), String> {
        let (position, drop) = match event {
            DragDropEvent::Enter { paths, position } => {
                self.cancel();
                self.paths = paths;
                (position, false)
            }
            DragDropEvent::Over { position } => (position, false),
            DragDropEvent::Drop { paths, position } => {
                self.paths = paths;
                (position, true)
            }
            DragDropEvent::Leave => {
                self.cancel();
                self.paths.clear();
                return Ok(());
            }
            _ => return Ok(()),
        };
        if self.paths.is_empty() {
            return Ok(());
        }
        let Some((webview, point)) = target_at(app, position) else {
            if crate::diag::flag("PULPO_LOG_DRAGS") {
                eprintln!("[pulpo] arrastre fuera de los servicios: {position:?}");
            }
            self.cancel();
            if drop {
                self.paths.clear();
            }
            return Ok(());
        };
        if self.target.as_ref().map(Webview::label) != Some(webview.label()) {
            self.cancel();
            self.target = Some(webview.clone());
            dispatch(&webview, "dragEnter", point, &self.paths)?;
        }
        dispatch(&webview, "dragOver", point, &self.paths)?;
        if drop {
            dispatch(&webview, "drop", point, &self.paths)?;
            self.target = None;
            self.paths.clear();
        }
        Ok(())
    }
}

fn target_at(app: &AppHandle, point: PhysicalPosition<f64>) -> Option<(Webview, (f64, f64))> {
    let ids: Vec<_> = app
        .state::<AppState>()
        .services
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.clone())
        .collect();
    let scale = app.get_window("main")?.scale_factor().ok()? * crate::current_zoom(app);
    for id in ids {
        if placement(app, &id) == Placement::Hidden {
            continue;
        }
        let Some(webview) = app.get_webview(&id) else {
            continue;
        };
        let Ok(origin) = webview.position() else {
            continue;
        };
        let Ok(size) = webview.size() else { continue };
        if let Some(point) = viewport_point(
            (point.x, point.y),
            (origin.x as f64, origin.y as f64),
            (size.width as f64, size.height as f64),
            scale,
        ) {
            return Some((webview, point));
        }
    }
    None
}

fn viewport_point(
    point: (f64, f64),
    origin: (f64, f64),
    size: (f64, f64),
    scale: f64,
) -> Option<(f64, f64)> {
    let (x, y) = (point.0 - origin.0, point.1 - origin.1);
    (scale.is_finite() && scale > 0.0 && x >= 0.0 && y >= 0.0 && x < size.0 && y < size.1)
        .then_some((x / scale, y / scale))
}

fn drag_data(paths: &[PathBuf]) -> Result<Value, String> {
    let files: Result<Vec<_>, _> = paths
        .iter()
        .map(|p| {
            p.to_str()
                .ok_or_else(|| format!("ruta no UTF-8: {}", p.display()))
        })
        .collect();
    Ok(json!({ "items": [], "files": files?, "dragOperationsMask": 1 }))
}

fn dispatch(
    webview: &Webview,
    kind: &str,
    (x, y): (f64, f64),
    paths: &[PathBuf],
) -> Result<(), String> {
    let message = json!({
        "id": allocate_devtools_message_id().map_err(|e| e.to_string())?,
        "method": "Input.dispatchDragEvent",
        "params": { "type": kind, "x": x, "y": y, "data": drag_data(paths)? },
    });
    webview
        .send_dev_tools_message(message.to_string().as_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_only_viewport_and_converts_hidpi_and_zoom() {
        let origin = (128.0, 200.0);
        let size = (1000.0, 600.0);
        assert_eq!(
            viewport_point((448.0, 520.0), origin, size, 3.2),
            Some((100.0, 100.0))
        );
        for point in [
            (127.0, 300.0),
            (300.0, 199.0),
            (1128.0, 300.0),
            (300.0, 800.0),
        ] {
            assert_eq!(viewport_point(point, origin, size, 1.0), None);
        }
        assert_eq!(viewport_point((200.0, 300.0), origin, size, 0.0), None);
    }

    #[test]
    fn preserves_multiple_files_and_unicode_without_reading_contents() {
        let paths = [
            PathBuf::from("/tmp/foto de férias.png"),
            PathBuf::from("/tmp/report.pdf"),
        ];
        let data = drag_data(&paths).unwrap();
        assert_eq!(
            data["files"],
            json!(["/tmp/foto de férias.png", "/tmp/report.pdf"])
        );
        assert_eq!(data["items"], json!([]));
        assert_eq!(data["dragOperationsMask"], 1);
    }
}
