//! The menu-bar presence: a template tray icon, a small menu, and the mini
//! panel window.
//!
//! docs/05-UI.md, "Menu bar": the question this app answers is a *standing* one
//! — a disk at 97% is a condition, not an event — so watching it should not cost
//! a window. Four rules the code below enforces:
//!
//! 1. **The panel is a viewer, never an actor.** It is a second webview onto the
//!    same commands, and the destructive ones are not reachable from it: the
//!    arming switch and the Trash affordances live in the main window's details
//!    panel only. A menu-bar surface appears under the cursor at the top of the
//!    screen, which is exactly where a mis-click is cheapest to make and most
//!    expensive to mean.
//! 2. **Closing the main window does not quit.** [`hide_instead_of_closing`] is
//!    what makes the tray icon a real menu-bar presence rather than a decoration
//!    that dies with the window. Quit stays explicit.
//! 3. **The panel hides when it loses focus**, like every other menu-bar popover
//!    on the platform. It is created once, on first use, and then shown and
//!    hidden — rebuilding a webview per click would cost a white flash and a
//!    re-fetch of everything it displays.
//! 4. **The icon is a template image.** Black with alpha, no colour: macOS
//!    inverts it for the menu bar appearance. A coloured icon looks correct in
//!    exactly one of the two.
//!
//! The panel does **not** poll from Rust. It is a webview running the same
//! frontend bundle under `?window=tray`, so it refetches `volumes` and
//! `scan_status` on its own slow timer through the ordinary command layer. This
//! module owns the window, not its contents.

use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

/// The label of the mini panel window. The frontend also keys off the
/// `?window=tray` query, which is what actually selects the panel UI; this is
/// how Rust finds the window again.
pub(crate) const PANEL_LABEL: &str = "tray-panel";

/// The label of the main window, as declared in `tauri.conf.json`.
pub(crate) const MAIN_LABEL: &str = "main";

/// Logical size of the panel. Fixed: it is a status readout with a known
/// number of rows, and a resizable popover is a popover people accidentally
/// resize.
const PANEL_WIDTH: f64 = 400.0;
const PANEL_HEIGHT: f64 = 520.0;

/// Gap between the menu bar and the top of the panel, in logical pixels.
const PANEL_GAP: f64 = 6.0;

/// Builds the tray icon, its menu, and its click behaviour.
///
/// # Errors
///
/// Whatever Tauri reports when the icon image cannot be decoded or the tray
/// cannot be registered with the platform.
pub(crate) fn build(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "tray:open", "Open RDirStat", true, None::<&str>)?;
    let panel = MenuItem::with_id(app, "tray:panel", "Show Status Panel", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "tray:quit", "Quit RDirStat", true, Some("Cmd+Q"))?;
    let menu = Menu::with_items(app, &[&open, &panel, &PredefinedMenuItem::separator(app)?, &quit])?;

    TrayIconBuilder::with_id("rdirstat")
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        // Template mode is the whole reason the icon is black-with-alpha.
        .icon_as_template(true)
        .tooltip("RDirStat — disk usage")
        .menu(&menu)
        // Left click belongs to the panel; the menu is the right-click gesture.
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                // `rect` is the icon's own screen rectangle in physical pixels,
                // which is what lets the panel appear under the icon the user
                // actually clicked rather than under the first display's.
                let app = tray.app_handle().clone();
                if let Err(error) = toggle_panel(&app, Some(rect)) {
                    tracing::error!(%error, "could not open the tray panel");
                }
            }
        })
        .build(app)?;
    Ok(())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the signature is Tauri's: TrayIconBuilder::on_menu_event takes an \
              Fn(&AppHandle, MenuEvent), so taking &MenuEvent here would not compile."
)]
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "tray:open" => {
            if let Err(error) = show_main_window(app) {
                tracing::error!(%error, "could not show the main window");
            }
        }
        "tray:panel" => {
            if let Err(error) = toggle_panel(app, None) {
                tracing::error!(%error, "could not open the tray panel");
            }
        }
        "tray:quit" => app.exit(0),
        _ => {}
    }
}

/// Shows and focuses the main window, creating nothing: the window exists from
/// `tauri.conf.json` and is only ever hidden.
///
/// # Errors
///
/// Whatever Tauri reports when the window cannot be shown or focused.
pub(crate) fn show_main_window(app: &AppHandle) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window(MAIN_LABEL) else {
        return Ok(());
    };
    window.show()?;
    window.unminimize()?;
    window.set_focus()
}

/// Shows the panel under the tray icon, or hides it if it is already up.
///
/// # Errors
///
/// Whatever Tauri reports when the panel window cannot be created, positioned,
/// or shown.
fn toggle_panel(app: &AppHandle, anchor: Option<tauri::Rect>) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(PANEL_LABEL) {
        if window.is_visible().unwrap_or(false) {
            window.hide()?;
            return Ok(());
        }
        position_panel(&window, anchor)?;
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(app, PANEL_LABEL, WebviewUrl::App("index.html?window=tray".into()))
        .title("RDirStat Status")
        .inner_size(PANEL_WIDTH, PANEL_HEIGHT)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;

    // A popover closes when you click away from it. Without this the panel
    // stays on top of everything until it is clicked again, which is a window,
    // not a popover.
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event
            && let Some(panel) = handle.get_webview_window(PANEL_LABEL)
            && let Err(error) = panel.hide()
        {
            tracing::debug!(%error, "could not hide the tray panel on focus loss");
        }
    });

    position_panel(&window, anchor)?;
    window.show()?;
    window.set_focus()
}

/// Places the panel under the tray icon, clamped to the icon's own monitor.
///
/// Falls back to the top-right of the primary monitor when the click carried no
/// rectangle — that is the menu-item path, which has no icon geometry.
/// A screen coordinate in physical pixels.
///
/// The workspace denies `cast_possible_truncation` so that every narrowing is
/// deliberate. `f64 as i32` saturates rather than wrapping, but it also
/// truncates toward zero; a pixel position wants rounding to nearest, and a
/// NaN — which `as` would silently turn into 0 — is a bug worth pinning to a
/// defined value on purpose rather than by accident.
fn physical_px(value: f64) -> i32 {
    if value.is_nan() {
        return 0;
    }
    let rounded = value.round();
    if rounded <= f64::from(i32::MIN) {
        i32::MIN
    } else if rounded >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the two branches above bound `rounded` inside i32's range, so this cannot truncate"
        )]
        {
            rounded as i32
        }
    }
}

fn position_panel(window: &tauri::WebviewWindow, anchor: Option<tauri::Rect>) -> tauri::Result<()> {
    use tauri::{LogicalPosition, PhysicalPosition, PhysicalSize, Position};

    let scale = window.scale_factor().unwrap_or(1.0);
    let panel_width = PANEL_WIDTH * scale;

    let Some(rect) = anchor else {
        // No anchor: the top-right corner of the primary monitor, inset by the
        // same gap the anchored case uses.
        let Some(monitor) = window.primary_monitor()? else {
            return Ok(());
        };
        let size = monitor.size();
        let position = monitor.position();
        let x = f64::from(position.x) + f64::from(size.width) - panel_width - PANEL_GAP * scale;
        let y = f64::from(position.y) + 24.0 * scale;
        return window.set_position(Position::Physical(PhysicalPosition::new(
            physical_px(x),
            physical_px(y),
        )));
    };

    // `Rect` carries either logical or physical units depending on the
    // platform's event; normalize both to physical, which is what
    // `set_position` takes.
    let icon_position: PhysicalPosition<f64> = match rect.position {
        Position::Physical(value) => PhysicalPosition::new(f64::from(value.x), f64::from(value.y)),
        Position::Logical(value) => LogicalPosition::new(value.x, value.y).to_physical(scale),
    };
    let icon_size: PhysicalSize<f64> = match rect.size {
        tauri::Size::Physical(value) => PhysicalSize::new(f64::from(value.width), f64::from(value.height)),
        tauri::Size::Logical(value) => tauri::LogicalSize::new(value.width, value.height).to_physical(scale),
    };

    // Centred under the icon, then clamped so a tray icon near the right edge
    // does not push the panel off screen.
    let mut x = icon_position.x + icon_size.width / 2.0 - panel_width / 2.0;
    let y = icon_position.y + icon_size.height + PANEL_GAP * scale;

    if let Some(monitor) = window.monitor_from_point(icon_position.x, icon_position.y)? {
        let monitor_left = f64::from(monitor.position().x);
        let monitor_right = monitor_left + f64::from(monitor.size().width);
        x = x.clamp(
            monitor_left + PANEL_GAP * scale,
            monitor_right - panel_width - PANEL_GAP * scale,
        );
    }

    window.set_position(Position::Physical(PhysicalPosition::new(
        physical_px(x),
        physical_px(y),
    )))
}

/// Turns the main window's close button into a hide.
///
/// Wired from `run()` for the main window only. The panel has no close button,
/// and a hidden panel is hidden by focus loss rather than by this.
pub(crate) fn hide_instead_of_closing(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != MAIN_LABEL {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        if let Err(error) = window.hide() {
            tracing::error!(%error, "could not hide the main window");
        }
    }
}

#[cfg(test)]
mod tests {
    /// The tray icon must decode, and it must not be blank.
    ///
    /// A menu-bar item whose image fails to load still *exists* — macOS shows an
    /// empty slot and nothing says why — so this asserts what the eye would
    /// otherwise have to catch: the PNG decodes, it is square, and it has ink.
    #[test]
    fn the_tray_icon_decodes_and_has_ink() {
        let bytes = include_bytes!("../icons/tray.png");
        let image = tauri::image::Image::from_bytes(bytes).expect("the tray icon must decode");
        assert_eq!(image.width(), image.height(), "a menu-bar icon is square");
        assert!(image.width() >= 22, "at least one menu-bar point of resolution");
        let opaque = image.rgba().chunks_exact(4).filter(|pixel| pixel[3] > 0).count();
        assert!(
            opaque > 0,
            "a template icon with no opaque pixels renders as an empty slot"
        );
    }
}
