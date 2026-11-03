(enabled, marker) => {
  if (window.top !== window) return;
  // Selectores de cada sitio: solo de respaldo y para mantener su propia interfaz
  // sincronizada. Los datos salen de Media Session y del elemento de audio/vídeo.
  const sites = {
    "https://music.youtube.com": {
      bar: "ytmusic-player-bar",
      title: ".title",
      artist: ".byline",
      image: "img",
      next: ".next-button",
      previous: ".previous-button",
    },
    "https://open.spotify.com": {
      bar: "[data-testid='now-playing-bar'], footer",
      title: "[data-testid='context-item-info-title']",
      artist: "[data-testid='context-item-info-artist']",
      image: "img[data-testid='cover-art-image'], img",
      toggle: "[data-testid='control-button-playpause']",
      next: "[data-testid='control-button-skip-forward']",
      previous: "[data-testid='control-button-skip-back']",
      position: "[data-testid='playback-position']",
      duration: "[data-testid='playback-duration']",
      volume: "[data-testid='volume-bar'] input[type='range']",
    },
  };
  const site = sites[location.origin];
  if (!site) return;
  // Los ganchos se instalan una sola vez y antes que la página, aunque el
  // reproductor esté apagado: Spotify registra sus acciones de Media Session al
  // cargar y su elemento de audio no está en el documento.
  const hooks = window.__pulpoMediaHooks ??= (() => {
    const hooks = { handlers: {}, position: null, element: null, onchange: null };
    const changed = () => hooks.onchange?.();
    const session = navigator.mediaSession;
    if (session) {
      const setAction = session.setActionHandler?.bind(session);
      if (setAction) session.setActionHandler = (action, handler) => {
        hooks.handlers[action] = handler;
        changed();
        return setAction(action, handler);
      };
      const setPosition = session.setPositionState?.bind(session);
      if (setPosition) session.setPositionState = (state) => {
        const result = setPosition(state);
        hooks.position = state?.duration > 0 ? { ...state, at: performance.now() } : null;
        changed();
        return result;
      };
      for (const key of ["metadata", "playbackState"]) {
        const prop = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(session), key);
        if (prop?.get && prop?.set) Object.defineProperty(session, key, {
          configurable: true,
          enumerable: true,
          get() { return prop.get.call(session); },
          set(value) { prop.set.call(session, value); changed(); },
        });
      }
    }
    const play = HTMLMediaElement.prototype.play;
    // Los vídeos mudos en bucle son decorado (los "Canvas" de Spotify), no la música.
    const track = (element) => {
      if (element.loop || element.muted) return;
      hooks.element = element;
      changed();
    };
    HTMLMediaElement.prototype.play = function (...args) {
      track(this);
      return play.apply(this, args);
    };
    // Los que arrancan solos (autoplay) no pasan por play().
    document.addEventListener("play", (e) => {
      if (e.target instanceof HTMLMediaElement) track(e.target);
    }, true);
    return hooks;
  })();
  window.__pulpoMusic?.stop();
  if (!enabled) return;
  let video = null;
  let bar = null;
  let discovery = null;
  let observer = null;
  let timer = null;
  let restoreTimer = null;
  let pending = false;
  let retries = 0;
  let last = "";
  let lastDetails = "";
  let seekPending = false;
  let savedTitle = null;
  let ownTitle = null;
  let stopped = false;
  let lastPosition = 0;
  const events = ["play", "pause", "loadedmetadata", "durationchange", "volumechange", "emptied", "ended", "ratechange"];
  const seeked = () => { seekPending = true; schedule(); };
  const timeupdate = () => {
    if (Math.abs((video?.currentTime || 0) - lastPosition) >= 5) schedule();
  };
  const restore = () => {
    if (savedTitle !== null && document.title === ownTitle) document.title = savedTitle;
    savedTitle = ownTitle = null;
  };
  const find = (name) => site[name] ? (bar || document).querySelector(site[name]) : null;
  const text = (name) => find(name)?.textContent?.trim() || "";
  // "1:23" o "1:02:03" → segundos.
  const seconds = (value) => {
    if (!/^\d+(:\d{1,2})+$/.test(value)) return 0;
    return value.split(":").reduce((total, part) => total * 60 + Number(part), 0);
  };
  const paused = () => {
    if (video) return video.paused;
    const state = navigator.mediaSession?.playbackState;
    return state !== "playing";
  };
  const timing = () => {
    if (video && Number.isFinite(video.duration) && video.duration > 0) {
      return { position: Math.max(0, video.currentTime || 0), duration: video.duration };
    }
    const state = hooks.position;
    if (state) {
      const rate = Number.isFinite(state.playbackRate) ? state.playbackRate : 1;
      const elapsed = paused() ? 0 : (performance.now() - state.at) / 1000 * rate;
      return { position: Math.min(state.duration, Math.max(0, (state.position || 0) + elapsed)), duration: state.duration };
    }
    return { position: seconds(text("position")), duration: seconds(text("duration")) };
  };
  const publish = () => {
    timer = null;
    if (stopped) return;
    // No pisar los atajos ni contadores que usan el mismo canal.
    if (document.title.startsWith(marker)) {
      if (++retries <= 5) timer = setTimeout(publish, 80);
      return;
    }
    retries = 0;
    const metadata = navigator.mediaSession?.metadata;
    const image = bar?.querySelector(site.image);
    const { position, duration } = timing();
    const details = {
      title: (metadata?.title || text("title")).slice(0, 256),
      artist: (metadata?.artist || text("artist")).slice(0, 256),
      artwork: (metadata?.artwork?.at(-1)?.src || (image?.getAttribute("src") ? image.src : "")).slice(0, 2048),
      paused: paused(),
      volume: video?.muted ? 0 : video?.volume ?? 1,
      duration,
      rate: Number.isFinite(video?.playbackRate) ? video.playbackRate : 1,
    };
    const key = JSON.stringify(details);
    // El texto del progreso también cambia el DOM: no reenviarlo cada vez.
    // Sincronizar cada cinco segundos, o al cambiar/pausar/buscar.
    if (key === lastDetails && Math.abs(position - lastPosition) < 5 && !seekPending) return;
    seekPending = false;
    const state = JSON.stringify({ ...details, position });
    if (state === last) return;
    lastDetails = key;
    lastPosition = position;
    last = state;
    savedTitle = document.title;
    ownTitle = marker + "music:" + state;
    document.title = ownTitle;
    restoreTimer = setTimeout(restore, 50);
  };
  const schedule = () => {
    if (!stopped && timer === null) timer = setTimeout(publish, 150);
  };
  const bind = () => {
    const nextVideo = hooks.element || document.querySelector("video:not([loop]), audio:not([loop])");
    const nextBar = document.querySelector(site.bar);
    if (video !== nextVideo) {
      video?.removeEventListener("timeupdate", timeupdate);
      video?.removeEventListener("seeked", seeked);
      if (video) for (const event of events) video.removeEventListener(event, schedule);
      video = nextVideo;
      if (video) for (const event of events) video.addEventListener(event, schedule);
      video?.addEventListener("timeupdate", timeupdate);
      video?.addEventListener("seeked", seeked);
    }
    if (bar !== nextBar) {
      observer?.disconnect();
      bar = nextBar;
      if (bar) {
        observer = new MutationObserver(schedule);
        // Solo texto y nodos del player; no observar atributos del progreso.
        observer.observe(bar, { childList: true, characterData: true, subtree: true });
      }
    }
    if (video && bar) {
      discovery?.disconnect();
      discovery = null;
    }
    schedule();
  };
  const discover = () => {
    if (pending || stopped) return;
    pending = true;
    queueMicrotask(() => { pending = false; if (!stopped) bind(); });
  };
  const start = () => {
    if (stopped) return;
    discovery = new MutationObserver(discover);
    discovery.observe(document.documentElement, { childList: true, subtree: true });
    bind();
  };
  const action = (name, details = {}) => {
    const handler = hooks.handlers[name];
    if (typeof handler !== "function") return false;
    try { handler({ action: name, ...details }); } catch { return false; }
    return true;
  };
  // Mueve el control de volumen de la página como si lo arrastrara el usuario,
  // para que no vuelva a poner el suyo al cambiar de canción.
  const slide = (input, fraction) => {
    const min = Number(input.min || 0);
    const max = Number(input.max || 1);
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
    setter.call(input, String(min + (max - min) * fraction));
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  };
  hooks.onchange = discover;
  window.__pulpoMusic = {
    control(name, value) {
      if (stopped) return;
      bind();
      if (name === "toggle") {
        const wasPaused = paused();
        const toggle = find("toggle");
        if (toggle) toggle.click();
        else if (!action(wasPaused ? "play" : "pause") && video) {
          if (video.paused) video.play().catch(() => {});
          else video.pause();
        }
      }
      if (name === "volume") {
        const input = find("volume");
        if (input) slide(input, value);
        if (video) { video.volume = value; video.muted = false; }
      }
      if (name === "seek" && Number.isFinite(value)) {
        const { duration } = timing();
        const target = Math.max(0, duration > 0 ? Math.min(value, duration) : value);
        if (!action("seekto", { seekTime: target }) && video && Number.isFinite(video.duration) && video.duration > 0) {
          video.currentTime = target;
        }
        seekPending = true;
      }
      if (name === "next" || name === "previous") {
        const control = find(name);
        if (control) control.click();
        else action(name === "next" ? "nexttrack" : "previoustrack");
      }
      schedule();
    },
    stop() {
      stopped = true;
      if (hooks.onchange === discover) hooks.onchange = null;
      clearTimeout(timer);
      clearTimeout(restoreTimer);
      restore();
      discovery?.disconnect();
      observer?.disconnect();
      if (video) for (const event of events) video.removeEventListener(event, schedule);
      video?.removeEventListener("timeupdate", timeupdate);
      video?.removeEventListener("seeked", seeked);
      document.removeEventListener("DOMContentLoaded", start);
    },
  };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
}
