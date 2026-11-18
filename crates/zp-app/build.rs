use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=ZP_BUILD_COMMIT={commit}");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // AppKit reads this name for the application menu of an unbundled executable.
    let plist =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("app-name.plist");
    fs::write(
        &plist,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>ZP</string>
    <key>CFBundleDisplayName</key>
    <string>ZP</string>
</dict>
</plist>
"#,
    )
    .expect("write macOS application menu name");
    println!(
        "cargo:rustc-link-arg-bin=zp=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
