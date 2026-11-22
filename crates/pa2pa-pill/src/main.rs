use clap::Parser;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde::Deserialize;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::WebViewBuilder;

const COMPACT_W: f64 = 300.0;
const COMPACT_H: f64 = 58.0;
const EXPANDED_W: f64 = 412.0;
const EXPANDED_H: f64 = 392.0;

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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let base_url = args.url.trim_end_matches('/').to_string();
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
    let dashboard_item = MenuItem::new("Open Web Dashboard", true, None);
    let quit_item = MenuItem::new("Quit pa2pa Pill", true, None);

    let toggle_id = toggle_item.id().clone();
    let dashboard_id = dashboard_item.id().clone();
    let quit_id = quit_item.id().clone();

    let tray_menu = Menu::new();
    let _ = tray_menu.append(&status_item);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&toggle_item);
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

    // Position near bottom-center of primary monitor
    if let Some(monitor) = window.primary_monitor() {
        let scale = monitor.scale_factor();
        let size = monitor.size();
        let screen_w = size.width as f64 / scale;
        let screen_h = size.height as f64 / scale;
        let x = ((screen_w - COMPACT_W) / 2.0).max(16.0);
        let y = (screen_h - COMPACT_H - 42.0).max(24.0);
        window.set_outer_position(LogicalPosition::new(x, y));
    }

    let _webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_url(&pill_url)
        .with_ipc_handler(move |req| {
            if let Ok(cmd) = serde_json::from_str::<PillIpc>(req.body()) {
                let _ = ipc_proxy.send_event(AppEvent::Ipc(cmd));
            }
        })
        .build(&window)?;

    let mut is_expanded = false;
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
                PillIpc::Resize { expanded } => {
                    if expanded == is_expanded {
                        return;
                    }
                    let scale = window.scale_factor();
                    let cur_pos = window
                        .outer_position()
                        .map(|p| (p.x as f64 / scale, p.y as f64 / scale))
                        .unwrap_or((100.0, 100.0));
                    let (old_w, old_h) = if is_expanded {
                        (EXPANDED_W, EXPANDED_H)
                    } else {
                        (COMPACT_W, COMPACT_H)
                    };
                    let (new_w, new_h) = if expanded {
                        (EXPANDED_W, EXPANDED_H)
                    } else {
                        (COMPACT_W, COMPACT_H)
                    };
                    let bottom_y = cur_pos.1 + old_h;
                    let center_x = cur_pos.0 + old_w / 2.0;
                    let new_x = (center_x - new_w / 2.0).max(8.0);
                    let new_y = (bottom_y - new_h).max(24.0);

                    window.set_inner_size(LogicalSize::new(new_w, new_h));
                    window.set_outer_position(LogicalPosition::new(new_x, new_y));
                    is_expanded = expanded;
                }
            },
            _ => {}
        }
    });
}
