use std::sync::Arc;

use blocker::Blocker;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, WebResourceRequestedEventHandler};
use windows::core::{Interface, HSTRING, PWSTR};

use crate::services::Theme;

/// Engancha el bloqueo de red al webview y carga `url`. Se llama dentro de
/// `Webview::with_webview`, en el hilo principal, antes de la primera carga.
pub fn attach(
    platform: tauri::webview::PlatformWebview,
    blocker: Option<Arc<Blocker>>,
    url: &str,
    theme: Theme,
) {
    let Some(platform) = platform.downcast_ref::<tauri_runtime_wry::Webview>() else {
        return;
    };
    if !theme.is_auto() {
        set_theme_inner(platform, theme);
    }
    let result = unsafe {
        match blocker {
            Some(blocker) => attach_inner(platform, blocker, url),
            None => platform
                .controller()
                .CoreWebView2()
                .and_then(|w| w.Navigate(&HSTRING::from(url))),
        }
    };
    if let Err(e) = result {
        eprintln!("[pulpo] no se pudo enganchar el bloqueo en WebView2: {e}");
    }
}

/// El tema que la página ve en `prefers-color-scheme` (del perfil, que es
/// solo de esta app). Se fija al crear el webview.
fn set_theme_inner(platform: &tauri_runtime_wry::Webview, theme: Theme) {
    let scheme = match theme {
        Theme::Auto => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_AUTO,
        Theme::Light => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        Theme::Dark => COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK,
    };
    let result = unsafe {
        platform
            .controller()
            .CoreWebView2()
            .and_then(|w| w.cast::<ICoreWebView2_13>())
            .and_then(|w| w.Profile())
            .and_then(|p| p.SetPreferredColorScheme(scheme))
    };
    if let Err(e) = result {
        eprintln!("[pulpo] no se pudo fijar el tema en WebView2: {e}");
    }
}

unsafe fn attach_inner(
    platform: &tauri_runtime_wry::Webview,
    blocker: Arc<Blocker>,
    url: &str,
) -> windows::core::Result<()> {
    let webview = platform.controller().CoreWebView2()?;
    let env = platform.environment();
    let filter = HSTRING::from("*");

    // Con la API nueva también se ven las peticiones de iframes y workers.
    if let Ok(webview22) = webview.cast::<ICoreWebView2_22>() {
        webview22.AddWebResourceRequestedFilterWithRequestSourceKinds(
            &filter,
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
            COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
        )?;
    } else {
        webview.AddWebResourceRequestedFilter(&filter, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
    }

    let mut token = 0i64;
    webview.add_WebResourceRequested(
        &WebResourceRequestedEventHandler::create(Box::new(move |sender, args| {
            let (Some(sender), Some(args)) = (sender, args) else {
                return Ok(());
            };
            let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
            args.ResourceContext(&mut context)?;
            let uri = {
                let mut uri = PWSTR::null();
                args.Request()?.Uri(&mut uri)?;
                take_pwstr(uri)
            };
            let source = {
                let mut source = PWSTR::null();
                sender.Source(&mut source)?;
                take_pwstr(source)
            };
            if blocker.should_block(&uri, &source, request_type(context)) {
                let response = env.CreateWebResourceResponse(
                    None,
                    403,
                    &HSTRING::from("Blocked"),
                    &HSTRING::new(),
                )?;
                args.SetResponse(&response)?;
            }
            Ok(())
        })),
        &mut token,
    )?;

    webview.Navigate(&HSTRING::from(url))
}

fn request_type(context: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> &'static str {
    match context {
        // WebView2 no distingue el documento principal de un iframe. Como
        // iframe se bloquean los de anuncios y las listas no tocan las
        // páginas de los propios servicios ni las de login.
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => "sub_frame",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST => "xmlhttprequest",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => "xmlhttprequest",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
        COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
        _ => "other",
    }
}
