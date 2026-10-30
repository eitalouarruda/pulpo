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
