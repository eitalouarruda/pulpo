//! Script que se inyecta al inicio de cada documento de un servicio.

/// `injected_script` son los scriptlets de uBO que adblock-rust ya resolvió
/// (funciones + llamadas). Esperan una variable global `scriptletGlobals`, que
/// uBO define al inyectarlos; aquí se define dentro del mismo cierre.
pub(crate) fn build(injected_script: &str, hide_selectors: &[String]) -> String {
    // Una regla por selector: si uno es inválido, el navegador descarta solo
    // esa regla y no la hoja entera.
    let css: String = hide_selectors
        .iter()
        .map(|s| format!("{s}{{display:none!important}}\n"))
        .collect();
    let css = serde_json::to_string(&css).expect("string");

    format!(
        r#"(function () {{
  if (window.__pulpoBlocker) return;
  window.__pulpoBlocker = true;
  (function () {{
    const scriptletGlobals = {{}};
{injected_script}
  }})();
  const css = {css};
  if (!css) return;
  const addStyle = () => {{
    const style = document.createElement('style');
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);
  }};
  if (document.documentElement) addStyle();
  else document.addEventListener('readystatechange', addStyle, {{ once: true }});
}})();
"#
    )
}
