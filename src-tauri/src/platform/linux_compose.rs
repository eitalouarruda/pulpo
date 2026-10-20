//! Tecla Compose y teclas muertas en las páginas.
//!
//! El CEF con códecs propietarios del AppImage (paquete `cef-vaapi` de Arch) se
//! compila con `use_gtk=false`, así que Chromium no tiene método de entrada en
//! X11: la tecla Compose (p. ej. `compose:rctrl`) llega a la página como un
//! `keydown` con `key` "Compose", las muertas con `key` "Dead", y los
//! caracteres siguientes se escriben tal cual (`'` + `a` en vez de `á`).
//!
//! Se suple desde la página: Rust lee del servidor X qué tecla muerta hay en
//! cada posición y nivel (normal, Mayús, AltGr, AltGr+Mayús) y la tabla de
//! compose del locale con libxkbcommon (la misma que usa el resto del sistema,
//! con `~/.XCompose` y `XCOMPOSEFILE`), y un script de inicio compone la
//! secuencia y la escribe con `insertText`. Con un CEF que sí trae GTK las
//! teclas llegan como `Process` (`keyCode` 229) y el script no hace nada.

use std::{
    collections::BTreeMap,
    ffi::{c_char, c_int, c_void, CStr, CString},
    fmt::Write as _,
    sync::OnceLock,
};

use x11_dl::xlib;

const MULTI_KEY: u32 = 0xff20;

fn is_dead(keysym: u32) -> bool {
    (0xfe50..=0xfe93).contains(&keysym)
}

/// Cómo se nombra en la tabla del script una tecla que no escribe nada (Compose
/// o muerta): un carácter del plano privado 15, que no sale de ningún teclado.
fn token(keysym: u32) -> Option<char> {
    (keysym == MULTI_KEY || is_dead(keysym))
        .then(|| char::from_u32(0xf0000 + (keysym & 0xffff)))
        .flatten()
}

/// El script de inicio, o `None` si no se pudo leer la tabla de compose.
pub fn compose_script() -> Option<&'static str> {
    static SCRIPT: OnceLock<Option<String>> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            let table = compose_table()?;
            Some(script(&dead_keys().unwrap_or_default(), &table))
        })
        .as_deref()
}

/// Las teclas muertas por `KeyboardEvent.code`, con su token en cada nivel.
type DeadKeys = BTreeMap<&'static str, [Option<char>; 4]>;

fn dead_keys() -> Option<DeadKeys> {
    let xlib = xlib::Xlib::open().ok()?;
    let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
    if display.is_null() {
        return None;
    }
    let (mut min, mut max) = (0, 0);
    unsafe { (xlib.XDisplayKeycodes)(display, &mut min, &mut max) };
    let count = max - min + 1;
    let mut per_keycode = 0;
    let map = unsafe { (xlib.XGetKeyboardMapping)(display, min as u8, count, &mut per_keycode) };
    let mut keys = DeadKeys::new();
    if !map.is_null() && per_keycode > 0 {
        let per_keycode = per_keycode as usize;
        let rows = unsafe { std::slice::from_raw_parts(map, count as usize * per_keycode) };
        for (keycode, row) in (min as u32..).zip(rows.chunks(per_keycode)) {
            // Columnas del mapa base que genera XKB: grupo 1 normal y Mayús, grupo 2
            // normal y Mayús, y luego los niveles 3 y 4 (AltGr) del grupo 1.
            let levels = [0, 1, 4, 5].map(|column| {
                let keysym = row.get(column).copied().unwrap_or(0) as u32;
                is_dead(keysym).then(|| token(keysym)).flatten()
            });
            if levels.iter().any(Option::is_some) {
                if let Some(code) = keycode.checked_sub(8).and_then(dom_code) {
                    keys.insert(code, levels);
                }
            }
        }
    }
    unsafe {
        if !map.is_null() {
            (xlib.XFree)(map.cast());
        }
        (xlib.XCloseDisplay)(display);
    }
    Some(keys)
}

/// `KeyboardEvent.code` de un código de tecla de evdev (el de X menos 8), para
/// las teclas que escriben algo (donde caen las teclas muertas).
fn dom_code(evdev: u32) -> Option<&'static str> {
    const DIGITS: [&str; 10] = [
        "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9",
        "Digit0",
    ];
    const ROW_Q: [&str; 10] = [
        "KeyQ", "KeyW", "KeyE", "KeyR", "KeyT", "KeyY", "KeyU", "KeyI", "KeyO", "KeyP",
    ];
    const ROW_A: [&str; 9] = [
        "KeyA", "KeyS", "KeyD", "KeyF", "KeyG", "KeyH", "KeyJ", "KeyK", "KeyL",
    ];
    const ROW_Z: [&str; 7] = ["KeyZ", "KeyX", "KeyC", "KeyV", "KeyB", "KeyN", "KeyM"];
    Some(match evdev {
        2..=11 => DIGITS[evdev as usize - 2],
        12 => "Minus",
        13 => "Equal",
        16..=25 => ROW_Q[evdev as usize - 16],
        26 => "BracketLeft",
        27 => "BracketRight",
        30..=38 => ROW_A[evdev as usize - 30],
        39 => "Semicolon",
        40 => "Quote",
        41 => "Backquote",
        43 => "Backslash",
        44..=50 => ROW_Z[evdev as usize - 44],
        51 => "Comma",
        52 => "Period",
        53 => "Slash",
        57 => "Space",
        86 => "IntlBackslash",
        89 => "IntlRo",
        124 => "IntlYen",
        _ => return None,
    })
}

/// La tabla de compose del locale como líneas `secuencia\tresultado`, con las
/// teclas Compose y muertas como tokens (ver [`token`]) y el resto como el
/// carácter que escriben.
fn compose_table() -> Option<String> {
    let entries = unsafe { read_compose_table() }?;
    let mut table = String::new();
    'entries: for (sequence, result) in entries {
        if sequence.first().and_then(|&(k, _)| token(k)).is_none()
            || result.is_empty()
            || result.contains(['\t', '\n'])
        {
            continue;
        }
        let mut line = String::new();
        for (keysym, utf32) in sequence {
            match token(keysym).or_else(|| char::from_u32(utf32).filter(|c| !c.is_control())) {
                Some(c) => line.push(c),
                None => continue 'entries,
            }
        }
        let _ = writeln!(table, "{line}\t{result}");
    }
    Some(table)
}

type Context = c_void;
type Table = c_void;
type Iterator = c_void;
type Entry = c_void;

/// Las entradas de la tabla: cada tecla de la secuencia con su carácter UTF-32
/// (0 si no escribe nada), y el texto que resulta.
#[allow(clippy::type_complexity)]
unsafe fn read_compose_table() -> Option<Vec<(Vec<(u32, u32)>, String)>> {
    // libxkbcommon ya viene con GTK (y dentro del AppImage). La iteración de
    // tablas es de la 1.6.
    let lib = unsafe { libloading::Library::new("libxkbcommon.so.0") }.ok()?;
    macro_rules! sym {
        ($name:ident: $ty:ty) => {
            let $name: libloading::Symbol<$ty> =
                unsafe { lib.get(concat!(stringify!($name), "\0").as_bytes()) }.ok()?;
        };
    }
    sym!(xkb_context_new: unsafe extern "C" fn(c_int) -> *mut Context);
    sym!(xkb_context_unref: unsafe extern "C" fn(*mut Context));
    sym!(xkb_compose_table_new_from_locale: unsafe extern "C" fn(*mut Context, *const c_char, c_int) -> *mut Table);
    sym!(xkb_compose_table_unref: unsafe extern "C" fn(*mut Table));
    sym!(xkb_compose_table_iterator_new: unsafe extern "C" fn(*mut Table) -> *mut Iterator);
    sym!(xkb_compose_table_iterator_free: unsafe extern "C" fn(*mut Iterator));
    sym!(xkb_compose_table_iterator_next: unsafe extern "C" fn(*mut Iterator) -> *mut Entry);
    sym!(xkb_compose_table_entry_sequence: unsafe extern "C" fn(*mut Entry, *mut usize) -> *const u32);
    sym!(xkb_compose_table_entry_utf8: unsafe extern "C" fn(*mut Entry) -> *const c_char);
    sym!(xkb_keysym_to_utf32: unsafe extern "C" fn(u32) -> u32);

    let context = unsafe { xkb_context_new(0) };
    if context.is_null() {
        return None;
    }
    // El AppImage solo trae la tabla de en_US.UTF-8: si la del locale no está,
    // esa (la de casi todos los locales UTF-8 es la misma).
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|l| !l.is_empty()))
        .unwrap_or_else(|| "C".into());
    let table = [locale.as_str(), "en_US.UTF-8"]
        .into_iter()
        .filter_map(|l| CString::new(l).ok())
        .map(|l| unsafe { xkb_compose_table_new_from_locale(context, l.as_ptr(), 0) })
        .find(|t| !t.is_null());
    let Some(table) = table else {
        unsafe { xkb_context_unref(context) };
        return None;
    };

    let mut entries = Vec::new();
    let iterator = unsafe { xkb_compose_table_iterator_new(table) };
    if !iterator.is_null() {
        loop {
            let entry = unsafe { xkb_compose_table_iterator_next(iterator) };
            if entry.is_null() {
                break;
            }
            let mut len = 0;
            let keysyms = unsafe { xkb_compose_table_entry_sequence(entry, &mut len) };
            let utf8 = unsafe { xkb_compose_table_entry_utf8(entry) };
            if keysyms.is_null() || utf8.is_null() {
                continue;
            }
            let sequence = unsafe { std::slice::from_raw_parts(keysyms, len) }
                .iter()
                .map(|&k| (k, unsafe { xkb_keysym_to_utf32(k) }))
                .collect();
            let result = unsafe { CStr::from_ptr(utf8) }
                .to_string_lossy()
                .into_owned();
            entries.push((sequence, result));
        }
        unsafe { xkb_compose_table_iterator_free(iterator) };
    }
    unsafe {
        xkb_compose_table_unref(table);
        xkb_context_unref(context);
    }
    Some(entries)
}

fn script(dead: &DeadKeys, table: &str) -> String {
    let compose = serde_json::to_string(&token(MULTI_KEY)).expect("token serializable");
    let dead = serde_json::to_string(dead).expect("teclas serializables");
    let table = serde_json::to_string(table).expect("tabla serializable");
    format!(
        r#"(() => {{
  if (window.__pulpoCompose) return;
  window.__pulpoCompose = true;
  const COMPOSE = {compose};
  // Teclas muertas por posición, en cada nivel (normal, Mayús, AltGr, AltGr+Mayús).
  const DEAD = {dead};
  const TABLE = {table};
  let map = null;
  let prefixes = null;
  const load = () => {{
    map = new Map();
    prefixes = new Set();
    for (const line of TABLE.split("\n")) {{
      const tab = line.indexOf("\t");
      if (tab < 0) continue;
      const seq = line.slice(0, tab);
      map.set(seq, line.slice(tab + 1));
      const chars = [...seq];
      for (let i = 1; i < chars.length; i++) prefixes.add(chars.slice(0, i).join(""));
    }}
  }};
  const MODIFIERS = new Set(["Shift", "Control", "Alt", "AltGraph", "Meta", "CapsLock", "NumLock", "Super", "Hyper", "Fn"]);
  // Como lo haría el motor al escribir: los editores con modelo propio
  // (Slate en Discord, Draft…) escuchan `beforeinput`/`textInput` e insertan
  // ellos; `execCommand` solo toca el DOM sin avisar y al redibujar lo
  // revierten. Si nadie lo atiende, se inserta con `execCommand`.
  const insert = (text) => {{
    const el = document.activeElement;
    const target = el && el !== document.body ? el : document;
    const before = new InputEvent("beforeinput", {{
      bubbles: true, cancelable: true, composed: true, inputType: "insertText", data: text,
    }});
    if (!target.dispatchEvent(before)) return;
    if (typeof TextEvent === "function") {{
      const textInput = document.createEvent("TextEvent");
      textInput.initTextEvent("textInput", true, true, window, text);
      if (!target.dispatchEvent(textInput)) return;
    }}
    if (document.execCommand("insertText", false, text)) return;
    if (el && typeof el.setRangeText === "function" && typeof el.selectionStart === "number") {{
      el.setRangeText(text, el.selectionStart, el.selectionEnd, "end");
      el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
    }}
  }};
  let seq = "";
  window.addEventListener("keydown", (e) => {{
    // Con método de entrada, la composición es del motor.
    if (e.isComposing || e.keyCode === 229) {{
      seq = "";
      return;
    }}
    let token = null;
    if (e.key === "Compose") token = COMPOSE;
    else if (e.key === "Dead") {{
      const levels = DEAD[e.code];
      token = levels && levels[(e.getModifierState("AltGraph") ? 2 : 0) + (e.shiftKey ? 1 : 0)];
      if (!token) return;
    }} else {{
      if (!seq || MODIFIERS.has(e.key)) return;
      if ([...e.key].length !== 1 || e.ctrlKey || e.metaKey) {{
        seq = "";
        return;
      }}
      token = e.key;
    }}
    e.preventDefault();
    e.stopImmediatePropagation();
    if (e.repeat) return;
    if (!map) load();
    const next = seq + token;
    const out = map.get(next);
    seq = out === undefined && prefixes.has(next) ? next : "";
    if (out !== undefined) insert(out);
  }}, true);
  window.addEventListener("blur", () => {{ seq = ""; }});
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_only_for_compose_and_dead_keys() {
        assert_eq!(token(MULTI_KEY), Some('\u{fff20}'));
        assert_eq!(token(0xfe51), Some('\u{ffe51}')); // dead_acute
        assert_eq!(token(0x61), None); // a
    }

    #[test]
    fn maps_dead_key_positions() {
        assert_eq!(dom_code(40), Some("Quote"));
        assert_eq!(dom_code(41), Some("Backquote"));
        assert_eq!(dom_code(11), Some("Digit0"));
        assert_eq!(dom_code(50), Some("KeyM"));
        assert_eq!(dom_code(1), None);
    }
}
