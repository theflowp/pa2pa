use anyhow::{anyhow, bail, Result};
use pa2pa_core::Pa2paPaths;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Missing,
    Info,
}

impl CheckStatus {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Missing => "--",
            Self::Info => "INFO",
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpCheck {
    pub tool: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeStatus {
    Updated,
    AlreadyConfigured,
}

pub fn compute_mcp_args(paths: &Pa2paPaths) -> Vec<String> {
    let is_custom_home = match Pa2paPaths::default_home() {
        Ok(default_home) => {
            let cur = fs::canonicalize(&paths.home).unwrap_or_else(|_| paths.home.clone());
            let def = fs::canonicalize(&default_home).unwrap_or(default_home);
            cur != def
        }
        Err(_) => true,
    };

    if is_custom_home {
        vec![
            "--home".to_string(),
            paths.home.display().to_string(),
            "mcp".to_string(),
        ]
    } else {
        vec!["mcp".to_string()]
    }
}

pub fn run_setup_mcp(paths: &Pa2paPaths, apply: bool, target: &str) -> Result<()> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "pa2pa".to_string());

    let args_vec = compute_mcp_args(paths);

    if !apply {
        let args_json = serde_json::to_string(&args_vec)?;
        let args_toml = format_toml_string_array(&args_vec);
        let exe_escaped = escape_toml_str(&exe);

        println!("=== Claude Code (user scope — available in all projects) ===");
        println!(
            "claude mcp add -s user pa2pa -- {exe} {}\n",
            args_vec.join(" ")
        );

        println!("=== Codex (~/.codex/config.toml) ===");
        println!("[mcp_servers.pa2pa]");
        println!("command = \"{exe_escaped}\"");
        println!("args = [{args_toml}]\n");

        println!("=== Mistral Vibe (~/.vibe/config.toml) ===");
        println!("[[mcp_servers]]");
        println!("name = \"pa2pa\"");
        println!("transport = \"stdio\"");
        println!("command = \"{exe_escaped}\"");
        println!("args = [{args_toml}]\n");

        println!("=== Antigravity (~/.gemini/config/mcp_config.json) & Cursor (~/.cursor/mcp.json) ===");
        println!(
            "{{\n  \"mcpServers\": {{\n    \"pa2pa\": {{\n      \"command\": {},\n      \"args\": {args_json}\n    }}\n  }}\n}}\n",
            serde_json::to_string(&exe)?
        );

        println!("=== OpenCode (~/.config/opencode/opencode.json) ===");
        let mut full_cmd = vec![exe.clone()];
        full_cmd.extend_from_slice(&args_vec);
        println!(
            "{{\n  \"mcp\": {{\n    \"pa2pa\": {{\n      \"type\": \"local\",\n      \"command\": {},\n      \"enabled\": true\n    }}\n  }}\n}}\n",
            serde_json::to_string(&full_cmd)?
        );

        println!("=== T3 Code ===");
        println!(
            "T3 Code has no standalone MCP config file; it automatically uses the MCP servers\n\
             configured for its providers (Claude Code, Codex, Antigravity, Mistral Vibe).\n"
        );

        println!(
            "Tip: pass `--apply` to automatically configure detected tools on this machine\n\
             (targets: all, claude, codex, antigravity, vibe, opencode, cursor)."
        );
        return Ok(());
    }

    let tgt = target.trim().to_ascii_lowercase();
    let do_all = tgt == "all" || tgt.is_empty();
    let valid_targets = [
        "all",
        "",
        "claude",
        "claude-code",
        "codex",
        "antigravity",
        "agy",
        "opencode",
        "cursor",
        "vibe",
        "mistral-vibe",
    ];
    if !valid_targets.contains(&tgt.as_str()) {
        bail!(
            "Unknown --target `{target}`. Supported targets: all, claude, codex, antigravity, vibe, opencode, cursor."
        );
    }

    let mut configured: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // 1. Claude Code (user scope via `claude mcp add -s user`)
    if do_all || tgt == "claude" || tgt == "claude-code" {
        if let Some(claude_bin) = pa2pa_daemon::runners::find_in_path("claude") {
            match configure_claude_code(&claude_bin, &exe, &args_vec) {
                Ok(msg) => configured.push(msg),
                Err(e) => errors.push(format!("Claude Code: {e}")),
            }
        } else if !do_all {
            errors.push(format!(
                "Claude Code: `claude` CLI not found in PATH. Once installed, run:\n      claude mcp add -s user pa2pa -- {exe} {}",
                args_vec.join(" ")
            ));
        }
    }

    // 2. Codex (`~/.codex/config.toml`)
    if do_all || tgt == "codex" {
        if let Some(home) = dirs::home_dir() {
            let codex_dir = home.join(".codex");
            let cli_installed = pa2pa_daemon::runners::find_in_path("codex").is_some();
            if cli_installed || codex_dir.is_dir() || !do_all {
                let cfg_path = codex_dir.join("config.toml");
                match merge_codex_toml(&cfg_path, &exe, &args_vec) {
                    Ok(status) => {
                        let suffix = format_status_suffix(status, cli_installed, "codex");
                        configured.push(format!("Codex ({}){suffix}", cfg_path.display()));
                    }
                    Err(e) => errors.push(format!("Codex ({}): {e}", cfg_path.display())),
                }
            }
        }
    }

    // 3. Antigravity (`~/.gemini/config/mcp_config.json`)
    if do_all || tgt == "antigravity" || tgt == "agy" {
        if let Some(home) = dirs::home_dir() {
            let gemini_cfg_dir = home.join(".gemini").join("config");
            let gemini_agy_dir = home.join(".gemini").join("antigravity");
            let cli_installed = pa2pa_daemon::runners::find_in_path("agy").is_some()
                || pa2pa_daemon::runners::find_in_path("antigravity").is_some();
            let dir_present = gemini_cfg_dir.is_dir() || gemini_agy_dir.is_dir();
            if cli_installed || dir_present || !do_all {
                let p = gemini_cfg_dir.join("mcp_config.json");
                match merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec) {
                    Ok(()) => {
                        let suffix = format_status_suffix(
                            MergeStatus::Updated,
                            cli_installed,
                            "agy/antigravity",
                        );
                        configured.push(format!("Antigravity ({}){suffix}", p.display()));
                    }
                    Err(e) => errors.push(format!("Antigravity ({}): {e}", p.display())),
                }
            }
        }
    }

    // 4. Mistral Vibe (`~/.vibe/config.toml` — `[[mcp_servers]]`)
    if do_all || tgt == "vibe" || tgt == "mistral-vibe" {
        if let Some(home) = dirs::home_dir() {
            let vibe_dir = home.join(".vibe");
            let cli_installed = pa2pa_daemon::runners::find_in_path("vibe").is_some();
            if cli_installed || vibe_dir.is_dir() || !do_all {
                let p = vibe_dir.join("config.toml");
                match merge_vibe_toml(&p, &exe, &args_vec) {
                    Ok(status) => {
                        let suffix = format_status_suffix(status, cli_installed, "vibe");
                        configured.push(format!("Mistral Vibe ({}){suffix}", p.display()));
                    }
                    Err(e) => errors.push(format!("Mistral Vibe ({}): {e}", p.display())),
                }
                let legacy_json = vibe_dir.join("mcp.json");
                if legacy_json.is_file() {
                    notes.push(format!(
                        "Legacy {} found (Mistral Vibe ignores mcp.json and reads config.toml); you can safely remove it.",
                        legacy_json.display()
                    ));
                }
            }
        }
    }

    // 5. OpenCode (`~/.config/opencode/opencode.json`)
    if do_all || tgt == "opencode" {
        if let Some(home) = dirs::home_dir() {
            let oc_dir = home.join(".config").join("opencode");
            let cli_installed = pa2pa_daemon::runners::find_in_path("opencode").is_some();
            if cli_installed || oc_dir.is_dir() || !do_all {
                let p = oc_dir.join("opencode.json");
                match merge_opencode_json(&p, &exe, &args_vec) {
                    Ok(()) => {
                        let suffix =
                            format_status_suffix(MergeStatus::Updated, cli_installed, "opencode");
                        configured.push(format!("OpenCode ({}){suffix}", p.display()));
                    }
                    Err(e) => errors.push(format!("OpenCode ({}): {e}", p.display())),
                }
            }
        }
    }

    // 6. Cursor (`~/.cursor/mcp.json`)
    if do_all || tgt == "cursor" {
        if let Some(home) = dirs::home_dir() {
            let cursor_dir = home.join(".cursor");
            let installed = is_cursor_installed(&home);
            if installed || cursor_dir.is_dir() || !do_all {
                let p = cursor_dir.join("mcp.json");
                match merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec) {
                    Ok(()) => {
                        let suffix =
                            format_status_suffix(MergeStatus::Updated, installed, "cursor");
                        configured.push(format!("Cursor ({}){suffix}", p.display()));
                    }
                    Err(e) => errors.push(format!("Cursor ({}): {e}", p.display())),
                }
            }
        }
    }

    if configured.is_empty() && errors.is_empty() {
        println!(
            "No supported agent installations or config directories were auto-detected.\n\
             Run `pa2pa setup-mcp` (without --apply) to print manual configuration snippets."
        );
    } else {
        if !configured.is_empty() {
            println!("Configured pa2pa MCP server for:");
            for item in &configured {
                println!("  [OK] {item}");
            }
        }
        if !errors.is_empty() {
            println!("\nFailed to configure:");
            for err in &errors {
                println!("  [ERR] {err}");
            }
        }
        for note in &notes {
            println!("\nNote: {note}");
        }
        if do_all {
            println!(
                "\nNote: T3 Code automatically inherits MCP configs from its providers (Claude Code, Codex, Antigravity, Mistral Vibe)."
            );
        }
        if !configured.is_empty() {
            println!("Restart your AI agent(s) so they pick up the `pa2pa` MCP tools.");
        }
    }

    if !errors.is_empty() {
        bail!(
            "{} MCP target(s) failed to configure (see errors above).",
            errors.len()
        );
    }

    Ok(())
}

fn format_status_suffix(status: MergeStatus, cli_installed: bool, binary_name: &str) -> String {
    let mut parts = Vec::new();
    if status == MergeStatus::AlreadyConfigured {
        parts.push("already configured".to_string());
    }
    if !cli_installed {
        parts.push(format!("config dir found; `{binary_name}` not in PATH"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join("; "))
    }
}

fn is_cursor_installed(home: &Path) -> bool {
    if pa2pa_daemon::runners::find_in_path("cursor").is_some() {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        if Path::new("/Applications/Cursor.app").exists()
            || home.join("Applications").join("Cursor.app").exists()
        {
            return true;
        }
    }
    let _ = home;
    false
}

fn configure_claude_code(claude_bin: &Path, exe: &str, args: &[String]) -> Result<String> {
    let out = Command::new(claude_bin)
        .args(["mcp", "add", "-s", "user", "pa2pa", "--", exe])
        .args(args)
        .output()?;

    if out.status.success() {
        return Ok("Claude Code (user scope via `claude mcp add -s user`)".to_string());
    }

    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();

    if combined.contains("already exists") {
        // Refresh the user-scope entry so binary path or --home changes take effect.
        let remove_out = Command::new(claude_bin)
            .args(["mcp", "remove", "-s", "user", "pa2pa"])
            .output()?;
        if remove_out.status.success() {
            let readd_out = Command::new(claude_bin)
                .args(["mcp", "add", "-s", "user", "pa2pa", "--", exe])
                .args(args)
                .output()?;
            if readd_out.status.success() {
                return Ok("Claude Code (updated user scope via `claude mcp`)".to_string());
            }
            let readd_err = String::from_utf8_lossy(&readd_out.stderr)
                .trim()
                .to_string();
            let readd_msg = if readd_err.is_empty() {
                String::from_utf8_lossy(&readd_out.stdout)
                    .trim()
                    .to_string()
            } else {
                readd_err
            };
            bail!("re-adding user-scope MCP server failed: {readd_msg}");
        }

        // If user-scope entry already exists (and couldn't be removed for some reason), accept it.
        if claude_has_user_scope_entry() {
            return Ok("Claude Code (already registered in user scope)".to_string());
        }

        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!(
            "`claude mcp add -s user` reported `pa2pa` already exists (possibly in local scope): {detail}. \
             Remove the local entry with `claude mcp remove pa2pa` and re-run `pa2pa setup-mcp --apply --target claude`."
        );
    }

    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit status {}", out.status)
    };
    bail!("`claude mcp add -s user` failed: {detail} (left ~/.claude.json untouched)")
}

fn claude_has_user_scope_entry() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".claude.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    doc.get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key("pa2pa"))
        .unwrap_or(false)
}

fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn backup_file_preserving_mode(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read(path)?;
    let orig_mode = file_mode(path);
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    for attempt in 0u32..1000 {
        let candidate_name = if attempt == 0 {
            format!("{file_name}.pa2pa.bak.{ts}")
        } else {
            format!("{file_name}.pa2pa.bak.{ts}.{attempt}")
        };
        let candidate = parent.join(candidate_name);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(orig_mode.unwrap_or(0o600));
        }
        match opts.open(&candidate) {
            Ok(mut f) => {
                f.write_all(&raw)?;
                f.sync_all().ok();
                #[cfg(unix)]
                if let Some(m) = orig_mode {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&candidate, fs::Permissions::from_mode(m));
                }
                return Ok(Some(candidate));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }

    bail!(
        "Could not create a unique backup file for {}",
        path.display()
    )
}

fn write_file_preserving_mode(
    path: &Path,
    content: &[u8],
    orig_mode: Option<u32>,
    default_new_mode: u32,
) -> Result<()> {
    if path.exists() {
        fs::write(path, content)?;
        #[cfg(unix)]
        if let Some(m) = orig_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(m));
        }
    } else {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(orig_mode.unwrap_or(default_new_mode));
        }
        let mut f = opts.open(path)?;
        f.write_all(content)?;
        f.sync_all().ok();
    }
    Ok(())
}

fn parse_strict_json_object(path: &Path, raw: &str) -> Result<Value> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let val: Value = serde_json::from_str(raw).map_err(|e| {
        anyhow!(
            "Refusing to overwrite {}: file is not valid strict JSON ({e}). \
             It may contain comments or trailing commas (JSONC). Existing file was left untouched; \
             run `pa2pa setup-mcp` to copy the snippet manually.",
            path.display()
        )
    })?;
    if !val.is_object() {
        bail!(
            "Refusing to overwrite {}: top-level JSON value must be an object (existing file left untouched).",
            path.display()
        );
    }
    Ok(val)
}

fn merge_mcp_servers_json(
    path: &Path,
    root_key: &str,
    exe: &str,
    args: &[String],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let orig_mode = file_mode(path);
    let mut doc: Value = if path.exists() {
        let raw = fs::read_to_string(path)?;
        parse_strict_json_object(path, &raw)?
    } else {
        json!({})
    };

    let map = doc.as_object_mut().unwrap();
    let servers = map
        .entry(root_key.to_string())
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        bail!(
            "Refusing to overwrite {}: `{root_key}` is not a JSON object (existing file left untouched).",
            path.display()
        );
    }
    servers.as_object_mut().unwrap().insert(
        "pa2pa".to_string(),
        json!({
            "command": exe,
            "args": args
        }),
    );

    backup_file_preserving_mode(path)?;
    let out = format!("{}\n", serde_json::to_string_pretty(&doc)?);
    write_file_preserving_mode(path, out.as_bytes(), orig_mode, 0o600)?;
    Ok(())
}

fn merge_opencode_json(path: &Path, exe: &str, args: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let orig_mode = file_mode(path);
    let mut doc: Value = if path.exists() {
        let raw = fs::read_to_string(path)?;
        parse_strict_json_object(path, &raw)?
    } else {
        json!({})
    };

    let map = doc.as_object_mut().unwrap();
    let mcp = map.entry("mcp".to_string()).or_insert_with(|| json!({}));
    if !mcp.is_object() {
        bail!(
            "Refusing to overwrite {}: `mcp` is not a JSON object (existing file left untouched).",
            path.display()
        );
    }
    let mut full_cmd = vec![exe.to_string()];
    full_cmd.extend_from_slice(args);
    mcp.as_object_mut().unwrap().insert(
        "pa2pa".to_string(),
        json!({
            "type": "local",
            "command": full_cmd,
            "enabled": true
        }),
    );

    backup_file_preserving_mode(path)?;
    let out = format!("{}\n", serde_json::to_string_pretty(&doc)?);
    write_file_preserving_mode(path, out.as_bytes(), orig_mode, 0o600)?;
    Ok(())
}

fn escape_toml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn format_toml_string_array(items: &[String]) -> String {
    items
        .iter()
        .map(|a| format!("\"{}\"", escape_toml_str(a)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn merge_codex_toml(path: &Path, exe: &str, args: &[String]) -> Result<MergeStatus> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let orig_mode = file_mode(path);
    let mut content = if path.exists() {
        let raw = fs::read_to_string(path)?;
        if !raw.trim().is_empty() {
            toml::from_str::<toml::Value>(&raw).map_err(|e| {
                anyhow!(
                    "Refusing to overwrite {}: file is not valid TOML ({e}). Existing file was left untouched.",
                    path.display()
                )
            })?;
        }
        raw
    } else {
        String::new()
    };

    let args_toml = format_toml_string_array(args);
    let exe_escaped = escape_toml_str(exe);
    let block = format!(
        "# >>> pa2pa MCP >>>\n[mcp_servers.pa2pa]\ncommand = \"{exe_escaped}\"\nargs = [{args_toml}]\n# <<< pa2pa MCP <<<\n"
    );

    if let (Some(start), Some(end)) = (
        content.find("# >>> pa2pa MCP >>>"),
        content.find("# <<< pa2pa MCP <<<"),
    ) {
        let mut after_end = end + "# <<< pa2pa MCP <<<".len();
        if content[after_end..].starts_with('\n') {
            after_end += 1;
        }
        content.replace_range(start..after_end, &block);
    } else if has_codex_pa2pa_entry(&content) {
        return Ok(MergeStatus::AlreadyConfigured);
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&block);
    }

    backup_file_preserving_mode(path)?;
    write_file_preserving_mode(path, content.as_bytes(), orig_mode, 0o600)?;
    Ok(MergeStatus::Updated)
}

fn merge_vibe_toml(path: &Path, exe: &str, args: &[String]) -> Result<MergeStatus> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let orig_mode = file_mode(path);
    let mut content = if path.exists() {
        let raw = fs::read_to_string(path)?;
        if !raw.trim().is_empty() {
            toml::from_str::<toml::Value>(&raw).map_err(|e| {
                anyhow!(
                    "Refusing to overwrite {}: file is not valid TOML ({e}). Existing file was left untouched.",
                    path.display()
                )
            })?;
        }
        raw
    } else {
        String::new()
    };

    let args_toml = format_toml_string_array(args);
    let exe_escaped = escape_toml_str(exe);
    let block = format!(
        "# >>> pa2pa MCP >>>\n[[mcp_servers]]\nname = \"pa2pa\"\ntransport = \"stdio\"\ncommand = \"{exe_escaped}\"\nargs = [{args_toml}]\n# <<< pa2pa MCP <<<\n"
    );

    if let (Some(start), Some(end)) = (
        content.find("# >>> pa2pa MCP >>>"),
        content.find("# <<< pa2pa MCP <<<"),
    ) {
        let mut after_end = end + "# <<< pa2pa MCP <<<".len();
        if content[after_end..].starts_with('\n') {
            after_end += 1;
        }
        content.replace_range(start..after_end, &block);
    } else if has_vibe_pa2pa_entry(&content) {
        return Ok(MergeStatus::AlreadyConfigured);
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&block);
    }

    backup_file_preserving_mode(path)?;
    write_file_preserving_mode(path, content.as_bytes(), orig_mode, 0o600)?;
    Ok(MergeStatus::Updated)
}

fn has_codex_pa2pa_entry(raw: &str) -> bool {
    let Ok(doc) = toml::from_str::<toml::Value>(raw) else {
        return raw.contains("[mcp_servers.pa2pa]");
    };
    doc.get("mcp_servers")
        .and_then(|v| v.as_table())
        .map(|t| t.contains_key("pa2pa"))
        .unwrap_or(false)
}

fn has_vibe_pa2pa_entry(raw: &str) -> bool {
    let Ok(doc) = toml::from_str::<toml::Value>(raw) else {
        return false;
    };
    doc.get("mcp_servers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|item| {
                item.as_table()
                    .and_then(|t| t.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("pa2pa")
            })
        })
        .unwrap_or(false)
}

fn check_command_exists(cmd: &str) -> bool {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains(std::path::MAIN_SEPARATOR) {
        p.is_file()
    } else {
        pa2pa_daemon::runners::find_in_path(cmd).is_some()
    }
}

pub fn inspect_mcp_registrations(_paths: &Pa2paPaths) -> Vec<McpCheck> {
    let mut checks = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return checks;
    };

    // 1. Claude Code (`~/.claude.json` read-only)
    {
        let cli_installed = pa2pa_daemon::runners::find_in_path("claude").is_some();
        let path = home.join(".claude.json");
        if let Ok(raw) = fs::read_to_string(&path) {
            match serde_json::from_str::<Value>(&raw) {
                Ok(doc) => {
                    if let Some(entry) = doc
                        .get("mcpServers")
                        .and_then(|v| v.as_object())
                        .and_then(|m| m.get("pa2pa"))
                    {
                        let cmd = entry
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if cmd.is_empty() || check_command_exists(cmd) {
                            checks.push(McpCheck {
                                tool: "Claude Code",
                                status: CheckStatus::Ok,
                                detail: format!("registered (user scope -> {cmd})"),
                            });
                        } else {
                            checks.push(McpCheck {
                                tool: "Claude Code",
                                status: CheckStatus::Warn,
                                detail: format!("registered (user scope), but command missing: {cmd}"),
                            });
                        }
                    } else {
                        let local_projects: Vec<&str> = doc
                            .get("projects")
                            .and_then(|v| v.as_object())
                            .map(|projs| {
                                projs
                                    .iter()
                                    .filter(|(_, pv)| {
                                        pv.get("mcpServers")
                                            .and_then(|m| m.as_object())
                                            .map(|m| m.contains_key("pa2pa"))
                                            .unwrap_or(false)
                                    })
                                    .map(|(k, _)| k.as_str())
                                    .collect()
                            })
                            .unwrap_or_default();

                        if !local_projects.is_empty() {
                            checks.push(McpCheck {
                                tool: "Claude Code",
                                status: CheckStatus::Warn,
                                detail: format!(
                                    "registered only in local scope ({} project(s)), not user scope — run `pa2pa setup-mcp --apply --target claude`",
                                    local_projects.len()
                                ),
                            });
                        } else {
                            checks.push(McpCheck {
                                tool: "Claude Code",
                                status: if cli_installed {
                                    CheckStatus::Warn
                                } else {
                                    CheckStatus::Missing
                                },
                                detail: if cli_installed {
                                    "CLI installed, pa2pa MCP not registered".into()
                                } else {
                                    "not registered".into()
                                },
                            });
                        }
                    }
                }
                Err(_) => checks.push(McpCheck {
                    tool: "Claude Code",
                    status: CheckStatus::Warn,
                    detail: format!("could not parse {}", path.display()),
                }),
            }
        } else {
            checks.push(McpCheck {
                tool: "Claude Code",
                status: if cli_installed {
                    CheckStatus::Warn
                } else {
                    CheckStatus::Missing
                },
                detail: if cli_installed {
                    "CLI installed, ~/.claude.json not found".into()
                } else {
                    "not installed".into()
                },
            });
        }
    }

    // 2. Codex (`~/.codex/config.toml`)
    {
        let cli_installed = pa2pa_daemon::runners::find_in_path("codex").is_some();
        let path = home.join(".codex").join("config.toml");
        if let Ok(raw) = fs::read_to_string(&path) {
            match toml::from_str::<toml::Value>(&raw) {
                Ok(doc) => {
                    if let Some(entry) = doc
                        .get("mcp_servers")
                        .and_then(|v| v.as_table())
                        .and_then(|t| t.get("pa2pa"))
                    {
                        let cmd = entry
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let cli_note = if cli_installed {
                            ""
                        } else {
                            " [CLI not in PATH]"
                        };
                        if cmd.is_empty() || check_command_exists(cmd) {
                            checks.push(McpCheck {
                                tool: "Codex",
                                status: CheckStatus::Ok,
                                detail: format!("registered ({} -> {cmd}){cli_note}", path.display()),
                            });
                        } else {
                            checks.push(McpCheck {
                                tool: "Codex",
                                status: CheckStatus::Warn,
                                detail: format!("registered in {}, but command missing: {cmd}", path.display()),
                            });
                        }
                    } else {
                        checks.push(McpCheck {
                            tool: "Codex",
                            status: if cli_installed {
                                CheckStatus::Warn
                            } else {
                                CheckStatus::Missing
                            },
                            detail: format!("pa2pa not found in {}", path.display()),
                        });
                    }
                }
                Err(e) => checks.push(McpCheck {
                    tool: "Codex",
                    status: CheckStatus::Warn,
                    detail: format!("invalid TOML in {} ({e})", path.display()),
                }),
            }
        } else {
            checks.push(McpCheck {
                tool: "Codex",
                status: if cli_installed {
                    CheckStatus::Warn
                } else {
                    CheckStatus::Missing
                },
                detail: if cli_installed {
                    "CLI installed, ~/.codex/config.toml not found".into()
                } else {
                    "not installed".into()
                },
            });
        }
    }

    // 3. Antigravity (`~/.gemini/config/mcp_config.json`)
    {
        let cli_installed = pa2pa_daemon::runners::find_in_path("agy").is_some()
            || pa2pa_daemon::runners::find_in_path("antigravity").is_some();
        let path = home.join(".gemini").join("config").join("mcp_config.json");
        checks.push(inspect_json_mcp_file(
            "Antigravity",
            &path,
            "mcpServers",
            cli_installed,
            false,
        ));
    }

    // 4. Mistral Vibe (`~/.vibe/config.toml`)
    {
        let cli_installed = pa2pa_daemon::runners::find_in_path("vibe").is_some();
        let path = home.join(".vibe").join("config.toml");
        let legacy = home.join(".vibe").join("mcp.json");
        if let Ok(raw) = fs::read_to_string(&path) {
            match toml::from_str::<toml::Value>(&raw) {
                Ok(doc) => {
                    let entry = doc
                        .get("mcp_servers")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| {
                            arr.iter().find(|item| {
                                item.as_table()
                                    .and_then(|t| t.get("name"))
                                    .and_then(|n| n.as_str())
                                    == Some("pa2pa")
                            })
                        });
                    if let Some(entry) = entry {
                        let cmd = entry
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let cli_note = if cli_installed {
                            ""
                        } else {
                            " [CLI not in PATH]"
                        };
                        if cmd.is_empty() || check_command_exists(cmd) {
                            checks.push(McpCheck {
                                tool: "Mistral Vibe",
                                status: CheckStatus::Ok,
                                detail: format!("registered ({} -> {cmd}){cli_note}", path.display()),
                            });
                        } else {
                            checks.push(McpCheck {
                                tool: "Mistral Vibe",
                                status: CheckStatus::Warn,
                                detail: format!("registered in {}, but command missing: {cmd}", path.display()),
                            });
                        }
                    } else if legacy.is_file() {
                        checks.push(McpCheck {
                            tool: "Mistral Vibe",
                            status: CheckStatus::Warn,
                            detail: format!(
                                "not in {} (found legacy {} which Vibe ignores — run `pa2pa setup-mcp --apply --target vibe`)",
                                path.display(),
                                legacy.display()
                            ),
                        });
                    } else {
                        checks.push(McpCheck {
                            tool: "Mistral Vibe",
                            status: if cli_installed {
                                CheckStatus::Warn
                            } else {
                                CheckStatus::Missing
                            },
                            detail: format!("pa2pa not found in {}", path.display()),
                        });
                    }
                }
                Err(e) => checks.push(McpCheck {
                    tool: "Mistral Vibe",
                    status: CheckStatus::Warn,
                    detail: format!("invalid TOML in {} ({e})", path.display()),
                }),
            }
        } else if legacy.is_file() {
            checks.push(McpCheck {
                tool: "Mistral Vibe",
                status: CheckStatus::Warn,
                detail: format!(
                    "found legacy {} which Vibe ignores; run `pa2pa setup-mcp --apply --target vibe` to write {}",
                    legacy.display(),
                    path.display()
                ),
            });
        } else {
            checks.push(McpCheck {
                tool: "Mistral Vibe",
                status: if cli_installed {
                    CheckStatus::Warn
                } else {
                    CheckStatus::Missing
                },
                detail: if cli_installed {
                    "CLI installed, ~/.vibe/config.toml not found".into()
                } else {
                    "not installed".into()
                },
            });
        }
    }

    // 5. OpenCode (`~/.config/opencode/opencode.json`)
    {
        let cli_installed = pa2pa_daemon::runners::find_in_path("opencode").is_some();
        let path = home
            .join(".config")
            .join("opencode")
            .join("opencode.json");
        checks.push(inspect_json_mcp_file(
            "OpenCode",
            &path,
            "mcp",
            cli_installed,
            true,
        ));
    }

    // 6. Cursor (`~/.cursor/mcp.json`)
    {
        let installed = is_cursor_installed(&home);
        let path = home.join(".cursor").join("mcp.json");
        checks.push(inspect_json_mcp_file(
            "Cursor",
            &path,
            "mcpServers",
            installed,
            false,
        ));
    }

    // 7. T3 Code
    checks.push(McpCheck {
        tool: "T3 Code",
        status: CheckStatus::Info,
        detail: "uses provider MCP configs (Claude Code, Codex, Antigravity, Mistral Vibe)".into(),
    });

    checks
}

fn inspect_json_mcp_file(
    tool: &'static str,
    path: &Path,
    root_key: &str,
    installed: bool,
    opencode_array_cmd: bool,
) -> McpCheck {
    let cli_note = if installed { "" } else { " [CLI not in PATH]" };
    let Ok(raw) = fs::read_to_string(path) else {
        return McpCheck {
            tool,
            status: if installed {
                CheckStatus::Warn
            } else {
                CheckStatus::Missing
            },
            detail: if installed {
                format!("installed, {} not found", path.display())
            } else {
                "not installed".into()
            },
        };
    };

    let doc = match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v,
        Err(e) => {
            return McpCheck {
                tool,
                status: CheckStatus::Warn,
                detail: format!("could not parse {} ({e})", path.display()),
            };
        }
    };

    let Some(entry) = doc
        .get(root_key)
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("pa2pa"))
    else {
        return McpCheck {
            tool,
            status: if installed {
                CheckStatus::Warn
            } else {
                CheckStatus::Missing
            },
            detail: format!("pa2pa not found in {}", path.display()),
        };
    };

    let cmd = if opencode_array_cmd {
        entry
            .get("command")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("")
    } else {
        entry.get("command").and_then(|v| v.as_str()).unwrap_or("")
    };

    if cmd.is_empty() || check_command_exists(cmd) {
        McpCheck {
            tool,
            status: CheckStatus::Ok,
            detail: format!("registered ({} -> {cmd}){cli_note}", path.display()),
        }
    } else {
        McpCheck {
            tool,
            status: CheckStatus::Warn,
            detail: format!(
                "registered in {}, but command missing: {cmd}",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pa2pa-setup-mcp-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn compute_mcp_args_includes_home_for_custom_dir() {
        let custom = temp_dir("custom-home");
        let paths = Pa2paPaths {
            home: custom.clone(),
        };
        let args = compute_mcp_args(&paths);
        assert_eq!(
            args,
            vec!["--home".to_string(), custom.display().to_string(), "mcp".to_string()]
        );
        let _ = fs::remove_dir_all(&custom);
    }

    #[test]
    fn invalid_jsonc_file_is_not_overwritten() {
        let dir = temp_dir("jsonc");
        let path = dir.join("mcp_config.json");
        let original = "{\n  // user comment\n  \"mcpServers\": {\n    \"keepme\": { \"command\": \"foo\", },\n  }\n}\n";
        fs::write(&path, original).unwrap();

        let err = merge_mcp_servers_json(&path, "mcpServers", "/usr/bin/pa2pa", &["mcp".into()])
            .unwrap_err();
        assert!(err.to_string().contains("Refusing to overwrite"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backups_are_unique_and_preserve_0600_mode() {
        let dir = temp_dir("backup-mode");
        let path = dir.join("config.toml");
        fs::write(&path, "model = \"gpt-5\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        merge_codex_toml(&path, "/bin/pa2pa-v1", &["mcp".into()]).unwrap();
        merge_codex_toml(&path, "/bin/pa2pa-v2", &["mcp".into()]).unwrap();

        let backups: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("config.toml.pa2pa.bak."))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(backups.len(), 2, "expected 2 distinct timestamped backups");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            for b in &backups {
                assert_eq!(fs::metadata(b).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("/bin/pa2pa-v2"));
        assert!(!updated.contains("/bin/pa2pa-v1"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn vibe_toml_merges_and_updates_without_duplicates() {
        let dir = temp_dir("vibe");
        let path = dir.join("config.toml");
        fs::write(&path, "active_model = \"devstral\"\n").unwrap();

        let s1 = merge_vibe_toml(&path, "/bin/pa2pa-1", &["mcp".into()]).unwrap();
        assert_eq!(s1, MergeStatus::Updated);
        let raw1 = fs::read_to_string(&path).unwrap();
        let parsed1: toml::Value = toml::from_str(&raw1).unwrap();
        let servers = parsed1["mcp_servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"].as_str(), Some("pa2pa"));
        assert_eq!(servers[0]["transport"].as_str(), Some("stdio"));
        assert_eq!(servers[0]["command"].as_str(), Some("/bin/pa2pa-1"));

        let s2 = merge_vibe_toml(&path, "/bin/pa2pa-2", &["--home".into(), "/tmp/h".into(), "mcp".into()]).unwrap();
        assert_eq!(s2, MergeStatus::Updated);
        let raw2 = fs::read_to_string(&path).unwrap();
        let parsed2: toml::Value = toml::from_str(&raw2).unwrap();
        let servers2 = parsed2["mcp_servers"].as_array().unwrap();
        assert_eq!(servers2.len(), 1);
        assert_eq!(servers2[0]["command"].as_str(), Some("/bin/pa2pa-2"));

        // Pre-existing unmarked entry should not be duplicated
        let path2 = dir.join("manual.toml");
        fs::write(
            &path2,
            "[[mcp_servers]]\nname = \"pa2pa\"\ntransport = \"stdio\"\ncommand = \"/custom/pa2pa\"\nargs = [\"mcp\"]\n",
        )
        .unwrap();
        let s3 = merge_vibe_toml(&path2, "/bin/pa2pa-3", &["mcp".into()]).unwrap();
        assert_eq!(s3, MergeStatus::AlreadyConfigured);

        let _ = fs::remove_dir_all(&dir);
    }
}
