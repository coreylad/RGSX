use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use eframe::egui;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const DEFAULT_CATALOG_URL: &str = "https://retrogamesets.fr/rgsx/manager/rgsx_catalog_api.php";
const CATALOG_PAGE_SIZE: u64 = 1000;
const MAX_CATALOG_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Standalone RGSX catalog browser and direct downloader"
)]
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let catalog_url = cli
        .catalog_url
        .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_owned());
    let client = reqwest::Client::builder()
        .user_agent(concat!("RGSX/", env!("CARGO_PKG_VERSION")))
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
            } => {
                let platforms =
                    fetch_pages::<Platform>(&client, &catalog_url, "platforms", None).await?;
                let platform = resolve_platform(&platforms, &platform)?;
                let games =
                    fetch_pages::<Game>(&client, &catalog_url, "games", Some(platform.id)).await?;
                let game = resolve_game(&games, &game)?;
                download_game(&client, &game, &output, overwrite, |bytes| {
                    eprint!("\rDownloaded {bytes} bytes")
                })
                .await?;
                eprintln!(
                    "\nSaved to {}",
                    output.join(safe_filename(&game.name)).display()
                );
            }
        }
    } else {
        launch_gui(client, catalog_url)?;
    }
    Ok(())
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
    let url = reqwest::Url::parse(&game.url).context("Game has an invalid download URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Only direct HTTP(S) download links are supported");
    }
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
        let output_dir = PathBuf::from(self.output_dir.trim());
        if output_dir.as_os_str().is_empty() {
            self.status = "Choose a download folder first.".to_owned();
            return;
        }
        self.downloading = true;
        self.status = format!("Starting download: {}", game.name);
        let client = self.client.clone();
        let sender = self.sender.clone();
        spawn_gui_task(async move {
            let destination = output_dir.join(safe_filename(&game.name));
            let progress_sender = sender.clone();
            let mut last_reported = 0;
            let result = download_game(&client, &game, &output_dir, false, move |bytes| {
                if bytes >= last_reported + 512 * 1024 {
                    last_reported = bytes;
                    let _ = progress_sender.send(GuiMessage::DownloadProgress(bytes));
                }
            })
            .await
            .map(|()| destination)
            .map_err(|error| format!("{error:#}"));
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
                    self.status = format!("Downloading… {bytes} bytes received");
                }
                GuiMessage::DownloadFinished(Ok(path)) => {
                    self.downloading = false;
                    self.status = format!("Download saved to {}", path.display());
                }
                GuiMessage::DownloadFinished(Err(error)) => {
                    self.downloading = false;
                    self.status = format!("Download failed: {error}");
                }
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_messages();
        let context = ui.ctx();
        context.request_repaint_after(Duration::from_millis(100));

        let mut requested_download = None;
        let previous_platform = self.selected_platform_id;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("RGSX — Retro Game Sets Xtra");
            ui.label("Browse the catalog and download directly to your chosen folder.");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Platform");
                let selected_name = self
                    .platforms
                    .iter()
                    .find(|platform| Some(platform.id) == self.selected_platform_id)
                    .map(|platform| platform.platform_name.as_str())
                    .unwrap_or("Select a platform");
                egui::ComboBox::from_id_salt("platform-picker")
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
                if ui.button("Refresh").clicked() {
                    self.load_platforms();
                }
            });

            if previous_platform != self.selected_platform_id {
                if let Some(id) = self.selected_platform_id {
                    self.load_games(id);
                }
            }

            ui.horizontal(|ui| {
                ui.label("Search");
                ui.text_edit_singleline(&mut self.search);
            });
            ui.horizontal(|ui| {
                ui.label("Download folder");
                ui.text_edit_singleline(&mut self.output_dir);
            });
            ui.separator();

            let query = self.search.trim().to_lowercase();
            let visible_games: Vec<_> = self
                .games
                .iter()
                .filter(|game| query.is_empty() || game.name.to_lowercase().contains(&query))
                .cloned()
                .collect();
            let no_visible_games = visible_games.is_empty();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for game in visible_games {
                        ui.horizontal(|ui| {
                            ui.label(&game.name);
                            if !game.size.is_empty() {
                                ui.label(&game.size);
                            }
                            let can_download = !self.downloading && !game.url.is_empty();
                            if ui
                                .add_enabled(can_download, egui::Button::new("Download"))
                                .clicked()
                            {
                                requested_download = Some(game.clone());
                            }
                        });
                        ui.separator();
                    }
                });

            ui.label(&self.status);
            if !self.games_loaded && self.selected_platform_id.is_some() {
                ui.label("Loading games…");
            } else if self.games.is_empty() && self.games_loaded {
                ui.label("No games in this platform.");
            } else if no_visible_games {
                ui.label("No games match this search.");
            }
        });

        if let Some(game) = requested_download {
            self.start_download(game);
        }
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
        Box::new(move |_creation_context| Ok(Box::new(GuiApp::new(client, catalog_url)))),
    )
    .map_err(|error| anyhow::anyhow!("Could not start RGSX GUI: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{resolve_game, resolve_platform, safe_filename, Game, Platform};

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
            },
            Game {
                name: "Example 2".to_owned(),
                url: String::new(),
                size: String::new(),
            },
        ];
        assert_eq!(resolve_game(&games, "example").unwrap().name, "Example");
        assert!(resolve_game(&games, "exam").is_err());
        assert!(resolve_game(&games, " ").is_err());
    }
}
