//! CI: los runs de GitHub Actions (build, tests, deploy…) de los repos que el
//! usuario elige, vistos desde la barra lateral.
//!
//! - Acceso con tokens personales de GitHub (fine-grained, "Actions: read"), uno
//!   por dueño de repos (usuario u organización). Se guardan en el llavero del
//!   sistema (Secret Service en Linux, Credential Manager en Windows), nunca en
//!   `settings.json` ni en la barra lateral. En `settings.json` solo van los
//!   logins y los repos elegidos.
//! - Un hilo consulta `actions/runs` de cada repo: cada 20 s si tiene runs en
//!   marcha, cada 60 s si no. Con `If-None-Match`: las respuestas 304 no gastan
//!   cuota de la API.
//! - Se muestran los runs en marcha. Al terminar uno, la barra lateral avisa
//!   (`ci-finished`) y sigue a la vista 10 minutos. Los que ya estaban
//!   terminados cuando Pulpo los vio por primera vez no se muestran.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

const API: &str = "https://api.github.com";
const USER_AGENT: &str = "Pulpo";
/// Entrada del llavero: servicio fijo, usuario = login de GitHub.
const KEYRING_SERVICE: &str = "com.pulpo.app.ci";
/// Un run terminado sigue en la lista este tiempo.
const LINGER: Duration = Duration::from_secs(10 * 60);
/// Repo con runs en marcha.
const ACTIVE_INTERVAL: Duration = Duration::from_secs(20);
/// Repo sin runs en marcha.
const IDLE_INTERVAL: Duration = Duration::from_secs(60);
/// Repo que dio error (sin acceso, borrado, límite de la API…).
const ERROR_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Cada cuánto se despierta el hilo a ver qué repo toca.
const TICK: Duration = Duration::from_secs(5);

/// Lo que se guarda en `settings.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CiSettings {
    /// Botón CI en la barra lateral (y consultas a GitHub). Oculto por defecto.
    #[serde(default)]
    pub enabled: bool,
    /// Logins con token en el llavero, en el orden en que se añadieron.
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub repos: Vec<WatchedRepo>,
}

impl CiSettings {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.accounts.is_empty() && self.repos.is_empty()
    }
}

/// Repo vigilado (`dueño/nombre`) y la cuenta cuyo token lo lee.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchedRepo {
    pub repo: String,
    pub account: String,
}

#[derive(Clone, Serialize)]
pub struct Account {
    login: String,
    /// `false` si GitHub rechazó el token (caducado o revocado).
    valid: bool,
}

/// Repo al que llega un token, para elegir cuáles vigilar.
#[derive(Clone, Serialize)]
pub struct RepoInfo {
    repo: String,
    account: String,
    private: bool,
    watched: bool,
}

/// Un run de un workflow, como lo ve la barra lateral.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Run {
    id: u64,
    repo: String,
    workflow: String,
    branch: Option<String>,
    /// Título del commit o del PR.
    title: String,
    /// `queued`, `in_progress`, `waiting` (espera aprobación), `completed`…
    status: String,
    /// Con `completed`: `success`, `failure`, `cancelled`…
    conclusion: Option<String>,
    url: String,
    /// ISO 8601, como los da GitHub.
    started_at: Option<String>,
    updated_at: Option<String>,
}

impl Run {
    fn from_api(repo: &str, value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        Some(Run {
            id: value["id"].as_u64()?,
            repo: repo.to_string(),
            workflow: text("name").unwrap_or_default(),
            branch: text("head_branch"),
            title: text("display_title").unwrap_or_default(),
            status: text("status")?,
            conclusion: text("conclusion"),
            url: text("html_url")?,
            started_at: text("run_started_at"),
            updated_at: text("updated_at"),
        })
    }

    fn finished(&self) -> bool {
        self.status == "completed"
    }
}

struct Tracked {
    run: Run,
    /// Cuándo se vio terminado (se quita `LINGER` después).
    finished_at: Option<Instant>,
}

#[derive(Default)]
struct RepoPoll {
    etag: Option<String>,
    next: Option<Instant>,
    /// Ids de la última página de runs (`None`: aún no se consultó). Un run
    /// terminado que no estaba aquí empezó y acabó entre dos consultas.
    known: Option<BTreeSet<u64>>,
}

/// Estado del módulo (`app.manage`).
pub struct Ci {
    runs: Mutex<BTreeMap<u64, Tracked>>,
    repos: Mutex<HashMap<String, RepoPoll>>,
    /// Tokens leídos del llavero (para no ir a él en cada consulta).
    tokens: Mutex<HashMap<String, String>>,
    invalid: Mutex<BTreeSet<String>>,
    wake: Mutex<Sender<()>>,
    wake_rx: Mutex<Option<Receiver<()>>>,
}

impl Default for Ci {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Ci {
            runs: Mutex::default(),
            repos: Mutex::default(),
            tokens: Mutex::default(),
            invalid: Mutex::default(),
            wake: Mutex::new(tx),
            wake_rx: Mutex::new(Some(rx)),
        }
    }
}

// ---------------------------------------------------------------- llavero

fn keyring_entry(login: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, login).map_err(|e| {
        eprintln!("[pulpo] llavero no disponible: {e}");
        "ci.error.keyring".to_string()
    })
}

fn token(app: &AppHandle, login: &str) -> Option<String> {
    let ci = app.state::<Ci>();
    if let Some(token) = ci.tokens.lock().unwrap().get(login) {
        return Some(token.clone());
    }
    let token = keyring_entry(login).ok()?.get_password().ok()?;
    ci.tokens
        .lock()
        .unwrap()
        .insert(login.to_string(), token.clone());
    Some(token)
}

// ---------------------------------------------------------------- API

struct Response {
    status: u16,
    etag: Option<String>,
    next_page: Option<String>,
    body: String,
}

fn get(token: &str, url: &str, etag: Option<&str>) -> Result<Response, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("Authorization", &format!("Bearer {token}"));
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = request.call().map_err(|e| e.to_string())?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let etag = header("etag");
    let next_page = header("link").as_deref().and_then(next_link);
    let status = response.status().as_u16();
    let body = if status == 200 {
        response
            .body_mut()
            .with_config()
            .limit(20 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| e.to_string())?
    } else {
        String::new()
    };
    Ok(Response {
        status,
        etag,
        next_page,
        body,
    })
}

/// La página siguiente de una cabecera `Link` de GitHub.
fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

// ---------------------------------------------------------------- comandos

fn save_settings(app: &AppHandle, change: impl FnOnce(&mut CiSettings)) {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    change(&mut settings.ci);
    if let Err(e) = crate::display::save(&state.settings_path, &settings) {
        eprintln!(
            "[pulpo] no se pudo guardar {}: {e}",
            state.settings_path.display()
        );
    }
}

fn settings(app: &AppHandle) -> CiSettings {
    app.state::<AppState>().settings.lock().unwrap().ci.clone()
}

fn accounts(app: &AppHandle) -> Vec<Account> {
    let invalid = app.state::<Ci>().invalid.lock().unwrap().clone();
    settings(app)
        .accounts
        .into_iter()
        .map(|login| Account {
            valid: !invalid.contains(&login),
            login,
        })
        .collect()
}

fn emit_accounts(app: &AppHandle) {
    let _ = app.emit_to("shell", "ci-accounts", accounts(app));
}

fn emit_runs(app: &AppHandle) {
    let _ = app.emit_to("shell", "ci-changed", runs(app));
}

fn runs(app: &AppHandle) -> Vec<Run> {
    let mut runs: Vec<Run> = app
        .state::<Ci>()
        .runs
        .lock()
        .unwrap()
        .values()
        .map(|t| t.run.clone())
        .collect();
    // En marcha primero; dentro de cada parte, lo más reciente arriba.
    runs.sort_by(|a, b| {
        a.finished()
            .cmp(&b.finished())
            .then_with(|| b.started_at.cmp(&a.started_at))
    });
    runs
}

#[tauri::command]
pub fn ci_accounts(app: AppHandle) -> Vec<Account> {
    accounts(&app)
}

#[tauri::command]
pub fn ci_runs(app: AppHandle) -> Vec<Run> {
    runs(&app)
}

/// Comprueba el token con GitHub, lo guarda en el llavero y devuelve su login.
#[tauri::command]
pub async fn ci_add_token(app: AppHandle, token: String) -> Result<String, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("ci.error.emptyToken".into());
    }
    let login = tauri::async_runtime::spawn_blocking({
        let token = token.clone();
        move || -> Result<String, String> {
            let response = get(&token, &format!("{API}/user"), None).map_err(|e| {
                eprintln!("[pulpo] ci: GitHub no responde: {e}");
                "ci.error.network".to_string()
            })?;
            if response.status == 401 {
                return Err("ci.error.badToken".into());
            }
            if response.status != 200 {
                eprintln!("[pulpo] ci: /user respondió {}", response.status);
                return Err("ci.error.network".into());
            }
            let user: Value =
                serde_json::from_str(&response.body).map_err(|_| "ci.error.network")?;
            let login = user["login"].as_str().ok_or("ci.error.network")?;
            keyring_entry(login)?.set_password(&token).map_err(|e| {
                eprintln!("[pulpo] no se pudo guardar el token en el llavero: {e}");
                "ci.error.keyring".to_string()
            })?;
            Ok(login.to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    let ci = app.state::<Ci>();
    ci.tokens.lock().unwrap().insert(login.clone(), token);
    ci.invalid.lock().unwrap().remove(&login);
    save_settings(&app, |s| {
        if !s.accounts.contains(&login) {
            s.accounts.push(login.clone());
        }
    });
    emit_accounts(&app);
    Ok(login)
}

/// Quita la cuenta, su token del llavero y los repos que leía.
#[tauri::command]
pub async fn ci_remove_account(app: AppHandle, login: String) -> Result<(), String> {
    {
        let login = login.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Ok(entry) = keyring_entry(&login) {
                if let Err(e) = entry.delete_credential() {
                    eprintln!("[pulpo] no se pudo borrar el token de {login}: {e}");
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    let ci = app.state::<Ci>();
    ci.tokens.lock().unwrap().remove(&login);
    ci.invalid.lock().unwrap().remove(&login);
    save_settings(&app, |s| {
        s.accounts.retain(|a| *a != login);
        s.repos.retain(|r| r.account != login);
    });
    forget_unwatched(&app);
    emit_accounts(&app);
    emit_runs(&app);
    Ok(())
}

/// Repos a los que llegan los tokens, los vigilados primero.
#[tauri::command]
pub async fn ci_list_repos(app: AppHandle) -> Result<Vec<RepoInfo>, String> {
    let settings = settings(&app);
    let tokens: Vec<(String, String)> = settings
        .accounts
        .iter()
        .filter_map(|login| Some((login.clone(), token(&app, login)?)))
        .collect();
    let handle = app.clone();
    let mut repos = tauri::async_runtime::spawn_blocking(move || {
        let mut repos: Vec<RepoInfo> = Vec::new();
        let mut failed = false;
        for (login, token) in tokens {
            match list_repos(&token, &login) {
                Ok(found) => {
                    for repo in found {
                        if !repos.iter().any(|r| r.repo == repo.repo) {
                            repos.push(repo);
                        }
                    }
                }
                Err(Some(Unauthorized)) => mark_invalid(&handle, &login),
                Err(_) => failed = true,
            }
        }
        if failed && repos.is_empty() {
            return Err("ci.error.network".to_string());
        }
        Ok(repos)
    })
    .await
    .map_err(|e| e.to_string())??;
    for repo in &mut repos {
        repo.watched = settings.repos.iter().any(|w| w.repo == repo.repo);
    }
    // Los vigilados que el token ya no ve siguen en la lista, para poder quitarlos.
    for watched in &settings.repos {
        if !repos.iter().any(|r| r.repo == watched.repo) {
            repos.push(RepoInfo {
                repo: watched.repo.clone(),
                account: watched.account.clone(),
                private: false,
                watched: true,
            });
        }
    }
    repos.sort_by_key(|r| !r.watched);
    Ok(repos)
}

struct Unauthorized;

fn list_repos(token: &str, login: &str) -> Result<Vec<RepoInfo>, Option<Unauthorized>> {
    let mut url = Some(format!("{API}/user/repos?per_page=100&sort=pushed"));
    let mut repos = Vec::new();
    // Hasta 1000 repos.
    for _ in 0..10 {
        let Some(page) = url.take() else { break };
        let response = get(token, &page, None).map_err(|e| {
            eprintln!("[pulpo] ci: no se pudo listar los repos de {login}: {e}");
            None
        })?;
        match response.status {
            200 => {}
            401 => return Err(Some(Unauthorized)),
            status => {
                eprintln!("[pulpo] ci: /user/repos de {login} respondió {status}");
                return Err(None);
            }
        }
        let list: Vec<Value> = serde_json::from_str(&response.body).map_err(|_| None)?;
        repos.extend(list.iter().filter_map(|r| {
            Some(RepoInfo {
                repo: r["full_name"].as_str()?.to_string(),
                account: login.to_string(),
                private: r["private"].as_bool().unwrap_or(false),
                watched: false,
            })
        }));
        url = response.next_page;
    }
    Ok(repos)
}

/// Elige qué repos vigilar.
#[tauri::command]
pub fn ci_set_repos(app: AppHandle, repos: Vec<WatchedRepo>) {
    let accounts = settings(&app).accounts;
    let mut chosen: Vec<WatchedRepo> = Vec::new();
    for repo in repos {
        if accounts.contains(&repo.account) && !chosen.iter().any(|r| r.repo == repo.repo) {
            chosen.push(repo);
        }
    }
    save_settings(&app, |s| s.repos = chosen);
    forget_unwatched(&app);
    emit_runs(&app);
    let _ = app.state::<Ci>().wake.lock().unwrap().send(());
}

#[tauri::command]
pub fn ci_enabled(app: AppHandle) -> bool {
    settings(&app).enabled
}

/// Muestra u oculta CI. Apagado no consulta GitHub; las cuentas y los repos
/// se conservan. Al volver a encenderlo se empieza de cero (lo ya terminado
/// no avisa).
#[tauri::command]
pub fn ci_set_enabled(app: AppHandle, enabled: bool) {
    save_settings(&app, |s| s.enabled = enabled);
    let ci = app.state::<Ci>();
    if !enabled {
        ci.runs.lock().unwrap().clear();
        ci.repos.lock().unwrap().clear();
    }
    let _ = app.emit_to("shell", "ci-enabled", enabled);
    emit_runs(&app);
    let _ = ci.wake.lock().unwrap().send(());
}

/// Abre en el navegador la página de un run (o la de crear tokens).
#[tauri::command]
pub fn ci_open(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !url.starts_with("https://github.com/") {
        return Err("ci.error.url".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Muestra el aviso de un run terminado. El texto lo arma la barra lateral
/// (conoce el idioma); un clic abre el run en el navegador.
#[tauri::command]
pub fn ci_notify(app: AppHandle, summary: String, body: String, url: String) {
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(&summary).body(&body).appname("Pulpo");
        #[cfg(target_os = "linux")]
        {
            notification.action("default", "Open");
            match notification.show() {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action == "default" {
                        let _ = ci_open(app, url);
                    }
                }),
                Err(e) => eprintln!("[pulpo] ci: no se pudo mostrar el aviso: {e}"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (app, url);
            if let Err(e) = notification.show() {
                eprintln!("[pulpo] ci: no se pudo mostrar el aviso: {e}");
            }
        }
    });
}

// ---------------------------------------------------------------- sondeo

fn mark_invalid(app: &AppHandle, login: &str) {
    let ci = app.state::<Ci>();
    if ci.invalid.lock().unwrap().insert(login.to_string()) {
        ci.tokens.lock().unwrap().remove(login);
        eprintln!("[pulpo] ci: GitHub rechazó el token de {login}");
        emit_accounts(app);
    }
}

/// Olvida los runs y el estado de los repos que ya no se vigilan.
fn forget_unwatched(app: &AppHandle) {
    let watched: BTreeSet<String> = settings(app).repos.into_iter().map(|r| r.repo).collect();
    let ci = app.state::<Ci>();
    ci.runs
        .lock()
        .unwrap()
        .retain(|_, t| watched.contains(&t.run.repo));
    ci.repos.lock().unwrap().retain(|r, _| watched.contains(r));
}

/// Consulta en segundo plano mientras dure la app.
pub fn watch(app: &AppHandle) {
    let app = app.clone();
    let Some(wake) = app.state::<Ci>().wake_rx.lock().unwrap().take() else {
        return;
    };
    std::thread::spawn(move || loop {
        poll_due(&app);
        if expire(&app.state::<Ci>(), Instant::now()) {
            emit_runs(&app);
        }
        let _ = wake.recv_timeout(TICK);
    });
}

/// Quita los runs terminados hace más de `LINGER`. `true` si quitó alguno.
fn expire(ci: &Ci, now: Instant) -> bool {
    let mut runs = ci.runs.lock().unwrap();
    let before = runs.len();
    runs.retain(|_, t| {
        t.finished_at
            .is_none_or(|at| now.duration_since(at) < LINGER)
    });
    runs.len() != before
}

fn poll_due(app: &AppHandle) {
    let settings = settings(app);
    if !settings.enabled {
        return;
    }
    let ci = app.state::<Ci>();
    let invalid = ci.invalid.lock().unwrap().clone();
    let now = Instant::now();
    for watched in settings.repos {
        if invalid.contains(&watched.account) {
            continue;
        }
        let (due, etag) = {
            let repos = ci.repos.lock().unwrap();
            let poll = repos.get(&watched.repo);
            (
                poll.and_then(|p| p.next).is_none_or(|next| next <= now),
                poll.and_then(|p| p.etag.clone()),
            )
        };
        if !due {
            continue;
        }
        let Some(token) = token(app, &watched.account) else {
            continue;
        };
        poll_repo(app, &watched, &token, etag);
    }
}

fn poll_repo(app: &AppHandle, watched: &WatchedRepo, token: &str, etag: Option<String>) {
    let url = format!("{API}/repos/{}/actions/runs?per_page=20", watched.repo);
    let response = get(token, &url, etag.as_deref());
    let ci = app.state::<Ci>();
    let mut next = Instant::now() + IDLE_INTERVAL;
    match response {
        Ok(r) if r.status == 304 => {}
        Ok(r) if r.status == 200 => {
            let list: Vec<Run> = serde_json::from_str::<Value>(&r.body)
                .ok()
                .and_then(|v| v["workflow_runs"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|v| Run::from_api(&watched.repo, v))
                .collect();
            let known = ci
                .repos
                .lock()
                .unwrap()
                .get(&watched.repo)
                .and_then(|p| p.known.clone());
            let ids = list.iter().map(|r| r.id).collect();
            let finished = {
                let mut runs = ci.runs.lock().unwrap();
                merge(&mut runs, known.as_ref(), list, Instant::now())
            };
            let mut repos = ci.repos.lock().unwrap();
            let poll = repos.entry(watched.repo.clone()).or_default();
            poll.etag = r.etag;
            poll.known = Some(ids);
            drop(repos);
            for run in finished {
                let _ = app.emit_to("shell", "ci-finished", run);
            }
            emit_runs(app);
        }
        Ok(r) if r.status == 401 => mark_invalid(app, &watched.account),
        Ok(r) => {
            eprintln!(
                "[pulpo] ci: {} respondió {} (se reintenta en unos minutos)",
                watched.repo, r.status
            );
            next = Instant::now() + ERROR_INTERVAL;
        }
        Err(e) => eprintln!("[pulpo] ci: {}: {e}", watched.repo),
    }
    let active = ci
        .runs
        .lock()
        .unwrap()
        .values()
        .any(|t| t.run.repo == watched.repo && !t.run.finished());
    if active && next < Instant::now() + ERROR_INTERVAL {
        next = Instant::now() + ACTIVE_INTERVAL;
    }
    ci.repos
        .lock()
        .unwrap()
        .entry(watched.repo.clone())
        .or_default()
        .next = Some(next);
}

/// Junta la última página de runs de un repo con lo que ya se mostraba.
/// Devuelve los runs que acaban de terminar. `known`: los ids de la consulta
/// anterior (`None` en la primera, que ignora todo lo ya terminado).
fn merge(
    runs: &mut BTreeMap<u64, Tracked>,
    known: Option<&BTreeSet<u64>>,
    list: Vec<Run>,
    now: Instant,
) -> Vec<Run> {
    let mut finished = Vec::new();
    for run in list {
        match runs.get_mut(&run.id) {
            Some(tracked) => {
                if run.finished() && tracked.finished_at.is_none() {
                    tracked.finished_at = Some(now);
                    finished.push(run.clone());
                }
                tracked.run = run;
            }
            None if !run.finished() => {
                runs.insert(
                    run.id,
                    Tracked {
                        run,
                        finished_at: None,
                    },
                );
            }
            // Terminado sin haberlo visto en marcha: solo interesa si es nuevo
            // (empezó y acabó entre dos consultas). Los que ya estaban, o los
            // que se quitaron de la lista pasados 10 minutos, no.
            None if known.is_some_and(|k| !k.contains(&run.id)) => {
                finished.push(run.clone());
                runs.insert(
                    run.id,
                    Tracked {
                        run,
                        finished_at: Some(now),
                    },
                );
            }
            None => {}
        }
    }
    finished
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn api_run(id: u64, status: &str, conclusion: Option<&str>) -> Value {
        json!({
            "id": id,
            "name": "Build",
            "head_branch": "main",
            "display_title": "Arregla cosas",
            "status": status,
            "conclusion": conclusion,
            "html_url": format!("https://github.com/o/r/actions/runs/{id}"),
            "run_started_at": "2026-10-20T10:00:00Z",
            "updated_at": "2026-10-20T10:05:00Z",
        })
    }

    fn run(id: u64, status: &str, conclusion: Option<&str>) -> Run {
        Run::from_api("o/r", &api_run(id, status, conclusion)).unwrap()
    }

    #[test]
    fn reads_api_runs() {
        let r = run(7, "in_progress", None);
        assert_eq!(r.workflow, "Build");
        assert_eq!(r.branch.as_deref(), Some("main"));
        assert_eq!(r.title, "Arregla cosas");
        assert!(!r.finished());
        assert!(Run::from_api("o/r", &json!({ "id": 1 })).is_none());
    }

    #[test]
    fn ignores_old_runs_on_first_poll() {
        let mut runs = BTreeMap::new();
        let now = Instant::now();
        let done = merge(
            &mut runs,
            None,
            vec![run(1, "completed", Some("success")), run(2, "queued", None)],
            now,
        );
        assert!(done.is_empty());
        assert_eq!(runs.keys().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn reports_runs_that_finish() {
        let mut runs = BTreeMap::new();
        let now = Instant::now();
        merge(
            &mut runs,
            None,
            vec![run(2, "in_progress", None), run(1, "completed", None)],
            now,
        );
        let known = BTreeSet::from([1, 2]);
        let done = merge(
            &mut runs,
            Some(&known),
            vec![
                run(3, "completed", Some("success")),
                run(2, "completed", Some("failure")),
                run(1, "completed", Some("success")),
            ],
            now,
        );
        // El 2 terminó, el 3 empezó y acabó entre consultas; el 1 ya estaba.
        assert_eq!(done.iter().map(|r| r.id).collect::<Vec<_>>(), vec![3, 2]);
        // Ya avisados: no se repiten.
        let known = BTreeSet::from([1, 2, 3]);
        let again = merge(
            &mut runs,
            Some(&known),
            vec![
                run(3, "completed", Some("success")),
                run(2, "completed", Some("failure")),
            ],
            now,
        );
        assert!(again.is_empty());
    }

    #[test]
    fn finished_runs_linger_ten_minutes() {
        let ci = Ci::default();
        let start = Instant::now();
        let known = BTreeSet::from([1]);
        {
            let mut runs = ci.runs.lock().unwrap();
            merge(&mut runs, None, vec![run(1, "queued", None)], start);
            merge(
                &mut runs,
                Some(&known),
                vec![run(1, "completed", Some("success"))],
                start,
            );
        }
        assert!(!expire(&ci, start + Duration::from_secs(9 * 60)));
        assert!(expire(&ci, start + LINGER));
        assert!(ci.runs.lock().unwrap().is_empty());
        // Ya quitado, sigue en la página de GitHub: no vuelve ni avisa otra vez.
        let mut runs = ci.runs.lock().unwrap();
        let done = merge(
            &mut runs,
            Some(&known),
            vec![run(1, "completed", Some("success"))],
            start + LINGER,
        );
        assert!(done.is_empty() && runs.is_empty());
    }

    #[test]
    fn follows_link_header() {
        let link = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=5>; rel="last""#;
        assert_eq!(
            next_link(link).as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
        assert_eq!(next_link(r#"<https://x/?page=1>; rel="prev""#), None);
    }
}
