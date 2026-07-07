//! Self-contained miner: the launcher carries the all-backends `cairn-miner`
//! binary embedded in its own executable (injected by build.rs from
//! `CAIRN_MINER_BIN`). At runtime it prefers a sibling binary (handy for dev and
//! the zip bundle) and otherwise extracts the embedded copy into the per-user
//! app dir — so `cairn-miner-launcher(.exe)` is the only file a user needs.

use std::io;
use std::path::PathBuf;

/// Embedded miner bytes — empty in local dev builds without `CAIRN_MINER_BIN`.
static EMBEDDED_MINER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded-miner.bin"));

const MINER_STEM: &str = "cairn-miner";

/// Does this build carry an embedded miner?
pub fn has_embedded() -> bool {
    !EMBEDDED_MINER.is_empty()
}

/// A runnable path to the miner: a sibling binary if present, else the extracted
/// embedded copy. Errors only if there's neither.
pub fn ensure_miner() -> io::Result<PathBuf> {
    if let Some(p) = sibling_miner() {
        return Ok(p);
    }
    if has_embedded() {
        return extract_embedded();
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no {MINER_STEM} beside the launcher and none embedded in this build"),
    ))
}

fn miner_filename() -> String {
    format!("{MINER_STEM}{}", std::env::consts::EXE_SUFFIX)
}

fn sibling_miner() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join(miner_filename());
    cand.exists().then_some(cand)
}

/// Extract to `<app dir>/bin/cairn-miner-<version>(.exe)`, rewriting only when
/// missing or a different size (so restarts are cheap and an upgraded launcher
/// replaces the old miner).
fn extract_embedded() -> io::Result<PathBuf> {
    let dir = crate::config::app_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{MINER_STEM}-{}{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::EXE_SUFFIX
    ));

    let up_to_date = std::fs::metadata(&path)
        .map(|m| m.len() == EMBEDDED_MINER.len() as u64)
        .unwrap_or(false);
    if !up_to_date {
        let tmp = path.with_extension("downloading");
        std::fs::write(&tmp, EMBEDDED_MINER)?;
        set_executable(&tmp)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

#[cfg(unix)]
fn set_executable(p: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm)
}

#[cfg(not(unix))]
fn set_executable(_p: &std::path::Path) -> io::Result<()> {
    Ok(())
}
