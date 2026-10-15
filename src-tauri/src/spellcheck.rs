//! Corrector ortográfico en varios idiomas. Por defecto Chromium (y WebView2)
//! solo usan el idioma del sistema o el inglés, y subrayan como error todo lo
//! que se escribe en otro.

/// Idiomas del corrector: los de la interfaz de Pulpo.
pub const LANGUAGES: &[&str] = &["en-US", "es", "pt-BR"];

/// Linux (CEF): lista de diccionarios del perfil; Chromium los descarga la
/// primera vez que hacen falta.
#[cfg(target_os = "linux")]
pub fn dictionaries() -> serde_json::Value {
    serde_json::json!(LANGUAGES)
}

/// Windows (WebView2): el corrector usa los idiomas preferidos del navegador.
/// Se pone primero el del sistema, para que las webs se sigan viendo en él, y
/// después los de la interfaz. Hay que repetir los argumentos que wry pasa por
/// defecto, porque estos los reemplazan.
#[cfg(windows)]
pub fn webview2_args() -> String {
    let mut langs: Vec<String> = system_locale().into_iter().collect();
    for lang in LANGUAGES {
        if !langs.iter().any(|l| l.eq_ignore_ascii_case(lang)) {
            langs.push((*lang).to_string());
        }
    }
    format!(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --accept-lang={}",
        langs.join(",")
    )
}

#[cfg(windows)]
fn system_locale() -> Option<String> {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    (len > 1).then(|| String::from_utf16_lossy(&buf[..len as usize - 1]))
}
