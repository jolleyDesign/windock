//! Embeds the app icon in windock.exe. The icon is drawn by src/logo.rs and compiled into
//! a Windows resource with llvm-rc (when cross-compiling) or rc.exe (on Windows).

#[path = "src/logo.rs"]
mod logo;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    println!("cargo:rerun-if-env-changed=RC");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("windock.ico");
    std::fs::write(&ico, ico_file(&[16, 20, 24, 32, 40, 48, 64, 96, 256])).unwrap();
    let rc = out.join("windock.rc");
    let ico_path = ico.to_string_lossy().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{ico_path}\"\n")).unwrap();
    let res = out.join("windock.res");

    if compile(&rc, &res) {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=no resource compiler (llvm-rc or rc.exe) found; windock.exe will have no icon");
    }
}

fn compile(rc: &Path, res: &Path) -> bool {
    let llvm = |tool: String| (tool, vec!["/no-preprocess".to_string()]);
    let mut tools: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(t) = std::env::var("RC") {
        tools.push((t, vec![]));
    }
    tools.push(llvm("llvm-rc".into()));
    tools.extend((14..=22).rev().map(|v| llvm(format!("llvm-rc-{v}"))));
    tools.push(("rc.exe".into(), vec![]));
    tools.into_iter().any(|(tool, extra)| Command::new(&tool).args(&extra).arg("/fo").arg(res).arg(rc).status().is_ok_and(|s| s.success()))
}

/// An .ico with uncompressed 32-bit images at each size.
fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| ico_image(s)).collect();
    let mut f = Vec::new();
    f.extend(0u16.to_le_bytes()); // reserved
    f.extend(1u16.to_le_bytes()); // type: icon
    f.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 }; // 0 means 256
        f.extend([dim, dim, 0, 0]);
        f.extend(1u16.to_le_bytes()); // planes
        f.extend(32u16.to_le_bytes()); // bits per pixel
        f.extend((img.len() as u32).to_le_bytes());
        f.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    images.iter().for_each(|img| f.extend(img));
    f
}

/// BITMAPINFOHEADER + bottom-up BGRA + an all-zero AND mask (alpha does the masking).
fn ico_image(size: u32) -> Vec<u8> {
    let rgba = logo::rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut v = Vec::new();
    v.extend(40u32.to_le_bytes());
    v.extend((size as i32).to_le_bytes());
    v.extend((size as i32 * 2).to_le_bytes()); // colour + mask
    v.extend(1u16.to_le_bytes());
    v.extend(32u16.to_le_bytes());
    v.extend(0u32.to_le_bytes()); // BI_RGB
    v.extend((size * size * 4 + mask_row * size).to_le_bytes());
    v.extend([0u8; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            v.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    v.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    v
}
