# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Pulpo: a Rambox-style desktop app (Tauri 3 alpha, Rust + vanilla TypeScript) that hosts many web apps in one window, each in its own session, with Brave's ad-blocking engine (adblock-rust). Targets Linux (CEF/Chromium runtime, runs through XWayland) and Windows (WebView2 via wry). Code comments, log messages and commit-adjacent prose in the codebase are in Spanish; UI strings live in `src/i18n.ts` (en/es/pt).

## Commands

```sh
./dev.sh                 # pnpm install if needed, then `pnpm tauri dev`, always as a test instance next to the real one: Vite on strict port 1450, data/cache in .dev-data/ (PULPO_DATA_HOME), CI tokens under keyring service com.pulpo.app.ci.test, window title "Pulpo (test)"
./dev.sh --update-ubo    # regenerate uBO scriptlets first (same as `pnpm update-ubo [tag]`)
./dev.sh --bench         # PULPO_BENCH=1: YouTube Music navigation benchmark
pnpm build               # tsc --noEmit + vite build (frontend only)
pnpm tauri build         # full bundle

# Rust checks (what CI runs; all from repo root)
cargo fmt --manifest-path src-tauri/Cargo.toml --all --check
cargo clippy --manifest-path src-tauri/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --workspace
cargo test --manifest-path src-tauri/Cargo.toml -p pulpo <test_name>   # single test

# Inspect what the blocker would do on a page, with the real lists
cd src-tauri && cargo run --release -p blocker --example inspect --features content-blocking -- https://music.youtube.com/
```

`tauri::generate_context!` needs `dist/` to exist; for cargo-only builds without a frontend build, `mkdir -p dist && echo '<!doctype html>' > dist/index.html` (CI does this). Linux builds need GTK 4.12+, CMake and Ninja; the first build downloads CEF (~1 GB). Windows-only code (`platform/windows.rs`, WebView2) only compiles on Windows — CI runs clippy/tests on both.

Diagnostic env vars (`src-tauri/src/diag.rs`): `PULPO_NO_NETFILTER`, `PULPO_NO_SCRIPTS`, `PULPO_BENCH`, `PULPO_UA`, `PULPO_LOG_BLOCKED` (Linux), `PULPO_DEBUG_PORT` (Linux, Chromium remote DevTools), `PULPO_DATA_HOME` (Linux, test instance: sets `XDG_DATA_HOME`/`XDG_CACHE_HOME` only inside the app, since mise/pnpm/CEF downloads also live under the real ones).

## Architecture

**One window, many child webviews.** `src-tauri/src/lib.rs` creates window `main` and adds child webviews (Tauri `unstable` feature, `Window::add_child`):
- `shell` — the local frontend (`index.html` + `src/main.ts`): the 64px sidebar, home page and all dialogs. Only this webview has IPC (`capabilities/default.json`).
- One webview per enabled service, labelled with the service `id`, each with its own profile dir (Linux: `<data>/cef/Profile-<id>`, must be a direct child of the CEF root cache path; Windows: `<data>/profiles/<id>`).
`layout()` positions/shows/hides all webviews from `AppState` (active service, `overlay`, zoom) via `placement()`: the active service full size, the `home_app` service's own webview (not a copy) in the bottom part of the home page (`home_pane` fraction in `settings.json`), the rest hidden at full size. When a dialog or the home page is open, the shell sets `overlay` and is stretched to full window width, covering the services. Almost every state change ends in `layout()`, which only queues a pass: a single `layout_worker` thread applies them in order, coalescing bursts (concurrent passes used to interleave show/hide). Never wait for a pass from the main thread; `request_layout(app, Some(id))` focuses a webview after the pass.

**Rust ↔ frontend.** The shell calls `#[tauri::command]`s via `invoke` and listens to events emitted with `emit_to("shell", …)`: `services-changed`, `badge`, `blocker-status`, `shortcut`, `zoom-changed`, `home-app-changed`. Command errors are i18n keys (e.g. `"error.unknownService"`) translated in the frontend by `tError`. TS types in `main.ts` mirror the serde structs in `services.rs`/`lib.rs` — keep them in sync (serde enums use `tag` + lowercase renames).

**Remote services have no IPC.** Communication from a service page back to Rust goes through `on_document_title_changed`:
- Unread badges are parsed from the page title (`services::badge_from_title`).
- In-app keyboard shortcuts (`shortcuts.rs`): an initialization script briefly sets the title to a per-run random marker + action, which Rust parses and restores. The title callback runs on the web engine's UI thread, so it must spawn a thread before doing anything that calls `layout()` (deadlock otherwise).

**Opening services (threading).** Webview creation is serialized by `AppState.opening` and done off the main thread (`open_missing`, `sync_home_pane`). Services with `adblock: true` wait until the blocker engine is built (`prepare_blocker`, stored in a `OnceLock`); services without it open immediately. Network blocking and injected scripts are fixed at webview creation, so changing URL/adblock closes and recreates the webview (`close_webview` waits for the label to disappear before reuse). On Linux the webview starts at `about:blank` and only navigates after the blocker is attached, so no first-load request escapes.

**Ad blocking** (`crates/blocker`, used by `platform/`):
- `blocker` crate: builds the adblock `Engine` from lists in `lists.rs` (downloaded, cached in `<cache>/filter-lists`, refreshed daily) plus `resources/ubo-resources.json` (uBO scriptlets, generated by `scripts/update-ubo-resources.mjs`; don't hand-edit). `page_script()` produces the per-URL cosmetic CSS + scriptlet injection script. Trusted lists (uBO, Brave) get the `TRUSTED` permission bit required by `trusted-*` scriptlets.
- `platform/linux.rs`: network blocking via CDP `Fetch` domain on CEF. `platform/windows.rs`: `WebResourceRequested` on WebView2.

**HTML drag and drop on Linux** (`platform/linux_drag.rs`): CEF hosted as a child window can't start X11's native drag (pages get `dragstart` then `dragend` immediately). Every webview, the shell included, enables CDP `Input.setInterceptDrags`; on `Input.dragIntercepted` Pulpo polls the pointer with `XQueryPointer` on the browser's XID and dispatches `dragEnter`/`dragOver`/`drop` (or `dragCancel` if released outside) in CSS px (`devicePixelRatio`). Only same-page drags are covered.

**Do not disturb** (`dnd.rs`): per-app `dnd` in `services.json` (weekly `schedules` with Monday = 0 and minutes since midnight; overnight ranges belong to the start day; manual `until` in Unix seconds). A thread rechecks every 15 s with local time (chrono), keeps `AppState.dnd` (active ids), tells each page via `eval(window.__pulpoDnd(bool))` (also on every page load) and emits `dnd-changed` to the shell. The init script swaps `Notification`/`showNotification` for silent fakes and rejects `play()`/`AudioBufferSourceNode.start` without recent user activation. Badges are unaffected. Service-worker push notifications are not covered.

**CI** (`ci.rs`): a native view of the shell (sidebar button `#ci`), not a service webview. `ci-view` is a full page (`#overlay.page-mode`) with only the runs; Settings opens the `ci-setup-view` modal on top (tree: each token with its repos to check, Remove), and "+ Add token" the `ci-token-view` modal (`ci_check_token` validates and lists repos without saving, `ci_add_token` saves token + chosen repos). `openOverlay` keeps `ci-view` visible under its modals and `currentView()` scans views in reverse. A fine-grained token can read every public repo, so `token_repos` keeps only the org's repos when a token reaches exactly one org. GitHub fine-grained tokens ("Actions: Read-only", one per resource owner) live in the system keyring (`keyring` crate: Secret Service via zbus / Windows Credential Manager; service `com.pulpo.app.ci`, user = account id). Every token of one user returns the same login, so each token is its own account (optional user `name`): id = login, then `login#2`, `login#3`…; only re-adding the exact same token reuses its account. `settings.json` (`ci`) only keeps the accounts (`id`, `login`, `owners`; old plain-login strings still load) and watched repos (`account` = id). A thread polls `actions/runs` per repo with `If-None-Match` (20 s with active runs, 60 s idle, 5 min after errors; 401 marks the account invalid), emits `ci-changed`/`ci-accounts` and, when a run finishes, `ci-finished`; the shell writes the localized text and calls `ci_notify` (`notify-rust`; click opens the run). Runs already finished on the first poll of a repo are ignored; finished ones are dropped 10 min later. `ci_open` only opens `https://github.com/` URLs.

**Hyprland focus guard** (`display::guard_focus`): listens to Hyprland's event socket and, if the active window becomes one of Pulpo's untitled auxiliary X windows (keyboard dead while Pulpo looks focused), focuses the main window again and logs it to `<data>/focus.log`.

**Compose / dead keys on Linux** (`platform/linux_compose.rs`): the codecs CEF (`cef-vaapi`) is built with `use_gtk=false`, so Chromium has no input method on X11 (Compose arrives as `key: "Compose"`, dead keys as `key: "Dead"`, and nothing composes). An init script in every webview (shell included, all frames) composes in the page, with the locale compose table (incl. `~/.XCompose`) read through libxkbcommon (dlopen) and the dead-key positions from the X keymap. With a GTK-enabled CEF (local dev) keys arrive as `Process`/229 and the script does nothing.

**Themes:** the shell theme (dark/light/system, `src/theme.ts`, `localStorage`) sets `:root[data-theme]`; colors are CSS variables in `styles.css`. Per-app `theme` in `services.json` (`auto`/`light`/`dark`) forces `prefers-color-scheme`: CDP `Emulation.setEmulatedMedia` on Linux (plus `Emulation.setAutoDarkModeOverride` for `dark`, so pages without a dark theme get darkened too), the WebView2 profile's `PreferredColorScheme` on Windows; set at creation (`platform::attach`) and changing it recreates the webview (like URL/adblock): changing it live did not reach every page.

**Wallpaper** (`wallpaper.rs`): background of the home page, and (under a `--wallpaper-veil` tint) of the CI page and dialogs. The home page also shows a clock, date and a random greeting by time of day (`src/greetings.ts`, en/es/pt); its sizes use `cqh` units of `#overlay.home-mode` (`container-type: size`) so everything shrinks to fit in split mode. Own images (`wallpaper_import`, base64 from the shell) or Wallhaven (`wallpaper_search`, SFW only; `wallpaper_download` only from `*.wallhaven.cc`) are saved in `<data>/wallpapers`; `settings.json` `wallpaper` = active file name; `wallpaper-changed` event. The shell turns the data URL into a `blob:` URL (a multi-MB data URL in a CSS variable is dropped by Chromium). `wallpaper-view` is a full page opened from Manage apps (`returnTo = "manage"`): header, saved row and search bar fixed, only `#wallpaper-scroll` scrolls.

**VPN per app** (`vpn.rs`): per-app `vpn` in `services.json` (ISO country code) routes only that app through Urban VPN's free proxies, using the unofficial API of its Chrome extension (none of its code): anonymous registration → `accs` security token → `entrypoints/countries` (servers + per-server signature, ~30 min) → `accs-proxy` token (~30 min) = proxy user, password `"1"`. CEF can't answer proxy auth, so each country in use gets a local relay on `127.0.0.1` (thread per connection) that adds `Proxy-Authorization`, retries once with a fresh token on 407/403 and forces `Connection: close` on plain HTTP. The webview gets `proxy_url` (Windows: `--proxy-server` in `additional_browser_args`, since wry ignores `proxy_url` when custom args are set) and WebRTC `disable_non_proxied_udp`; if either fails the app doesn't open (never falls back to the direct connection). Set at creation: changing it recreates the webview. `cargo test -p pulpo vpn -- --ignored` checks it against the real servers.

**Windows tray** (`tray.rs`, `tray-icon` feature only in the Windows target deps): closing the window hides it (`CloseRequested` → `prevent_close`), apps keep running; tray click shows it, menu Quit exits. Menu texts come from the shell (`set_tray_texts`).

**Permissions:** service webviews allow web notifications (`on_permission_request`); WebView2 otherwise left them blocked with no UI to accept. Service webviews get `background_color` white plus `CANVAS_SCRIPT` (`:where(html){background-color:Canvas}`), because CEF painted pages without their own background black, and recreated webviews ignore `background_color`.

**Other modules:** `services.rs` (Service model, built-in `CATALOG`, `services.json` load/save, URL/group normalization), `display.rs` (`settings.json`: per-monitor zoom overrides and `home_app`; auto zoom asks Hyprland for monitor scale because of `xwayland:force_zero_scaling`), `popups.rs` (which `window.open` targets stay in-app — OAuth sign-in pop-ups — vs. go to the system browser), `links.rs` (user link rules in `settings.json`: pattern → app or browser, checked first in `on_new_window`; `links.js` turns Ctrl/middle clicks into `window.open` on Linux because CEF drops them; the opener plugin's own link script is disabled because it routes `_blank` clicks through IPC, which remote services lack), `icons.rs` (favicon fetch), `spellcheck.rs`.

**Updates** (`update.rs`): the shell calls `check_update` at startup (GitHub `releases/latest` vs `package_info().version`; release builds only, or `PULPO_UPDATE_CHECK=1`) and asks; `install_update` downloads the asset with `update-progress` events and exits. AppImage (`$APPIMAGE`): downloaded next to the running file, renamed to the new version if the name carries the old one (else replaced in place), old file deleted; a detached `sh` waits for this process and any orphaned CEF processes of the same `--user-data-dir`, then starts the new AppImage with `APPIMAGELAUNCHER_DISABLE=1`. Windows: NSIS setup run with `/P /R /UPDATE` after Pulpo exits. CI marks each release `--latest`.

**Persistence:** `~/.local/share/com.pulpo.app/services.json` (array order = sidebar order; `group` groups apps, `services::group_together` keeps groups contiguous) and `settings.json`.

## Release / CI

`.github/workflows/build.yml`: every push to `main` runs fmt/clippy/tests on Linux+Windows, then publishes a release versioned `MAJOR.MINOR` from `tauri.conf.json` + run number. The Linux AppImage is built in an Arch container (newest glibc), repacked as SquashFS with extra NSS/OpenGL libs, and smoke-tested on Ubuntu before publishing. Pushing to `main` = shipping a release.

The AppImage uses CEF **with proprietary codecs** (H.264/AAC; the official Spotify CDN build has none, so Discord GIFs — MP4 — stay still). `scripts/cef-codecs.sh <version> --upload` assembles it (official minimal distribution + `libcef.so`/resources from the Arch `cef-vaapi` package, same version and API hash) and uploads it to the prerelease `cef-<version>-codecs`; the `build-linux` job downloads it into `CEF_PATH` (pinned by `CEF_CODECS_SHA256`) and installs the Arch libs that build links (openh264, libxml2, dav1d…) so quick-sharun bundles them. When `tauri-runtime-cef` moves to another CEF, rerun the script and update `CEF_VERSION` + `CEF_CODECS_SHA256`; the job fails if `Cargo.lock` and `CEF_VERSION` disagree, or if the bundled `libcef.so` isn't the codecs one (cef-dll-sys silently falls back to downloading the official CEF). Local dev builds and the `test` job still use the official CEF.
