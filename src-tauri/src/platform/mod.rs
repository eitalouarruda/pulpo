//! Bloqueo de red a nivel del motor web de cada sistema.
//!
//! - Linux (CEF/Chromium): se pausan las peticiones con el dominio `Fetch` del
//!   DevTools Protocol y se consulta el motor de Brave en cada una.
//! - Windows (WebView2): se intercepta `WebResourceRequested` y se consulta el
//!   motor de Brave en cada petición.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod cef;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use cef::*;
#[cfg(target_os = "linux")]
mod linux_compose;
#[cfg(target_os = "linux")]
pub use linux_compose::compose_script;
#[cfg(target_os = "linux")]
mod linux_drag;
#[cfg(target_os = "linux")]
pub use linux_drag::enable_drag;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;
