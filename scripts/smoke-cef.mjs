// Prueba el binario real con dos perfiles y archivos reales, sin servicios
// externos ni credenciales. CDP ejercita Chromium, no el explorador del SO.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:http";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const executable = resolve(process.argv[2] ?? "src-tauri/target/debug/pulpo");
const home = await mkdtemp(join(tmpdir(), "pulpo-smoke-"));
const fixture = `<!doctype html><title>Pulpo file drop fixture</title>
<style>body { margin: 0; } #drop { width: 100vw; height: 100vh; background: #ddd; }</style>
<div id="drop">Drop files here</div><script>
window.dropped = null;
for (const event of ['dragenter', 'dragover']) document.addEventListener(event, e => e.preventDefault());
document.addEventListener('drop', async e => {
  e.preventDefault();
  window.dropped = await Promise.all([...e.dataTransfer.files].map(async f => ({name: f.name, text: await f.text()})));
});
window.fixtureReady = true;
</script>`;
const server = createServer((_req, res) => {
  res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
  res.end(fixture);
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const origin = `http://127.0.0.1:${server.address().port}`;
const portServer = createServer();
portServer.listen(0, "127.0.0.1");
await once(portServer, "listening");
const debugPort = portServer.address().port;
await new Promise(resolve => portServer.close(resolve));
const data = join(home, "data/com.pulpo.app");
await mkdir(data, { recursive: true });
await writeFile(join(data, "services.json"), JSON.stringify(["one", "two"].map(id => ({
  id, name: id, url: `${origin}/${id}`, color: "#888888", icon_source: "letter",
  adblock: false, enabled: true,
}))));
const paths = [join(home, "foto de férias.txt"), join(home, "report.txt")];
await writeFile(paths[0], "arquivo de teste — Pulpo");
await writeFile(paths[1], "second file");
const sockets = [];
const child = spawn(executable, [], {
  env: { ...process.env, PULPO_DATA_HOME: home, PULPO_DEBUG_PORT: String(debugPort) },
  detached: true,
  stdio: ["ignore", "pipe", "pipe"],
});
child.stdout.pipe(process.stdout);
child.stderr.pipe(process.stderr);
let launchError;
child.on("error", error => { launchError = error; });

async function until(check, label, timeout = 60_000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (launchError) throw launchError;
    if (child.exitCode !== null || child.signalCode !== null) throw new Error("Pulpo exited before the smoke test finished");
    const result = await check();
    if (result) return result;
    await delay(200);
  }
  throw new Error(`Timed out: ${label}`);
}

async function connect(target) {
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  sockets.push(socket);
  await once(socket, "open");
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (pending.has(message.id)) {
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  });
  const call = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 10_000);
    pending.set(id, message => {
      clearTimeout(timer);
      if (message.error) reject(new Error(JSON.stringify(message.error)));
      else resolve(message.result);
    });
    socket.send(JSON.stringify({ id, method, params }));
  });
  return {
    call,
    async evaluate(expression) {
      const result = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
      assert.equal(result.exceptionDetails, undefined, JSON.stringify(result.exceptionDetails));
      return result.result.value;
    },
  };
}

try {
  const targets = await until(async () => {
    try {
      const list = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`, { signal: AbortSignal.timeout(2000) })).json();
      return ["one", "two"].every(id => list.some(t => t.url === `${origin}/${id}`)) && list;
    } catch { return false; }
  }, "two service pages navigating away from about:blank");
  const one = await connect(targets.find(t => t.url === `${origin}/one`));
  const two = await connect(targets.find(t => t.url === `${origin}/two`));
  for (const page of [one, two]) await until(() => page.evaluate("window.fixtureReady"), "fixture ready");
  assert.match(await one.evaluate("navigator.userAgent"), /Chrome\//);
  await one.evaluate("localStorage.setItem('session', 'one'); document.cookie = 'session=one;path=/;max-age=3600'");
  assert.equal(await two.evaluate("localStorage.getItem('session')"), null);
  assert.equal(await two.evaluate("document.cookie"), "");
  await two.evaluate("localStorage.setItem('session', 'two')");
  assert.equal(await one.evaluate("localStorage.getItem('session')"), "one");
  console.log("PASS: Chromium loads service pages and isolates cookies/localStorage at the same origin.");

  // Hace visible el servicio por el mismo botón que usa el usuario.
  const shellTarget = targets.find(t => !t.url.startsWith(origin) && t.type === "page");
  assert.ok(shellTarget, "local shell target");
  const shell = await connect(shellTarget);
  await until(() => shell.evaluate("Boolean(document.querySelector('.service[data-id=one]'))"), "sidebar ready");
  await shell.evaluate("document.querySelector('.service[data-id=one]').click()");
  await delay(500);
  for (const type of ["dragEnter", "dragOver", "drop"]) {
    await one.call("Input.dispatchDragEvent", {
      type, x: 100, y: 100,
      data: { items: [], files: paths, dragOperationsMask: 1 },
    });
  }
  const dropped = await until(() => one.evaluate("window.dropped"), "file drop delivered to DOM");
  assert.deepEqual(dropped, [
    { name: "foto de férias.txt", text: "arquivo de teste — Pulpo" },
    { name: "report.txt", text: "second file" },
  ]);
  assert.equal(await two.evaluate("window.dropped"), null);
  console.log("PASS: multiple real files reach the selected page, with Unicode names and intact contents.");
  if (process.argv.includes("--native-linux")) {
    await one.evaluate("window.dropped = null");
    const source = spawn("python3", ["scripts/native-file-drag.py", ...paths], {
      env: { ...process.env, GDK_BACKEND: "x11" }, stdio: "inherit",
    });
    const [code] = await once(source, "exit");
    assert.equal(code, 0, "external GTK drag source");
    const nativeDrop = await until(() => one.evaluate("window.dropped"), "native file drop forwarded from the parent window", 10_000);
    assert.deepEqual(nativeDrop, dropped);
    assert.equal(await two.evaluate("window.dropped"), null);
    console.log("PASS: real external GTK/X11 file drag reaches only the selected service.");
  }
} finally {
  for (const socket of sockets) socket.close();
  // Solo el grupo creado por este test, nunca otras instancias del usuario.
  if (child.pid) {
    try { process.kill(-child.pid, "SIGTERM"); } catch {}
    await delay(1000);
    try { process.kill(-child.pid, "SIGKILL"); } catch {}
  }
  server.closeAllConnections();
  server.close();
  await rm(home, { recursive: true, force: true });
}
