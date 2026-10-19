//! Localhost endpoint the browser extension posts downloads to.

use crate::manager::Manager;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;

pub const PORT: u16 = 47219;

#[derive(Deserialize)]
struct Add {
    url: String,
    referrer: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
}

pub async fn serve(m: Manager, listener: TcpListener) -> std::io::Result<()> {
    axum::serve(listener, Router::new().route("/add", post(add)).with_state(m)).await
}

async fn add(State(m): State<Manager>, h: HeaderMap, Json(a): Json<Add>) -> StatusCode {
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
    m.add(a.url, headers);
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Handle;

    #[tokio::test(flavor = "multi_thread")]
    async fn only_extensions_can_add() {
        let dir = std::env::temp_dir().join(format!("idm-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let m = Manager::new(Handle::current(), dir.clone(), dir.join("db.json"), 1, 1).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/add", listener.local_addr().unwrap());
        tokio::spawn(serve(m.clone(), listener));

        let body = serde_json::json!({ "url": "http://127.0.0.1:1/x.bin", "cookies": "a=b" });
        let post = |origin: &'static str| reqwest::Client::new().post(&url).header("Origin", origin).json(&body).send();
        assert_eq!(post("https://evil.example").await.unwrap().status(), 403);
        assert!(m.items().is_empty());
        assert_eq!(post("chrome-extension://abc").await.unwrap().status(), 200);
        let items = m.items();
        assert!(items.len() == 1 && items[0].headers == [("Cookie".to_string(), "a=b".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
