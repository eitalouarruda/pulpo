//! Atajos de teclado que funcionan con el foco dentro de una app:
//!
//! - Esc tres veces seguidas: página inicial.
//! - Ctrl dos veces (solo, sin otra tecla): la app anterior.
//! - Ctrl+1…9: la app en esa posición de la barra lateral.
//! - Ctrl+Espacio: selector de apps (se escribe en la barra lateral).
//!
//! Además avisa del clic en una notificación de la app, para traer Pulpo al
//! frente con esa app abierta.
//!
//! Las apps son webs remotas sin acceso al IPC de Tauri, así que el script de
//! inicio avisa cambiando un instante el título del documento a una marca con
//! un código aleatorio de esta ejecución más la acción; el cambio de título sí
//! llega a Rust (en Linux y en Windows) y el título original se restaura
//! enseguida.

use std::sync::OnceLock;

use serde::Serialize;

/// Esc: pulsaciones necesarias y ventana de tiempo en la que tienen que caer.
const ESC_PRESSES: usize = 3;
const ESC_WINDOW_MS: u32 = 1000;
/// Ctrl Ctrl: tiempo máximo entre los dos toques.
const CTRL_WINDOW_MS: u32 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Shortcut {
    Home,
    Previous,
    /// Posición en la barra lateral, desde 1.
    Index {
        n: u8,
    },
    /// Ctrl+Espacio.
    Switcher,
    /// Clic en una notificación que mostró la página.
    Notification,
}

fn marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("\u{2063}pulpo:{:x}:{:x}:", nanos, std::process::id())
    })
}

/// El atajo que anuncia este título, si es una marca de Pulpo.
pub fn parse(title: &str) -> Option<Shortcut> {
    match title.strip_prefix(marker())? {
        "home" => Some(Shortcut::Home),
        "previous" => Some(Shortcut::Previous),
        "switcher" => Some(Shortcut::Switcher),
        "notification" => Some(Shortcut::Notification),
        n => n
            .parse()
            .ok()
            .filter(|n| (1..=9).contains(n))
            .map(|n| Shortcut::Index { n }),
    }
}

pub fn script() -> String {
    let marker = serde_json::to_string(marker()).expect("marca serializable");
    format!(
        r#"(() => {{
  if (window.top !== window || window.__pulpoShortcuts) return;
  window.__pulpoShortcuts = true;
  const MARK = {marker};
  // Dos avisos seguidos (p. ej. un clic en una notificación y un atajo) no
  // pueden dejar la marca como título: se guarda el de la página una vez.
  let saved = null;
  const signal = (action) => {{
    if (saved === null) saved = document.title;
    document.title = MARK + action;
    setTimeout(() => {{
      if (document.title === MARK + action) document.title = saved;
      if (!document.title.startsWith(MARK)) saved = null;
    }}, 50);
  }};
  let escapes = [];
  let ctrlAlone = false;
  let lastCtrlTap = -Infinity;
  window.addEventListener("keydown", (e) => {{
    if (e.key === "Control") {{
      if (!e.repeat) ctrlAlone = true;
      return;
    }}
    ctrlAlone = false;
    if (e.key === "Escape" && !e.repeat) {{
      const now = performance.now();
      escapes = escapes.filter((t) => now - t < {ESC_WINDOW_MS});
      escapes.push(now);
      if (escapes.length >= {ESC_PRESSES}) {{
        escapes = [];
        signal("home");
      }}
      return;
    }}
    if (e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && /^Digit[1-9]$/.test(e.code)) {{
      e.preventDefault();
      e.stopImmediatePropagation();
      if (!e.repeat) signal(e.code.slice(5));
      return;
    }}
    if (e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && e.code === "Space") {{
      e.preventDefault();
      e.stopImmediatePropagation();
      if (!e.repeat) signal("switcher");
    }}
  }}, true);
  window.addEventListener("keyup", (e) => {{
    if (e.key !== "Control" || !ctrlAlone) return;
    ctrlAlone = false;
    const now = performance.now();
    if (now - lastCtrlTap < {CTRL_WINDOW_MS}) {{
      lastCtrlTap = -Infinity;
      signal("previous");
    }} else {{
      lastCtrlTap = now;
    }}
  }}, true);
  // Al perder el foco (p. ej. Alt+Tab) no cuenta como toque.
  window.addEventListener("blur", () => {{ ctrlAlone = false; }});
  // Notificaciones: Chromium entrega el clic a la página (al objeto
  // `Notification`), no a Pulpo. Se escucha en cada una que cree la página,
  // además de lo que haga ella (ir al chat, etc.).
  const Native = window.Notification;
  if (typeof Native === "function") {{
    window.Notification = class Notification extends Native {{
      constructor(title, options) {{
        super(title, options);
        this.addEventListener("click", () => signal("notification"));
      }}
    }};
  }}
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_signals() {
        assert!(script().contains(&serde_json::to_string(marker()).unwrap()));
        let m = marker();
        assert_eq!(parse(&format!("{m}home")), Some(Shortcut::Home));
        assert_eq!(parse(&format!("{m}previous")), Some(Shortcut::Previous));
        assert_eq!(parse(&format!("{m}switcher")), Some(Shortcut::Switcher));
        assert_eq!(
            parse(&format!("{m}notification")),
            Some(Shortcut::Notification)
        );
        assert_eq!(parse(&format!("{m}3")), Some(Shortcut::Index { n: 3 }));
        assert_eq!(parse(&format!("{m}0")), None);
        assert_eq!(parse(&format!("{m}10")), None);
        assert_eq!(parse("(3) WhatsApp"), None);
        assert_eq!(parse("home"), None);
    }
}
