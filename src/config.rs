//! On-disk state: `config.json` (registry) + `sessions/<name>.sqlite` (auth).
//!
//! The "unlimited accounts" promise lives here: the registry is a plain JSON
//! array, sessions are one small sqlite file each, and dormant accounts cost
//! zero RAM — they are only opened when you connect to them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::premium::PremiumToggles;
use crate::util;

/// Per-account bookkeeping. Session secrets live in the sqlite file, NOT here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountMeta {
    /// User-chosen handle, e.g. "work", "personal", "support-bot".
    pub name: String,
    /// Phone number used at login (as entered, e.g. "+6591234567").
    pub phone: Option<String>,
    /// Telegram user id, captured after a successful login.
    pub user_id: Option<i64>,
    /// Username at time of login (may drift; re-check with `whoami`).
    pub username: Option<String>,
    /// Unix seconds when the account was first registered in this client.
    pub created_unix: u64,
    /// Unix seconds of the last interactive login.
    pub last_login_unix: Option<u64>,
}

/// Root configuration document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Telegram API credentials from https://my.telegram.org/apps.
    /// Can also be supplied via MLXYSFTELE_API_ID / MLXYSFTELE_API_HASH.
    pub api_id: Option<i32>,
    pub api_hash: Option<String>,
    /// Registry of known accounts. Dormant entries cost ~100 bytes each.
    pub accounts: Vec<AccountMeta>,
    /// Local premium conveniences (see premium.rs / docs/LOCAL_PREMIUM.md).
    pub local_premium: PremiumToggles,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_id: None,
            api_hash: None,
            accounts: Vec::new(),
            local_premium: PremiumToggles::default(),
        }
    }
}

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.json")
    }

    /// Load config; a missing file is a fresh default, a corrupt file is an
    /// error (we never silently overwrite user state).
    pub fn load(data_dir: &Path) -> io::Result<Config> {
        let path = Self::path(data_dir);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomically persist config (write tmp, fsync-ish, rename).
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        fs::create_dir_all(data_dir)?;
        fs::create_dir_all(util::sessions_dir(data_dir))?;
        let path = Self::path(data_dir);
        let tmp = data_dir.join(".config.json.tmp");
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, text)?;
        util::secure_file(&tmp)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find_account(&self, name: &str) -> Option<&AccountMeta> {
        self.accounts.iter().find(|a| a.name == name)
    }

    pub fn find_account_mut(&mut self, name: &str) -> Option<&mut AccountMeta> {
        self.accounts.iter_mut().find(|a| a.name == name)
    }

    /// Insert or replace an account entry.
    pub fn upsert_account(&mut self, meta: AccountMeta) {
        match self.find_account_mut(&meta.name) {
            Some(existing) => *existing = meta,
            None => self.accounts.push(meta),
        }
    }

    /// Remove an account entry; returns the removed meta if present.
    pub fn remove_account(&mut self, name: &str) -> Option<AccountMeta> {
        let idx = self.accounts.iter().position(|a| a.name == name)?;
        Some(self.accounts.remove(idx))
    }

    /// Effective API credentials: environment wins over config file.
    pub fn effective_api_creds(&self) -> Option<(i32, String)> {
        let env_id = std::env::var("MLXYSFTELE_API_ID")
            .ok()
            .and_then(|v| v.parse::<i32>().ok());
        let env_hash = std::env::var("MLXYSFTELE_API_HASH").ok();
        match (env_id.or(self.api_id), env_hash.or_else(|| self.api_hash.clone())) {
            (Some(id), Some(hash)) if !hash.is_empty() => Some((id, hash)),
            _ => None,
        }
    }
}

/// Render the registry as a plain table for `accounts`.
pub fn render_account_table(cfg: &Config) -> String {
    if cfg.accounts.is_empty() {
        return "no accounts registered yet — use `mlxysftele login <name>`".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<16} {:>11} {:<20} {}\n",
        "NAME", "PHONE", "USER ID", "USERNAME", "LAST LOGIN"
    ));
    for a in &cfg.accounts {
        out.push_str(&format!(
            "{:<20} {:<16} {:>11} {:<20} {}\n",
            a.name,
            a.phone.as_deref().unwrap_or("—"),
            a.user_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| "—".to_string()),
            a.username.as_deref().map(|u| format!("@{u}")).unwrap_or_else(|| "—".to_string()),
            a.last_login_unix
                .map(util::human_ago)
                .unwrap_or_else(|| "never".to_string()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_account(name: &str) -> AccountMeta {
        AccountMeta {
            name: name.to_string(),
            phone: Some("+6581234567".to_string()),
            user_id: Some(42),
            username: Some("someone".to_string()),
            created_unix: 1_760_000_000,
            last_login_unix: Some(1_762_900_000),
        }
    }

    #[test]
    fn upsert_inserts_then_replaces() {
        let mut cfg = Config::default();
        cfg.upsert_account(sample_account("work"));
        cfg.upsert_account(sample_account("work"));
        assert_eq!(cfg.accounts.len(), 1);
        cfg.upsert_account(sample_account("personal"));
        assert_eq!(cfg.accounts.len(), 2);
    }

    #[test]
    fn remove_account_returns_removed_entry() {
        let mut cfg = Config::default();
        cfg.upsert_account(sample_account("a"));
        cfg.upsert_account(sample_account("b"));
        let removed = cfg.remove_account("a");
        assert_eq!(removed.map(|a| a.name).as_deref(), Some("a"));
        assert_eq!(cfg.accounts.len(), 1);
        assert!(cfg.remove_account("missing").is_none());
    }

    #[test]
    fn serde_roundtrip_preserves_registry() {
        let mut cfg = Config::default();
        cfg.api_id = Some(123456);
        cfg.api_hash = Some("deadbeef".to_string());
        cfg.accounts.push(sample_account("work"));
        cfg.local_premium.enabled = true;
        let text = serde_json::to_string_pretty(&cfg).expect("serialize");
        let back: Config = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.api_id, Some(123456));
        assert_eq!(back.accounts.len(), 1);
        assert_eq!(back.accounts[0].name, "work");
        assert!(back.local_premium.enabled);
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compat() {
        let cfg: Config =
            serde_json::from_str(r#"{"future_field": {"x": 1}, "accounts": []}"#)
                .expect("tolerant parse");
        assert!(cfg.accounts.is_empty());
        assert_eq!(cfg.api_id, None);
    }

    #[test]
    fn save_and_load_roundtrip_via_disk() {
        let dir = std::env::temp_dir().join(format!(
            "mlxysftele-test-{}-{}",
            std::process::id(),
            crate::util::now_unix()
        ));
        let mut cfg = Config::default();
        cfg.api_id = Some(777);
        cfg.accounts.push(sample_account("disk"));
        cfg.save(&dir).expect("save");
        let loaded = Config::load(&dir).expect("load");
        assert_eq!(loaded.api_id, Some(777));
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].name, "disk");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
