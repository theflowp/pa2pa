use clap::Parser;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde::Deserialize;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::WebViewBuilder;

const COMPACT_W: f64 = 300.0;
const COMPACT_H: f64 = 48.0;
const EXPANDED_W: f64 = 396.0;
const EXPANDED_H: f64 = 392.0;
const EDGE_MARGIN: f64 = 18.0;
const CLAMP_MARGIN: f64 = 8.0;

/// Usable screen work area in logical top-left coordinates (excludes menu bar & Dock/taskbar).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WorkArea {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl WorkArea {
    fn bottom_right_position(&self, win_w: f64, win_h: f64) -> (f64, f64) {
        let x = (self.x + self.width - win_w - EDGE_MARGIN).max(self.x + CLAMP_MARGIN);
        let y = (self.y + self.height - win_h - EDGE_MARGIN).max(self.y + CLAMP_MARGIN);
        (x, y)
    }

    fn clamp_bottom_right_resize(
        &self,
        cur_x: f64,
        cur_y: f64,
        old_w: f64,
        old_h: f64,
        new_w: f64,
        new_h: f64,
    ) -> (f64, f64) {
        let right_x = cur_x + old_w;
        let bottom_y = cur_y + old_h;

        let min_x = self.x + CLAMP_MARGIN;
        let max_x = (self.x + self.width - new_w - CLAMP_MARGIN).max(min_x);
        let min_y = self.y + CLAMP_MARGIN;
        let max_y = (self.y + self.height - new_h - CLAMP_MARGIN).max(min_y);

        let new_x = (right_x - new_w).clamp(min_x, max_x);
        let new_y = (bottom_y - new_h).clamp(min_y, max_y);
        (new_x, new_y)
    }
}

/// Convert AppKit's bottom-left `visibleFrame` into top-left logical coordinates (matching `tao`).
#[cfg(any(target_os = "macos", test))]
fn appkit_visible_frame_to_work_area(
    primary_screen_height: f64,
    vis_x: f64,
    vis_y: f64,
    vis_w: f64,
    vis_h: f64,
) -> WorkArea {
    WorkArea {
        x: vis_x,
        y: primary_screen_height - (vis_y + vis_h),
        width: vis_w,
        height: vis_h,
    }
}

#[cfg(target_os = "macos")]
fn macos_work_area() -> Option<WorkArea> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    if screens.is_empty() {
        return None;
    }
    // In AppKit, screens[0] is always the primary display (defines the global Y=0 bottom origin).
    let primary = screens.objectAtIndex(0);
    let primary_height = primary.frame().size.height;
    let target = NSScreen::mainScreen(mtm).unwrap_or(primary);
    let vis = target.visibleFrame();
    Some(appkit_visible_frame_to_work_area(
        primary_height,
        vis.origin.x,
        vis.origin.y,
        vis.size.width,
        vis.size.height,
    ))
}

fn current_work_area(window: &Window) -> Option<WorkArea> {
    #[cfg(target_os = "macos")]
    {
        if let Some(area) = macos_work_area() {
            return Some(area);
        }
    }

    let monitor = window.current_monitor().or_else(|| window.primary_monitor())?;
    let scale = monitor.scale_factor();
    let pos = monitor.position();
    let size = monitor.size();
    let mx = pos.x as f64 / scale;
    let my = pos.y as f64 / scale;
    let mw = size.width as f64 / scale;
    let mh = size.height as f64 / scale;

    // Conservative safe area fallback for Windows taskbar / Linux desktop panels
    let top_inset = 28.0;
    let bottom_inset = 72.0;
    Some(WorkArea {
        x: mx,
        y: my + top_inset,
        width: mw,
        height: (mh - top_inset - bottom_inset).max(200.0),
    })
}

#[cfg(target_os = "macos")]
fn macos_primary_screen_height() -> Option<f64> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    if screens.is_empty() {
        return None;
    }
    Some(screens.objectAtIndex(0).frame().size.height)
}

/// Read current window top-left logical position & size (uses exact AppKit `NSRect` on macOS
/// to avoid physical-pixel rounding jitter during atomic bottom-right resizing).
fn get_window_rect_logical(window: &Window, fallback_w: f64, fallback_h: f64) -> (f64, f64, f64, f64) {
    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_foundation::NSRect;
        use tao::platform::macos::WindowExtMacOS;

        if let Some(primary_h) = macos_primary_screen_height() {
            let ns_win = window.ns_window() as *mut AnyObject;
            if !ns_win.is_null() {
                let rect: NSRect = unsafe { msg_send![ns_win, frame] };
                let top_y = primary_h - (rect.origin.y + rect.size.height);
                return (rect.origin.x, top_y, rect.size.width, rect.size.height);
            }
        }
    }

    let scale = window.scale_factor();
    let (x, y) = window
        .outer_position()
        .map(|p| (p.x as f64 / scale, p.y as f64 / scale))
        .unwrap_or((100.0, 100.0));
    (x, y, fallback_w, fallback_h)
}

/// Atomically set both size and top-left logical position of the window in a single AppKit
/// transaction on macOS so the bottom-right pill never jumps or flickers during expand/collapse.
fn set_window_rect_atomic(window: &Window, x: f64, y: f64, w: f64, h: f64) {
    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::runtime::{AnyObject, Bool};
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        use tao::platform::macos::WindowExtMacOS;

        if let Some(primary_h) = macos_primary_screen_height() {
            let ns_win = window.ns_window() as *mut AnyObject;
            if !ns_win.is_null() {
                let appkit_y = primary_h - (y + h);
                let rect = NSRect::new(NSPoint::new(x, appkit_y), NSSize::new(w, h));
                unsafe {
                    let _: () = msg_send![ns_win, setFrame: rect, display: Bool::YES];
                }
                return;
            }
        }
    }

    window.set_inner_size(LogicalSize::new(w, h));
    window.set_outer_position(LogicalPosition::new(x, y));
}

#[cfg(target_os = "macos")]
fn enforce_macos_true_transparency(window: &Window, webview: &wry::WebView) {
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{msg_send, sel};
    use objc2_app_kit::NSColor;
    use objc2_foundation::{ns_string, NSNumber};
    use tao::platform::macos::WindowExtMacOS;
    use wry::WebViewExtMacOS;

    unsafe {
        let clear = NSColor::clearColor();
        let clear_ptr: *const AnyObject = (&*clear as *const NSColor).cast();

        let ns_win = window.ns_window() as *mut AnyObject;
        if !ns_win.is_null() {
            let _: () = msg_send![ns_win, setOpaque: Bool::NO];
            let _: () = msg_send![ns_win, setHasShadow: Bool::NO];
            let _: () = msg_send![ns_win, setBackgroundColor: clear_ptr];
        }

        let wk = webview.webview();
        let wk_ptr: *mut AnyObject = &*wk as *const _ as *mut AnyObject;
        if !wk_ptr.is_null() {
            let no = NSNumber::new_bool(false);
            let no_ptr: *const AnyObject = (&*no as *const NSNumber).cast();
            let key = ns_string!("drawsBackground");
            let _: () = msg_send![wk_ptr, setValue: no_ptr, forKey: key];

            let has_under_page: Bool =
                msg_send![wk_ptr, respondsToSelector: sel!(setUnderPageBackgroundColor:)];
            if has_under_page.as_bool() {
                let _: () = msg_send![wk_ptr, setUnderPageBackgroundColor: clear_ptr];
            }
        }
    }
}

/// Give the WKWebView a fixed EXPANDED_W x EXPANDED_H frame pinned to the bottom-right corner of
/// the window's content view. Expanding/collapsing the NSWindow then only changes how much of the
/// (already rendered) page is revealed: WebKit never re-lays out or repaints asynchronously during a
/// resize, which eliminates the one-frame blink of the card and pill on open/close.
#[cfg(target_os = "macos")]
fn pin_macos_webview_bottom_right(webview: &wry::WebView) {
    use objc2::msg_send;
    use objc2::runtime::{AnyObject, Bool};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use wry::WebViewExtMacOS;

    // NSViewMinXMargin (1) | NSViewMaxYMargin (32) in a non-flipped parent, or
    // NSViewMinXMargin (1) | NSViewMinYMargin (8) in a flipped one: flexible left & top margins.
    unsafe {
        let wk = webview.webview();
        let wk_ptr: *mut AnyObject = &*wk as *const _ as *mut AnyObject;
        if wk_ptr.is_null() {
            return;
        }
        let parent: *mut AnyObject = msg_send![wk_ptr, superview];
        if parent.is_null() {
            return;
        }
        let bounds: NSRect = msg_send![parent, bounds];
        let flipped: Bool = msg_send![parent, isFlipped];
        let x = bounds.size.width - EXPANDED_W;
        let (y, mask): (f64, usize) = if flipped.as_bool() {
            (bounds.size.height - EXPANDED_H, 1 | 8)
        } else {
            (0.0, 1 | 32)
        };
        let _: () = msg_send![wk_ptr, setAutoresizingMask: mask];
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(EXPANDED_W, EXPANDED_H));
        let _: () = msg_send![wk_ptr, setFrame: frame];
    }
}

fn position_window_bottom_right(window: &Window, win_w: f64, win_h: f64) {
    if let Some(area) = current_work_area(window) {
        let (x, y) = area.bottom_right_position(win_w, win_h);
        window.set_outer_position(LogicalPosition::new(x, y));
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "pa2pa-pill",
    about = "Cross-platform floating desktop pill + menu bar / system tray controller for pa2pa"
)]
struct Args {
    /// Base URL of the local pa2pa daemon (e.g. http://127.0.0.1:4789)
    #[arg(long, default_value = "http://127.0.0.1:4789", env = "PA2PA_DAEMON_URL")]
    url: String,
    /// Start hidden in the system tray / menu bar until an approval arrives or hotkey is pressed
    #[arg(long)]
    start_hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayVisual {
    Offline,
    Connected,
    PendingApproval,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PillIpc {
    Resize {
        expanded: bool,
        #[serde(default)]
        height: Option<f64>,
    },
    Drag,
    OpenDashboard,
    Quit,
    Status {
        relay_connected: bool,
        pending_approvals: usize,
        unread_messages: usize,
        online_peers: usize,
    },
}

#[derive(Debug)]
enum AppEvent {
    Ipc(PillIpc),
    Menu(MenuEvent),
    HotKey(GlobalHotKeyEvent),
}

/// Programmatically render a crisp 32x32 RGBA tray/menu-bar icon with live status dot.
fn build_tray_icon(state: TrayVisual) -> anyhow::Result<Icon> {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];

    let (dot_r, dot_g, dot_b) = match state {
        TrayVisual::Connected => (52u8, 211u8, 153u8),       // emerald
        TrayVisual::PendingApproval => (251u8, 191u8, 36u8), // amber
        TrayVisual::Offline => (248u8, 113u8, 113u8),        // coral red
    };

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = (y * SIZE + x) * 4;
            let fx = x as f32 - 15.5;
            let fy = y as f32 - 15.5;
            let dist = (fx * fx + fy * fy).sqrt();

            // Outer dark indigo/slate badge circle
            if dist <= 13.5 {
                let is_ring = dist >= 11.2;
                let (r, g, b) = if is_ring {
                    (108u8, 140u8, 255u8)
                } else {
                    (18u8, 22u8, 34u8)
                };
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }

            // Two stylized agent nodes inside the badge connected by a bridge
            let left_node = ((x as f32 - 11.0).powi(2) + (y as f32 - 16.0).powi(2)).sqrt() <= 3.1;
            let right_node = ((x as f32 - 21.0).powi(2) + (y as f32 - 16.0).powi(2)).sqrt() <= 3.1;
            let bridge = (12..=20).contains(&x) && (15..=17).contains(&y);
            if left_node || right_node || bridge {
                rgba[idx] = 236;
                rgba[idx + 1] = 242;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }

            // Live status indicator dot in bottom-right corner
            let dx = x as f32 - 24.5;
            let dy = y as f32 - 24.5;
            let d_dot = (dx * dx + dy * dy).sqrt();
            if d_dot <= 5.8 {
                if d_dot > 4.4 {
                    // dark halo for contrast
                    rgba[idx] = 11;
                    rgba[idx + 1] = 13;
                    rgba[idx + 2] = 18;
                    rgba[idx + 3] = 255;
                } else {
                    rgba[idx] = dot_r;
                    rgba[idx + 1] = dot_g;
                    rgba[idx + 2] = dot_b;
                    rgba[idx + 3] = 255;
                }
            }
        }
    }

    Ok(Icon::from_rgba(rgba, SIZE as u32, SIZE as u32)?)
}

fn validate_loopback_daemon_url(url: &str) -> anyhow::Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("pa2pa-pill only connects to loopback http:// URLs"))?;
    let host_port = rest.split('/').next().unwrap_or("");
    let ok = host_port == "127.0.0.1"
        || host_port == "localhost"
        || host_port == "[::1]"
        || host_port
            .rsplit_once(':')
            .map(|(h, p)| {
                matches!(h, "127.0.0.1" | "localhost" | "[::1]")
                    && !p.is_empty()
                    && p.chars().all(|c| c.is_ascii_digit())
            })
            .unwrap_or(false);
    if !ok {
        anyhow::bail!(
            "Refusing to load non-loopback URL `{url}` into pa2pa-pill WebView (security policy)"
        );
    }
    Ok(trimmed.to_string())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let base_url = validate_loopback_daemon_url(&args.url)?;
    let pill_url = format!("{base_url}/pill");

    #[cfg(target_os = "macos")]
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    #[cfg(not(target_os = "macos"))]
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    // Run as a macOS menu bar accessory (no Dock icon, stays cleanly across Spaces).
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let ipc_proxy = event_loop.create_proxy();
    let menu_proxy = event_loop.create_proxy();
    let hotkey_proxy = event_loop.create_proxy();

    // Forward tray menu & global hotkey events directly into tao's event loop
    MenuEvent::set_event_handler(Some(move |ev| {
        let _ = menu_proxy.send_event(AppEvent::Menu(ev));
    }));
    GlobalHotKeyEvent::set_event_handler(Some(move |ev| {
        let _ = hotkey_proxy.send_event(AppEvent::HotKey(ev));
    }));

    // Register global hotkey: Cmd+Shift+P (macOS) or Ctrl+Shift+P (Windows/Linux)
    let hotkey_manager = GlobalHotKeyManager::new().ok();
    let toggle_hotkey = {
        #[cfg(target_os = "macos")]
        let mods = Modifiers::SUPER | Modifiers::SHIFT;
        #[cfg(not(target_os = "macos"))]
        let mods = Modifiers::CONTROL | Modifiers::SHIFT;
        HotKey::new(Some(mods), Code::KeyP)
    };
    let toggle_hotkey_id = toggle_hotkey.id();
    if let Some(ref mgr) = hotkey_manager {
        if let Err(e) = mgr.register(toggle_hotkey) {
            eprintln!("Warning: could not register global hotkey: {e}");
        }
    }

    // Build native system tray / top menu bar icon & menu (macOS, Windows, Linux)
    let status_item = MenuItem::new("pa2pa · Connecting…", false, None);
    #[cfg(target_os = "macos")]
    let toggle_label = "Show / Hide Pill (⌘⇧P)";
    #[cfg(not(target_os = "macos"))]
    let toggle_label = "Show / Hide Pill (Ctrl+Shift+P)";
    let toggle_item = MenuItem::new(toggle_label, true, None);
    let reset_pos_item = MenuItem::new("Reset Pill Position (Bottom-Right)", true, None);
    let dashboard_item = MenuItem::new("Open Web Dashboard", true, None);
    let quit_item = MenuItem::new("Quit pa2pa Pill", true, None);

    let toggle_id = toggle_item.id().clone();
    let reset_pos_id = reset_pos_item.id().clone();
    let dashboard_id = dashboard_item.id().clone();
    let quit_id = quit_item.id().clone();

    let tray_menu = Menu::new();
    let _ = tray_menu.append(&status_item);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&toggle_item);
    let _ = tray_menu.append(&reset_pos_item);
    let _ = tray_menu.append(&dashboard_item);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&quit_item);

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("pa2pa — Personal Agent to Personal Agent")
        .with_icon(build_tray_icon(TrayVisual::Offline)?)
        .build()
        .ok();

    let mut builder = WindowBuilder::new()
        .with_title("pa2pa")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_visible(!args.start_hidden)
        .with_inner_size(LogicalSize::new(COMPACT_W, COMPACT_H));

    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder = builder.with_has_shadow(false);
    }

    let window = builder.build(&event_loop)?;
    window.set_visible_on_all_workspaces(true);

    // Position cleanly at bottom-right of the usable work area (above/beside the Dock/taskbar).
    position_window_bottom_right(&window, COMPACT_W, COMPACT_H);

    let webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_url(&pill_url)
        .with_ipc_handler(move |req| {
            if let Ok(cmd) = serde_json::from_str::<PillIpc>(req.body()) {
                let _ = ipc_proxy.send_event(AppEvent::Ipc(cmd));
            }
        })
        .build(&window)?;

    #[cfg(target_os = "macos")]
    enforce_macos_true_transparency(&window, &webview);
    #[cfg(target_os = "macos")]
    pin_macos_webview_bottom_right(&webview);

    let mut is_expanded = false;
    let mut cur_w = COMPACT_W;
    let mut cur_h = COMPACT_H;
    let mut is_visible = !args.start_hidden;
    let mut current_visual = TrayVisual::Offline;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep tray_icon & hotkey_manager alive for the lifetime of the event loop
        let _keep_hotkey = &hotkey_manager;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Hide to tray instead of quitting abruptly
                is_visible = false;
                window.set_visible(false);
            }
            Event::UserEvent(AppEvent::HotKey(hk)) => {
                if hk.id == toggle_hotkey_id && hk.state == HotKeyState::Pressed {
                    is_visible = !is_visible;
                    window.set_visible(is_visible);
                }
            }
            Event::UserEvent(AppEvent::Menu(ev)) => {
                if ev.id == toggle_id {
                    is_visible = !is_visible;
                    window.set_visible(is_visible);
                } else if ev.id == reset_pos_id {
                    position_window_bottom_right(&window, cur_w, cur_h);
                    if !is_visible {
                        is_visible = true;
                        window.set_visible(true);
                    }
                } else if ev.id == dashboard_id {
                    let _ = open::that(&base_url);
                } else if ev.id == quit_id {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(AppEvent::Ipc(cmd)) => match cmd {
                PillIpc::Drag => {
                    let _ = window.drag_window();
                }
                PillIpc::OpenDashboard => {
                    let _ = open::that(&base_url);
                }
                PillIpc::Quit => {
                    *control_flow = ControlFlow::Exit;
                }
                PillIpc::Status {
                    relay_connected,
                    pending_approvals,
                    unread_messages,
                    online_peers,
                } => {
                    let next_visual = if pending_approvals > 0 {
                        TrayVisual::PendingApproval
                    } else if relay_connected {
                        TrayVisual::Connected
                    } else {
                        TrayVisual::Offline
                    };

                    // Auto-surface the pill if hidden and an approval arrives
                    if pending_approvals > 0 && !is_visible {
                        is_visible = true;
                        window.set_visible(true);
                    }

                    let summary = if pending_approvals > 0 {
                        format!(
                            "pa2pa · {} approval{} pending!",
                            pending_approvals,
                            if pending_approvals > 1 { "s" } else { "" }
                        )
                    } else if relay_connected {
                        format!(
                            "pa2pa · Online ({online_peers} peers, {unread_messages} unread)"
                        )
                    } else {
                        "pa2pa · Relay Offline".to_string()
                    };
                    status_item.set_text(&summary);

                    if let Some(ref tray) = tray_icon {
                        let _ = tray.set_tooltip(Some(&summary));
                        if next_visual != current_visual {
                            if let Ok(icon) = build_tray_icon(next_visual) {
                                let _ = tray.set_icon(Some(icon));
                                current_visual = next_visual;
                            }
                        }
                    }
                }
                PillIpc::Resize { expanded, height } => {
                    let (new_w, new_h) = if expanded {
                        let clamped_h = height
                            .unwrap_or(EXPANDED_H)
                            .clamp(COMPACT_H + 56.0, EXPANDED_H);
                        (EXPANDED_W, clamped_h)
                    } else {
                        (COMPACT_W, COMPACT_H)
                    };

                    if expanded == is_expanded
                        && (new_w - cur_w).abs() < 1.0
                        && (new_h - cur_h).abs() < 1.0
                    {
                        return;
                    }

                    let (cur_x, cur_y, actual_w, actual_h) =
                        get_window_rect_logical(&window, cur_w, cur_h);

                    let (new_x, new_y) = if let Some(area) = current_work_area(&window) {
                        area.clamp_bottom_right_resize(
                            cur_x, cur_y, actual_w, actual_h, new_w, new_h,
                        )
                    } else {
                        let right_x = cur_x + actual_w;
                        let bottom_y = cur_y + actual_h;
                        (
                            (right_x - new_w).max(CLAMP_MARGIN),
                            (bottom_y - new_h).max(28.0),
                        )
                    };

                    set_window_rect_atomic(&window, new_x, new_y, new_w, new_h);
                    // Autoresizing already keeps it anchored; re-pin as a cheap safety net.
                    #[cfg(target_os = "macos")]
                    pin_macos_webview_bottom_right(&webview);
                    is_expanded = expanded;
                    cur_w = new_w;
                    cur_h = new_h;
                    let _keep_webview = &webview;
                }
            },
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_bottom_dock_places_pill_bottom_right_above_dock() {
        // 1512x982 MacBook Pro, 38px menu bar at top, 82px Dock at bottom
        let area = appkit_visible_frame_to_work_area(982.0, 0.0, 82.0, 1512.0, 862.0);
        assert_eq!(area.x, 0.0);
        assert_eq!(area.y, 38.0);
        assert_eq!(area.width, 1512.0);
        assert_eq!(area.height, 862.0);

        let (x, y) = area.bottom_right_position(COMPACT_W, COMPACT_H);
        assert_eq!(x, 1512.0 - COMPACT_W - EDGE_MARGIN);
        assert_eq!(y, 38.0 + 862.0 - COMPACT_H - EDGE_MARGIN);
        // Pill bottom edge stays strictly above the Dock top (900.0)
        assert!(y + COMPACT_H < 900.0);
    }

    #[test]
    fn macos_right_dock_places_pill_left_of_dock() {
        // 1512x982 screen, 38px menu bar, 80px Dock on the right
        let area = appkit_visible_frame_to_work_area(982.0, 0.0, 0.0, 1432.0, 944.0);
        let (x, y) = area.bottom_right_position(COMPACT_W, COMPACT_H);
        assert!(x + COMPACT_W < 1432.0);
        assert!(y + COMPACT_H < 982.0);
    }

    #[test]
    fn expand_from_bottom_right_keeps_right_and_bottom_edges_anchored() {
        let area = WorkArea {
            x: 0.0,
            y: 38.0,
            width: 1512.0,
            height: 862.0,
        };
        let (cx, cy) = area.bottom_right_position(COMPACT_W, COMPACT_H);
        let (ex, ey) =
            area.clamp_bottom_right_resize(cx, cy, COMPACT_W, COMPACT_H, EXPANDED_W, EXPANDED_H);
        assert!((ex + EXPANDED_W - (cx + COMPACT_W)).abs() < 0.01);
        assert!((ey + EXPANDED_H - (cy + COMPACT_H)).abs() < 0.01);
        assert!(ey >= area.y + CLAMP_MARGIN);
        assert!(ey + EXPANDED_H <= area.y + area.height - CLAMP_MARGIN);
    }
}
