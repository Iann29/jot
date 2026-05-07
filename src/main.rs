mod app;
mod config;
mod db;
mod image_canvas;
mod maintenance;
mod note;
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

    app::run()
}
