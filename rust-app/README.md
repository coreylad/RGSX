# Standalone RGSX Rust CLI

This is a small, cross-platform Rust application with a native GUI for browsing
the RGSX catalog and downloading direct HTTP(S) links. It does not require
Batocera, Python, EmulationStation, or a local RGSX installation.

It currently supports catalog platform listing, game listing/search, and direct
downloads. It does not yet replace the graphical interface, web server,
premium/debrid provider integrations, torrent backend, or Batocera-specific
features of the existing application.

## Build and launch

Install the Rust toolchain, then build and launch the GUI:

```sh
cargo build --release
cargo run --release
```

The binary is created at `target/release/rgsx` (or `rgsx.exe` on Windows).
Running it without arguments opens the GUI. No Python runtime or Batocera
packages are needed. On Linux, install the platform's usual X11 or Wayland
development/runtime libraries if they are not already present.

The GUI lets you choose a platform, filter games by title, set a download
folder, and start direct downloads. Download progress and errors appear in the
window.

## Optional CLI

```sh
 rgsx platforms
 rgsx games --platform nes
 rgsx games --platform "Super Nintendo" --search "Mario"
 rgsx download --platform nes --game "Example Game.zip" --output ./roms
```

`--platform` accepts a platform name or folder key. Ambiguous platform/game
matches are rejected with a prompt to use a more specific name. Downloads
refuse to replace existing files unless `--overwrite` is supplied. Use
`rgsx --help` for command details.

The catalog endpoint defaults to the public RGSX catalog. Override it with
`--catalog-url URL` or the `RGSX_CATALOG_SYNC_URL` environment variable.
Catalog browsing requires network access; downloaded links must be direct
HTTP(S) URLs.
