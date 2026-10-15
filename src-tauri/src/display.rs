//! Tamaño de la interfaz por pantalla.
//!
//! En Linux Pulpo corre por XWayland (el Chromium embebido usa X11). Con
//! Hyprland y `xwayland:force_zero_scaling = true` las apps X11 se dibujan a
//! escala 1 en todas las pantallas, así que en una pantalla con escala 1,6 se
//! ven un 60 % más pequeñas. Para compensarlo se aplica zoom a los webviews:
//! por defecto la escala que Hyprland da a esa pantalla, y si el usuario lo
//! ajusta, lo que eligió para ella (se guarda por nombre de pantalla).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MIN_ZOOM: f64 = 0.5;
pub const MAX_ZOOM: f64 = 3.0;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Zoom elegido por el usuario, por nombre de pantalla (`eDP-1`, …).
    #[serde(default)]
    pub zoom: HashMap<String, f64>,
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

pub fn load(path: &Path) -> Settings {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, settings: &Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)?;
    std::fs::rename(tmp, path)
}

/// Nombre de la pantalla como lo da el compositor: en XWayland llega como
/// `"eDP-1 (eDP-1)"`.
pub fn monitor_name(raw: &str) -> String {
    raw.split(" (").next().unwrap_or(raw).trim().to_string()
}

pub fn clamp(zoom: f64) -> f64 {
    // Pasos de 5 %: evita acumular errores al sumar 0,1 muchas veces.
    ((zoom.clamp(MIN_ZOOM, MAX_ZOOM)) * 20.0).round() / 20.0
}

/// Zoom automático para una pantalla: la escala que le da el compositor
/// cuando este no escala las apps X11. 1 si no se sabe.
pub fn auto_zoom(monitor: &str) -> f64 {
    #[cfg(target_os = "linux")]
    if let Some(scale) = hyprland::xwayland_unscaled_monitor_scale(monitor) {
        return clamp(scale);
    }
    let _ = monitor;
    1.0
}

#[cfg(target_os = "linux")]
mod hyprland {
    //! Consulta a Hyprland por su socket de control (lo mismo que `hyprctl`).

    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use serde::Deserialize;

    fn request(command: &str) -> Option<Vec<u8>> {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let socket = format!("{runtime}/hypr/{signature}/.socket.sock");
        let mut stream = UnixStream::connect(socket).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        stream.write_all(command.as_bytes()).ok()?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).ok()?;
        Some(reply)
    }

    #[derive(Deserialize)]
    struct Monitor {
        name: String,
        scale: f64,
    }

    /// Según la versión de Hyprland, una opción booleana llega como `bool` o
    /// como `int`.
    #[derive(Deserialize)]
    struct BoolOption {
        #[serde(default)]
        bool: Option<bool>,
        #[serde(default)]
        int: Option<i64>,
    }

    impl BoolOption {
        fn enabled(&self) -> bool {
            self.bool.unwrap_or(false) || self.int.is_some_and(|i| i != 0)
        }
    }

    /// Escala de `monitor` si Hyprland no escala las apps X11
    /// (`xwayland:force_zero_scaling`); si las escala él, no hay que hacer nada.
    pub fn xwayland_unscaled_monitor_scale(monitor: &str) -> Option<f64> {
        let option: BoolOption =
            serde_json::from_slice(&request("j/getoption xwayland:force_zero_scaling")?).ok()?;
        if !option.enabled() {
            return None;
        }
        let monitors: Vec<Monitor> = serde_json::from_slice(&request("j/monitors")?).ok()?;
        monitors
            .into_iter()
            .find(|m| m.name == monitor)
            .map(|m| m.scale)
            .filter(|s| s.is_finite() && *s > 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_five_percent_steps() {
        assert_eq!(clamp(1.6), 1.6);
        assert_eq!(clamp(1.0 + 0.1 + 0.1 + 0.1), 1.3);
        assert_eq!(clamp(0.1), MIN_ZOOM);
        assert_eq!(clamp(9.0), MAX_ZOOM);
    }

    #[test]
    fn cleans_monitor_names() {
        assert_eq!(monitor_name("eDP-1 (eDP-1)"), "eDP-1");
        assert_eq!(monitor_name("DP-1"), "DP-1");
    }

    #[test]
    fn roundtrips_settings() {
        let dir = std::env::temp_dir().join(format!("pulpo-settings-{}", std::process::id()));
        let path = settings_path(&dir);
        let mut settings = Settings::default();
        settings.zoom.insert("eDP-1".into(), 1.5);
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).zoom.get("eDP-1"), Some(&1.5));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
