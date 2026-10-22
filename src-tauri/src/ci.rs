//! CI: los runs de GitHub Actions (build, tests, deploy…) de los repos que el
//! usuario elige, vistos desde la barra lateral.
//!
//! - Acceso con tokens personales de GitHub (fine-grained, "Actions: read"), uno
//!   por dueño de repos (usuario u organización). Todos los de un usuario dan el
//!   mismo login, así que cada token es una cuenta con su propio id (el login, o
//!   `login#2`, `login#3`… para los siguientes) y un nombre opcional. Se guardan en el llavero del
//!   sistema (Secret Service en Linux, Credential Manager en Windows), nunca en
//!   `settings.json` ni en la barra lateral. En `settings.json` solo van los ids,
//!   los logins, los dueños a los que llega cada token y los repos elegidos.
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
/// Entrada del llavero: servicio fijo, usuario = id de la cuenta.
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
    /// Tokens en el llavero, en el orden en que se añadieron.
    #[serde(default)]
    pub accounts: Vec<AccountEntry>,
    #[serde(default)]
    pub repos: Vec<WatchedRepo>,
}

impl CiSettings {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.accounts.is_empty() && self.repos.is_empty()
    }
}

/// Un token guardado. `id` es el usuario de su entrada en el llavero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "StoredAccount")]
pub struct AccountEntry {
    pub id: String,
    pub login: String,
    /// Dueños de los repos a los que llega (vacío: aún no se sabe).
    #[serde(default)]
    pub owners: Vec<String>,
    /// Nombre que le dio el usuario ("personal", "trabajo"…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Versiones anteriores guardaban solo el login (un token por login).
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredAccount {
    Login(String),
    Entry {
        id: String,
        login: String,
        #[serde(default)]
        owners: Vec<String>,
        #[serde(default)]
        name: Option<String>,
    },
}

impl From<StoredAccount> for AccountEntry {
    fn from(stored: StoredAccount) -> Self {
        match stored {
            StoredAccount::Login(login) => AccountEntry {
                id: login.clone(),
                login,
                owners: Vec::new(),
                name: None,
            },
            StoredAccount::Entry {
                id,
                login,
                owners,
                name,
            } => AccountEntry {
                id,
                login,
                owners,
                name,
            },
        }
    }
}

/// Repo vigilado (`dueño/nombre`) y el id de la cuenta cuyo token lo lee.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchedRepo {
    pub repo: String,
    pub account: String,
}

#[derive(Clone, Serialize)]
pub struct Account {
    id: String,
    login: String,
    owners: Vec<String>,
    name: Option<String>,
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

    /// Terminó mal (como `ciState` "fail" en la barra lateral): no se quita
    /// solo, hasta que el usuario lo descarte.
    fn failed(&self) -> bool {
        self.finished()
            && matches!(
                self.conclusion.as_deref(),
                Some("failure" | "timed_out" | "startup_failure" | "action_required")
            )
    }
}

struct Tracked {
    run: Run,
    /// Cuándo se vio terminado (se quita `LINGER` después, salvo si falló).
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
    /// Tokens leídos del llavero por id de cuenta (para no ir a él en cada
    /// consulta).
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

fn keyring_entry(id: &str) -> Result<keyring::Entry, String> {
    // La instancia de pruebas no toca los tokens de la de uso real.
    let service = if crate::diag::test_home().is_some() {
        format!("{KEYRING_SERVICE}.test")
    } else {
        KEYRING_SERVICE.to_string()
    };
    keyring::Entry::new(&service, id).map_err(|e| {
        eprintln!("[pulpo] llavero no disponible: {e}");
        "ci.error.keyring".to_string()
    })
}

fn token(app: &AppHandle, id: &str) -> Option<String> {
    let ci = app.state::<Ci>();
    if let Some(token) = ci.tokens.lock().unwrap().get(id) {
        return Some(token.clone());
    }
    let token = keyring_entry(id).ok()?.get_password().ok()?;
    ci.tokens
        .lock()
        .unwrap()
        .insert(id.to_string(), token.clone());
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
        .map(|a| Account {
            valid: !invalid.contains(&a.id),
            id: a.id,
            login: a.login,
            owners: a.owners,
            name: a.name,
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

/// Quita de la lista un run terminado (el botón ✕ de la vista CI).
#[tauri::command]
pub fn ci_dismiss(app: AppHandle, id: u64) {
    let removed = {
        let ci = app.state::<Ci>();
        let mut runs = ci.runs.lock().unwrap();
        match runs.get(&id) {
            Some(t) if t.run.finished() => runs.remove(&id).is_some(),
            _ => false,
        }
    };
    if removed {
        emit_runs(&app);
    }
}

/// Dueños (usuario u organización) de una lista de repos, sin repetir.
fn owners_of(repos: &[RepoInfo]) -> Vec<String> {
    let owners: BTreeSet<&str> = repos
        .iter()
        .filter_map(|r| r.repo.split_once('/').map(|(owner, _)| owner))
        .collect();
    owners.into_iter().map(str::to_string).collect()
}

/// Id para un token nuevo de `login`: el login o el primer `login#N` libre.
/// (Volver a añadir el mismo token reusa su cuenta: ver `ci_add_token`.)
fn account_id(accounts: &[AccountEntry], login: &str) -> String {
    let taken = |id: &str| accounts.iter().any(|a| a.id == id);
    if !taken(login) {
        return login.to_string();
    }
    (2..)
        .map(|n| format!("{login}#{n}"))
        .find(|id| !taken(id))
        .unwrap()
}

/// Login del dueño de un token y los repos a los que llega. Bloquea: se
/// llama desde `spawn_blocking`.
fn inspect_token(token: &str) -> Result<(String, Vec<RepoInfo>), String> {
    let response = get(token, &format!("{API}/user"), None).map_err(|e| {
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
    let user: Value = serde_json::from_str(&response.body).map_err(|_| "ci.error.network")?;
    let login = user["login"].as_str().ok_or("ci.error.network")?;
    match list_repos(token, login) {
        Ok(repos) => Ok((login.to_string(), token_repos(login, repos))),
        Err(Some(Unauthorized)) => Err("ci.error.badToken".into()),
        Err(None) => Err("ci.error.network".into()),
    }
}

/// Un token comprobado y todavía sin guardar: para elegir sus repos.
#[derive(Serialize)]
pub struct TokenCheck {
    login: String,
    repos: Vec<RepoInfo>,
}

/// Comprueba el token con GitHub y lista sus repos, sin guardar nada.
#[tauri::command]
pub async fn ci_check_token(token: String) -> Result<TokenCheck, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("ci.error.emptyToken".into());
    }
    let (login, mut repos) = tauri::async_runtime::spawn_blocking(move || inspect_token(&token))
        .await
        .map_err(|e| e.to_string())??;
    repos.sort_by_key(|r| r.repo.to_lowercase());
    Ok(TokenCheck { login, repos })
}

/// Guarda el token en el llavero, como una cuenta nueva (o renovada), y vigila
/// los `repos` elegidos con él. Devuelve el id de la cuenta.
#[tauri::command]
pub async fn ci_add_token(
    app: AppHandle,
    token: String,
    name: Option<String>,
    repos: Vec<String>,
) -> Result<String, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("ci.error.emptyToken".into());
    }
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let accounts = settings(&app).accounts;
    let entry = tauri::async_runtime::spawn_blocking({
        let token = token.clone();
        move || -> Result<AccountEntry, String> {
            let (login, found) = inspect_token(&token)?;
            let owners = owners_of(&found);
            // El mismo token otra vez: su cuenta. Dos tokens distintos del mismo
            // usuario pueden llegar a los mismos repos (los públicos), así que
            // solo el token en sí dice si es el mismo.
            let same = accounts.iter().find(|a| {
                a.login == login
                    && keyring_entry(&a.id)
                        .and_then(|e| e.get_password().map_err(|e| e.to_string()))
                        .is_ok_and(|t| t == token)
            });
            let id = match same {
                Some(a) => a.id.clone(),
                None => account_id(&accounts, &login),
            };
            keyring_entry(&id)?.set_password(&token).map_err(|e| {
                eprintln!("[pulpo] no se pudo guardar el token en el llavero: {e}");
                "ci.error.keyring".to_string()
            })?;
            Ok(AccountEntry {
                id,
                login,
                owners,
                name,
            })
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    let ci = app.state::<Ci>();
    ci.tokens.lock().unwrap().insert(entry.id.clone(), token);
    ci.invalid.lock().unwrap().remove(&entry.id);
    let id = entry.id.clone();
    save_settings(&app, |s| {
        match s.accounts.iter_mut().find(|a| a.id == entry.id) {
            Some(existing) => *existing = entry,
            None => s.accounts.push(entry),
        }
        // Los elegidos pasan a leerse con este token, aunque otro también llegue.
        s.repos.retain(|r| !repos.contains(&r.repo));
        s.repos.extend(repos.iter().map(|repo| WatchedRepo {
            repo: repo.clone(),
            account: id.clone(),
        }));
    });
    forget_unwatched(&app);
    emit_accounts(&app);
    emit_runs(&app);
    let _ = ci.wake.lock().unwrap().send(());
    Ok(id)
}

/// Quita la cuenta, su token del llavero y los repos que leía.
#[tauri::command]
pub async fn ci_remove_account(app: AppHandle, id: String) -> Result<(), String> {
    {
        let id = id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Ok(entry) = keyring_entry(&id) {
                if let Err(e) = entry.delete_credential() {
                    eprintln!("[pulpo] no se pudo borrar el token de {id}: {e}");
                }
            }
        })
        .await
        .map_err(|e| e.to_string())?;
    }
    let ci = app.state::<Ci>();
    ci.tokens.lock().unwrap().remove(&id);
    ci.invalid.lock().unwrap().remove(&id);
    save_settings(&app, |s| {
        s.accounts.retain(|a| a.id != id);
        s.repos.retain(|r| r.account != id);
    });
    forget_unwatched(&app);
    emit_accounts(&app);
    emit_runs(&app);
    Ok(())
}

/// Repos a los que llega cada token (uno puede salir bajo varios), por cuenta
/// y por nombre. `watched`: vigilado con el token de esa cuenta.
#[tauri::command]
pub async fn ci_list_repos(app: AppHandle) -> Result<Vec<RepoInfo>, String> {
    let settings = settings(&app);
    let tokens: Vec<(String, String, String)> = settings
        .accounts
        .iter()
        .filter_map(|a| Some((a.id.clone(), a.login.clone(), token(&app, &a.id)?)))
        .collect();
    let handle = app.clone();
    let (mut repos, owners) = tauri::async_runtime::spawn_blocking(move || {
        let mut repos: Vec<RepoInfo> = Vec::new();
        let mut owners: HashMap<String, Vec<String>> = HashMap::new();
        let mut failed = false;
        for (id, login, token) in tokens {
            match list_repos(&token, &id) {
                Ok(found) => {
                    let found = token_repos(&login, found);
                    owners.insert(id, owners_of(&found));
                    repos.extend(found);
                }
                Err(Some(Unauthorized)) => mark_invalid(&handle, &id),
                Err(_) => failed = true,
            }
        }
        if failed && repos.is_empty() {
            return Err("ci.error.network".to_string());
        }
        Ok((repos, owners))
    })
    .await
    .map_err(|e| e.to_string())??;
    // Los dueños de cada token pueden cambiar (repos nuevos, o cuentas de
    // versiones anteriores que aún no los tenían).
    let changed = settings
        .accounts
        .iter()
        .any(|a| owners.get(&a.id).is_some_and(|o| *o != a.owners));
    if changed {
        save_settings(&app, |s| {
            for a in &mut s.accounts {
                if let Some(o) = owners.get(&a.id) {
                    a.owners = o.clone();
                }
            }
        });
        emit_accounts(&app);
    }
    for repo in &mut repos {
        repo.watched = settings
            .repos
            .iter()
            .any(|w| w.repo == repo.repo && w.account == repo.account);
    }
    // Los vigilados que el token ya no ve siguen en la lista, para poder quitarlos.
    for watched in &settings.repos {
        if !repos
            .iter()
            .any(|r| r.repo == watched.repo && r.account == watched.account)
        {
            repos.push(RepoInfo {
                repo: watched.repo.clone(),
                account: watched.account.clone(),
                private: false,
                watched: true,
            });
        }
    }
    let order = |account: &str| settings.accounts.iter().position(|a| a.id == account);
    repos.sort_by(|a, b| {
        order(&a.account)
            .cmp(&order(&b.account))
            .then_with(|| a.repo.to_lowercase().cmp(&b.repo.to_lowercase()))
    });
    Ok(repos)
}

struct Unauthorized;

/// Los repos que el token tiene de verdad. Un token fine-grained puede leer
/// todos los repos públicos, y `/user/repos` devuelve también los del usuario
/// aunque el token sea de una organización. Si llega a repos de una sola
/// organización, esa es su dueña: solo los suyos.
fn token_repos(login: &str, repos: Vec<RepoInfo>) -> Vec<RepoInfo> {
    let others: BTreeSet<&str> = repos
        .iter()
        .filter_map(|r| r.repo.split_once('/').map(|(owner, _)| owner))
        .filter(|owner| *owner != login)
        .collect();
    let [org] = others.into_iter().collect::<Vec<_>>()[..] else {
        return repos;
    };
    let org = format!("{org}/");
    repos
        .into_iter()
        .filter(|r| r.repo.starts_with(&org))
        .collect()
}

/// Repos a los que llega el token de la cuenta `account` (su id).
fn list_repos(token: &str, account: &str) -> Result<Vec<RepoInfo>, Option<Unauthorized>> {
    let mut url = Some(format!("{API}/user/repos?per_page=100&sort=pushed"));
    let mut repos = Vec::new();
    // Hasta 1000 repos.
    for _ in 0..10 {
        let Some(page) = url.take() else { break };
        let response = get(token, &page, None).map_err(|e| {
            eprintln!("[pulpo] ci: no se pudo listar los repos de {account}: {e}");
            None
        })?;
        match response.status {
            200 => {}
            401 => return Err(Some(Unauthorized)),
            status => {
                eprintln!("[pulpo] ci: /user/repos de {account} respondió {status}");
                return Err(None);
            }
        }
        let list: Vec<Value> = serde_json::from_str(&response.body).map_err(|_| None)?;
        repos.extend(list.iter().filter_map(|r| {
            Some(RepoInfo {
                repo: r["full_name"].as_str()?.to_string(),
                account: account.to_string(),
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
        if accounts.iter().any(|a| a.id == repo.account)
            && !chosen.iter().any(|r| r.repo == repo.repo)
        {
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
/// (conoce el idioma); un clic abre el run en el navegador. `state` es el de
/// `ciState` en la barra lateral ("ok", "fail" u otro): elige el icono y el
/// sonido, y un fallo queda en pantalla hasta cerrarlo.
#[tauri::command]
pub fn ci_notify(app: AppHandle, summary: String, body: String, url: String, state: String) {
    std::thread::spawn(move || {
        let kind = NoticeKind::from(state.as_str());
        let mut notification = notify_rust::Notification::new();
        notification.summary(&summary).body(&body).appname("Pulpo");
        if let Some(icon) = notice_icon(&app, kind) {
            notification.icon(&icon.to_string_lossy());
        }
        #[cfg(target_os = "linux")]
        {
            notification.urgency(match kind {
                NoticeKind::Fail => notify_rust::Urgency::Critical,
                _ => notify_rust::Urgency::Normal,
            });
            notification.action("default", "Open");
            match notification.show() {
                Ok(handle) => {
                    play_sound(&app, kind);
                    handle.wait_for_action(|action| {
                        if action == "default" {
                            let _ = ci_open(app, url);
                        }
                    })
                }
                Err(e) => eprintln!("[pulpo] ci: no se pudo mostrar el aviso: {e}"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Windows suena solo: el sonido por defecto de los avisos del sistema.
            let _ = url;
            if let Err(e) = notification.show() {
                eprintln!("[pulpo] ci: no se pudo mostrar el aviso: {e}");
            }
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NoticeKind {
    Ok,
    Fail,
    Other,
}

impl From<&str> for NoticeKind {
    fn from(state: &str) -> Self {
        match state {
            "ok" => Self::Ok,
            "fail" => Self::Fail,
            _ => Self::Other,
        }
    }
}

impl NoticeKind {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fail => "fail",
            Self::Other => "off",
        }
    }
}

/// Escribe (una vez) `bytes` en `<cache>/ci/<name>` y devuelve la ruta: el
/// servidor de avisos y el reproductor necesitan un archivo.
fn cached_file(app: &AppHandle, name: &str, bytes: &[u8]) -> Option<std::path::PathBuf> {
    let dir = app.path().app_cache_dir().ok()?.join("ci");
    let path = dir.join(name);
    if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, bytes).ok()?;
    }
    Some(path)
}

/// Icono del aviso: círculo verde, rojo o gris, como los de la vista CI.
fn notice_icon(app: &AppHandle, kind: NoticeKind) -> Option<std::path::PathBuf> {
    let bytes: &[u8] = match kind {
        NoticeKind::Ok => include_bytes!("../icons/ci-ok.png"),
        NoticeKind::Fail => include_bytes!("../icons/ci-fail.png"),
        NoticeKind::Other => include_bytes!("../icons/ci-off.png"),
    };
    cached_file(app, &format!("{}.png", kind.name()), bytes)
}

/// Suena al llegar el aviso. En Linux el servidor de avisos no pone sonido
/// (en las apps web lo pone la página), así que Pulpo reproduce el suyo con el
/// primer reproductor que haya: PipeWire, PulseAudio o ALSA.
#[cfg(target_os = "linux")]
fn play_sound(app: &AppHandle, kind: NoticeKind) {
    let Some(path) = cached_file(app, &format!("{}.wav", kind.name()), &chime(kind)) else {
        return;
    };
    let played = ["pw-play", "paplay", "aplay"].iter().any(|player| {
        let mut command = std::process::Command::new(player);
        if *player == "aplay" {
            command.arg("-q");
        }
        command
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if !played {
        eprintln!("[pulpo] ci: no se pudo reproducir el sonido del aviso");
    }
}

/// Campanita en WAV (mono, 16 bits): dos notas que suben si salió bien, que
/// bajan si falló, una sola si se canceló.
#[cfg(any(target_os = "linux", test))]
fn chime(kind: NoticeKind) -> Vec<u8> {
    const RATE: u32 = 44_100;
    const NOTE: f32 = 0.16;
    const TAIL: f32 = 0.5;
    let notes: &[f32] = match kind {
        NoticeKind::Ok => &[659.25, 987.77],
        NoticeKind::Fail => &[523.25, 392.0, 311.13],
        NoticeKind::Other => &[659.25],
    };
    let total = NOTE * (notes.len() - 1) as f32 + TAIL;
    let len = (total * RATE as f32) as usize;
    let mut samples = vec![0f32; len];
    for (i, freq) in notes.iter().enumerate() {
        let start = (i as f32 * NOTE * RATE as f32) as usize;
        for (n, sample) in samples[start..].iter_mut().enumerate() {
            let t = n as f32 / RATE as f32;
            // Ataque corto (sin chasquido) y caída exponencial, con un armónico.
            let envelope = (t / 0.005).min(1.0) * (-t * 7.0).exp();
            let phase = 2.0 * std::f32::consts::PI * freq * t;
            *sample += envelope * (phase.sin() + 0.25 * (2.0 * phase).sin());
        }
    }
    let data_len = (len * 2) as u32;
    let mut wav = Vec::with_capacity(44 + len * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let value = (sample * 0.3).clamp(-1.0, 1.0) * i16::MAX as f32;
        wav.extend_from_slice(&(value as i16).to_le_bytes());
    }
    wav
}

// ---------------------------------------------------------------- sondeo

fn mark_invalid(app: &AppHandle, id: &str) {
    let ci = app.state::<Ci>();
    if ci.invalid.lock().unwrap().insert(id.to_string()) {
        ci.tokens.lock().unwrap().remove(id);
        eprintln!("[pulpo] ci: GitHub rechazó el token de {id}");
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

/// Quita los runs terminados hace más de `LINGER`, menos los fallidos (esos
/// esperan a `ci_dismiss`). `true` si quitó alguno.
fn expire(ci: &Ci, now: Instant) -> bool {
    let mut runs = ci.runs.lock().unwrap();
    let before = runs.len();
    runs.retain(|_, t| {
        t.run.failed()
            || t.finished_at
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

    #[test]
    fn chime_is_a_valid_wav() {
        for kind in [NoticeKind::Ok, NoticeKind::Fail, NoticeKind::Other] {
            let wav = chime(kind);
            assert_eq!(&wav[..4], b"RIFF");
            assert_eq!(&wav[8..16], b"WAVEfmt ");
            let riff = u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize;
            let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
            assert_eq!(riff + 8, wav.len());
            assert_eq!(data + 44, wav.len());
        }
        assert_eq!(NoticeKind::from("fail"), NoticeKind::Fail);
        assert_eq!(NoticeKind::from("pending"), NoticeKind::Other);
    }

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
    fn failed_runs_stay_until_dismissed() {
        let ci = Ci::default();
        let start = Instant::now();
        {
            let mut runs = ci.runs.lock().unwrap();
            merge(
                &mut runs,
                None,
                vec![run(1, "queued", None), run(2, "queued", None)],
                start,
            );
            merge(
                &mut runs,
                Some(&BTreeSet::from([1, 2])),
                vec![
                    run(1, "completed", Some("failure")),
                    run(2, "completed", Some("cancelled")),
                ],
                start,
            );
        }
        assert!(expire(&ci, start + LINGER * 10));
        let left: Vec<u64> = ci.runs.lock().unwrap().keys().copied().collect();
        assert_eq!(left, vec![1]);
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

    fn entry(id: &str, login: &str, owners: &[&str]) -> AccountEntry {
        AccountEntry {
            id: id.into(),
            login: login.into(),
            owners: owners.iter().map(|o| o.to_string()).collect(),
            name: None,
        }
    }

    #[test]
    fn one_account_per_token() {
        assert_eq!(account_id(&[], "ana"), "ana");
        let accounts = vec![entry("ana", "ana", &["ana"])];
        assert_eq!(account_id(&accounts, "ana"), "ana#2");
        let accounts = vec![
            entry("ana", "ana", &["ana"]),
            entry("ana#2", "ana", &["acme"]),
        ];
        assert_eq!(account_id(&accounts, "ana"), "ana#3");
        assert_eq!(account_id(&accounts, "bob"), "bob");
    }

    #[test]
    fn org_token_shows_only_org_repos() {
        let repo = |name: &str| RepoInfo {
            repo: name.into(),
            account: "ana".into(),
            private: false,
            watched: false,
        };
        let names = |repos: Vec<RepoInfo>| repos.into_iter().map(|r| r.repo).collect::<Vec<_>>();
        // Token de la organización: los públicos de la usuaria sobran.
        let found = vec![repo("ana/blog"), repo("acme/app"), repo("acme/api")];
        assert_eq!(names(token_repos("ana", found)), ["acme/app", "acme/api"]);
        // Token de la usuaria, o que llega a varias organizaciones: todos.
        let found = vec![repo("ana/blog"), repo("ana/dots")];
        assert_eq!(names(token_repos("ana", found)).len(), 2);
        let found = vec![repo("ana/blog"), repo("acme/app"), repo("globex/x")];
        assert_eq!(names(token_repos("ana", found)).len(), 3);
    }

    #[test]
    fn reads_old_accounts() {
        let settings: CiSettings = serde_json::from_value(json!({
            "accounts": ["ana", { "id": "ana#2", "login": "ana", "owners": ["acme"] }],
        }))
        .unwrap();
        assert_eq!(
            settings.accounts,
            vec![entry("ana", "ana", &[]), entry("ana#2", "ana", &["acme"])]
        );
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
