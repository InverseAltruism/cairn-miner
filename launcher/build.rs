//! Build script: turn the cairn logo PNG into a multi-size Windows `.ico` and,
//! when building for Windows, embed it as the executable icon (Explorer/taskbar).
//!
//! Icon generation runs on every host (cheap, and keeps it verifiable off
//! Windows); only the `winresource` embed step is Windows-only.

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    embed_miner(&out_dir);

    let png = std::path::Path::new(&manifest).join("../assets/cairn-logo.png");
    println!("cargo:rerun-if-changed={}", png.display());

    let ico = std::path::Path::new(&out_dir).join("cairn.ico");
    if let Err(e) = generate_ico(&png, &ico) {
        println!("cargo:warning=cairn icon generation skipped: {e}");
        return;
    }

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().unwrap());
        if let Err(e) = res.compile() {
            println!("cargo:warning=embedding cairn icon failed: {e}");
        }
    }
}

/// Embed the miner binary the launcher will run. CI sets `CAIRN_MINER_BIN` to a
/// freshly built all-backends `cairn-miner.exe`; we copy it into OUT_DIR so the
/// launcher can `include_bytes!` it and be a single self-contained download.
/// When the env var is unset (local dev), we write an empty placeholder and the
/// launcher falls back to a sibling / PATH `cairn-miner` at runtime.
fn embed_miner(out_dir: &str) {
    println!("cargo:rerun-if-env-changed=CAIRN_MINER_BIN");
    let dest = std::path::Path::new(out_dir).join("embedded-miner.bin");
    match std::env::var("CAIRN_MINER_BIN") {
        Ok(src) if !src.trim().is_empty() => {
            println!("cargo:rerun-if-changed={src}");
            if let Err(e) = std::fs::copy(&src, &dest) {
                println!("cargo:warning=could not embed miner from {src}: {e}; using empty placeholder");
                let _ = std::fs::write(&dest, b"");
            }
        }
        _ => {
            let _ = std::fs::write(&dest, b"");
        }
    }
}

fn generate_ico(
    png: &std::path::Path,
    out: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let img = image::open(png)?.to_rgba8();
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let resized =
            image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
        let icon_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        dir.add_entry(ico::IconDirEntry::encode(&icon_image)?);
    }
    let mut file = std::fs::File::create(out)?;
    dir.write(&mut file)?;
    Ok(())
}
