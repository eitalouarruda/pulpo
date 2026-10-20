//! Qué hacer cuando una web abre una ventana nueva.
//!
//! Los logins con Google, Apple, Microsoft… abren una ventana emergente y
//! esperan que vuelva a la página que la abrió (`window.opener`). Si se abre en
//! el navegador del sistema ese vínculo se pierde y el login no termina, así
//! que esas se abren dentro de Pulpo, con la misma sesión del servicio. El
//! resto (enlaces normales con `target="_blank"`) van al navegador.

use tauri::Url;

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// Ventana emergente de Pulpo, ligada a la página que la abrió.
    InApp,
    /// Navegador del sistema.
    Browser,
}

/// Proveedores de login: (dominio, prefijo de ruta).
const AUTH: &[(&str, &str)] = &[
    ("accounts.google.com", "/"),
    ("appleid.apple.com", "/"),
    ("login.microsoftonline.com", "/"),
    ("login.live.com", "/"),
    ("login.microsoft.com", "/"),
    ("github.com", "/login"),
    ("gitlab.com", "/users/sign_in"),
    ("gitlab.com", "/oauth"),
    ("www.facebook.com", "/dialog"),
    ("www.facebook.com", "/v"),
    ("www.facebook.com", "/login"),
    ("discord.com", "/oauth2"),
    ("slack.com", "/oauth"),
    ("slack.com", "/openid"),
    ("x.com", "/i/oauth2"),
    ("api.twitter.com", "/oauth"),
    ("twitter.com", "/i/oauth2"),
    ("www.linkedin.com", "/oauth"),
    ("accounts.spotify.com", "/"),
    ("id.atlassian.com", "/"),
    ("login.yahoo.com", "/"),
    ("auth.openai.com", "/"),
];

/// Dominios de proveedores genéricos de identidad (`*.auth0.com`, …).
const AUTH_SUFFIXES: &[&str] = &[".auth0.com", ".okta.com", ".onelogin.com", ".b2clogin.com"];

/// `sized`: la página pidió tamaño para la ventana (`window.open(url, name,
/// "width=…,height=…")`), lo típico de una ventana emergente de login o de
/// pago, no de un enlace.
pub fn target(url: &Url, sized: bool) -> Target {
    if sized || is_login(url) {
        Target::InApp
    } else {
        Target::Browser
    }
}

/// Ventana que tiene que quedar ligada a la página que la abrió: un login, o
/// una en blanco que la página llenará después.
pub fn is_login(url: &Url) -> bool {
    url.as_str() == "about:blank" || is_auth(url)
}

fn is_auth(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let path = url.path();
    AUTH.iter()
        .any(|(h, prefix)| host == *h && path.starts_with(prefix))
        || AUTH_SUFFIXES.iter().any(|s| host.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str, sized: bool) -> Target {
        target(&Url::parse(url).unwrap(), sized)
    }

    #[test]
    fn logins_stay_in_app() {
        assert_eq!(
            t(
                "https://accounts.google.com/o/oauth2/v2/auth?client_id=x",
                false
            ),
            Target::InApp
        );
        assert_eq!(
            t("https://appleid.apple.com/auth/authorize", false),
            Target::InApp
        );
        assert_eq!(
            t("https://github.com/login/oauth/authorize", false),
            Target::InApp
        );
        assert_eq!(t("https://acme.auth0.com/authorize", false), Target::InApp);
        assert_eq!(t("https://pay.example.com/checkout", true), Target::InApp);
    }

    #[test]
    fn links_go_to_browser() {
        assert_eq!(
            t("https://github.com/tauri-apps/tauri", false),
            Target::Browser
        );
        assert_eq!(
            t("https://www.google.com/search?q=pulpo", false),
            Target::Browser
        );
        assert_eq!(t("https://example.com/", false), Target::Browser);
    }
}
