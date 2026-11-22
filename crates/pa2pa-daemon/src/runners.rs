use anyhow::{anyhow, bail, Context, Result};
use pa2pa_core::{Config, Contact, ExecutionMode, Pa2paPaths, RunnerKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub id: String,
    pub label: String,
    pub binary: String,
    pub installed: bool,
    pub path: Option<String>,
}

pub fn runner_binary(kind: &RunnerKind) -> Option<&'static str> {
    match kind {
        RunnerKind::Claude => Some("claude"),
        RunnerKind::Codex => Some("codex"),
        RunnerKind::Hermes => Some("hermes"),
        RunnerKind::Antigravity => Some("agy"),
        RunnerKind::T3 => Some("t3"),
        RunnerKind::Vibe => Some("vibe"),
        RunnerKind::Opencode => Some("opencode"),
        RunnerKind::Custom | RunnerKind::None => None,
    }
}

pub fn runner_label(kind: &RunnerKind) -> &'static str {
    match kind {
        RunnerKind::Claude => "Claude Code",
        RunnerKind::Codex => "Codex CLI",
        RunnerKind::Hermes => "Hermes Agent",
        RunnerKind::Antigravity => "Antigravity (agy)",
        RunnerKind::T3 => "T3 Code",
        RunnerKind::Vibe => "Mistral Vibe",
        RunnerKind::Opencode => "OpenCode",
        RunnerKind::Custom => "Custom Command",
        RunnerKind::None => "Disabled (Inbox Only)",
    }
}

pub fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn detect_installed_runners() -> Vec<RunnerInfo> {
    RunnerKind::all_builtins()
        .iter()
        .map(|kind| {
            let bin = runner_binary(kind).unwrap_or_default();
            let found = find_in_path(bin);
            RunnerInfo {
                id: kind.as_str().to_string(),
                label: runner_label(kind).to_string(),
                binary: bin.to_string(),
                installed: found.is_some(),
                path: found.map(|p| p.display().to_string()),
            }
        })
        .collect()
}

pub async fn run_agent_responder(
    paths: &Pa2paPaths,
    config: &Config,
    contact: &Contact,
    mode: &ExecutionMode,
    prompt: &str,
    thread_title: &str,
) -> Result<String> {
    let kind = contact
        .runner
        .clone()
        .unwrap_or_else(|| config.daemon.default_runner.clone());

    if kind == RunnerKind::None {
        bail!("Auto-responder runner is disabled (`none`)");
    }

    let safe_title: String = thread_title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect();
    let workdir = resolve_workdir(paths, config, contact)?;
    let model = config.daemon.runner_models.get(kind.as_str()).cloned();
    let timeout_dur = Duration::from_secs(config.daemon.runner_timeout_secs.max(15));

    let (raw_program, raw_args, pipe_stdin) = build_command(
        &kind,
        config,
        contact,
        mode,
        prompt,
        &safe_title,
        &Some(workdir.clone()),
        model.as_deref(),
    )?;

    let (program, args, sandboxed) = maybe_wrap_os_sandbox(
        config.daemon.os_sandbox && *mode == ExecutionMode::ReadOnly,
        &raw_program,
        raw_args,
    );

    tracing::info!(
        runner = %kind,
        program = %program,
        peer = %contact.alias,
        mode = ?mode,
        sandboxed,
        workdir = %workdir.display(),
        "Invoking headless agent runner"
    );

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if pipe_stdin {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to launch runner `{program}` — is it installed in PATH?"))?;

    if pipe_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }
    }

    let output = timeout(timeout_dur, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("Runner `{kind}` timed out after {}s", timeout_dur.as_secs()))??;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!(
            "Runner `{kind}` exited with status {}: {}",
            output.status,
            truncate(&detail, 400)
        );
    }

    if stdout.is_empty() {
        bail!("Runner `{kind}` returned empty stdout");
    }

    Ok(stdout)
}

/// Resolve a safe working directory for the headless agent runner.
/// Never falls back to the daemon's inherited CWD (which might be `$HOME` or a repo with `.env`):
/// if no `allowed_paths` or `default_workdir` is configured, isolates the runner in `<pa2pa_home>/sandbox`.
fn resolve_workdir(paths: &Pa2paPaths, config: &Config, contact: &Contact) -> Result<PathBuf> {
    if let Some(first_allowed) = contact.allowed_paths.first() {
        let p = Config::expand_path(first_allowed);
        if p.is_dir() {
            return Ok(p);
        }
    }
    if let Some(ref default_wd) = config.daemon.default_workdir {
        let p = Config::expand_path(default_wd);
        if p.is_dir() {
            return Ok(p);
        }
    }
    let sandbox = paths.sandbox_dir();
    std::fs::create_dir_all(&sandbox)?;
    Ok(sandbox)
}

#[allow(clippy::too_many_arguments)]
fn build_command(
    kind: &RunnerKind,
    config: &Config,
    contact: &Contact,
    mode: &ExecutionMode,
    prompt: &str,
    thread_title: &str,
    workdir: &Option<PathBuf>,
    model: Option<&str>,
) -> Result<(String, Vec<String>, bool)> {
    match kind {
        RunnerKind::Claude => {
            let mut args = vec!["-p".to_string()];
            if *mode == ExecutionMode::ReadOnly {
                args.push("--permission-mode".into());
                args.push("plan".into());
                args.push("--allowedTools".into());
                args.push("Read,Glob,Grep".into());
            }
            if let Some(m) = model {
                args.push("--model".into());
                args.push(m.into());
            }
            for p in &contact.allowed_paths {
                args.push("--add-dir".into());
                args.push(Config::expand_path(p).display().to_string());
            }
            Ok(("claude".into(), args, true))
        }
        RunnerKind::Codex => {
            let mut args = vec!["exec".to_string()];
            if *mode == ExecutionMode::ReadOnly {
                args.push("--sandbox".into());
                args.push("read-only".into());
            }
            if let Some(m) = model {
                args.push("-m".into());
                args.push(m.into());
            }
            if let Some(dir) = workdir {
                args.push("-C".into());
                args.push(dir.display().to_string());
            }
            args.push("-".into());
            Ok(("codex".into(), args, true))
        }
        RunnerKind::Hermes => {
            let mut args = Vec::new();
            if *mode == ExecutionMode::ScopedExec {
                args.push("chat".into());
                args.push("--yolo".into());
                if let Some(m) = model {
                    args.push("--model".into());
                    args.push(m.into());
                }
                args.push("-q".into());
                args.push(prompt.to_string());
            } else {
                args.push("-z".into());
                args.push(prompt.to_string());
            }
            Ok(("hermes".into(), args, false))
        }
        RunnerKind::Antigravity => {
            let mut args = vec![
                "-p".to_string(),
                prompt.to_string(),
                "--output-format".into(),
                "text".into(),
            ];
            if let Some(m) = model {
                args.push("--model".into());
                args.push(m.into());
            }
            Ok(("agy".into(), args, false))
        }
        RunnerKind::T3 => {
            let project = workdir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ".".to_string());
            let mut args = vec![
                "thread".to_string(),
                "start".into(),
                "--wait".into(),
                "--title".into(),
                format!("pa2pa: {thread_title}"),
            ];
            args.push(project);
            args.push("-".into());
            Ok(("t3".into(), args, true))
        }
        RunnerKind::Vibe => {
            let mut args = vec!["--prompt".to_string(), prompt.to_string()];
            if let Some(dir) = workdir {
                args.push("--workdir".into());
                args.push(dir.display().to_string());
            }
            if let Some(m) = model {
                args.push("--model".into());
                args.push(m.into());
            }
            if *mode == ExecutionMode::ScopedExec {
                args.push("--trust".into());
            }
            Ok(("vibe".into(), args, false))
        }
        RunnerKind::Opencode => {
            let mut args = vec!["run".to_string()];
            if let Some(m) = model {
                args.push("--model".into());
                args.push(m.into());
            }
            if *mode == ExecutionMode::ScopedExec {
                args.push("--auto".into());
            }
            args.push(prompt.to_string());
            Ok(("opencode".into(), args, false))
        }
        RunnerKind::Custom => {
            let custom = config
                .daemon
                .custom_runner
                .as_ref()
                .ok_or_else(|| anyhow!("`custom` runner selected but `[daemon.custom_runner]` is not configured"))?;
            let cwd_str = workdir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ".".into());
            let mode_str = match mode {
                ExecutionMode::ReadOnly => "read-only",
                ExecutionMode::ScopedExec => "scoped-exec",
            };
            let args = custom
                .args
                .iter()
                .map(|a| {
                    a.replace("{prompt}", prompt)
                        .replace("{cwd}", &cwd_str)
                        .replace("{mode}", mode_str)
                        .replace("{sender}", &contact.alias)
                })
                .collect();
            Ok((custom.command.clone(), args, custom.stdin_prompt))
        }
        RunnerKind::None => bail!("Runner is disabled"),
    }
}

fn maybe_wrap_os_sandbox(
    enable: bool,
    program: &str,
    args: Vec<String>,
) -> (String, Vec<String>, bool) {
    if !enable {
        return (program.to_string(), args, false);
    }

    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            // Seatbelt profile: allow normal execution/network/reads, deny reading sensitive credential
            // directories (~/.ssh, ~/.aws, ~/.gnupg) and deny writes outside /tmp & /private/tmp.
            let mut deny_rules = String::new();
            if let Some(home) = std::env::var_os("HOME") {
                let h = home.to_string_lossy().replace('"', "");
                for sensitive in [".ssh", ".aws", ".gnupg"] {
                    deny_rules.push_str(&format!(
                        "(deny file-read* (subpath \"{h}/{sensitive}\"))\n"
                    ));
                }
            }
            let profile = format!(
                "(version 1)\n\
                 (allow default)\n\
                 {deny_rules}\
                 (deny file-write* (subpath \"/\"))\n\
                 (allow file-write* (subpath \"/tmp\") (subpath \"/private/tmp\") (subpath \"/dev\"))\n"
            );
            let mut wrapped = vec!["-p".to_string(), profile, program.to_string()];
            wrapped.extend(args);
            return ("/usr/bin/sandbox-exec".to_string(), wrapped, true);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(bwrap) = find_in_path("bwrap") {
            let mut wrapped = vec![
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev".to_string(),
                "/dev".to_string(),
                "--proc".to_string(),
                "/proc".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
            ];
            if let Some(home) = std::env::var_os("HOME") {
                let ssh_dir = PathBuf::from(&home).join(".ssh");
                if ssh_dir.is_dir() {
                    wrapped.push("--tmpfs".to_string());
                    wrapped.push(ssh_dir.display().to_string());
                }
            }
            wrapped.push("--".to_string());
            wrapped.push(program.to_string());
            wrapped.extend(args);
            return (bwrap.display().to_string(), wrapped, true);
        }
    }

    (program.to_string(), args, false)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}
