//! Descarga y caché de las listas de filtros.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Una lista de filtros. `trusted` habilita los scriptlets `trusted-*`, como
/// hacen uBO y Brave con sus propias listas.
pub struct ListSource {
    pub id: &'static str,
    pub url: &'static str,
    pub trusted: bool,
}

const fn list(id: &'static str, url: &'static str, trusted: bool) -> ListSource {
    ListSource { id, url, trusted }
}

/// Las listas de "Brave Default Adblock/Privacy Filters"
/// (github.com/brave/adblock-resources, filter_lists/list_catalog.json).
/// `filters.txt` de uBO incluye con `!#include` los ficheros por año
/// (filters-2024.txt, …), que se resuelven al descargar.
pub const DEFAULT_LISTS: &[ListSource] = &[
    list("ubo-filters", concat!("https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/", "filters.txt"), true),
    list("ubo-quick-fixes", concat!("https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/", "quick-fixes.txt"), true),
    list("ubo-unbreak", concat!("https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/", "unbreak.txt"), true),
    list("ubo-privacy", concat!("https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/", "privacy.txt"), true),
    list("ubo-badware", concat!("https://raw.githubusercontent.com/uBlockOrigin/uAssets/master/filters/", "badware.txt"), true),
    list("easylist", "https://easylist.to/easylist/easylist.txt", false),
    list("easyprivacy", "https://easylist.to/easylist/easyprivacy.txt", false),
    list("brave-unbreak", "https://raw.githubusercontent.com/brave/adblock-lists/master/brave-unbreak.txt", true),
    list("brave-specific", "https://raw.githubusercontent.com/brave/adblock-lists/master/brave-lists/brave-specific.txt", true),
];

const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LIST_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn load_all(cache_dir: &Path) -> Vec<(String, bool)> {
    let _ = fs::create_dir_all(cache_dir);
    DEFAULT_LISTS
        .iter()
        .filter_map(|l| load_one(l, cache_dir).map(|text| (text, l.trusted)))
        .collect()
}

fn load_one(list: &ListSource, cache_dir: &Path) -> Option<String> {
    let path = cache_dir.join(format!("{}.txt", list.id));
    if is_fresh(&path) {
        if let Ok(text) = fs::read_to_string(&path) {
            return Some(text);
        }
    }
    match download_with_includes(list.url) {
        Ok(text) => {
            let _ = write_atomic(&path, &text);
            Some(text)
        }
        Err(e) => {
            eprintln!("[blocker] {}: {e}; se usa la copia en caché", list.id);
            fs::read_to_string(&path).ok()
        }
    }
}

fn is_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_AGE)
}

fn download(url: &str) -> Result<String, ureq::Error> {
    ureq::get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_LIST_BYTES)
        .read_to_string()
}

/// Descarga una lista y sustituye sus `!#include fichero` (relativos a la
/// misma carpeta) por el contenido del fichero incluido.
fn download_with_includes(url: &str) -> Result<String, ureq::Error> {
    let text = download(url)?;
    let base = &url[..url.rfind('/').map_or(url.len(), |i| i + 1)];
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.strip_prefix("!#include ") {
            Some(file) if !file.contains("://") && !file.contains("..") => {
                match download(&format!("{base}{}", file.trim())) {
                    Ok(included) => out.push_str(&included),
                    Err(e) => eprintln!("[blocker] include {file}: {e}"),
                }
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    Ok(out)
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)
}
