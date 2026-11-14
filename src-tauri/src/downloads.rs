//! Descargas de todas las apps: dónde se guardan, su progreso y el historial.
//!
//! - Destino (`on_download` de Tauri, en todos los sistemas): la carpeta de
//!   descargas del usuario, con el nombre que propone la página y un sufijo
//!   ` (n)` si ya existe, sin diálogo de guardar.
//! - Progreso (solo CEF): eventos `Browser.downloadWillBegin` y
//!   `Browser.downloadProgress` del DevTools Protocol de cada webview. En
//!   WebView2 una descarga se ve empezar y terminar, sin porcentaje.
//!
//! La lista vive en memoria; las terminadas se guardan en
//! `<datos>/downloads.json` (las últimas `MAX_HISTORY`). El shell recibe la
//! lista entera en `downloads-changed`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

const MAX_HISTORY: usize = 200;
/// Entre dos avisos de progreso al shell (los cambios de estado van siempre).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Progress,
    Done,
    Failed,
    Canceled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    /// App que la inició (id del servicio).
    pub app: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub received: u64,
    /// 0: tamaño desconocido.
    pub total: u64,
    pub state: State,
    /// Segundos Unix.
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    /// Identificador del DevTools Protocol (CEF), para su progreso. No se puede
    /// cancelar por ahí: `Browser.cancelDownload` solo encuentra el contexto
    /// por defecto, y cada app tiene su perfil.
    #[serde(default, skip_serializing)]
    #[cfg_attr(windows, allow(dead_code))]
    guid: Option<String>,
}

pub struct Downloads {
    list: Mutex<Vec<Download>>,
    file: PathBuf,
    seq: AtomicU64,
    last_emit: Mutex<Option<Instant>>,
}

impl Downloads {
    pub fn load(data_dir: &Path) -> Self {
        let file = data_dir.join("downloads.json");
        let list: Vec<Download> = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            list: Mutex::new(list),
            file,
            seq: AtomicU64::new(0),
            last_emit: Mutex::default(),
        }
    }

    fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{n}", now())
    }

    fn save(&self, list: &[Download]) {
        let done: Vec<&Download> = list
            .iter()
            .filter(|d| d.state != State::Progress)
            .take(MAX_HISTORY)
            .collect();
        let result = serde_json::to_vec(&done)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                let tmp = self.file.with_extension("json.tmp");
                std::fs::write(&tmp, json)?;
                std::fs::rename(&tmp, &self.file)
            });
        if let Err(e) = result {
            eprintln!("[pulpo] no se pudo guardar {}: {e}", self.file.display());
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// La lista para el shell, la más reciente primero.
fn snapshot(list: &[Download]) -> Vec<Download> {
    list.to_vec()
}

/// Avisa al shell. `force`: cambió el estado de alguna (no solo bytes).
fn emit(app: &AppHandle, downloads: &Downloads, list: &[Download], force: bool) {
    {
        let mut last = downloads.last_emit.lock().unwrap();
        if !force && last.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    let _ = app.emit_to("shell", "downloads-changed", snapshot(list));
}

/// Nombre de archivo seguro: sin rutas ni caracteres que no valen en Windows.
fn clean_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "download".into()
    } else {
        cleaned
    }
}

/// `dir/name`, o `dir/name (n).ext` si ya existe o lo va a usar otra descarga.
fn unique_path(dir: &Path, name: &str, taken: &[PathBuf]) -> PathBuf {
    let candidate = dir.join(name);
    let free = |p: &Path| !p.exists() && !taken.iter().any(|t| t == p);
    if free(&candidate) {
        return candidate;
    }
    let (stem, ext) = split_ext(name);
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| free(p))
        .expect("siempre hay un nombre libre")
}

/// `archivo.tar.gz` → (`archivo`, `.tar.gz`); `.bashrc` o sin punto: sin extensión.
fn split_ext(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst"] {
        if lower.ends_with(double) && name.len() > double.len() {
            let at = name.len() - double.len();
            return (&name[..at], &name[at..]);
        }
    }
    match name.rfind('.') {
        Some(at) if at > 0 => (&name[..at], &name[at..]),
        _ => (name, ""),
    }
}

fn download_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .download_dir()
        .or_else(|_| app.path().home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// `on_download` → `Requested`: fija el destino (sin diálogo de guardar) y
/// registra la descarga. Devuelve si se permite.
pub fn requested(app: &AppHandle, label: &str, url: &str, destination: &mut PathBuf) -> bool {
    let downloads = app.state::<Downloads>();
    let dir = download_dir(app);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[pulpo] no se pudo crear {}: {e}", dir.display());
    }
    let suggested = destination
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut list = downloads.list.lock().unwrap();
    let taken: Vec<PathBuf> = list
        .iter()
        .filter(|d| d.state == State::Progress)
        .filter_map(|d| d.path.as_ref().map(PathBuf::from))
        .collect();
    // CEF ya la anunció por el DevTools Protocol, con su nombre.
    let announced = list.iter().position(|d| {
        d.state == State::Progress && d.path.is_none() && d.app == label && d.url == url
    });
    let name = clean_name(match (suggested.is_empty(), announced) {
        (false, _) => &suggested,
        (true, Some(i)) => &list[i].name,
        (true, None) => url.rsplit('/').next().unwrap_or_default(),
    });
    let path = unique_path(&dir, &name, &taken);
    *destination = path.clone();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(name);
    match announced {
        Some(i) => {
            list[i].name = file_name;
            list[i].path = Some(path.to_string_lossy().into_owned());
        }
        None => {
            let id = downloads.next_id();
            list.insert(
                0,
                Download {
                    id,
                    app: label.to_string(),
                    name: file_name,
                    url: url.to_string(),
                    path: Some(path.to_string_lossy().into_owned()),
                    received: 0,
                    total: 0,
                    state: State::Progress,
                    started: now(),
                    finished: None,
                    guid: None,
                },
            );
        }
    }
    emit(app, &downloads, &list, true);
    true
}

/// `on_download` → `Finished`.
pub fn finished(app: &AppHandle, label: &str, url: &str, path: Option<&Path>, success: bool) {
    let downloads = app.state::<Downloads>();
    let mut list = downloads.list.lock().unwrap();
    let Some(d) = list
        .iter_mut()
        .find(|d| d.state == State::Progress && d.app == label && d.url == url)
    else {
        return;
    };
    finish(d, if success { State::Done } else { State::Failed });
    if let Some(path) = path {
        d.path = Some(path.to_string_lossy().into_owned());
    }
    downloads.save(&list);
    emit(app, &downloads, &list, true);
}

fn finish(d: &mut Download, state: State) {
    // CEF manda `canceled` y después `Finished` sin éxito: queda cancelada.
    if d.state != State::Progress {
        return;
    }
    d.state = state;
    d.finished = Some(now());
    if state == State::Done {
        if let Some(size) = d.path.as_ref().and_then(|p| std::fs::metadata(p).ok()) {
            d.received = size.len();
        }
        d.total = d.received;
    }
}

/// Evento del DevTools Protocol de la app `label` (solo CEF). Ignora los que
/// no son de descargas sin parsearlos.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn devtools_message(app: &AppHandle, label: &str, raw: &[u8]) {
    if !raw.starts_with(br#"{"method":"Browser.download"#) {
        return;
    }
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return;
    };
    let params = &message["params"];
    let text = |key: &str| params[key].as_str().unwrap_or_default().to_string();
    let guid = text("guid");
    if guid.is_empty() {
        return;
    }
    let downloads = app.state::<Downloads>();
    let mut list = downloads.list.lock().unwrap();
    match message["method"].as_str() {
        Some("Browser.downloadWillBegin") => {
            let url = text("url");
            // `on_download` pudo llegar antes y ya la registró.
            if let Some(d) = list.iter_mut().find(|d| {
                d.state == State::Progress && d.guid.is_none() && d.app == label && d.url == url
            }) {
                d.guid = Some(guid);
            } else {
                let id = downloads.next_id();
                list.insert(
                    0,
                    Download {
                        id,
                        app: label.to_string(),
                        name: clean_name(&text("suggestedFilename")),
                        url,
                        path: None,
                        received: 0,
                        total: 0,
                        state: State::Progress,
                        started: now(),
                        finished: None,
                        guid: Some(guid),
                    },
                );
            }
            emit(app, &downloads, &list, true);
        }
        Some("Browser.downloadProgress") => {
            let Some(d) = list
                .iter_mut()
                .find(|d| d.guid.as_deref() == Some(guid.as_str()))
            else {
                return;
            };
            if d.state != State::Progress {
                return;
            }
            d.received = params["receivedBytes"].as_f64().unwrap_or_default() as u64;
            d.total = params["totalBytes"].as_f64().unwrap_or_default() as u64;
            let changed = match params["state"].as_str() {
                Some("completed") => {
                    if let Some(path) = params["filePath"].as_str().filter(|p| !p.is_empty()) {
                        d.path = Some(path.to_string());
                    }
                    finish(d, State::Done);
                    true
                }
                Some("canceled") => {
                    finish(d, State::Canceled);
                    true
                }
                _ => false,
            };
            if changed {
                downloads.save(&list);
            }
            emit(app, &downloads, &list, changed);
        }
        _ => {}
    }
}

#[tauri::command]
pub fn downloads_list(app: AppHandle) -> Vec<Download> {
    snapshot(&app.state::<Downloads>().list.lock().unwrap())
}

fn path_of(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    let downloads = app.state::<Downloads>();
    let list = downloads.list.lock().unwrap();
    let path = list
        .iter()
        .find(|d| d.id == id)
        .and_then(|d| d.path.clone())
        .map(PathBuf::from)
        .ok_or("downloads.error.missing")?;
    if path.exists() {
        Ok(path)
    } else {
        Err("downloads.error.missing".into())
    }
}

#[tauri::command]
pub fn download_open(app: AppHandle, id: String) -> Result<(), String> {
    let path = path_of(&app, &id)?;
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn download_show(app: AppHandle, id: String) -> Result<(), String> {
    let path = path_of(&app, &id)?;
    tauri_plugin_opener::reveal_item_in_dir(&path).map_err(|e| e.to_string())
}

/// Abre la carpeta de descargas.
#[tauri::command]
pub fn downloads_folder(app: AppHandle) -> Result<(), String> {
    let dir = download_dir(&app);
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Quita una terminada de la lista (el archivo se queda).
#[tauri::command]
pub fn download_remove(app: AppHandle, id: String) {
    let downloads = app.state::<Downloads>();
    let mut list = downloads.list.lock().unwrap();
    list.retain(|d| d.id != id || d.state == State::Progress);
    downloads.save(&list);
    emit(&app, &downloads, &list, true);
}

/// Vacía el historial (las que siguen en curso se quedan).
#[tauri::command]
pub fn downloads_clear(app: AppHandle) {
    let downloads = app.state::<Downloads>();
    let mut list = downloads.list.lock().unwrap();
    list.retain(|d| d.state == State::Progress);
    downloads.save(&list);
    emit(&app, &downloads, &list, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_names() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("a\\b\\c.txt"), "c.txt");
        assert_eq!(clean_name("re:port?.pdf"), "re_port_.pdf");
        assert_eq!(clean_name(""), "download");
        assert_eq!(clean_name(".."), "download");
        assert_eq!(clean_name(".hidden"), "hidden");
    }

    #[test]
    fn splits_extensions() {
        assert_eq!(split_ext("foto.jpg"), ("foto", ".jpg"));
        assert_eq!(split_ext("app.tar.gz"), ("app", ".tar.gz"));
        assert_eq!(split_ext("README"), ("README", ""));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
    }

    #[test]
    fn picks_free_names() {
        let dir = std::env::temp_dir().join(format!("pulpo-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        let taken = vec![dir.join("a (1).txt")];
        assert_eq!(unique_path(&dir, "b.txt", &taken), dir.join("b.txt"));
        assert_eq!(unique_path(&dir, "a.txt", &taken), dir.join("a (2).txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
