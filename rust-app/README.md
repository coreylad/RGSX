# Standalone RGSX Rust GUI and CLI

This is a cross-platform Rust application with a native GUI for browsing
the RGSX catalog, looking up box art, and downloading HTTP(S) and torrent links. It does not require
Batocera, Python, EmulationStation, or a local RGSX installation.

It supports catalog platform listing, game listing/search, native torrent
downloads, and hoster-link unlocking through configurable debrid providers.
Its GUI does not yet replace the controller-first interface, web server, or
Batocera-specific features of the existing application.

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

- **Catalog**: choose a platform, search titles, view available box art, and start a download.
- **Downloads**: see the active transfer's received bytes and this session's
  completed or failed downloads. Starting a download opens this tab immediately.
- **Settings**: set the download folder, choose a debrid provider, and enter its API key.

The status bar stays visible when resizing the window. Long game titles are
shortened (hover to read the full title) so Download buttons stay on screen.
Downloads run one at a time and never replace existing files. Errors, including
unavailable links or unwritable folders, remain in the Downloads tab.
Web pages returned instead of files are rejected with an explanation. HTTP
connections time out after 15 seconds, and stalled reads after 30 seconds.

## Torrents and debrid providers

Torrent transfers run inside the executable; no Python, qBittorrent, or separate
torrent client is needed. Catalog `rgsx+torrent://` links select the release inside
a multi-file torrent rather than downloading the entire collection. Magnet and
HTTP(S) `.torrent` links are also supported. Transfers depend on available peers;
allow the app through your firewall if necessary. Metadata lookup times out after
three minutes; a transfer with no verified progress for five minutes fails so
another download can be started. Download only content you are
authorized to obtain.

For supported hoster links, choose **Real-Debrid**, **AllDebrid**, or
**Debrid-Link** in Settings and enter your own API key. An active account and
support for the linked host are required. Direct HTTP(S) and native torrents do
not require a debrid subscription. Keys are masked in the GUI and are not saved
to disk; re-enter them when restarting the app. Unsupported hosters, expired
accounts, login pages, and unavailable releases produce an error rather than a
false success.

## Box art

The catalog view automatically looks up artwork as releases become visible.
Lookup is available throughout the release list, not just its first page, and
does not block browsing or downloads. Catalog-provided image URLs take priority;
supported systems also use the public Libretro thumbnail collection. Missing artwork is shown as a placeholder:
not every release, hack, regional variant, or platform has an image in the
artwork source. Artwork availability does not determine download availability.

## Optional CLI

```sh
rgsx platforms
rgsx games --platform nes
rgsx games --platform "Super Nintendo" --search "Mario"
rgsx download --platform nes --game "Example Game.zip" --output ./roms
```

`--platform` accepts a platform name or folder key. Ambiguous platform/game
matches are rejected with a prompt to use a more specific name. Downloads
refuse to replace existing files; `--overwrite` is available for HTTP downloads,
not native torrent downloads. Use
`rgsx --help` for command details.

For hoster links, add `--provider real-debrid`, `--provider all-debrid`, or
`--provider debrid-link`. Set the API key through `RGSX_REAL_DEBRID_API_KEY`,
`RGSX_ALL_DEBRID_API_KEY`, or `RGSX_DEBRID_LINK_API_KEY`, respectively
(`RGSX_DEBRID_API_KEY` is a fallback). Do not put API keys in download URLs or
command-line arguments. `RGSX_DEBRID_PROVIDER` selects the initial provider in
the GUI and the default CLI provider. Torrent links always use the native
backend, independently of the hoster provider.

The catalog endpoint defaults to the public RGSX catalog. Override it with
`--catalog-url URL` or the `RGSX_CATALOG_SYNC_URL` environment variable.
Catalog browsing requires network access.
