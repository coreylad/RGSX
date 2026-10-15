use anyhow::{bail, Result};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const TORRENT_POLLS: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    Direct,
    RealDebrid,
    AllDebrid,
    DebridLink,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::Direct,
        Provider::RealDebrid,
        Provider::AllDebrid,
        Provider::DebridLink,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::RealDebrid => "Real-Debrid",
            Self::AllDebrid => "AllDebrid",
            Self::DebridLink => "Debrid-Link",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::Direct => "",
            Self::RealDebrid => "https://api.real-debrid.com/rest/1.0/",
            Self::AllDebrid => "https://api.alldebrid.com/v4/",
            Self::DebridLink => "https://debrid-link.com/api/v2/",
        }
    }
}

// Deliberately not Debug or Serialize: API keys are session-only credentials.
#[derive(Clone, Default)]
pub struct Settings {
    pub provider: Provider,
    pub api_key: String,
}

pub async fn resolve(_client: &Client, settings: &Settings, value: &str) -> Result<String> {
    if settings.provider == Provider::Direct {
        return http_url(value);
    }
    // Never share download-client credentials, redirect policy, or default headers.
    let client = Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent("RGSX")
        .build()
        .map_err(|_| anyhow::anyhow!("Could not initialize debrid API client"))?;
    resolve_with(
        &client,
        settings,
        value,
        settings.provider.endpoint(),
        Duration::from_secs(2),
    )
    .await
}

fn http_url(value: &str) -> Result<String> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid download URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Expected an HTTP or HTTPS download URL without embedded credentials");
    }
    Ok(url.into())
}

struct Api<'a> {
    client: &'a Client,
    settings: &'a Settings,
    endpoint: &'a str,
}

impl Api<'_> {
    async fn request(&self, method: Method, path: &str, fields: &[(&str, &str)]) -> Result<Value> {
        let mut request = self
            .client
            .request(method.clone(), format!("{}{path}", self.endpoint));
        match self.settings.provider {
            Provider::AllDebrid => {
                request =
                    request.query(&[("agent", "RGSX"), ("apikey", self.settings.api_key.trim())]);
                request = if method == Method::GET {
                    request.query(fields)
                } else {
                    request.form(fields)
                };
            }
            Provider::DebridLink => {
                request = request.bearer_auth(self.settings.api_key.trim()).json(
                    &fields
                        .iter()
                        .copied()
                        .collect::<std::collections::HashMap<_, _>>(),
                );
            }
            Provider::RealDebrid => {
                request = request.bearer_auth(self.settings.api_key.trim());
                if method != Method::GET {
                    request = request.form(fields);
                }
            }
            Provider::Direct => bail!("No debrid provider selected"),
        }
        // Do not propagate reqwest errors: they may contain query-string API keys.
        let mut response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Debrid API request failed (connection or timeout)"))?;
        if !response.status().is_success() {
            bail!("Debrid API returned HTTP {}", response.status().as_u16());
        }
        if response.status() == reqwest::StatusCode::NO_CONTENT
            && self.settings.provider == Provider::RealDebrid
            && path.starts_with("torrents/selectFiles/")
        {
            return Ok(Value::Null);
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            bail!("Debrid API response exceeds size limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Could not read debrid API response"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("Debrid API response exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let result: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Debrid API returned invalid JSON"))?;
        let failed = match self.settings.provider {
            Provider::RealDebrid => {
                result.get("error").is_some() || result.get("error_code").is_some()
            }
            Provider::AllDebrid => result["status"] != "success",
            Provider::DebridLink => result["success"] != true,
            Provider::Direct => true,
        };
        if failed {
            // Provider messages are untrusted and may echo credentials or source URLs.
            bail!("Debrid provider rejected the request; check your API key, account, and source");
        }
        Ok(result)
    }

    async fn unlock(&self, value: &str) -> Result<String> {
        let (method, path, field) = match self.settings.provider {
            Provider::RealDebrid => (Method::POST, "unrestrict/link", "link"),
            Provider::AllDebrid => (Method::GET, "link/unlock", "link"),
            Provider::DebridLink => (Method::POST, "downloader/add", "url"),
            Provider::Direct => return http_url(value),
        };
        let response = self.request(method, path, &[(field, value)]).await?;
        let candidates: &[&str] = match self.settings.provider {
            Provider::RealDebrid => &["/download"],
            Provider::AllDebrid => &["/data/link", "/data/download", "/data/streamingLink"],
            Provider::DebridLink => &[
                "/value/downloadUrl",
                "/value/downloadURL",
                "/value/link",
                "/value/url",
            ],
            Provider::Direct => &[],
        };
        let link = candidates
            .iter()
            .find_map(|path| response.pointer(path).and_then(Value::as_str))
            .ok_or_else(|| {
                anyhow::anyhow!("Debrid provider did not return a ready download URL")
            })?;
        http_url(link)
    }

    async fn real_debrid_magnet(&self, value: &str, poll_delay: Duration) -> Result<String> {
        let added = self
            .request(Method::POST, "torrents/addMagnet", &[("magnet", value)])
            .await?;
        let id = added["id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            })
            .ok_or_else(|| anyhow::anyhow!("Debrid provider returned an invalid torrent ID"))?;
        let mut selected = false;
        for attempt in 0..TORRENT_POLLS {
            let info = self
                .request(Method::GET, &format!("torrents/info/{id}"), &[])
                .await?;
            match info["status"].as_str() {
                Some("downloaded") => {
                    let links = info["links"].as_array().ok_or_else(|| {
                        anyhow::anyhow!("Debrid torrent has no ready download links")
                    })?;
                    if links.len() != 1 {
                        bail!("Debrid torrent contains multiple files; use native torrent downloading");
                    }
                    let link = links[0].as_str().ok_or_else(|| {
                        anyhow::anyhow!("Debrid torrent returned an invalid download link")
                    })?;
                    return self.unlock(&http_url(link)?).await;
                }
                Some("waiting_files_selection") if !selected => {
                    let files = info["files"]
                        .as_array()
                        .filter(|files| files.len() == 1)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                            "Debrid torrent requires file selection; use native torrent downloading"
                        )
                        })?;
                    let file = files[0]["id"]
                        .as_u64()
                        .ok_or_else(|| {
                            anyhow::anyhow!("Debrid torrent returned an invalid file ID")
                        })?
                        .to_string();
                    self.request(
                        Method::POST,
                        &format!("torrents/selectFiles/{id}"),
                        &[("files", &file)],
                    )
                    .await?;
                    selected = true;
                }
                Some("magnet_error" | "error" | "virus" | "dead") => {
                    bail!("Debrid provider could not download this torrent");
                }
                Some(
                    "magnet_conversion"
                    | "waiting_files_selection"
                    | "queued"
                    | "downloading"
                    | "compressing"
                    | "uploading",
                ) => {}
                _ => bail!("Debrid provider returned an unknown torrent status"),
            }
            if attempt + 1 < TORRENT_POLLS {
                tokio::time::sleep(poll_delay).await;
            }
        }
        bail!("Debrid torrent is not ready yet; retry later or use native torrent downloading")
    }
}

async fn resolve_with(
    client: &Client,
    settings: &Settings,
    value: &str,
    endpoint: &str,
    poll_delay: Duration,
) -> Result<String> {
    if settings.provider == Provider::Direct {
        return http_url(value);
    }
    if settings.api_key.trim().is_empty() {
        bail!("Enter an API key for the selected debrid provider");
    }
    let api = Api {
        client,
        settings,
        endpoint,
    };
    if value.starts_with("magnet:") {
        if settings.provider != Provider::RealDebrid {
            bail!("Magnet unrestriction is currently supported only with Real-Debrid; use native torrent downloading");
        }
        let magnet = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid magnet link"))?;
        if !magnet.query_pairs().any(|(key, value)| {
            key == "xt" && (value.starts_with("urn:btih:") || value.starts_with("urn:btmh:"))
        }) {
            bail!("Magnet link has no BitTorrent info hash");
        }
        return api.real_debrid_magnet(value, poll_delay).await;
    }
    api.unlock(&http_url(value)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn mock(
        replies: Vec<(u16, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handle = thread::spawn(move || {
            for (status, body) in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "Missing API request");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0, "Incomplete request");
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map(|value| value.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if bytes.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (endpoint, requests, handle)
    }

    fn settings(provider: Provider) -> Settings {
        Settings {
            provider,
            api_key: "session-secret".into(),
        }
    }

    fn client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn direct_validates_without_network() {
        assert_eq!(
            resolve(
                &client(),
                &Settings::default(),
                "https://files.example/game.zip"
            )
            .await
            .unwrap(),
            "https://files.example/game.zip"
        );
        for value in [
            "file:///secret",
            "ftp://files.example/game",
            "******files.example/",
        ] {
            assert!(resolve(&client(), &Settings::default(), value)
                .await
                .is_err());
        }
        assert_eq!(Provider::ALL.len(), 4);
    }

    #[tokio::test]
    async fn hoster_auth_and_payloads() {
        for (provider, body, path, field) in [
            (
                Provider::RealDebrid,
                r#"{"download":"https://cdn.example/game"}"#,
                "unrestrict/link",
                "link=",
            ),
            (
                Provider::AllDebrid,
                r#"{"status":"success","data":{"link":"https://cdn.example/game"}}"#,
                "link/unlock",
                "link=",
            ),
            (
                Provider::DebridLink,
                r#"{"success":true,"value":{"downloadUrl":"https://cdn.example/game"}}"#,
                "downloader/add",
                r#""url":"https://host.example/game""#,
            ),
        ] {
            let (endpoint, requests, handle) = mock(vec![(200, body.into())]);
            let result = resolve_with(
                &client(),
                &settings(provider),
                "https://host.example/game",
                &endpoint,
                Duration::ZERO,
            )
            .await
            .unwrap();
            handle.join().unwrap();
            assert_eq!(result, "https://cdn.example/game");
            let captured = requests.lock().unwrap();
            let request = &captured[0];
            assert!(request.contains(path));
            assert!(request.contains(field));
            if provider == Provider::AllDebrid {
                assert!(request.contains("apikey=session-secret"));
                assert!(request.contains("agent=RGSX"));
                assert!(request.starts_with("GET "));
            } else {
                assert!(request
                    .to_lowercase()
                    .contains("authorization: bearer session-secret"));
                assert!(!request.lines().next().unwrap().contains("session-secret"));
                assert!(request.starts_with("POST "));
            }
        }
    }

    #[tokio::test]
    async fn errors_never_echo_provider_content_or_credentials() {
        for (provider, status, body) in [
            (
                Provider::AllDebrid,
                200,
                r#"{"status":"error","error":{"message":"session-secret"}}"#,
            ),
            (
                Provider::RealDebrid,
                200,
                r#"{"error":"session-secret","error_code":8}"#,
            ),
            (
                Provider::DebridLink,
                200,
                r#"{"success":false,"error":"session-secret"}"#,
            ),
            (Provider::AllDebrid, 401, "session-secret"),
            (Provider::AllDebrid, 302, "session-secret"),
            (Provider::RealDebrid, 200, "session-secret"),
            (
                Provider::RealDebrid,
                200,
                r#"{"download":"file:///session-secret"}"#,
            ),
            (Provider::DebridLink, 200, r#"{"success":true,"value":{}}"#),
        ] {
            let (endpoint, _, handle) = mock(vec![(status, body.into())]);
            let error = resolve_with(
                &client(),
                &settings(provider),
                "https://host.example/game",
                &endpoint,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
            handle.join().unwrap();
            let error = format!("{error:#}");
            assert!(!error.contains("session-secret"), "{error}");
            assert!(!error.contains(&endpoint), "{error}");
        }
    }

    #[tokio::test]
    async fn real_debrid_magnet_selects_then_unrestricts() {
        let (endpoint, requests, handle) = mock(vec![
            (201, r#"{"id":"ABC123"}"#.into()),
            (
                200,
                r#"{"status":"waiting_files_selection","files":[{"id":7}]}"#.into(),
            ),
            (204, String::new()),
            (
                200,
                r#"{"status":"downloaded","links":["https://host.example/selected"]}"#.into(),
            ),
            (200, r#"{"download":"https://cdn.example/game"}"#.into()),
        ]);
        let result = resolve_with(
            &client(),
            &settings(Provider::RealDebrid),
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            &endpoint,
            Duration::ZERO,
        )
        .await;
        handle.join().unwrap();
        assert_eq!(result.unwrap(), "https://cdn.example/game");
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("magnet=magnet"));
        assert!(requests[2].contains("torrents/selectFiles/ABC123"));
        assert!(requests[2].contains("files=7"));
    }

    #[tokio::test]
    async fn torrent_failures_and_pending_are_not_download_links() {
        for info in [
            r#"{"status":"error"}"#,
            r#"{"status":"unexpected"}"#,
            r#"{"status":"downloaded","links":["https://host.example/a","https://host.example/b"]}"#,
            r#"{"status":"waiting_files_selection","files":[{"id":1},{"id":2}]}"#,
        ] {
            let (endpoint, _, handle) =
                mock(vec![(201, r#"{"id":"ABC"}"#.into()), (200, info.into())]);
            assert!(resolve_with(
                &client(),
                &settings(Provider::RealDebrid),
                "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
                &endpoint,
                Duration::ZERO
            )
            .await
            .is_err());
            handle.join().unwrap();
        }
        let mut replies = vec![(201, r#"{"id":"ABC"}"#.into())];
        replies.extend((0..TORRENT_POLLS).map(|_| (200, r#"{"status":"queued"}"#.into())));
        let (endpoint, requests, handle) = mock(replies);
        let error = resolve_with(
            &client(),
            &settings(Provider::RealDebrid),
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            &endpoint,
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        handle.join().unwrap();
        assert!(error.to_string().contains("not ready"));
        assert_eq!(requests.lock().unwrap().len(), TORRENT_POLLS + 1);
    }

    #[tokio::test]
    async fn rejects_oversized_response_and_missing_key() {
        let (endpoint, _, handle) = mock(vec![(200, "x".repeat(MAX_RESPONSE_BYTES + 1))]);
        let result = resolve_with(
            &client(),
            &settings(Provider::RealDebrid),
            "https://host.example/game",
            &endpoint,
            Duration::ZERO,
        )
        .await;
        // The client may close the connection immediately on observing Content-Length.
        let _ = handle.join();
        assert!(result.unwrap_err().to_string().contains("size limit"));
        let settings = Settings {
            provider: Provider::AllDebrid,
            api_key: String::new(),
        };
        assert!(resolve_with(
            &client(),
            &settings,
            "https://host.example/game",
            "http://127.0.0.1:1/",
            Duration::ZERO
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("API key"));
    }
}
