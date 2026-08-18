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

/// Gap between the menu bar and the top of the panel, in logical points.
const PANEL_GAP: f64 = 6.0;

/// Height of the macOS menu bar in logical points, for the unanchored case.
const MENU_BAR_POINTS: f64 = 26.0;

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

/// Places the panel under the tray icon, clamped to that icon's own monitor.
///
/// Falls back to the top-right of the monitor that owns the menu bar when the
/// click carried no rectangle — that is the menu-item path, which has no icon
/// geometry.
///
/// # Everything here is in logical points, deliberately
///
/// The first version of this function did the arithmetic in physical pixels,
/// and the panel walked off the screen. On a Mac with a 2× built-in display and
/// a 1× ultra-wide beside it, "physical pixels" is not one coordinate space:
/// `set_position` interprets a physical position using the scale factor of the
/// monitor the window is currently on, so a position computed from the primary
/// monitor's physical width (2788) landed at 2788 *points* once the window had
/// drifted onto the 1× display — and each subsequent open pushed it further,
/// 1394 → 2788 → 4250. Observed, not theorised.
///
/// Points are the one space every display agrees on, so the anchor rect, the
/// monitor bounds, and the panel size are all converted to points up front and
/// the result is set as [`Position::Logical`].
fn position_panel(window: &tauri::WebviewWindow, anchor: Option<tauri::Rect>) -> tauri::Result<()> {
    use tauri::{LogicalPosition, Position};

    // The monitor the panel should appear on: the one under the tray icon, or
    // the one that owns the menu bar when we have no icon geometry.
    let anchor_physical = anchor.as_ref().map(|rect| match rect.position {
        Position::Physical(value) => (f64::from(value.x), f64::from(value.y)),
        // A logical anchor needs *a* scale to reach physical for the monitor
        // lookup; the window's own is the best available guess, and the lookup
        // only has to land on the right screen.
        Position::Logical(value) => {
            let scale = window.scale_factor().unwrap_or(1.0);
            (value.x * scale, value.y * scale)
        }
    });

    let monitor = match anchor_physical {
        Some((x, y)) => window.monitor_from_point(x, y)?,
        None => None,
    };
    let Some(monitor) = monitor.or(window.primary_monitor()?) else {
        return Ok(());
    };

    let scale = monitor.scale_factor();
    let left = f64::from(monitor.position().x) / scale;
    let top = f64::from(monitor.position().y) / scale;
    let width = f64::from(monitor.size().width) / scale;

    let Some(rect) = anchor else {
        // No anchor: top-right, inset by the same gap the anchored case uses,
        // and below the menu bar rather than under it.
        return window.set_position(Position::Logical(LogicalPosition::new(
            left + width - PANEL_WIDTH - PANEL_GAP,
            top + MENU_BAR_POINTS,
        )));
    };

    let (icon_left, icon_top) = match rect.position {
        Position::Physical(value) => (f64::from(value.x) / scale, f64::from(value.y) / scale),
        Position::Logical(value) => (value.x, value.y),
    };
    let (icon_width, icon_height) = match rect.size {
        tauri::Size::Physical(value) => (f64::from(value.width) / scale, f64::from(value.height) / scale),
        tauri::Size::Logical(value) => (value.width, value.height),
    };

    // Centred under the icon, then clamped so an icon near either edge does not
    // push the panel off the screen it belongs to.
    let centred = icon_left + icon_width / 2.0 - PANEL_WIDTH / 2.0;
    let x = clamp_to_monitor(centred, left, width);
    let y = icon_top + icon_height + PANEL_GAP;

    window.set_position(Position::Logical(LogicalPosition::new(x, y)))
}

/// Keeps the panel's left edge inside `[left, left + width]`, gap included.
///
/// Separated so it can be tested without a window: the clamp is the part that
/// has an off-by-one-monitor bug in it if the arithmetic is wrong, and a
/// monitor narrower than the panel must still produce a defined answer rather
/// than an inverted range.
fn clamp_to_monitor(x: f64, monitor_left: f64, monitor_width: f64) -> f64 {
    let min = monitor_left + PANEL_GAP;
    let max = monitor_left + monitor_width - PANEL_WIDTH - PANEL_GAP;
    if max <= min { min } else { x.clamp(min, max) }
}

/// Turns the main window's close button into a hide.
///
/// Wired from `run()` for the main window only. This is what makes the tray a
/// real menu-bar presence rather than a decoration that dies with the window:
/// closing the window leaves the app running and reachable from the icon, and
/// Quit stays explicit. The panel has no close button, and a visible panel is
/// dismissed by focus loss rather than by this.
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
    /// The panel stays on the monitor it was opened from.
    ///
    /// The bug this pins: positions computed in physical pixels are read back
    /// with whichever monitor's scale factor the window currently sits on, so
    /// on a mixed-DPI setup the panel walked right across the displays, one
    /// screen per open. In points there is one coordinate space and the clamp
    /// is total.
    // Exact float equality is correct here and the lint is wrong about it.
    // `clamp_to_monitor` either returns one of its inputs untouched or a value
    // built from the two `PANEL_*` constants — no accumulated arithmetic, so
    // there is no epsilon to allow for. An approximate assertion would be
    // strictly weaker: it would pass on a clamp that was slightly wrong, which
    // is the only failure mode worth catching.
    #[allow(clippy::float_cmp)]
    #[test]
    fn the_panel_is_clamped_to_the_monitor_it_opens_on() {
        use super::{PANEL_GAP, PANEL_WIDTH, clamp_to_monitor};

        // A 1512-point built-in at the origin: an icon at the far right edge
        // pulls the panel back inside instead of off the screen.
        assert_eq!(clamp_to_monitor(1400.0, 0.0, 1512.0), 1512.0 - PANEL_WIDTH - PANEL_GAP);
        assert_eq!(clamp_to_monitor(-50.0, 0.0, 1512.0), PANEL_GAP);
        assert_eq!(
            clamp_to_monitor(600.0, 0.0, 1512.0),
            600.0,
            "room to spare is left alone"
        );

        // A monitor to the right of the built-in keeps its own bounds — this is
        // the case that used to send the panel to x = 4250.
        let right = 1512.0;
        assert!(clamp_to_monitor(9_999.0, right, 3440.0) < right + 3440.0);
        assert!(clamp_to_monitor(-9_999.0, right, 3440.0) >= right);

        // Narrower than the panel: defined, not an inverted clamp range.
        assert_eq!(clamp_to_monitor(10.0, 0.0, 100.0), PANEL_GAP);
    }

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
