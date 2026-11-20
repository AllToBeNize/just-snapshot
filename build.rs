//! Compiles the executable's icon resource with the Windows SDK's rc.exe.
//! `snapshot.ps1` sets up the Visual Studio environment that puts it on PATH.

use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/snapshot.rc");
    println!("cargo:rerun-if-changed=assets/icon/snapshot.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let resource = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("snapshot.res");
    let compiled = Command::new("rc.exe")
        .args(["/nologo", "/fo"])
        .arg(&resource)
        .arg("assets/snapshot.rc")
        .status()
        .is_ok_and(|status| status.success());
    if !compiled {
        panic!("rc.exe could not compile assets/snapshot.rc. Build through snapshot.ps1, which sets up the Visual Studio environment.");
    }
    println!("cargo:rustc-link-arg-bins={}", resource.display());
}
