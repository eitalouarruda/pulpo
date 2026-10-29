mod bitwarden;
mod ci;
mod diag;
mod display;
mod dnd;
mod icons;
mod links;
mod music;
mod platform;
mod popups;
mod services;
mod shortcuts;
mod spellcheck;
#[cfg(windows)]
mod tray;
mod update;
mod wallpaper;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use blocker::Blocker;
use serde::Serialize;
use tauri::webview::{
    NewWindowResponse, PageLoadEvent, PermissionKind, PermissionResponse, WebviewBuilder,
};
use tauri::window::WindowBuilder;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, Webview, WebviewUrl,
    WindowEvent,
};
use tauri_plugin_opener::OpenerExt;

use services::{Dnd, DndSchedule, IconSource, Preset, Service, Theme};

/// Ancho de la barra lateral (webview "shell"), en píxeles lógicos.
const SIDEBAR_WIDTH: f64 = 64.0;

struct AppState {
    config_path: PathBuf,
    services: Mutex<Vec<Service>>,
    active: Mutex<Option<String>>,
    status: Mutex<BlockerStatus>,
    /// Se fija una vez construido el motor (`None` si falló: los servicios con
    /// bloqueo se abren igual, sin él).
    blocker: OnceLock<Option<Arc<Blocker>>>,
    /// La barra lateral ocupa toda la ventana (diálogo de agregar/editar).
    overlay: AtomicBool,
    /// Abrir servicios espera al motor web: uno a la vez.
    opening: Mutex<()>,
    settings_path: PathBuf,
    settings: Mutex<display::Settings>,
    /// Pantalla donde está la ventana y el zoom que le toca.
    monitor: Mutex<Option<String>>,
    zoom: Mutex<f64>,
    /// La página inicial se ve sin diálogos encima: ahí va el panel de abajo.
    home_pane_visible: AtomicBool,
    /// Versión nueva encontrada al arrancar, esperando a que el usuario acepte.
    update: Mutex<Option<update::Release>>,
    /// Apps con no molestar activo ahora (ver `dnd`).
    dnd: Mutex<BTreeSet<String>>,
    /// Pedidos de recolocar los webviews (ver `layout`).
    layout: LayoutQueue,
}

/// Recolocar los webviews lo hace un solo hilo (`layout_worker`), en orden.
/// `layout` se pide desde muchos hilos a la vez (comandos de la barra lateral,
/// atajos, apertura de apps…), y cada pasada lee el estado y después manda
/// mostrar u ocultar: dos pasadas a la vez se pisaban y una vieja podía dejar
/// oculta la app de la página inicial después de que una nueva la mostrara.
/// No sirve un `Mutex` alrededor: una pasada espera al hilo principal (tamaño
/// de la ventana) y el hilo principal también pide pasadas.
#[derive(Default)]
struct LayoutQueue {
    request: Mutex<LayoutRequest>,
    wake: Condvar,
}

#[derive(Default)]
struct LayoutRequest {
    pending: bool,
    /// Webview que recibe el foco después de la pasada (ya visible).
    focus: Option<String>,
}

#[derive(Clone, Serialize)]
struct ZoomInfo {
    monitor: Option<String>,
    zoom: f64,
    /// `false` si el usuario eligió el tamaño para esta pantalla.
    auto: bool,
}

fn zoom_info(app: &AppHandle) -> ZoomInfo {
    let state = app.state::<AppState>();
    let monitor = state.monitor.lock().unwrap().clone();
    let auto = monitor
        .as_ref()
        .is_none_or(|m| !state.settings.lock().unwrap().zoom.contains_key(m));
    let zoom = *state.zoom.lock().unwrap();
    ZoomInfo {
        monitor,
        zoom,
        auto,
    }
}

#[tauri::command]
fn get_zoom(app: AppHandle) -> ZoomInfo {
    zoom_info(&app)
}

/// `step` ±1 agranda o achica un 10 % en esta pantalla; 0 vuelve al automático.
#[tauri::command]
fn change_zoom(app: AppHandle, step: i32) {
    let state = app.state::<AppState>();
    let Some(monitor) = state.monitor.lock().unwrap().clone() else {
        return;
    };
    {
        let mut settings = state.settings.lock().unwrap();
        if step == 0 {
            settings.zoom.remove(&monitor);
        } else {
            let current = *state.zoom.lock().unwrap();
            let zoom = display::clamp(current + 0.1 * step as f64);
            settings.zoom.insert(monitor.clone(), zoom);
        }
        if let Err(e) = display::save(&state.settings_path, &settings) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
    update_zoom(&app, true);
}

/// Mira en qué pantalla está la ventana y, si cambió (o `force`), aplica el
/// zoom que le corresponde.
fn update_zoom(app: &AppHandle, force: bool) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let Some(name) = window
        .current_monitor()
        .ok()
        .flatten()
        .and_then(|m| m.name().map(|n| display::monitor_name(n)))
    else {
        return;
    };
    let state = app.state::<AppState>();
    {
        let mut monitor = state.monitor.lock().unwrap();
        if !force && monitor.as_deref() == Some(name.as_str()) {
            return;
        }
        *monitor = Some(name.clone());
    }
    let chosen = state.settings.lock().unwrap().zoom.get(&name).copied();
    let zoom = chosen.map_or_else(|| display::auto_zoom(&name), display::clamp);
    *state.zoom.lock().unwrap() = zoom;
    if let Some(shell) = app.get_webview("shell") {
        let _ = shell.set_zoom(zoom);
    }
    let ids: Vec<String> = state
        .services
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    for id in &ids {
        if let Some(webview) = app.get_webview(id) {
            let _ = webview.set_zoom(zoom);
        }
    }
    layout(app);
    let _ = app.emit_to("shell", "zoom-changed", zoom_info(app));
}

fn current_zoom(app: &AppHandle) -> f64 {
    *app.state::<AppState>().zoom.lock().unwrap()
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum BlockerStatus {
    #[default]
    Loading,
    Ready,
    Error {
        message: String,
    },
}

#[derive(Clone, Serialize)]
struct BadgePayload {
    id: String,
    count: u32,
}

#[tauri::command]
fn list_services(state: tauri::State<'_, AppState>) -> Vec<Service> {
    state.services.lock().unwrap().clone()
}

#[tauri::command]
fn catalog() -> &'static [Preset] {
    services::CATALOG
}

#[tauri::command]
fn blocker_status(state: tauri::State<'_, AppState>) -> BlockerStatus {
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn active_service(state: tauri::State<'_, AppState>) -> Option<String> {
    state.active.lock().unwrap().clone()
}

#[tauri::command]
fn activate_service(app: AppHandle, id: String) {
    *app.state::<AppState>().active.lock().unwrap() = Some(id.clone());
    request_layout(&app, Some(id));
}

/// Página inicial: ninguna app visible.
#[tauri::command]
fn show_home(app: AppHandle) {
    *app.state::<AppState>().active.lock().unwrap() = None;
    layout(&app);
}

#[tauri::command]
fn reload_service(app: AppHandle, id: String) -> Result<(), String> {
    let webview = app.get_webview(&id).ok_or("error.unknownService")?;
    webview.reload().map_err(|e| e.to_string())
}

#[tauri::command]
fn add_service(
    app: AppHandle,
    name: String,
    url: String,
    adblock: bool,
    group: Option<String>,
    theme: Theme,
) -> Result<Service, String> {
    let url = services::normalize_url(&url)?;
    let state = app.state::<AppState>();
    let service = {
        let mut list = state.services.lock().unwrap();
        let name = match name.trim() {
            "" => tauri::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_else(|| url.clone()),
            name => name.to_string(),
        };
        let service = Service {
            id: services::new_id(&list),
            color: services::color_for(&name, &url),
            name,
            url,
            icon: None,
            icon_source: IconSource::Web,
            group: services::normalize_group(group),
            adblock,
            enabled: true,
            dnd: Dnd::default(),
            theme,
        };
        list.push(service.clone());
        service
    };
    *state.active.lock().unwrap() = Some(service.id.clone());
    services_changed(&app);
    fetch_icon(&app, service.id.clone(), service.url.clone());
    open_missing_async(&app);
    Ok(service)
}

#[tauri::command]
fn update_service(
    app: AppHandle,
    id: String,
    name: String,
    url: String,
    adblock: bool,
    group: Option<String>,
    theme: Theme,
) -> Result<(), String> {
    let url = services::normalize_url(&url)?;
    let state = app.state::<AppState>();
    let (url_changed, reopen) = {
        let mut list = state.services.lock().unwrap();
        let service = list
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("error.unknownService")?;
        let url_changed = service.url != url;
        // El tema también: cambiarlo en caliente no llega a todas las páginas
        // (algunas lo leen solo al crear el webview).
        let reopen = url_changed || service.adblock != adblock || service.theme != theme;
        if !name.trim().is_empty() {
            service.name = name.trim().to_string();
        }
        if url_changed {
            service.url = url.clone();
            if service.icon_source == IconSource::Web {
                service.icon = None;
            }
        }
        service.adblock = adblock;
        service.group = services::normalize_group(group);
        service.theme = theme;
        (url_changed, reopen)
    };
    services_changed(&app);
    if url_changed {
        fetch_icon(&app, id.clone(), url);
    }
    if reopen {
        // El bloqueo de red, los scripts y el tema se fijan al crear el webview.
        let app = app.clone();
        std::thread::spawn(move || {
            close_webview(&app, &id);
            open_missing(&app);
        });
    }
    Ok(())
}

/// Cambia el no molestar de una app y lo aplica enseguida.
fn change_dnd(app: &AppHandle, id: &str, change: impl FnOnce(&mut Dnd)) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut list = state.services.lock().unwrap();
        let service = list
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("error.unknownService")?;
        change(&mut service.dnd);
    }
    services_changed(app);
    dnd::refresh(app);
    Ok(())
}

#[tauri::command]
fn set_dnd_schedules(
    app: AppHandle,
    id: String,
    schedules: Vec<DndSchedule>,
) -> Result<(), String> {
    change_dnd(&app, &id, |dnd| {
        dnd.schedules = Dnd::clean_schedules(schedules)
    })
}

/// Silencia la app a mano hasta `until` (segundos Unix); `None` lo quita.
#[tauri::command]
fn silence_service(app: AppHandle, id: String, until: Option<i64>) -> Result<(), String> {
    change_dnd(&app, &id, |dnd| dnd.until = until)
}

#[tauri::command]
fn dnd_active(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.dnd.lock().unwrap().iter().cloned().collect()
}

#[tauri::command]
fn set_service_enabled(app: AppHandle, id: String, enabled: bool) -> Result<(), String> {
    set_enabled(&app, &id, enabled)
}

#[tauri::command]
fn remove_service(app: AppHandle, id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.services.lock().unwrap().retain(|s| s.id != id);
    {
        let mut active = state.active.lock().unwrap();
        if active.as_deref() == Some(id.as_str()) {
            *active = None;
        }
    }
    services_changed(&app);
    let app = app.clone();
    {
        let mut settings = state.settings.lock().unwrap();
        let links = settings.links.len();
        settings
            .links
            .retain(|r| r.app.as_deref() != Some(id.as_str()));
        if settings.home_app.as_deref() == Some(id.as_str()) || settings.links.len() != links {
            if settings.home_app.as_deref() == Some(id.as_str()) {
                settings.home_app = None;
            }
            let _ = display::save(&state.settings_path, &settings);
        }
    }
    std::thread::spawn(move || {
        close_webview(&app, &id);
        layout(&app);
        // Sesión y caché del servicio: ya no sirven.
        if let Some(dir) = profile_dir(&app, &id) {
            std::thread::sleep(Duration::from_secs(2));
            let _ = std::fs::remove_dir_all(dir);
        }
    });
    Ok(())
}

#[tauri::command]
fn reorder_services(app: AppHandle, order: Vec<OrderItem>) {
    let state = app.state::<AppState>();
    {
        let mut list = state.services.lock().unwrap();
        for service in list.iter_mut() {
            if let Some(item) = order.iter().find(|o| o.id == service.id) {
                service.group = services::normalize_group(item.group.clone());
            }
        }
        list.sort_by_key(|s| {
            order
                .iter()
                .position(|o| o.id == s.id)
                .unwrap_or(usize::MAX)
        });
    }
    services_changed(&app);
}

/// Posición de un servicio tras arrastrarlo: su grupo puede cambiar.
#[derive(serde::Deserialize)]
struct OrderItem {
    id: String,
    group: Option<String>,
}

/// Cambia el icono: `Custom` con la imagen elegida, `Web` vuelve a descargar
/// el favicon, `Letter` deja la inicial.
#[tauri::command]
fn set_service_icon(
    app: AppHandle,
    id: String,
    source: IconSource,
    icon: Option<String>,
) -> Result<(), String> {
    if source == IconSource::Custom {
        services::validate_icon(icon.as_deref().unwrap_or_default())?;
    }
    let state = app.state::<AppState>();
    let url = {
        let mut list = state.services.lock().unwrap();
        let service = list
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("error.unknownService")?;
        service.icon_source = source;
        service.icon = match source {
            IconSource::Custom => icon,
            IconSource::Web | IconSource::Letter => None,
        };
        service.url.clone()
    };
    services_changed(&app);
    if source == IconSource::Web {
        fetch_icon(&app, id, url);
    }
    Ok(())
}

#[tauri::command]
fn get_home_app(state: tauri::State<'_, AppState>) -> Option<String> {
    state.settings.lock().unwrap().home_app.clone()
}

/// Elige la app de la mitad inferior de la página inicial (`None`: ninguna).
#[tauri::command]
fn set_home_app(app: AppHandle, id: Option<String>) {
    let state = app.state::<AppState>();
    {
        let mut settings = state.settings.lock().unwrap();
        settings.home_app = id.clone();
        if let Err(e) = display::save(&state.settings_path, &settings) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
    let _ = app.emit_to("shell", "home-app-changed", id);
    layout(&app);
}

#[tauri::command]
fn get_link_rules(state: tauri::State<'_, AppState>) -> Vec<links::LinkRule> {
    state.settings.lock().unwrap().links.clone()
}

/// Guarda las reglas de enlaces (en orden: gana la primera que cubre el enlace).
#[tauri::command]
fn set_link_rules(app: AppHandle, rules: Vec<links::LinkRule>) -> Vec<links::LinkRule> {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    settings.links = links::normalize(rules);
    if let Err(e) = display::save(&state.settings_path, &settings) {
        eprintln!(
            "[pulpo] no se pudo guardar {}: {e}",
            state.settings_path.display()
        );
    }
    settings.links.clone()
}

/// Altura de la app de la página inicial, como parte de la ventana.
#[tauri::command]
fn get_home_pane_size(state: tauri::State<'_, AppState>) -> f64 {
    state.settings.lock().unwrap().home_pane()
}

/// Cambia la altura de la app de la página inicial. Mientras se arrastra el
/// divisor solo se recoloca (`save` falso); al soltarlo se guarda.
#[tauri::command]
fn set_home_pane_size(app: AppHandle, size: f64, save: bool) -> f64 {
    let state = app.state::<AppState>();
    let size = display::clamp_home_pane(size);
    {
        let mut settings = state.settings.lock().unwrap();
        settings.home_pane = Some(size);
        if save {
            if let Err(e) = display::save(&state.settings_path, &settings) {
                eprintln!(
                    "[pulpo] no se pudo guardar {}: {e}",
                    state.settings_path.display()
                );
            }
        }
    }
    layout(&app);
    size
}

/// Busca una versión nueva en GitHub. La barra lateral lo pide al arrancar y
/// pregunta antes de instalar nada.
#[tauri::command]
async fn check_update(app: AppHandle) -> Option<update::UpdateInfo> {
    if !update::supported() {
        return None;
    }
    let current = app.package_info().version.to_string();
    let found = tauri::async_runtime::spawn_blocking(move || update::check(&current))
        .await
        .ok()
        .flatten();
    let info = found.as_ref().map(update::Release::info);
    *app.state::<AppState>().update.lock().unwrap() = found;
    info
}

/// Descarga e instala la versión encontrada y cierra Pulpo; la versión nueva
/// arranca sola. Avisa del progreso con `update-progress`.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let release = app
        .state::<AppState>()
        .update
        .lock()
        .unwrap()
        .clone()
        .ok_or("update.none")?;
    let current = app.package_info().version.to_string();
    let progress_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        update::install(&release, &current, |progress| {
            let _ = progress_app.emit_to("shell", "update-progress", progress);
        })
    })
    .await
    .map_err(|e| e.to_string())??;
    // Que la versión nueva encuentre las cookies que las páginas acaban de
    // renovar (ver `platform::flush_cookies`).
    #[cfg(target_os = "linux")]
    {
        let webviews: Vec<_> = app.webviews().into_values().collect();
        tauri::async_runtime::spawn_blocking(move || {
            platform::flush_cookies(&webviews, Duration::from_secs(5))
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    app.exit(0);
    Ok(())
}

/// La barra lateral avisa cuando se ve la página inicial sin diálogos.
#[tauri::command]
fn set_home_pane_visible(app: AppHandle, visible: bool) {
    app.state::<AppState>()
        .home_pane_visible
        .store(visible, Ordering::SeqCst);
    layout(&app);
}

#[tauri::command]
fn set_overlay(app: AppHandle, open: bool) {
    app.state::<AppState>()
        .overlay
        .store(open, Ordering::SeqCst);
    layout(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Instancia de pruebas (`./dev.sh --test`): datos y caché aparte, para
    // poder abrirla junto a la de uso real. Antes de todo: Tauri y Chromium
    // leen estas variables al resolver sus carpetas.
    #[cfg(target_os = "linux")]
    if let Some(home) = diag::test_home() {
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
        std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    }
    let builder = tauri::Builder::default();
    #[cfg(target_os = "linux")]
    let builder = builder.runtime({
        let mut cef = tauri_runtime_cef::Cef::default()
            .profile_preference("browser.enable_spellchecking", true)
            .profile_preference_value("spellcheck.dictionaries", spellcheck::dictionaries());
        // Chromium guarda las sesiones bajo esta raíz. Por defecto es la caché
        // del usuario; se mueve a los datos de la app para que limpiar la
        // caché no cierre la sesión de WhatsApp.
        if let Some(dir) = cef_root() {
            cef = cef.root_cache_path(dir);
        }
        if diag::flag("PULPO_BENCH") {
            // El benchmark corre con la ventana en otro escritorio: que Chromium
            // no la dé por oculta ni frene sus timers.
            for arg in [
                "disable-backgrounding-occluded-windows",
                "disable-renderer-backgrounding",
                "disable-background-timer-throttling",
            ] {
                cef = cef.command_line_arg(arg, None::<String>);
            }
        }
        // Con la API del runtime: si no, fija `remote_debugging.allowed` a
        // false y Chromium rechaza el puerto aunque llegue por la línea de órdenes.
        match diag::debug_port() {
            Some(port) => cef.remote_debugging(tauri_runtime_cef::RemoteDebugging::Port {
                port,
                allowed_origins: Vec::new(),
            }),
            None => cef,
        }
    });
    #[cfg(windows)]
    let builder = builder.runtime(tauri_runtime_wry::Wry::default());
    builder
        // Sin su script de enlaces: cancela los clics en `target="_blank"` y pide
        // abrirlos por IPC, que las apps remotas no tienen, así que no se abría
        // nada. Esos enlaces pasan por `on_new_window` (ver `open_service`).
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let config_path = services::config_path(&data_dir);
            let settings_path = display::settings_path(&data_dir);
            let settings = display::load(&settings_path);
            let list = services::load(&config_path);
            // En modo benchmark YouTube Music tiene que estar visible: el motor
            // no pinta (ni corre requestAnimationFrame) en webviews ocultos.
            // Se arranca en la página inicial.
            let active = diag::flag("PULPO_BENCH").then(|| "ytmusic".to_string());
            let missing_icons: Vec<_> = list
                .iter()
                .filter(|s| s.icon.is_none() && s.icon_source == IconSource::Web)
                .map(|s| (s.id.clone(), s.url.clone()))
                .collect();
            app.manage(AppState {
                config_path,
                services: Mutex::new(list),
                active: Mutex::new(active),
                status: Mutex::default(),
                blocker: OnceLock::new(),
                overlay: AtomicBool::new(false),
                opening: Mutex::new(()),
                settings_path,
                settings: Mutex::new(settings),
                monitor: Mutex::default(),
                zoom: Mutex::new(1.0),
                home_pane_visible: AtomicBool::new(false),
                update: Mutex::default(),
                dnd: Mutex::default(),
                layout: LayoutQueue::default(),
            });
            layout_worker(app.handle());
            app.manage(music::Player::new(app.handle().clone()));
            #[cfg(target_os = "linux")]
            display::guard_focus(data_dir.join("focus.log"));
            dnd::watch(app.handle());
            app.manage(bitwarden::Vault::default());
            app.manage(ci::Ci::default());
            ci::watch(app.handle());

            let window = WindowBuilder::new(app, "main")
                .title(if diag::test_home().is_some() {
                    "Pulpo (test)"
                } else {
                    "Pulpo"
                })
                .inner_size(1280.0, 820.0)
                .min_inner_size(640.0, 420.0)
                .build()?;
            let height = window
                .inner_size()?
                .to_logical::<f64>(window.scale_factor()?)
                .height;
            let shell = WebviewBuilder::new("shell", WebviewUrl::App("index.html".into()))
                .position(LogicalPosition::new(0.0, 0.0))
                .size(LogicalSize::new(SIDEBAR_WIDTH, height))
                // El de Tauri (soltar archivos) rompe el arrastrar de HTML.
                .disable_drag_drop_handler()
                // Un zoom fijado antes de que cargue la página se pierde.
                .on_page_load(|webview, _| {
                    let _ = webview.set_zoom(current_zoom(webview.app_handle()));
                });
            #[cfg(target_os = "linux")]
            let shell = match platform::compose_script() {
                Some(script) => shell.initialization_script(script),
                None => shell,
            };
            // En desarrollo, la consola de la barra lateral sale por la terminal.
            #[cfg(all(debug_assertions, target_os = "linux"))]
            let shell = {
                use tauri_runtime_cef::WebviewBuilderCefExt;
                shell.on_console_message(|m| {
                    eprintln!(
                        "[shell] {:?} {} ({}:{})",
                        m.level, m.message, m.source, m.line
                    )
                })
            };
            window.add_child(shell)?;
            #[cfg(windows)]
            tray::create(app.handle())?;
            let handle = app.handle().clone();
            #[cfg(windows)]
            let tray_window = window.clone();
            window.on_window_event(move |event| {
                match event {
                    // Windows: cerrar la deja en la bandeja, funcionando (ver `tray.rs`).
                    #[cfg(windows)]
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        let _ = tray_window.hide();
                    }
                    // Al pasar a otra pantalla cambia el zoom (y con él el layout).
                    WindowEvent::Moved(_) => update_zoom(&handle, false),
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        update_zoom(&handle, false);
                        layout(&handle);
                    }
                    _ => {}
                }
            });

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                // Reordenar arrastrando en la barra lateral (ver `platform::enable_drag`).
                #[cfg(target_os = "linux")]
                if let Some(shell) = handle.get_webview("shell") {
                    if let Err(e) = platform::enable_drag(&shell) {
                        eprintln!("[pulpo] arrastrar en la barra lateral no disponible: {e}");
                    }
                }
                update_zoom(&handle, true);
                // Los servicios sin bloqueo no esperan a las listas.
                open_missing(&handle);
                prepare_blocker(&handle);
                open_missing(&handle);
            });
            for (id, url) in missing_icons {
                fetch_icon(app.handle(), id, url);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            music::music_get,
            music::music_set_enabled,
            music::music_control,
            list_services,
            catalog,
            blocker_status,
            active_service,
            activate_service,
            show_home,
            get_zoom,
            change_zoom,
            reload_service,
            add_service,
            update_service,
            set_service_enabled,
            remove_service,
            reorder_services,
            set_service_icon,
            set_overlay,
            get_home_app,
            set_home_app,
            set_home_pane_visible,
            get_home_pane_size,
            set_home_pane_size,
            get_link_rules,
            set_link_rules,
            set_dnd_schedules,
            silence_service,
            dnd_active,
            check_update,
            install_update,
            bitwarden::bw_enabled,
            bitwarden::bw_set_enabled,
            bitwarden::bw_status,
            bitwarden::bw_unlock,
            bitwarden::bw_lock,
            bitwarden::bw_logins,
            bitwarden::bw_fill,
            ci::ci_accounts,
            ci::ci_check_token,
            ci::ci_runs,
            ci::ci_dismiss,
            ci::ci_add_token,
            ci::ci_remove_account,
            ci::ci_list_repos,
            ci::ci_set_repos,
            ci::ci_open,
            ci::ci_notify,
            ci::ci_enabled,
            ci::ci_set_enabled,
            set_tray_texts,
            wallpaper::wallpaper_get,
            wallpaper::wallpaper_list,
            wallpaper::wallpaper_use,
            wallpaper::wallpaper_delete,
            wallpaper::wallpaper_import,
            wallpaper::wallpaper_search,
            wallpaper::wallpaper_download,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// `$XDG_DATA_HOME`, o `~/.local/share`: lo mismo que usa Tauri para
/// `app_data_dir`, pero disponible antes de crear la app.
#[cfg(target_os = "linux")]
pub(crate) fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(Into::into)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// Raíz de Chromium: perfiles, caché de GPU… (ver `run`).
#[cfg(target_os = "linux")]
pub(crate) fn cef_root() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("com.pulpo.app").join("cef"))
}

/// Textos del menú de la bandeja (solo Windows tiene bandeja).
#[tauri::command]
fn set_tray_texts(app: AppHandle, show: String, quit: String) {
    #[cfg(windows)]
    tray::set_tray_texts(app, show, quit);
    #[cfg(not(windows))]
    let _ = (app, show, quit);
}

/// Fondo de la página para las que no pintan el suyo: `Canvas` es blanco, u
/// oscuro si la página declara `color-scheme: dark`. Con `:where` no pesa
/// nada: cualquier fondo de la página manda.
const CANVAS_SCRIPT: &str = r#"(() => {
  const add = () => {
    const style = document.createElement("style");
    style.textContent = ":where(html) { background-color: Canvas; }";
    document.documentElement.prepend(style);
  };
  if (document.documentElement) return add();
  new MutationObserver((_, observer) => {
    if (!document.documentElement) return;
    observer.disconnect();
    add();
  }).observe(document, { childList: true });
})();"#;

/// `outerWidth`/`outerHeight` iguales al tamaño del webview, como un navegador
/// a pantalla completa. Al ser hijo de la ventana, el motor da el tamaño de la
/// ventana entera (barra lateral, panel de inicio…) y páginas que comparan
/// `outer*` con `inner*` para detectar DevTools (DevPulse: más de 180 px)
/// creían tenerlas abiertas.
const OUTER_SIZE_SCRIPT: &str = r#"(() => {
  for (const [outer, inner] of [["outerWidth", "innerWidth"], ["outerHeight", "innerHeight"]]) {
    Object.defineProperty(window, outer, {
      get: () => window[inner],
      set: () => {},
      configurable: true,
      enumerable: true,
    });
  }
})();"#;

/// Carpeta del perfil (cookies, sesión, caché) de un servicio.
fn profile_dir(app: &AppHandle, id: &str) -> Option<PathBuf> {
    let data = app.path().app_data_dir().ok()?;
    #[cfg(target_os = "linux")]
    return Some(data.join("cef").join(format!("Profile-{id}")));
    #[cfg(not(target_os = "linux"))]
    return Some(data.join("profiles").join(id));
}

/// Guarda la configuración y avisa a la barra lateral.
fn services_changed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let list = {
        let mut list = state.services.lock().unwrap();
        services::group_together(&mut list);
        list.clone()
    };
    if let Err(e) = services::save(&state.config_path, &list) {
        eprintln!(
            "[pulpo] no se pudo guardar {}: {e}",
            state.config_path.display()
        );
    }
    let _ = app.emit_to("shell", "services-changed", list);
}

fn set_enabled(app: &AppHandle, id: &str, enabled: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut list = state.services.lock().unwrap();
        let service = list
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("error.unknownService")?;
        service.enabled = enabled;
    }
    services_changed(app);
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        if enabled {
            open_missing(&app);
        } else {
            close_webview(&app, &id);
            layout(&app);
        }
    });
    Ok(())
}

/// Descarga el icono en segundo plano y lo guarda si el servicio sigue
/// apuntando a la misma URL.
fn fetch_icon(app: &AppHandle, id: String, url: String) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(icon) = icons::fetch(&url) else {
            return;
        };
        let state = app.state::<AppState>();
        {
            let mut list = state.services.lock().unwrap();
            let wanted =
                |s: &&mut Service| s.id == id && s.url == url && s.icon_source == IconSource::Web;
            match list.iter_mut().find(wanted) {
                Some(service) => service.icon = Some(icon),
                None => return,
            }
        }
        services_changed(&app);
    });
}

/// Cierra el webview de un servicio y espera a que desaparezca, para poder
/// volver a crearlo con la misma etiqueta.
fn close_webview(app: &AppHandle, id: &str) {
    let Some(webview) = app.get_webview(id) else {
        return;
    };
    let _ = webview.close();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.get_webview(id).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_status(app: &AppHandle, status: BlockerStatus) {
    *app.state::<AppState>().status.lock().unwrap() = status.clone();
    let _ = app.emit_to("shell", "blocker-status", status);
}

/// Descarga/lee las listas y construye el motor. Tarda: corre en un hilo aparte.
fn prepare_blocker(app: &AppHandle) {
    let built = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())
        .and_then(|dir| blocker::Lists::load(&dir.join("filter-lists")).map_err(|e| e.to_string()))
        .and_then(|lists| Blocker::new(&lists).map_err(|e| e.to_string()));
    let blocker = match built {
        Ok(blocker) => {
            set_status(app, BlockerStatus::Ready);
            Some(Arc::new(blocker))
        }
        Err(message) => {
            // Sin listas los servicios se abren igual, pero sin bloqueo.
            set_status(app, BlockerStatus::Error { message });
            None
        }
    };
    let _ = app.state::<AppState>().blocker.set(blocker);
}

fn open_missing_async(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || open_missing(&app));
}

/// Abre los servicios activados que todavía no tienen webview. Los que llevan
/// bloqueo esperan a que el motor esté listo.
fn open_missing(app: &AppHandle) {
    let state = app.state::<AppState>();
    let _one_at_a_time = state.opening.lock().unwrap();
    let list = state.services.lock().unwrap().clone();
    for service in list.iter().filter(|s| s.enabled) {
        if app.get_webview(&service.id).is_some() {
            continue;
        }
        let blocker = match (service.adblock, state.blocker.get()) {
            (false, _) => None,
            (true, None) => continue,
            (true, Some(blocker)) => blocker.clone(),
        };
        if let Err(e) = open_service(app, service, blocker) {
            eprintln!("[pulpo] no se pudo abrir {}: {e}", service.id);
        }
    }
    drop(_one_at_a_time);
    layout(app);
}

/// Abre el webview de `service`, con su id como etiqueta.
fn open_service(
    app: &AppHandle,
    service: &Service,
    blocker: Option<Arc<Blocker>>,
) -> tauri::Result<()> {
    let window = app.get_window("main").expect("ventana principal");
    // Cada servicio con su perfil: cookies y sesiones separadas. En Linux la
    // ruta es relativa a la raíz de Chromium (ver `run`), y tiene que ser una
    // subcarpeta directa: Chromium no crea perfiles más adentro.
    #[cfg(target_os = "linux")]
    let profile = PathBuf::from(format!("Profile-{}", service.id));
    #[cfg(not(target_os = "linux"))]
    let profile = app
        .path()
        .app_data_dir()?
        .join("profiles")
        .join(&service.id);

    // Se crea en blanco y se navega después de enganchar el bloqueo de red, para
    // que no se escape ninguna petición de la primera carga.
    #[cfg(target_os = "linux")]
    let (initial_load_tx, initial_load) = std::sync::mpsc::channel();
    #[cfg(target_os = "linux")]
    let initial_load_tx = Mutex::new(Some(initial_load_tx));
    let id = service.id.clone();
    let music_tx = app.state::<music::Player>().sender.clone();
    let music_load_tx = music_tx.clone();
    // La página puso un contador con `navigator.setAppBadge`: manda ese, no el
    // del título (Telegram pone ahí el nombre del chat). Vale hasta la siguiente carga.
    let app_badge = Arc::new(AtomicBool::new(false));
    let app_badge_load = app_badge.clone();
    let mut builder =
        WebviewBuilder::new(&id, WebviewUrl::External("about:blank".parse().unwrap()))
            .data_directory(profile)
            // Sin el manejador de soltar archivos de Tauri: no hace falta y en CEF
            // añade un paso por cada petición.
            .disable_drag_drop_handler()
            // Fondo blanco, como un navegador normal. Sin él CEF usa uno negro y
            // las páginas que no pintan su fondo (webmail de Locaweb) quedan con
            // texto oscuro sobre negro. Solo se respeta en los webviews creados
            // al arrancar; `CANVAS_SCRIPT` cubre los recreados después.
            .background_color(tauri::window::Color(255, 255, 255, 255))
            .initialization_script(CANVAS_SCRIPT)
            .initialization_script(OUTER_SIZE_SCRIPT)
            // Notificaciones permitidas: la app la añadió el usuario (como en
            // Rambox) y "No molestar" las silencia. WebView2 no tiene dónde
            // aceptarlas y las dejaba bloqueadas; CEF ya las permitía.
            .on_permission_request(|_, kind| match kind {
                PermissionKind::Notifications => PermissionResponse::Allow,
                _ => PermissionResponse::Default,
            })
            .initialization_script(shortcuts::script())
            .initialization_script(dnd::SCRIPT)
            .on_document_title_changed(move |webview: Webview, title| {
                // No consultar el runtime desde este callback de CEF.
                if music::receive(&music_tx, &id, &title) {
                    return;
                }
                if let Some(shortcut) = shortcuts::parse(&title) {
                    // Este aviso llega en el hilo de UI del motor web, y `layout` espera
                    // a ese mismo hilo: desde aquí se bloquearía.
                    let app = webview.app_handle().clone();
                    let id = id.clone();
                    std::thread::spawn(move || on_shortcut(&app, shortcut, &id));
                    return;
                }
                let count = match shortcuts::parse_badge(&title) {
                    Some(count) => {
                        app_badge.store(true, Ordering::Relaxed);
                        count
                    }
                    None if shortcuts::is_signal(&title) || app_badge.load(Ordering::Relaxed) => {
                        return
                    }
                    None => services::badge_from_title(&title).unwrap_or(0),
                };
                let _ = webview.app_handle().emit_to(
                    "shell",
                    "badge",
                    BadgePayload {
                        id: id.clone(),
                        count,
                    },
                );
            })
            .on_page_load(move |webview, payload| {
                if payload.event() == PageLoadEvent::Started {
                    app_badge_load.store(false, Ordering::Relaxed);
                    music::clear(&music_load_tx, webview.label());
                }
                // El estado de no molestar vive en la página: se repone en cada carga.
                let app = webview.app_handle();
                dnd::apply(app, webview.label(), dnd::is_active(app, webview.label()));
                // Chromium guarda el zoom por dominio: al cambiar de página vuelve al
                // que tuviera ese dominio, así que se repone.
                let _ = webview.set_zoom(current_zoom(webview.app_handle()));
                #[cfg(target_os = "linux")]
                // La carga en blanco que pidió la app (no el documento interno previo
                // del runtime): a partir de aquí se puede navegar.
                if payload.event() == PageLoadEvent::Finished
                    && payload.url().as_str() == "about:blank"
                {
                    if let Some(tx) = initial_load_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                }
                #[cfg(not(target_os = "linux"))]
                let _ = payload;
            })
            .on_new_window({
                let app = app.clone();
                move |url, features| {
                    // Las reglas del usuario mandan, salvo en los logins: esos necesitan
                    // volver a la página que los abrió.
                    if !popups::is_login(&url) {
                        let rule = links::route(
                            &app.state::<AppState>().settings.lock().unwrap().links,
                            &url,
                        )
                        .cloned();
                        if let Some(rule) = rule {
                            open_link(&app, url, rule.app);
                            return NewWindowResponse::Deny;
                        }
                    }
                    match popups::target(&url, features.size().is_some()) {
                        // Logins y ventanas emergentes: dentro de Pulpo, con la sesión del
                        // servicio y ligadas a la página que las abrió.
                        popups::Target::InApp => NewWindowResponse::Allow,
                        popups::Target::Browser => {
                            open_link(&app, url, None);
                            NewWindowResponse::Deny
                        }
                    }
                }
            });
    #[cfg(windows)]
    {
        builder = builder.additional_browser_args(&spellcheck::webview2_args());
    }
    #[cfg(target_os = "linux")]
    if let Some(script) = platform::compose_script() {
        builder = builder.initialization_script_for_all_frames(script);
    }
    #[cfg(target_os = "linux")]
    {
        builder = builder.initialization_script_for_all_frames(links::SCRIPT);
    }
    if music::is_music(&service.url) {
        builder = builder.initialization_script(music::script(
            app.state::<AppState>()
                .settings
                .lock()
                .unwrap()
                .music_player,
        ));
    }
    if let Some(ua) = diag::user_agent() {
        builder = builder.user_agent(&ua);
    }
    if let Some(blocker) = blocker.as_ref().filter(|_| !diag::flag("PULPO_NO_SCRIPTS")) {
        builder = builder.initialization_script_for_all_frames(blocker.page_script(&service.url));
    }
    if diag::flag("PULPO_BENCH") && service.id == "ytmusic" {
        builder = builder.initialization_script(diag::BENCH_SCRIPT);
        #[cfg(target_os = "linux")]
        {
            use tauri_runtime_cef::WebviewBuilderCefExt;
            builder = builder.on_console_message(|m| println!("[console] {}", m.message));
        }
    }

    let webview = window.add_child(
        builder
            .position(LogicalPosition::new(SIDEBAR_WIDTH, 0.0))
            .size(LogicalSize::new(1.0, 1.0)),
    )?;
    if placement(app, &service.id) == Placement::Hidden {
        webview.hide()?;
    }

    #[cfg(target_os = "linux")]
    platform::attach(&webview, initial_load, blocker, &service.url, service.theme)?;
    #[cfg(windows)]
    {
        let url = service.url.clone();
        let theme = service.theme;
        webview.with_webview(move |platform| platform::attach(platform, blocker, &url, theme))?;
    }
    Ok(())
}

/// Abre un enlace en la app `id`, o en el navegador del sistema si no hay app
/// (o ya no existe, está desactivada o aún no se abrió). Se llama desde el hilo
/// de UI del motor web: lo que toca el layout va en otro hilo.
fn open_link(app: &AppHandle, url: Url, id: Option<String>) {
    let service = id.and_then(|id| {
        app.state::<AppState>()
            .services
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id && s.enabled)
            .cloned()
    });
    let webview = service
        .as_ref()
        .and_then(|s| app.get_webview(&s.id).map(|w| (s, w)));
    let Some((service, webview)) = webview else {
        let _ = app.opener().open_url(url.as_str(), None::<&str>);
        return;
    };
    let url = links::for_app(&url, &service.url);
    let app = app.clone();
    let id = service.id.clone();
    std::thread::spawn(move || {
        if let Err(e) = webview.navigate(url) {
            eprintln!("[pulpo] no se pudo abrir el enlace en {id}: {e}");
            return;
        }
        // La barra lateral la selecciona (conoce el historial y los diálogos).
        let _ = app.emit_to("shell", "show-app", id);
    });
}

/// Atajo pulsado dentro de la app `id`. La barra lateral decide a cuál ir
/// (conoce el orden y el historial); la página inicial se muestra ya desde aquí.
fn on_shortcut(app: &AppHandle, shortcut: shortcuts::Shortcut, id: &str) {
    match shortcut {
        shortcuts::Shortcut::Home => show_home(app.clone()),
        // El selector se escribe en la barra lateral: el teclado pasa a ella.
        shortcuts::Shortcut::Switcher => {
            if let Some(shell) = app.get_webview("shell") {
                let _ = shell.set_focus();
            }
        }
        // Clic en una notificación: Pulpo al frente, en la app que la mostró.
        // Rellenar con Bitwarden: el shell busca las entradas y pregunta.
        shortcuts::Shortcut::Fill => {
            if bitwarden::enabled(app) {
                let _ = app.emit_to("shell", "fill-request", id);
            }
            return;
        }
        shortcuts::Shortcut::Notification => {
            raise_window(app);
            let _ = app.emit_to("shell", "notification-clicked", id);
            return;
        }
        shortcuts::Shortcut::Previous | shortcuts::Shortcut::Index { .. } => {}
    }
    let _ = app.emit_to("shell", "shortcut", shortcut);
}

/// Trae la ventana al frente aunque esté minimizada o en otro escritorio.
fn raise_window(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
    // Hyprland no deja que una app X11 se dé el foco a sí misma: se le pide a él.
    #[cfg(target_os = "linux")]
    display::focus_own_window();
}

/// Dónde se ve el webview de una app.
#[derive(PartialEq)]
enum Placement {
    Hidden,
    /// Toda la ventana a la derecha de la barra lateral: la app activa.
    Full,
    /// La parte de abajo de la página inicial: la app elegida para ella. Es el
    /// mismo webview de la app, no una copia.
    HomePane,
}

fn placement(app: &AppHandle, id: &str) -> Placement {
    let state = app.state::<AppState>();
    let overlay = state.overlay.load(Ordering::SeqCst);
    let active = state.active.lock().unwrap().clone();
    if !overlay && active.as_deref() == Some(id) {
        return Placement::Full;
    }
    let home = overlay && active.is_none() && state.home_pane_visible.load(Ordering::SeqCst);
    if home && state.settings.lock().unwrap().home_app.as_deref() == Some(id) {
        return Placement::HomePane;
    }
    Placement::Hidden
}

/// Pide recolocar la barra lateral y los servicios (ver `LayoutQueue`). No
/// espera: los pedidos seguidos se juntan en una sola pasada con el estado más
/// reciente.
fn layout(app: &AppHandle) {
    request_layout(app, None);
}

/// Como `layout`, y después le da el foco al webview `focus`.
fn request_layout(app: &AppHandle, focus: Option<String>) {
    let queue = &app.state::<AppState>().layout;
    let mut request = queue.request.lock().unwrap();
    request.pending = true;
    if focus.is_some() {
        request.focus = focus;
    }
    queue.wake.notify_one();
}

fn layout_worker(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        let focus = {
            let queue = &app.state::<AppState>().layout;
            let mut request = queue.request.lock().unwrap();
            while !request.pending {
                request = queue.wake.wait(request).unwrap();
            }
            request.pending = false;
            request.focus.take()
        };
        apply_layout(&app);
        if let Some(webview) = focus.and_then(|id| app.get_webview(&id)) {
            let _ = webview.set_focus();
        }
    });
}

/// Coloca la barra lateral y los servicios según el tamaño de la ventana: la
/// app activa a pantalla completa, la de la página inicial en su parte de
/// abajo, y el resto ocultas. Solo desde `layout_worker`.
fn apply_layout(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let size = size.to_logical::<f64>(scale);
    let state = app.state::<AppState>();
    let overlay = state.overlay.load(Ordering::SeqCst);
    // La barra lateral crece con el zoom, como su contenido.
    let sidebar = (SIDEBAR_WIDTH * *state.zoom.lock().unwrap()).round();
    if let Some(shell) = app.get_webview("shell") {
        let width = if overlay { size.width } else { sidebar };
        let _ = shell.set_size(LogicalSize::new(width, size.height));
    }
    let content = LogicalSize::new((size.width - sidebar).max(1.0), size.height);
    let ids: Vec<String> = state
        .services
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    // La barra lateral (estirada a toda la ventana) pinta la parte de arriba de
    // la página inicial.
    let pane_size = state.settings.lock().unwrap().home_pane();
    let top = (size.height * (1.0 - pane_size)).round();
    for id in ids {
        let Some(webview) = app.get_webview(&id) else {
            continue;
        };
        let place = placement(app, &id);
        // Las ocultas conservan el tamaño completo: no se re-maquetan al volver.
        let (y, height) = match place {
            Placement::HomePane => (top, (size.height - top).max(1.0)),
            Placement::Full | Placement::Hidden => (0.0, size.height),
        };
        let _ = webview.set_position(LogicalPosition::new(sidebar, y));
        let _ = webview.set_size(LogicalSize::new(content.width, height));
        let _ = if place == Placement::Hidden {
            webview.hide()
        } else {
            webview.show()
        };
    }
}
