#!/usr/bin/env bash
#
# scripts/bundle-windows.sh
#
#   Build a RELOCATABLE GTK4 + libadwaita bundle for jot and zip it as
#   dist/jot-<version>-x86_64-windows.zip
#
#   MUST be run from the MSYS2 **UCRT64** shell, after
#       cargo build --release --locked
#
#   Usage:  ./scripts/bundle-windows.sh v1.7.0
#           ./scripts/bundle-windows.sh          # version from Cargo.toml
#
# ─── Layout produced (flat-root, so the user double-clicks jot.exe) ──────
#
#   jot-v1.7.0-x86_64-windows/
#     jot.exe
#     *.dll                                   ← every transitive UCRT64 dep
#     gspawn-win64-helper.exe
#     gspawn-win64-helper-console.exe
#     gdbus.exe                               ← GLib's win32 session bus
#     lib/gdk-pixbuf-2.0/2.10.0/loaders/*.dll
#     lib/gdk-pixbuf-2.0/2.10.0/loaders.cache ← RELATIVE paths
#     etc/gtk-4.0/settings.ini
#     etc/fonts/…                             ← if fontconfig is in the tree
#     share/glib-2.0/schemas/gschemas.compiled
#     share/icons/Adwaita/…
#     share/icons/hicolor/…
#     share/locale/…                          ← optional, see TRIM_LOCALES
#     README.md  LICENSE  README-WINDOWS.txt
#
#   Why flat-root works: every GLib/GTK/GdkPixbuf path lookup on Windows
#   goes through g_win32_get_package_installation_directory_of_module(),
#   which takes the directory of the calling DLL and strips a trailing
#   "bin" or "lib" component. With the DLLs sitting next to jot.exe in
#   <root>/, the computed prefix IS <root>/, so <root>/share, <root>/lib
#   and <root>/etc all resolve. No env vars, no absolute paths, no
#   registry. (A bin/ subdir layout works identically — it just makes the
#   user hunt for the exe.)
#
set -euo pipefail
shopt -s nullglob

# ── 0. Environment ──────────────────────────────────────────────────────
PREFIX="${MINGW_PREFIX:-/ucrt64}"
BIN="$PREFIX/bin"
LIB="$PREFIX/lib"
SHARE="$PREFIX/share"
ETCDIR="$PREFIX/etc"

[[ -d "$BIN" ]] || { echo "!! $BIN not found — run this from the MSYS2 UCRT64 shell" >&2; exit 1; }
[[ "$(uname -m)" == "x86_64" ]] || echo ":: warning: expected x86_64, got $(uname -m)" >&2

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="v$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
fi

# CARGO_TARGET_DIR is a *Windows* path when set by the workflow.
TARGET_DIR_RAW="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
TARGET_DIR="$(cygpath -u "$TARGET_DIR_RAW" 2>/dev/null || printf '%s' "$TARGET_DIR_RAW")"
EXE="$TARGET_DIR/release/jot.exe"
[[ -f "$EXE" ]] || { echo "!! $EXE not found — run 'cargo build --release --locked' first" >&2; exit 1; }

PKG="jot-${VERSION}-x86_64-windows"
DIST="$REPO_ROOT/dist"
OUT="$DIST/$PKG"
rm -rf "$OUT"
mkdir -p "$OUT"

# Set TRIM_LOCALES=1 to drop share/locale (~40 MB of GTK translations).
TRIM_LOCALES="${TRIM_LOCALES:-0}"
# Set WITH_EMOJI=1 to ship the non-English emoji-chooser gresources.
WITH_EMOJI="${WITH_EMOJI:-0}"

echo "==> bundling $PKG"
echo "    prefix : $PREFIX"
echo "    exe    : $EXE"
echo "    out    : $OUT"

# ── 1. The executable ───────────────────────────────────────────────────
cp -f "$EXE" "$OUT/jot.exe"

# ── 2. Transitive DLL closure ───────────────────────────────────────────
#
# objdump -p reads the PE import table directly. Preferred over MSYS2's
# `ldd` (a Cygwin tool that mis-resolves mingw binaries) and over ntldd
# (extra package, and it resolves via the *loader*, i.e. against the
# current PATH). objdump still works on `strip = true` binaries — the
# import table is never stripped.
#
# Two rules keep this honest:
#   * only files that exist under $BIN are copied, so System32 DLLs
#     (kernel32, user32, ucrtbase, …) are excluded automatically;
#   * ucrtbase.dll must NEVER be bundled — it is an OS component and
#     shipping it breaks on machines with a newer UCRT.
#
declare -A SEEN=()

pe_imports() {
  objdump -p "$1" 2>/dev/null | sed -n 's/^[[:space:]]*DLL Name:[[:space:]]*//p'
}

walk() {
  local file="$1" dll key cand
  while read -r dll; do
    [[ -n "$dll" ]] || continue
    key="${dll,,}"
    [[ -n "${SEEN[$key]+x}" ]] && continue
    cand="$BIN/$dll"                 # NTFS is case-insensitive: this is enough
    if [[ -f "$cand" ]]; then
      SEEN[$key]="$cand"             # mark BEFORE recursing (cycles exist)
      cp -f "$cand" "$OUT/"
      walk "$cand"
    else
      SEEN[$key]=""                  # system DLL — remember so we stop asking
    fi
  done < <(pe_imports "$file")
}

echo "==> walking DLL imports (seed: jot.exe)"
walk "$OUT/jot.exe"

# ── 3. gdk-pixbuf loaders ───────────────────────────────────────────────
#
# These are dlopen()ed at runtime, so step 2 can never find them.
# librsvg's SVG loader (`pixbufloader_svg.dll` on modern librsvg, the
# older spelling was `libpixbufloader-svg.dll`) is MANDATORY here: every
# Adwaita *-symbolic icon jot uses (document-new-symbolic,
# media-record-symbolic, object-select-symbolic, pan-down-symbolic, …)
# is an SVG, and GtkIconTheme renders SVGs through gdk-pixbuf.
# Without it every button in jot shows the "broken image" glyph.
#
PB_VER="2.10.0"
PB_SRC="$LIB/gdk-pixbuf-2.0/$PB_VER/loaders"
PB_DST="$OUT/lib/gdk-pixbuf-2.0/$PB_VER/loaders"
[[ -d "$PB_SRC" ]] || { echo "!! $PB_SRC missing — install mingw-w64-ucrt-x86_64-gdk-pixbuf2" >&2; exit 1; }
mkdir -p "$PB_DST"
cp -f "$PB_SRC"/*.dll "$PB_DST/"
# NAME TRAP: librsvg >= 2.5x is built with cargo and installs its loader as
# `pixbufloader_svg.dll` (underscore, no lib prefix) — NOT the historic
# `libpixbufloader-svg.dll`. Match loosely so either spelling passes.
compgen -G "$PB_DST/*svg*.dll" > /dev/null \
  || { echo "!! no SVG pixbuf loader in $PB_SRC — install mingw-w64-ucrt-x86_64-librsvg" >&2; exit 1; }

echo "==> walking DLL imports (seed: pixbuf loaders — pulls librsvg's tree)"
for l in "$PB_DST"/*.dll; do walk "$l"; done

# ── 4. GLib helper executables ──────────────────────────────────────────
#
# gspawn-win64-helper*.exe — g_spawn_async()/GSubprocess exec these on
# Windows; GLib finds them next to libglib. jot needs them for every
# ffmpeg/ffprobe invocation in the compare editor.
#
# gdbus.exe — less obvious and easy to skip. jot is a single-instance
# GtkApplication and GApplication registration goes through the session
# bus. There is no dbus-daemon on win32: GLib autolaunches its own bus by
# spawning `gdbus.exe _win32_run_session_bus` (gdbusaddress.c). This is
# cheap insurance rather than a crash fix — g_application_impl_register()
# treats "no session bus" as "carry on as a non-unique application", so
# the failure mode is SILENT: launching jot.exe twice gives two windows,
# two connections to the same WAL database, and two divergent in-memory
# note lists. Nothing in CI can see that, which is exactly why it is
# bundled unconditionally instead of being left to chance.
#
# All three are walked as DLL seeds too — gdbus.exe in particular links
# the whole gio/glib chain, which is already present, but the walk costs
# nothing and guards against a future MSYS2 layout change.
for h in gspawn-win64-helper.exe gspawn-win64-helper-console.exe gdbus.exe; do
  if [[ -f "$BIN/$h" ]]; then
    cp -f "$BIN/$h" "$OUT/"
    walk "$OUT/$h"
  else
    echo "::warning::$h not found in $BIN — GLib spawn/session-bus may fail at runtime"
  fi
done

# ── 5. GSettings schemas — COMPILED ─────────────────────────────────────
#
# GTK4 calls g_settings_new("org.gtk.gtk4.Settings.FileChooser") etc.
# A missing schema is a g_error(), i.e. abort() — not a warning. This is
# the single most common "my GTK app instantly dies on Windows" cause.
# Copy the XML from every installed schema provider (gtk4, libadwaita,
# gsettings-desktop-schemas) and compile ONE gschemas.compiled.
#
SCHEMA_DST="$OUT/share/glib-2.0/schemas"
mkdir -p "$SCHEMA_DST"
cp -f "$SHARE/glib-2.0/schemas/"*.gschema.xml       "$SCHEMA_DST/" 2>/dev/null || true
cp -f "$SHARE/glib-2.0/schemas/"*.gschema.override  "$SCHEMA_DST/" 2>/dev/null || true
cp -f "$SHARE/glib-2.0/schemas/"*.enums.xml         "$SCHEMA_DST/" 2>/dev/null || true
glib-compile-schemas "$SCHEMA_DST"
[[ -f "$SCHEMA_DST/gschemas.compiled" ]] \
  || { echo "!! glib-compile-schemas produced nothing" >&2; exit 1; }
# The XML is not read at runtime; drop it.
rm -f "$SCHEMA_DST"/*.xml "$SCHEMA_DST"/*.gschema.override
echo "==> schemas compiled ($(du -h "$SCHEMA_DST/gschemas.compiled" | cut -f1))"

# ── 6. Icon themes ──────────────────────────────────────────────────────
#
# Adwaita: jot's whole UI is symbolic icons. Copy the tree wholesale —
# the scalable/ vs symbolic/ split has been reshuffled across releases
# (adwaita-icon-theme 45 → 46 → 48 → 50), so cherry-picking subdirs is a
# maintenance trap. hicolor: GTK REQUIRES the fallback theme to exist;
# index.theme alone is enough but the tree is tiny anyway.
#
ICON_DST="$OUT/share/icons"
mkdir -p "$ICON_DST"
[[ -d "$SHARE/icons/Adwaita" ]] || { echo "!! Adwaita icon theme missing" >&2; exit 1; }
cp -r "$SHARE/icons/Adwaita" "$ICON_DST/"
if [[ -d "$SHARE/icons/hicolor" ]]; then cp -r "$SHARE/icons/hicolor" "$ICON_DST/"; fi
mkdir -p "$ICON_DST/hicolor/scalable/apps"
if [[ -f "$REPO_ROOT/data/icons/jot.svg" ]]; then
  cp -f "$REPO_ROOT/data/icons/jot.svg" "$ICON_DST/hicolor/scalable/apps/jot.svg"
fi

# jot's own symbolic artwork. `tag-outline-symbolic` (src/window.rs, the tag
# picker and tag filter) exists in NO version of adwaita-icon-theme; on Linux
# it happens to resolve from whatever the user's system theme carries, in a
# self-contained bundle it would render as the broken-image glyph with no
# crash and no log line. data/icons/symbolic/ is where jot ships the
# replacements; hicolor/scalable/actions is a context hicolor's index.theme
# declares, so the lookup chain (Adwaita miss → hicolor hit) finds them.
#
# ORDER MATTERS: this must land BEFORE the icon cache below. GTK4 trusts
# icon-theme.cache for directory listings when it is present, so a file
# copied in after the cache was written is invisible to the lookup even
# though it is right there on disk.
if [[ -d "$REPO_ROOT/data/icons/symbolic" ]]; then
  mkdir -p "$ICON_DST/hicolor/scalable/actions"
  cp -f "$REPO_ROOT/data/icons/symbolic/"*.svg "$ICON_DST/hicolor/scalable/actions/" 2>/dev/null || true
fi

# Icon cache: optional for GTK4 but it removes a few thousand stat() calls
# from startup. Binary name differs by package — probe both.
ICON_CACHE_BIN="$(command -v gtk4-update-icon-cache || command -v gtk-update-icon-cache || true)"
if [[ -n "$ICON_CACHE_BIN" ]]; then
  "$ICON_CACHE_BIN" -q -t -f "$ICON_DST/Adwaita" || true
  if [[ -d "$ICON_DST/hicolor" ]]; then "$ICON_CACHE_BIN" -q -t -f "$ICON_DST/hicolor" || true; fi
else
  echo ":: warning: no gtk(4)-update-icon-cache; skipping icon-theme.cache"
fi

# Icon-name audit. jot hardcodes ~25 *-symbolic names; if one is not in the
# bundled themes the button renders as the "broken image" glyph and nothing
# crashes, so it would ship broken.
#
# Read this as a NAME-PRESENCE check, not a resolution check: it only proves
# a file with that basename exists somewhere under share/icons. It does not
# prove the containing directory is listed in the theme's index.theme, nor
# that icon-theme.cache knows about it. Warn, do not fail — GTK also serves
# a set of icons from its own compiled-in gresource (pan-down-symbolic et
# al.), so a theme miss is not automatically a render miss.
_missing=()
while read -r _icon; do
  [[ -n "$_icon" ]] || continue
  if [[ -z "$(find "$ICON_DST" \( -name "$_icon.svg" -o -name "$_icon.png" \) -print -quit)" ]]; then
    _missing+=("$_icon")
  fi
done < <(grep -rhoE '"[a-z0-9]+(-[a-z0-9]+)*-symbolic"' "$REPO_ROOT/src" | tr -d '"' | sort -u)
if (( ${#_missing[@]} )); then
  echo "::warning::icon names not present in the bundled themes: ${_missing[*]}"
fi

# ── 7. GTK config: etc/gtk-4.0/settings.ini ─────────────────────────────
#
# NOTE the path: GTK reads $sysconfdir/gtk-4.0/settings.ini, and
# sysconfdir relocates to <root>/etc — NOT <root>/share. (share/gtk-4.0
# is the common wrong guess; verified against a real GTK 4.22 install.)
#
# jot forces its own light/dark CSS via AdwStyleManager, so this file is
# cosmetic: it only sets the font and stops GTK from drawing a Linux-ish
# default. Prefer AdwStyleManager::set_color_scheme(ForceDark) in code
# over gtk-application-prefer-dark-theme here — under libadwaita the
# settings.ini key does not drive the Adwaita stylesheet.
mkdir -p "$OUT/etc/gtk-4.0"
cat > "$OUT/etc/gtk-4.0/settings.ini" <<'INI'
[Settings]
gtk-font-name=Segoe UI 10
gtk-application-prefer-dark-theme=1
gtk-hint-font-metrics=1
INI

# ── 8. fontconfig (only if it is actually in the closure) ───────────────
# Pango on Windows normally uses the win32 font backend, but librsvg and
# some pango builds pull libfontconfig in. If it got copied in step 2/3,
# it will emit "Fontconfig error: Cannot load default config file" and
# fall back to a hardcoded config unless etc/fonts/fonts.conf exists.
if compgen -G "$OUT/libfontconfig*.dll" > /dev/null && [[ -d "$ETCDIR/fonts" ]]; then
  mkdir -p "$OUT/etc"
  cp -r "$ETCDIR/fonts" "$OUT/etc/"
  echo "==> bundled etc/fonts (fontconfig is in the closure)"
fi

# ── 9. Optional data ────────────────────────────────────────────────────
# Emoji chooser data (GtkText right-click → Insert Emoji). English is
# compiled into libgtk; other locales live here.
if [[ "$WITH_EMOJI" == "1" && -d "$SHARE/gtk-4.0/emoji" ]]; then
  mkdir -p "$OUT/share/gtk-4.0"
  cp -r "$SHARE/gtk-4.0/emoji" "$OUT/share/gtk-4.0/"
fi
# Translations. Big and jot is English-only — but GTK's own stock strings
# ("Cancel", "Open", …) come from here.
if [[ "$TRIM_LOCALES" != "1" && -d "$SHARE/locale" ]]; then
  mkdir -p "$OUT/share/locale"
  cp -r "$SHARE/locale/." "$OUT/share/locale/"
fi
# Deliberately NOT bundled:
#   lib/gtk-4.0/4.0.0/media/       — GtkVideo/GtkMediaFile backend; jot has
#                                    no GtkVideo, and shipping it drags the
#                                    entire GStreamer tree (100 MB+).
#   lib/gtk-4.0/4.0.0/printbackends/ — no printing in jot.
#   lib/gio/modules/               — gvfs/glib-networking; reqwest uses
#                                    rustls, so no GIO TLS module is needed.
#   share/mime/                    — GLib uses the Windows registry for
#                                    content types. (If you do bundle it,
#                                    you must also run update-mime-database.)
#   ucrtbase.dll / api-ms-win-*    — OS components. Never ship these.
#   ffmpeg.exe                     — deliberately NOT vendored. The compare
#                                    editor shells out to ffmpeg/ffprobe if
#                                    they are on PATH and degrades to a clear
#                                    "install ffmpeg" message if they are
#                                    not; bundling an LGPL/GPL ffmpeg build
#                                    would add ~80 MB and a licence question.

# ── 10. loaders.cache with RELATIVE paths ───────────────────────────────
#
# gdk-pixbuf-query-loaders writes each module path relative to
# g_win32_get_package_installation_directory_of_module(NULL) — the
# TOPLEVEL OF ITSELF. So run a copy of it from inside the bundle: the
# computed toplevel becomes $OUT and the emitted paths come out as
# "lib\\gdk-pixbuf-2.0\\2.10.0\\loaders\\pixbufloader_svg.dll".
# At runtime gdk-pixbuf resolves those against the toplevel of the
# gdk-pixbuf DLL, which is also $OUT. Fully relocatable, no env vars.
#
# Running the system copy from /ucrt64/bin instead bakes
# C:\msys64\ucrt64\... into the cache and the zip breaks on every other
# machine — with icons silently missing rather than a crash.
#
QL_SRC="$BIN/gdk-pixbuf-query-loaders.exe"
[[ -f "$QL_SRC" ]] || { echo "!! gdk-pixbuf-query-loaders.exe not found" >&2; exit 1; }
cp -f "$QL_SRC" "$OUT/gdk-pixbuf-query-loaders.exe"
(
  cd "$OUT"
  GDK_PIXBUF_MODULEDIR="$(cygpath -w "$PB_DST")" \
    ./gdk-pixbuf-query-loaders.exe > "lib/gdk-pixbuf-2.0/$PB_VER/loaders.cache"
)
rm -f "$OUT/gdk-pixbuf-query-loaders.exe"

CACHE="$OUT/lib/gdk-pixbuf-2.0/$PB_VER/loaders.cache"
grep -qi 'svg' "$CACHE" \
  || { echo "!! loaders.cache registers no svg loader — every symbolic icon will be blank" >&2; exit 1; }

# Belt-and-braces: if this gdk-pixbuf build did NOT strip its toplevel,
# strip it ourselves. Literal string surgery via awk/ENVIRON — no regex,
# so no backslash-escaping minefield. (awk -v mangles backslashes; ENVIRON
# does not. That distinction is why this is written the ugly way.)
if grep -qE '^"[A-Za-z]:' "$CACHE"; then
  echo ":: loaders.cache came out absolute — rewriting the paths to relative"
  _w="$(cygpath -w "$OUT")"          # D:\a\jot\jot\dist\pkg
  _m="$(cygpath -m "$OUT")"          # D:/a/jot/jot/dist/pkg
  JOT_P1="${_w//\\/\\\\}\\\\" \
  JOT_P2="${_w}\\" \
  JOT_P3="${_m}/" \
  awk 'BEGIN{split("",P); P[1]=ENVIRON["JOT_P1"]; P[2]=ENVIRON["JOT_P2"]; P[3]=ENVIRON["JOT_P3"]}
       { for (i=1;i<=3;i++) { n=index($0,P[i]);
           if (n>0) { $0 = substr($0,1,n-1) substr($0,n+length(P[i])); break } }
         print }' "$CACHE" > "$CACHE.tmp"
  mv -f "$CACHE.tmp" "$CACHE"
fi

# Hard gate: no drive letter may survive in a module path.
if grep -qE '^"[A-Za-z]:' "$CACHE"; then
  echo "!! loaders.cache still contains ABSOLUTE paths — the zip is not relocatable." >&2
  grep -m3 -E '^"[A-Za-z]:' "$CACHE" >&2
  exit 1
fi
echo "==> loaders.cache OK ($(grep -c '^"lib' "$CACHE") modules, all relative)"

# ── 11. Docs ────────────────────────────────────────────────────────────
cp -f "$REPO_ROOT/README.md" "$REPO_ROOT/LICENSE" "$OUT/"
cat > "$OUT/README-WINDOWS.txt" <<'TXT'
jot for Windows — portable build
================================

Unzip anywhere and run jot.exe. Nothing is installed, nothing is written
to the registry. The whole GTK4 + libadwaita runtime lives in this
folder; do not move jot.exe out of it.

Window behaviour
----------------
* Escape minimizes the window. Alt+F4 (or the X button) saves and quits.
* The window is fully opaque on Windows — the transparency slider from
  the Linux build is hidden because per-pixel alpha is not wired up in
  the win32 GDK backend yet.

Optional: ffmpeg
----------------
The before/after GIF compare editor shells out to ffmpeg and ffprobe.
They are NOT bundled (an ffmpeg build would roughly double the download
and drag in a licensing question), so install them once and make sure
they are on your PATH:

    winget install Gyan.FFmpeg

Then restart jot. Without ffmpeg the compare editor opens and tells you
what is missing instead of failing silently.

The screen-region GIF *recorder* (Super+R on Linux) is Linux/Wayland
only — it drives wf-recorder, slurp and wl-copy, which have no Windows
equivalent. It is compiled out of this build; GIF export from the
compare editor is unaffected.

Where your data lives
---------------------
    %LOCALAPPDATA%\jot\notes.db      SQLite database (WAL mode)
    %LOCALAPPDATA%\jot\images\       Pasted / dropped images
    %LOCALAPPDATA%\jot\backups\      Daily snapshots (last 7)
    %LOCALAPPDATA%\jot\jot.log       Runtime log
    %APPDATA%\jot\config.toml        Window size, theme, font, Groq API key
    %LOCALAPPDATA%\jot-panic.log     Last crash message (if any)

Deleting the folder you unzipped removes the app but not your notes.

Troubleshooting
---------------
* Blank, black or garbled window (common on RDP, in VMs, and on machines
  with no Vulkan driver): GTK picks a GPU renderer by default. Force the
  software path before launching —

      set GSK_RENDERER=cairo
      jot.exe

  or try  set GSK_RENDERER=gl  for the OpenGL path. To make it stick,
  add GSK_RENDERER as a user environment variable.
* jot.exe closes immediately with no window: check
  %LOCALAPPDATA%\jot-panic.log and %LOCALAPPDATA%\jot\jot.log. If both
  are absent the process died before Rust ran, which almost always means
  a file was deleted from this folder — re-extract the zip.
* SmartScreen warns about an unknown publisher: these builds are not
  code-signed. "More info" → "Run anyway", or verify the download
  against the .sha256 published next to the zip.
TXT

# ── 12. Static closure check ────────────────────────────────────────────
#
# Deterministic complement to the GUI smoke test: re-read the import table
# of EVERY PE in the bundle and assert each imported DLL is either present
# in the bundle or on the Windows system allowlist. Catches a missed
# transitive dep by NAME, instead of the launch test returning a bare
# 0xC0000135 (STATUS_DLL_NOT_FOUND). Needs no window station, so it also
# works if you ever move this to a container.
#
SYS_ALLOW='^(advapi32|api-ms-win-.*|bcrypt|combase|comctl32|comdlg32|crypt32|d3d11|d3d12|dbghelp|dcomp|dnsapi|dwmapi|dwrite|dxgi|gdi32|gdiplus|hid|imm32|iphlpapi|kernel32|kernelbase|mf|mfplat|mfreadwrite|msimg32|msvcrt|ncrypt|netapi32|normaliz|ntdll|ole32|oleaut32|opengl32|pdh|powrprof|propsys|psapi|rpcrt4|secur32|setupapi|shcore|shell32|shlwapi|synchronization|user32|userenv|usp10|vcruntime140.*|version|vulkan-1|winhttp|wininet|winmm|winspool\.drv|wldap32|ws2_32|wsock32|ucrtbase|dhcpcsvc|dsound|avrt|mmdevapi|ksuser|d2d1|windowscodecs|wtsapi32|uxtheme|cfgmgr32|bcryptprimitives|onecore.*|ext-ms-.*)$'
echo "==> verifying the DLL closure is complete"
_broken=0
while IFS= read -r pe; do
  while read -r dep; do
    [[ -n "$dep" ]] || continue
    d="${dep,,}"; d="${d%.dll}"
    [[ -f "$OUT/$dep" ]] && continue
    [[ -f "$PB_DST/$dep" ]] && continue
    [[ "$d" =~ $SYS_ALLOW ]] && continue
    echo "!! $(basename "$pe") imports $dep — not in the bundle and not a known system DLL" >&2
    _broken=1
  done < <(pe_imports "$pe")
done < <(find "$OUT" -type f \( -name '*.dll' -o -name '*.exe' \))
(( _broken == 0 )) || { echo "!! incomplete DLL closure — refusing to ship" >&2; exit 1; }
# Explicit note on vulkan: libgtk-4-1.dll imports vulkan-1.dll at LOAD time
# (not lazily), so a missing one is a hard startup failure. MSYS2 ships the
# loader in /ucrt64/bin, so step 2 copies it and it is self-contained.
if [[ -f "$OUT/vulkan-1.dll" ]]; then
  echo "==> vulkan-1.dll bundled (loader; ICDs still come from the user's driver)"
else
  echo "::warning::vulkan-1.dll NOT bundled — relying on the user's system copy"
fi
# Never ship the OS CRT.
rm -f "$OUT"/ucrtbase.dll "$OUT"/api-ms-win-*.dll "$OUT"/vcruntime140*.dll 2>/dev/null || true

# ── 13. Zip + checksum ──────────────────────────────────────────────────
( cd "$DIST" && rm -f "$PKG.zip" "$PKG.zip.sha256" && zip -qr "$PKG.zip" "$PKG" )
( cd "$DIST" && sha256sum "$PKG.zip" > "$PKG.zip.sha256" )

echo
echo "==> $DIST/$PKG.zip"
echo "    unpacked : $(du -sh "$OUT" | cut -f1)"
echo "    zipped   : $(du -h "$DIST/$PKG.zip" | cut -f1)"
echo "    dlls     : $(find "$OUT" -maxdepth 1 -name '*.dll' | wc -l)"
echo "    loaders  : $(find "$PB_DST" -name '*.dll' | wc -l)"
