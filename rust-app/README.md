# Standalone RGSX Rust GUI and CLI

This is a small, cross-platform Rust application with a native GUI for browsing
the RGSX catalog and downloading direct HTTP(S) links. It does not require
Batocera, Python, EmulationStation, or a local RGSX installation.

It currently supports catalog platform listing, game listing/search, and direct
downloads. Its GUI does not yet replace the controller-first interface, web server,
premium/debrid provider integrations, torrent backend, or Batocera-specific
features of the existing application.

## Build and launch

For prebuilt 64-bit Windows executables, see the
[Standalone Windows prereleases](https://github.com/coreylad/RGSX/releases).
Every successful push build publishes `rgsx.exe` under a commit-specific
`rust-<full-commit-SHA>` prerelease tag, separate from the legacy Python ZIP releases.

Install the Rust toolchain, then build and launch the GUI from `rust-app/`:

```sh
cargo build --release --locked
cargo run --release --locked
```

The binary is created at `target/release/rgsx` (or `rgsx.exe` on Windows).
Running it without arguments opens the GUI. No Python runtime or Batocera
packages are needed. On Linux, install the platform's usual X11 or Wayland
development/runtime libraries if they are not already present.

The GUI has three tabs:

- **Catalog**: choose a platform, search titles, and start a download.
- **Downloads**: see the active transfer's received bytes and this session's
  completed or failed downloads. Starting a download opens this tab immediately.
- **Settings**: set the download folder and review supported link types.

The status bar stays visible when resizing the window. Long game titles are
shortened (hover to read the full title) so Download buttons stay on screen.
Downloads run one at a time and never replace existing files. Errors, including
unavailable links or unwritable folders, remain in the Downloads tab.
Web pages returned instead of files are rejected with an explanation; provider
login/debrid and torrent links still require the legacy app. Connections time
out after 15 seconds, and stalled reads after 30 seconds.

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
