use std::io::Write;
use std::path::PathBuf;

include!("src/icon_art.rs");

/// Builds a multi-size .ico (32-bit BMP entries) from the procedural icon.
fn write_ico(path: &PathBuf, sizes: &[u32]) {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in sizes {
        let rgba = render_icon_rgba(size);
        let mut bmp = Vec::new();
        // BITMAPINFOHEADER (height is doubled: XOR + AND masks)
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(size as i32).to_le_bytes());
        bmp.extend_from_slice(&((size * 2) as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&32u16.to_le_bytes()); // bpp
        bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bmp.extend_from_slice(&0u32.to_le_bytes()); // image size (may be 0)
        bmp.extend_from_slice(&[0u8; 16]); // ppm x/y, clr used/important
        // XOR bitmap: bottom-up BGRA
        for y in (0..size).rev() {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                bmp.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        // AND mask: 1 bpp, rows padded to 32 bits; all zero (alpha is used).
        let row_bytes = size.div_ceil(32) * 4;
        bmp.extend(std::iter::repeat_n(0u8, (row_bytes * size) as usize));
        images.push(bmp);
    }

    let mut f = std::fs::File::create(path).expect("create ico");
    f.write_all(&0u16.to_le_bytes()).unwrap(); // reserved
    f.write_all(&1u16.to_le_bytes()).unwrap(); // type: icon
    f.write_all(&(sizes.len() as u16).to_le_bytes()).unwrap();
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, img) in sizes.iter().zip(&images) {
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        f.write_all(&[dim, dim, 0, 0]).unwrap(); // w, h, colors, reserved
        f.write_all(&1u16.to_le_bytes()).unwrap(); // planes
        f.write_all(&32u16.to_le_bytes()).unwrap(); // bpp
        f.write_all(&(img.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&offset.to_le_bytes()).unwrap();
        offset += img.len() as u32;
    }
    for img in &images {
        f.write_all(img).unwrap();
    }
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    write_ico(&ico, &[16, 20, 24, 32, 40, 48, 64, 256]);

    // Copy next to the installer script so Inno Setup can use the same icon.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let installer_dir = manifest_dir.join("installer");
    if installer_dir.is_dir() {
        let _ = std::fs::copy(&ico, installer_dir.join("app.ico"));
    }

    // winresource есть в build-зависимостях только на Windows-хосте.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico.to_str().unwrap());
        res.set("FileDescription", "Preset Launcher");
        res.set("ProductName", "Preset Launcher");
        res.compile().expect("compile windows resources");
    }
}
