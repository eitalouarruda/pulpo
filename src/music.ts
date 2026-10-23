import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t, tError } from "./i18n";

interface Track { title: string; artist: string; artwork: string; paused: boolean; volume: number; position: number; duration: number; rate: number }
interface Snapshot { enabled: boolean; service: string | null; track: Track | null }

export async function setupMusic(openService: (id: string) => void) {
  const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
  const card = $("music-player");
  const enabled = $<HTMLInputElement>("music-enabled");
  const volume = $<HTMLInputElement>("music-volume");
  const toggle = $<HTMLButtonElement>("music-toggle");
  const artwork = $<HTMLImageElement>("music-artwork");
  const progress = $<HTMLInputElement>("music-progress");
  let state: Snapshot = { enabled: false, service: null, track: null };
  let busy = false;
  let receivedAt = performance.now();
  let scrubbing = false;
  const clock = (seconds: number) => {
    const n = Math.max(0, Math.floor(seconds));
    const minutes = Math.floor(n / 60);
    return `${minutes}:${String(n % 60).padStart(2, "0")}`;
  };
  const fill = (input: HTMLInputElement) => {
    const percent = Number(input.max) > 0 ? Number(input.value) / Number(input.max) * 100 : 0;
    input.style.setProperty("--fill", `${percent}%`);
  };
  function renderProgress() {
    const track = state.track;
    const duration = track?.duration || 0;
    const elapsed = track && !track.paused ? (performance.now() - receivedAt) / 1000 * track.rate : 0;
    if (!scrubbing) {
      progress.max = String(duration || 1);
      progress.value = String(Math.min(duration, (track?.position || 0) + elapsed));
    }
    $("music-elapsed").textContent = clock(Number(progress.value));
    $("music-duration").textContent = clock(duration);
    progress.setAttribute("aria-valuetext", `${clock(Number(progress.value))} / ${clock(duration)}`);
    fill(progress);
    $("music-scrubber").style.setProperty("--fill", progress.style.getPropertyValue("--fill"));
  }

  function render(next = state) {
    if (next !== state) receivedAt = performance.now();
    state = next;
    enabled.checked = state.enabled;
    card.hidden = !state.enabled;
    $("home-view").classList.toggle("has-music", state.enabled);
    const track = state.track;
    const title = $("music-title");
    if (track?.title) {
      delete title.dataset.i18n;
      title.textContent = track.title;
    } else {
      title.dataset.i18n = "music.empty";
      title.textContent = t("music.empty");
    }
    $("music-artist").textContent = track?.artist || "";
    const src = track?.artwork || "";
    artwork.hidden = !src;
    $("music-placeholder").hidden = !!src;
    if (src && artwork.getAttribute("src") !== src) artwork.src = src;
    if (!src) artwork.removeAttribute("src");
    toggle.classList.toggle("is-playing", !!track && !track.paused);
    card.classList.toggle("is-playing", !!track && !track.paused);
    toggle.dataset.i18nLabel = track && !track.paused ? "music.pause" : "music.play";
    toggle.setAttribute("aria-label", t(track && !track.paused ? "music.pause" : "music.play"));
    for (const button of card.querySelectorAll<HTMLButtonElement>(".music-controls button")) button.disabled = busy || !track?.title;
    volume.disabled = busy || !track?.title;
    progress.disabled = busy || !track?.title || !(track.duration > 0);
    $("music-scrubber").classList.toggle("is-disabled", progress.disabled);
    if (document.activeElement !== volume) volume.value = String(Math.round((track?.volume ?? 1) * 100));
    fill(volume);
    renderProgress();
  }

  artwork.addEventListener("error", () => {
    artwork.hidden = true;
    $("music-placeholder").hidden = false;
  });
  enabled.addEventListener("change", async () => {
    enabled.disabled = true;
    const error = $("music-settings-error");
    error.hidden = true;
    try {
      render(await invoke<Snapshot>("music_set_enabled", { enabled: enabled.checked }));
    } catch (err) {
      error.textContent = t("music.error");
      error.hidden = false;
      console.error(err);
      // O arquivo pode ter sido salvo antes de falhar a instalação do script.
      render(await invoke<Snapshot>("music_get").catch(() => state));
    } finally { enabled.disabled = false; }
  });

  async function control(action: string, value?: number) {
    if (action === "open") {
      if (state.service) openService(state.service);
      else {
        $("music-error").textContent = t("music.unavailable");
        $("music-error").hidden = false;
      }
      return;
    }
    if (busy) return;
    busy = true;
    render();
    const error = $("music-error");
    error.hidden = true;
    try { await invoke("music_control", { action, volume: value ?? null }); }
    catch (err) {
      error.textContent = tError(err);
      error.hidden = false;
    } finally { busy = false; render(); }
  }
  for (const action of ["toggle", "next", "previous"]) {
    $(`music-${action}`).addEventListener("click", () => { void control(action); });
  }
  volume.addEventListener("change", () => { void control("volume", Number(volume.value) / 100); });
  volume.addEventListener("input", () => fill(volume));
  progress.addEventListener("input", () => { scrubbing = true; renderProgress(); });
  progress.addEventListener("change", () => {
    const position = Number(progress.value);
    scrubbing = false;
    if (state.track) { state.track.position = position; receivedAt = performance.now(); }
    void control("seek", position);
  });
  progress.addEventListener("pointercancel", () => { scrubbing = false; renderProgress(); });
  $("music-open").addEventListener("click", () => { void control("open"); });
  card.addEventListener("click", (event) => {
    if (event.target instanceof Element && !event.target.closest("button, input, label")) void control("open");
  });
  // Só o relógio visual avança aqui: nenhuma consulta ao Rust/webview.
  setInterval(() => {
    if (state.enabled && !state.track?.paused && !document.hidden && card.getClientRects().length) renderProgress();
  }, 1000);
  await listen<Snapshot>("music-changed", ({ payload }) => render(payload));
  // Sem polling: o cache inicial é local, e os próximos estados chegam por evento.
  render(await invoke<Snapshot>("music_get"));
  await listen("services-changed", async () => render(await invoke<Snapshot>("music_get")));
}
