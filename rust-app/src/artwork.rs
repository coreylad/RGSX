use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const MAX_ACTIVE: usize = 4;
const MAX_TEXTURES: usize = 96;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const ART_SIZE: egui::Vec2 = egui::vec2(52.0, 72.0);

type ArtworkResult = (String, Option<egui::ColorImage>);

/// Only visible releases are requested. Failed lookups are remembered independently
/// of the bounded texture cache, so scrolling never repeatedly fetches missing art.
pub struct ArtworkCache {
    textures: HashMap<String, egui::TextureHandle>,
    recent: VecDeque<String>,
    missing: HashSet<String>,
    active: HashSet<String>,
    sender: Sender<ArtworkResult>,
    receiver: Receiver<ArtworkResult>,
}

impl Default for ArtworkCache {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            textures: HashMap::new(),
            recent: VecDeque::new(),
            missing: HashSet::new(),
            active: HashSet::new(),
            sender,
            receiver,
        }
    }
}

impl ArtworkCache {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        client: &reqwest::Client,
        platform: &super::Platform,
        game: &super::Game,
    ) {
        while let Ok((key, image)) = self.receiver.try_recv() {
            self.active.remove(&key);
            if let Some(image) = image {
                if self.textures.len() >= MAX_TEXTURES {
                    if let Some(oldest) = self.recent.pop_front() {
                        self.textures.remove(&oldest);
                    }
                }
                let texture = ui.ctx().load_texture(
                    format!("box-art-{key}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(key.clone(), texture);
                self.recent.push_back(key);
            } else {
                self.missing.insert(key);
            }
        }
        let catalog_urls = catalog_artwork(game);
        let key = format!(
            "{}\0{}\0{}",
            platform.id,
            game.name,
            catalog_urls
                .iter()
                .map(reqwest::Url::as_str)
                .collect::<Vec<_>>()
                .join("\0"),
        );
        if let Some(texture) = self.textures.get(&key) {
            self.recent.retain(|entry| entry != &key);
            self.recent.push_back(key);
            ui.add(
                egui::Image::from_texture(texture)
                    .fit_to_exact_size(ART_SIZE)
                    .maintain_aspect_ratio(true),
            );
            return;
        }
        if !self.missing.contains(&key)
            && !self.active.contains(&key)
            && self.active.len() < MAX_ACTIVE
        {
            let urls = candidates(platform, game);
            if urls.is_empty() {
                self.missing.insert(key.clone());
            } else {
                self.active.insert(key.clone());
                let sender = self.sender.clone();
                let client = client.clone();
                let context = ui.ctx().clone();
                let request_key = key.clone();
                super::spawn_gui_task(async move {
                    let result =
                        tokio::time::timeout(Duration::from_secs(15), fetch_artwork(&client, urls))
                            .await
                            .ok()
                            .flatten();
                    let _ = sender.send((request_key, result));
                    context.request_repaint();
                });
            }
        }
        let loading = !self.missing.contains(&key);
        let (rect, _) = ui.allocate_exact_size(ART_SIZE, egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_rgb(32, 43, 61));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if loading {
                "Loading\nart…"
            } else {
                "No\nartwork"
            },
            egui::FontId::proportional(11.0),
            egui::Color32::LIGHT_GRAY,
        );
    }
}

fn libretro_system(platform: &super::Platform) -> Option<&'static str> {
    // Folder identifiers take priority over display names; never guess by substring
    // (for example, Game Boy Color is not the original Game Boy).
    fn identify(value: &str) -> Option<&'static str> {
        match value.trim().to_lowercase().as_str() {
            "nes"
            | "famicom"
            | "nintendo entertainment system"
            | "nintendo - nintendo entertainment system" => {
                Some("Nintendo - Nintendo Entertainment System")
            }
            "snes"
            | "sfc"
            | "super nintendo"
            | "super nintendo entertainment system"
            | "nintendo - super nintendo entertainment system" => {
                Some("Nintendo - Super Nintendo Entertainment System")
            }
            "gb" | "game boy" | "nintendo game boy" => Some("Nintendo - Game Boy"),
            "gbc" | "game boy color" | "nintendo game boy color" => {
                Some("Nintendo - Game Boy Color")
            }
            "gba" | "game boy advance" | "nintendo game boy advance" => {
                Some("Nintendo - Game Boy Advance")
            }
            "n64" | "nintendo 64" => Some("Nintendo - Nintendo 64"),
            "nds" | "nintendo ds" => Some("Nintendo - Nintendo DS"),
            "3ds" | "n3ds" | "nintendo 3ds" => Some("Nintendo - Nintendo 3DS"),
            "gc" | "gamecube" | "nintendo gamecube" => Some("Nintendo - GameCube"),
            "wii" | "nintendo wii" => Some("Nintendo - Wii"),
            "wiiu" | "wii u" | "nintendo wii u" => Some("Nintendo - Wii U"),
            "switch" | "nintendo switch" => Some("Nintendo - Nintendo Switch"),
            "virtualboy" | "virtual boy" => Some("Nintendo - Virtual Boy"),
            "megadrive" | "genesis" | "mega drive" | "sega genesis" | "sega mega drive" => {
                Some("Sega - Mega Drive - Genesis")
            }
            "mastersystem" | "master system" | "sega master system" => {
                Some("Sega - Master System - Mark III")
            }
            "gamegear" | "game gear" | "sega game gear" => Some("Sega - Game Gear"),
            "sega32x" | "32x" => Some("Sega - 32X"),
            "segacd" | "megacd" | "sega cd" | "mega cd" => Some("Sega - Mega-CD - Sega CD"),
            "saturn" | "sega saturn" => Some("Sega - Saturn"),
            "dreamcast" | "sega dreamcast" => Some("Sega - Dreamcast"),
            "psx" | "ps1" | "playstation" | "sony playstation" => Some("Sony - PlayStation"),
            "ps2" | "playstation 2" | "sony playstation 2" => Some("Sony - PlayStation 2"),
            "ps3" | "playstation 3" | "sony playstation 3" => Some("Sony - PlayStation 3"),
            "psp" | "playstation portable" => Some("Sony - PlayStation Portable"),
            "psvita" | "psv" | "playstation vita" => Some("Sony - PlayStation Vita"),
            "pcengine" | "pc engine" | "turbografx16" => Some("NEC - PC Engine - TurboGrafx 16"),
            "pcenginecd" | "pc engine cd" | "turbografxcd" => {
                Some("NEC - PC Engine CD - TurboGrafx-CD")
            }
            "atari2600" | "atari 2600" => Some("Atari - 2600"),
            "atari5200" | "atari 5200" => Some("Atari - 5200"),
            "atari7800" | "atari 7800" => Some("Atari - 7800"),
            "lynx" | "atari lynx" => Some("Atari - Lynx"),
            "jaguar" | "atari jaguar" => Some("Atari - Jaguar"),
            "ngp" | "neo geo pocket" => Some("SNK - Neo Geo Pocket"),
            "ngpc" | "neo geo pocket color" => Some("SNK - Neo Geo Pocket Color"),
            "wonderswan" => Some("Bandai - WonderSwan"),
            "wonderswancolor" | "wonderswan color" => Some("Bandai - WonderSwan Color"),
            "colecovision" => Some("Coleco - ColecoVision"),
            "intellivision" => Some("Mattel - Intellivision"),
            "3do" => Some("The 3DO Company - 3DO"),
            "amiga" => Some("Commodore - Amiga"),
            "c64" | "commodore 64" => Some("Commodore - 64"),
            "zxspectrum" | "zx spectrum" => Some("Sinclair - ZX Spectrum"),
            "amstradcpc" | "amstrad cpc" => Some("Amstrad - CPC"),
            "msx" => Some("Microsoft - MSX"),
            "msx2" => Some("Microsoft - MSX2"),
            "dos" => Some("DOS"),
            _ => None,
        }
    }
    identify(&platform_identity(&platform.folder)).or_else(|| {
        let name = platform_identity(&platform.platform_name);
        identify(&name).or_else(|| identify(&name.replace(" - ", " ")))
    })
}

fn platform_identity(value: &str) -> String {
    let mut name = value.trim().to_owned();
    loop {
        let lowercase = name.to_ascii_lowercase();
        let Some(suffix) = [
            "(torrents)",
            "[torrents]",
            "torrents",
            "(torrent)",
            "[torrent]",
            "torrent",
            "(1fichier)",
            "[1fichier]",
            "1fichier",
        ]
        .into_iter()
        .find(|suffix| lowercase.ends_with(suffix)) else {
            break;
        };
        name.truncate(name.len() - suffix.len());
        name = name.trim_end_matches([' ', '-', '_', '.', ':']).to_owned();
    }
    name
}

fn release_title(name: &str) -> String {
    let mut title = name.trim().to_owned();
    for extension in [
        ".torrent", ".zip", ".7z", ".rar", ".iso", ".chd", ".bin", ".cue", ".nes", ".sfc", ".smc",
        ".gb", ".gbc", ".gba", ".nds", ".n64", ".z64", ".v64", ".cso",
    ] {
        if title.to_lowercase().ends_with(extension) {
            title.truncate(title.len() - extension.len());
            // A release may have both a torrent and an archive suffix.
        }
    }
    title
}

fn normalized_title(name: &str) -> String {
    let mut title = release_title(name);
    loop {
        let trimmed = title.trim_end();
        let opening = if trimmed.ends_with(')') {
            '('
        } else if trimmed.ends_with(']') {
            '['
        } else {
            break;
        };
        let Some(index) = trimmed.rfind(opening) else {
            break;
        };
        if opening == '(' && !release_metadata(&trimmed[index + 1..trimmed.len() - 1]) {
            break;
        }
        title.truncate(index);
    }
    title.trim().to_owned()
}

fn release_metadata(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let first = value.split([',', ' ']).next().unwrap_or_default();
    matches!(
        first,
        "usa"
            | "europe"
            | "japan"
            | "world"
            | "asia"
            | "korea"
            | "china"
            | "taiwan"
            | "germany"
            | "france"
            | "spain"
            | "italy"
            | "australia"
            | "brazil"
            | "russia"
            | "canada"
            | "rev"
            | "disc"
            | "disk"
            | "side"
            | "proto"
            | "beta"
            | "demo"
            | "unl"
    ) || (value.starts_with('v') && value.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
        || value.split(',').all(|language| {
            let language = language.trim();
            (2..=3).contains(&language.len())
                && language.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
}

fn catalog_artwork(game: &super::Game) -> Vec<reqwest::Url> {
    fn add(urls: &mut Vec<reqwest::Url>, value: &str) {
        if let Ok(url) = reqwest::Url::parse(value) {
            if matches!(url.scheme(), "http" | "https") && !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    fn fields(urls: &mut Vec<reqwest::Url>, metadata: &serde_json::Value) {
        for key in [
            "artwork_url",
            "boxart_url",
            "boxart",
            "box_art",
            "cover_url",
            "cover",
            "image_url",
            "image",
            "thumbnail_url",
            "thumbnail",
            "front",
        ] {
            let value = &metadata[key];
            if let Some(url) = value
                .as_str()
                .or_else(|| value.get("url").and_then(|url| url.as_str()))
            {
                add(urls, url);
            }
        }
    }
    let mut urls = Vec::new();
    add(&mut urls, &game.artwork_url);
    fields(&mut urls, &game.metadata_json);
    for container in ["artwork", "images"] {
        fields(&mut urls, &game.metadata_json[container]);
        if container == "artwork" {
            if let Some(url) = game.metadata_json[container].as_str().or_else(|| {
                game.metadata_json[container]
                    .get("url")
                    .and_then(|url| url.as_str())
            }) {
                add(&mut urls, url);
            }
        }
    }
    urls
}

fn candidates(platform: &super::Platform, game: &super::Game) -> Vec<reqwest::Url> {
    let mut urls = catalog_artwork(game);
    let Some(system) = libretro_system(platform) else {
        return urls;
    };
    let original = release_title(&game.name);
    let normalized = normalized_title(&game.name);
    if normalized.is_empty() {
        return urls;
    }
    let mut titles = vec![original, normalized.clone()];
    for region in ["USA", "Europe", "Japan", "World"] {
        titles.push(format!("{normalized} ({region})"));
    }
    let mut seen = HashSet::new();
    for title in titles {
        // Libretro uses these replacements for filename-incompatible characters.
        let filename = format!(
            "{}.png",
            title.replace(['&', '*', '/', ':', '`', '<', '>', '?', '\\', '|'], "_")
        );
        if !seen.insert(filename.clone()) {
            continue;
        }
        let repository = system.replace(' ', "_");
        let mut url = reqwest::Url::parse("https://raw.githubusercontent.com/").unwrap();
        url.path_segments_mut().unwrap().extend([
            "libretro-thumbnails",
            &repository,
            "master",
            "Named_Boxarts",
            &filename,
        ]);
        urls.push(url);
    }
    urls
}

async fn fetch_artwork(
    client: &reqwest::Client,
    urls: Vec<reqwest::Url>,
) -> Option<egui::ColorImage> {
    for url in urls {
        if let Some(image) = fetch_image(client, url).await {
            return Some(image);
        }
    }
    None
}

async fn fetch_image(client: &reqwest::Client, url: reqwest::Url) -> Option<egui::ColorImage> {
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    decode_image(&bytes)
}

fn decode_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?.thumbnail(144, 192).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform(folder: &str) -> super::super::Platform {
        super::super::Platform {
            folder: folder.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn matches_systems_exactly_and_keeps_color_separate() {
        assert_eq!(
            libretro_system(&platform("gb")),
            Some("Nintendo - Game Boy")
        );
        assert_eq!(
            libretro_system(&platform("gbc")),
            Some("Nintendo - Game Boy Color")
        );
        assert_eq!(
            libretro_system(&super::super::Platform {
                platform_name: "Nintendo - Game Boy Color".to_owned(),
                ..Default::default()
            }),
            Some("Nintendo - Game Boy Color")
        );
        assert_eq!(libretro_system(&platform("unknown snes collection")), None);
    }

    #[test]
    fn source_suffixes_do_not_change_the_matched_platform() {
        for folder in [
            "gbc_torrent",
            "gbc-1fichier",
            "gbc (Torrent)",
            "gbc [1Fichier]",
        ] {
            assert_eq!(
                libretro_system(&platform(folder)),
                Some("Nintendo - Game Boy Color")
            );
        }
        for name in [
            "Super Nintendo - Torrent",
            "Super Nintendo (1Fichier)",
            "Super Nintendo [Torrents]",
        ] {
            assert_eq!(
                libretro_system(&super::super::Platform {
                    platform_name: name.to_owned(),
                    ..Default::default()
                }),
                Some("Nintendo - Super Nintendo Entertainment System")
            );
        }
        assert_eq!(libretro_system(&platform("unrecognized_torrent")), None);
    }

    #[test]
    fn normalizes_release_metadata_without_removing_internal_title() {
        assert_eq!(normalized_title("Example (USA) (Rev 1) [!].zip"), "Example");
        assert_eq!(
            normalized_title("Example (The Return) II (Europe).7z"),
            "Example (The Return) II"
        );
        assert_eq!(normalized_title("Example.zip.torrent"), "Example");
        assert_eq!(
            normalized_title("Example (The Return).zip"),
            "Example (The Return)"
        );
    }

    #[test]
    fn catalog_art_precedes_platform_specific_fallback_for_any_release() {
        let game = super::super::Game {
            name: "Example (USA).zip".to_owned(),
            artwork_url: "https://example.com/cover.jpg".to_owned(),
            ..Default::default()
        };
        let urls = candidates(&platform("gbc"), &game);
        assert_eq!(urls[0].as_str(), game.artwork_url);
        assert!(urls[1..].iter().all(|url| url
            .path()
            .starts_with("/libretro-thumbnails/Nintendo_-_Game_Boy_Color/master/")));
        assert_eq!(candidates(&platform("unknown"), &game).len(), 1);
        assert!(decode_image(b"not an image").is_none());
    }

    #[test]
    fn reads_per_release_metadata_without_using_platform_logos() {
        let mut game: super::super::Game = serde_json::from_value(serde_json::json!({
            "name": "Example",
            "metadata_json": serde_json::json!({
                "cover_url": "https://example.com/cover.png",
                "images": {"front": "https://example.com/front.jpg"},
                "platform_assets": {"image_url": "https://example.com/logo.png"},
                "url": "https://example.com/download.zip"
            }).to_string()
        }))
        .unwrap();
        let urls = candidates(&platform("unknown"), &game);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "https://example.com/cover.png");
        assert_eq!(urls[1].as_str(), "https://example.com/front.jpg");
        game.artwork_url = "https://example.com/preferred.png".to_owned();
        assert_eq!(
            candidates(&platform("unknown"), &game)[0].as_str(),
            game.artwork_url
        );
        let malformed: super::super::Game = serde_json::from_value(serde_json::json!({
            "name": "Example", "metadata_json": "not JSON"
        }))
        .unwrap();
        assert!(catalog_artwork(&malformed).is_empty());
    }

    #[tokio::test]
    async fn fetches_and_decodes_mocked_artwork_into_bounded_pixels() {
        use std::io::{Read, Write};
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(400, 600)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}/art", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 2048];
            assert!(socket.read(&mut request).unwrap() > 0);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            socket.write_all(&bytes).unwrap();
        });
        let image = fetch_artwork(&reqwest::Client::new(), vec![url])
            .await
            .unwrap();
        assert!(image.size[0] <= 144 && image.size[1] <= 192);
        server.join().unwrap();
    }

    #[test]
    fn failed_lookups_are_cached_without_starting_more_requests() {
        let mut cache = ArtworkCache::default();
        let game = super::super::Game {
            name: "Example".to_owned(),
            ..Default::default()
        };
        let platform = platform("nes");
        let key = format!("{}\0{}\0{}", platform.id, game.name, game.artwork_url);
        cache.sender.send((key.clone(), None)).unwrap();
        let mut output = egui::Context::default().run_ui(Default::default(), |ui| {
            cache.show(ui, &reqwest::Client::new(), &platform, &game);
        });
        output.textures_delta.clear();
        assert!(cache.missing.contains(&key));
        assert!(cache.active.is_empty());
    }

    #[test]
    fn full_worker_pool_does_not_queue_more_requests() {
        let mut cache = ArtworkCache::default();
        for index in 0..MAX_ACTIVE {
            cache.active.insert(index.to_string());
        }
        let game = super::super::Game {
            name: "Example".to_owned(),
            ..Default::default()
        };
        let mut output = egui::Context::default().run_ui(Default::default(), |ui| {
            cache.show(ui, &reqwest::Client::new(), &platform("nes"), &game);
        });
        output.textures_delta.clear();
        assert_eq!(cache.active.len(), MAX_ACTIVE);
        assert!(cache.receiver.try_recv().is_err());
    }

    #[test]
    fn downloaded_artwork_is_rendered_as_an_image_texture() {
        let mut cache = ArtworkCache::default();
        let game = super::super::Game {
            name: "Example".to_owned(),
            ..Default::default()
        };
        let platform = platform("nes");
        let key = format!("{}\0{}\0{}", platform.id, game.name, game.artwork_url);
        cache
            .sender
            .send((
                key.clone(),
                Some(egui::ColorImage::filled([2, 2], egui::Color32::RED)),
            ))
            .unwrap();
        let context = egui::Context::default();
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 400.0),
                )),
                ..Default::default()
            },
            |ui| cache.show(ui, &reqwest::Client::new(), &platform, &game),
        );
        output.textures_delta.clear();
        let texture = cache.textures[&key].id();
        let primitives = context.tessellate(output.shapes, output.pixels_per_point);
        assert!(primitives.iter().any(|primitive| {
            matches!(&primitive.primitive, egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture)
        }));
    }
}
