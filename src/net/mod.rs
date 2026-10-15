//! HTTP client integration used by remote image loading.

mod images;
pub(crate) mod update;

pub(crate) use images::remote_image;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures::AsyncReadExt;
use futures::FutureExt;
use futures::channel::oneshot;
use gpui::App;
use gpui::http_client::{self, AsyncBody, HttpClient};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, USER_AGENT};

/// Preferred image response formats for requests without an explicit Accept header.
const DEFAULT_IMAGE_ACCEPT: &str =
    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
/// Browser-like language preferences used when the caller provides none.
const DEFAULT_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en-US;q=0.8,en;q=0.7";
/// Installs the image HTTP bridge, retaining GPUI's client if construction fails.
pub(crate) fn install_http_client(cx: &mut App) {
    match ReqwestTransportHttpClient::new(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
    ) {
        Ok(client) => cx.set_http_client(Arc::new(client)),
        Err(error) => eprintln!("failed to install HTTP client for image loading: {error}"),
    }
}

/// Returns whether the source parses as a URI with an HTTP or HTTPS scheme.
pub(crate) fn is_remote_image_source(source: &str) -> bool {
    http_client::Uri::from_str(source)
        .ok()
        .and_then(|uri| uri.scheme_str().map(str::to_owned))
        .is_some_and(|scheme| scheme == "http" || scheme == "https")
}

/// Shares an async connection pool and a small runtime across remote image loads.
struct ReqwestTransportHttpClient {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    /// User-agent value exposed through GPUI's client interface.
    user_agent: HeaderValue,
    /// Image-fetch defaults that do not override explicit request headers.
    default_headers: HeaderMap,
}

impl ReqwestTransportHttpClient {
    /// Bounds stalled image requests without forcing CDN cache revalidation.
    fn new(user_agent: &str) -> anyhow::Result<Self> {
        let default_headers = default_image_request_headers(user_agent)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("velotype-images")
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent(user_agent)
            .default_headers(default_headers.clone())
            .build()?;
        Ok(Self {
            client,
            runtime,
            user_agent: HeaderValue::from_str(user_agent)?,
            default_headers,
        })
    }

    /// Bridges HTTP metadata and bodies without blocking a GPUI executor thread.
    ///
    /// HTTP error statuses are returned as responses rather than transport errors.
    async fn execute_request(
        client: reqwest::Client,
        default_headers: HeaderMap,
        request: http_client::Request<AsyncBody>,
    ) -> anyhow::Result<http_client::Response<AsyncBody>> {
        let (parts, mut body) = request.into_parts();
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
        let url = parts.uri.to_string();
        let mut body_bytes = Vec::new();
        body.read_to_end(&mut body_bytes).await?;

        let mut builder = apply_missing_default_headers(
            client.request(method, url),
            &parts.headers,
            &default_headers,
        );
        if let Some(timeout) = parts.extensions.get::<http_client::RequestTimeout>() {
            builder = builder.timeout(timeout.0);
        }
        if !body_bytes.is_empty() {
            builder = builder.body(body_bytes);
        }

        let response = builder.send().await?;
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;

        let mut response_builder = http_client::Response::builder()
            .status(status)
            .version(version);
        for (name, value) in &headers {
            response_builder = response_builder.header(name, value);
        }
        Ok(response_builder.body(AsyncBody::from(bytes))?)
    }
}

/// Builds browser-like image headers, rejecting an invalid user-agent value.
fn default_image_request_headers(user_agent: &str) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_str(user_agent)?);
    headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_IMAGE_ACCEPT));
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(DEFAULT_ACCEPT_LANGUAGE),
    );
    Ok(headers)
}

/// Applies caller headers and fills only header names missing from the request.
fn apply_missing_default_headers(
    builder: reqwest::RequestBuilder,
    request_headers: &HeaderMap,
    default_headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut headers = request_headers.clone();
    for (name, value) in default_headers {
        if !headers.contains_key(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    builder.headers(headers)
}

impl HttpClient for ReqwestTransportHttpClient {
    /// Exposes the user-agent configured when the bridge was created.
    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    /// Multiplexes requests on the shared runtime instead of spawning a thread per image.
    fn send(
        &self,
        request: http_client::Request<AsyncBody>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<http_client::Response<AsyncBody>>> {
        let client = self.client.clone();
        let default_headers = self.default_headers.clone();
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(Self::execute_request(client, default_headers, request).await);
        });
        async move {
            rx.await
                .map_err(|_| anyhow::anyhow!("image HTTP worker dropped before responding"))?
        }
        .boxed()
    }

    /// Exposes no GPUI proxy override; reqwest retains its own proxy behavior.
    fn proxy(&self) -> Option<&http_client::Url> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ACCEPT_LANGUAGE, DEFAULT_IMAGE_ACCEPT, apply_missing_default_headers,
        default_image_request_headers, is_remote_image_source,
    };
    use reqwest::header::{
        ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, HOST,
        HeaderMap, HeaderValue, PRAGMA, USER_AGENT,
    };

    const TEST_USER_AGENT: &str = "VelotypeTest/1.0";

    #[test]
    fn image_http_bridge_decodes_gzip_responses() {
        use futures::AsyncReadExt;
        use gpui::http_client::{AsyncBody, HttpClient};
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            assert!(request.to_lowercase().contains("accept-encoding: gzip"));
            assert!(!request.to_lowercase().contains("cache-control: no-cache"));
            let mut compressed =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            compressed.write_all(b"<svg>image data</svg>").unwrap();
            let compressed = compressed.finish().unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).unwrap();
            stream.write_all(&compressed).unwrap();
        });
        let client = super::ReqwestTransportHttpClient::new(TEST_USER_AGENT).unwrap();
        futures::executor::block_on(async {
            let mut response = client
                .get(
                    &format!("http://{address}/badge.svg"),
                    AsyncBody::default(),
                    true,
                )
                .await
                .unwrap();
            let mut bytes = Vec::new();
            response.body_mut().read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"<svg>image data</svg>");
            assert!(
                !response
                    .headers()
                    .contains_key(reqwest::header::CONTENT_ENCODING)
            );
        });
        server.join().unwrap();
    }

    #[test]
    fn image_http_bridge_honors_request_deadlines() {
        use gpui::http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let client = super::ReqwestTransportHttpClient::new(TEST_USER_AGENT).unwrap();
        let request = Request::builder()
            .uri(format!("http://{address}/slow.svg"))
            .timeout(Duration::from_millis(30))
            .body(AsyncBody::default())
            .unwrap();
        let error = futures::executor::block_on(client.send(request))
            .err()
            .expect("the request should time out");
        assert!(
            error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout)
        );
        server.join().unwrap();
    }

    #[test]
    fn default_image_headers_include_browser_like_fetch_context() {
        let headers = default_image_request_headers(TEST_USER_AGENT).expect("headers");

        assert_eq!(headers.get(USER_AGENT).unwrap(), TEST_USER_AGENT);
        assert_eq!(headers.get(ACCEPT).unwrap(), DEFAULT_IMAGE_ACCEPT);
        assert_eq!(
            headers.get(ACCEPT_LANGUAGE).unwrap(),
            DEFAULT_ACCEPT_LANGUAGE
        );
        assert!(!headers.contains_key(CACHE_CONTROL));
        assert!(!headers.contains_key(PRAGMA));
    }

    #[test]
    fn default_image_headers_leave_transport_managed_headers_unset() {
        let headers = default_image_request_headers(TEST_USER_AGENT).expect("headers");

        assert!(!headers.contains_key(ACCEPT_ENCODING));
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key(CONTENT_LENGTH));
        assert!(!headers.contains_key(HOST));
    }

    #[test]
    fn explicit_request_headers_override_default_image_headers() {
        let defaults = default_image_request_headers(TEST_USER_AGENT).expect("headers");
        let mut request_headers = HeaderMap::new();
        request_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        request_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-GB"));
        request_headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));

        let request = apply_missing_default_headers(
            reqwest::Client::new().get("https://example.com/image.png"),
            &request_headers,
            &defaults,
        )
        .build()
        .expect("request should build");
        let headers = request.headers();

        assert_eq!(headers.get(ACCEPT).unwrap(), "application/json");
        assert_eq!(headers.get(ACCEPT_LANGUAGE).unwrap(), "en-GB");
        assert_eq!(headers.get(USER_AGENT).unwrap(), TEST_USER_AGENT);
        assert_eq!(headers.get(CACHE_CONTROL).unwrap(), "no-cache");
        assert!(!headers.contains_key(PRAGMA));
    }

    #[test]
    fn detects_remote_http_sources() {
        assert!(is_remote_image_source("https://example.com/image.png"));
        assert!(is_remote_image_source("http://example.com/image.gif"));
        assert!(!is_remote_image_source("./image.png"));
        assert!(!is_remote_image_source("images/photo.jpg"));
    }
}
