use anyhow::{anyhow, Result};
use pa2pa_core::{Config, Pa2paPaths};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

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
        anyhow::bail!("Automatic service installation is supported on macOS, Linux, and Windows.");
    }

    Ok(())
}

pub fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot locate HOME"))?
            .join("Library")
            .join("LaunchAgents");
        for label in ["dev.pa2pa.daemon.plist", "dev.pa2pa.pill.plist"] {
            let p = agents_dir.join(label);
            if p.exists() {
                let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&p).status();
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
                .status();
            let p = unit_dir.join(unit);
            if p.exists() {
                fs::remove_file(&p)?;
                println!("Removed systemd user service: {}", p.display());
            }
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
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

pub fn service_status() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let agents_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot locate HOME"))?
            .join("Library")
            .join("LaunchAgents");
        for label in ["dev.pa2pa.daemon.plist", "dev.pa2pa.pill.plist"] {
            let p = agents_dir.join(label);
            println!(
                "  {} : {}",
                label,
                if p.exists() {
                    format!("installed ({})", p.display())
                } else {
                    "not installed".to_string()
                }
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        for unit in ["pa2pa-daemon.service", "pa2pa-pill.service"] {
            let out = Command::new("systemctl")
                .args(["--user", "is-active", unit])
                .output();
            let state = out
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".into());
            println!("  {unit} : {state}");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(startup) = windows_startup_dir() {
            let vbs = startup.join("pa2pa.vbs");
            println!(
                "  pa2pa.vbs : {}",
                if vbs.exists() {
                    format!("installed ({})", vbs.display())
                } else {
                    "not installed".to_string()
                }
            );
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos_launchd(
    paths: &Pa2paPaths,
    pa2pa_exe: &PathBuf,
    pill_exe: Option<&PathBuf>,
    ui_url: &str,
    with_pill: bool,
) -> Result<()> {
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
    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&daemon_path)
        .status();
    let _ = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&daemon_path)
        .status();
    println!("Installed & started macOS daemon LaunchAgent: {}", daemon_path.display());

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
            let _ = Command::new("launchctl")
                .args(["unload", "-w"])
                .arg(&pill_path)
                .status();
            let _ = Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&pill_path)
                .status();
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
fn install_linux_systemd(
    paths: &Pa2paPaths,
    pa2pa_exe: &PathBuf,
    pill_exe: Option<&PathBuf>,
    ui_url: &str,
    with_pill: bool,
) -> Result<()> {
    let unit_dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("Cannot locate XDG config dir"))?
        .join("systemd")
        .join("user");
    fs::create_dir_all(&unit_dir)?;

    let daemon_unit = format!(
        "[Unit]\nDescription=pa2pa E2E Agent Daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} --home {} daemon\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n",
        pa2pa_exe.display(),
        paths.home.display()
    );
    let daemon_path = unit_dir.join("pa2pa-daemon.service");
    fs::write(&daemon_path, daemon_unit)?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pa2pa-daemon.service"])
        .status();
    println!("Installed & started systemd user service: {}", daemon_path.display());

    if with_pill {
        if let Some(pill) = pill_exe {
            let pill_unit = format!(
                "[Unit]\nDescription=pa2pa System Tray & Floating Pill\nAfter=pa2pa-daemon.service graphical-session.target\n\n\
                 [Service]\nExecStart={} --url {}\nRestart=on-failure\nRestartSec=3\n\n\
                 [Install]\nWantedBy=graphical-session.target\n",
                pill.display(),
                ui_url
            );
            let pill_path = unit_dir.join("pa2pa-pill.service");
            fs::write(&pill_path, pill_unit)?;
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "enable", "--now", "pa2pa-pill.service"])
                .status();
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
    pa2pa_exe: &PathBuf,
    _pill_exe: Option<&PathBuf>,
    _ui_url: &str,
    with_pill: bool,
) -> Result<()> {
    let startup = windows_startup_dir()
        .ok_or_else(|| anyhow!("Could not locate Windows Startup folder (%APPDATA%)"))?;
    fs::create_dir_all(&startup)?;
    let pill_flag = if with_pill { " --pill" } else { "" };
    let vbs = format!(
        "Set WshShell = CreateObject(\"WScript.Shell\")\r\n\
         WshShell.Run \"\"\"{}\"\" --home \"\"{}\"\" daemon{}\", 0, False\r\n",
        pa2pa_exe.display(),
        paths.home.display(),
        pill_flag
    );
    let vbs_path = startup.join("pa2pa.vbs");
    fs::write(&vbs_path, vbs)?;
    println!(
        "Installed Windows Startup launcher (background daemon + system tray icon): {}",
        vbs_path.display()
    );
    Ok(())
}
