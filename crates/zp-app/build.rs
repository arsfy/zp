use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
