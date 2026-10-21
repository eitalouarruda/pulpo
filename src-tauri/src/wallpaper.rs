//! Fondo de pantalla de la página inicial: una imagen propia o una de
//! Wallhaven (solo las aptas para todo público). Las elegidas se guardan en
//! `<datos>/wallpapers` para volver a usarlas sin conexión; `settings.json`
//! guarda el nombre del archivo activo.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

const WALLHAVEN_API: &str = "https://wallhaven.cc/api/v1/search";
/// Tope de una imagen (propia o descargada).
const MAX_SIZE: u64 = 40 * 1024 * 1024;
const EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

fn dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("wallpapers");
    std::fs::create_dir_all(&dir).map_err(|e| {
        eprintln!("[pulpo] no se pudo crear {}: {e}", dir.display());
        "wallpaper.error.save".to_string()
    })?;
    Ok(dir)
}

/// Solo nombres simples de esa carpeta, con extensión de imagen.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && extension(name).is_some()
}

fn extension(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    EXTENSIONS.into_iter().find(|e| *e == ext)
}

fn mime(name: &str) -> &'static str {
    match extension(name) {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

fn data_url(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let bytes = std::fs::read(path).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{};base64,{data}", mime(name)))
}

fn set_current(app: &AppHandle, name: Option<String>) {
    let state = app.state::<AppState>();
    {
        let mut settings = state.settings.lock().unwrap();
        settings.wallpaper = name;
        if let Err(e) = crate::display::save(&state.settings_path, &settings) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
    let _ = app.emit_to("shell", "wallpaper-changed", ());
}

/// El fondo activo como data URL (`None`: sin fondo, o el archivo ya no está).
#[tauri::command]
pub async fn wallpaper_get(app: AppHandle) -> Option<String> {
    let name = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .wallpaper
        .clone()?;
    let path = dir(&app).ok()?.join(name);
    tauri::async_runtime::spawn_blocking(move || data_url(&path))
        .await
        .ok()
        .flatten()
}

#[derive(Serialize)]
pub struct Saved {
    name: String,
    /// La imagen como data URL, para la miniatura.
    data: String,
    current: bool,
}

/// Fondos guardados, los más recientes primero.
#[tauri::command]
pub async fn wallpaper_list(app: AppHandle) -> Result<Vec<Saved>, String> {
    let dir = dir(&app)?;
    let current = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .wallpaper
        .clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_str().is_some_and(valid_name))
                    .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        files
            .into_iter()
            .filter_map(|(_, path)| {
                let name = path.file_name()?.to_str()?.to_string();
                Some(Saved {
                    current: current.as_deref() == Some(name.as_str()),
                    data: data_url(&path)?,
                    name,
                })
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Usa un fondo guardado (`None`: quita el fondo).
#[tauri::command]
pub fn wallpaper_use(app: AppHandle, name: Option<String>) -> Result<(), String> {
    if let Some(name) = &name {
        if !valid_name(name) || !dir(&app)?.join(name).is_file() {
            return Err("wallpaper.error.missing".into());
        }
    }
    set_current(&app, name);
    Ok(())
}

/// Borra un fondo guardado (si es el activo, se queda sin fondo).
#[tauri::command]
pub fn wallpaper_delete(app: AppHandle, name: String) -> Result<(), String> {
    if !valid_name(&name) {
        return Err("wallpaper.error.missing".into());
    }
    let _ = std::fs::remove_file(dir(&app)?.join(&name));
    let current = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .wallpaper
        .clone();
    if current.as_deref() == Some(name.as_str()) {
        set_current(&app, None);
    }
    Ok(())
}

/// Guarda una imagen propia (en base64, la lee la barra lateral) y la usa.
#[tauri::command]
pub fn wallpaper_import(app: AppHandle, name: String, data: String) -> Result<(), String> {
    let ext = extension(&name).ok_or("wallpaper.error.format")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| "wallpaper.error.format")?;
    if bytes.len() as u64 > MAX_SIZE {
        return Err("wallpaper.error.size".into());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let file = format!("own-{stamp}.{ext}");
    std::fs::write(dir(&app)?.join(&file), bytes).map_err(|e| {
        eprintln!("[pulpo] no se pudo guardar el fondo: {e}");
        "wallpaper.error.save".to_string()
    })?;
    set_current(&app, Some(file));
    Ok(())
}

#[derive(Serialize)]
pub struct Found {
    id: String,
    /// Miniatura (la muestra la barra lateral directamente).
    thumb: String,
    /// Imagen completa.
    full: String,
    resolution: String,
}

#[derive(Serialize)]
pub struct SearchPage {
    items: Vec<Found>,
    last_page: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// Busca en Wallhaven (solo SFW). Sin texto: las más populares.
#[tauri::command]
pub async fn wallpaper_search(
    query: String,
    sorting: String,
    page: u64,
) -> Result<SearchPage, String> {
    let sorting = match sorting.as_str() {
        "random" | "date_added" | "views" | "favorites" => sorting,
        _ => "toplist".to_string(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let page = page.max(1).to_string();
        let mut request = agent()
            .get(WALLHAVEN_API)
            .header("User-Agent", "Pulpo")
            .query("categories", "111")
            .query("purity", "100")
            .query("sorting", &sorting)
            .query("page", &page);
        if !query.trim().is_empty() {
            request = request.query("q", query.trim());
        }
        let text = request
            .call()
            .and_then(|mut r| r.body_mut().read_to_string())
            .map_err(|e| {
                eprintln!("[pulpo] wallhaven: {e}");
                "wallpaper.error.network".to_string()
            })?;
        let body: Value = serde_json::from_str(&text).map_err(|_| "wallpaper.error.network")?;
        let items = body["data"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|w| {
                        Some(Found {
                            id: w["id"].as_str()?.to_string(),
                            thumb: w["thumbs"]["large"].as_str()?.to_string(),
                            full: w["path"].as_str()?.to_string(),
                            resolution: w["resolution"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(SearchPage {
            items,
            last_page: body["meta"]["last_page"].as_u64().unwrap_or(1),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Descarga una imagen de Wallhaven, la guarda y la usa.
#[tauri::command]
pub async fn wallpaper_download(app: AppHandle, id: String, url: String) -> Result<(), String> {
    let parsed = tauri::Url::parse(&url).map_err(|_| "wallpaper.error.network")?;
    let from_wallhaven = parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|h| h == "wallhaven.cc" || h.ends_with(".wallhaven.cc"));
    let ext = extension(parsed.path()).ok_or("wallpaper.error.format")?;
    let id: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if !from_wallhaven || id.is_empty() {
        return Err("wallpaper.error.network".into());
    }
    let file = format!("wallhaven-{id}.{ext}");
    let path = dir(&app)?.join(&file);
    if !path.is_file() {
        let bytes = tauri::async_runtime::spawn_blocking(move || {
            agent()
                .get(url.as_str())
                .header("User-Agent", "Pulpo")
                .call()
                .and_then(|mut r| r.body_mut().with_config().limit(MAX_SIZE).read_to_vec())
                .map_err(|e| {
                    eprintln!("[pulpo] wallhaven: {e}");
                    "wallpaper.error.network".to_string()
                })
        })
        .await
        .map_err(|e| e.to_string())??;
        std::fs::write(&path, bytes).map_err(|e| {
            eprintln!("[pulpo] no se pudo guardar el fondo: {e}");
            "wallpaper.error.save".to_string()
        })?;
    }
    set_current(&app, Some(file));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_image_names() {
        assert!(valid_name("wallhaven-abc123.jpg"));
        assert!(valid_name("own-1.PNG"));
        assert!(!valid_name("../settings.json"));
        assert!(!valid_name("a/b.jpg"));
        assert!(!valid_name(".jpg"));
        assert!(!valid_name("notes.txt"));
    }
}
