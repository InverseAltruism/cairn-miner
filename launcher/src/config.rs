//! Launcher settings: mining mode, which GPUs are selected, CPU intensity, and
//! identity (address/worker/pool). Persisted as the launcher's own TOML in the
//! per-user app dir — separate from the miner's `config.toml`, because the
//! launcher now drives several miner processes with per-device flags rather than
//! one config file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which compute the user wants to mine with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    GpuPlusCpu,
    GpuOnly,
    CpuOnly,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::GpuOnly
    }
}

impl Mode {
    pub fn uses_gpu(self) -> bool {
        matches!(self, Mode::GpuPlusCpu | Mode::GpuOnly)
    }
    pub fn uses_cpu(self) -> bool {
        matches!(self, Mode::GpuPlusCpu | Mode::CpuOnly)
    }
    pub fn label(self) -> &'static str {
        match self {
            Mode::GpuPlusCpu => "GPU + CPU",
            Mode::GpuOnly => "GPU only",
            Mode::CpuOnly => "CPU only",
        }
    }
    pub const ALL: [Mode; 3] = [Mode::GpuOnly, Mode::GpuPlusCpu, Mode::CpuOnly];
}

/// How hard the CPU worker runs (mapped to a thread count from the core count).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuIntensity {
    Light,
    Medium,
    Full,
}

impl Default for CpuIntensity {
    fn default() -> Self {
        CpuIntensity::Medium
    }
}

impl CpuIntensity {
    pub fn label(self) -> &'static str {
        match self {
            CpuIntensity::Light => "Light",
            CpuIntensity::Medium => "Medium",
            CpuIntensity::Full => "Full",
        }
    }
    pub const ALL: [CpuIntensity; 3] =
        [CpuIntensity::Light, CpuIntensity::Medium, CpuIntensity::Full];

    /// Threads to use given the machine's logical core count. Always leaves at
    /// least one core free (except tiny machines), and Full keeps 2 for the OS.
    pub fn threads(self, logical_cores: usize) -> usize {
        let c = logical_cores.max(1);
        match self {
            CpuIntensity::Light => (c / 4).max(1),
            CpuIntensity::Medium => (c / 2).max(1),
            CpuIntensity::Full => c.saturating_sub(2).max(1),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherConfig {
    pub address: String,
    pub worker: String,
    /// Endpoints in priority order; empty = the miner's built-in default pool.
    pub pools: Vec<String>,
    pub mode: Mode,
    /// Selected GPU identities as `"<backend>:<index>"` (e.g. `"cuda:0"`), so a
    /// choice survives restarts and reorderings.
    pub selected_gpus: Vec<String>,
    pub cpu_intensity: CpuIntensity,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            worker: String::new(),
            pools: Vec::new(),
            mode: Mode::default(),
            selected_gpus: Vec::new(),
            cpu_intensity: CpuIntensity::default(),
        }
    }
}

impl LauncherConfig {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)
            .unwrap_or_else(|_| String::new());
        std::fs::write(path, text)
    }
}

/// Per-user app directory: `%APPDATA%\cairn-miner` on Windows, else
/// `$XDG_CONFIG_HOME/cairn-miner` or `~/.config/cairn-miner`.
pub fn app_dir() -> PathBuf {
    platform_config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("cairn-miner")
}

pub fn config_path() -> PathBuf {
    app_dir().join("launcher.toml")
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
    fn cpu_intensity_thread_mapping() {
        assert_eq!(CpuIntensity::Light.threads(16), 4);
        assert_eq!(CpuIntensity::Medium.threads(16), 8);
        assert_eq!(CpuIntensity::Full.threads(16), 14);
        // Never zero, even on tiny machines.
        assert_eq!(CpuIntensity::Full.threads(1), 1);
        assert_eq!(CpuIntensity::Light.threads(2), 1);
    }

    #[test]
    fn mode_capabilities() {
        assert!(Mode::GpuPlusCpu.uses_gpu() && Mode::GpuPlusCpu.uses_cpu());
        assert!(Mode::GpuOnly.uses_gpu() && !Mode::GpuOnly.uses_cpu());
        assert!(!Mode::CpuOnly.uses_gpu() && Mode::CpuOnly.uses_cpu());
    }

    #[test]
    fn config_round_trips() {
        let mut c = LauncherConfig::default();
        c.address = "abc".into();
        c.mode = Mode::GpuPlusCpu;
        c.selected_gpus = vec!["cuda:0".into(), "opencl:1".into()];
        c.cpu_intensity = CpuIntensity::Full;
        let text = toml::to_string_pretty(&c).unwrap();
        let back: LauncherConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.address, "abc");
        assert_eq!(back.mode, Mode::GpuPlusCpu);
        assert_eq!(back.selected_gpus, vec!["cuda:0", "opencl:1"]);
        assert_eq!(back.cpu_intensity, CpuIntensity::Full);
    }
}
