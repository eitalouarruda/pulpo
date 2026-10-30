//! Total de mensajes sin leer en el icono de Pulpo de la barra de tareas: la
//! barra lateral suma los contadores de las apps y lo manda aquí.
//!
//! - Windows: icono superpuesto (`ITaskbarList3::SetOverlayIcon`), un círculo
//!   rojo con el número dibujado aquí mismo (sin fuentes: dígitos de 3×5).
//! - Linux: contador de la API de lanzadores de Unity (Dash to Dock, Plank,
//!   KDE…), y además `<caché>/badge.json` (`{"count": N, "pid": P}`) para
//!   barras que no la leen (un widget de la barra de Omarchy lo vigila). El
//!   archivo existe mientras Pulpo está abierto: se escribe al arrancar y se
//!   borra al salir (`clear`).

use tauri::{AppHandle, Manager};

/// Linux: deja el total en `<caché>/badge.json`, de una vez (archivo temporal
/// y `rename`) para que quien lo vigile no lea uno a medias.
#[cfg(target_os = "linux")]
pub fn publish(app: &AppHandle, count: u32) {
    let Ok(dir) = app.path().app_cache_dir() else {
        return;
    };
    let body = format!(r#"{{"count":{count},"pid":{}}}"#, std::process::id());
    let tmp = dir.join("badge.json.tmp");
    let written = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, dir.join("badge.json")));
    if let Err(e) = written {
        eprintln!("[pulpo] no se pudo escribir badge.json: {e}");
    }
}

/// Linux: al salir, sin archivo (Pulpo ya no está abierto).
#[cfg(target_os = "linux")]
pub fn clear(app: &AppHandle) {
    if let Ok(dir) = app.path().app_cache_dir() {
        let _ = std::fs::remove_file(dir.join("badge.json"));
    }
}

#[tauri::command]
pub fn set_app_badge(app: AppHandle, count: u32) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    #[cfg(windows)]
    {
        let rgba = (count > 0).then(|| overlay(count));
        let icon = rgba
            .as_deref()
            .map(|rgba| tauri::image::Image::new(rgba, SIZE, SIZE));
        if let Err(e) = window.set_overlay_icon(icon) {
            eprintln!("[pulpo] no se pudo poner el contador en la barra de tareas: {e}");
        }
    }
    #[cfg(not(windows))]
    let _ = window.set_badge_count((count > 0).then_some(i64::from(count)));
    #[cfg(target_os = "linux")]
    publish(&app, count);
}

/// Lado del icono superpuesto. Windows lo muestra a 16 px por cada 100 % de
/// escala: 32 se ve nítido hasta 200 %.
#[cfg(any(windows, test))]
const SIZE: u32 = 32;

/// Dígitos y `+` de 3×5, una fila por elemento (bit 2 = columna izquierda).
#[cfg(any(windows, test))]
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '4' => [5, 5, 7, 1, 1],
        '5' => [7, 4, 7, 1, 7],
        '6' => [7, 4, 7, 5, 7],
        '7' => [7, 1, 1, 1, 1],
        '8' => [7, 5, 7, 5, 7],
        '9' => [7, 5, 7, 1, 7],
        _ => [0, 2, 7, 2, 0],
    }
}

/// Círculo rojo con el número en blanco (`99+` desde 100), en RGBA.
#[cfg(any(windows, test))]
fn overlay(count: u32) -> Vec<u8> {
    let size = SIZE as usize;
    let mut rgba = vec![0u8; size * size * 4];
    // Círculo con borde suavizado: la cobertura de cada píxel según su
    // distancia al centro.
    let radius = SIZE as f32 / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - radius;
            let dy = y as f32 + 0.5 - radius;
            let coverage = (radius - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            let i = (y * size + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[229, 72, 77, (coverage * 255.0) as u8]);
        }
    }
    let label = if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    };
    let chars: Vec<char> = label.chars().collect();
    let scale = match chars.len() {
        1 => 4,
        2 => 3,
        _ => 2,
    };
    let width = chars.len() * 3 * scale + (chars.len() - 1) * scale;
    let height = 5 * scale;
    let left = (size - width) / 2;
    let top = (size - height) / 2;
    for (n, c) in chars.into_iter().enumerate() {
        let x0 = left + n * 4 * scale;
        for (row, bits) in glyph(c).into_iter().enumerate() {
            for col in 0..3 {
                if bits & (4 >> col) == 0 {
                    continue;
                }
                for y in top + row * scale..top + (row + 1) * scale {
                    for x in x0 + col * scale..x0 + (col + 1) * scale {
                        let i = (y * size + x) * 4;
                        rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                    }
                }
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Píxeles blancos del número.
    fn white(rgba: &[u8]) -> usize {
        rgba.chunks(4)
            .filter(|p| p == &[255, 255, 255, 255])
            .count()
    }

    #[test]
    fn overlay_fits() {
        for count in [1, 7, 10, 42, 99, 100, 12345] {
            let rgba = overlay(count);
            assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
            assert!(white(&rgba) > 0, "{count}");
        }
        // Las esquinas quedan transparentes (fuera del círculo).
        assert_eq!(overlay(1)[3], 0);
        // Desde 100 todo se ve igual.
        assert_eq!(overlay(100), overlay(12345));
    }
}
