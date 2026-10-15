<p align="center">
  <img src="icon.svg" alt="Pulpo logo" width="96" />
</p>

<h1 align="center">Pulpo</h1>

<p align="center">
  Tus apps web en una sola ventana, sin anuncios.<br />
  Rust + Tauri para Linux y Windows, con el motor de bloqueo de Brave.
</p>

---

Pulpo junta servicios web (por ahora **WhatsApp** y **YouTube Music**) en una
ventana con barra lateral, al estilo Rambox, pero sin Electron ni Node.js: la
app es Rust ([Tauri 3](https://v3.tauri.app)) y las webs corren en Chromium.
En Linux se incluye Chromium (CEF), porque WebKitGTK navegaba ~3 veces más
lento; en Windows se usa WebView2, que ya es Chromium y viene con el sistema.

## Apps

Pulpo arranca en una **página inicial** con todas las apps por grupo y sus
contadores de no leídos. Se vuelve a ella con el pulpo de arriba de la barra
lateral o pulsando **Esc tres veces seguidas** dentro de cualquier app.

Atajos (funcionan también con el foco dentro de una app):

| Atajo | Acción |
| --- | --- |
| `Ctrl` `Ctrl` (dos toques) | Alterna entre las dos últimas apps |
| `Ctrl+1` … `Ctrl+9` | Va a la app en esa posición de la barra lateral |
| `Esc` `Esc` `Esc` | Página inicial |

Se agregan con **+** (catálogo o una URL cualquiera) y se administran con
**⚙** o con clic derecho sobre su icono: recargar, bloqueo de anuncios,
editar (nombre, dirección, grupo, icono), desactivar y quitar. Una app
desactivada queda cerrada: no carga ni recibe notificaciones.

El orden y los grupos se cambian arrastrando los iconos (soltar sobre el
título de un grupo mete la app en él), con ↑/↓ en **⚙** o editando
`~/.local/share/com.pulpo.app/services.json` **con Pulpo cerrado**: el orden
del archivo es el de la barra lateral, y `"group"` agrupa las apps (las que no
tienen grupo van arriba). Los grupos se pliegan con un clic en su título.

Los logins con Google, Apple, Microsoft, GitHub… se abren en una ventana de
Pulpo, con la sesión de la app; el resto de enlaces que abren ventana nueva van
al navegador del sistema.

## Tamaño por pantalla

En Linux Pulpo corre por XWayland. Con Hyprland y
`xwayland:force_zero_scaling = true` (Omarchy) las apps X11 no se escalan, así
que Pulpo aplica él mismo la escala de la pantalla donde está la ventana (p.
ej. 160 % en un portátil a 1,6) y la cambia al moverla de pantalla. Se ajusta
por pantalla con `Ctrl +` / `Ctrl -` / `Ctrl 0` (automático) o en **⚙**; se
guarda en `settings.json`.

## Bloqueo de anuncios

Usa [adblock-rust](https://github.com/brave/adblock-rust), el motor de Brave,
con las mismas listas que Brave activa por defecto (uBlock Origin, EasyList,
EasyPrivacy y las de Brave). Las listas se descargan al arrancar y se
refrescan una vez al día (`~/.cache/com.pulpo.app/filter-lists`).

| Capa | Linux (CEF) | Windows (WebView2) |
| --- | --- | --- |
| Red (peticiones a dominios de anuncios) | Dominio `Fetch` del DevTools Protocol + motor de Brave en cada petición | `WebResourceRequested` + motor de Brave en cada petición |
| Cosmético (ocultar elementos) | CSS inyectado al inicio de cada documento | igual |
| Scriptlets de uBO (`+js(...)`) | Inyectados al inicio de cada documento | igual |

Los anuncios de YouTube no se pueden bloquear por URL: vienen dentro de las
respuestas del reproductor. Los quitan los scriptlets de uBlock Origin
(`json-prune`, `set-constant`, …), igual que en Brave. Se generan desde el
código de uBO con:

```sh
pnpm update-ubo        # o: node scripts/update-ubo-resources.mjs <tag>
```

## Desarrollo

```sh
./dev.sh
```

Ver qué haría el bloqueador en una página, con las listas reales:

```sh
cd src-tauri
cargo run --release -p blocker --example inspect --features content-blocking -- https://music.youtube.com/
```

Cada push a `main` publica una release en GitHub con el AppImage (Linux) y los
instaladores `.exe`/`.msi` (Windows).

## Licencias

`src-tauri/crates/blocker/resources/ubo-resources.json` contiene los
scriptlets de uBlock Origin (GPL-3.0). adblock-rust es MPL-2.0.
