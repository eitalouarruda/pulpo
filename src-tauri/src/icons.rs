//! Descarga el icono de una web para la barra lateral: el mejor `<link
//! rel="…icon…">` de su HTML o, si no hay, `/favicon.ico`. Se guarda como data
//! URL en `services.json`, así que solo se descarga una vez.

use std::io::Read;
use std::time::Duration;

use base64::Engine;
use tauri::Url;

const MAX_HTML: u64 = 2 * 1024 * 1024;
const MAX_ICON: u64 = 512 * 1024;
/// Hay webs (WhatsApp, Google) que devuelven otra página a clientes que no
/// parecen un navegador.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

pub fn fetch(page_url: &str) -> Option<String> {
    let page = Url::parse(page_url).ok()?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();

    let mut candidates = Vec::new();
    if let Ok(html) = get(&agent, page.as_str(), MAX_HTML) {
        let html = String::from_utf8_lossy(&html.0);
        candidates = icon_links(&html, &page);
    }
    candidates.extend(page.join("/favicon.ico").ok());

    candidates.into_iter().find_map(|url| {
        let (bytes, content_type) = get(&agent, url.as_str(), MAX_ICON).ok()?;
        let mime = image_mime(&bytes, content_type.as_deref(), url.path())?;
        Some(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    })
}

fn get(
    agent: &ureq::Agent,
    url: &str,
    limit: u64,
) -> Result<(Vec<u8>, Option<String>), ureq::Error> {
    let response = agent.get(url).header("User-Agent", USER_AGENT).call()?;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(limit)
        .read_to_end(&mut bytes)?;
    Ok((bytes, content_type))
}

/// Los iconos que declara el HTML, del mejor al peor: primero
/// `apple-touch-icon` (suelen ser grandes y sin transparencia rara), después
/// los de mayor tamaño declarado.
fn icon_links(html: &str, base: &Url) -> Vec<Url> {
    let mut found: Vec<(u32, Url)> = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut rest = 0;
    while let Some(start) = lower[rest..].find("<link") {
        let start = rest + start;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + len];
        rest = start + len;

        let attrs = attributes(tag);
        let attr = |name: &str| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let Some(rel) = attr("rel").map(str::to_ascii_lowercase) else {
            continue;
        };
        if !rel.split_ascii_whitespace().any(|r| r.contains("icon")) || rel.contains("mask") {
            continue;
        }
        let Some(url) = attr("href").and_then(|h| base.join(h).ok()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https" | "data") {
            continue;
        }
        let size = attr("sizes")
            .and_then(|s| s.split(['x', 'X']).next()?.parse::<u32>().ok())
            .unwrap_or(16);
        let score = if rel.contains("apple-touch-icon") {
            10_000
        } else {
            0
        } + size.min(512);
        found.push((score, url));
    }
    found.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    found.into_iter().map(|(_, url)| url).collect()
}

/// Atributos `nombre="valor"` (o con comillas simples, o sin comillas) de una
/// etiqueta. Suficiente para `<link>`; no pretende ser un parser de HTML.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let tag = tag.trim_start_matches("<link");
    let mut chars = tag.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !(c.is_ascii_alphabetic()) {
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, c)) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '-' {
                end = j + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let name = tag[i..end].to_string();
        if chars.peek().map(|&(_, c)| c) != Some('=') {
            attrs.push((name, String::new()));
            continue;
        }
        chars.next();
        let value = match chars.peek().map(|&(_, c)| c) {
            Some(q @ ('"' | '\'')) => {
                chars.next();
                let mut v = String::new();
                for (_, c) in chars.by_ref() {
                    if c == q {
                        break;
                    }
                    v.push(c);
                }
                v
            }
            _ => {
                let mut v = String::new();
                while let Some(&(_, c)) = chars.peek() {
                    if c.is_whitespace() || c == '/' {
                        break;
                    }
                    v.push(c);
                    chars.next();
                }
                v
            }
        };
        attrs.push((name, value.replace("&amp;", "&")));
    }
    attrs
}

/// Tipo MIME de una imagen, por cabecera, firma o extensión. `None` si no es
/// una imagen (p. ej. una página de error en HTML).
fn image_mime(bytes: &[u8], content_type: Option<&str>, path: &str) -> Option<&'static str> {
    if bytes.is_empty() {
        return None;
    }
    let by_magic = match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', ..] => Some("image/gif"),
        [0, 0, 1, 0, ..] => Some("image/x-icon"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    };
    if by_magic.is_some() {
        return by_magic;
    }
    let looks_svg = content_type.is_some_and(|t| t.contains("svg")) || path.ends_with(".svg");
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_ascii_lowercase();
    (looks_svg && text.contains("<svg")).then_some("image/svg+xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_best_icon() {
        let base = Url::parse("https://example.com/app/").unwrap();
        let html = r#"<head>
            <link rel="icon" href="/favicon-16.png" sizes="16x16">
            <LINK REL='icon' HREF='icon-192.png' sizes='192x192'>
            <link rel="mask-icon" href="/mask.svg">
            <link rel=stylesheet href=/style.css>
            <link rel="apple-touch-icon" href="https://cdn.example.com/touch.png?a=1&amp;b=2">
        </head>"#;
        let urls: Vec<String> = icon_links(html, &base)
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            urls,
            [
                "https://cdn.example.com/touch.png?a=1&b=2",
                "https://example.com/app/icon-192.png",
                "https://example.com/favicon-16.png",
            ]
        );
    }

    #[test]
    fn detects_images() {
        assert_eq!(image_mime(b"\x89PNG\r\n", None, "/a"), Some("image/png"));
        assert_eq!(
            image_mime(b"\0\0\x01\0", None, "/favicon.ico"),
            Some("image/x-icon")
        );
        assert_eq!(
            image_mime(
                b"<svg xmlns='http://www.w3.org/2000/svg'/>",
                Some("image/svg+xml"),
                "/i"
            ),
            Some("image/svg+xml")
        );
        assert_eq!(
            image_mime(b"<!doctype html><html>", Some("text/html"), "/favicon.ico"),
            None
        );
    }
}
