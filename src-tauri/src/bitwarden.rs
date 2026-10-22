//! Bitwarden: rellena usuario y contraseña en la app activa con el CLI oficial
//! (`bw`), que ya guarda la bóveda cifrada y sabe con qué entradas casa una URL.
//!
//! - El primer `bw login` se hace en una terminal (2FA y verificación de
//!   dispositivo nuevo son preguntas interactivas del CLI). Pulpo solo
//!   desbloquea: `bw unlock` da una clave de sesión que vive en memoria hasta
//!   que se bloquea o se cierra Pulpo; viaja a `bw` por `BW_SESSION`, nunca en
//!   los argumentos (se verían en `ps`).
//! - Las entradas se buscan por la URL real del webview (no la que diga la
//!   página). Al shell solo llegan nombre y usuario; la contraseña va de Rust
//!   directo a la página, y solo si sigue en el mismo sitio que al buscar.
//! - Se pide desde el shell (botón de la barra lateral) o con Ctrl+Mayús+L
//!   dentro de una app (`shortcuts::Shortcut::Fill`). Solo el documento
//!   principal: un formulario dentro de un iframe no se rellena.

use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Default)]
pub struct Vault {
    session: Mutex<Option<String>>,
    /// Las entradas de la última búsqueda, con contraseña: para rellenar la
    /// elegida sin volver a llamar a `bw`.
    found: Mutex<Option<Found>>,
}

struct Found {
    service: String,
    url: String,
    logins: Vec<Login>,
}

struct Login {
    id: String,
    username: String,
    password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// `bw` no está instalado.
    Missing,
    /// Falta `bw login` en una terminal.
    Unauthenticated,
    Locked,
    Unlocked,
}

#[derive(Serialize)]
pub struct Status {
    state: State,
    email: Option<String>,
}

/// Lo que el shell ve de una entrada.
#[derive(Serialize)]
pub struct Entry {
    id: String,
    name: String,
    username: String,
}

#[derive(Deserialize)]
struct BwStatus {
    status: String,
    #[serde(rename = "userEmail")]
    user_email: Option<String>,
}

#[derive(Deserialize)]
struct BwItem {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: u8,
    login: Option<BwLogin>,
}

#[derive(Deserialize)]
struct BwLogin {
    username: Option<String>,
    password: Option<String>,
}

/// Ejecuta `bw` con la sesión (si la hay) y devuelve la salida estándar.
fn bw(args: &[&str], session: Option<&str>, password: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("bw");
    command
        .args(args)
        .arg("--nointeraction")
        .stdin(Stdio::null())
        .env("BW_NOINTERACTION", "true");
    if let Some(session) = session {
        command.env("BW_SESSION", session);
    }
    if let Some(password) = password {
        command.env("PULPO_BW_PASSWORD", password);
    }
    let output = command.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "bitwarden.error.missing".to_string()
        } else {
            eprintln!("[pulpo] bitwarden: no se pudo ejecutar bw: {e}");
            "bitwarden.error.cli".to_string()
        }
    })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        eprintln!("[pulpo] bitwarden: bw {} falló: {}", args[0], err.trim());
        return Err(if err.contains("Invalid master password") {
            "bitwarden.error.password"
        } else if err.contains("not logged in") || err.contains("You are not logged in") {
            "bitwarden.error.unauthenticated"
        } else if err.contains("locked") {
            "bitwarden.error.locked"
        } else {
            "bitwarden.error.cli"
        }
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn session(app: &AppHandle) -> Option<String> {
    app.state::<Vault>().session.lock().unwrap().clone()
}

fn status_blocking(app: &AppHandle) -> Result<Status, String> {
    let raw = match bw(&["status"], session(app).as_deref(), None) {
        Err(e) if e == "bitwarden.error.missing" => {
            return Ok(Status {
                state: State::Missing,
                email: None,
            })
        }
        other => other?,
    };
    let status: BwStatus = serde_json::from_str(&raw).map_err(|e| {
        eprintln!("[pulpo] bitwarden: estado ilegible: {e}");
        "bitwarden.error.cli".to_string()
    })?;
    let state = match status.status.as_str() {
        "unauthenticated" => State::Unauthenticated,
        // Con una sesión que ya no vale (`bw lock` en una terminal), `bw` dice
        // "locked": se olvida.
        "unlocked" => State::Unlocked,
        _ => {
            app.state::<Vault>().session.lock().unwrap().take();
            State::Locked
        }
    };
    Ok(Status {
        state,
        email: status.user_email,
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| "bitwarden.error.cli".to_string())?
}

/// Si el usuario lo activó en Gestionar apps.
pub fn enabled(app: &AppHandle) -> bool {
    app.state::<crate::AppState>()
        .settings
        .lock()
        .unwrap()
        .bitwarden
}

#[tauri::command]
pub fn bw_enabled(app: AppHandle) -> bool {
    enabled(&app)
}

/// Muestra u oculta Bitwarden. Al apagarlo se olvida la sesión.
#[tauri::command]
pub fn bw_set_enabled(app: AppHandle, enabled: bool) {
    let state = app.state::<crate::AppState>();
    {
        let mut settings = state.settings.lock().unwrap();
        settings.bitwarden = enabled;
        if let Err(e) = crate::display::save(&state.settings_path, &settings) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
    if !enabled {
        let vault = app.state::<Vault>();
        vault.found.lock().unwrap().take();
        let session = vault.session.lock().unwrap().take();
        if let Some(key) = session {
            std::thread::spawn(move || {
                let _ = bw(&["lock"], Some(&key), None);
            });
        }
    }
    let _ = app.emit_to("shell", "bw-enabled", enabled);
}

#[tauri::command]
pub async fn bw_status(app: AppHandle) -> Result<Status, String> {
    blocking(move || status_blocking(&app)).await
}

#[tauri::command]
pub async fn bw_unlock(app: AppHandle, password: String) -> Result<(), String> {
    if password.is_empty() {
        return Err("bitwarden.error.password".into());
    }
    blocking(move || {
        let key = bw(
            &["unlock", "--raw", "--passwordenv", "PULPO_BW_PASSWORD"],
            None,
            Some(&password),
        )?;
        if key.is_empty() {
            return Err("bitwarden.error.cli".into());
        }
        *app.state::<Vault>().session.lock().unwrap() = Some(key.clone());
        // Trae lo último del servidor sin hacer esperar al relleno.
        std::thread::spawn(move || {
            if let Err(e) = bw(&["sync"], Some(&key), None) {
                eprintln!("[pulpo] bitwarden: sync falló: {e}");
            }
        });
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn bw_lock(app: AppHandle) -> Result<(), String> {
    let vault = app.state::<Vault>();
    vault.found.lock().unwrap().take();
    let Some(key) = vault.session.lock().unwrap().take() else {
        return Ok(());
    };
    blocking(move || bw(&["lock"], Some(&key), None).map(|_| ())).await
}

/// URL actual del webview de la app.
fn service_url(app: &AppHandle, service: &str) -> Result<String, String> {
    let webview = app
        .get_webview(service)
        .ok_or("error.unknownService".to_string())?;
    let url = webview
        .url()
        .map_err(|_| "error.unknownService".to_string())?;
    match url.scheme() {
        "https" | "http" => Ok(url.to_string()),
        _ => Err("bitwarden.error.noLogins".into()),
    }
}

/// Las entradas de inicio de sesión que casan con la página abierta en la app.
#[tauri::command]
pub async fn bw_logins(app: AppHandle, service: String) -> Result<Vec<Entry>, String> {
    if !enabled(&app) {
        return Err("bitwarden.error.disabled".into());
    }
    let url = service_url(&app, &service)?;
    blocking(move || {
        let key = session(&app).ok_or("bitwarden.error.locked")?;
        let raw = bw(&["list", "items", "--url", &url], Some(&key), None)?;
        let items: Vec<BwItem> = serde_json::from_str(&raw).map_err(|e| {
            eprintln!("[pulpo] bitwarden: lista ilegible: {e}");
            "bitwarden.error.cli".to_string()
        })?;
        let mut entries = Vec::new();
        let mut logins = Vec::new();
        for item in items {
            // 1 = inicio de sesión (2 notas, 3 tarjetas, 4 identidades).
            let (1, Some(login)) = (item.kind, item.login) else {
                continue;
            };
            let username = login.username.unwrap_or_default();
            entries.push(Entry {
                id: item.id.clone(),
                name: item.name,
                username: username.clone(),
            });
            logins.push(Login {
                id: item.id,
                username,
                password: login.password.unwrap_or_default(),
            });
        }
        *app.state::<Vault>().found.lock().unwrap() = Some(Found {
            service,
            url,
            logins,
        });
        Ok(entries)
    })
    .await
}

/// Mismo sitio: esquema, host y puerto. La contraseña no se manda a otro.
fn same_site(a: &str, b: &str) -> bool {
    match (tauri::Url::parse(a), tauri::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// Rellena en la app la entrada elegida de la última búsqueda.
#[tauri::command]
pub fn bw_fill(app: AppHandle, service: String, id: String) -> Result<(), String> {
    let found = app.state::<Vault>().found.lock().unwrap().take();
    let Some(found) = found.filter(|f| f.service == service) else {
        return Err("bitwarden.error.changed".into());
    };
    // La página pudo navegar mientras se elegía.
    if !same_site(&found.url, &service_url(&app, &service)?) {
        return Err("bitwarden.error.changed".into());
    }
    let login = found
        .logins
        .into_iter()
        .find(|l| l.id == id)
        .ok_or("bitwarden.error.changed")?;
    let webview = app
        .get_webview(&service)
        .ok_or("error.unknownService".to_string())?;
    let script = format!(
        "{FILL_SCRIPT}({}, {})",
        serde_json::to_string(&login.username).unwrap(),
        serde_json::to_string(&login.password).unwrap(),
    );
    webview
        .eval(script)
        .map_err(|_| "bitwarden.error.cli".to_string())
}

/// Rellena el formulario de inicio de sesión visible: la contraseña y el campo
/// de usuario anterior a ella; sin contraseña a la vista (logins en dos pasos,
/// como Google o Microsoft), solo el usuario. El valor se pone con el `setter`
/// nativo y se avisa con `input`/`change`, para que React y compañía lo vean.
const FILL_SCRIPT: &str = r#"((username, password) => {
  const visible = (el) =>
    !el.disabled && !el.readOnly && el.getClientRects().length > 0 &&
    getComputedStyle(el).visibility !== "hidden";
  const textual = (el) => ["text", "email", "tel"].includes(el.type);
  const set = (el, value) => {
    el.focus();
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const inputs = [...document.querySelectorAll("input")].filter(visible);
  const pass = inputs.find((el) => el.type === "password");
  const named = (el) => el.autocomplete === "username" ||
    /user|mail|login|account|identifier|phone|name/i.test(`${el.name} ${el.id} ${el.autocomplete} ${el.placeholder}`);
  const candidates = (pass ? inputs.slice(0, inputs.indexOf(pass)) : inputs).filter(textual);
  const user = pass
    ? candidates.findLast(named) ?? candidates.at(-1)
    : candidates.find(named) ?? candidates[0];
  if (user && username) set(user, username);
  if (pass && password) set(pass, password);
  (pass ?? user)?.focus();
})"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_site_compares_origins() {
        assert!(same_site(
            "https://web.telegram.org/a/",
            "https://web.telegram.org/k/#@chat"
        ));
        assert!(!same_site(
            "https://web.telegram.org/a/",
            "https://evil.org/web.telegram.org"
        ));
        assert!(!same_site(
            "https://accounts.example.com/",
            "http://accounts.example.com/"
        ));
        assert!(!same_site("about:blank", "about:blank"));
    }

    #[test]
    fn parses_items() {
        let raw = r#"[{"id":"a","name":"GitHub","type":1,"login":{"username":"me","password":"pw","uris":[]}},
            {"id":"b","name":"Nota","type":2,"login":null}]"#;
        let items: Vec<BwItem> = serde_json::from_str(raw).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].login.as_ref().unwrap().password.as_deref(),
            Some("pw")
        );
        assert!(items[1].login.is_none());
    }
}
