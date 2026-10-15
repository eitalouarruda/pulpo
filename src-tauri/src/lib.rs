mod diag;
mod display;
mod icons;
mod platform;
mod popups;
mod services;
mod shortcuts;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use blocker::Blocker;
use serde::Serialize;
#[cfg(target_os = "linux")]
use tauri::webview::PageLoadEvent;
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::window::WindowBuilder;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Webview, WebviewUrl, WindowEvent,
};
use tauri_plugin_opener::OpenerExt;

use services::{IconSource, Preset, Service};

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
    for id in ids {
        if let Some(webview) = app.get_webview(&id) {
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
    layout(&app);
    if let Some(webview) = app.get_webview(&id) {
        let _ = webview.set_focus();
    }
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
        let reopen = url_changed || service.adblock != adblock;
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
        (url_changed, reopen)
    };
    services_changed(&app);
    if url_changed {
        fetch_icon(&app, id.clone(), url);
    }
    if reopen {
        // El bloqueo de red y los scripts se fijan al crear el webview.
        let app = app.clone();
        std::thread::spawn(move || {
            close_webview(&app, &id);
            open_missing(&app);
        });
    }
    Ok(())
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
fn set_overlay(app: AppHandle, open: bool) {
    app.state::<AppState>()
        .overlay
        .store(open, Ordering::SeqCst);
    layout(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(target_os = "linux")]
    let builder = builder.runtime({
        let mut cef = tauri_runtime_cef::Cef::default();
        // Chromium guarda las sesiones bajo esta raíz. Por defecto es la caché
        // del usuario; se mueve a los datos de la app para que limpiar la
        // caché no cierre la sesión de WhatsApp.
        if let Some(dir) = data_dir() {
            cef = cef.root_cache_path(dir.join("com.pulpo.app").join("cef"));
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
        match diag::debug_port() {
            Some(port) => cef.command_line_arg("remote-debugging-port", Some(port)),
            None => cef,
        }
    });
    #[cfg(windows)]
    let builder = builder.runtime(tauri_runtime_wry::Wry::default());
    builder
        .plugin(tauri_plugin_opener::init())
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
            });

            let window = WindowBuilder::new(app, "main")
                .title("Pulpo")
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
            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                match event {
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// `$XDG_DATA_HOME`, o `~/.local/share`: lo mismo que usa Tauri para
/// `app_data_dir`, pero disponible antes de crear la app.
#[cfg(target_os = "linux")]
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(Into::into)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

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
    layout(app);
}

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
    let mut builder = WebviewBuilder::new(
        &service.id,
        WebviewUrl::External("about:blank".parse().unwrap()),
    )
    .data_directory(profile)
    // Sin el manejador de soltar archivos de Tauri: no hace falta y en CEF
    // añade un paso por cada petición.
    .disable_drag_drop_handler()
    .initialization_script(shortcuts::script())
    .on_document_title_changed(move |webview: Webview, title| {
        if let Some(shortcut) = shortcuts::parse(&title) {
            // Este aviso llega en el hilo de UI del motor web, y `layout` espera
            // a ese mismo hilo: desde aquí se bloquearía.
            let app = webview.app_handle().clone();
            std::thread::spawn(move || on_shortcut(&app, shortcut));
            return;
        }
        let count = services::badge_from_title(&title).unwrap_or(0);
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
        // Chromium guarda el zoom por dominio: al cambiar de página vuelve al
        // que tuviera ese dominio, así que se repone.
        let _ = webview.set_zoom(current_zoom(webview.app_handle()));
        #[cfg(target_os = "linux")]
        // La carga en blanco que pidió la app (no el documento interno previo
        // del runtime): a partir de aquí se puede navegar.
        if payload.event() == PageLoadEvent::Finished && payload.url().as_str() == "about:blank" {
            if let Some(tx) = initial_load_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = payload;
    })
    .on_new_window({
        let app = app.clone();
        move |url, features| match popups::target(&url, features.size().is_some()) {
            // Logins y ventanas emergentes: dentro de Pulpo, con la sesión del
            // servicio y ligadas a la página que las abrió.
            popups::Target::InApp => NewWindowResponse::Allow,
            popups::Target::Browser => {
                let _ = app.opener().open_url(url.as_str(), None::<&str>);
                NewWindowResponse::Deny
            }
        }
    });
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
    if !is_visible(app, &service.id) {
        webview.hide()?;
    }

    #[cfg(target_os = "linux")]
    platform::attach(&webview, initial_load, blocker, &service.url)?;
    #[cfg(windows)]
    {
        let url = service.url.clone();
        webview.with_webview(move |platform| platform::attach(platform, blocker, &url))?;
    }
    Ok(())
}

/// Atajo pulsado dentro de una app. La barra lateral decide a cuál ir (conoce
/// el orden y el historial); la página inicial se muestra ya desde aquí.
fn on_shortcut(app: &AppHandle, shortcut: shortcuts::Shortcut) {
    if shortcut == shortcuts::Shortcut::Home {
        show_home(app.clone());
    }
    let _ = app.emit_to("shell", "shortcut", shortcut);
}

fn is_visible(app: &AppHandle, id: &str) -> bool {
    let state = app.state::<AppState>();
    !state.overlay.load(Ordering::SeqCst) && state.active.lock().unwrap().as_deref() == Some(id)
}

/// Coloca la barra lateral y los servicios según el tamaño de la ventana, y
/// deja visible solo el activo.
fn layout(app: &AppHandle) {
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
    for id in ids {
        let Some(webview) = app.get_webview(&id) else {
            continue;
        };
        let _ = webview.set_position(LogicalPosition::new(sidebar, 0.0));
        let _ = webview.set_size(content);
        let _ = if is_visible(app, &id) {
            webview.show()
        } else {
            webview.hide()
        };
    }
}
