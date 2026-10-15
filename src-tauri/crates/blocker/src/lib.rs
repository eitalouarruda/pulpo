//! Bloqueo de anuncios para los servicios de Pulpo.
//!
//! Usa el motor de Brave (`adblock`) con las listas que Brave activa por
//! defecto, más los scriptlets de uBlock Origin (`+js(...)`), que son los que
//! quitan la publicidad de YouTube: esos anuncios vienen dentro de las
//! respuestas JSON del reproductor y no se pueden bloquear por URL.

mod lists;
mod page_script;

use std::path::Path;

use adblock::lists::{FilterSet, ParseOptions, RuleTypes};
use adblock::request::Request;
use adblock::resources::{PermissionMask, Resource};
use adblock::Engine;

pub use lists::{ListSource, DEFAULT_LISTS};

/// Bit de permiso de las listas de confianza (uBO y Brave). Los scriptlets
/// `trusted-*` lo exigen; ver `scripts/update-ubo-resources.mjs`.
const TRUSTED: PermissionMask = PermissionMask::from_bits(1);

static UBO_RESOURCES: &str = include_str!("../resources/ubo-resources.json");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no hay ninguna lista de filtros disponible (¿sin conexión en el primer arranque?)")]
    NoLists,
    #[error("recursos de uBO inválidos: {0}")]
    Resources(#[from] serde_json::Error),
    #[error("no se pudieron convertir las reglas al formato de WebKit")]
    ContentBlocking,
}

/// Texto de las listas, ya descargado (o leído de la caché).
pub struct Lists {
    sources: Vec<(String, bool)>,
}

impl Lists {
    /// Lee las listas de `cache_dir`, descargando las que falten o tengan más
    /// de un día. Si la red falla se usa la copia vieja.
    pub fn load(cache_dir: &Path) -> Result<Self, Error> {
        let sources = lists::load_all(cache_dir);
        if sources.is_empty() {
            return Err(Error::NoLists);
        }
        Ok(Self { sources })
    }

    fn filter_set(&self, debug: bool, rule_types: RuleTypes) -> FilterSet {
        let mut set = FilterSet::new(debug);
        for (text, trusted) in &self.sources {
            set.add_filter_list(
                text.clone(),
                ParseOptions {
                    rule_types,
                    permissions: if *trusted {
                        TRUSTED
                    } else {
                        PermissionMask::default()
                    },
                    ..ParseOptions::default()
                },
            );
        }
        set
    }

    /// Identificador estable del contenido de las listas, para saber si una
    /// compilación guardada (p. ej. el content filter de WebKit) sigue valiendo.
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (text, trusted) in &self.sources {
            text.hash(&mut h);
            trusted.hash(&mut h);
        }
        UBO_RESOURCES.hash(&mut h);
        format!("pulpo-{:016x}", h.finish())
    }

    /// Reglas de red en el formato JSON de content blockers de WebKit/Safari.
    #[cfg(feature = "content-blocking")]
    pub fn content_blocking_json(&self) -> Result<String, Error> {
        let (rules, _unsupported) = self
            .filter_set(true, RuleTypes::NetworkOnly)
            .into_content_blocking()
            .map_err(|()| Error::ContentBlocking)?;
        Ok(serde_json::to_string(&rules)?)
    }
}

#[derive(serde::Deserialize)]
struct UboResources {
    resources: Vec<Resource>,
}

pub struct Blocker {
    engine: Engine,
}

impl Blocker {
    pub fn new(lists: &Lists) -> Result<Self, Error> {
        let mut engine = Engine::new_with_filter_set(lists.filter_set(false, RuleTypes::All));
        let ubo: UboResources = serde_json::from_str(UBO_RESOURCES)?;
        engine.use_resources(ubo.resources);
        Ok(Self { engine })
    }

    /// ¿Hay que bloquear esta petición? `request_type` usa los nombres de
    /// adblock (`script`, `image`, `xmlhttprequest`, `document`, …).
    pub fn should_block(&self, url: &str, source_url: &str, request_type: &str) -> bool {
        match Request::new(url, source_url, request_type, "GET") {
            Ok(request) => self.engine.check_network_request(&request).should_block(),
            Err(_) => false,
        }
    }

    /// Script a inyectar al inicio de cada documento de `page_url`: scriptlets
    /// de uBO y CSS que oculta los elementos de publicidad.
    pub fn page_script(&self, page_url: &str) -> String {
        let res = self.engine.url_cosmetic_resources(page_url);
        let mut selectors: Vec<_> = res.hide_selectors.into_iter().collect();
        selectors.sort();
        page_script::build(&res.injected_script, &selectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocker(rules: &str) -> Blocker {
        let lists = Lists {
            sources: vec![(rules.to_string(), true)],
        };
        Blocker::new(&lists).unwrap()
    }

    #[test]
    fn blocks_network_rule() {
        let b = blocker("||ads.example.com^");
        assert!(b.should_block(
            "https://ads.example.com/x.js",
            "https://music.youtube.com/",
            "script"
        ));
        assert!(!b.should_block(
            "https://cdn.example.org/x.js",
            "https://music.youtube.com/",
            "script"
        ));
    }

    #[test]
    fn injects_ubo_scriptlet_with_dependencies() {
        let b = blocker("music.youtube.com##+js(json-prune, playerResponse.adPlacements)");
        let script = b.page_script("https://music.youtube.com/");
        assert!(script.contains("function jsonPrune("));
        assert!(script.contains("function safeSelf("));
        assert!(script.contains("jsonPrune(\"playerResponse.adPlacements\")"));
        assert!(script.contains("scriptletGlobals"));
    }

    #[test]
    fn trusted_scriptlet_needs_trusted_list() {
        let rule = "example.com##+js(trusted-set-constant, foo, 1)";
        let untrusted = Blocker::new(&Lists {
            sources: vec![(rule.to_string(), false)],
        })
        .unwrap();
        assert!(!untrusted
            .page_script("https://example.com/")
            .contains("trustedSetConstant"));
        assert!(blocker(rule)
            .page_script("https://example.com/")
            .contains("trustedSetConstant"));
    }

    #[test]
    fn hides_cosmetic_selectors() {
        let b = blocker("music.youtube.com##ytmusic-mealbar-promo-renderer");
        let script = b.page_script("https://music.youtube.com/");
        assert!(script.contains("ytmusic-mealbar-promo-renderer"));
    }

    #[cfg(feature = "content-blocking")]
    #[test]
    fn exports_webkit_content_blocking_rules() {
        let lists = Lists {
            sources: vec![("||ads.example.com^\n".to_string(), true)],
        };
        let json = lists.content_blocking_json().unwrap();
        assert!(json.contains("\"action\""));
        assert!(json.contains("ads\\\\.example\\\\.com"));
    }
}
