//! Actualizaciones desde las releases de GitHub.
//!
//! Al arrancar se mira la última release (`releases/latest`); si es más nueva
//! que esta, la barra lateral pregunta. Nunca se actualiza sin permiso.
//!
//! - Linux (AppImage): se descarga junto a la actual y la reemplaza, de modo que
//!   quede una sola. Si el nombre del archivo lleva la versión
//!   (`Pulpo_0.1.7_amd64.AppImage`), la nueva se llama con la suya y la vieja se
//!   borra; si no (`pulpo.AppImage`, renombrada por un integrador), se reemplaza
//!   en el mismo sitio y los accesos del menú siguen valiendo.
//! - Windows: se descarga el instalador NSIS y se ejecuta en modo pasivo
//!   (`/P`: solo la barra de progreso; `/UPDATE`: conserva datos y accesos;
//!   `/R`: abre Pulpo al terminar).
//!
//! La versión nueva arranca cuando este proceso ya terminó: con las dos vivas,
//! Chromium no abre los perfiles.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

const LATEST: &str = "https://api.github.com/repos/eitalouarruda/pulpo/releases/latest";
const USER_AGENT: &str = "Pulpo";

/// Release más nueva que la instalada, con el archivo para este sistema.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    asset_name: String,
    asset_url: String,
    asset_size: u64,
}

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
}

#[derive(Clone, Serialize)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
}

impl Release {
    pub fn info(&self) -> UpdateInfo {
        UpdateInfo {
            version: self.version.clone(),
        }
    }
}

/// Si esta instalación sabe actualizarse sola: una AppImage en Linux, siempre
/// en Windows. En desarrollo solo con `PULPO_UPDATE_CHECK=1`.
pub fn supported() -> bool {
    if cfg!(debug_assertions) && !crate::diag::flag("PULPO_UPDATE_CHECK") {
        return false;
    }
    if cfg!(target_os = "linux") {
        return appimage().is_some();
    }
    cfg!(windows)
}

/// La última release, si es más nueva que `current` y trae archivo para este
/// sistema. Cualquier fallo (sin red, límite de la API…) cuenta como "nada".
pub fn check(current: &str) -> Option<Release> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let body = agent
        .get(LATEST)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let release: Value = serde_json::from_str(&body).ok()?;
    let version = release["tag_name"].as_str()?.trim_start_matches('v');
    if !newer(version, current) {
        return None;
    }
    release["assets"]
        .as_array()?
        .iter()
        .find(|asset| asset["name"].as_str().is_some_and(is_installer))
        .and_then(|asset| {
            Some(Release {
                version: version.to_string(),
                asset_name: asset["name"].as_str()?.to_string(),
                asset_url: asset["browser_download_url"].as_str()?.to_string(),
                asset_size: asset["size"].as_u64()?,
            })
        })
}

/// El archivo de la release que sirve aquí.
fn is_installer(name: &str) -> bool {
    if cfg!(windows) {
        name.ends_with("-setup.exe")
    } else {
        name.ends_with(".AppImage")
    }
}

/// `a` es una versión posterior a `b` (números separados por puntos).
fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|n| n.parse().ok())
            .collect()
    };
    parse(a) > parse(b)
}

/// La AppImage que se está ejecutando (la ruta la pone su runtime).
fn appimage() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

/// Descarga la release, la deja lista y programa el arranque de la versión
/// nueva para cuando este proceso termine. Después hay que salir de la app.
/// `progress` recibe lo descargado de vez en cuando.
pub fn install(
    release: &Release,
    current: &str,
    progress: impl Fn(Progress),
) -> Result<(), String> {
    let destination = if cfg!(windows) {
        std::env::temp_dir().join(&release.asset_name)
    } else {
        let running = appimage().ok_or("update.notAppImage")?;
        appimage_target(&running, current, &release.version)
    };
    let dir = destination.parent().ok_or("update.notWritable")?;
    // Se descarga a un archivo oculto de la misma carpeta: el cambio final es
    // un `rename`, que no deja nunca una AppImage a medias.
    let partial = dir.join(format!(".{}.part", release.asset_name));
    let result = download(release, &partial, progress)
        .and_then(|()| finish(&partial, &destination, current));
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

/// Dónde queda la AppImage nueva: con la versión nueva en el nombre si la
/// actual lleva la suya; si no, en el mismo sitio que la actual.
fn appimage_target(running: &Path, current: &str, version: &str) -> PathBuf {
    match running.file_name().and_then(|n| n.to_str()) {
        Some(name) if name.contains(current) => {
            running.with_file_name(name.replace(current, version))
        }
        _ => running.to_path_buf(),
    }
}

fn download(release: &Release, path: &Path, progress: impl Fn(Progress)) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .build()
        .into();
    let response = agent
        .get(&release.asset_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| e.to_string())?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(path).map_err(|_| "update.notWritable")?;
    let total = release.asset_size;
    let mut downloaded = 0u64;
    let mut reported = 0u64;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        downloaded += n as u64;
        // Un aviso por cada 1 % (y el último).
        if downloaded - reported >= total / 100 || downloaded == total {
            reported = downloaded;
            progress(Progress { downloaded, total });
        }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    if downloaded != total {
        return Err("update.incomplete".into());
    }
    Ok(())
}

/// Linux: pone la AppImage en su sitio (ejecutable), borra la vieja si se
/// llama distinto y arranca la nueva cuando Pulpo termine.
#[cfg(not(windows))]
fn finish(partial: &Path, destination: &Path, _current: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    let running = appimage().ok_or("update.notAppImage")?;
    std::fs::set_permissions(partial, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    std::fs::rename(partial, destination).map_err(|_| "update.notWritable")?;
    if destination != running {
        // La AppImage en marcha sigue montada: borrar su archivo no la afecta.
        let _ = std::fs::remove_file(&running);
    }
    // Un `sh` aparte (y en su propio grupo, para que no muera con Pulpo)
    // espera a que este proceso termine y arranca la versión nueva. Al cerrarse
    // CEF puede caerse y dejar procesos suyos huérfanos (el zygote) con la
    // caché abierta: se les da un momento y si siguen, se terminan; con ellos
    // vivos la versión nueva no abre bien los perfiles ni la caché de la GPU.
    // Sin las variables del runtime de esta AppImage, que apuntan a su montaje,
    // y sin el diálogo de AppImageLauncher ("¿integrar?"): su daemon actualiza
    // solo la entrada del menú al ver el archivo nuevo en su carpeta.
    let cef_root = crate::cef_root().ok_or("update.notAppImage")?;
    Command::new("/bin/sh")
        .arg("-c")
        .arg(RELAUNCH)
        .arg("sh")
        .arg(std::process::id().to_string())
        .arg(destination)
        .arg(&cef_root)
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("ARGV0")
        .env_remove("OWD")
        .env("APPIMAGELAUNCHER_DISABLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `$1`: este proceso; `$2`: la AppImage nueva; `$3`: la raíz de Chromium,
/// con la que se reconocen sus procesos (`--user-data-dir=…`). El patrón se
/// arma aquí dentro para que no aparezca tal cual en la línea de comandos de
/// este `sh` y `pgrep` no lo encuentre a él.
#[cfg(not(windows))]
const RELAUNCH: &str = r#"
while kill -0 "$1" 2>/dev/null; do sleep 0.3; done
cef="--user-data-dir=$3"
i=0
while pgrep -f -- "$cef" >/dev/null && [ $i -lt 10 ]; do sleep 0.3; i=$((i + 1)); done
pkill -f -- "$cef" && sleep 1
exec "$2"
"#;

/// Windows: ejecuta el instalador cuando Pulpo termine (si lo encuentra
/// abierto, el instalador pide cerrarlo).
#[cfg(windows)]
fn finish(partial: &Path, destination: &Path, _current: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let _ = std::fs::remove_file(destination);
    std::fs::rename(partial, destination).map_err(|e| e.to_string())?;
    let installer = destination.display().to_string().replace('\'', "''");
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; \
         Start-Process -FilePath '{installer}' -ArgumentList '/P','/R','/UPDATE'",
        std::process::id()
    );
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script.as_str(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.1.9", "0.1.8"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("0.2.0", "0.1.99"));
        assert!(!newer("0.1.8", "0.1.8"));
        assert!(!newer("0.1.7", "0.1.8"));
    }

    #[test]
    fn names_the_new_appimage() {
        let dir = Path::new("/home/u/Applications");
        assert_eq!(
            appimage_target(&dir.join("Pulpo_0.1.7_amd64.AppImage"), "0.1.7", "0.1.9"),
            dir.join("Pulpo_0.1.9_amd64.AppImage")
        );
        // Renombrada por el usuario o por un integrador: en el mismo sitio.
        assert_eq!(
            appimage_target(&dir.join("pulpo.appimage"), "0.1.7", "0.1.9"),
            dir.join("pulpo.appimage")
        );
    }
}
