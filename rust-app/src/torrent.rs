use anyhow::{bail, ensure, Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};
use reqwest::Url;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
const STALLED_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
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

fn metadata_path<BufType: AsRef<[u8]>>(
    filename: &librqbit::FileIteratorName<'_, BufType>,
) -> String {
    filename.iter_components().collect::<Vec<_>>().join("/")
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
    on_progress: impl FnMut(u64),
) -> Result<PathBuf> {
    download_with_options(
        client,
        value,
        output,
        name,
        on_progress,
        SessionOptions {
            dht: Some(librqbit::DhtSessionConfig {
                persistence: None,
                ..Default::default()
            }),
            ..Default::default()
        },
        STALLED_DOWNLOAD_TIMEOUT,
    )
    .await
}

async fn download_with_options(
    client: &reqwest::Client,
    value: &str,
    output: &Path,
    name: &str,
    mut on_progress: impl FnMut(u64),
    session_options: SessionOptions,
    stalled_timeout: Duration,
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
        Session::new_with_opts(staging.0.clone(), session_options)
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
                    path: metadata_path(&file.filename),
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
        let mut last_activity = tokio::time::Instant::now();
        on_progress(0);
        loop {
            let stats = handle.stats();
            if let Some(error) = stats.error {
                bail!("Torrent download failed: {error}");
            }
            let progress = stats.file_progress.get(index).copied().unwrap_or(0);
            if progress > last_progress {
                last_activity = tokio::time::Instant::now();
            }
            last_progress = last_progress.max(progress.min(selected.size));
            on_progress(last_progress);
            if stats.finished {
                break;
            }
            ensure!(
                last_activity.elapsed() < stalled_timeout,
                "Torrent stalled: no verified download progress for {} seconds; no reachable peers may be available",
                stalled_timeout.as_secs()
            );
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
    // Declare cleanup before the handle so Windows closes it before removing a failed copy.
    let mut incomplete = IncompleteFile(None);
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .await
        .context("Could not reserve torrent destination (will not overwrite)")?;
    incomplete.0 = Some(destination.clone());
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

    struct LocalTracker {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl LocalTracker {
        fn start(
            listener: std::net::TcpListener,
            manifest: Vec<u8>,
            peer: std::net::SocketAddr,
        ) -> Self {
            use std::io::{Read, Write};
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_stop = stop.clone();
            listener.set_nonblocking(true).unwrap();
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("Local tracker failed: {error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = [0; 8192];
                    let length = stream.read(&mut request).unwrap_or(0);
                    let body = if request[..length].starts_with(b"GET /manifest.torrent ") {
                        manifest.clone()
                    } else {
                        let std::net::IpAddr::V4(address) = peer.ip() else {
                            panic!("Expected loopback IPv4 peer");
                        };
                        let mut body = b"d8:intervali1e5:peers6:".to_vec();
                        body.extend_from_slice(&address.octets());
                        body.extend_from_slice(&peer.port().to_be_bytes());
                        body.push(b'e');
                        body
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self {
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for LocalTracker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

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
    fn nested_metadata_paths_use_catalog_slashes() {
        let metadata = librqbit::torrent_from_bytes(
            b"d4:infod5:filesld6:lengthi10e4:pathl6:nested6:folder8:game.zipeee4:name10:Collection12:piece lengthi16384e6:pieces20:00000000000000000000ee",
        )
        .unwrap();
        let info = metadata.info.data.validate().unwrap();
        let files = info
            .iter_file_details()
            .map(|file| TorrentFile {
                path: metadata_path(&file.filename),
                size: file.len,
            })
            .collect::<Vec<_>>();
        assert_eq!(files[0].path, "nested/folder/game.zip");
        safe_relative(&files[0].path).unwrap();
        let selection = parse(&legacy("Collection/nested/folder/game.zip", "1")).unwrap();
        assert_eq!(
            select_file(&selection, &files, Some("Collection"), "ignored").unwrap(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_seed_downloads_only_selected_file_and_stops_sessions() {
        let directory = Staging::create(&std::env::current_dir().unwrap()).unwrap();
        let seed_directory = directory.0.join("seed");
        let output = directory.0.join("download");
        tokio::fs::create_dir(&seed_directory).await.unwrap();
        let wanted = vec![42_u8; 16 * 1024];
        tokio::fs::write(seed_directory.join("chosen.bin"), &wanted)
            .await
            .unwrap();
        tokio::fs::write(
            seed_directory.join("unselected.bin"),
            vec![99_u8; 16 * 1024],
        )
        .await
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tracker_address = listener.local_addr().unwrap();
        let seed = SessionGuard(
            Session::new_with_opts(
                seed_directory.clone(),
                SessionOptions {
                    dht: None,
                    disable_trackers: true,
                    disable_local_service_discovery: true,
                    ipv4_only: true,
                    listen: Some(librqbit::ListenerOptions {
                        listen_addr: "127.0.0.1:0".parse().unwrap(),
                        ipv4_only: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
        let (torrent, handle) = seed
            .0
            .create_and_serve_torrent(
                &seed_directory,
                librqbit::CreateTorrentOptions {
                    name: Some("Collection"),
                    trackers: vec![format!("http://{tracker_address}/announce")],
                    piece_length: Some(16 * 1024),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle.wait_until_completed())
            .await
            .unwrap()
            .unwrap();
        let index = handle
            .with_metadata(|metadata| {
                metadata
                    .info
                    .iter_file_details()
                    .position(|file| file.filename.to_string() == "chosen.bin")
                    .unwrap()
            })
            .unwrap();
        let _tracker = LocalTracker::start(
            listener,
            torrent.as_bytes().unwrap().to_vec(),
            seed.0.listen_addr().unwrap(),
        );
        let mut url = Url::parse("rgsx+torrent://download").unwrap();
        url.query_pairs_mut()
            .append_pair(
                "source",
                &format!("http://{tracker_address}/manifest.torrent"),
            )
            .append_pair("path", "Collection/chosen.bin")
            .append_pair("index", &(index + 1).to_string())
            .append_pair("size", &wanted.len().to_string());
        let client = reqwest::Client::new();
        let stopped = seed.0.cancellation_token().child_token();
        let mut progress = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            download_with_options(
                &client,
                url.as_str(),
                &output,
                "renamed.bin",
                |bytes| progress.push(bytes),
                SessionOptions {
                    dht: None,
                    disable_local_service_discovery: true,
                    cancellation_token: Some(stopped.clone()),
                    ..Default::default()
                },
                Duration::from_secs(10),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, output.canonicalize().unwrap().join("renamed.bin"));
        assert_eq!(tokio::fs::read(result).await.unwrap(), wanted);
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        assert_eq!(progress.last(), Some(&(wanted.len() as u64)));
        assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(
            stopped.is_cancelled(),
            "Downloader must stop before returning"
        );
        assert_eq!(handle.stats().uploaded_bytes, wanted.len() as u64);

        seed.0.stop().await;
        assert!(seed.0.cancellation_token().is_cancelled());
        let stalled_options = SessionOptions {
            dht: None,
            disable_local_service_discovery: true,
            cancellation_token: Some(Default::default()),
            ..Default::default()
        };
        let stalled = stalled_options.cancellation_token.as_ref().unwrap().clone();
        // A stopped seed advertised by a tracker must not occupy the download slot forever.
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            download_with_options(
                &client,
                url.as_str(),
                &output,
                "no-peers.bin",
                |_| {},
                stalled_options,
                Duration::from_millis(100),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            error.to_string().contains("no reachable peers"),
            "{error:#}"
        );
        assert!(!output.join("no-peers.bin").exists());
        assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
        assert!(stalled.is_cancelled());
    }
}
