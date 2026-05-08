mod app;
mod config;
mod db;
mod export;
mod image_canvas;
mod maintenance;
mod markdown_tags;
mod note;
mod themes;
mod transcribe;
mod window;

use gtk::glib;
use tracing_subscriber::EnvFilter;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Always print panic messages to stderr + a cache file — when jot is
    // launched via the Hyprland keybind its stderr can otherwise vanish.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let log_path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("jot-panic.log");
        let _ = std::fs::write(&log_path, format!("PANIC: {info}\n\nBacktrace:\n{bt}\n"));
        eprintln!("PANIC: {info}");
        eprintln!("Backtrace written to {}", log_path.display());
        eprintln!("{bt}");
    }));

    app::run()
}
