//! Windows: icono en la bandeja del sistema. Cerrar la ventana la oculta y
//! Pulpo sigue funcionando (las apps reciben mensajes y avisan); un clic en el
//! icono la vuelve a mostrar y su menú permite salir del todo.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

const TRAY_ID: &str = "pulpo";

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let menu = menu(app, "Pulpo", "Quit")?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Pulpo")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show(app),
            "quit" => crate::quit::quit_async(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn menu(app: &AppHandle, show: &str, quit: &str) -> tauri::Result<Menu<tauri::DynRuntime>> {
    Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "show", show, true, None::<&str>)?,
            &MenuItem::with_id(app, "quit", quit, true, None::<&str>)?,
        ],
    )
}

/// Vuelve a mostrar la ventana, delante.
pub fn show(app: &AppHandle) {
    if let Some(window) = app.get_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Textos del menú en el idioma de la interfaz (la barra lateral lo sabe).
pub fn set_tray_texts(app: AppHandle, show: String, quit: String) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match menu(&app, &show, &quit) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(e) => eprintln!("[pulpo] no se pudo rehacer el menú de la bandeja: {e}"),
    }
}
