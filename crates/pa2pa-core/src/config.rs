use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Pa2paPaths {
    pub home: PathBuf,
}

impl Pa2paPaths {
    pub fn default_home() -> Result<PathBuf> {
        let base = dirs::config_dir()
            .or_else(dirs::home_dir)
            .context("Could not determine home/config directory")?;
        Ok(base.join("pa2pa"))
    }

    pub fn resolve(explicit_home: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(h) = explicit_home {
            h
        } else if let Ok(env_home) = std::env::var("PA2PA_HOME") {
            PathBuf::from(env_home)
        } else {
            Self::default_home()?
        };
        Ok(Self { home })
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn contacts_dir(&self) -> PathBuf {
        self.home.join("contacts")
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join("pa2pa.db")
    }

    pub fn dedicated_key_path(&self) -> PathBuf {
        self.home.join("id_ed25519")
    }

    pub fn daemon_token_path(&self) -> PathBuf {
        self.home.join("daemon.token")
    }

    pub fn sandbox_dir(&self) -> PathBuf {
        self.home.join("sandbox")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.home)?;
        fs::create_dir_all(self.contacts_dir())?;
        fs::create_dir_all(self.sandbox_dir())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.home, fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerKind {
    Claude,
    Codex,
    Hermes,
    Antigravity,
    T3,
    Vibe,
    Opencode,
    Custom,
    None,
}

impl Default for RunnerKind {
    fn default() -> Self {
        Self::Claude
    }
}

impl RunnerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Hermes => "hermes",
            Self::Antigravity => "antigravity",
            Self::T3 => "t3",
            Self::Vibe => "vibe",
            Self::Opencode => "opencode",
            Self::Custom => "custom",
            Self::None => "none",
        }
    }

    pub fn all_builtins() -> &'static [RunnerKind] {
        &[
            Self::Claude,
            Self::Codex,
            Self::Hermes,
            Self::Antigravity,
            Self::T3,
            Self::Vibe,
            Self::Opencode,
        ]
    }
}

impl std::fmt::Display for RunnerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RunnerKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "hermes" => Ok(Self::Hermes),
            "antigravity" | "agy" => Ok(Self::Antigravity),
            "t3" | "t3-code" | "t3code" => Ok(Self::T3),
            "vibe" | "mistral-vibe" => Ok(Self::Vibe),
            "opencode" | "open-code" => Ok(Self::Opencode),
            "custom" => Ok(Self::Custom),
            "none" | "off" | "disabled" => Ok(Self::None),
            other => anyhow::bail!(
                "Unknown runner `{other}`. Valid runners: claude, codex, hermes, antigravity, t3, vibe, opencode, custom, none"
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomRunnerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_true")]
    pub stdin_prompt: bool,
}

fn default_true() -> bool {
    true
}

fn default_ui_bind() -> String {
    "127.0.0.1:4789".to_string()
}

fn default_timeout_secs() -> u64 {
    120
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_ui_bind")]
    pub ui_bind: String,
    #[serde(default = "default_true")]
    pub notifications: bool,
    #[serde(default)]
    pub default_runner: RunnerKind,
    #[serde(default)]
    pub default_workdir: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub runner_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub sync_allowlist_to_relay: bool,
    #[serde(default = "default_true")]
    pub os_sandbox: bool,
    #[serde(default)]
    pub custom_runner: Option<CustomRunnerConfig>,
    #[serde(default)]
    pub runner_models: HashMap<String, String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            ui_bind: default_ui_bind(),
            notifications: true,
            default_runner: RunnerKind::Claude,
            default_workdir: None,
            runner_timeout_secs: default_timeout_secs(),
            sync_allowlist_to_relay: true,
            os_sandbox: true,
            custom_runner: None,
            runner_models: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TransportMode {
    /// Direct P2P QUIC first (zero server needed), optional mailbox relay fallback, plus local outbox retry.
    Hybrid,
    /// Direct P2P QUIC + local outbox retry only (never touches a mailbox relay).
    P2pOnly,
    /// Mailbox relay only (disables local P2P QUIC endpoint).
    RelayOnly,
}

impl Default for TransportMode {
    fn default() -> Self {
        Self::Hybrid
    }
}

impl TransportMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::P2pOnly => "p2p-only",
            Self::RelayOnly => "relay-only",
        }
    }

    pub fn p2p_enabled(&self) -> bool {
        !matches!(self, Self::RelayOnly)
    }

    pub fn relay_enabled(&self) -> bool {
        !matches!(self, Self::P2pOnly)
    }
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TransportMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hybrid" | "auto" => Ok(Self::Hybrid),
            "p2p" | "p2p-only" | "direct" => Ok(Self::P2pOnly),
            "relay" | "relay-only" => Ok(Self::RelayOnly),
            other => anyhow::bail!("Unknown transport `{other}`. Valid: hybrid, p2p-only, relay-only"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub display_name: String,
    /// Optional WebSocket mailbox relay URL. Omit (`None`) for pure zero-server P2P mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub transport: TransportMode,
    pub key_path: PathBuf,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl Config {
    pub fn load(paths: &Pa2paPaths) -> Result<Self> {
        let p = paths.config_path();
        let raw = fs::read_to_string(&p).with_context(|| {
            format!(
                "pa2pa is not initialized at {} — run `pa2pa init` first",
                p.display()
            )
        })?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config {}", p.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, paths: &Pa2paPaths) -> Result<()> {
        paths.ensure_dirs()?;
        let body = toml::to_string_pretty(self)?;
        let cfg_path = paths.config_path();
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&cfg_path)?;
            f.write_all(body.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&cfg_path, body)?;
        }
        Ok(())
    }

    pub fn expand_path(raw: &str) -> PathBuf {
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        Path::new(raw).to_path_buf()
    }
}
