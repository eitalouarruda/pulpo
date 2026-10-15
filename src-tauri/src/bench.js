// Benchmark de navegación en YouTube Music (PULPO_BENCH=1). Hace clic en las
// entradas de la guía lateral y mide hasta `yt-navigate-finish` y hasta que el
// hilo principal queda libre.
(() => {
  if (window.top !== window || location.hostname !== "music.youtube.com") return;
  const log = (...a) => console.log("[bench]", ...a);
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const waitFor = (ev, timeout = 20000) =>
    new Promise((res) => {
      const t0 = performance.now();
      const h = () => {
        document.removeEventListener(ev, h);
        res(performance.now() - t0);
      };
      document.addEventListener(ev, h);
      setTimeout(() => {
        document.removeEventListener(ev, h);
        res(-1);
      }, timeout);
    });
  const idle = () => new Promise((r) => requestAnimationFrame(() => setTimeout(r, 0)));

  // Bloqueos del hilo principal: retraso de un timer de 50 ms.
  let blocked = 0;
  let last = performance.now();
  setInterval(() => {
    const now = performance.now();
    const lag = now - last - 50;
    if (lag > 50) blocked += lag;
    last = now;
  }, 50);

  addEventListener("load", async () => {
    log("load", Math.round(performance.now()), "ms");
    await wait(6000);
    const items = () =>
      [...document.querySelectorAll("ytmusic-guide-entry-renderer tp-yt-paper-item")].slice(0, 3);
    log("guide items", items().length);
    const times = [];
    for (let round = 0; round < 3; round++) {
      for (const i of [1, 2, 0]) {
        const el = items()[i];
        if (!el) {
          log("no item", i);
          continue;
        }
        performance.clearResourceTimings();
        blocked = 0;
        const t0 = performance.now();
        el.click();
        // "Listo" = sin peticiones nuevas ni cambios en el DOM durante 600 ms.
        let lastActivity = performance.now();
        const mo = new MutationObserver(() => (lastActivity = performance.now()));
        mo.observe(document.body, { subtree: true, childList: true, attributes: true });
        let seen = 0;
        while (performance.now() - lastActivity < 600 && performance.now() - t0 < 20000) {
          const n = performance.getEntriesByType("resource").length;
          if (n !== seen) {
            seen = n;
            lastActivity = performance.now();
          }
          await wait(50);
        }
        mo.disconnect();
        const total = lastActivity - t0;
        times.push(total);
        const fmt = (r) => {
          const o = (t) => (t ? Math.round(t - r.startTime) : "-");
          return (
            `${r.name.replace(/^https:\/\//, "").slice(0, 40)} ${r.nextHopProtocol || "?"} ` +
            `worker=${o(r.workerStart)} fetch=${o(r.fetchStart)} dns=${o(r.domainLookupStart)}..${o(r.domainLookupEnd)} ` +
            `connect=${o(r.connectStart)}..${o(r.secureConnectionStart)}..${o(r.connectEnd)} ` +
            `request=${o(r.requestStart)} response=${o(r.responseStart)}..${o(r.responseEnd)}`
          );
        };
        const res = performance.getEntriesByType("resource");
        const browse = res.filter((r) => r.name.includes("youtubei/v1/") || r.name.includes("i.ytimg.com")).slice(0, 3);
        log(
          `nav item=${i} settled=${Math.round(total)}ms blocked=${Math.round(blocked)}ms requests=${res.length}`,
        );
        for (const r of browse) log("   ", fmt(r));
        await wait(2500);
      }
    }
    times.sort((a, b) => a - b);
    log("median", Math.round(times[times.length >> 1]), "ms");
  });
})();
