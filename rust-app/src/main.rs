use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::path::{Path, PathBuf};
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
    command: Command,
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

#[derive(Debug, Default, Deserialize)]
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

    match cli.command {
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
                search
                    .as_ref()
                    .is_none_or(|query| game.name.to_lowercase().contains(&query.to_lowercase()))
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
            download_game(&client, &game, &output, overwrite).await?;
        }
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
            eprint!("\rDownloaded {} bytes", downloaded);
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
    eprintln!("\nSaved to {}", destination.display());
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
