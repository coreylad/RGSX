use anyhow::{bail, ensure, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};
use reqwest::Url;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Selection {
    source: String,
    index: Option<usize>,
    path: Option<String>,
    size: Option<u64>,
}

pub fn is_torrent(value: &str) -> bool {
    let value = value.trim();
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "rgsx+torrent" | "magnet")
            || (matches!(url.scheme(), "http" | "https")
                && url.path().to_ascii_lowercase().ends_with(".torrent"))
    })
}

pub fn validate(value: &str) -> Result<()> {
    parse(value).map(|_| ())
}

fn validate_source(source: &str) -> Result<()> {
    let url = Url::parse(source).context("Invalid torrent source URL")?;
    match url.scheme() {
        "http" | "https" => {
            ensure!(url.host_str().is_some(), "Torrent source has no host");
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "Credentials in torrent source URLs are not supported"
            );
        }
        "magnet" => {
            let magnet = librqbit::Magnet::parse(source).context("Invalid magnet link")?;
            ensure!(
                magnet.as_id20().is_some(),
                "Only BitTorrent v1/hybrid magnets are supported"
            );
        }
        _ => bail!("Torrent sources must use HTTP, HTTPS or magnet"),
    }
    Ok(())
}

fn safe_relative(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "Empty torrent file path");
    ensure!(
        !value.contains('\\') && !value.contains(':') && !value.contains('\0'),
        "Unsafe torrent file path"
    );
    for part in value.split('/') {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(char::is_control),
            "Unsafe torrent file path"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "Reserved torrent file path"
        );
    }
    Ok(())
}

fn parse(value: &str) -> Result<Selection> {
    let value = value.trim();
    let url = Url::parse(value).context("Invalid torrent URL")?;
    if url.scheme() != "rgsx+torrent" {
        validate_source(value)?;
        return Ok(Selection {
            source: value.to_owned(),
            index: None,
            path: None,
            size: None,
        });
    }
    ensure!(
        url.host_str() == Some("download") && matches!(url.path(), "" | "/"),
        "Invalid legacy torrent download URL"
    );
    let mut params = std::collections::HashMap::new();
    for (key, value) in url.query_pairs() {
        ensure!(
            params
                .insert(key.into_owned(), value.into_owned())
                .is_none(),
            "Duplicate torrent URL parameter"
        );
    }
    let source = params.remove("source").context("Missing torrent source")?;
    validate_source(&source)?;
    let path = params
        .remove("path")
        .context("Missing selected torrent path")?;
    safe_relative(&path)?;
    let index = params
        .remove("index")
        .unwrap_or_else(|| "1".to_owned())
        .parse::<usize>()
        .context("Invalid selected torrent index")?;
    ensure!(index > 0, "Torrent file indices start at one");
    let size = params
        .remove("size")
        .map(|value| {
            value
                .parse::<u64>()
                .context("Invalid selected torrent size")
        })
        .transpose()?
        .filter(|size| *size > 0);
    Ok(Selection {
        source,
        index: Some(index - 1),
        path: Some(path),
        size,
    })
}

#[derive(Debug)]
struct TorrentFile {
    path: String,
    size: u64,
}

fn select_file(
    selection: &Selection,
    files: &[TorrentFile],
    root: Option<&str>,
    name: &str,
) -> Result<usize> {
    ensure!(!files.is_empty(), "Torrent contains no files");
    // Validate every path before giving the manifest to filesystem storage.
    for file in files {
        safe_relative(&file.path)?;
    }
    if let Some(index) = selection.index {
        let file = files
            .get(index)
            .context("Selected torrent index is out of range")?;
        let expected = selection.path.as_deref().context("Missing selected path")?;
        let rooted = root.map(|root| format!("{root}/{}", file.path));
        ensure!(
            expected == file.path || rooted.as_deref() == Some(expected),
            "Selected torrent path does not match its file index"
        );
        if let Some(size) = selection.size {
            ensure!(size == file.size, "Selected torrent size has changed");
        }
        return Ok(index);
    }
    if files.len() == 1 {
        return Ok(0);
    }
    let matches = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.path == name || file.path.rsplit('/').next() == Some(name))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "Multi-file torrent selection is ambiguous; select a catalog file with its path and index"
    );
    Ok(matches[0])
}

struct Staging(PathBuf);

impl Staging {
    fn create(output: &Path) -> Result<Self> {
        for _ in 0..100 {
            let id = NEXT_DOWNLOAD.fetch_add(1, Ordering::Relaxed);
            let path = output.join(format!(".rgsx-torrent-{}-{id}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("Could not create torrent staging directory")
                }
            }
        }
        bail!("Could not reserve a unique torrent staging directory")
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct SessionGuard(std::sync::Arc<Session>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.cancellation_token().cancel();
    }
}

struct IncompleteFile(Option<PathBuf>);

impl Drop for IncompleteFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Downloads exactly one selected file; never replaces an existing destination.
pub async fn download(
    client: &reqwest::Client,
    value: &str,
    output: &Path,
    name: &str,
    mut on_progress: impl FnMut(u64),
) -> Result<PathBuf> {
    let selection = parse(value)?;
    safe_relative(name).context("Unsafe destination filename")?;
    // Legacy catalogs use relative display paths to disambiguate duplicate basenames.
    let filename = name
        .rsplit('/')
        .next()
        .context("Missing destination filename")?;
    tokio::fs::create_dir_all(output).await?;
    let output = tokio::fs::canonicalize(output).await?;
    let destination = output.join(filename);
    match tokio::fs::symlink_metadata(&destination).await {
        Ok(_) => bail!("Destination already exists: {}", destination.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let staging = Staging::create(&output)?;
    let session = SessionGuard(
        Session::new_with_opts(
            staging.0.clone(),
            SessionOptions {
                dht: Some(librqbit::DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .context("Could not initialize native torrent session")?,
    );
    let result: Result<(PathBuf, u64)> = async {
        let source = if Url::parse(&selection.source)?.scheme() == "magnet" {
            AddTorrent::from_url(selection.source.clone())
        } else {
            let mut response = client
                .get(&selection.source)
                .send()
                .await?
                .error_for_status()
                .context("Could not fetch torrent manifest")?;
            ensure!(
                response.content_length().unwrap_or(0) <= MAX_MANIFEST_BYTES as u64,
                "Torrent manifest exceeds size limit"
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(
                    bytes.len().saturating_add(chunk.len()) <= MAX_MANIFEST_BYTES,
                    "Torrent manifest exceeds size limit"
                );
                bytes.extend_from_slice(&chunk);
            }
            AddTorrent::from_bytes(bytes)
        };
        let listing = tokio::time::timeout(
            Duration::from_secs(180),
            session.0.add_torrent(
                source,
                Some(AddTorrentOptions {
                    list_only: true,
                    sub_folder: Some(String::new()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .context("Timed out resolving torrent metadata")??;
        let AddTorrentResponse::ListOnly(listing) = listing else {
            bail!("Unexpected torrent metadata response");
        };
        let root = listing.info.name().map(|name| name.into_owned());
        if let Some(root) = &root {
            safe_relative(root)?;
            ensure!(!root.contains('/'), "Unsafe torrent root name");
        }
        let files = listing
            .info
            .iter_file_details()
            .map(|file| {
                ensure!(
                    !file.attrs().symlink && file.symlink_path.is_none(),
                    "Torrent symlinks are not supported"
                );
                Ok(TorrentFile {
                    path: file.filename.to_string(),
                    size: file.len,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let index = select_file(&selection, &files, root.as_deref(), name)?;
        let selected = &files[index];
        let handle = session
            .0
            .add_torrent(
                AddTorrent::from_bytes(listing.torrent_bytes),
                Some(AddTorrentOptions {
                    only_files: Some(vec![index]),
                    sub_folder: Some(String::new()),
                    initial_peers: Some(listing.seen_peers),
                    overwrite: false,
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("Could not start selected torrent file")?;
        let mut last_progress = 0;
        on_progress(0);
        loop {
            let stats = handle.stats();
            if let Some(error) = stats.error {
                bail!("Torrent download failed: {error}");
            }
            let progress = stats.file_progress.get(index).copied().unwrap_or(0);
            last_progress = last_progress.max(progress.min(selected.size));
            on_progress(last_progress);
            if stats.finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let path = staging.0.join(&selected.path);
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        ensure!(
            metadata.is_file() && metadata.len() == selected.size,
            "Selected torrent file is incomplete or not a regular file"
        );
        Ok((path, selected.size))
    }
    .await;
    session.0.stop().await;
    let (source, size) = result?;
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .await
        .context("Could not reserve torrent destination (will not overwrite)")?;
    let mut incomplete = IncompleteFile(Some(destination.clone()));
    let mut source = tokio::fs::File::open(source).await?;
    let copied = tokio::io::copy(&mut source, &mut target).await?;
    ensure!(copied == size, "Incomplete torrent file copy");
    target.flush().await?;
    target.sync_all().await?;
    incomplete.0 = None;
    on_progress(size);
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(path: &str, index: &str) -> String {
        let mut url = Url::parse("rgsx+torrent://download").unwrap();
        url.query_pairs_mut()
            .append_pair("source", "https://example.org/archive.torrent?token=a+b")
            .append_pair("path", path)
            .append_pair("index", index);
        url.into()
    }

    fn files() -> Vec<TorrentFile> {
        vec![
            TorrentFile {
                path: "a/game.zip".into(),
                size: 10,
            },
            TorrentFile {
                path: "b/game.zip".into(),
                size: 20,
            },
        ]
    }

    #[test]
    fn parses_legacy_percent_encoding_and_one_based_index() {
        let selection = parse(&legacy("Collection/a/game.zip", "1")).unwrap();
        assert_eq!(selection.index, Some(0));
        assert_eq!(
            selection.source,
            "https://example.org/archive.torrent?token=a+b"
        );
        assert_eq!(
            select_file(&selection, &files(), Some("Collection"), "ignored").unwrap(),
            0
        );
    }

    #[test]
    fn rejects_traversal_and_cross_platform_unsafe_paths() {
        for path in [
            "../game",
            "/game",
            "a//game",
            "a/./game",
            "a\\game",
            "C:/game",
            "a/NUL.zip",
            "a/game.",
        ] {
            assert!(parse(&legacy(path, "1")).is_err(), "{path}");
        }
        assert!(parse(&legacy("game", "0")).is_err());
        assert!(parse(&legacy("game", "bad")).is_err());
        assert!(parse(&(legacy("game", "1") + "&index=2")).is_err());
    }

    #[test]
    fn rejects_selection_mismatch_and_ambiguous_multi_file_torrents() {
        let selection = parse(&legacy("Collection/b/game.zip", "1")).unwrap();
        assert!(select_file(&selection, &files(), Some("Collection"), "game.zip").is_err());
        let raw = parse("https://example.org/games.torrent").unwrap();
        assert!(select_file(&raw, &files(), None, "game.zip").is_err());
        assert!(select_file(&raw, &files(), None, "unrelated").is_err());
        assert_eq!(select_file(&raw, &files(), None, "b/game.zip").unwrap(), 1);
        assert_eq!(
            select_file(&raw, &files()[..1], None, "anything").unwrap(),
            0
        );
    }

    #[test]
    fn detects_supported_sources_and_rejects_unsupported_sources() {
        assert!(is_torrent("https://example.org/GAME.TORRENT?download=1"));
        assert!(is_torrent(
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789"
        ));
        assert!(validate("magnet:?xt=urn:btih:0123456789012345678901234567890123456789").is_ok());
        assert!(validate("magnet:?dn=missing-hash").is_err());
        assert!(!is_torrent("https://example.org/game.zip"));
        assert!(parse("file:///games.torrent").is_err());
        let mut url = Url::parse(&legacy("game", "1")).unwrap();
        url.query_pairs_mut().append_pair("source", "file:///local");
        assert!(parse(url.as_str()).is_err());
    }

    #[test]
    fn selected_size_must_match_manifest() {
        let selection = parse(&(legacy("a/game.zip", "1") + "&size=99")).unwrap();
        assert!(select_file(&selection, &files(), None, "game.zip").is_err());
    }

    #[test]
    fn validates_all_manifest_paths_before_starting_download() {
        let raw = parse("https://example.org/games.torrent").unwrap();
        let files = [TorrentFile {
            path: "../escape".into(),
            size: 1,
        }];
        assert!(select_file(&raw, &files, None, "escape").is_err());
    }

    #[tokio::test]
    async fn download_rejects_unsafe_names_and_preserves_existing_files() {
        let directory = Staging::create(&std::env::current_dir().unwrap()).unwrap();
        let client = reqwest::Client::new();
        let source = "https://example.invalid/games.torrent";
        for name in ["../escape", "nested/../escape", "C:\\escape", "NUL"] {
            assert!(download(&client, source, &directory.0, name, |_| {})
                .await
                .is_err());
        }
        let existing = directory.0.join("game.zip");
        tokio::fs::write(&existing, b"original").await.unwrap();
        let error = download(&client, source, &directory.0, "game.zip", |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(tokio::fs::read(existing).await.unwrap(), b"original");
        let error = download(&client, source, &directory.0, "nested/game.zip", |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }
}
