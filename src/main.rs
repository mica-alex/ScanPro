// Hide the console window for the GUI on Windows release builds. The CLI
// re-attaches to the parent console (see `attach_console`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("scanpro=info")).init();
    scanpro::system::install_panic_log();
    // rayon aborts the process when a spawned task panics unless the pool
    // has a handler; the panic hook has already logged it, so just carry on.
    let _ = rayon::ThreadPoolBuilder::new().panic_handler(|_| {}).build_global();
    let args = cli::Cli::parse();
    match args.command {
        Some(command) => {
            attach_console();
            cli::run(command)
        }
        None => app::run(args.files),
    }
}

/// A `windows_subsystem = "windows"` binary has no console, so CLI output
/// would vanish; borrow the console of the shell that launched us.
fn attach_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
