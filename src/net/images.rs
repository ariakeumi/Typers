//! Recovery for transient remote-image failures without invalidating successful GPUI assets.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, EntityId, Global, ImageCacheError, Img, ImgResourceLoader, RenderImage, Resource,
    SharedUri, Window, img,
};

const MAX_RETRIES: u8 = 2;
const RETRY_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_FAILED_IMAGES: usize = 256;

/// Tracks only failures; successful and pending image data stay in GPUI's shared asset cache.
#[derive(Default)]
struct ImageRetries {
    entries: HashMap<SharedUri, RetryEntry>,
    next_ticket: u64,
}

impl Global for ImageRetries {}

/// A single backoff shared by every view displaying the same failed URL.
struct RetryEntry {
    attempts: u8,
    waiting: bool,
    last_failure: Instant,
    ticket: u64,
    views: HashSet<EntityId>,
}

/// Keeps the standard image decoder/cache and retries only the affected remote resource.
pub(crate) fn remote_image(uri: SharedUri) -> Img {
    img(move |window: &mut Window, cx: &mut App| load_remote_image(&uri, window, cx))
}

fn load_remote_image(
    uri: &SharedUri,
    window: &mut Window,
    cx: &mut App,
) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
    let resource = Resource::Uri(uri.clone());
    let result = window.use_asset::<ImgResourceLoader>(&resource, cx);
    if matches!(&result, Some(Ok(_))) {
        if cx.try_global::<ImageRetries>().is_some() {
            cx.global_mut::<ImageRetries>().entries.remove(uri);
        }
    } else if let Some(Err(error)) = &result {
        let retryable = match error {
            ImageCacheError::BadStatus { status, .. } => {
                status.is_server_error() || matches!(status.as_u16(), 408 | 429)
            }
            ImageCacheError::Asset(_) => false,
            _ => true,
        };
        if retryable && schedule_retry(uri, window.current_view(), cx) {
            return None;
        }
    }
    result
}

/// Coalesces retry timers across views and stops after two attempts until a later interaction.
fn schedule_retry(uri: &SharedUri, view: EntityId, cx: &mut App) -> bool {
    if cx.try_global::<ImageRetries>().is_none() {
        cx.set_global(ImageRetries::default());
    }
    let retries = cx.global_mut::<ImageRetries>();
    if !retries.entries.contains_key(uri) && retries.entries.len() >= MAX_FAILED_IMAGES {
        let oldest = retries
            .entries
            .iter()
            .filter(|(_, entry)| !entry.waiting && entry.last_failure.elapsed() >= RETRY_COOLDOWN)
            .min_by_key(|(_, entry)| entry.last_failure)
            .map(|(uri, _)| uri.clone());
        if let Some(oldest) = oldest {
            retries.entries.remove(&oldest);
        } else {
            return false;
        }
    }
    let entry = retries
        .entries
        .entry(uri.clone())
        .or_insert_with(|| RetryEntry {
            attempts: 0,
            waiting: false,
            last_failure: Instant::now(),
            ticket: 0,
            views: HashSet::new(),
        });
    if entry.waiting {
        entry.views.insert(view);
        return true;
    }
    if entry.attempts >= MAX_RETRIES {
        if entry.last_failure.elapsed() < RETRY_COOLDOWN {
            return false;
        }
        entry.attempts = 0;
    }
    entry.attempts += 1;
    entry.waiting = true;
    entry.last_failure = Instant::now();
    entry.views.insert(view);
    retries.next_ticket += 1;
    entry.ticket = retries.next_ticket;
    let ticket = entry.ticket;
    let delay = Duration::from_millis(500 * u64::from(entry.attempts));
    let uri = uri.clone();
    let timer = cx.background_executor().timer(delay);
    cx.spawn(async move |cx| {
        timer.await;
        cx.update(|cx| {
            if cx.try_global::<ImageRetries>().is_none() {
                return;
            }
            let Some(entry) = cx
                .global_mut::<ImageRetries>()
                .entries
                .get_mut(&uri)
                .filter(|entry| entry.ticket == ticket)
            else {
                return;
            };
            entry.waiting = false;
            let views = std::mem::take(&mut entry.views);
            let resource = Resource::Uri(uri);
            // A newer success must never be discarded by an older retry timer.
            if matches!(cx.fetch_asset::<ImgResourceLoader>(&resource), Some(Err(_))) {
                cx.remove_asset::<ImgResourceLoader>(&resource);
            }
            for view in views {
                cx.notify(view);
            }
        });
    })
    .detach();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::http_client::{AsyncBody, FakeHttpClient, Response};
    use gpui::{
        Context, IntoElement, ParentElement, Render, Styled, TestAppContext, VisualTestContext,
        div, px,
    };
    use std::sync::Mutex;

    const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"80\" height=\"20\"><rect width=\"80\" height=\"20\" fill=\"red\"/></svg>";

    struct ImageView {
        visible: bool,
    }

    impl Render for ImageView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let mut view = div().w_full().h_full();
            if self.visible {
                for url in [
                    "https://test.example/good.svg",
                    "https://test.example/bad.svg",
                ] {
                    view = view.child(remote_image(url.into()).w(px(80.0)).h(px(20.0)));
                }
            }
            view
        }
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
    }

    fn cached_bad(cx: &mut VisualTestContext) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        cx.update(|_, cx| {
            cx.fetch_asset::<ImgResourceLoader>(&Resource::Uri(
                "https://test.example/bad.svg".into(),
            ))
        })
    }

    #[gpui::test]
    async fn remote_images_recover_failures_without_refetching_successes(cx: &mut TestAppContext) {
        let counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        cx.update(|cx| {
            let counts = counts.clone();
            cx.set_http_client(FakeHttpClient::create(move |request| {
                let path = request.uri().path().to_string();
                let mut counts = counts.lock().unwrap();
                let count = counts.entry(path.clone()).or_default();
                *count += 1;
                let fail = path == "/bad.svg" && *count == 1;
                async move {
                    Ok(Response::builder()
                        .status(if fail { 503 } else { 200 })
                        .body(AsyncBody::from(SVG))?)
                }
            }));
        });
        let (view, cx) = cx.add_window_view(|_, _| ImageView { visible: true });
        redraw(cx);
        redraw(cx);
        assert!(matches!(cached_bad(cx), Some(Err(_))));
        assert_eq!(counts.lock().unwrap()["/bad.svg"], 1);
        cx.cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        redraw(cx);
        redraw(cx);
        assert!(matches!(cached_bad(cx), Some(Ok(_))));
        for visible in [false, true, false, true] {
            view.update(cx, |view, cx| {
                view.visible = visible;
                cx.notify();
            });
            redraw(cx);
        }
        let counts = counts.lock().unwrap();
        assert_eq!(counts["/good.svg"], 1);
        assert_eq!(counts["/bad.svg"], 2);
    }

    #[gpui::test]
    async fn delayed_retry_cannot_invalidate_a_newer_success(cx: &mut TestAppContext) {
        let requests = Arc::new(Mutex::new(0));
        cx.update(|cx| {
            let requests = requests.clone();
            cx.set_http_client(FakeHttpClient::create(move |request| {
                let mut count = requests.lock().unwrap();
                let fail = if request.uri().path() == "/bad.svg" {
                    *count += 1;
                    *count == 1
                } else {
                    false
                };
                async move {
                    Ok(Response::builder()
                        .status(if fail { 503 } else { 200 })
                        .body(AsyncBody::from(SVG))?)
                }
            }));
        });
        let (_, cx) = cx.add_window_view(|_, _| ImageView { visible: true });
        redraw(cx);
        redraw(cx);
        assert!(matches!(cached_bad(cx), Some(Err(_))));
        cx.update(|_, cx| {
            cx.remove_asset::<ImgResourceLoader>(&Resource::Uri(
                "https://test.example/bad.svg".into(),
            ))
        });
        redraw(cx);
        redraw(cx);
        assert!(matches!(cached_bad(cx), Some(Ok(_))));
        cx.cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        redraw(cx);
        assert!(matches!(cached_bad(cx), Some(Ok(_))));
        assert_eq!(*requests.lock().unwrap(), 2);
    }

    #[gpui::test]
    async fn failed_image_budget_does_not_restart_recent_retry_cycles(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_, _| ImageView { visible: false });
        let view = view.entity_id();
        cx.update(|_, cx| {
            for index in 0..MAX_FAILED_IMAGES {
                assert!(schedule_retry(
                    &format!("https://test.example/{index}.svg").into(),
                    view,
                    cx
                ));
            }
            assert!(!schedule_retry(
                &"https://test.example/overflow.svg".into(),
                view,
                cx
            ));
            let retries = cx.global_mut::<ImageRetries>();
            for entry in retries.entries.values_mut() {
                entry.waiting = false;
                entry.attempts = MAX_RETRIES;
            }
            assert!(!schedule_retry(
                &"https://test.example/overflow.svg".into(),
                view,
                cx
            ));
            assert!(!schedule_retry(
                &"https://test.example/0.svg".into(),
                view,
                cx
            ));
            assert_eq!(cx.global::<ImageRetries>().entries.len(), MAX_FAILED_IMAGES);
        });
    }

    #[gpui::test]
    async fn remote_image_retries_are_bounded_and_not_found_is_not_retried(
        cx: &mut TestAppContext,
    ) {
        let counts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        cx.update(|cx| {
            let counts = counts.clone();
            cx.set_http_client(FakeHttpClient::create(move |request| {
                let path = request.uri().path().to_string();
                *counts.lock().unwrap().entry(path.clone()).or_default() += 1;
                async move {
                    Ok(Response::builder()
                        .status(if path == "/bad.svg" { 503 } else { 404 })
                        .body(AsyncBody::from("unavailable"))?)
                }
            }));
        });
        let (_, cx) = cx.add_window_view(|_, _| ImageView { visible: true });
        redraw(cx);
        redraw(cx);
        for delay in [500, 1000, 2000] {
            cx.cx.executor().advance_clock(Duration::from_millis(delay));
            cx.run_until_parked();
            redraw(cx);
            redraw(cx);
        }
        assert!(matches!(cached_bad(cx), Some(Err(_))));
        let counts = counts.lock().unwrap();
        assert_eq!(counts["/good.svg"], 1);
        assert_eq!(counts["/bad.svg"], 3);
    }
}
