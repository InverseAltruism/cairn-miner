//! Device discovery: runs the miner's `devices --json` and parses the unified
//! GPU list + CPU core count that drives the launcher's pickers.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct Gpu {
    pub backend: String, // "cuda" | "opencl"
    pub index: usize,
    pub name: String,
}

impl Gpu {
    /// Stable identity used to persist a selection (`"cuda:0"`).
    pub fn key(&self) -> String {
        format!("{}:{}", self.backend, self.index)
    }
    /// Human label for the checkbox row.
    pub fn display(&self) -> String {
        format!("{}  ·  {}", self.name.trim(), self.backend.to_uppercase())
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Cpu {
    pub logical_cores: usize,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Devices {
    pub gpus: Vec<Gpu>,
    pub cpu: Cpu,
}

/// Run `<miner> devices --json` and parse it. On any failure returns a
/// CPU-only view (no GPUs) so the launcher still works.
pub fn probe(miner: &Path, log_dir: &Path) -> Devices {
    let mut cmd = Command::new(miner);
    cmd.arg("devices")
        .arg("--json")
        .arg("--log-dir")
        .arg(log_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut devices = match cmd.output() {
        Ok(out) => parse(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Devices::default(),
    };

    // Ensure a sane core count even if the probe failed or reported 0.
    if devices.cpu.logical_cores == 0 {
        devices.cpu.logical_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
    }
    devices
}

/// Parse the JSON line out of the miner's stdout (last `{...}` line).
fn parse(stdout: &str) -> Devices {
    stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .and_then(|l| serde_json::from_str::<Devices>(l).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_miner_json() {
        let s = "some log line\n{\"gpus\":[{\"backend\":\"cuda\",\"index\":0,\"name\":\"RTX 2080 SUPER\"}],\"cpu\":{\"logical_cores\":16}}\n";
        let d = parse(s);
        assert_eq!(d.gpus.len(), 1);
        assert_eq!(d.gpus[0].key(), "cuda:0");
        assert_eq!(d.gpus[0].backend, "cuda");
        assert_eq!(d.cpu.logical_cores, 16);
    }

    #[test]
    fn empty_on_garbage() {
        assert_eq!(parse("no json here").gpus.len(), 0);
        assert_eq!(parse("").cpu.logical_cores, 0);
    }
}
