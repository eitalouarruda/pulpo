//! Ajustes de la interfaz: tamaño por pantalla, app de la página inicial y
//! reglas de enlaces (`links`).
//!
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

/// Parte de la altura de la ventana que ocupa la app de la página inicial.
pub const DEFAULT_HOME_PANE: f64 = 0.5;
const MIN_HOME_PANE: f64 = 0.15;
/// Arriba queda sitio para una fila de iconos.
const MAX_HOME_PANE: f64 = 0.92;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Zoom elegido por el usuario, por nombre de pantalla (`eDP-1`, …).
    #[serde(default)]
    pub zoom: HashMap<String, f64>,
    /// App que se ve en la mitad inferior de la página inicial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_app: Option<String>,
    /// Altura de esa app, como parte de la ventana (sin valor: la mitad).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_pane: Option<f64>,
    /// Enlaces que se abren en una app en vez de en el navegador.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<crate::links::LinkRule>,
    /// Cuentas de GitHub y repos vigilados en la vista CI (los tokens van al llavero).
    #[serde(default, skip_serializing_if = "crate::ci::CiSettings::is_empty")]
    pub ci: crate::ci::CiSettings,
    /// Botón de Bitwarden y Ctrl+Mayús+L (ver `bitwarden`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bitwarden: bool,
    /// Fondo de la página inicial: un archivo de `<datos>/wallpapers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallpaper: Option<String>,
}

impl Settings {
    pub fn home_pane(&self) -> f64 {
        self.home_pane.map_or(DEFAULT_HOME_PANE, clamp_home_pane)
    }
}

pub fn clamp_home_pane(size: f64) -> f64 {
    if size.is_finite() {
        size.clamp(MIN_HOME_PANE, MAX_HOME_PANE)
    } else {
        DEFAULT_HOME_PANE
    }
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

/// Pide al compositor el foco para la ventana de Pulpo (y que cambie al
/// escritorio donde está). Solo hace algo con Hyprland.
#[cfg(target_os = "linux")]
pub fn focus_own_window() {
    hyprland::focus_window(std::process::id());
}

/// Devuelve el foco a la ventana de Pulpo si Hyprland se lo da a una de sus
/// ventanas auxiliares (ver `hyprland::guard_focus`). Anota cada vez en `log`.
#[cfg(target_os = "linux")]
pub fn guard_focus(log: std::path::PathBuf) {
    std::thread::spawn(move || hyprland::guard_focus(std::process::id(), &log));
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

    /// Despacha `focuswindow` con `target` (`pid:…`, `address:…`), en la
    /// sintaxis clásica o, si no responde `ok`, en la de Lua.
    fn dispatch_focus(target: &str) {
        let ok = |reply: Option<Vec<u8>>| reply.is_some_and(|r| r.trim_ascii() == b"ok");
        if !ok(request(&format!("dispatch focuswindow {target}"))) {
            let _ = request(&format!(
                r#"dispatch hl.dsp.focus({{ window = "{target}" }})"#
            ));
        }
    }

    #[derive(Deserialize)]
    struct Client {
        address: String,
        pid: i64,
        title: String,
    }

    /// Chromium y el runtime crean ventanas X auxiliares sin título ni tipo
    /// (1×1, `override-redirect`). Si Hyprland le da el foco a una, el teclado
    /// deja de llegar a las apps aunque Pulpo se vea activa, y un clic no lo
    /// arregla (para Hyprland la ventana ya tiene el foco); cambiar de
    /// escritorio y volver sí. Esto hace lo mismo solo: escucha los cambios de
    /// ventana activa y, si es una de Pulpo sin título, enfoca la principal.
    pub fn guard_focus(pid: u32, log: &std::path::Path) {
        use std::io::BufRead;
        let (Ok(signature), Ok(runtime)) = (
            std::env::var("HYPRLAND_INSTANCE_SIGNATURE"),
            std::env::var("XDG_RUNTIME_DIR"),
        ) else {
            return;
        };
        let socket = format!("{runtime}/hypr/{signature}/.socket2.sock");
        loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                for line in std::io::BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    if let Some(address) = line.strip_prefix("activewindowv2>>") {
                        check_focus(pid, address, log);
                    }
                }
            }
            // Hyprland reiniciado o socket cerrado: se reintenta.
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn check_focus(pid: u32, address: &str, log: &std::path::Path) {
        let address = address.trim().trim_start_matches("0x");
        if address.is_empty() || address == "," {
            return;
        }
        let Some(clients) =
            request("j/clients").and_then(|r| serde_json::from_slice::<Vec<Client>>(&r).ok())
        else {
            return;
        };
        let ours = |c: &&Client| c.pid == pid as i64;
        let Some(active) = clients
            .iter()
            .filter(ours)
            .find(|c| c.address.trim_start_matches("0x") == address)
        else {
            return;
        };
        if !active.title.is_empty() {
            return;
        }
        let Some(main) = clients.iter().filter(ours).find(|c| c.title == "Pulpo") else {
            return;
        };
        note(
            log,
            &format!(
                "foco en una ventana auxiliar ({}); se devuelve a {}",
                active.address, main.address
            ),
        );
        dispatch_focus(&format!("address:{}", main.address));
    }

    /// Una línea con la hora en `log` (que no crece sin límite).
    fn note(log: &std::path::Path, message: &str) {
        use std::io::Write as _;
        eprintln!("[pulpo] {message}");
        if std::fs::metadata(log).is_ok_and(|m| m.len() > 64 * 1024) {
            let _ = std::fs::remove_file(log);
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(file, "{secs} {message}");
        }
    }

    /// Enfoca la ventana del proceso `pid`. Con la configuración en Lua
    /// (Hyprland 0.55+) `dispatch` espera la sintaxis de `hl.dsp` y rechaza la
    /// clásica: se prueba la clásica y, si no responde `ok`, la de Lua.
    pub fn focus_window(pid: u32) {
        dispatch_focus(&format!("pid:{pid}"));
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
    fn clamps_home_pane() {
        assert_eq!(Settings::default().home_pane(), DEFAULT_HOME_PANE);
        assert_eq!(clamp_home_pane(0.05), MIN_HOME_PANE);
        assert_eq!(clamp_home_pane(0.95), MAX_HOME_PANE);
        assert_eq!(clamp_home_pane(f64::NAN), DEFAULT_HOME_PANE);
        assert_eq!(clamp_home_pane(0.4), 0.4);
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
