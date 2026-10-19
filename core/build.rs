//! Compiles the HLSL kernels to DXIL with the Windows SDK's dxc, so the engine embeds them and
//! needs no shader compiler at run time. Set DXC to use a specific dxc.exe.

use std::path::{Path, PathBuf};
use std::process::Command;

const SHADERS: &[&str] = &["demosaic", "downsample", "develop", "tonesmall", "toneguide", "tonestats", "atrousblur", "atrousv", "atroushrink", "sharpblur", "sharpen"];

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dxc = find_dxc().expect("dxc.exe not found: install the Windows SDK or set DXC");
    println!("cargo:rerun-if-env-changed=DXC");
    println!("cargo:rerun-if-changed=shaders/common.hlsli");
    println!("cargo:rerun-if-changed=shaders/tone.hlsli");
    println!("cargo:rerun-if-changed=shaders/lens.hlsli");
    println!("cargo:rerun-if-changed=shaders/detail.hlsli");
    for name in SHADERS {
        let src = format!("shaders/{name}.hlsl");
        println!("cargo:rerun-if-changed={src}");
        let dst = out.join(format!("{name}.dxil"));
        let status = Command::new(&dxc)
            .args(["-nologo", "-T", "cs_6_0", "-E", "main", "-O3", "-Ishaders", "-Fo"])
            .arg(&dst)
            .arg(&src)
            .status()
            .expect("failed to run dxc");
        assert!(status.success(), "dxc failed on {src}");
    }
    // The develop pass again with the masks compiled in. It runs only when a photo has masks, so
    // the pass without them is exactly what it was (the mask code would cost it registers).
    for (out_name, flags) in [
        ("developmasks.dxil", &["-DMASKS"][..]),
        ("developprofile.dxil", &["-DPROFILE"][..]),
        ("developprofilemasks.dxil", &["-DPROFILE", "-DMASKS"][..]),
    ] {
        let dst = out.join(out_name);
        let status = Command::new(&dxc)
            .args(["-nologo", "-T", "cs_6_0", "-E", "main", "-O3", "-Ishaders"])
            .args(flags)
            .arg("-Fo")
            .arg(&dst)
            .arg("shaders/develop.hlsl")
            .status()
            .expect("failed to run dxc");
        assert!(status.success(), "dxc failed on {out_name}");
    }
}

/// The newest dxc.exe in the Windows SDK, which also signs the DXIL (dxil.dll sits beside it).
fn find_dxc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("DXC") {
        return Some(PathBuf::from(path));
    }
    let bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(r"x64\dxc.exe").exists())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join(r"x64\dxc.exe"))
}
