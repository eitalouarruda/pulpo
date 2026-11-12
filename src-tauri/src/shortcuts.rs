//! Atajos de teclado que funcionan con el foco dentro de una app:
//!
//! - Esc tres veces seguidas: página inicial.
//! - Ctrl dos veces (solo, sin otra tecla): la app anterior.
//! - Ctrl+1…9: la app en esa posición de la barra lateral.
//! - Ctrl+Espacio: selector de apps (se escribe en la barra lateral).
//! - Ctrl+Mayús+L: rellenar usuario y contraseña con Bitwarden (`bitwarden`).
//!
//! Además avisa del clic en una notificación de la app, para traer Pulpo al
//! frente con esa app abierta, y del contador que la página pone con
//! `navigator.setAppBadge` (Telegram no lo pone en el título).
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
    /// Ctrl+Mayús+L.
    Fill,
    /// Clic en una notificación que mostró la página.
    Notification,
    /// Borde izquierdo dentro de la app: abrir/cerrar la barra lateral.
    SidebarShow,
    SidebarHide,
}

pub(crate) fn marker() -> &'static str {
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
        "fill" => Some(Shortcut::Fill),
        "notification" => Some(Shortcut::Notification),
        "s1" => Some(Shortcut::SidebarShow),
        "s0" => Some(Shortcut::SidebarHide),
        n => n
            .parse()
            .ok()
            .filter(|n| (1..=9).contains(n))
            .map(|n| Shortcut::Index { n }),
    }
}

/// El contador que anuncia este título con `navigator.setAppBadge`, si es una
/// marca de Pulpo.
pub fn parse_badge(title: &str) -> Option<u32> {
    title
        .strip_prefix(marker())?
        .strip_prefix("badge:")?
        .parse()
        .ok()
}

/// Si el título es una marca de Pulpo (de cualquier tipo): no es el de la página.
pub fn is_signal(title: &str) -> bool {
    title.starts_with(marker())
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
    if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey && e.code === "KeyL") {{
      e.preventDefault();
      e.stopImmediatePropagation();
      if (!e.repeat) signal("fill");
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
  // Economia em segundo plano: pausa animações CSS e vídeos mutados/loop quando oculto.
  const isMusicSite = /music\.youtube\.com|open\.spotify\.com/.test(location.origin);
  if (!isMusicSite) {{
    let bgStyle = null;
    const applyBgEconomy = () => {{
      if (document.hidden) {{
        if (!bgStyle) {{
          bgStyle = document.createElement("style");
          bgStyle.id = "_pulpo_bg_eco";
          bgStyle.textContent = "* {{ animation-play-state: paused !important; transition-duration: 0.001s !important; }}";
          (document.head || document.documentElement).appendChild(bgStyle);
        }}
        for (const v of document.querySelectorAll("video")) {{
          if (v.muted || v.loop) {{
            try {{ v.pause(); v.__pulpoPaused = true; }} catch {{}}
          }}
        }}
      }} else {{
        if (bgStyle) {{
          bgStyle.remove();
          bgStyle = null;
        }}
        for (const v of document.querySelectorAll("video")) {{
          if (v.__pulpoPaused) {{
            delete v.__pulpoPaused;
            try {{ v.play(); }} catch {{}}
          }}
        }}
      }}
    }};
    document.addEventListener("visibilitychange", applyBgEconomy, true);
  }}
  // Borde izquierdo dentro de la app: revela u oculta la barra lateral.
  let sbTimer = null;
  let sbSent = false;
  window.addEventListener("mousemove", (e) => {{
    if (e.clientX <= 3) {{
      if (!sbSent && !sbTimer) {{
        sbTimer = setTimeout(() => {{
          sbTimer = null;
          sbSent = true;
          signal("s1");
        }}, 90);
      }}
    }} else {{
      if (sbTimer) {{
        clearTimeout(sbTimer);
        sbTimer = null;
      }}
      if (e.clientX > 6 && sbSent) {{
        sbSent = false;
        signal("s0");
      }}
    }}
  }}, {{ passive: true }});
  // Interceptação de document.cookie para estender cookies de sessão para 30 dias (Max-Age=2592000).
  try {{
    const proto = Document.prototype;
    const cookieDesc = Object.getOwnPropertyDescriptor(proto, "cookie") ||
                       Object.getOwnPropertyDescriptor(HTMLDocument.prototype, "cookie");
    if (cookieDesc && cookieDesc.set) {{
      const origSet = cookieDesc.set;
      const origGet = cookieDesc.get;
      Object.defineProperty(document, "cookie", {{
        configurable: true,
        enumerable: true,
        get() {{
          return origGet.call(this);
        }},
        set(val) {{
          let str = String(val);
          if (!/(?:expires|max-age)\s*=/i.test(str)) {{
            str += "; Max-Age=2592000; SameSite=Lax";
          }}
          return origSet.call(this, str);
        }}
      }});
    }}
  }} catch (_) {{}}
  // Notificaciones: Chromium entrega el clic a la página (al objeto
  // `Notification`), no a Pulpo. Se escucha en cada una que cree la página,
  // además de lo que haga ella (ir al chat, etc.).
  const Native = window.Notification;
  if (typeof Native === "function") {{
    window.Notification = class Notification extends Native {{
      constructor(title, options) {{
        super(title, options);
        this.addEventListener("click", () => signal("notification"));
        if (!lastBadge) {{
          badge(1);
        }}
      }}
    }};
  }}
  // Contador de la app (Badging API). Sin `count` es un punto sin número: 1.
  let lastBadge = null;
  const badge = (count) => {{
    const n = count === undefined ? 1 : Math.max(0, Math.floor(Number(count) || 0));
    if (n !== lastBadge) {{
      lastBadge = n;
      signal("badge:" + n);
    }}
    return Promise.resolve();
  }};
  navigator.setAppBadge = badge;
  navigator.clearAppBadge = () => badge(0);
  // Tauri define `window.isTauri` en todas las páginas (antes que este script,
  // y no se puede quitar), así que Telegram Web A se cree su app de escritorio
  // y manda por un IPC que las apps no tienen el contador
  // (`window.tauri.setNotificationsCount`) y los enlaces (`window.open` y los
  // clics en `<a>` pasan por el plugin shell). Se queda con estas funciones en
  // vez de las suyas y, cuando escribe en `window.tauri` (solo en ese modo),
  // los enlaces vuelven al `window.open` nativo, que Pulpo enruta como siempre.
  if (!window.tauri) {{
    const nativeOpen = window.open;
    const open = (url) => nativeOpen.call(window, url, "_blank", "noopener");
    let desktopMode = false;
    const enterDesktopMode = () => {{
      if (desktopMode) return;
      desktopMode = true;
      // Su `window.open` se guarda pero no se usa.
      Object.defineProperty(window, "open", {{
        get: () => nativeOpen,
        set: () => {{}},
        configurable: true,
      }});
      // Como su manejador (en `window`, después de los de la página), pero
      // registrado antes: al marcar el evento, el suyo ya no hace nada.
      const onClick = (e) => {{
        if (e.defaultPrevented) return;
        const a = e.target instanceof Element ? e.target.closest("a[href]") : null;
        if (!a || !(a.target === "_blank" || e.ctrlKey || e.metaKey || e.button === 1)) return;
        e.preventDefault();
        open(a.href);
      }};
      window.addEventListener("click", onClick);
      window.addEventListener("auxclick", onClick);
    }};
    const tauri = {{}};
    const keep = (name, value) => Object.defineProperty(tauri, name, {{
      get: () => value,
      set: enterDesktopMode,
      enumerable: true,
    }});
    keep("setNotificationsCount", badge);
    keep("openNewWindow", async (url) => {{
      open(url);
      return true;
    }});
    window.tauri = tauri;
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
        assert_eq!(parse(&format!("{m}fill")), Some(Shortcut::Fill));
        assert_eq!(
            parse(&format!("{m}notification")),
            Some(Shortcut::Notification)
        );
        assert_eq!(parse(&format!("{m}s1")), Some(Shortcut::SidebarShow));
        assert_eq!(parse(&format!("{m}s0")), Some(Shortcut::SidebarHide));
        assert_eq!(parse(&format!("{m}3")), Some(Shortcut::Index { n: 3 }));
        assert_eq!(parse(&format!("{m}0")), None);
        assert_eq!(parse(&format!("{m}10")), None);
        assert_eq!(parse("(3) WhatsApp"), None);
        assert_eq!(parse("home"), None);
        assert_eq!(parse(&format!("{m}badge:4")), None);
        assert_eq!(parse_badge(&format!("{m}badge:4")), Some(4));
        assert_eq!(parse_badge(&format!("{m}badge:x")), None);
        assert_eq!(parse_badge("badge:4"), None);
        assert!(is_signal(&format!("{m}badge:x")));
    }
}
