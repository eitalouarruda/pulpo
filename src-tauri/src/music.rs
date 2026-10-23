//! El callback de CEF solo hace try_send: los getters síncronos del runtime
//! nunca se ejecutan allí. Un trabajador procesa una cola acotada de cambios.
use std::collections::HashMap;
use std::sync::{mpsc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{display, shortcuts, AppState};

pub(crate) struct Update {
    id: String,
    raw: Option<String>,
}

pub struct Player {
    pub(crate) sender: mpsc::SyncSender<Update>,
    tracks: Mutex<HashMap<String, Track>>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Track {
    title: String,
    artist: String,
    artwork: String,
    paused: bool,
    volume: f64,
    #[serde(default)]
    position: f64,
    #[serde(default)]
    duration: f64,
    #[serde(default = "default_rate")]
    rate: f64,
}

fn default_rate() -> f64 {
    1.0
}

#[derive(Clone, Serialize)]
pub struct Snapshot {
    enabled: bool,
    service: Option<String>,
    track: Option<Track>,
}

pub fn is_music(url: &str) -> bool {
    url.parse::<tauri::Url>()
        .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("music.youtube.com"))
}

impl Player {
    pub fn new(app: AppHandle) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Update>(8);
        std::thread::spawn(move || {
            while let Ok(update) = receiver.recv() {
                let state = app.state::<AppState>();
                if !state.settings.lock().unwrap().music_player {
                    continue;
                }
                let track = update.raw.and_then(|raw| {
                    let view = app.get_webview(&update.id)?;
                    // Este getter puede esperar al hilo de UI: solo en el trabajador.
                    if !view.url().is_ok_and(|u| is_music(u.as_str())) {
                        return None;
                    }
                    parse_track(&raw)
                });
                let player = app.state::<Player>();
                let changed = {
                    let mut tracks = player.tracks.lock().unwrap();
                    if let Some(track) = track {
                        if tracks.get(&update.id) == Some(&track) {
                            false
                        } else {
                            tracks.insert(update.id, track);
                            true
                        }
                    } else {
                        tracks.remove(&update.id).is_some()
                    }
                };
                if changed {
                    let _ = app.emit_to("shell", "music-changed", snapshot(&app));
                }
            }
        });
        Self {
            sender,
            tracks: Mutex::default(),
        }
    }
}

fn parse_track(raw: &str) -> Option<Track> {
    let mut track: Track = serde_json::from_str(raw).ok()?;
    if track.title.len() > 1024
        || track.artist.len() > 1024
        || track.artwork.len() > 4096
        || !track.volume.is_finite()
        || !(0.0..=1.0).contains(&track.volume)
        || !track.position.is_finite()
        || !(0.0..=1e9).contains(&track.position)
        || !track.duration.is_finite()
        || !(0.0..=1e9).contains(&track.duration)
        || !track.rate.is_finite()
        || !(0.0..=16.0).contains(&track.rate)
    {
        return None;
    }
    // Solo imágenes HTTPS; el texto se presenta con textContent en el shell.
    if !track
        .artwork
        .parse::<tauri::Url>()
        .is_ok_and(|u| u.scheme() == "https")
    {
        track.artwork.clear();
    }
    Some(track)
}

/// No accede a AppHandle, getters, discos ni mutexes del runtime.
pub(crate) fn receive(sender: &mpsc::SyncSender<Update>, id: &str, title: &str) -> bool {
    let Some(raw) = title
        .strip_prefix(shortcuts::marker())
        .and_then(|s| s.strip_prefix("music:"))
    else {
        return false;
    };
    if raw.len() <= 16384 {
        let _ = sender.try_send(Update {
            id: id.into(),
            raw: Some(raw.into()),
        });
    }
    true
}

pub(crate) fn clear(sender: &mpsc::SyncSender<Update>, id: &str) {
    let _ = sender.try_send(Update {
        id: id.into(),
        raw: None,
    });
}

fn selected_id(app: &AppHandle) -> Option<String> {
    let state = app.state::<AppState>();
    let services = state.services.lock().unwrap();
    let player = app.state::<Player>();
    let tracks = player.tracks.lock().unwrap();
    let candidates = || services.iter().filter(|s| s.enabled && is_music(&s.url));
    candidates()
        .find(|s| tracks.get(&s.id).is_some_and(|t| !t.paused))
        .or_else(|| candidates().next())
        .map(|s| s.id.clone())
}

fn snapshot(app: &AppHandle) -> Snapshot {
    let enabled = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .music_player;
    let service = enabled.then(|| selected_id(app)).flatten();
    let track = service.as_ref().and_then(|id| {
        app.state::<Player>()
            .tracks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
    });
    Snapshot {
        enabled,
        service,
        track,
    }
}

pub fn script(enabled: bool) -> String {
    format!(
        "({})({enabled}, {})",
        include_str!("music.js"),
        serde_json::to_string(shortcuts::marker()).unwrap()
    )
}

#[tauri::command]
pub fn music_get(app: AppHandle) -> Snapshot {
    snapshot(&app)
}

#[tauri::command]
pub async fn music_set_enabled(app: AppHandle, enabled: bool) -> Result<Snapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        {
            let mut settings = state.settings.lock().unwrap();
            let previous = settings.music_player;
            settings.music_player = enabled;
            if let Err(e) = display::save(&state.settings_path, &settings) {
                settings.music_player = previous;
                return Err(e.to_string());
            }
        }
        app.state::<Player>().tracks.lock().unwrap().clear();
        let ids: Vec<_> = state
            .services
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.enabled && is_music(&s.url))
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            if let Some(view) = app.get_webview(&id) {
                // Fuera de callbacks y sin mantener locks durante consultas al runtime.
                if view.url().is_ok_and(|u| is_music(u.as_str())) {
                    view.eval(script(enabled)).map_err(|e| e.to_string())?;
                }
            }
        }
        let result = snapshot(&app);
        let _ = app.emit_to("shell", "music-changed", result.clone());
        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Toggle,
    Next,
    Previous,
    Volume,
    Seek,
}

#[tauri::command]
pub async fn music_control(
    app: AppHandle,
    action: Action,
    volume: Option<f64>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        if !app.state::<AppState>().settings.lock().unwrap().music_player {
            return Err("music.unavailable".into());
        }
        let id = selected_id(&app).ok_or("music.unavailable")?;
        let view = app.get_webview(&id).ok_or("music.unavailable")?;
        if !view.url().is_ok_and(|u| is_music(u.as_str())) {
            return Err("music.unavailable".into());
        }
        let action = match action {
            Action::Toggle => "toggle", Action::Next => "next",
            Action::Previous => "previous", Action::Volume => "volume",
            Action::Seek => "seek",
        };
        let max = if matches!(action, "seek") { 1e9 } else { 1.0 };
        let volume = volume.filter(|v| v.is_finite()).unwrap_or(0.0).clamp(0.0, max);
        view.eval(format!("if (location.origin === 'https://music.youtube.com') window.__pulpoMusic?.control('{action}', {volume});"))
            .map_err(|e| e.to_string())
    }).await.map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_other_origins_and_malformed_tracks() {
        assert!(is_music("https://music.youtube.com/watch?v=x"));
        for url in [
            "http://music.youtube.com/",
            "https://music.youtube.com.evil.org/",
            "https://youtube.com/",
        ] {
            assert!(!is_music(url));
        }
        assert!(parse_track(
            r#"{"title":"song","artist":"artist","artwork":"","paused":false,"volume":2}"#
        )
        .is_none());
        let track = parse_track(r#"{"title":"song","artist":"artist","artwork":"javascript:alert(1)","paused":false,"volume":0.5}"#).unwrap();
        assert!(track.artwork.is_empty());
    }

    #[test]
    fn cef_callback_never_waits_for_a_full_queue() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let title = format!("{}music:{{}}", shortcuts::marker());
        assert!(receive(&tx, "ytmusic", &title));
        // El receptor no consume: send() bloquearía aquí indefinidamente.
        assert!(receive(&tx, "ytmusic", &title));
        clear(&tx, "ytmusic");
        assert!(!receive(&tx, "ytmusic", "YouTube Music"));
    }
}
