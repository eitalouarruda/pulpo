(enabled, marker) => {
  if (window.top !== window || location.origin !== "https://music.youtube.com") return;
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
    const image = bar?.querySelector("img");
    const position = Number.isFinite(video?.currentTime) ? Math.max(0, video.currentTime) : 0;
    const details = {
      title: (metadata?.title || bar?.querySelector(".title")?.textContent?.trim() || "").slice(0, 256),
      artist: (metadata?.artist || bar?.querySelector(".byline")?.textContent?.trim() || "").slice(0, 256),
      artwork: (metadata?.artwork?.at(-1)?.src || (image?.getAttribute("src") ? image.src : "")).slice(0, 2048),
      paused: video?.paused ?? true,
      volume: video?.muted ? 0 : video?.volume ?? 1,
      duration: Number.isFinite(video?.duration) ? Math.max(0, video.duration) : 0,
      rate: Number.isFinite(video?.playbackRate) ? video.playbackRate : 1,
    };
    const key = JSON.stringify(details);
    // El texto del progreso de YouTube también cambia el DOM: no reenviarlo
    // cada vez. Sincronizar cada cinco segundos, o al cambiar/pausar/buscar.
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
    const nextVideo = document.querySelector("video");
    const nextBar = document.querySelector("ytmusic-player-bar");
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
  window.__pulpoMusic = {
    control(action, volume) {
      if (stopped) return;
      bind();
      if (action === "toggle" && video) {
        if (video.paused) video.play().catch(() => {});
        else video.pause();
      }
      if (action === "volume" && video) { video.volume = volume; video.muted = false; }
      if (action === "seek" && video && Number.isFinite(volume) && Number.isFinite(video.duration) && video.duration > 0) {
        video.currentTime = Math.max(0, Math.min(volume, video.duration));
        seekPending = true;
      }
      if (action === "next") bar?.querySelector(".next-button")?.click();
      if (action === "previous") bar?.querySelector(".previous-button")?.click();
      schedule();
    },
    stop() {
      stopped = true;
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
