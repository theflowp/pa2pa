use anyhow::Result;
use pa2pa_core::Pa2paPaths;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub fn run_setup_mcp(paths: &Pa2paPaths, apply: bool, target: &str) -> Result<()> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "pa2pa".to_string());

    let args_vec: Vec<String> = if std::env::var("PA2PA_HOME").is_ok() {
        vec![
            "--home".to_string(),
            paths.home.display().to_string(),
            "mcp".to_string(),
        ]
    } else {
        vec!["mcp".to_string()]
    };

    if !apply {
        let args_json = serde_json::to_string(&args_vec)?;
        println!("=== Claude Code ===");
        println!("claude mcp add pa2pa -- {exe} {}\n", args_vec.join(" "));

        println!("=== Codex (~/.codex/config.toml) ===");
        println!("[mcp_servers.pa2pa]");
        println!("command = \"{exe}\"");
        println!("args = {args_json}\n");

        println!("=== Generic MCP JSON (T3 Code, Antigravity, OpenCode, Hermes, Mistral Vibe, Cursor) ===");
        println!(
            "{{\n  \"mcpServers\": {{\n    \"pa2pa\": {{\n      \"command\": \"{exe}\",\n      \"args\": {args_json}\n    }}\n  }}\n}}"
        );
        println!("\nTip: pass `--apply` to automatically configure detected tools on this machine.");
        return Ok(());
    }

    let tgt = target.trim().to_ascii_lowercase();
    let do_all = tgt == "all" || tgt.is_empty();
    let mut configured = Vec::new();

    // 1. Claude Code (`~/.claude.json` or `claude mcp add`)
    if do_all || tgt == "claude" || tgt == "claude-code" {
        if let Some(claude_bin) = pa2pa_daemon::runners::find_in_path("claude") {
            let mut cmd = Command::new(&claude_bin);
            cmd.args(["mcp", "add", "pa2pa", "--", &exe]);
            cmd.args(&args_vec);
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    configured.push("Claude Code (via `claude mcp add`)".to_string());
                }
                _ => {
                    if let Some(home) = dirs::home_dir() {
                        let p = home.join(".claude.json");
                        merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec)?;
                        configured.push(format!("Claude Code ({})", p.display()));
                    }
                }
            }
        } else if !do_all {
            if let Some(home) = dirs::home_dir() {
                let p = home.join(".claude.json");
                merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec)?;
                configured.push(format!("Claude Code ({})", p.display()));
            }
        }
    }

    // 2. Codex (`~/.codex/config.toml`)
    if do_all || tgt == "codex" {
        if let Some(home) = dirs::home_dir() {
            let codex_dir = home.join(".codex");
            let has_codex =
                codex_dir.is_dir() || pa2pa_daemon::runners::find_in_path("codex").is_some();
            if has_codex || !do_all {
                let cfg_path = codex_dir.join("config.toml");
                merge_codex_toml(&cfg_path, &exe, &args_vec)?;
                configured.push(format!("Codex ({})", cfg_path.display()));
            }
        }
    }

    // 3. OpenCode (`~/.config/opencode/opencode.json`)
    if do_all || tgt == "opencode" {
        if let Some(home) = dirs::home_dir() {
            let oc_dir = home.join(".config").join("opencode");
            let has_oc =
                oc_dir.is_dir() || pa2pa_daemon::runners::find_in_path("opencode").is_some();
            if has_oc || !do_all {
                let p = oc_dir.join("opencode.json");
                merge_opencode_json(&p, &exe, &args_vec)?;
                configured.push(format!("OpenCode ({})", p.display()));
            }
        }
    }

    // 4. Cursor (`~/.cursor/mcp.json`)
    if do_all || tgt == "cursor" {
        if let Some(home) = dirs::home_dir() {
            let cursor_dir = home.join(".cursor");
            if cursor_dir.is_dir() || !do_all {
                let p = cursor_dir.join("mcp.json");
                merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec)?;
                configured.push(format!("Cursor ({})", p.display()));
            }
        }
    }

    // 5. Mistral Vibe (`~/.vibe/mcp.json`)
    if do_all || tgt == "vibe" || tgt == "mistral-vibe" {
        if let Some(home) = dirs::home_dir() {
            let vibe_dir = home.join(".vibe");
            let has_vibe =
                vibe_dir.is_dir() || pa2pa_daemon::runners::find_in_path("vibe").is_some();
            if has_vibe || !do_all {
                let p = vibe_dir.join("mcp.json");
                merge_mcp_servers_json(&p, "mcpServers", &exe, &args_vec)?;
                configured.push(format!("Mistral Vibe ({})", p.display()));
            }
        }
    }

    if configured.is_empty() {
        println!("No supported agent config directories were auto-detected. Run `pa2pa setup-mcp` (without --apply) to copy the generic MCP JSON snippet.");
    } else {
        println!("Configured pa2pa MCP server for:");
        for item in configured {
            println!("  [OK] {item}");
        }
        println!("\nRestart your AI agent(s) so they pick up the new `pa2pa` MCP tools.");
    }

    Ok(())
}

fn merge_mcp_servers_json(
    path: &PathBuf,
    root_key: &str,
    exe: &str,
    args: &[String],
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut doc: Value = if path.exists() {
        let raw = fs::read_to_string(path)?;
        let backup = path.with_extension("json.pa2pa.bak");
        let _ = fs::write(backup, &raw);
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !doc.is_object() {
        doc = json!({});
    }
    let map = doc.as_object_mut().unwrap();
    let servers = map
        .entry(root_key.to_string())
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers.as_object_mut().unwrap().insert(
        "pa2pa".to_string(),
        json!({
            "command": exe,
            "args": args
        }),
    );

    fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn merge_opencode_json(path: &PathBuf, exe: &str, args: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut doc: Value = if path.exists() {
        let raw = fs::read_to_string(path)?;
        let backup = path.with_extension("json.pa2pa.bak");
        let _ = fs::write(backup, &raw);
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !doc.is_object() {
        doc = json!({});
    }
    let map = doc.as_object_mut().unwrap();
    let mcp = map.entry("mcp".to_string()).or_insert_with(|| json!({}));
    if !mcp.is_object() {
        *mcp = json!({});
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

    fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn merge_codex_toml(path: &PathBuf, exe: &str, args: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut content = if path.exists() {
        let raw = fs::read_to_string(path)?;
        let backup = path.with_extension("toml.pa2pa.bak");
        let _ = fs::write(backup, &raw);
        raw
    } else {
        String::new()
    };

    let args_toml = args
        .iter()
        .map(|a| format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let exe_escaped = exe.replace('\\', "\\\\").replace('"', "\\\"");
    let block = format!(
        "\n# >>> pa2pa MCP >>>\n[mcp_servers.pa2pa]\ncommand = \"{exe_escaped}\"\nargs = [{args_toml}]\n# <<< pa2pa MCP <<<\n"
    );

    if let (Some(start), Some(end)) = (
        content.find("# >>> pa2pa MCP >>>"),
        content.find("# <<< pa2pa MCP <<<"),
    ) {
        let after_end = end + "# <<< pa2pa MCP <<<".len();
        content.replace_range(start..after_end, block.trim());
    } else if !content.contains("[mcp_servers.pa2pa]") {
        content.push_str(&block);
    }

    fs::write(path, content)?;
    Ok(())
}
