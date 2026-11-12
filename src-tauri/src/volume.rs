//! Volumen por app. El sistema ve todo el sonido de Pulpo como una sola
//! fuente (un único proceso de audio del motor web), así que el volumen de cada
//! app se aplica dentro de su página.
//!
//! Rust le dice a la página su nivel (0–1) con `eval`, en cada carga y al
//! cambiarlo. El script de inicio (en todos los marcos: el marco principal se
//! lo pasa a los suyos con `postMessage`) lo multiplica en:
//!
//! - `HTMLMediaElement.volume`: la página sigue leyendo el volumen que ella
//!   pidió; al elemento le llega ese por el nivel. Cubre `<audio>`/`<video>`,
//!   `new Audio()` y el audio remoto de las llamadas WebRTC (`srcObject`).
//! - Web Audio: lo que se conecta a `destination` pasa antes por un
//!   `GainNode` propio de cada contexto (los `OfflineAudioContext` no suenan y
//!   se dejan igual).
//!
//! No llega al sonido de los service workers ni de los plugins.

use tauri::{AppHandle, Manager};

use crate::services::Service;
use crate::AppState;

pub const SCRIPT: &str = r#"(() => {
  if (window.__pulpoVolume) return;
  const KEY = "__pulpoVolume";
  let level = 1;

  const prop = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, "volume");
  const wanted = new WeakMap();
  const seen = new WeakSet();
  const tracked = new Set();
  const applyMedia = (el) => {
    if (!wanted.has(el)) wanted.set(el, prop.get.call(el));
    if (!seen.has(el)) seen.add(el), tracked.add(new WeakRef(el));
    prop.set.call(el, wanted.get(el) * level);
  };
  Object.defineProperty(HTMLMediaElement.prototype, "volume", {
    configurable: true,
    enumerable: prop.enumerable,
    get() {
      return wanted.has(this) ? wanted.get(this) : prop.get.call(this);
    },
    set(value) {
      const v = Number(value);
      // Fuera de rango: que el original lance su error.
      if (!(v >= 0 && v <= 1)) return prop.set.call(this, value);
      wanted.set(this, v);
      applyMedia(this);
    },
  });
  const play = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (...args) {
    applyMedia(this);
    return play.apply(this, args);
  };
  // Los que arrancan solos (autoplay) no pasan por play().
  document.addEventListener("play", (e) => {
    if (e.target instanceof HTMLMediaElement) applyMedia(e.target);
  }, true);

  const gains = [];
  const byContext = new WeakMap();
  const Base = window.BaseAudioContext?.prototype;
  const Node = window.AudioNode?.prototype;
  if (Base && Node) {
    const createGain = Base.createGain;
    const connect = Node.connect;
    const disconnect = Node.disconnect;
    const isOutput = (target) =>
      target instanceof AudioDestinationNode &&
      !(window.OfflineAudioContext && target.context instanceof OfflineAudioContext);
    const gainFor = (ctx) => {
      let gain = byContext.get(ctx);
      if (!gain) {
        gain = createGain.call(ctx);
        gain.gain.value = level;
        connect.call(gain, ctx.destination);
        byContext.set(ctx, gain);
        gains.push(new WeakRef(gain));
      }
      return gain;
    };
    Node.connect = function (target, ...rest) {
      if (isOutput(target)) target = gainFor(target.context);
      return connect.call(this, target, ...rest);
    };
    Node.disconnect = function (target, ...rest) {
      if (isOutput(target) && byContext.has(target.context)) target = byContext.get(target.context);
      return disconnect.call(this, target, ...rest);
    };
  }

  const tell = (target) => {
    for (let i = 0; i < target.frames.length; i++) {
      try { target.frames[i].postMessage({ [KEY]: level }, "*"); } catch {}
    }
  };
  const set = (value) => {
    const v = Math.min(1, Math.max(0, Number(value) || 0));
    if (v === level) return;
    level = v;
    for (const ref of tracked) {
      const el = ref.deref();
      if (el) prop.set.call(el, wanted.get(el) * level);
      else tracked.delete(ref);
    }
    for (const el of document.querySelectorAll("audio, video")) applyMedia(el);
    gains.splice(0, gains.length, ...gains.filter((ref) => ref.deref()));
    for (const ref of gains) {
      const gain = ref.deref();
      if (gain) gain.gain.value = level;
    }
    tell(window);
  };
  window.__pulpoVolume = set;

  // Los marcos hijos reciben el nivel del suyo y se lo pasan a los suyos; uno
  // nuevo lo pide al cargar.
  window.addEventListener("message", (e) => {
    const data = e.data;
    if (!data || typeof data !== "object" || !(KEY in data)) return;
    if (data[KEY] === "ask") {
      try { e.source?.postMessage({ [KEY]: level }, "*"); } catch {}
    } else if (window.top !== window && e.source === window.parent) {
      set(data[KEY]);
    }
  });
  if (window.top !== window) window.parent.postMessage({ [KEY]: "ask" }, "*");

  if (window.top === window) {
    // ---------------- Ponte de sessionStorage para localStorage (_pss_) ----------------
    try {
      const PSS_PRE = "_pss_";
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k && k.startsWith(PSS_PRE)) {
          const rawKey = k.slice(PSS_PRE.length);
          if (sessionStorage.getItem(rawKey) === null) {
            sessionStorage.setItem(rawKey, localStorage.getItem(k));
          }
        }
      }
      const origSetItem = sessionStorage.setItem;
      sessionStorage.setItem = function(k, v) {
        origSetItem.call(this, k, v);
        try { localStorage.setItem(PSS_PRE + k, String(v)); } catch (_) {}
      };
      const origRemoveItem = sessionStorage.removeItem;
      sessionStorage.removeItem = function(k) {
        origRemoveItem.call(this, k);
        try { localStorage.removeItem(PSS_PRE + k); } catch (_) {}
      };
      const origClear = sessionStorage.clear;
      sessionStorage.clear = function() {
        origClear.call(this);
        try {
          const toRem = [];
          for (let i = 0; i < localStorage.length; i++) {
            const k = localStorage.key(i);
            if (k && k.startsWith(PSS_PRE)) toRem.push(k);
          }
          for (const k of toRem) localStorage.removeItem(k);
        } catch (_) {}
      };
    } catch (_) {}

    // ---------------- Autofill de Login (_pcred) ----------------
    try {
      const CRED_KEY = "_pcred";
      const autofill = () => {
        try {
          const raw = localStorage.getItem(CRED_KEY);
          if (!raw) return;
          const cred = JSON.parse(raw);
          if (!cred || !cred.u || !cred.p) return;
          const passInputs = Array.from(document.querySelectorAll('input[type="password"]'));
          for (const pass of passInputs) {
            if (!pass.value) {
              pass.value = cred.p;
              pass.dispatchEvent(new Event("input", { bubbles: true }));
              pass.dispatchEvent(new Event("change", { bubbles: true }));
            }
            const form = pass.form || pass.closest("form") || document;
            const user = form.querySelector('input[type="email"], input[type="text"], input[name*="user"], input[name*="login"], input[id*="user"], input[id*="login"]') ||
                         document.querySelector('input[type="email"], input[name*="user"], input[name*="login"]');
            if (user && !user.value) {
              user.value = cred.u;
              user.dispatchEvent(new Event("input", { bubbles: true }));
              user.dispatchEvent(new Event("change", { bubbles: true }));
            }
          }
        } catch (_) {}
      };

      document.addEventListener("submit", (e) => {
        try {
          const form = e.target;
          if (!(form instanceof HTMLFormElement)) return;
          const pass = form.querySelector('input[type="password"]');
          if (!pass || !pass.value) return;
          const user = form.querySelector('input[type="email"], input[type="text"], input[name*="user"], input[name*="login"]') ||
                       form.querySelector('input:not([type="password"]):not([type="hidden"]):not([type="submit"])');
          if (user && user.value) {
            localStorage.setItem(CRED_KEY, JSON.stringify({ u: user.value, p: pass.value }));
          }
        } catch (_) {}
      }, true);

      if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", () => setTimeout(autofill, 300));
      } else {
        setTimeout(autofill, 300);
      }
    } catch (_) {}
  }
})();"#;

/// Nivel de 0 a 1 que se aplica a la página de la app.
pub fn level(service: &Service) -> f64 {
    level_of(service.volume, service.muted)
}

pub fn level_of(volume: u8, muted: bool) -> f64 {
    if muted {
        0.0
    } else {
        f64::from(volume.min(100)) / 100.0
    }
}

pub fn apply_level(app: &AppHandle, id: &str, level: f64) {
    if let Some(webview) = app.get_webview(id) {
        let _ = webview.eval(format!(
            "window.__pulpoVolume && window.__pulpoVolume({level})"
        ));
    }
}

/// Repone en la página de la app el volumen guardado.
pub fn apply(app: &AppHandle, id: &str) {
    let level = {
        let state = app.state::<AppState>();
        let list = state.services.lock().unwrap();
        match list.iter().find(|s| s.id == id) {
            Some(service) => level(service),
            None => return,
        }
    };
    apply_level(app, id, level);
}
