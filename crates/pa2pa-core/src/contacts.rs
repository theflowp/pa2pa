use crate::config::{Pa2paPaths, RunnerKind};
use anyhow::{bail, Context, Result};
use pa2pa_crypto::PeerPublicKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    Muted,
    Ask,
    AutoRead,
    AutoExec,
}

impl Default for TrustLevel {
    fn default() -> Self {
        Self::Ask
    }
}

impl TrustLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Muted => "muted",
            Self::Ask => "ask",
            Self::AutoRead => "auto-read",
            Self::AutoExec => "auto-exec",
        }
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "muted" | "deny" | "blocked" => Ok(Self::Muted),
            "ask" | "manual" => Ok(Self::Ask),
            "auto-read" | "read" | "readonly" | "read-only" => Ok(Self::AutoRead),
            "auto-exec" | "exec" | "auto" => Ok(Self::AutoExec),
            other => bail!("Invalid trust level `{other}`. Use: muted, ask, auto-read, auto-exec"),
        }
    }
}

fn default_max_auto_turns() -> u32 {
    4
}

fn default_require_approval_for() -> Vec<String> {
    vec![
        "write".into(),
        "shell".into(),
        "git_push".into(),
        "secrets".into(),
    ]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ContactFrontmatter {
    alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    ssh_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay: Option<String>,
    #[serde(default)]
    trust: TrustLevel,
    #[serde(default)]
    allowed_paths: Vec<String>,
    #[serde(default = "default_require_approval_for")]
    require_approval_for: Vec<String>,
    #[serde(default = "default_max_auto_turns")]
    max_auto_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runner: Option<RunnerKind>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contact {
    pub alias: String,
    pub display_name: Option<String>,
    pub ssh_key: String,
    pub fingerprint: String,
    pub relay: Option<String>,
    pub trust: TrustLevel,
    pub allowed_paths: Vec<String>,
    pub require_approval_for: Vec<String>,
    pub max_auto_turns: u32,
    pub runner: Option<RunnerKind>,
    pub prompt: String,
}

impl Contact {
    pub fn new(
        alias: impl Into<String>,
        ssh_key: impl Into<String>,
        display_name: Option<String>,
        relay: Option<String>,
        trust: TrustLevel,
        prompt: impl Into<String>,
    ) -> Result<Self> {
        let alias = sanitize_alias(&alias.into())?;
        let key_str = ssh_key.into();
        let peer = PeerPublicKey::from_openssh(&key_str)
            .with_context(|| format!("Invalid Ed25519 SSH public key for contact `{alias}`"))?;
        Ok(Self {
            alias,
            display_name,
            ssh_key: peer.openssh_public_key().to_string(),
            fingerprint: peer.fingerprint().to_string(),
            relay,
            trust,
            allowed_paths: Vec::new(),
            require_approval_for: default_require_approval_for(),
            max_auto_turns: default_max_auto_turns(),
            runner: None,
            prompt: prompt.into(),
        })
    }

    pub fn peer_public_key(&self) -> Result<PeerPublicKey> {
        Ok(PeerPublicKey::from_openssh(&self.ssh_key)?)
    }

    pub fn to_markdown(&self) -> Result<String> {
        let fm = ContactFrontmatter {
            alias: self.alias.clone(),
            display_name: self.display_name.clone(),
            ssh_key: self.ssh_key.clone(),
            relay: self.relay.clone(),
            trust: self.trust.clone(),
            allowed_paths: self.allowed_paths.clone(),
            require_approval_for: self.require_approval_for.clone(),
            max_auto_turns: self.max_auto_turns,
            runner: self.runner.clone(),
        };
        let yaml = serde_yaml::to_string(&fm)?;
        let body = self.prompt.trim();
        Ok(format!("---\n{yaml}---\n\n{body}\n"))
    }

    pub fn from_markdown(raw: &str) -> Result<Self> {
        let trimmed = raw.trim_start();
        let rest = trimmed
            .strip_prefix("---")
            .context("Contact file must start with YAML frontmatter `---`")?;
        let rest = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n')).unwrap_or(rest);
        let end_idx = rest
            .find("\n---")
            .context("Contact file missing closing YAML frontmatter `---`")?;
        let yaml_part = &rest[..end_idx];
        let body_start = end_idx + 4;
        let body = rest[body_start..]
            .trim_start_matches(['\r', '\n'])
            .trim()
            .to_string();

        let fm: ContactFrontmatter =
            serde_yaml::from_str(yaml_part).context("Invalid YAML frontmatter in contact file")?;
        let alias = sanitize_alias(&fm.alias)?;
        let peer = PeerPublicKey::from_openssh(&fm.ssh_key)
            .with_context(|| format!("Invalid SSH key in contact `{alias}`"))?;

        Ok(Self {
            alias,
            display_name: fm.display_name,
            ssh_key: peer.openssh_public_key().to_string(),
            fingerprint: peer.fingerprint().to_string(),
            relay: fm.relay,
            trust: fm.trust,
            allowed_paths: fm.allowed_paths,
            require_approval_for: fm.require_approval_for,
            max_auto_turns: fm.max_auto_turns,
            runner: fm.runner,
            prompt: body,
        })
    }
}

pub fn sanitize_alias(alias: &str) -> Result<String> {
    let cleaned = alias.trim().to_ascii_lowercase();
    if cleaned.is_empty() {
        bail!("Contact alias cannot be empty");
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Contact alias `{cleaned}` may only contain letters, digits, `-`, and `_`");
    }
    Ok(cleaned)
}

#[derive(Clone, Debug)]
pub struct ContactStore {
    dir: PathBuf,
}

impl ContactStore {
    pub fn new(paths: &Pa2paPaths) -> Self {
        Self {
            dir: paths.contacts_dir(),
        }
    }

    pub fn file_path(&self, alias: &str) -> Result<PathBuf> {
        let alias = sanitize_alias(alias)?;
        Ok(self.dir.join(format!("{alias}.md")))
    }

    pub fn save(&self, contact: &Contact) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let path = self.file_path(&contact.alias)?;
        // Validate SSH key before writing
        let peer = contact.peer_public_key()?;
        // Prevent ambiguous duplicate SSH fingerprints under two different aliases
        for existing in self.list()? {
            if existing.alias != contact.alias && existing.fingerprint == peer.fingerprint() {
                bail!(
                    "SSH key {} is already registered under contact `@{}`",
                    peer.fingerprint(),
                    existing.alias
                );
            }
        }
        fs::write(&path, contact.to_markdown()?)?;
        Ok(path)
    }

    pub fn get(&self, alias: &str) -> Result<Option<Contact>> {
        let path = self.file_path(alias)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        Ok(Some(Contact::from_markdown(&raw)?))
    }

    pub fn delete(&self, alias: &str) -> Result<bool> {
        let path = self.file_path(alias)?;
        if path.exists() {
            fs::remove_file(path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn list(&self) -> Result<Vec<Contact>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                let raw = fs::read_to_string(&path)?;
                match Contact::from_markdown(&raw) {
                    Ok(c) => out.push(c),
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "Skipping invalid contact file"),
                }
            }
        }
        out.sort_by(|a, b| a.alias.cmp(&b.alias));
        Ok(out)
    }

    pub fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Contact>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|c| c.fingerprint == fingerprint))
    }

    pub fn find_by_ed25519_bytes(&self, pub_bytes: &[u8; 32]) -> Result<Option<Contact>> {
        for c in self.list()? {
            if let Ok(pk) = c.peer_public_key() {
                if &pk.ed25519_public_bytes() == pub_bytes {
                    return Ok(Some(c));
                }
            }
        }
        Ok(None)
    }

    /// Returns fingerprints of all non-muted contacts (for optional relay-edge allowlist).
    pub fn allowed_fingerprints(&self) -> Result<Vec<String>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|c| c.trust != TrustLevel::Muted)
            .map(|c| c.fingerprint)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa2pa_crypto::Identity;

    #[test]
    fn contact_markdown_roundtrip() {
        let id = Identity::generate("benjamin").unwrap();
        let mut c = Contact::new(
            "benjamin",
            id.openssh_public_key(),
            Some("Benjamin (Lead)".into()),
            Some("wss://relay.example.com/ws".into()),
            TrustLevel::AutoRead,
            "Collègue de confiance.\nNe partage pas les secrets.",
        )
        .unwrap();
        c.allowed_paths.push("~/projects/app".into());

        let md = c.to_markdown().unwrap();
        let parsed = Contact::from_markdown(&md).unwrap();
        assert_eq!(parsed.alias, "benjamin");
        assert_eq!(parsed.fingerprint, id.fingerprint());
        assert_eq!(parsed.trust, TrustLevel::AutoRead);
        assert_eq!(parsed.allowed_paths, vec!["~/projects/app"]);
        assert!(parsed.prompt.contains("Collègue de confiance."));
    }
}
