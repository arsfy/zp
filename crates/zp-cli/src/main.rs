//! Headless entry point for ZP, used to exercise the core crates without the GUI.
//!
//! Prompting and tool execution arrive in milestones M1–M3. See `PLAN.md`.

use anyhow::Result;

fn main() -> Result<()> {
    zp_core::logging::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => println!("{} {}", zp_core::APP_NAME, zp_core::VERSION),
        Some("paths") => {
            let data_dir = zp_core::paths::data_dir()?;
            println!("data:     {}", data_dir.display());
            println!(
                "settings: {}",
                zp_core::paths::settings_file(&data_dir).display()
            );
            println!(
                "state:    {}",
                zp_core::paths::state_file(&data_dir).display()
            );
            println!(
                "sessions: {}",
                zp_core::paths::sessions_dir(&data_dir).display()
            );
        }
        _ => {
            println!("{} CLI {}", zp_core::APP_NAME, zp_core::VERSION);
            println!();
            println!("Usage:");
            println!("  zp-cli --version   Print the version");
            println!("  zp-cli paths       Print data locations");
        }
    }
    Ok(())
}
