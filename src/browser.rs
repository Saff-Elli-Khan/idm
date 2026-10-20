//! Localhost endpoint: the browser extension posts downloads to `/add`, and a second app launch
//! posts to `/show` to bring up the already-running window instead of starting a duplicate.

use crate::manager::Manager;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use tokio::net::TcpListener;

pub const PORT: u16 = 47219;

#[derive(Deserialize)]
struct Add {
    url: String,
    referrer: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
}

#[derive(Clone)]
struct App {
    m: Manager,
    show: Arc<dyn Fn() + Send + Sync>,
}

pub async fn serve(m: Manager, listener: TcpListener, show: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<()> {
    let app = Router::new().route("/add", post(add)).route("/show", post(show_window)).with_state(App { m, show });
    axum::serve(listener, app).await
}

/// Asks an already-running instance to show its window, and to add `open` (a magnet link or
/// .torrent path the OS launched us with). `true` if one answered.
pub async fn show_running(open: Option<&str>) -> bool {
    let url = format!("http://127.0.0.1:{PORT}/show");
    let body = open.unwrap_or_default().to_string();
    reqwest::Client::new().post(url).body(body).send().await.is_ok_and(|r| r.status().is_success())
}

async fn add(State(app): State<App>, h: HeaderMap, Json(a): Json<Add>) -> StatusCode {
    // Only extensions may add downloads; any web page can reach localhost, but it can't fake its Origin.
    // (Json also rejects non-JSON bodies, so a page can't sneak in a preflight-free form POST.)
    let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !(origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")) {
        return StatusCode::FORBIDDEN;
    }
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        return StatusCode::BAD_REQUEST;
    }
    let headers = [("Referer", a.referrer), ("Cookie", a.cookies), ("User-Agent", a.user_agent)]
        .into_iter()
        .filter_map(|(k, v)| Some((k.to_string(), v.filter(|v| !v.is_empty())?)))
        .collect();
    app.m.add(a.url, headers);
    (app.show)(); // the browser shows nothing once we take the download over, so let the user see it started
    StatusCode::OK
}

async fn show_window(State(app): State<App>, h: HeaderMap, open: String) -> StatusCode {
    // Browsers always send an Origin on cross-site POSTs; our own second launch doesn't.
    // So web pages can't pop the window open or add torrents.
    if h.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN;
    }
    if crate::torrent::is_torrent(&open) {
        app.m.add(open, vec![]);
    }
    (app.show)();
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::runtime::Handle;

    #[tokio::test(flavor = "multi_thread")]
    async fn only_extensions_can_add_and_only_the_app_can_show() {
        let dir = std::env::temp_dir().join(format!("idm-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Manager::new(Handle::current(), dir.clone(), dir.join("db.json"), 1, 1).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let shown = Arc::new(AtomicUsize::new(0));
        let s = shown.clone();
        tokio::spawn(serve(m.clone(), listener, Arc::new(move || {
            s.fetch_add(1, Ordering::Relaxed);
        })));

        let body = serde_json::json!({ "url": "http://127.0.0.1:1/x.bin", "cookies": "a=b" });
        let add = |origin: &'static str| reqwest::Client::new().post(format!("{base}/add")).header("Origin", origin).json(&body).send();
        assert_eq!(add("https://evil.example").await.unwrap().status(), 403);
        assert!(m.items().is_empty());
        assert_eq!(shown.load(Ordering::Relaxed), 0, "a rejected request must not pop the window");
        assert_eq!(add("chrome-extension://abc").await.unwrap().status(), 200);
        let items = m.items();
        assert!(items.len() == 1 && items[0].headers == [("Cookie".to_string(), "a=b".to_string())]);
        assert_eq!(shown.load(Ordering::Relaxed), 1, "an accepted browser download shows the window");

        let show = || reqwest::Client::new().post(format!("{base}/show"));
        assert_eq!(show().header("Origin", "https://evil.example").send().await.unwrap().status(), 403);
        assert_eq!(show().send().await.unwrap().status(), 200);
        assert_eq!(shown.load(Ordering::Relaxed), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
