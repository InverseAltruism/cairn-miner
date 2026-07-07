//! Launcher settings, persisted as the miner's own `config.toml` so the file is
//! interchangeable between the launcher and a hand-run `cairn-miner --config`.
//!
//! Keys mirror `cairn-miner`'s `FileConfig` (snake_case; `pool` may be a single
//! string or a failover list). The launcher only *models* the common knobs it
//! exposes in the UI, but on save it preserves every other key already in the
//! file, so an advanced user's `blocks`/`nonces_per_thread`/etc. survive edits.

use std::path::{Path, PathBuf};

use toml::value::{Table, Value};

/// The subset of miner config the launcher UI edits, plus the untouched
/// remainder of the file for lossless round-tripping.
#[derive(Clone, Debug)]
pub struct LauncherConfig {
    pub address: String,
    pub worker: String,
    /// Endpoints in priority order; empty means "use the miner's built-in
    /// default" (the public cairn pool).
    pub pools: Vec<String>,
    pub backend: String,
    pub device: u64,
    pub cpu_threads: u64,
    pub reserve: u64,
    /// Every key from the loaded file, so save() doesn't drop unknown/advanced
    /// keys. The managed keys above are re-applied over this on save.
    raw: Table,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            worker: String::new(),
            pools: Vec::new(),
            backend: "auto".to_string(),
            device: 0,
            cpu_threads: 0,
            reserve: 4,
            raw: Table::new(),
        }
    }
}

impl LauncherConfig {
    /// Parse a config from TOML text, filling unset keys with defaults.
    pub fn from_toml(text: &str) -> Self {
        let raw: Table = toml::from_str(text).unwrap_or_default();
        let mut cfg = LauncherConfig {
            raw: raw.clone(),
            ..Default::default()
        };
        if let Some(s) = raw.get("address").and_then(Value::as_str) {
            cfg.address = s.to_string();
        }
        if let Some(s) = raw.get("worker").and_then(Value::as_str) {
            cfg.worker = s.to_string();
        }
        if let Some(s) = raw.get("backend").and_then(Value::as_str) {
            cfg.backend = s.to_string();
        }
        if let Some(n) = raw.get("device").and_then(Value::as_integer) {
            cfg.device = n.max(0) as u64;
        }
        if let Some(n) = raw.get("cpu_threads").and_then(Value::as_integer) {
            cfg.cpu_threads = n.max(0) as u64;
        }
        if let Some(n) = raw.get("reserve").and_then(Value::as_integer) {
            cfg.reserve = n.max(0) as u64;
        }
        cfg.pools = match raw.get("pool") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        cfg
    }

    /// Serialize to TOML, re-applying the managed keys over the preserved
    /// remainder of the original file.
    pub fn to_toml(&self) -> String {
        let mut t = self.raw.clone();
        set_or_remove_str(&mut t, "address", &self.address);
        set_or_remove_str(&mut t, "worker", &self.worker);
        set_or_remove_str(&mut t, "backend", &self.backend);
        t.insert("device".into(), Value::Integer(self.device as i64));
        t.insert("cpu_threads".into(), Value::Integer(self.cpu_threads as i64));
        t.insert("reserve".into(), Value::Integer(self.reserve as i64));

        let clean: Vec<String> = self
            .pools
            .iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        match clean.len() {
            0 => {
                t.remove("pool");
            }
            1 => {
                t.insert("pool".into(), Value::String(clean[0].clone()));
            }
            _ => {
                t.insert(
                    "pool".into(),
                    Value::Array(clean.into_iter().map(Value::String).collect()),
                );
            }
        }
        toml::to_string_pretty(&Value::Table(t))
            .unwrap_or_else(|_| String::new())
    }

    /// Load from `path`, or return defaults if it's missing/unreadable.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml(&s),
            Err(_) => Self::default(),
        }
    }

    /// Write to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_toml())
    }
}

fn set_or_remove_str(t: &mut Table, key: &str, val: &str) {
    let v = val.trim();
    if v.is_empty() {
        t.remove(key);
    } else {
        t.insert(key.into(), Value::String(v.to_string()));
    }
}

/// The config path the launcher owns and passes to the miner via `--config`:
/// `<platform config dir>/cairn-miner/config.toml`, matching the miner's own
/// platform-dir logic (Windows `%APPDATA%`, else `$XDG_CONFIG_HOME`/`~/.config`).
pub fn config_path() -> PathBuf {
    platform_config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cairn-miner")
        .join("config.toml")
}

fn platform_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            if !x.is_empty() {
                return Some(PathBuf::from(x));
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_managed_fields() {
        let cfg = LauncherConfig {
            address: "03ec5155c0153e5f95fabcc09b6a181465adceb4".into(),
            worker: "rig-01".into(),
            pools: vec!["cairn-pool.com:3333".into(), "backup:3333".into()],
            backend: "cuda".into(),
            device: 1,
            cpu_threads: 8,
            reserve: 2,
            raw: Table::new(),
        };
        let parsed = LauncherConfig::from_toml(&cfg.to_toml());
        assert_eq!(parsed.address, cfg.address);
        assert_eq!(parsed.worker, "rig-01");
        assert_eq!(parsed.pools, cfg.pools);
        assert_eq!(parsed.backend, "cuda");
        assert_eq!(parsed.device, 1);
        assert_eq!(parsed.cpu_threads, 8);
        assert_eq!(parsed.reserve, 2);
    }

    #[test]
    fn single_pool_writes_as_string_and_reads_back() {
        let mut cfg = LauncherConfig::default();
        cfg.pools = vec!["only:3333".into()];
        let text = cfg.to_toml();
        assert!(text.contains("pool = \"only:3333\""));
        assert_eq!(LauncherConfig::from_toml(&text).pools, vec!["only:3333"]);
    }

    #[test]
    fn preserves_unknown_advanced_keys() {
        let text = "blocks = 720\nnonces_per_thread = 8192\naddress = \"abc\"\n";
        let cfg = LauncherConfig::from_toml(text);
        let out = cfg.to_toml();
        assert!(out.contains("blocks = 720"), "advanced key dropped: {out}");
        assert!(out.contains("nonces_per_thread = 8192"));
    }

    #[test]
    fn empty_address_is_omitted_not_blank() {
        let cfg = LauncherConfig::default();
        let out = cfg.to_toml();
        assert!(!out.contains("address ="), "blank address should be omitted: {out}");
    }
}
