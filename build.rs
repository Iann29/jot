//! Build script — Windows resource embedding only.
//!
//! On Linux this is a no-op: everything jot needs at build time comes from
//! pkg-config through the gtk4-sys / libadwaita-sys build scripts. On Windows
//! a PE has no equivalent of a `.desktop` file — the icon that Explorer, the
//! taskbar and Alt+Tab draw has to be *linked into the exe* as an RT_ICON
//! resource. Without this, jot.exe ships with the generic "unknown app"
//! glyph even though the in-window icon (data/icons/jot.svg, via the icon
//! theme) is correct.
//!
//! The same resource carries the application manifest — see the notes on
//! `set_manifest` below for what is deliberately *not* in it.

fn main() {
    // Cheap and keeps incremental rebuilds honest: without an explicit
    // rerun-if-changed, cargo re-runs the script whenever *any* tracked file
    // changes, which on a build script that shells out to windres is wasteful.
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    embed_windows_resources();
}

/// Link the .ico and the application manifest into jot.exe.
///
/// The `cfg(windows)` gate is about the **host**: build scripts are always
/// compiled and run for the host, never for the target. So this is still
/// reachable when a Windows box cross-compiles to something else, and
/// `CARGO_CFG_TARGET_OS` — the target-side truth — is the real guard.
/// (The winresource dependency is declared the same way, under
/// `[target.'cfg(windows)'.build-dependencies]`, so the two stay in step.)
#[cfg(windows)]
fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=data/icons/jot.ico");

    let mut res = winresource::WindowsResource::new();

    // Relative to CARGO_MANIFEST_DIR, which is this script's working
    // directory. 7 sizes from 16 to 256 so Explorer's icon views, the
    // taskbar and the Alt+Tab switcher each pick a native resolution.
    res.set_icon("data/icons/jot.ico");

    // Minimal by design. Two things are deliberately absent:
    //
    //   * `<dpiAware>` / `<dpiAwareness>` — GDK's win32 backend calls
    //     SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) during
    //     gdk_init(). That API *fails* if the manifest already declared an
    //     awareness level, and GTK would then be stuck with whatever the
    //     manifest picked. Staying silent here lets GTK win.
    //   * `<compatibility><supportedOS>` — only affects version-lie shims
    //     that nothing in this stack reads.
    //
    // longPathAware is the one thing worth opting into: notes, images and
    // backups live under %LOCALAPPDATA%\jot, and a deep user profile plus a
    // long pasted-image filename can cross MAX_PATH. (It only takes effect
    // when the machine also has the LongPathsEnabled policy on, so it is a
    // strict improvement, never a regression.)
    res.set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="com.amageweb.Jot" version="1.0.0.0"/>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings xmlns:ws2016="http://schemas.microsoft.com/SMI/2016/WindowsSettings">
      <ws2016:longPathAware>true</ws2016:longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#,
    );

    // Fail loudly. This only ever runs on a Windows target, where a missing
    // windres/ar (mingw-w64-ucrt-x86_64-binutils) is a broken toolchain and
    // silently shipping an icon-less exe would be worse than a red build.
    res.compile().expect(
        "winresource: failed to compile the Windows resource (needs windres from binutils)",
    );
}
