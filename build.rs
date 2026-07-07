//! Build script: embed the cairn logo as the miner executable's Windows icon.
//!
//! Generates a multi-size `.ico` from the logo PNG on every host (cheap, and
//! keeps it verifiable off Windows); only the `winresource` embed is
//! Windows-only. Keeps the console `cairn-miner.exe` branded in Explorer.

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let png = std::path::Path::new(&manifest).join("assets/cairn-logo.png");
    println!("cargo:rerun-if-changed={}", png.display());

    let out_dir = std::env::var("OUT_DIR").unwrap();
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
