//! Salir de Pulpo del todo, sin dejar procesos del motor web vivos.
//!
//! Al cerrar, el motor web a veces no termina con Pulpo: en Linux CEF puede
//! caerse y dejar huérfanos (el zygote y los renderers, con YouTube Music
//! sonando); en Windows quedaban los `msedgewebview2.exe` de cada perfil
//! ("Utilitário" sueltos en el Administrador de tareas). Por eso, al salir:
//!
//! 1. Se paran los sonidos de todas las páginas, por si algo queda vivo un rato.
//! 2. Linux: se guardan las cookies (ver `platform::flush_cookies`) y, al
//!    terminar Pulpo, un `sh` aparte termina los procesos de Chromium que
//!    sigan con su `--user-data-dir`. Windows: se cierran los webviews, se
//!    espera a que los navegadores de WebView2 (hijos de Pulpo) terminen solos
//!    y si no, se terminan.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager};

static QUITTING: AtomicBool = AtomicBool::new(false);

/// Para el sonido de la página (y de sus marcos, ver `volume`).
const SILENCE: &str = r#"(() => {
  window.__pulpoVolume && window.__pulpoVolume(0);
  for (const media of document.querySelectorAll("audio, video")) media.pause();
})();"#;

/// Sale de Pulpo. No se llama desde el hilo principal ni desde un aviso del
/// motor web: espera a los webviews.
pub fn quit(app: &AppHandle) {
    if QUITTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let webviews: Vec<_> = app
        .webviews()
        .into_iter()
        .filter(|(label, _)| label != "shell")
        .map(|(_, webview)| webview)
        .collect();
    // Que desaparezca ya: lo que sigue (guardar, cerrar el motor web) tarda.
    if let Some(window) = app.get_window("main") {
        let _ = window.hide();
    }
    for webview in &webviews {
        let _ = webview.eval(SILENCE);
    }
    #[cfg(target_os = "linux")]
    crate::platform::flush_cookies(&webviews, Duration::from_secs(5));
    #[cfg(windows)]
    {
        let children = win::webview_browsers();
        for webview in &webviews {
            let _ = webview.close();
        }
        win::wait_or_terminate(&children, Duration::from_secs(4));
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = (webviews, Duration::ZERO);
    app.exit(0);
}

/// Sale de Pulpo desde cualquier hilo.
pub fn quit_async(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || quit(&app));
}

/// Al terminar el bucle de eventos, pase por `quit` o no (Linux: cerrar la
/// ventana desde el gestor): que nada del motor web siga vivo.
pub fn on_exit(app: &AppHandle) {
    let _ = app;
    #[cfg(target_os = "linux")]
    crate::badge::clear(app);
    #[cfg(target_os = "linux")]
    if !crate::update::relaunching() {
        linux::reap_after_exit();
    }
    #[cfg(windows)]
    win::wait_or_terminate(&win::webview_browsers(), Duration::ZERO);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    /// `$1`: este proceso; `$2`: la opción `--user-data-dir=…` de Chromium; el
    /// resto, sus procesos al salir. Cuando este proceso termina les da 3 s para
    /// terminar solos; luego SIGTERM y, si aún siguen, SIGKILL. Solo a esos (y
    /// si todavía tienen esa opción): los de un Pulpo abierto justo después no
    /// se tocan.
    const REAP: &str = r#"
pulpo=$1; cef=$2; shift 2
ours() { grep -qaF -- "$cef" "/proc/$1/cmdline" 2>/dev/null; }
left() { for p in "$@"; do ours "$p" && echo "$p"; done; }
while kill -0 "$pulpo" 2>/dev/null; do sleep 0.2; done
i=0
while [ -n "$(left "$@")" ] && [ $i -lt 15 ]; do sleep 0.2; i=$((i + 1)); done
pids=$(left "$@")
[ -n "$pids" ] || exit 0
kill $pids 2>/dev/null
sleep 1
pids=$(left $pids)
[ -n "$pids" ] && kill -KILL $pids 2>/dev/null
exit 0
"#;

    /// Los procesos con esta opción en su línea de órdenes, salvo este.
    fn processes_with(option: &str) -> Vec<u32> {
        let me = std::process::id();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        dir.filter_map(|entry| {
            let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let matches = cmdline
                .split(|&b| b == 0)
                .any(|arg| arg == option.as_bytes());
            (matches && pid != me).then_some(pid)
        })
        .collect()
    }

    pub fn reap_after_exit() {
        let Some(cef_root) = crate::cef_root() else {
            return;
        };
        let option = format!("--user-data-dir={}", cef_root.display());
        let pids = processes_with(&option);
        if pids.is_empty() {
            return;
        }
        let spawned = Command::new("/bin/sh")
            .arg("-c")
            .arg(REAP)
            .arg("sh")
            .arg(std::process::id().to_string())
            .arg(&option)
            .args(pids.iter().map(u32::to_string))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // En su propio grupo, para que no muera con Pulpo.
            .process_group(0)
            .spawn();
        if let Err(e) = spawned {
            eprintln!("[pulpo] no se pudo vigilar el cierre de Chromium: {e}");
        }
    }
}

#[cfg(windows)]
mod win {
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };

    /// `GetExitCodeProcess` de un proceso que sigue vivo.
    const STILL_ACTIVE: u32 = 259;

    /// Los navegadores de WebView2 que lanzó Pulpo (uno por perfil): sus
    /// renderers y demás procesos terminan con ellos.
    pub fn webview_browsers() -> Vec<u32> {
        let me = std::process::id();
        let mut found = Vec::new();
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return found;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if entry.th32ParentProcessID == me
                    && name.eq_ignore_ascii_case("msedgewebview2.exe")
                {
                    found.push(entry.th32ProcessID);
                }
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        found
    }

    /// Espera hasta `timeout` a que terminen y termina los que sigan.
    pub fn wait_or_terminate(pids: &[u32], timeout: Duration) {
        let handles: Vec<HANDLE> = pids
            .iter()
            .filter_map(|&pid| unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                    false,
                    pid,
                )
                .ok()
            })
            .collect();
        let alive = |handle: HANDLE| {
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok() && code == STILL_ACTIVE
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && handles.iter().any(|&h| alive(h)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        for handle in handles {
            if alive(handle) {
                let _ = unsafe { TerminateProcess(handle, 0) };
            }
            let _ = unsafe { CloseHandle(handle) };
        }
    }
}
