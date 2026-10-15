use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use eframe::egui;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

mod artwork;
mod debrid;
mod torrent;

const DEFAULT_CATALOG_URL: &str = "https://retrogamesets.fr/rgsx/manager/rgsx_catalog_api.php";
const CATALOG_PAGE_SIZE: u64 = 1000;
const MAX_CATALOG_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Parser)]
#[command(version, about = "Standalone RGSX catalog browser and downloader")]
struct Cli {
    /// Catalog API endpoint (or set RGSX_CATALOG_SYNC_URL).
    #[arg(long, global = true, env = "RGSX_CATALOG_SYNC_URL")]
    catalog_url: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List catalog platforms.
    Platforms,
    /// List games in a platform, optionally filtering by title.
    Games {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        search: Option<String>,
    },
    /// Download a catalog game by exact or unambiguous partial title.
    Download {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        game: String,
        /// Destination directory.
        #[arg(long, default_value = ".")]
        output: PathBuf,
        /// Replace a file with the same name if it already exists.
        #[arg(long)]
        overwrite: bool,
        /// Hoster provider: direct, real-debrid, all-debrid, or debrid-link.
        #[arg(long, env = "RGSX_DEBRID_PROVIDER", default_value = "direct", value_parser = parse_provider)]
        provider: debrid::Provider,
    },
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    next_after: u64,
    #[serde(default)]
    has_more: bool,
    error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Platform {
    id: i64,
    #[serde(default)]
    platform_name: String,
    #[serde(default)]
    folder: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Game {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    size: String,
    #[serde(
        default,
        alias = "boxart",
        alias = "boxart_url",
        alias = "image_url",
        alias = "cover_url",
        alias = "thumbnail"
    )]
    artwork_url: String,
    #[serde(default, deserialize_with = "deserialize_metadata")]
    metadata_json: serde_json::Value,
}

fn deserialize_metadata<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<serde_json::Value, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(text) => serde_json::from_str(&text).unwrap_or_default(),
        value => value,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let catalog_url = cli
        .catalog_url
        .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_owned());
    let client = reqwest::Client::builder()
        .user_agent(concat!("RGSX/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("Could not initialize HTTP client")?;

    if let Some(command) = cli.command {
        match command {
            Command::Platforms => {
                for platform in
                    fetch_pages::<Platform>(&client, &catalog_url, "platforms", None).await?
                {
                    println!(
                        "{}\t{}\t{}",
                        platform.platform_name, platform.folder, platform.id
                    );
                }
            }
            Command::Games { platform, search } => {
                let platforms =
                    fetch_pages::<Platform>(&client, &catalog_url, "platforms", None).await?;
                let platform = resolve_platform(&platforms, &platform)?;
                let games =
                    fetch_pages::<Game>(&client, &catalog_url, "games", Some(platform.id)).await?;
                for game in games.into_iter().filter(|game| {
                    search.as_ref().is_none_or(|query| {
                        game.name.to_lowercase().contains(&query.to_lowercase())
                    })
                }) {
                    println!("{}\t{}", game.name, game.size);
                }
            }
            Command::Download {
                platform,
                game,
                output,
                overwrite,
                provider,
            } => {
                let platforms =
                    fetch_pages::<Platform>(&client, &catalog_url, "platforms", None).await?;
                let platform = resolve_platform(&platforms, &platform)?;
                let games =
                    fetch_pages::<Game>(&client, &catalog_url, "games", Some(platform.id)).await?;
                let game = resolve_game(&games, &game)?;
                let settings = provider_settings(provider);
                let destination =
                    download_release(&client, game, &output, overwrite, &settings, |bytes| {
                        eprint!("\rDownloaded {bytes} bytes")
                    })
                    .await?;
                eprintln!("\nSaved to {}", destination.display());
            }
        }
    } else {
        launch_gui(client, catalog_url)?;
    }
    Ok(())
}

fn parse_provider(value: &str) -> std::result::Result<debrid::Provider, String> {
    match value
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
        .as_str()
    {
        "direct" => Ok(debrid::Provider::Direct),
        "realdebrid" => Ok(debrid::Provider::RealDebrid),
        "alldebrid" => Ok(debrid::Provider::AllDebrid),
        "debridlink" => Ok(debrid::Provider::DebridLink),
        _ => Err("Choose direct, real-debrid, all-debrid, or debrid-link".to_owned()),
    }
}

fn provider_settings(provider: debrid::Provider) -> debrid::Settings {
    let variable = match provider {
        debrid::Provider::Direct => "",
        debrid::Provider::RealDebrid => "RGSX_REAL_DEBRID_API_KEY",
        debrid::Provider::AllDebrid => "RGSX_ALL_DEBRID_API_KEY",
        debrid::Provider::DebridLink => "RGSX_DEBRID_LINK_API_KEY",
    };
    debrid::Settings {
        provider,
        api_key: std::env::var(variable)
            .or_else(|_| std::env::var("RGSX_DEBRID_API_KEY"))
            .unwrap_or_default(),
    }
}

fn validate_download(value: &str) -> Result<()> {
    if torrent::is_torrent(value) {
        return torrent::validate(value);
    }
    download_url(value).map(|_| ())
}

async fn download_release(
    client: &reqwest::Client,
    game: &Game,
    output: &Path,
    overwrite: bool,
    settings: &debrid::Settings,
    on_progress: impl FnMut(u64),
) -> Result<PathBuf> {
    validate_download(&game.url)?;
    if torrent::is_torrent(&game.url) {
        if overwrite {
            bail!("--overwrite is not supported for native torrent downloads");
        }
        return torrent::download(
            client,
            &game.url,
            output,
            &safe_filename(&game.name),
            on_progress,
        )
        .await;
    }
    let resolved = debrid::resolve(client, settings, &game.url).await?;
    let resolved_game = Game {
        url: resolved,
        ..game.clone()
    };
    download_game(client, &resolved_game, output, overwrite, on_progress).await?;
    Ok(output.join(safe_filename(&game.name)))
}

async fn fetch_pages<T: for<'de> Deserialize<'de> + Default>(
    client: &reqwest::Client,
    endpoint: &str,
    entity: &str,
    platform_id: Option<i64>,
) -> Result<Vec<T>> {
    let mut items = Vec::new();
    let mut cursor = 0;
    loop {
        let mut request = client.get(endpoint).query(&[
            ("action", "snapshot".to_owned()),
            ("entity", entity.to_owned()),
            ("after", cursor.to_string()),
            ("limit", CATALOG_PAGE_SIZE.to_string()),
        ]);
        if let Some(id) = platform_id {
            request = request.query(&[("platform_id", id.to_string())]);
        }

        let mut response = request
            .send()
            .await
            .with_context(|| format!("Could not fetch {entity} catalog"))?
            .error_for_status()
            .with_context(|| format!("Catalog rejected the {entity} request"))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("Could not read {entity} catalog response"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_RESPONSE_BYTES {
                bail!("{entity} catalog page exceeds the 32 MiB response limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let page: ApiResponse<T> =
            serde_json::from_slice(&bytes).context("Catalog returned invalid JSON")?;
        if !page.success {
            bail!(
                "{}",
                page.error
                    .unwrap_or_else(|| format!("Catalog request for {entity} failed"))
            );
        }
        if page.has_more && page.next_after <= cursor {
            bail!("Catalog pagination cursor did not advance for {entity}");
        }
        if page.items.is_empty() && page.has_more {
            bail!("Catalog returned an empty non-final page for {entity}");
        }
        items.extend(page.items);
        if !page.has_more {
            return Ok(items);
        }
        cursor = page.next_after;
    }
}

fn resolve_platform(platforms: &[Platform], query: &str) -> Result<Platform> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        bail!("Platform name cannot be empty");
    }
    let exact: Vec<_> = platforms
        .iter()
        .filter(|platform| {
            platform.platform_name.to_lowercase() == query
                || platform.folder.to_lowercase() == query
        })
        .collect();
    let matches: Vec<_> = if exact.is_empty() {
        platforms
            .iter()
            .filter(|platform| {
                platform.platform_name.to_lowercase().contains(&query)
                    || platform.folder.to_lowercase().contains(&query)
            })
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [platform] => Ok((*platform).clone()),
        [] => bail!("Platform not found: {query}"),
        _ => bail!(
            "Platform name is ambiguous; use a more specific name: {}",
            matches
                .iter()
                .map(|platform| platform.platform_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn resolve_game<'a>(games: &'a [Game], query: &str) -> Result<&'a Game> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        bail!("Game title cannot be empty");
    }
    let exact: Vec<_> = games
        .iter()
        .filter(|game| game.name.to_lowercase() == query)
        .collect();
    let matches: Vec<_> = if exact.is_empty() {
        games
            .iter()
            .filter(|game| game.name.to_lowercase().contains(&query))
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [game] => Ok(game),
        [] => bail!("Game not found: {query}"),
        _ => bail!(
            "Game title is ambiguous; use a more specific title: {}",
            matches
                .iter()
                .map(|game| game.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn download_game(
    client: &reqwest::Client,
    game: &Game,
    output_dir: &Path,
    overwrite: bool,
    mut on_progress: impl FnMut(u64),
) -> Result<()> {
    let url = download_url(&game.url)?;
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("Could not create {}", output_dir.display()))?;

    let filename = safe_filename(&game.name);
    let destination = output_dir.join(&filename);
    if !overwrite && tokio::fs::try_exists(&destination).await? {
        bail!(
            "{} already exists; pass --overwrite to replace it",
            destination.display()
        );
    }
    if tokio::fs::try_exists(&destination).await?
        && tokio::fs::metadata(&destination).await?.is_dir()
    {
        bail!("{} is a directory", destination.display());
    }
    let temporary = output_dir.join(format!("{filename}.part-{}", std::process::id()));
    let result = async {
        let mut response = client
            .get(url)
            .send()
            .await
            .context("Could not start download")?
            .error_for_status()
            .context("Download server returned an error")?;
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let media_type = value.split(';').next().unwrap_or_default().trim();
                media_type.eq_ignore_ascii_case("text/html")
                    || media_type.eq_ignore_ascii_case("application/xhtml+xml")
            })
        {
            bail!("This link returned a web page, not a file. Use a direct file link or configure a supported debrid provider in Settings (CLI: --provider).");
        }
        let mut file = tokio::fs::File::create(&temporary)
            .await
            .with_context(|| format!("Could not create {}", temporary.display()))?;
        let mut downloaded: u64 = 0;
        while let Some(chunk) = response.chunk().await.context("Download failed")? {
            file.write_all(&chunk)
                .await
                .context("Could not write download")?;
            downloaded += chunk.len() as u64;
            on_progress(downloaded);
        }
        file.flush()
            .await
            .context("Could not finish writing download")?;
        on_progress(downloaded);
        drop(file);
        if overwrite && tokio::fs::try_exists(&destination).await? {
            let backup = output_dir.join(format!("{filename}.backup-{}", std::process::id()));
            if tokio::fs::try_exists(&backup).await? {
                bail!(
                    "{} already exists; refusing to overwrite it",
                    backup.display()
                );
            }
            tokio::fs::rename(&destination, &backup)
                .await
                .with_context(|| format!("Could not preserve {}", destination.display()))?;
            if let Err(error) = tokio::fs::rename(&temporary, &destination).await {
                let restore = tokio::fs::rename(&backup, &destination).await;
                if let Err(restore_error) = restore {
                    bail!(
                        "Could not save {} ({error}); previous file remains at {} ({restore_error})",
                        destination.display(),
                        backup.display()
                    );
                }
                return Err(error)
                    .with_context(|| format!("Could not save {}", destination.display()));
            }
            tokio::fs::remove_file(&backup)
                .await
                .with_context(|| {
                    format!(
                        "Download saved, but could not remove {}",
                        backup.display()
                    )
                })?;
        } else if overwrite {
            tokio::fs::rename(&temporary, &destination)
                .await
                .with_context(|| format!("Could not save {}", destination.display()))?;
        } else {
            let mut source = tokio::fs::File::open(&temporary).await?;
            let mut target = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .await
                .with_context(|| {
                    format!(
                        "Could not save {}; it may already exist",
                        destination.display()
                    )
                })?;
            if let Err(error) = tokio::io::copy(&mut source, &mut target).await {
                drop(target);
                let _ = tokio::fs::remove_file(&destination).await;
                return Err(error).with_context(|| {
                    format!("Could not write download to {}", destination.display())
                });
            }
            if let Err(error) = target.flush().await {
                drop(target);
                let _ = tokio::fs::remove_file(&destination).await;
                return Err(error)
                    .with_context(|| format!("Could not finish writing {}", destination.display()));
            }
            drop(target);
            tokio::fs::remove_file(&temporary).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result?;
    Ok(())
}

fn download_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value.trim()).context("Game has an invalid download URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Use an HTTP(S) file or torrent URL, or a valid magnet link.");
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("HTTP(S) download URL must have a host and no embedded credentials");
    }
    Ok(url)
}

fn safe_filename(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let sanitized: String = basename
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    let sanitized = sanitized.trim().trim_end_matches('.');
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "download".to_owned()
    } else {
        sanitized.to_owned()
    }
}

enum GuiMessage {
    Platforms(std::result::Result<Vec<Platform>, String>),
    Games(std::result::Result<Vec<Game>, String>),
    DownloadProgress(u64),
    DownloadFinished(std::result::Result<PathBuf, String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Catalog,
    Downloads,
    Settings,
}

struct DownloadEntry {
    name: String,
    bytes: u64,
    status: String,
    failed: bool,
}

struct GuiApp {
    client: reqwest::Client,
    catalog_url: String,
    platforms: Vec<Platform>,
    selected_platform_id: Option<i64>,
    games: Vec<Game>,
    games_loaded: bool,
    search: String,
    output_dir: String,
    status: String,
    downloading: bool,
    tab: Tab,
    downloads: Vec<DownloadEntry>,
    debrid: debrid::Settings,
    artwork: artwork::ArtworkCache,
    sender: Sender<GuiMessage>,
    receiver: Receiver<GuiMessage>,
}

impl GuiApp {
    fn new(client: reqwest::Client, catalog_url: String) -> Self {
        let (sender, receiver) = mpsc::channel();
        let mut app = Self {
            client,
            catalog_url,
            platforms: Vec::new(),
            selected_platform_id: None,
            games: Vec::new(),
            games_loaded: false,
            search: String::new(),
            output_dir: ".".to_owned(),
            status: "Loading catalog…".to_owned(),
            downloading: false,
            tab: Tab::Catalog,
            downloads: Vec::new(),
            debrid: provider_settings(
                std::env::var("RGSX_DEBRID_PROVIDER")
                    .ok()
                    .and_then(|value| parse_provider(&value).ok())
                    .unwrap_or(debrid::Provider::Direct),
            ),
            artwork: artwork::ArtworkCache::default(),
            sender,
            receiver,
        };
        app.load_platforms();
        app
    }

    fn load_platforms(&mut self) {
        self.status = "Loading platforms…".to_owned();
        let client = self.client.clone();
        let endpoint = self.catalog_url.clone();
        let sender = self.sender.clone();
        spawn_gui_task(async move {
            let result = fetch_pages::<Platform>(&client, &endpoint, "platforms", None)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(GuiMessage::Platforms(result));
        });
    }

    fn load_games(&mut self, platform_id: i64) {
        self.games.clear();
        self.games_loaded = false;
        self.status = "Loading games…".to_owned();
        let client = self.client.clone();
        let endpoint = self.catalog_url.clone();
        let sender = self.sender.clone();
        spawn_gui_task(async move {
            let result = fetch_pages::<Game>(&client, &endpoint, "games", Some(platform_id))
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(GuiMessage::Games(result));
        });
    }

    fn start_download(&mut self, game: Game) {
        if self.downloading {
            return;
        }
        self.tab = Tab::Downloads;
        self.downloads.push(DownloadEntry {
            name: game.name.clone(),
            bytes: 0,
            status: "Connecting to download server…".to_owned(),
            failed: false,
        });
        let output_dir = PathBuf::from(self.output_dir.trim());
        let validation = if output_dir.as_os_str().is_empty() {
            Err(anyhow::anyhow!(
                "Choose a download folder in Settings first."
            ))
        } else {
            validate_download(&game.url)
        };
        if let Err(error) = validation {
            self.finish_download(Err(format!("{error:#}")));
            return;
        }
        self.downloading = true;
        self.status = format!("Starting download: {}", game.name);
        let client = self.client.clone();
        let sender = self.sender.clone();
        let settings = self.debrid.clone();
        spawn_gui_task(async move {
            let progress_sender = sender.clone();
            let mut last_reported = 0;
            let mut received = 0;
            let result = download_release(&client, &game, &output_dir, false, &settings, |bytes| {
                received = bytes;
                if bytes >= last_reported + 512 * 1024 {
                    last_reported = bytes;
                    let _ = progress_sender.send(GuiMessage::DownloadProgress(bytes));
                }
            })
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(GuiMessage::DownloadProgress(received));
            let _ = sender.send(GuiMessage::DownloadFinished(result));
        });
    }

    fn receive_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                GuiMessage::Platforms(Ok(platforms)) => {
                    self.platforms = platforms;
                    if !self
                        .platforms
                        .iter()
                        .any(|platform| Some(platform.id) == self.selected_platform_id)
                    {
                        self.selected_platform_id = self.platforms.first().map(|p| p.id);
                    }
                    if let Some(id) = self.selected_platform_id {
                        self.load_games(id);
                    } else {
                        self.status = "The catalog has no platforms.".to_owned();
                    }
                }
                GuiMessage::Platforms(Err(error)) | GuiMessage::Games(Err(error)) => {
                    self.status = error;
                }
                GuiMessage::Games(Ok(games)) => {
                    self.games = games;
                    self.games_loaded = true;
                    self.status = format!("{} games loaded.", self.games.len());
                }
                GuiMessage::DownloadProgress(bytes) => {
                    if let Some(entry) = self.downloads.last_mut() {
                        entry.bytes = bytes;
                        entry.status = "Receiving file…".to_owned();
                    }
                }
                GuiMessage::DownloadFinished(result) => {
                    self.finish_download(result);
                }
            }
        }
    }

    fn finish_download(&mut self, result: std::result::Result<PathBuf, String>) {
        self.downloading = false;
        let failed = result.is_err();
        let status = match result {
            Ok(path) => format!("Saved to {}", path.display()),
            Err(error) => format!("Download failed: {error}"),
        };
        if let Some(entry) = self.downloads.last_mut() {
            entry.status = status;
            entry.failed = failed;
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("navigation")
            .min_size(112.0)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(22, 30, 48))
                    .inner_margin(16.0),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(
                        egui::RichText::new("RGSX")
                            .size(28.0)
                            .color(egui::Color32::from_rgb(102, 218, 211)),
                    );
                    ui.label(egui::RichText::new("RETRO GAME SETS XTRA").small().weak());
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Catalog, "Catalog");
                    let label = if self.downloading {
                        "Downloads • active"
                    } else {
                        "Downloads"
                    };
                    ui.selectable_value(&mut self.tab, Tab::Downloads, label);
                    ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                });
            });

        egui::Panel::bottom("status-bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.downloading {
                    ui.spinner();
                    ui.label("Download in progress • see Downloads");
                } else {
                    ui.label(
                        egui::RichText::new("Ready").color(egui::Color32::from_rgb(102, 218, 211)),
                    );
                }
                ui.separator();
                ui.add(egui::Label::new(&self.status).truncate())
                    .on_hover_text(&self.status);
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(18.0))
            .show(ui, |ui| match self.tab {
                Tab::Catalog => self.show_catalog(ui),
                Tab::Downloads => self.show_downloads(ui),
                Tab::Settings => self.show_settings(ui),
            });
    }

    fn show_catalog(&mut self, ui: &mut egui::Ui) {
        ui.heading("Explore your next classic");
        ui.label(
            egui::RichText::new("Choose a system, find a game, and start downloading.").weak(),
        );
        ui.add_space(12.0);
        let previous_platform = self.selected_platform_id;
        ui.horizontal(|ui| {
            let selected_name = self
                .platforms
                .iter()
                .find(|platform| Some(platform.id) == self.selected_platform_id)
                .map(|platform| platform.platform_name.as_str())
                .unwrap_or("Select a platform");
            egui::ComboBox::from_id_salt("platform-picker")
                .width(240.0)
                .selected_text(selected_name)
                .show_ui(ui, |ui| {
                    for platform in &self.platforms {
                        ui.selectable_value(
                            &mut self.selected_platform_id,
                            Some(platform.id),
                            &platform.platform_name,
                        );
                    }
                });
            if ui.button("Refresh catalog").clicked() {
                self.load_platforms();
            }
        });
        if previous_platform != self.selected_platform_id {
            if let Some(id) = self.selected_platform_id {
                self.load_games(id);
            }
        }
        ui.add_space(8.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search games…")
                .desired_width(f32::INFINITY),
        );
        let query = self.search.trim().to_lowercase();
        let visible_games: Vec<_> = self
            .games
            .iter()
            .filter(|game| query.is_empty() || game.name.to_lowercase().contains(&query))
            .collect();
        ui.label(
            egui::RichText::new(format!("{} games", visible_games.len()))
                .small()
                .weak(),
        );
        ui.separator();
        let mut requested_download = None;
        let platform = self
            .platforms
            .iter()
            .find(|platform| Some(platform.id) == self.selected_platform_id)
            .cloned()
            .unwrap_or_default();
        egui::ScrollArea::vertical()
            .id_salt("catalog-list")
            .auto_shrink([false, false])
            .show_rows(ui, 96.0, visible_games.len().max(1), |ui, range| {
                if !self.games_loaded && self.selected_platform_id.is_some() {
                    ui.label("Loading games…");
                } else if visible_games.is_empty() {
                    ui.label(if self.games_loaded && self.games.is_empty() {
                        "No games in this platform."
                    } else {
                        "No games match this search."
                    });
                }
                for index in range {
                    let Some(game) = visible_games.get(index) else {
                        continue;
                    };
                    ui.push_id(index, |ui| {
                        egui::Frame::group(ui.style())
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.set_min_height(72.0);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let available =
                                            !self.downloading && !game.url.trim().is_empty();
                                        let response = ui.add_enabled(
                                            available,
                                            egui::Button::new("Download")
                                                .min_size(egui::vec2(94.0, 32.0)),
                                        );
                                        let response = if self.downloading {
                                            response.on_disabled_hover_text(
                                                "Wait for the current download to finish.",
                                            )
                                        } else if game.url.trim().is_empty() {
                                            response.on_disabled_hover_text(
                                                "This catalog entry has no download link.",
                                            )
                                        } else {
                                            response
                                        };
                                        if response.clicked() {
                                            requested_download = Some((*game).clone());
                                        }
                                        self.artwork.show(ui, &self.client, &platform, game);
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(ui.available_width(), 40.0),
                                            egui::Layout::top_down(egui::Align::Min),
                                            |ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(&game.name).strong(),
                                                    )
                                                    .truncate(),
                                                )
                                                .on_hover_text(&game.name);
                                                if !game.size.is_empty() {
                                                    ui.label(
                                                        egui::RichText::new(&game.size)
                                                            .small()
                                                            .weak(),
                                                    );
                                                }
                                            },
                                        );
                                    },
                                );
                            });
                        ui.add_space(4.0);
                    });
                }
            });
        if let Some(game) = requested_download {
            self.start_download(game);
        }
    }

    fn show_downloads(&mut self, ui: &mut egui::Ui) {
        ui.heading("Your downloads");
        ui.label(
            egui::RichText::new("Progress and results for this session. One download at a time.")
                .weak(),
        );
        ui.add_space(12.0);
        egui::ScrollArea::vertical()
            .id_salt("download-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.downloads.is_empty() {
                    ui.label("Nothing here yet. Start a download from the Catalog tab.");
                }
                for (index, entry) in self.downloads.iter().enumerate().rev() {
                    egui::Frame::group(ui.style())
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(
                                egui::Label::new(egui::RichText::new(&entry.name).strong()).wrap(),
                            );
                            if self.downloading && index + 1 == self.downloads.len() {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(format!(
                                        "{:.2} MiB received",
                                        entry.bytes as f64 / 1_048_576.0
                                    ));
                                });
                            } else {
                                ui.label(format!(
                                    "{:.2} MiB received",
                                    entry.bytes as f64 / 1_048_576.0
                                ));
                            }
                            let color = if entry.failed {
                                egui::Color32::from_rgb(255, 150, 145)
                            } else {
                                egui::Color32::from_rgb(102, 218, 211)
                            };
                            ui.add(
                                egui::Label::new(egui::RichText::new(&entry.status).color(color))
                                    .wrap(),
                            );
                        });
                    ui.add_space(8.0);
                }
            });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("settings").show(ui, |ui| {
                ui.heading("Make yourself at home");
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Download folder").strong());
                ui.add(egui::TextEdit::singleline(&mut self.output_dir).desired_width(f32::INFINITY));
                ui.label("Use a full folder path, or “.” for the current working directory. Changes apply to the next download.");
                ui.add_space(16.0);
                ui.label(egui::RichText::new("Safe downloads").strong());
                ui.label("Existing files are never replaced. Choose another folder or move an existing file before retrying.");
                ui.add_space(16.0);
                ui.label(egui::RichText::new("Supported links").strong());
                ui.label("HTTP(S) files, .torrent URLs and magnet links. Torrents download natively; hoster links can use a debrid provider.");
                ui.add_space(16.0);
                ui.label(egui::RichText::new("Hoster provider").strong());
                let previous = self.debrid.provider;
                egui::ComboBox::from_id_salt("debrid-provider")
                    .selected_text(self.debrid.provider.label())
                    .show_ui(ui, |ui| {
                        for provider in debrid::Provider::ALL {
                            ui.selectable_value(&mut self.debrid.provider, provider, provider.label());
                        }
                    });
                if previous != self.debrid.provider {
                    self.debrid = provider_settings(self.debrid.provider);
                }
                if self.debrid.provider != debrid::Provider::Direct {
                    ui.label("API key");
                    ui.add(egui::TextEdit::singleline(&mut self.debrid.api_key)
                        .password(true).desired_width(f32::INFINITY));
                }
                ui.label("Keys stay in memory for this session; they are never saved to disk. Environment variables are also supported.");
                ui.add_space(16.0);
                ui.label(egui::RichText::new("Box art").strong());
                ui.label("Loaded on demand from catalog images or Libretro thumbnails for recognized systems. Missing artwork is shown as a placeholder.");
            });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_messages();
        let context = ui.ctx();
        context.request_repaint_after(Duration::from_millis(100));

        self.show(ui);
    }
}

fn spawn_gui_task(future: impl std::future::Future<Output = ()> + Send + 'static) {
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("could not create GUI worker runtime");
        runtime.block_on(future);
    });
}

fn launch_gui(client: reqwest::Client, catalog_url: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_min_inner_size([600.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RGSX",
        options,
        Box::new(move |creation_context| {
            let context = &creation_context.egui_ctx;
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgb(16, 22, 35);
            visuals.selection.bg_fill = egui::Color32::from_rgb(30, 99, 110);
            context.set_visuals(visuals);
            context.global_style_mut(|style| {
                style.spacing.item_spacing = egui::vec2(10.0, 8.0);
                style.spacing.button_padding = egui::vec2(14.0, 8.0);
            });
            Ok(Box::new(GuiApp::new(client, catalog_url)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Could not start RGSX GUI: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn test_app() -> GuiApp {
        let (sender, receiver) = mpsc::channel();
        GuiApp {
            client: reqwest::Client::new(),
            catalog_url: String::new(),
            platforms: vec![Platform {
                id: 1,
                platform_name: "Test system".to_owned(),
                folder: "test".to_owned(),
            }],
            selected_platform_id: Some(1),
            games: vec![Game {
                name: "A very long game title ".repeat(25),
                url: "rgsx+torrent://unsupported".to_owned(),
                size: "24 MiB".to_owned(),
                ..Default::default()
            }],
            games_loaded: true,
            search: String::new(),
            output_dir: ".".to_owned(),
            status: "Catalog ready".to_owned(),
            downloading: false,
            tab: Tab::Catalog,
            downloads: Vec::new(),
            debrid: debrid::Settings::default(),
            artwork: artwork::ArtworkCache::default(),
            sender,
            receiver,
        }
    }

    fn render(
        app: &mut GuiApp,
        context: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ui| app.show(ui),
        );
        output.textures_delta.clear();
        output
    }

    fn text_rect(output: &egui::FullOutput, text: &str) -> egui::Rect {
        fn find(shape: &egui::epaint::Shape, text: &str) -> Option<egui::Rect> {
            match shape {
                egui::epaint::Shape::Text(shape) if shape.galley.text() == text => {
                    Some(shape.galley.rect.translate(shape.pos.to_vec2()))
                }
                egui::epaint::Shape::Vec(shapes) => {
                    shapes.iter().find_map(|shape| find(shape, text))
                }
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|shape| find(&shape.shape, text))
            .unwrap_or_else(|| panic!("Missing visible text: {text}"))
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = PathBuf::from(format!(".rgsx-test-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn serve_download(
        content_type: &str,
        body: &[u8],
        status: &str,
    ) -> (Game, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let game = Game {
            name: "example.zip".to_owned(),
            url: format!("http://{}/file", listener.local_addr().unwrap()),
            size: String::new(),
            ..Default::default()
        };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let body = body.to_vec();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket.write_all(response.as_bytes()).unwrap();
            socket.write_all(&body).unwrap();
        });
        (game, server)
    }

    #[test]
    fn download_button_is_visible_and_clickable_with_long_titles_at_minimum_size() {
        let mut app = test_app();
        let context = egui::Context::default();
        let size = egui::vec2(600.0, 400.0);
        let output = render(&mut app, &context, size, vec![]);
        let button = text_rect(&output, "Download");
        let footer = text_rect(&output, "Ready");
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert!(screen.contains_rect(button));
        assert!(screen.contains_rect(footer));
        assert!(button.bottom() < footer.top());
        let position = button.center();
        for pressed in [true, false] {
            render(
                &mut app,
                &context,
                size,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert_eq!(app.tab, Tab::Downloads);
        assert_eq!(app.downloads.len(), 1);
        assert!(app.downloads[0].failed);
        let output = render(&mut app, &context, size, vec![]);
        assert!(screen.contains_rect(text_rect(&output, "Your downloads")));
        assert!(screen.contains_rect(text_rect(&output, "Ready")));
    }

    #[test]
    fn invalid_folder_and_unsupported_links_have_visible_errors() {
        let mut app = test_app();
        app.output_dir = " ".to_owned();
        app.start_download(app.games[0].clone());
        assert!(!app.downloading);
        assert!(app.downloads[0].status.contains("Settings"));
        app.output_dir = ".".to_owned();
        app.start_download(app.games[0].clone());
        assert!(app.downloads[1].status.contains("Invalid legacy torrent"));
        assert!(app.downloads[1].failed);
    }

    #[test]
    fn catalog_messages_do_not_replace_download_results() {
        let mut app = test_app();
        app.start_download(app.games[0].clone());
        let error = app.downloads[0].status.clone();
        app.sender.send(GuiMessage::Games(Ok(vec![]))).unwrap();
        app.receive_messages();
        assert_eq!(app.downloads[0].status, error);
    }

    #[test]
    fn tabs_keep_status_visible_when_resized() {
        let mut app = test_app();
        for size in [egui::vec2(600.0, 400.0), egui::vec2(900.0, 700.0)] {
            for tab in [Tab::Catalog, Tab::Downloads, Tab::Settings] {
                app.tab = tab;
                let output = render(&mut app, &egui::Context::default(), size, vec![]);
                let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                assert!(screen.contains_rect(text_rect(&output, "Ready")));
                assert!(screen.contains_rect(text_rect(&output, "Settings")));
            }
        }
    }

    #[test]
    fn gui_download_reports_small_file_completion() {
        let directory = TestDirectory::new();
        let (game, server) = serve_download("application/zip", b"test file", "200 OK");
        let mut app = test_app();
        app.output_dir = directory.0.to_string_lossy().into_owned();
        app.start_download(game);
        assert_eq!(app.tab, Tab::Downloads);
        assert!(app.downloading);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.downloading && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            app.receive_messages();
        }
        assert!(!app.downloading);
        assert!(!app.downloads[0].failed, "{}", app.downloads[0].status);
        assert!(app.downloads[0].status.contains("Saved to"));
        assert_eq!(app.downloads[0].bytes, 9);
        assert_eq!(
            std::fs::read(directory.0.join("example.zip")).unwrap(),
            b"test file"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn release_dispatcher_validates_legacy_torrents_before_rejecting_overwrite() {
        let directory = TestDirectory::new();
        let client = reqwest::Client::new();
        for (url, expected) in [
            (
                "rgsx+torrent://unsupported",
                "Invalid legacy torrent download URL",
            ),
            (
                "rgsx+torrent://download?source=http%3A%2F%2F127.0.0.1%3A1%2Funused.torrent&path=folder%2Fgame.zip&index=1",
                "--overwrite is not supported for native torrent downloads",
            ),
        ] {
            let game = Game {
                name: "folder/game.zip".to_owned(),
                url: url.to_owned(),
                ..Default::default()
            };
            let error = download_release(
                &client,
                &game,
                &directory.0,
                true,
                &debrid::Settings::default(),
                |_| panic!("Rejected torrent must not start transferring"),
            )
            .await
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn release_dispatcher_sanitizes_legacy_torrent_destination() {
        let directory = TestDirectory::new();
        std::fs::write(directory.0.join("game.zip"), b"original").unwrap();
        let game = Game {
            name: r"..\folder\game.zip".to_owned(),
            url: "rgsx+torrent://download?source=http%3A%2F%2F127.0.0.1%3A1%2Funused.torrent&path=folder%2Fgame.zip&index=1".to_owned(),
            ..Default::default()
        };
        let error = download_release(
            &reqwest::Client::new(),
            &game,
            &directory.0,
            false,
            &debrid::Settings::default(),
            |_| panic!("Existing destination must not start transferring"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Destination already exists"));
        assert_eq!(
            std::fs::read(directory.0.join("game.zip")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn download_rejects_web_pages_and_server_errors_without_saving() {
        for (content_type, status, expected) in [
            ("text/html; charset=utf-8", "200 OK", "web page"),
            (
                "application/octet-stream",
                "403 Forbidden",
                "server returned an error",
            ),
        ] {
            let directory = TestDirectory::new();
            let (game, server) = serve_download(content_type, b"not a game", status);
            let error = download_game(&reqwest::Client::new(), &game, &directory.0, false, |_| {})
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains(expected));
            assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn download_preserves_existing_files() {
        let directory = TestDirectory::new();
        std::fs::write(directory.0.join("existing.zip"), b"original").unwrap();
        let game = Game {
            name: "existing.zip".to_owned(),
            url: "http://127.0.0.1:1/unused".to_owned(),
            size: String::new(),
            ..Default::default()
        };
        let error = download_game(&reqwest::Client::new(), &game, &directory.0, false, |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read(directory.0.join("existing.zip")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn filename_cannot_escape_output_directory() {
        assert_eq!(safe_filename("../../roms/game.zip"), "game.zip");
        assert_eq!(safe_filename(r"..\..\roms\game.zip"), "game.zip");
        assert_eq!(safe_filename(""), "download");
        assert_eq!(safe_filename("bad:name?.zip"), "bad_name_.zip");
    }

    #[test]
    fn platform_selection_accepts_name_or_folder_and_rejects_ambiguity() {
        let platforms = vec![
            Platform {
                id: 1,
                platform_name: "Nintendo Entertainment System".to_owned(),
                folder: "nes".to_owned(),
            },
            Platform {
                id: 2,
                platform_name: "Super Nintendo".to_owned(),
                folder: "snes".to_owned(),
            },
        ];
        assert_eq!(resolve_platform(&platforms, "NES").unwrap().id, 1);
        assert!(resolve_platform(&platforms, "nintendo").is_err());
        assert!(resolve_platform(&platforms, " ").is_err());
    }

    #[test]
    fn game_selection_prefers_exact_title() {
        let games = vec![
            Game {
                name: "Example".to_owned(),
                url: String::new(),
                size: String::new(),
                ..Default::default()
            },
            Game {
                name: "Example 2".to_owned(),
                url: String::new(),
                size: String::new(),
                ..Default::default()
            },
        ];
        assert_eq!(resolve_game(&games, "example").unwrap().name, "Example");
        assert!(resolve_game(&games, "exam").is_err());
        assert!(resolve_game(&games, " ").is_err());
    }

    #[test]
    fn cli_accepts_all_hoster_providers_and_rejects_unknown_values() {
        for provider in ["direct", "real-debrid", "all-debrid", "debrid-link"] {
            assert!(Cli::try_parse_from([
                "rgsx",
                "download",
                "--platform",
                "nes",
                "--game",
                "Example",
                "--provider",
                provider,
            ])
            .is_ok());
        }
        assert!(parse_provider("unknown").is_err());
    }

    #[test]
    fn invalid_downloads_fail_synchronously_even_with_a_provider() {
        for value in [
            "not a URL",
            "ftp://example.com/file",
            "magnet:invalid",
            "magnet:?xt=urn:btih:not-a-valid-hash",
        ] {
            let mut app = test_app();
            app.debrid.provider = debrid::Provider::RealDebrid;
            let game = Game {
                url: value.to_owned(),
                ..Default::default()
            };
            app.start_download(game);
            assert!(!app.downloading, "{value}");
            assert!(app.downloads[0].failed, "{value}");
            assert!(app.downloads[0].status.contains("failed"));
        }
        assert!(
            validate_download("magnet:?xt=urn:btih:0123456789012345678901234567890123456789")
                .is_ok()
        );
    }

    #[test]
    fn catalog_accepts_artwork_aliases() {
        for field in [
            "artwork_url",
            "boxart",
            "boxart_url",
            "image_url",
            "cover_url",
            "thumbnail",
        ] {
            let game: Game = serde_json::from_value(serde_json::json!({
                "name": "Example", field: "https://example.com/art.png"
            }))
            .unwrap();
            assert_eq!(game.artwork_url, "https://example.com/art.png", "{field}");
        }
    }
}
