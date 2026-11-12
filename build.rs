//! Build script for embedding the Windows application icon into `RustEd.exe`.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path_var) = env::var("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let arch_dir = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    for root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join(arch_dir).join("rc.exe"))
            .filter(|p| p.is_file())
            .collect();
        versions.sort();
        if let Some(latest) = versions.pop() {
            return Some(latest);
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=assets/rusted.rc");
    println!("cargo:rerun-if-changed=assets/rusted.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let rc_path = Path::new("assets/rusted.rc");
    if !rc_path.is_file() {
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; RustEd.exe will not embed an .ico resource");
        return;
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR set by Cargo"));
    let res_path = out_dir.join("rusted.res");

    let status = Command::new(rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg(rc_path)
        .status();

    match status {
        Ok(s) if s.success() && res_path.is_file() => {
            println!("cargo:rustc-link-arg-bin=RustEd={}", res_path.display());
        }
        Ok(s) => {
            println!("cargo:warning=rc.exe exited with {s}; skipping Windows icon resource");
        }
        Err(err) => {
            println!("cargo:warning=failed to run rc.exe ({err}); skipping Windows icon resource");
        }
    }
}
