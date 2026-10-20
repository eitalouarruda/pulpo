//! Dónde se abre un enlace que una app manda a una ventana nueva.
//!
//! Por defecto va al navegador del sistema (ver `popups`). El usuario puede
//! añadir reglas en `settings.json` para que algunos se abran en una de sus
//! apps: los de `*.atlassian.net` en Jira, los de `wa.me` en WhatsApp…
//!
//! Un patrón es `host` o `host/ruta`, sin esquema (si lo trae se ignora), con
//! `*` como comodín:
//! - el host vale también para sus subdominios: `atlassian.net` cubre
//!   `acme.atlassian.net`;
//! - la ruta es un prefijo: `github.com/acme` cubre `github.com/acme/repo`.

use serde::{Deserialize, Serialize};
use tauri::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRule {
    pub pattern: String,
    /// App donde se abre (su id). Sin valor: el navegador del sistema.
    #[serde(default)]
    pub app: Option<String>,
}

/// Script de inicio de cada app (todos los marcos) en Linux. Con CEF, Ctrl+clic
/// y clic central en un enlace no pasan por `on_new_window`: Chromium abre una
/// ventana suelta que no se ve. Los de `window.open` sí pasan, así que el
/// script los convierte. (`target="_blank"` ya pasa por `on_new_window`.)
#[cfg(target_os = "linux")]
pub const SCRIPT: &str = include_str!("links.js");

/// La primera regla que cubre `url`.
pub fn route<'a>(rules: &'a [LinkRule], url: &Url) -> Option<&'a LinkRule> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    rules.iter().find(|r| matches(&r.pattern, url))
}

/// Deja las reglas como se guardan: patrones sin espacios ni esquema, sin
/// vacíos.
pub fn normalize(rules: Vec<LinkRule>) -> Vec<LinkRule> {
    rules
        .into_iter()
        .filter_map(|r| {
            let pattern = strip_scheme(r.pattern.trim()).trim_end_matches('/');
            (!pattern.is_empty()).then(|| LinkRule {
                pattern: pattern.to_lowercase(),
                app: r.app.filter(|id| !id.is_empty()),
            })
        })
        .collect()
}

fn strip_scheme(pattern: &str) -> &str {
    pattern.split_once("://").map_or(pattern, |(_, rest)| rest)
}

pub fn matches(pattern: &str, url: &Url) -> bool {
    let pattern = pattern.trim().to_lowercase();
    let pattern = strip_scheme(&pattern);
    if pattern.is_empty() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let (host_pattern, path_pattern) = match pattern.find('/') {
        Some(i) => pattern.split_at(i),
        None => (pattern, ""),
    };
    // El host o cualquiera de sus dominios padre.
    let host_ok = std::iter::once(host.as_str())
        .chain(host.match_indices('.').map(|(i, _)| &host[i + 1..]))
        .any(|h| glob(host_pattern, h));
    if !host_ok {
        return false;
    }
    if path_pattern.is_empty() {
        return true;
    }
    let mut path = url.path().to_lowercase();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(&query.to_lowercase());
    }
    glob(&format!("{path_pattern}*"), &path)
}

/// `pattern` cubre todo `text`, con `*` como cualquier secuencia.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    // Última estrella vista y dónde estaba el texto entonces.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Ajusta el enlace a la app que lo abre. Los de `wa.me` (y la página
/// intermedia `api.whatsapp.com/send`) pasan por una pantalla de "continuar
/// al chat": en WhatsApp Web se va directo a `web.whatsapp.com/send`.
pub fn for_app(url: &Url, app_url: &str) -> Url {
    let app_is_whatsapp = Url::parse(app_url)
        .ok()
        .is_some_and(|u| u.host_str() == Some("web.whatsapp.com"));
    if !app_is_whatsapp {
        return url.clone();
    }
    let phone = match url.host_str() {
        Some("wa.me") => url.path().trim_matches('/').to_string(),
        Some("api.whatsapp.com" | "whatsapp.com" | "www.whatsapp.com")
            if url.path().trim_end_matches('/') == "/send" =>
        {
            url.query_pairs()
                .find(|(k, _)| k == "phone")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        }
        _ => return url.clone(),
    };
    let mut out = Url::parse("https://web.whatsapp.com/send").unwrap();
    {
        let mut query = out.query_pairs_mut();
        let phone: String = phone.chars().filter(char::is_ascii_digit).collect();
        if !phone.is_empty() {
            query.append_pair("phone", &phone);
        }
        if let Some((_, text)) = url.query_pairs().find(|(k, _)| k == "text") {
            query.append_pair("text", &text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, url: &str) -> bool {
        matches(pattern, &Url::parse(url).unwrap())
    }

    #[test]
    fn host_and_subdomains() {
        assert!(m("wa.me", "https://wa.me/5511999999999"));
        assert!(m("atlassian.net", "https://acme.atlassian.net/browse/AP-1"));
        assert!(m(
            "https://acme.atlassian.net",
            "https://acme.atlassian.net/"
        ));
        assert!(!m("wa.me", "https://wa.meet.com/"));
        assert!(!m("wa.me", "https://fwa.me/"));
        assert!(!m("wa.me", "https://www.google.com/search?q=wa.me"));
    }

    #[test]
    fn wildcards_and_paths() {
        assert!(m("jira.*", "https://jira.empresa.com/browse/X-1"));
        assert!(m("*jira*", "https://myjira.example.com/"));
        assert!(m(
            "atlassian.net/browse",
            "https://acme.atlassian.net/browse/AP-1"
        ));
        assert!(!m(
            "atlassian.net/browse",
            "https://acme.atlassian.net/wiki/x"
        ));
        assert!(m(
            "*.atlassian.net/*/AP-",
            "https://acme.atlassian.net/browse/AP-12"
        ));
        assert!(m("github.com/acme", "https://github.com/acme/repo?tab=1"));
    }

    #[test]
    fn first_rule_wins_and_only_web_links() {
        let rules = normalize(vec![
            LinkRule {
                pattern: " HTTPS://Acme.Atlassian.net/ ".into(),
                app: Some("jira".into()),
            },
            LinkRule {
                pattern: "atlassian.net".into(),
                app: None,
            },
            LinkRule {
                pattern: "  ".into(),
                app: Some("x".into()),
            },
        ]);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].pattern, "acme.atlassian.net");
        let url = Url::parse("https://acme.atlassian.net/browse/AP-1").unwrap();
        assert_eq!(route(&rules, &url).unwrap().app.as_deref(), Some("jira"));
        let url = Url::parse("https://other.atlassian.net/").unwrap();
        assert_eq!(route(&rules, &url).unwrap().app, None);
        assert!(route(&rules, &Url::parse("mailto:a@acme.atlassian.net").unwrap()).is_none());
    }

    #[test]
    fn whatsapp_links_go_straight_to_the_chat() {
        let wa =
            |u: &str| for_app(&Url::parse(u).unwrap(), "https://web.whatsapp.com/").to_string();
        assert_eq!(
            wa("https://wa.me/5511999999999?text=hola%20que%20tal"),
            "https://web.whatsapp.com/send?phone=5511999999999&text=hola+que+tal"
        );
        assert_eq!(
            wa("https://api.whatsapp.com/send?phone=+55 11 9999&text=x"),
            "https://web.whatsapp.com/send?phone=55119999&text=x"
        );
        assert_eq!(wa("https://example.com/"), "https://example.com/");
        let other = for_app(
            &Url::parse("https://wa.me/1").unwrap(),
            "https://acme.atlassian.net/",
        );
        assert_eq!(other.as_str(), "https://wa.me/1");
    }
}
