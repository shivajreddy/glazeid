fn main() {
    // Only applies to Windows targets.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    #[cfg(target_os = "windows")]
    windows_resources();
}

#[cfg(target_os = "windows")]
fn windows_resources() {
    use std::path::PathBuf;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("glazeid.ico");

    // Generate a multi-resolution .ico from the bundled PNG at build time.
    generate_ico(&ico_path);

    // Embed the icon into the PE via a Windows resource script.
    let mut res = winres::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());

    // Suppress the console window in release builds only.
    // Debug builds keep the console so `cargo run` shows output and Ctrl+C works.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rustc-link-arg=/SUBSYSTEM:WINDOWS");
        println!("cargo:rustc-link-arg=/ENTRY:mainCRTStartup");
    }

    if let Err(e) = res.compile() {
        // Non-fatal: icon just won't appear in Task Manager.
        eprintln!("winres failed (icon won't be embedded): {e}");
    }
}

#[cfg(target_os = "windows")]
fn generate_ico(out: &std::path::PathBuf) {
    const LOGO: &[u8] = include_bytes!("resources/glazeid.png");
    const SIZES: &[u32] = &[16, 32, 256];

    let src = image::load_from_memory(LOGO)
        .expect("valid logo PNG")
        .into_rgba8();

    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in SIZES {
        let resized =
            image::imageops::resize(&src, size, size, image::imageops::FilterType::Lanczos3);
        let mut png_bytes: Vec<u8> = Vec::new();
        resized
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .expect("encode resized icon");
        images.push(png_bytes);
    }

    write_ico(out, &images, SIZES);
}

#[cfg(target_os = "windows")]
fn write_ico(path: &std::path::PathBuf, images: &[Vec<u8>], sizes: &[u32]) {
    use std::io::Write;
    let count = images.len() as u16;
    let dir_size = 6 + count as usize * 16;

    let mut offsets: Vec<u32> = Vec::new();
    let mut offset = dir_size as u32;
    for img in images {
        offsets.push(offset);
        offset += img.len() as u32;
    }

    let mut f = std::fs::File::create(path).expect("create .ico");

    // File header: reserved(2) + type=1(2) + image count(2)
    f.write_all(&[0u8, 0, 1, 0]).unwrap();
    f.write_all(&count.to_le_bytes()).unwrap();

    // ICONDIRENTRY × count
    for (i, img) in images.iter().enumerate() {
        let w = if sizes[i] >= 256 { 0u8 } else { sizes[i] as u8 };
        let h = if sizes[i] >= 256 { 0u8 } else { sizes[i] as u8 };
        f.write_all(&[w, h, 0u8, 0u8]).unwrap(); // width, height, colorCount, reserved
        f.write_all(&1u16.to_le_bytes()).unwrap(); // planes
        f.write_all(&32u16.to_le_bytes()).unwrap(); // bitCount
        f.write_all(&(img.len() as u32).to_le_bytes()).unwrap(); // bytesInRes
        f.write_all(&offsets[i].to_le_bytes()).unwrap(); // imageOffset
    }

    for img in images {
        f.write_all(img).unwrap();
    }
}
