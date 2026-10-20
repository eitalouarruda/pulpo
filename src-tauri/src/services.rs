//! Los servicios web que muestra Pulpo: los que el usuario agregó, guardados en
//! `services.json`, y el catálogo de apps conocidas para agregarlas rápido.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    /// También es la etiqueta del webview y el nombre de su perfil.
    pub id: String,
    pub name: String,
    pub url: String,
    /// Color de la letra en la barra lateral cuando no hay icono.
    pub color: String,
    /// Icono como data URL: el favicon de la web o una imagen elegida.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_source: IconSource,
    /// Grupo de la barra lateral. Los de un mismo grupo van juntos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Bloqueo de anuncios (red, cosmético y scriptlets).
    pub adblock: bool,
    /// Desactivado: sin webview, no consume memoria.
    pub enabled: bool,
    /// No molestar: sin avisos ni sonidos en sus horarios o mientras está
    /// silenciada a mano. Los contadores siguen.
    #[serde(default, skip_serializing_if = "Dnd::is_empty")]
    pub dnd: Dnd,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dnd {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<DndSchedule>,
    /// Silenciada a mano hasta este instante (segundos Unix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

/// Un horario semanal. Si `end` no es posterior a `start` pasa la medianoche:
/// cuenta como del día en que empieza (lunes 22:00 a 08:00 = hasta el martes a
/// las 8). Igual inicio y fin: el día entero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DndSchedule {
    /// Días en que empieza: 0 = lunes … 6 = domingo.
    pub days: Vec<u8>,
    /// Minutos desde la medianoche.
    pub start: u16,
    pub end: u16,
}

impl Dnd {
    fn is_empty(&self) -> bool {
        self.schedules.is_empty() && self.until.is_none()
    }

    /// Activo en este instante: `weekday` (0 = lunes), `minute` desde la
    /// medianoche (hora local) y `now` en segundos Unix.
    pub fn active(&self, weekday: u8, minute: u16, now: i64) -> bool {
        self.until.is_some_and(|until| now < until)
            || self.schedules.iter().any(|s| s.active(weekday, minute))
    }

    /// Horarios válidos: días sin repetir dentro de la semana, minutos dentro
    /// del día y sin horarios sin días.
    pub fn clean_schedules(schedules: Vec<DndSchedule>) -> Vec<DndSchedule> {
        schedules
            .into_iter()
            .filter_map(|mut s| {
                s.days.retain(|d| *d < 7);
                s.days.sort_unstable();
                s.days.dedup();
                (!s.days.is_empty() && s.start < 24 * 60 && s.end < 24 * 60).then_some(s)
            })
            .collect()
    }
}

impl DndSchedule {
    fn active(&self, weekday: u8, minute: u16) -> bool {
        let today = self.days.contains(&weekday);
        if self.start == self.end {
            return today;
        }
        if self.start < self.end {
            return today && (self.start..self.end).contains(&minute);
        }
        let yesterday = self.days.contains(&((weekday + 6) % 7));
        (today && minute >= self.start) || (yesterday && minute < self.end)
    }
}

/// De dónde sale el icono de la barra lateral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSource {
    /// El favicon de la web, descargado automáticamente.
    #[default]
    Web,
    /// Una imagen que eligió el usuario.
    Custom,
    /// La inicial con color, sin imagen.
    Letter,
}

/// Una app conocida del catálogo.
#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub name: &'static str,
    pub url: &'static str,
    pub color: &'static str,
    /// Lo que viene marcado al agregarla. En apps de mensajería no aporta
    /// nada y cada petición pasaría por el motor sin necesidad.
    pub adblock: bool,
}

const fn preset(
    name: &'static str,
    url: &'static str,
    color: &'static str,
    adblock: bool,
) -> Preset {
    Preset {
        name,
        url,
        color,
        adblock,
    }
}

pub const CATALOG: &[Preset] = &[
    preset("WhatsApp", "https://web.whatsapp.com/", "#25d366", false),
    preset("Telegram", "https://web.telegram.org/a/", "#2aabee", false),
    preset("Discord", "https://discord.com/app", "#5865f2", false),
    preset("Slack", "https://app.slack.com/client", "#611f69", false),
    preset("Messenger", "https://www.messenger.com/", "#0084ff", true),
    preset("Instagram", "https://www.instagram.com/", "#e1306c", true),
    preset(
        "Microsoft Teams",
        "https://teams.microsoft.com/",
        "#5059c9",
        false,
    ),
    preset("Gmail", "https://mail.google.com/", "#ea4335", true),
    preset("Outlook", "https://outlook.live.com/mail/", "#0078d4", true),
    preset(
        "Google Calendar",
        "https://calendar.google.com/",
        "#4285f4",
        false,
    ),
    preset(
        "YouTube Music",
        "https://music.youtube.com/",
        "#ff0033",
        true,
    ),
    preset("YouTube", "https://www.youtube.com/", "#ff0000", true),
    preset("X", "https://x.com/", "#14171a", true),
    preset("LinkedIn", "https://www.linkedin.com/", "#0a66c2", true),
    preset("ChatGPT", "https://chatgpt.com/", "#10a37f", false),
    preset("Claude", "https://claude.ai/", "#d97757", false),
    preset("Notion", "https://www.notion.so/", "#37352f", false),
    preset("Trello", "https://trello.com/", "#0079bf", false),
    preset("GitHub", "https://github.com/", "#24292f", false),
];

/// Los de la primera vez. Los ids coinciden con los perfiles que ya existían.
fn defaults() -> Vec<Service> {
    vec![
        Service {
            id: "whatsapp".into(),
            name: "WhatsApp".into(),
            url: "https://web.whatsapp.com/".into(),
            color: "#25d366".into(),
            icon: None,
            icon_source: IconSource::Web,
            group: None,
            adblock: false,
            enabled: true,
            dnd: Dnd::default(),
        },
        Service {
            id: "ytmusic".into(),
            name: "YouTube Music".into(),
            url: "https://music.youtube.com/".into(),
            color: "#ff0033".into(),
            icon: None,
            icon_source: IconSource::Web,
            group: None,
            adblock: true,
            enabled: true,
            dnd: Dnd::default(),
        },
    ]
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("services.json")
}

/// Lee `services.json`; si no existe (primera vez) devuelve los de fábrica.
pub fn load(path: &Path) -> Vec<Service> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            eprintln!(
                "[pulpo] {} inválido ({e}); se usan los de fábrica",
                path.display()
            );
            defaults()
        }),
        Err(_) => defaults(),
    }
}

/// Escritura atómica: un corte a mitad no deja el archivo a medias.
pub fn save(path: &Path, services: &[Service]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(services)?)?;
    std::fs::rename(tmp, path)
}

/// Deja los servicios de cada grupo juntos, sin cambiar el orden relativo:
/// primero los que no tienen grupo, después los grupos en el orden en que
/// aparece su primer servicio, y cada servicio dentro del suyo en el orden en
/// que estaba.
pub fn group_together(services: &mut [Service]) {
    // Los que no tienen grupo, siempre arriba.
    let mut order: Vec<Option<String>> = vec![None];
    for s in services.iter() {
        if !order.contains(&s.group) {
            order.push(s.group.clone());
        }
    }
    services.sort_by_key(|s| order.iter().position(|g| *g == s.group));
}

/// Nombre de grupo limpio: sin espacios sobrantes, vacío = sin grupo.
pub fn normalize_group(group: Option<String>) -> Option<String> {
    group
        .map(|g| g.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|g| !g.is_empty())
}

/// Icono elegido por el usuario: solo imágenes como data URL y de tamaño
/// razonable (se guardan dentro de `services.json`).
pub fn validate_icon(icon: &str) -> Result<(), String> {
    const MAX: usize = 400 * 1024;
    if !icon.starts_with("data:image/") || !icon.contains(";base64,") {
        return Err("error.iconNotImage".into());
    }
    if icon.len() > MAX {
        return Err("error.iconTooLarge".into());
    }
    Ok(())
}

/// Id nuevo, válido como etiqueta de webview y como nombre de carpeta.
pub fn new_id(services: &[Service]) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut n = (nanos as u64) & 0xffff_ffff;
    loop {
        let id = format!("s-{n:08x}");
        if !services.iter().any(|s| s.id == id) {
            return id;
        }
        n = n.wrapping_add(1);
    }
}

/// Valida y normaliza la URL que escribe el usuario: sin esquema se asume
/// https, y solo se aceptan http(s). Los errores son claves `error.*` que
/// traduce la interfaz.
pub fn normalize_url(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("error.urlEmpty".into());
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url = tauri::Url::parse(&with_scheme).map_err(|_| "error.urlInvalid".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("error.urlScheme".into());
    }
    Ok(url.into())
}

/// Color de la paleta para una app sin color propio, estable por nombre.
pub fn color_for(name: &str, url: &str) -> String {
    if let Some(p) = CATALOG.iter().find(|p| p.url == url) {
        return p.color.into();
    }
    const PALETTE: &[&str] = &[
        "#e5484d", "#f76b15", "#ffc53d", "#46a758", "#12a594", "#0090ff", "#3e63dd", "#8e4ec6",
        "#d6409f",
    ];
    let hash = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[hash as usize % PALETTE.len()].into()
}

/// Saca el número de "(3) WhatsApp". `None` si el título no lleva contador.
pub fn badge_from_title(title: &str) -> Option<u32> {
    let rest = title.trim_start().strip_prefix('(')?;
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_badge() {
        assert_eq!(badge_from_title("(3) WhatsApp"), Some(3));
        assert_eq!(badge_from_title("(12) WhatsApp Business"), Some(12));
        assert_eq!(badge_from_title("WhatsApp"), None);
        assert_eq!(badge_from_title("(Live) concert"), None);
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("web.telegram.org").unwrap(),
            "https://web.telegram.org/"
        );
        assert_eq!(
            normalize_url(" https://app.slack.com/client ").unwrap(),
            "https://app.slack.com/client"
        );
        assert!(normalize_url("").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn keeps_groups_together() {
        let mk = |id: &str, group: Option<&str>| Service {
            group: group.map(Into::into),
            id: id.into(),
            ..defaults().remove(0)
        };
        let mut list = vec![
            mk("a", None),
            mk("b", Some("Trabajo")),
            mk("c", None),
            mk("d", Some("Ocio")),
            mk("e", Some("Trabajo")),
        ];
        group_together(&mut list);
        let ids: Vec<_> = list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "c", "b", "e", "d"]);
        assert_eq!(
            normalize_group(Some("  Mi   grupo ".into())),
            Some("Mi grupo".into())
        );
        assert_eq!(normalize_group(Some("   ".into())), None);
    }

    #[test]
    fn validates_icons() {
        assert!(validate_icon("data:image/png;base64,iVBOR").is_ok());
        assert!(validate_icon("data:text/html;base64,PGh0bWw+").is_err());
        assert!(validate_icon("https://example.com/a.png").is_err());
    }

    #[test]
    fn ids_are_unique_labels() {
        let services = defaults();
        let id = new_id(&services);
        assert!(id.starts_with("s-") && id.len() == 10);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn dnd_schedules() {
        let night = DndSchedule {
            days: vec![0, 1, 2, 3, 4], // lunes a viernes
            start: 22 * 60,
            end: 8 * 60,
        };
        let dnd = Dnd {
            schedules: vec![night],
            until: None,
        };
        assert!(dnd.active(0, 23 * 60, 0)); // lunes 23:00
        assert!(dnd.active(1, 7 * 60, 0)); // martes 07:00 (del lunes)
        assert!(!dnd.active(1, 9 * 60, 0)); // martes 09:00
        assert!(dnd.active(5, 7 * 60, 0)); // sábado 07:00 (del viernes)
        assert!(!dnd.active(5, 23 * 60, 0)); // sábado 23:00
        assert!(!dnd.active(0, 7 * 60, 0)); // lunes 07:00 (domingo no cuenta)

        let lunch = Dnd {
            schedules: vec![DndSchedule {
                days: vec![2],
                start: 12 * 60,
                end: 13 * 60,
            }],
            until: None,
        };
        assert!(lunch.active(2, 12 * 60 + 30, 0));
        assert!(!lunch.active(2, 13 * 60, 0));

        let all_day = Dnd {
            schedules: vec![DndSchedule {
                days: vec![6],
                start: 0,
                end: 0,
            }],
            until: None,
        };
        assert!(all_day.active(6, 0, 0) && all_day.active(6, 1439, 0));
        assert!(!all_day.active(0, 0, 0));

        let manual = Dnd {
            schedules: vec![],
            until: Some(100),
        };
        assert!(manual.active(0, 0, 99));
        assert!(!manual.active(0, 0, 100));

        let cleaned = Dnd::clean_schedules(vec![
            DndSchedule {
                days: vec![3, 9, 1, 3],
                start: 60,
                end: 120,
            },
            DndSchedule {
                days: vec![],
                start: 60,
                end: 120,
            },
            DndSchedule {
                days: vec![1],
                start: 1440,
                end: 0,
            },
        ]);
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].days, [1, 3]);
    }

    #[test]
    fn roundtrips_config() {
        let dir = std::env::temp_dir().join(format!("pulpo-test-{}", std::process::id()));
        let path = config_path(&dir);
        save(&path, &defaults()).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert!(!loaded[0].adblock && loaded[1].adblock);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
