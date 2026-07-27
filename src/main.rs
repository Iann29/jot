// Windows: link with the GUI subsystem so launching jot.exe from Explorer
// doesn't drag a black console window along for the life of the process.
// Must stay the first item in the crate — inner attributes precede `mod`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod canvas;
mod compare_editor;
mod config;
mod db;
mod export;
mod gif_recorder;
mod image_canvas;
mod maintenance;
mod markdown_tags;
mod note;
// The screen-region recorder is built on slurp + wf-recorder + hyprctl
// (wlroots/Hyprland only), so the whole module — UI included — is
// Linux-only. gif_recorder stays compiled everywhere: its portable half
// backs the compare editor's GIF export.
#[cfg(unix)]
mod recorder_window;
mod themes;
mod transcribe;
mod window;

use gtk::glib;
use tracing_subscriber::EnvFilter;

fn main() -> glib::ExitCode {
    init_tracing();

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
        // Under the Windows GUI subsystem the eprintln!s above write to a
        // null stderr handle and are silently dropped, so a crash would look
        // like the app simply blinking out. Point the user at the log we
        // just wrote. `panic = "abort"` means this runs exactly once, and
        // the hook always runs before the abort.
        #[cfg(windows)]
        message_box(
            "jot crashed",
            &format!("jot crashed — details in {}", log_path.display()),
        );
    }));

    app::run()
}

fn log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Linux: logs go to stdout, where a terminal launch or the compositor's
/// journal picks them up.
#[cfg(unix)]
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .init();
}

/// Windows: the GUI subsystem gives us a null stdout, and
/// tracing-subscriber discards write errors — every log line would vanish.
/// Append to `%LOCALAPPDATA%\jot\jot.log` instead; that file is the only
/// remote-debugging channel a Windows user has. If the file can't be
/// opened we still install the stdout subscriber so the app starts.
#[cfg(windows)]
fn init_tracing() {
    let file = dirs::data_local_dir().and_then(|dir| {
        let dir = dir.join("jot");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("jot.log"))
            .ok()
    });
    match file {
        // Mutex<File> is tracing-subscriber's MakeWriter for a plain file:
        // one lock per event keeps lines from interleaving across threads.
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(log_filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(log_filter())
            .init(),
    }
}

/// Modal Win32 message box. Used by the panic hook, so it must not
/// allocate anything that could panic again beyond the two UTF-16 buffers.
#[cfg(windows)]
fn message_box(title: &str, body: &str) {
    use std::iter::once;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let wide = |s: &str| s.encode_utf16().chain(once(0)).collect::<Vec<u16>>();
    let body = wide(body);
    let title = wide(title);
    // SAFETY: both buffers are NUL-terminated UTF-16 and outlive the call;
    // a null owner HWND makes the box application-modal, which is what we
    // want from a panic hook that has no window to parent to.
    let _ = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}
