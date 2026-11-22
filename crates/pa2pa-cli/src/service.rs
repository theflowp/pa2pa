use crate::setup_mcp::CheckStatus;
use anyhow::{anyhow, bail, Result};
use pa2pa_core::{Config, Pa2paPaths};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct ServiceCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

pub fn install_service(paths: &Pa2paPaths, with_pill: bool) -> Result<()> {
    let cfg = Config::load(paths)?;
    let pa2pa_exe = std::env::current_exe()?;
    let pill_exe = crate::find_pill_binary();
    let ui_url = format!("http://{}", cfg.daemon.ui_bind);

    #[cfg(target_os = "macos")]
    {
        install_macos_launchd(paths, &pa2pa_exe, pill_exe.as_ref(), &ui_url, with_pill)?;
    }

    #[cfg(target_os = "linux")]
    {
        install_linux_systemd(paths, &pa2pa_exe, pill_exe.as_ref(), &ui_url, with_pill)?;
    }

    #[cfg(target_os = "windows")]
    {
        install_windows_startup(paths, &pa2pa_exe, pill_exe.as_ref(), &ui_url, with_pill)?;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (pa2pa_exe, pill_exe, ui_url, with_pill);
        bail!("Automatic service installation is supported on macOS, Linux, and Windows.");
    }

    Ok(())
}

pub fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let uid = macos_uid()?;
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot locate HOME"))?
            .join("Library")
            .join("LaunchAgents");
        for (label, file) in [
            ("dev.pa2pa.daemon", "dev.pa2pa.daemon.plist"),
            ("dev.pa2pa.pill", "dev.pa2pa.pill.plist"),
        ] {
            let p = agents_dir.join(file);
            if launchctl_is_loaded(&uid, label) {
                let target = format!("gui/{uid}/{label}");
                let out = Command::new("launchctl")
                    .args(["bootout", &target])
                    .output()?;
                if !out.status.success() {
                    let err = combined_output(&out);
                    eprintln!("Warning: launchctl bootout {target} returned error: {err}");
                }
            }
            if p.exists() {
                fs::remove_file(&p)?;
                println!("Removed LaunchAgent: {}", p.display());
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("Cannot locate XDG config dir"))?
            .join("systemd")
            .join("user");
        for unit in ["pa2pa-daemon.service", "pa2pa-pill.service"] {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", unit])
                .output();
            let p = unit_dir.join(unit);
            if p.exists() {
                fs::remove_file(&p)?;
                println!("Removed systemd user service: {}", p.display());
            }
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(startup) = windows_startup_dir() {
            let vbs = startup.join("pa2pa.vbs");
            if vbs.exists() {
                fs::remove_file(&vbs)?;
                println!("Removed Windows startup launcher: {}", vbs.display());
            }
        }
    }

    Ok(())
}

pub fn restart_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let uid = macos_uid()?;
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot locate HOME"))?
            .join("Library")
            .join("LaunchAgents");
        let mut restarted = 0usize;
        for (label, file) in [
            ("dev.pa2pa.daemon", "dev.pa2pa.daemon.plist"),
            ("dev.pa2pa.pill", "dev.pa2pa.pill.plist"),
        ] {
            let p = agents_dir.join(file);
            if !p.exists() {
                continue;
            }
            if launchctl_is_loaded(&uid, label) {
                let target = format!("gui/{uid}/{label}");
                let out = Command::new("launchctl")
                    .args(["kickstart", "-k", &target])
                    .output()?;
                if !out.status.success() {
                    bail!(
                        "Failed to restart {label} via `launchctl kickstart -k {target}`: {}",
                        combined_output(&out)
                    );
                }
            } else {
                let domain = format!("gui/{uid}");
                let out = Command::new("launchctl")
                    .args(["bootstrap", &domain])
                    .arg(&p)
                    .output()?;
                if !out.status.success() {
                    bail!(
                        "Failed to start {label} via `launchctl bootstrap`: {}",
                        combined_output(&out)
                    );
                }
            }
            println!("Restarted macOS LaunchAgent: {label}");
            restarted += 1;
        }
        if restarted == 0 {
            println!("No pa2pa LaunchAgents are installed. Run `pa2pa service install` first.");
        }
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let unit_dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("Cannot locate XDG config dir"))?
            .join("systemd")
            .join("user");
        let mut restarted = 0usize;
        for unit in ["pa2pa-daemon.service", "pa2pa-pill.service"] {
            if !unit_dir.join(unit).exists() {
                continue;
            }
            let out = Command::new("systemctl")
                .args(["--user", "restart", unit])
                .output()?;
            if !out.status.success() {
                bail!(
                    "Failed to restart {unit} via `systemctl --user restart`: {}",
                    combined_output(&out)
                );
            }
            println!("Restarted systemd user service: {unit}");
            restarted += 1;
        }
        if restarted == 0 {
            println!("No pa2pa systemd user services are installed. Run `pa2pa service install` first.");
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        println!(
            "On Windows, stop any running `pa2pa.exe` / `pa2pa-pill.exe` process (or sign out and back in) \
             so the Startup launcher picks up the new binary."
        );
        return Ok(());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        bail!("Service restart is supported on macOS, Linux, and Windows.");
    }
}

pub fn inspect_services() -> Vec<ServiceCheck> {
    let mut checks = Vec::new();

    #[cfg(target_os = "macos")]
    {
        let uid = macos_uid().ok();
        if let Some(home) = dirs::home_dir() {
            let agents_dir = home.join("Library").join("LaunchAgents");
            for (label, file) in [
                ("dev.pa2pa.daemon", "dev.pa2pa.daemon.plist"),
                ("dev.pa2pa.pill", "dev.pa2pa.pill.plist"),
            ] {
                let p = agents_dir.join(file);
                if !p.exists() {
                    checks.push(ServiceCheck {
                        name: label.to_string(),
                        status: CheckStatus::Missing,
                        detail: "not installed".to_string(),
                    });
                    continue;
                }
                if let Some(ref u) = uid {
                    match launchctl_service_state(u, label) {
                        LaunchdState::Running(pid) => checks.push(ServiceCheck {
                            name: label.to_string(),
                            status: CheckStatus::Ok,
                            detail: format!("running (PID {pid}) — {}", p.display()),
                        }),
                        LaunchdState::Loaded => checks.push(ServiceCheck {
                            name: label.to_string(),
                            status: CheckStatus::Ok,
                            detail: format!("loaded — {}", p.display()),
                        }),
                        LaunchdState::NotLoaded => checks.push(ServiceCheck {
                            name: label.to_string(),
                            status: CheckStatus::Warn,
                            detail: format!(
                                "installed ({}), but not loaded — run `pa2pa service restart`",
                                p.display()
                            ),
                        }),
                    }
                } else {
                    checks.push(ServiceCheck {
                        name: label.to_string(),
                        status: CheckStatus::Warn,
                        detail: format!("installed ({}), launchctl state unknown", p.display()),
                    });
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_dir = dirs::config_dir().map(|d| d.join("systemd").join("user"));
        for unit in ["pa2pa-daemon.service", "pa2pa-pill.service"] {
            let installed = unit_dir
                .as_ref()
                .map(|d| d.join(unit).exists())
                .unwrap_or(false);
            if !installed {
                checks.push(ServiceCheck {
                    name: unit.to_string(),
                    status: CheckStatus::Missing,
                    detail: "not installed".to_string(),
                });
                continue;
            }
            let out = Command::new("systemctl")
                .args(["--user", "is-active", unit])
                .output();
            let state = out
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".into());
            let status = if state == "active" {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            };
            checks.push(ServiceCheck {
                name: unit.to_string(),
                status,
                detail: state,
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(startup) = windows_startup_dir() {
            let vbs = startup.join("pa2pa.vbs");
            if vbs.exists() {
                checks.push(ServiceCheck {
                    name: "pa2pa.vbs".to_string(),
                    status: CheckStatus::Ok,
                    detail: format!("installed ({})", vbs.display()),
                });
            } else {
                checks.push(ServiceCheck {
                    name: "pa2pa.vbs".to_string(),
                    status: CheckStatus::Missing,
                    detail: "not installed".to_string(),
                });
            }
        }
    }

    checks
}

pub fn service_status() -> Result<()> {
    let checks = inspect_services();
    if checks.is_empty() {
        println!("  No service inspection available on this platform.");
        return Ok(());
    }
    for c in checks {
        println!("  [{}] {:<22} {}", c.status.tag(), c.name, c.detail);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn combined_output(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match (stderr.is_empty(), stdout.is_empty()) {
        (false, true) => stderr,
        (true, false) => stdout,
        (false, false) => format!("{stderr} ({stdout})"),
        (true, true) => format!("exit status {}", out.status),
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchdState {
    Running(u32),
    Loaded,
    NotLoaded,
}

#[cfg(target_os = "macos")]
fn macos_uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output()?;
    if !out.status.success() {
        bail!("Could not determine current UID via `id -u`");
    }
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        bail!("Unexpected output from `id -u`: `{uid}`");
    }
    Ok(uid)
}

#[cfg(target_os = "macos")]
fn launchctl_service_state(uid: &str, label: &str) -> LaunchdState {
    let target = format!("gui/{uid}/{label}");
    let Ok(out) = Command::new("launchctl")
        .args(["print", &target])
        .output()
    else {
        return LaunchdState::NotLoaded;
    };
    if !out.status.success() {
        return LaunchdState::NotLoaded;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            if let Ok(pid) = rest.trim().parse::<u32>() {
                return LaunchdState::Running(pid);
            }
        }
    }
    LaunchdState::Loaded
}

#[cfg(target_os = "macos")]
fn launchctl_is_loaded(uid: &str, label: &str) -> bool {
    !matches!(launchctl_service_state(uid, label), LaunchdState::NotLoaded)
}

#[cfg(target_os = "macos")]
fn macos_reload_launch_agent(uid: &str, label: &str, plist_path: &Path) -> Result<()> {
    if launchctl_is_loaded(uid, label) {
        let target = format!("gui/{uid}/{label}");
        let _ = Command::new("launchctl")
            .args(["bootout", &target])
            .output();
        // Give launchd a brief moment to release the service label before bootstrapping again.
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    let domain = format!("gui/{uid}");
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(plist_path)
        .output()?;
    if !out.status.success() {
        bail!(
            "Failed to load macOS LaunchAgent `{label}` ({}): {}",
            plist_path.display(),
            combined_output(&out)
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos_launchd(
    paths: &Pa2paPaths,
    pa2pa_exe: &Path,
    pill_exe: Option<&PathBuf>,
    ui_url: &str,
    with_pill: bool,
) -> Result<()> {
    let uid = macos_uid()?;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Cannot locate HOME"))?;
    let agents_dir = home.join("Library").join("LaunchAgents");
    let logs_dir = paths.home.join("logs");
    fs::create_dir_all(&agents_dir)?;
    fs::create_dir_all(&logs_dir)?;

    let daemon_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.pa2pa.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--home</string>
        <string>{pa2pa_home}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{logs}/daemon.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/daemon.err.log</string>
</dict>
</plist>
"#,
        exe = xml_escape(&pa2pa_exe.display().to_string()),
        pa2pa_home = xml_escape(&paths.home.display().to_string()),
        logs = xml_escape(&logs_dir.display().to_string()),
    );

    let daemon_path = agents_dir.join("dev.pa2pa.daemon.plist");
    fs::write(&daemon_path, daemon_plist)?;
    macos_reload_launch_agent(&uid, "dev.pa2pa.daemon", &daemon_path)?;
    println!(
        "Installed & started macOS daemon LaunchAgent: {}",
        daemon_path.display()
    );

    if with_pill {
        if let Some(pill) = pill_exe {
            let pill_plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.pa2pa.pill</string>
    <key>ProgramArguments</key>
    <array>
        <string>{pill}</string>
        <string>--url</string>
        <string>{ui_url}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{logs}/pill.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/pill.err.log</string>
</dict>
</plist>
"#,
                pill = xml_escape(&pill.display().to_string()),
                ui_url = xml_escape(ui_url),
                logs = xml_escape(&logs_dir.display().to_string()),
            );
            let pill_path = agents_dir.join("dev.pa2pa.pill.plist");
            fs::write(&pill_path, pill_plist)?;
            macos_reload_launch_agent(&uid, "dev.pa2pa.pill", &pill_path)?;
            println!(
                "Installed & started macOS menu bar + floating pill LaunchAgent: {}",
                pill_path.display()
            );
        } else {
            eprintln!("Warning: `pa2pa-pill` binary not found; skipped pill LaunchAgent.");
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "linux")]
fn systemd_quote(s: &str) -> Result<String> {
    if s.chars().any(|c| c.is_control()) {
        bail!("Path contains unsafe control characters");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "linux")]
fn install_linux_systemd(
    paths: &Pa2paPaths,
    pa2pa_exe: &Path,
    pill_exe: Option<&PathBuf>,
    ui_url: &str,
    with_pill: bool,
) -> Result<()> {
    let unit_dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("Cannot locate XDG config dir"))?
        .join("systemd")
        .join("user");
    fs::create_dir_all(&unit_dir)?;

    let exe_q = systemd_quote(&pa2pa_exe.display().to_string())?;
    let home_q = systemd_quote(&paths.home.display().to_string())?;
    let daemon_unit = format!(
        "[Unit]\nDescription=pa2pa E2E Agent Daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={exe_q} --home {home_q} daemon\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n"
    );
    let daemon_path = unit_dir.join("pa2pa-daemon.service");
    fs::write(&daemon_path, daemon_unit)?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    let enable_out = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pa2pa-daemon.service"])
        .output()?;
    if !enable_out.status.success() {
        bail!(
            "Failed to enable/start pa2pa-daemon.service via systemctl: {}",
            combined_output(&enable_out)
        );
    }
    println!(
        "Installed & started systemd user service: {}",
        daemon_path.display()
    );

    if with_pill {
        if let Some(pill) = pill_exe {
            let pill_q = systemd_quote(&pill.display().to_string())?;
            let url_q = systemd_quote(ui_url)?;
            let pill_unit = format!(
                "[Unit]\nDescription=pa2pa System Tray & Floating Pill\nAfter=pa2pa-daemon.service graphical-session.target\n\n\
                 [Service]\nExecStart={pill_q} --url {url_q}\nRestart=on-failure\nRestartSec=3\n\n\
                 [Install]\nWantedBy=graphical-session.target\n"
            );
            let pill_path = unit_dir.join("pa2pa-pill.service");
            fs::write(&pill_path, pill_unit)?;
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
            let pill_out = Command::new("systemctl")
                .args(["--user", "enable", "--now", "pa2pa-pill.service"])
                .output()?;
            if !pill_out.status.success() {
                bail!(
                    "Failed to enable/start pa2pa-pill.service via systemctl: {}",
                    combined_output(&pill_out)
                );
            }
            println!(
                "Installed & started systemd tray/pill service: {}",
                pill_path.display()
            );
        }
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_startup_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup"),
    )
}

#[cfg(target_os = "windows")]
fn install_windows_startup(
    paths: &Pa2paPaths,
    pa2pa_exe: &Path,
    _pill_exe: Option<&PathBuf>,
    _ui_url: &str,
    with_pill: bool,
) -> Result<()> {
    let startup = windows_startup_dir()
        .ok_or_else(|| anyhow!("Could not locate Windows Startup folder (%APPDATA%)"))?;
    fs::create_dir_all(&startup)?;
    let exe_str = pa2pa_exe.display().to_string();
    let home_str = paths.home.display().to_string();
    if exe_str.chars().any(|c| c.is_control() || c == '"')
        || home_str.chars().any(|c| c.is_control() || c == '"')
    {
        bail!("Executable or home path contains unsafe characters for Windows Startup VBS");
    }
    let pill_flag = if with_pill { " --pill" } else { "" };
    let vbs = format!(
        "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
         WshShell.Run \"\"\"{exe_str}\"\" --home \"\"{home_str}\"\" daemon{pill_flag}\", 0, False\r\n"
    );
    let vbs_path = startup.join("pa2pa.vbs");
    fs::write(&vbs_path, vbs)?;
    println!(
        "Installed Windows Startup launcher (background daemon + system tray icon): {}",
        vbs_path.display()
    );
    Ok(())
}
