//! No molestar por app: en sus horarios semanales, o mientras está silenciada
//! a mano, la página no muestra notificaciones ni reproduce sonidos que no
//! haya pedido el usuario (mensajes nuevos, llamadas entrantes). Los mensajes
//! siguen llegando y los contadores siguen saliendo en el título.
//!
//! Rust decide cuándo está activo (revisa los horarios cada poco) y se lo dice
//! a la página con `eval`. El script de inicio se queda en medio de:
//!
//! - `new Notification(…)` y `ServiceWorkerRegistration.showNotification`:
//!   devuelven una notificación que nunca se muestra, sin que la app note nada
//!   (el permiso sigue concedido, así que no vuelve a pedirlo).
//! - `HTMLMediaElement.play` y `AudioBufferSourceNode.start`: se rechazan si no
//!   vienen de un clic o una tecla reciente (`navigator.userActivation`). Las
//!   notas de voz o los vídeos que abre el usuario suenan igual; los vídeos sin
//!   sonido también.
//!
//! No llega a los avisos push que muestra el service worker con la página en
//! segundo plano.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{Datelike, Local, Timelike};
use tauri::{AppHandle, Emitter, Manager};

use crate::services::Service;
use crate::AppState;

/// Cada cuánto se revisan los horarios.
const TICK: Duration = Duration::from_secs(15);

pub const SCRIPT: &str = r#"(() => {
  if (window.top !== window || window.__pulpoDnd) return;
  let on = false;
  window.__pulpoDnd = (value) => { on = !!value; };
  const quiet = () => on && !navigator.userActivation?.isActive;

  const Native = window.Notification;
  if (typeof Native === "function") {
    class Silent extends EventTarget {
      constructor(title, options = {}) {
        super();
        Object.assign(this, {
          title: String(title), body: options.body ?? "", tag: options.tag ?? "",
          icon: options.icon ?? "", image: options.image ?? "", badge: options.badge ?? "",
          data: options.data ?? null, dir: options.dir ?? "auto", lang: options.lang ?? "",
          requireInteraction: !!options.requireInteraction, silent: true, renotify: false,
          timestamp: Date.now(), actions: [], vibrate: [],
          onclick: null, onshow: null, onclose: null, onerror: null,
        });
      }
      close() {}
    }
    window.Notification = new Proxy(Native, {
      construct(target, args, newTarget) {
        return on ? new Silent(...args) : Reflect.construct(target, args, newTarget);
      },
    });
  }

  const registration = window.ServiceWorkerRegistration?.prototype;
  if (registration?.showNotification) {
    const show = registration.showNotification;
    registration.showNotification = function (...args) {
      return on ? Promise.resolve() : show.apply(this, args);
    };
  }

  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (...args) {
    if (quiet() && !this.muted && this.volume > 0) {
      return Promise.reject(new DOMException("Do not disturb", "NotAllowedError"));
    }
    return play.apply(this, args);
  };

  const source = window.AudioBufferSourceNode?.prototype;
  if (source?.start) {
    const start = source.start;
    source.start = function (...args) {
      if (!quiet()) return start.apply(this, args);
    };
  }
})();"#;

/// Las apps con no molestar activo ahora.
fn active_ids(services: &[Service]) -> BTreeSet<String> {
    let now = Local::now();
    let weekday = now.weekday().num_days_from_monday() as u8;
    let minute = (now.hour() * 60 + now.minute()) as u16;
    services
        .iter()
        .filter(|s| s.dnd.active(weekday, minute, now.timestamp()))
        .map(|s| s.id.clone())
        .collect()
}

pub fn is_active(app: &AppHandle, id: &str) -> bool {
    app.state::<AppState>().dnd.lock().unwrap().contains(id)
}

/// Le dice a la página de la app si está en no molestar.
pub fn apply(app: &AppHandle, id: &str, on: bool) {
    if let Some(webview) = app.get_webview(id) {
        let _ = webview.eval(format!("window.__pulpoDnd && window.__pulpoDnd({on})"));
    }
}

/// Recalcula qué apps están en no molestar, avisa a las que cambiaron y a la
/// barra lateral.
pub fn refresh(app: &AppHandle) {
    let state = app.state::<AppState>();
    let now = active_ids(&state.services.lock().unwrap());
    let changed: Vec<(String, bool)> = {
        let mut current = state.dnd.lock().unwrap();
        if *current == now {
            return;
        }
        let changed = current
            .symmetric_difference(&now)
            .map(|id| (id.clone(), now.contains(id)))
            .collect();
        *current = now.clone();
        changed
    };
    for (id, on) in changed {
        apply(app, &id, on);
    }
    let _ = app.emit_to("shell", "dnd-changed", now);
}

/// Revisa los horarios en segundo plano mientras dure la app.
pub fn watch(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        refresh(&app);
        std::thread::sleep(TICK);
    });
}
