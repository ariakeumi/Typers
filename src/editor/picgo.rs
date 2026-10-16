//! Optional PicGo Server transport for pasted local images.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use gpui::ImageFormat;
use serde_json::json;
use url::Url;

use crate::components::PastedImageSource;

/// Accept only an HTTP loopback origin, never a remote host or user-provided path.
pub(crate) fn server_url(input: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(input.trim()).context("invalid PicGo Server address")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || url.username() != ""
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("PicGo Server must be an HTTP loopback address without a path or credentials");
    }
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| anyhow!("invalid PicGo loopback address"))?;
    }
    Ok(url)
}

/// Probes PicGo without uploading. A 404/405 permits older servers to try upload.
pub(crate) fn probe_server(input: &str) -> anyhow::Result<bool> {
    let mut url = server_url(input)?;
    url.set_path("/heartbeat");
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    let response = client
        .post(url)
        .send()
        .context("PicGo Server is unreachable")?;
    if matches!(response.status().as_u16(), 404 | 405) {
        return Ok(false);
    }
    if !response.status().is_success() {
        bail!("PicGo Server heartbeat returned {}", response.status());
    }
    let reply: serde_json::Value =
        serde_json::from_slice(&response.bytes()?).context("invalid PicGo heartbeat")?;
    if reply.get("success").and_then(|value| value.as_bool()) == Some(true)
        && reply.get("result").and_then(|value| value.as_str()) == Some("alive")
    {
        Ok(true)
    } else {
        bail!("unexpected PicGo heartbeat response")
    }
}

/// Owns temporary clipboard images until PicGo has read their paths.
struct TempImages(PathBuf);

impl Drop for TempImages {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn image_extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Ico => "ico",
        ImageFormat::Pnm => "pnm",
    }
}

fn upload_paths(
    sources: &[PastedImageSource],
) -> anyhow::Result<(Vec<PathBuf>, Option<TempImages>)> {
    let mut temporary = None;
    let mut paths = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        match source {
            PastedImageSource::LocalPath(path) => paths.push(path.clone()),
            PastedImageSource::ClipboardImage(image) => {
                if temporary.is_none() {
                    let dir = std::env::temp_dir()
                        .join(format!("velotype-picgo-{}", uuid::Uuid::new_v4()));
                    fs::create_dir(&dir)
                        .with_context(|| format!("failed to create '{}'", dir.display()))?;
                    temporary = Some(TempImages(dir));
                }
                let path = temporary
                    .as_ref()
                    .expect("directory created")
                    .0
                    .join(format!("pasted-{index}.{}", image_extension(image.format)));
                fs::write(&path, &image.bytes)
                    .with_context(|| format!("failed to write '{}'", path.display()))?;
                paths.push(path);
            }
        }
    }
    Ok((paths, temporary))
}

/// Sends one paste batch in one HTTP request, accepting only complete URL results.
pub(crate) async fn upload_batch(
    input: &str,
    sources: &[PastedImageSource],
) -> anyhow::Result<Vec<String>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let mut url = server_url(input)?;
    url.set_path("/upload");
    let (paths, _temporary) = upload_paths(sources)?;
    let list = paths
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&json!({ "list": list }))?)
        .send()
        .await
        .context("PicGo upload request failed")?;
    if !response.status().is_success() {
        bail!("PicGo upload returned {}", response.status());
    }
    parse_upload_response(&response.bytes().await?, sources.len())
}

/// Rejects partial batches so fallback never mixes remote and local images.
fn parse_upload_response(bytes: &[u8], expected: usize) -> anyhow::Result<Vec<String>> {
    let body: serde_json::Value =
        serde_json::from_slice(bytes).context("invalid PicGo upload response")?;
    if body.get("success").and_then(|value| value.as_bool()) != Some(true) {
        bail!("PicGo reported upload failure");
    }
    let results = body
        .get("result")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow!("PicGo returned no image links"))?;
    if results.len() != expected {
        bail!("PicGo returned an incomplete image batch");
    }
    results
        .iter()
        .map(|value| {
            let link = value
                .as_str()
                .ok_or_else(|| anyhow!("PicGo returned an invalid link"))?;
            if link.chars().any(char::is_control) {
                bail!("PicGo returned an image URL containing control characters");
            }
            let parsed = Url::parse(link).context("PicGo returned an invalid image URL")?;
            if !matches!(parsed.scheme(), "http" | "https") {
                bail!("PicGo returned a non-HTTP image URL");
            }
            Ok(parsed.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn endpoint_is_loopback_only() {
        assert!(server_url("http://127.0.0.1:36677").is_ok());
        assert!(server_url("http://localhost:36677/").is_ok());
        assert!(server_url("http://[::1]:36677").is_ok());
        for invalid in [
            "https://127.0.0.1:36677",
            "http://example.com",
            "http://127.0.0.1:36677/upload",
            "http://localhost:36677/?secret=x",
        ] {
            assert!(server_url(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn clipboard_files_are_cleaned_up() {
        let source = PastedImageSource::ClipboardImage(gpui::Image {
            format: ImageFormat::Png,
            bytes: vec![1, 2, 3].into(),
            id: 1,
        });
        let (paths, temporary) = upload_paths(&[source]).unwrap();
        assert!(paths[0].exists());
        drop(temporary);
        assert!(!paths[0].exists());
    }

    #[test]
    fn incomplete_or_invalid_batches_are_rejected() {
        let complete = br#"{"success":true,"result":["https://example.com/a.png","https://example.com/b.png"]}"#;
        assert_eq!(parse_upload_response(complete, 2).unwrap().len(), 2);
        assert!(parse_upload_response(complete, 3).is_err());
        assert!(parse_upload_response(br#"{"success":false,"result":[]}"#, 1).is_err());
        assert!(
            parse_upload_response(br#"{"success":true,"result":["file:///tmp/x"]}"#, 1).is_err()
        );
        assert_eq!(
            parse_upload_response(
                br#"{"success":true,"result":["https://example.com/a b.png"]}"#,
                1
            )
            .unwrap(),
            vec!["https://example.com/a%20b.png"]
        );
    }

    #[test]
    fn uploads_a_batch_to_one_local_request() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(10013) =>
            {
                return;
            }
            Err(error) => panic!("loopback test setup failed: {error}"),
        };
        let socket_address = listener.local_addr().unwrap();
        match TcpStream::connect(socket_address) {
            Ok(client) => {
                let (server, _) = listener.accept().unwrap();
                drop(client);
                drop(server);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(10013) =>
            {
                return;
            }
            Err(error) => panic!("loopback test setup failed: {error}"),
        }
        let address = format!("http://{socket_address}");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            assert!(request_line.starts_with("POST /upload HTTP/1.1"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(list["list"].as_array().unwrap().len(), 2);
            let reply = br#"{"success":true,"result":["https://example.com/one.png","https://example.com/two.png"]}"#;
            let stream = reader.get_mut();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len()).unwrap();
            stream.write_all(reply).unwrap();
        });
        let root =
            std::env::temp_dir().join(format!("velotype-picgo-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let paths = [root.join("one.png"), root.join("two.png")];
        for path in &paths {
            fs::write(path, b"image").unwrap();
        }
        let sources = paths
            .iter()
            .cloned()
            .map(PastedImageSource::LocalPath)
            .collect::<Vec<_>>();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(upload_batch(&address, &sources)).unwrap();
        assert_eq!(
            result,
            vec!["https://example.com/one.png", "https://example.com/two.png"]
        );
        server.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
