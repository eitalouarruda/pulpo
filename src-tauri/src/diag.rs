//! Interruptores de diagnóstico por variable de entorno, para aislar qué
//! afecta al rendimiento:
//!
//! - `PULPO_NO_NETFILTER=1`: sin bloqueo de red.
//! - `PULPO_NO_SCRIPTS=1`: sin scriptlets ni CSS cosmético.
//! - `PULPO_BENCH=1`: navega solo por YouTube Music y escribe los tiempos
//!   por stdout (`[bench] ...`).
//! - `PULPO_UA=<user agent>`: sustituye el User-Agent de los servicios.
//! - `PULPO_LOG_BLOCKED=1`: escribe por stderr cada petición bloqueada (Linux).
//! - `PULPO_DEBUG_PORT=<puerto>`: abre el DevTools remoto de Chromium (Linux).
//! - `PULPO_UPDATE_CHECK=1`: busca actualizaciones también en desarrollo.
//! - `PULPO_DATA_HOME=<carpeta>`: instancia de pruebas con sus propios datos,
//!   caché y tokens de CI (`./dev.sh --test`, Linux).

pub fn flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v != "0")
}

pub fn test_home() -> Option<std::path::PathBuf> {
    std::env::var_os("PULPO_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(Into::into)
}

pub const BENCH_SCRIPT: &str = include_str!("bench.js");

pub fn user_agent() -> Option<String> {
    std::env::var("PULPO_UA").ok().filter(|ua| !ua.is_empty())
}

#[cfg(target_os = "linux")]
pub fn debug_port() -> Option<u16> {
    std::env::var("PULPO_DEBUG_PORT").ok()?.parse().ok()
}
