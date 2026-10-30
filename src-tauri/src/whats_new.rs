//! "Novedades": las entradas viven en el frontend (`src/whatsnew.ts`); aquí se
//! recuerda cuáles ya vio el usuario y con qué versión llegaron, en
//! `settings.json` (`whats_new`: id de la entrada → versión). La versión la
//! pone el CI al publicar (`MAJOR.MINOR.<run>`), así que no se sabe al
//! escribir la entrada: se apunta la que la muestra por primera vez.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{display, AppState};

/// Primera vez que se abre Pulpo (no había `services.json`): las novedades
/// de antes no son novedad para quien acaba de instalarlo.
static FIRST_RUN: AtomicBool = AtomicBool::new(false);

pub fn set_first_run(first: bool) {
    FIRST_RUN.store(first, Ordering::Relaxed);
}

#[derive(Serialize)]
pub struct WhatsNew {
    /// Versión en marcha.
    version: String,
    /// Entradas que el usuario aún no había visto (a mostrar ahora).
    fresh: Vec<String>,
    /// Versión con la que llegó cada entrada vista.
    versions: BTreeMap<String, String>,
}

/// Recibe los ids de todas las entradas; las no vistas quedan vistas con la
/// versión actual.
#[tauri::command]
pub fn whats_new_check(app: AppHandle, ids: Vec<String>) -> WhatsNew {
    let version = app.package_info().version.to_string();
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    let fresh: Vec<String> = ids
        .into_iter()
        .filter(|id| !settings.whats_new.contains_key(id))
        .collect();
    for id in &fresh {
        settings.whats_new.insert(id.clone(), version.clone());
    }
    if !fresh.is_empty() {
        if let Err(e) = display::save(&state.settings_path, &settings) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
    WhatsNew {
        fresh: if FIRST_RUN.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            fresh
        },
        versions: settings.whats_new.clone(),
        version,
    }
}
