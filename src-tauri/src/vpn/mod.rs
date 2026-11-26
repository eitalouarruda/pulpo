//! Túnel WireGuard por app em espaço de usuário + proxy HTTP CONNECT local.

pub mod config;
pub mod peer_auth;
pub mod proxy;
pub mod tunnel;
pub mod turn;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;

use crate::AppState;
use config::{
    build_validated_profile, delete_profile_secrets, generate_profile_id, load_profile_secrets,
    parse_wg_conf, save_profile_secrets, ValidatedConfig, VpnProfile, VpnSettings, MAX_PROFILES,
};
use proxy::start_proxy;
use tunnel::{run_connectivity_test, spawn_tunnel, TunnelHandle};

#[derive(Clone)]
enum ProfileHealth {
    Ok(u64),
    Failed,
}

#[derive(Clone, Serialize)]
pub struct VpnNotice {
    pub id: u64,
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
    pub latency_ms: Option<u64>,
}

#[derive(Default)]
pub struct Vpn {
    runtime: Mutex<Option<ActiveRuntime>>,
    busy: Arc<AtomicBool>,
    turn_creds: OnceLock<turn::TurnCredentials>,
    health: Mutex<HashMap<String, ProfileHealth>>,
    notice: Mutex<Option<VpnNotice>>,
    notice_seq: AtomicU64,
}

impl Vpn {
    fn turn_creds(&self) -> turn::TurnCredentials {
        self.turn_creds
            .get_or_init(turn::TurnCredentials::generate)
            .clone()
    }
}

struct ActiveRuntime {
    port: u16,
    proxy_shutdown: watch::Sender<bool>,
    tunnel_slot: Arc<RwLock<TunnelHandle>>,
    tunnel_shutdown: watch::Sender<bool>,
    active_profile_id: String,
    watchdog_shutdown: watch::Sender<bool>,
}

impl ActiveRuntime {
    fn current_handle(&self) -> TunnelHandle {
        self.tunnel_slot.read().unwrap().clone()
    }

    fn shutdown_all(&self) {
        let _ = self.watchdog_shutdown.send(true);
        let _ = self.tunnel_shutdown.send(true);
        let _ = self.proxy_shutdown.send(true);
    }
}

struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Vpn {
    fn lock_busy(&self) -> Result<BusyGuard, String> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| BusyGuard(self.busy.clone()))
            .map_err(|_| "vpn.error.busy".to_string())
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VpnState {
    Disabled,
    Unconfigured,
    Ready,
    Connected,
    Error,
}

#[derive(Clone, Serialize)]
pub struct VpnProfileSummary {
    pub id: String,
    pub label: String,
    pub endpoint: String,
    pub health: Option<String>,
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct VpnStatus {
    pub state: VpnState,
    pub label: Option<String>,
    pub endpoint: Option<String>,
    pub profiles: Vec<VpnProfileSummary>,
    pub active_profile_id: Option<String>,
    pub auto_failover: bool,
    pub services: Vec<String>,
    pub error: Option<String>,
    pub notice: Option<VpnNotice>,
}

#[derive(Clone, Serialize)]
pub struct VpnTestResult {
    pub latency_ms: u64,
}

pub fn init_settings(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut guard = state.settings.lock().unwrap();
    if guard.vpn.migrate_legacy() {
        if let Err(e) = crate::display::save(&state.settings_path, &guard) {
            eprintln!(
                "[pulpo] no se pudo guardar {}: {e}",
                state.settings_path.display()
            );
        }
    }
}

pub fn start_if_needed(app: &AppHandle) {
    init_settings(app);
    let settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    if !settings.enabled || !settings.has_config() || settings.services.is_empty() {
        return;
    }
    let vpn = app.state::<Vpn>();
    if vpn.runtime.lock().unwrap().is_some() {
        return;
    }

    let Some((profile_id, validated)) = load_first_valid_profile(&settings) else {
        return;
    };

    let (proxy_shutdown_tx, proxy_shutdown_rx) = watch::channel(false);
    let (tunnel_shutdown_tx, tunnel_shutdown_rx) = watch::channel(false);
    let (watchdog_shutdown_tx, watchdog_shutdown_rx) = watch::channel(false);

    let handle = spawn_tunnel(validated, tunnel_shutdown_rx);
    let tunnel_slot = Arc::new(RwLock::new(handle));

    match tauri::async_runtime::block_on(start_proxy(
        tunnel_slot.clone(),
        vpn.turn_creds(),
        proxy_shutdown_rx,
    )) {
        Ok(port) => {
            *vpn.runtime.lock().unwrap() = Some(ActiveRuntime {
                port,
                proxy_shutdown: proxy_shutdown_tx,
                tunnel_slot,
                tunnel_shutdown: tunnel_shutdown_tx,
                active_profile_id: profile_id,
                watchdog_shutdown: watchdog_shutdown_tx,
            });
            spawn_failover_watchdog(app.clone(), watchdog_shutdown_rx);
        }
        Err(e) => {
            let _ = tunnel_shutdown_tx.send(true);
            let _ = proxy_shutdown_tx.send(true);
            let _ = watchdog_shutdown_tx.send(true);
            eprintln!("[pulpo] vpn: não foi possível iniciar o proxy local: {e}");
        }
    }
}

fn load_first_valid_profile(settings: &VpnSettings) -> Option<(String, ValidatedConfig)> {
    let active_id = settings.active_profile().map(|p| p.id.as_str());
    let ordered = settings
        .profiles
        .iter()
        .filter(|p| Some(p.id.as_str()) == active_id)
        .chain(
            settings
                .profiles
                .iter()
                .filter(|p| Some(p.id.as_str()) != active_id),
        );
    for profile in ordered {
        if let Ok(secrets) = load_profile_secrets(&profile.id) {
            if let Ok(validated) = build_validated_profile(profile, secrets) {
                return Some((profile.id.clone(), validated));
            }
        }
    }
    None
}

fn install_runtime_handle(
    app: &AppHandle,
    profile_id: &str,
    handle: TunnelHandle,
    shutdown_tx: watch::Sender<bool>,
) {
    let vpn = app.state::<Vpn>();
    let mut guard = vpn.runtime.lock().unwrap();
    let Some(active) = guard.as_mut() else {
        let _ = shutdown_tx.send(true);
        return;
    };
    *active.tunnel_slot.write().unwrap() = handle;
    let old_shutdown = std::mem::replace(&mut active.tunnel_shutdown, shutdown_tx);
    active.active_profile_id = profile_id.to_string();
    let _ = old_shutdown.send(true);
}

fn swap_runtime_tunnel(app: &AppHandle, profile_id: &str, validated: ValidatedConfig) {
    let (new_shutdown_tx, new_shutdown_rx) = watch::channel(false);
    let new_handle = spawn_tunnel(validated, new_shutdown_rx);
    install_runtime_handle(app, profile_id, new_handle, new_shutdown_tx);
}

fn hot_swap_profile_sync(app: &AppHandle, profile: &VpnProfile) -> Result<(), String> {
    let secrets = load_profile_secrets(&profile.id)?;
    let validated = build_validated_profile(profile, secrets)?;
    swap_runtime_tunnel(app, &profile.id, validated);
    Ok(())
}

pub fn stop(app: &AppHandle) {
    let active = app.state::<Vpn>().runtime.lock().unwrap().take();
    if let Some(active) = active {
        active.shutdown_all();
    }
}

pub fn on_service_removed(app: &AppHandle, services_empty: bool) {
    if services_empty {
        stop(app);
    }
    emit_changed(app);
}

pub fn proxy_for_service(app: &AppHandle, id: &str) -> Option<String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap();
    if !settings.vpn.enabled
        || !settings.vpn.has_config()
        || !settings.vpn.services.iter().any(|s| s == id)
    {
        return None;
    }
    drop(settings);
    let vpn = app.state::<Vpn>();
    let port = vpn.runtime.lock().unwrap().as_ref().map(|a| a.port);
    if let Some(port) = port {
        Some(format!("http://127.0.0.1:{port}"))
    } else {
        // Fail-closed: bloqueia a conexão se o túnel deveria estar ativo mas não iniciou.
        Some("http://127.0.0.1:1".to_string())
    }
}

pub fn webrtc_script_for_service(app: &AppHandle, id: &str) -> Option<String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap();
    if !settings.vpn.enabled
        || !settings.vpn.has_config()
        || !settings.vpn.services.iter().any(|s| s == id)
    {
        return None;
    }
    drop(settings);
    let vpn = app.state::<Vpn>();
    Some(turn::webrtc_patch_script(&vpn.turn_creds()))
}

pub fn current_status(app: &AppHandle) -> VpnStatus {
    let mut settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    settings.migrate_legacy();
    let vpn = app.state::<Vpn>();
    let health_map = {
        let mut guard = vpn.health.lock().unwrap();
        guard.retain(|id, _| settings.profiles.iter().any(|p| &p.id == id));
        guard.clone()
    };
    let notice = vpn.notice.lock().unwrap().clone();
    let profiles: Vec<VpnProfileSummary> = settings
        .profiles
        .iter()
        .map(|p| {
            let (health, latency_ms) = match health_map.get(&p.id) {
                Some(ProfileHealth::Ok(ms)) => (Some("ok".to_string()), Some(*ms)),
                Some(ProfileHealth::Failed) => (Some("error".to_string()), None),
                None => (None, None),
            };
            VpnProfileSummary {
                id: p.id.clone(),
                label: p.label.clone().unwrap_or_else(|| "WireGuard".to_string()),
                endpoint: p.endpoint.clone(),
                health,
                latency_ms,
            }
        })
        .collect();
    let active = settings.active_profile();

    let runtime = vpn.runtime.lock().unwrap();
    let (state, error) = if !settings.enabled {
        (VpnState::Disabled, None)
    } else if !settings.has_config() {
        (VpnState::Unconfigured, None)
    } else if let Some(runtime) = runtime.as_ref() {
        let handle = runtime.current_handle();
        if let Some(err) = handle.stats.last_error() {
            (VpnState::Error, Some(err))
        } else if handle.stats.is_connected() {
            (VpnState::Connected, None)
        } else {
            (VpnState::Ready, None)
        }
    } else {
        (VpnState::Ready, None)
    };

    VpnStatus {
        state,
        label: active.and_then(|p| p.label.clone()),
        endpoint: active.map(|p| p.endpoint.clone()),
        profiles,
        active_profile_id: settings.active_profile_id,
        auto_failover: settings.auto_failover,
        services: settings.services,
        error,
        notice,
    }
}

fn set_profile_health(app: &AppHandle, id: &str, health: ProfileHealth) {
    app.state::<Vpn>()
        .health
        .lock()
        .unwrap()
        .insert(id.to_string(), health);
}

fn set_vpn_notice(
    app: &AppHandle,
    kind: &str,
    from: String,
    to: Option<String>,
    latency_ms: Option<u64>,
) {
    let vpn = app.state::<Vpn>();
    let id = vpn.notice_seq.fetch_add(1, Ordering::Relaxed) + 1;
    *vpn.notice.lock().unwrap() = Some(VpnNotice {
        id,
        kind: kind.to_string(),
        from,
        to,
        latency_ms,
    });
}

fn clear_vpn_notice(app: &AppHandle) {
    *app.state::<Vpn>().notice.lock().unwrap() = None;
}

fn emit_changed(app: &AppHandle) {
    let _ = app.emit_to("shell", "vpn-changed", current_status(app));
}

fn save_vpn_settings(app: &AppHandle, settings: VpnSettings) {
    let state = app.state::<AppState>();
    let mut guard = state.settings.lock().unwrap();
    guard.vpn = settings;
    if let Err(e) = crate::display::save(&state.settings_path, &guard) {
        eprintln!(
            "[pulpo] no se pudo guardar {}: {e}",
            state.settings_path.display()
        );
    }
}

fn active_services_to_reopen(app: &AppHandle, ids: &[String]) -> Vec<String> {
    let state = app.state::<AppState>();
    let list = state.services.lock().unwrap();
    ids.iter()
        .filter(|id| {
            list.iter().any(|s| &s.id == *id && s.enabled) && app.get_webview(id).is_some()
        })
        .cloned()
        .collect()
}

async fn failover_from_profile(app: &AppHandle, failed_id: &str) -> bool {
    let settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    if !settings.enabled
        || !settings.auto_failover
        || settings.active_profile_id.as_deref() != Some(failed_id)
    {
        return false;
    }
    let candidates: Vec<VpnProfile> = settings
        .profiles
        .iter()
        .filter(|p| p.id != failed_id)
        .cloned()
        .collect();
    if candidates.is_empty() {
        return false;
    }

    let orig_profile = settings
        .profiles
        .iter()
        .find(|p| p.id == failed_id)
        .cloned();
    let failed_label = orig_profile
        .as_ref()
        .and_then(|p| p.label.clone())
        .unwrap_or_else(|| "WireGuard".to_string());

    set_profile_health(app, failed_id, ProfileHealth::Failed);
    emit_changed(app);

    // Encerra o túnel ativo antes de testar backups para evitar colisão de 1 dispositivo na Proton Free.
    let had_runtime = {
        let vpn = app.state::<Vpn>();
        let guard = vpn.runtime.lock().unwrap();
        if let Some(active) = guard.as_ref() {
            let _ = active.tunnel_shutdown.send(true);
            true
        } else {
            false
        }
    };
    if !had_runtime {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    for candidate in candidates {
        if app.state::<Vpn>().runtime.lock().unwrap().is_none() {
            return false;
        }
        let id_for_load = candidate.id.clone();
        let Ok(Ok(secrets)) =
            tauri::async_runtime::spawn_blocking(move || load_profile_secrets(&id_for_load)).await
        else {
            set_profile_health(app, &candidate.id, ProfileHealth::Failed);
            continue;
        };
        let Ok(validated) = build_validated_profile(&candidate, secrets) else {
            set_profile_health(app, &candidate.id, ProfileHealth::Failed);
            continue;
        };
        let (probe_shutdown_tx, probe_shutdown_rx) = watch::channel(false);
        let probe_handle = spawn_tunnel(validated, probe_shutdown_rx);
        let probe_res = tokio::time::timeout(
            Duration::from_secs(10),
            run_connectivity_test(&probe_handle),
        )
        .await;

        if let Ok(Ok(ms)) = probe_res {
            let mut updated_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
            if updated_settings.enabled
                && updated_settings.auto_failover
                && updated_settings.active_profile_id.as_deref() == Some(failed_id)
                && updated_settings
                    .profiles
                    .iter()
                    .any(|p| p.id == candidate.id)
            {
                let to_label = candidate
                    .label
                    .clone()
                    .unwrap_or_else(|| "WireGuard".to_string());
                updated_settings.active_profile_id = Some(candidate.id.clone());
                save_vpn_settings(app, updated_settings);
                set_profile_health(app, &candidate.id, ProfileHealth::Ok(ms));
                install_runtime_handle(app, &candidate.id, probe_handle, probe_shutdown_tx);
                set_vpn_notice(app, "switched", failed_label, Some(to_label), Some(ms));
                if crate::diag::flag("PULPO_VPN_DEBUG") {
                    eprintln!(
                        "[pulpo] vpn: auto-failover -> {} ({ms}ms)",
                        candidate.label.as_deref().unwrap_or(&candidate.id)
                    );
                }
                emit_changed(app);
                return true;
            }
            let _ = probe_shutdown_tx.send(true);
            return false;
        }

        set_profile_health(app, &candidate.id, ProfileHealth::Failed);
        let _ = probe_shutdown_tx.send(true);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    if let Some(orig) = orig_profile {
        if hot_swap_profile_sync(app, &orig).is_ok() {
            if let Some(active) = app.state::<Vpn>().runtime.lock().unwrap().as_ref() {
                active.current_handle().stats.set_error("vpn.error.timeout");
            }
        }
    }
    set_vpn_notice(app, "all_failed", failed_label, None, None);
    emit_changed(app);
    false
}

fn spawn_failover_watchdog(app: AppHandle, mut shutdown: watch::Receiver<bool>) {
    tauri::async_runtime::spawn(async move {
        let started_at = Instant::now();
        let mut last_failover: Option<Instant> = None;
        let mut consecutive_all_failed: u32 = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                _ = tick.tick() => {}
            }

            if *shutdown.borrow() {
                break;
            }
            let cooldown = match consecutive_all_failed {
                0 => Duration::from_secs(20),
                1 => Duration::from_secs(60),
                _ => Duration::from_secs(180),
            };
            if started_at.elapsed() < Duration::from_secs(8)
                || last_failover.is_some_and(|t| t.elapsed() < cooldown)
            {
                continue;
            }

            let settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
            if !settings.enabled || !settings.auto_failover || settings.profiles.len() < 2 {
                continue;
            }

            let active_pair = {
                let vpn = app.state::<Vpn>();
                let guard = vpn.runtime.lock().unwrap();
                guard
                    .as_ref()
                    .map(|r| (r.current_handle(), r.active_profile_id.clone()))
            };
            let Some((handle, active_id)) = active_pair else {
                break;
            };

            let was_connected = handle.stats.is_connected();
            let degraded = !was_connected
                || handle.stats.last_error().is_some()
                || handle.stats.consecutive_failures() >= 2
                || handle.stats.is_stalled(Duration::from_secs(15));
            if !degraded {
                continue;
            }

            // Confirma queda real antes de trocar de servidor (não troca só por latência alta).
            if let Ok(Ok(ms)) =
                tokio::time::timeout(Duration::from_secs(10), run_connectivity_test(&handle)).await
            {
                handle.stats.record_success();
                consecutive_all_failed = 0;
                set_profile_health(&app, &active_id, ProfileHealth::Ok(ms));
                emit_changed(&app);
                continue;
            }

            // Se estava conectado, exige uma 2ª confirmação rápida para evitar derrubar call por oscilação.
            if was_connected {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if *shutdown.borrow() {
                    break;
                }
                if let Ok(Ok(ms)) =
                    tokio::time::timeout(Duration::from_secs(8), run_connectivity_test(&handle))
                        .await
                {
                    handle.stats.record_success();
                    consecutive_all_failed = 0;
                    set_profile_health(&app, &active_id, ProfileHealth::Ok(ms));
                    emit_changed(&app);
                    continue;
                }
            }

            let Ok(_busy) = app.state::<Vpn>().lock_busy() else {
                continue;
            };
            let switched = failover_from_profile(&app, &active_id).await;
            if switched {
                consecutive_all_failed = 0;
            } else {
                consecutive_all_failed = consecutive_all_failed.saturating_add(1);
            }
            last_failover = Some(Instant::now());
        }
    });
}

#[tauri::command]
pub fn vpn_get_settings(state: tauri::State<'_, AppState>) -> VpnSettings {
    state.settings.lock().unwrap().vpn.clone()
}

#[tauri::command]
pub fn vpn_status(app: AppHandle) -> VpnStatus {
    current_status(&app)
}

#[tauri::command]
pub fn vpn_import_conf(
    app: AppHandle,
    raw: String,
    label: Option<String>,
) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;

    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    if vpn_settings.profiles.len() >= MAX_PROFILES {
        return Err("vpn.error.maxProfiles".into());
    }

    let mut parsed = parse_wg_conf(&raw, label.as_deref())?;
    let id = generate_profile_id(&vpn_settings.profiles);
    parsed.profile.id = id.clone();

    save_profile_secrets(&id, &parsed.secrets)?;
    let new_profile = parsed.profile.clone();

    vpn_settings.profiles.push(parsed.profile);
    vpn_settings.active_profile_id = Some(id);
    save_vpn_settings(&app, vpn_settings.clone());
    clear_vpn_notice(&app);

    let had_runtime = vpn.runtime.lock().unwrap().is_some();
    let reopen = if had_runtime {
        hot_swap_profile_sync(&app, &new_profile)?;
        Vec::new()
    } else {
        start_if_needed(&app);
        if vpn_settings.enabled {
            active_services_to_reopen(&app, &vpn_settings.services)
        } else {
            Vec::new()
        }
    };

    emit_changed(&app);
    if !reopen.is_empty() {
        crate::reopen_services_for_vpn(&app, reopen);
    }
    Ok(vpn_settings)
}

#[tauri::command]
pub fn vpn_select_profile(app: AppHandle, id: String) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;

    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    let Some(profile) = vpn_settings.profiles.iter().find(|p| p.id == id).cloned() else {
        return Err("vpn.error.unconfigured".into());
    };
    if vpn_settings.active_profile_id.as_deref() == Some(id.as_str()) {
        return Ok(vpn_settings);
    }

    vpn_settings.active_profile_id = Some(id);
    save_vpn_settings(&app, vpn_settings.clone());
    clear_vpn_notice(&app);

    let had_runtime = vpn.runtime.lock().unwrap().is_some();
    if had_runtime {
        hot_swap_profile_sync(&app, &profile)?;
    } else if vpn_settings.enabled {
        start_if_needed(&app);
    }

    emit_changed(&app);
    Ok(vpn_settings)
}

#[tauri::command]
pub fn vpn_remove_profile(app: AppHandle, id: String) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;

    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    let before = vpn_settings.profiles.len();
    vpn_settings.profiles.retain(|p| p.id != id);
    if vpn_settings.profiles.len() == before {
        return Ok(vpn_settings);
    }
    delete_profile_secrets(&id);

    let removed_active = vpn_settings.active_profile_id.as_deref() == Some(id.as_str());
    if removed_active {
        vpn_settings.active_profile_id = vpn_settings.profiles.first().map(|p| p.id.clone());
    }
    save_vpn_settings(&app, vpn_settings.clone());

    let had_runtime = vpn.runtime.lock().unwrap().is_some();
    let reopen = if vpn_settings.profiles.is_empty() {
        stop(&app);
        if vpn_settings.enabled {
            active_services_to_reopen(&app, &vpn_settings.services)
        } else {
            Vec::new()
        }
    } else {
        if removed_active && had_runtime {
            if let Some(next_profile) = vpn_settings.active_profile() {
                let _ = hot_swap_profile_sync(&app, next_profile);
            }
        }
        Vec::new()
    };

    emit_changed(&app);
    if !reopen.is_empty() {
        crate::reopen_services_for_vpn(&app, reopen);
    }
    Ok(vpn_settings)
}

#[tauri::command]
pub fn vpn_set_auto_failover(app: AppHandle, enabled: bool) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;
    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    if vpn_settings.auto_failover == enabled {
        return Ok(vpn_settings);
    }
    vpn_settings.auto_failover = enabled;
    save_vpn_settings(&app, vpn_settings.clone());
    emit_changed(&app);
    Ok(vpn_settings)
}

#[tauri::command]
pub fn vpn_set_enabled(app: AppHandle, enabled: bool) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;
    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    if vpn_settings.enabled == enabled {
        return Ok(vpn_settings);
    }
    vpn_settings.enabled = enabled;
    save_vpn_settings(&app, vpn_settings.clone());
    clear_vpn_notice(&app);

    stop(&app);
    if enabled {
        start_if_needed(&app);
    }
    let reopen = active_services_to_reopen(&app, &vpn_settings.services);
    emit_changed(&app);
    if !reopen.is_empty() {
        crate::reopen_services_for_vpn(&app, reopen);
    }
    Ok(vpn_settings)
}

#[tauri::command]
pub fn vpn_set_services(app: AppHandle, services: Vec<String>) -> Result<VpnSettings, String> {
    init_settings(&app);
    let vpn = app.state::<Vpn>();
    let _busy = vpn.lock_busy()?;
    let valid_ids: Vec<String> = {
        let state = app.state::<AppState>();
        let known = state.services.lock().unwrap();
        let mut out = Vec::new();
        for id in services {
            if known.iter().any(|s| s.id == id) && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    };

    let mut vpn_settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    let old_services = vpn_settings.services.clone();
    if old_services == valid_ids {
        return Ok(vpn_settings);
    }

    let mut changed: Vec<String> = old_services
        .iter()
        .filter(|id| !valid_ids.contains(id))
        .cloned()
        .collect();
    for id in &valid_ids {
        if !old_services.contains(id) {
            changed.push(id.clone());
        }
    }

    vpn_settings.services = valid_ids;
    save_vpn_settings(&app, vpn_settings.clone());

    if vpn_settings.enabled {
        if vpn_settings.services.is_empty() {
            stop(&app);
        } else {
            start_if_needed(&app);
        }
    }
    let reopen = if vpn_settings.enabled {
        active_services_to_reopen(&app, &changed)
    } else {
        Vec::new()
    };
    emit_changed(&app);
    if !reopen.is_empty() {
        crate::reopen_services_for_vpn(&app, reopen);
    }
    Ok(vpn_settings)
}

#[tauri::command]
pub async fn vpn_test(app: AppHandle, profile_id: Option<String>) -> Result<VpnTestResult, String> {
    init_settings(&app);
    let _busy = app.state::<Vpn>().lock_busy()?;

    let settings = app.state::<AppState>().settings.lock().unwrap().vpn.clone();
    let Some(profile) = (match profile_id {
        Some(id) => settings.profiles.iter().find(|p| p.id == id).cloned(),
        None => settings.active_profile().cloned(),
    }) else {
        return Err("vpn.error.unconfigured".into());
    };

    let (running_handle, running_other) = {
        let vpn = app.state::<Vpn>();
        let guard = vpn.runtime.lock().unwrap();
        match guard.as_ref() {
            Some(a) if a.active_profile_id == profile.id => (Some(a.current_handle()), None),
            Some(a) => {
                // Evita colisão de 2 sessões simultâneas na mesma conta (Proton Free).
                let _ = a.tunnel_shutdown.send(true);
                let other = settings
                    .profiles
                    .iter()
                    .find(|p| p.id == a.active_profile_id)
                    .cloned();
                (None, other)
            }
            None => (None, None),
        }
    };

    let tested_active = running_handle.is_some();
    let res = if let Some(handle) = running_handle {
        let r = tokio::time::timeout(Duration::from_secs(12), run_connectivity_test(&handle))
            .await
            .map_err(|_| "vpn.error.timeout".to_string())
            .and_then(|inner| inner);
        if r.is_err() {
            handle.stats.set_error("vpn.error.timeout");
        }
        r
    } else {
        if running_other.is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let target_id = profile.id.clone();
        let test_res =
            match tauri::async_runtime::spawn_blocking(move || load_profile_secrets(&target_id))
                .await
            {
                Ok(Ok(secrets)) => match build_validated_profile(&profile, secrets) {
                    Ok(validated) => {
                        let (shutdown_tx, shutdown_rx) = watch::channel(false);
                        let handle = spawn_tunnel(validated, shutdown_rx);
                        let r = tokio::time::timeout(
                            Duration::from_secs(12),
                            run_connectivity_test(&handle),
                        )
                        .await
                        .map_err(|_| "vpn.error.timeout".to_string())
                        .and_then(|inner| inner);
                        let _ = shutdown_tx.send(true);
                        r
                    }
                    Err(e) => Err(e),
                },
                Ok(Err(e)) => Err(e),
                Err(_) => Err("vpn.error.keyring".to_string()),
            };

        if let Some(restore_profile) = running_other {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = hot_swap_profile_sync(&app, &restore_profile);
        }
        test_res
    };

    match &res {
        Ok(ms) => {
            set_profile_health(&app, &profile.id, ProfileHealth::Ok(*ms));
            clear_vpn_notice(&app);
        }
        Err(_) => {
            set_profile_health(&app, &profile.id, ProfileHealth::Failed);
            if tested_active && settings.auto_failover && settings.profiles.len() >= 2 {
                let _ = failover_from_profile(&app, &profile.id).await;
            }
        }
    }
    emit_changed(&app);
    res.map(|latency_ms| VpnTestResult { latency_ms })
}

#[tauri::command]
pub fn vpn_open_proton(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(
            "https://account.protonvpn.com/downloads#wireguard-configuration",
            None::<&str>,
        )
        .map_err(|e| e.to_string())
}

fn sanitize_notification_text(raw: &str, max_chars: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(max_chars)
        .collect()
}

#[tauri::command]
pub fn vpn_notify(summary: String, body: String, ok: bool) {
    let summary = sanitize_notification_text(&summary, 120);
    let body = sanitize_notification_text(&body, 300);
    if summary.is_empty() && body.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let mut notification = notify_rust::Notification::new();
        notification.summary(&summary).body(&body).appname("Pulpo");
        #[cfg(target_os = "linux")]
        notification.urgency(if ok {
            notify_rust::Urgency::Normal
        } else {
            notify_rust::Urgency::Critical
        });
        #[cfg(not(target_os = "linux"))]
        let _ = ok;
        if let Err(e) = notification.show() {
            eprintln!("[pulpo] vpn: não foi possível exibir notificação: {e}");
        }
    });
}
