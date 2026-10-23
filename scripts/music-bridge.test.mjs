// node --test scripts/music-bridge.test.mjs
import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = fs.readFileSync(new URL("../src-tauri/src/music.js", import.meta.url), "utf8");

function harness(origin = "https://music.youtube.com") {
  const timers = new Map();
  const observers = [];
  const signals = [];
  const video = new EventTarget();
  Object.assign(video, {
    paused: false, muted: false, volume: 0.5, currentTime: 10, duration: 240, playbackRate: 1,
    pause() { this.paused = true; this.dispatchEvent(new Event("pause")); },
    play() { this.paused = false; this.dispatchEvent(new Event("play")); return Promise.resolve(); },
  });
  let next = 0, previous = 0, title = "YouTube Music", timerId = 0;
  const bar = { querySelector(selector) {
    if (selector === ".next-button") return { click() { next++; } };
    if (selector === ".previous-button") return { click() { previous++; } };
    return null;
  } };
  const document = new EventTarget();
  Object.assign(document, {
    readyState: "complete", documentElement: {},
    querySelector(selector) { return selector === "video" ? video : selector === "ytmusic-player-bar" ? bar : null; },
  });
  Object.defineProperty(document, "title", {
    get: () => title,
    set(value) { title = value; if (value.startsWith("MARK:music:")) signals.push(JSON.parse(value.slice("MARK:music:".length))); },
  });
  const metadata = { title: "Song", artist: "Artist", artwork: [{ src: "https://example.org/cover.jpg" }] };
  const window = {};
  window.top = window;
  const context = vm.createContext({
    window, document, location: { origin }, navigator: { mediaSession: { metadata } },
    queueMicrotask: (fn) => fn(),
    setTimeout(fn) { const id = ++timerId; timers.set(id, fn); return id; },
    clearTimeout(id) { timers.delete(id); },
    MutationObserver: class {
      constructor(callback) { this.callback = callback; this.active = false; observers.push(this); }
      observe() { this.active = true; }
      disconnect() { this.active = false; }
    },
  });
  const install = vm.runInContext(`(${source})`, context);
  const flush = () => {
    let steps = 0;
    while (timers.size) {
      assert.ok(++steps < 30, "bridge must settle without a polling loop");
      const [id, fn] = timers.entries().next().value;
      timers.delete(id);
      fn();
    }
  };
  return { install, flush, timers, observers, signals, video, metadata, document, window,
    counts: () => ({ next, previous }) };
}

test("publishes initial state, coalesces events and stays idle without changes", () => {
  const h = harness();
  h.install(true, "MARK:"); h.flush();
  assert.equal(h.signals.length, 1);
  assert.equal(h.signals[0].title, "Song");
  assert.equal(h.document.title, "YouTube Music");
  assert.equal(h.observers.filter(o => o.active).length, 1, "discovery disconnects after binding");
  for (let i = 0; i < 100; i++) h.video.dispatchEvent(new Event("volumechange"));
  assert.equal(h.timers.size, 1);
  h.flush(); assert.equal(h.signals.length, 1, "identical state is not sent");
  assert.equal(h.timers.size, 0, "no periodic work remains");
  h.metadata.title = "Another song";
  h.observers.find(o => o.active).callback(); h.flush();
  assert.equal(h.signals.at(-1).title, "Another song");
});

test("controls playback, track buttons and volume", () => {
  const h = harness(); h.install(true, "MARK:"); h.flush();
  const control = h.window.__pulpoMusic.control;
  control("toggle"); h.flush(); assert.equal(h.signals.at(-1).paused, true);
  control("toggle"); h.flush(); assert.equal(h.signals.at(-1).paused, false);
  control("next"); control("previous"); h.flush();
  assert.deepEqual(h.counts(), { next: 1, previous: 1 });
  control("volume", 0.2); h.flush(); assert.equal(h.signals.at(-1).volume, 0.2);
});

test("disabling removes listeners, observers and pending work; reenable is clean", () => {
  const h = harness(); h.install(true, "MARK:"); h.flush();
  h.video.pause();
  h.install(false, "MARK:");
  assert.equal(h.timers.size, 0);
  assert.ok(h.observers.every(o => !o.active));
  h.video.play(); assert.equal(h.timers.size, 0);
  h.install(true, "MARK:"); h.flush();
  assert.equal(h.observers.filter(o => o.active).length, 1);
  assert.equal(h.signals.length, 2);
});

test("does not overwrite shortcut signals and retries are bounded", () => {
  const h = harness();
  h.document.title = "MARK:home";
  h.install(true, "MARK:"); h.flush();
  assert.equal(h.signals.length, 0);
  assert.equal(h.document.title, "MARK:home");
  h.document.title = "YouTube Music";
  h.video.pause(); h.flush(); assert.equal(h.signals.length, 1);
});

test("does not install on another origin or while disabled", () => {
  const wrong = harness("https://example.org"); wrong.install(true, "MARK:");
  assert.equal(wrong.observers.length, 0);
  const off = harness(); off.install(false, "MARK:");
  assert.equal(off.observers.length, 0); assert.equal(off.timers.size, 0);
});

test("progress synchronizes at most every five seconds and seeks publish immediately", () => {
  const h = harness(); h.install(true, "MARK:"); h.flush();
  assert.equal(h.signals[0].position, 10);
  assert.equal(h.signals[0].duration, 240);
  const observer = h.observers.find(o => o.active);
  for (let i = 1; i < 50; i++) {
    h.video.currentTime = 10 + i / 10;
    h.video.dispatchEvent(new Event("timeupdate"));
    observer.callback(); h.flush();
  }
  assert.equal(h.signals.length, 1, "even progress DOM mutations must be throttled");
  h.video.currentTime = 15;
  h.video.dispatchEvent(new Event("timeupdate")); h.flush();
  assert.equal(h.signals.length, 2);
  h.window.__pulpoMusic.control("seek", 17); h.flush();
  assert.equal(h.video.currentTime, 17);
  assert.equal(h.signals.at(-1).position, 17);
  h.window.__pulpoMusic.control("seek", 9999); h.flush();
  assert.equal(h.video.currentTime, 240);
  h.video.duration = Infinity;
  h.window.__pulpoMusic.control("seek", 50); h.flush();
  assert.equal(h.video.currentTime, 240, "live streams cannot be seeked by this control");
});
