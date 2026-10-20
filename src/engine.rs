//! Segmented, resumable HTTP downloader.
//!
//! The file is preallocated and every connection writes straight to its own offset, so there is
//! no merge step at the end. When a connection finishes its range it steals the back half of the
//! largest remaining one (dynamic segmentation), which keeps all connections busy until the last byte.

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// Never split a range into halves smaller than this.
const MIN_SPLIT: u64 = 1 << 20;
/// Consecutive failures without progress before a connection gives up.
const RETRIES: u32 = 5;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Info {
    /// Final URL after redirects.
    pub url: String,
    pub total: Option<u64>,
    pub resumable: bool,
    /// ETag or Last-Modified; a mismatch on resume means the remote file changed.
    pub validator: Option<String>,
    pub filename: String,
}

pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    pub conns: usize,
    pub segs: Vec<SegView>,
}

/// One range of the file: `[start, pos)` is downloaded, `[pos, end)` is left.
#[derive(Clone, Debug, PartialEq)]
pub struct SegView {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
    /// A connection is working on it right now.
    pub active: bool,
    /// Filled in by the caller; the engine doesn't track per-range speed.
    pub speed: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Seg {
    pos: u64,
    end: u64,
    #[serde(skip)]
    active: bool,
}

#[derive(Serialize, Deserialize)]
struct State {
    info: Info,
    segs: Vec<Seg>,
}

/// Global bandwidth cap shared by every connection of every download. 0 = unlimited.
pub struct Limiter {
    rate: AtomicU64,
    next: Mutex<Instant>,
}

impl Limiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Limiter { rate: AtomicU64::new(bytes_per_sec), next: Mutex::new(Instant::now()) }
    }

    pub fn set(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.rate.load(Relaxed)
    }

    /// Waits until `n` more bytes fit under the cap. While we wait we don't read the socket,
    /// so TCP flow control slows the server down too.
    async fn take(&self, n: usize) {
        let rate = self.rate.load(Relaxed);
        if rate == 0 {
            return;
        }
        let wait = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + Duration::from_secs_f64(n as f64 / rate as f64);
            start - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// `headers` (e.g. Cookie, Referer from the browser) are sent with every request.
pub fn client(headers: &[(String, String)]) -> Result<Client> {
    let mut map = header::HeaderMap::new();
    // Honest UA by default: bot filters drop clients that claim to be Chrome but don't look like it.
    // Browser downloads override it with the browser's own UA (cookies can be tied to it).
    map.insert(header::USER_AGENT, header::HeaderValue::from_static(concat!("idm/", env!("CARGO_PKG_VERSION"))));
    for (k, v) in headers {
        map.insert(header::HeaderName::try_from(k.as_str())?, header::HeaderValue::try_from(v.as_str())?);
    }
    Ok(Client::builder()
        .default_headers(map)
        // HTTP/2 would multiplex every segment over one TCP connection; the speed comes from many.
        .http1_only()
        .connect_timeout(Duration::from_secs(15))
        // A stalled connection errors out instead of hanging; its range gets retried or stolen.
        .read_timeout(Duration::from_secs(30))
        .build()?)
}

pub async fn probe(client: &Client, url: &str) -> Result<Info> {
    let resp = client.get(url).header(header::RANGE, "bytes=0-0").send().await?.error_for_status()?;
    let h = resp.headers();
    let get = |k: header::HeaderName| h.get(k).and_then(|v| v.to_str().ok());
    let partial = resp.status() == StatusCode::PARTIAL_CONTENT;
    let total = if partial {
        get(header::CONTENT_RANGE).and_then(|v| v.rsplit('/').next()?.parse().ok())
    } else {
        resp.content_length()
    };
    let name = get(header::CONTENT_DISPOSITION)
        .and_then(filename_from_cd)
        .unwrap_or_else(|| filename_from_url(resp.url()));
    Ok(Info {
        url: resp.url().to_string(),
        total,
        resumable: partial && total.is_some(),
        validator: get(header::ETAG).or_else(|| get(header::LAST_MODIFIED)).map(String::from),
        filename: sanitize(&name),
    })
}

/// Downloads to `out`, resuming from `out.part` if a matching state file exists.
/// Returns `false` if stopped early (state is saved; call again to resume).
/// `conns` may change while running: raising it opens new connections within a second,
/// lowering it closes connections as they finish their current range.
pub async fn download(
    client: &Client,
    info: Info,
    out: &Path,
    conns: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    limit: Arc<Limiter>,
    on_progress: impl Fn(Progress),
) -> Result<bool> {
    let Some(total) = info.total.filter(|_| info.resumable) else {
        return single(client, &info, out, &stop, &limit, on_progress).await;
    };
    let (part, meta) = (with_ext(out, ".part"), with_ext(out, ".part.json"));
    let saved = fs::read(&meta)
        .ok()
        .and_then(|b| serde_json::from_slice::<State>(&b).ok())
        .filter(|s| s.info.total == info.total && s.info.validator == info.validator && part.exists());
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(saved.is_none())
        .open(&part)
        .with_context(|| format!("can't open {}", part.display()))?;
    let segs = match saved {
        Some(s) => s.segs,
        None => {
            file.set_len(total)?; // fail on a full disk now, not at 90%
            split(total, conns.load(Relaxed))
        }
    };

    let sh = Arc::new(Shared { client: client.clone(), info, file, segs: Mutex::new(segs), stop, limit, conns });
    let mut set = JoinSet::new();
    let mut spawned = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_err = None;
    loop {
        // Only spawn when the target goes up, so connections the server refused aren't retried every tick.
        let want = sh.conns.load(Relaxed).max(1);
        for _ in spawned..want {
            set.spawn(worker(sh.clone()));
        }
        spawned = want;
        tokio::select! {
            r = set.join_next() => match r {
                None => break,
                Some(Ok(Ok(()))) => {}
                // That connection gave up; the others pick up its range.
                Some(Ok(Err(e))) => last_err = Some(e),
                Some(Err(e)) => last_err = Some(e.into()),
            },
            _ = tick.tick() => {
                sh.save(&meta)?;
                on_progress(sh.progress());
            }
        }
    }
    on_progress(sh.progress());
    if sh.progress().done < total {
        sh.save(&meta)?;
        if sh.stop.load(Relaxed) {
            return Ok(false);
        }
        return Err(last_err.unwrap_or_else(|| anyhow!("download stalled")));
    }
    sh.file.sync_all()?;
    drop(sh); // close the file before renaming (Windows)
    fs::rename(&part, out)?;
    fs::remove_file(&meta).ok();
    Ok(true)
}

struct Shared {
    client: Client,
    info: Info,
    file: File,
    segs: Mutex<Vec<Seg>>,
    stop: Arc<AtomicBool>,
    limit: Arc<Limiter>,
    conns: Arc<AtomicUsize>,
}

impl Shared {
    fn seg(&self, i: usize) -> (u64, u64) {
        let s = &self.segs.lock().unwrap()[i];
        (s.pos, s.end)
    }

    /// Picks an unclaimed range, or splits the biggest one in progress.
    fn claim(&self) -> Option<usize> {
        if self.stop.load(Relaxed) {
            return None;
        }
        let mut segs = self.segs.lock().unwrap();
        if segs.iter().filter(|s| s.active).count() >= self.conns.load(Relaxed).max(1) {
            return None; // connection count was lowered
        }
        let i = match segs.iter().position(|s| !s.active && s.pos < s.end) {
            Some(i) => i,
            None => {
                let (j, left) = segs.iter().map(|s| s.end - s.pos).enumerate().max_by_key(|&(_, l)| l)?;
                if left < 2 * MIN_SPLIT {
                    return None;
                }
                let (mid, end) = (segs[j].end - left / 2, segs[j].end);
                segs[j].end = mid;
                segs.push(Seg { pos: mid, end, active: false });
                segs.len() - 1
            }
        };
        segs[i].active = true;
        Some(i)
    }

    async fn fetch(&self, i: usize) -> Result<()> {
        let mut fails = 0;
        loop {
            let before = self.seg(i).0;
            let Err(e) = self.fetch_once(i).await else { return Ok(()) };
            if self.stop.load(Relaxed) {
                return Ok(());
            }
            fails = if self.seg(i).0 > before { 1 } else { fails + 1 };
            if fails > RETRIES {
                return Err(e);
            }
            tokio::time::sleep(Duration::from_secs(1 << fails)).await;
        }
    }

    async fn fetch_once(&self, i: usize) -> Result<()> {
        let (mut pos, end) = self.seg(i);
        if pos >= end {
            return Ok(());
        }
        let resp = self
            .client
            .get(&self.info.url)
            .header(header::RANGE, format!("bytes={pos}-{}", end - 1))
            .send()
            .await?
            .error_for_status()?;
        let range_ok = resp.status() == StatusCode::PARTIAL_CONTENT
            && resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with(&format!("bytes {pos}-")));
        if !range_ok {
            bail!("server did not honor range {pos}-{end}");
        }
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            if self.stop.load(Relaxed) {
                return Ok(());
            }
            let chunk = chunk?;
            self.limit.take(chunk.len()).await;
            // `end` only shrinks (another connection stole the tail), so re-read it every chunk.
            // A write racing a steal may spill past the new end; those bytes are identical, so it's harmless.
            let end = self.seg(i).1;
            let n = chunk.len().min(end.saturating_sub(pos) as usize);
            // ponytail: blocking write on the async thread; page-cache writes are µs. spawn_blocking if disk-bound.
            write_at(&self.file, &chunk[..n], pos)?;
            pos += n as u64;
            let mut segs = self.segs.lock().unwrap();
            segs[i].pos = pos.min(segs[i].end); // advanced only after the write, so saved state never lies
            if pos >= segs[i].end {
                return Ok(());
            }
        }
        bail!("connection closed at byte {pos}")
    }

    fn progress(&self) -> Progress {
        let segs = self.segs.lock().unwrap();
        let left: u64 = segs.iter().map(|s| s.end - s.pos).sum();
        // Ranges tile the file end to end (a split cuts one in two), so each starts where the previous ends.
        let mut ends: Vec<u64> = segs.iter().map(|s| s.end).collect();
        ends.sort_unstable();
        let start = |end: u64| ends.partition_point(|&e| e < end).checked_sub(1).map_or(0, |k| ends[k]);
        Progress {
            done: self.info.total.unwrap_or(0) - left,
            total: self.info.total,
            conns: segs.iter().filter(|s| s.active).count(),
            segs: segs
                .iter()
                .map(|s| SegView { start: start(s.end), pos: s.pos, end: s.end, active: s.active, speed: 0 })
                .collect(),
        }
    }

    fn save(&self, meta: &Path) -> Result<()> {
        // Snapshot before flushing: every byte the snapshot counts is then on disk.
        let segs = self.segs.lock().unwrap().clone();
        self.file.sync_data()?;
        let tmp = with_ext(meta, ".tmp");
        fs::write(&tmp, serde_json::to_vec(&State { info: self.info.clone(), segs })?)?;
        fs::rename(tmp, meta)?; // atomic replace: a crash never leaves half a state file
        Ok(())
    }
}

async fn worker(sh: Arc<Shared>) -> Result<()> {
    while let Some(i) = sh.claim() {
        let r = sh.fetch(i).await;
        sh.segs.lock().unwrap()[i].active = false;
        r?;
    }
    Ok(())
}

/// Fallback for servers without range support: one connection, not resumable.
async fn single(
    client: &Client,
    info: &Info,
    out: &Path,
    stop: &AtomicBool,
    limit: &Limiter,
    on_progress: impl Fn(Progress),
) -> Result<bool> {
    let part = with_ext(out, ".part");
    let mut file = File::create(&part)?;
    let mut body = client.get(&info.url).send().await?.error_for_status()?.bytes_stream();
    let (mut done, mut last) = (0, Instant::now());
    while let Some(chunk) = body.next().await {
        if stop.load(Relaxed) {
            drop(file);
            fs::remove_file(&part).ok();
            return Ok(false);
        }
        let chunk = chunk?;
        limit.take(chunk.len()).await;
        file.write_all(&chunk)?;
        done += chunk.len() as u64;
        if last.elapsed() >= Duration::from_secs(1) {
            let end = info.total.unwrap_or(done);
            let segs = vec![SegView { start: 0, pos: done, end, active: true, speed: 0 }];
            on_progress(Progress { done, total: info.total, conns: 1, segs });
            last = Instant::now();
        }
    }
    if info.total.is_some_and(|t| t != done) {
        bail!("connection closed at byte {done}");
    }
    drop(file);
    fs::rename(&part, out)?;
    Ok(true)
}

fn split(total: u64, n: usize) -> Vec<Seg> {
    let n = (n as u64).min(total / MIN_SPLIT).max(1);
    let size = total.div_ceil(n);
    (0..n).map(|i| Seg { pos: i * size, end: ((i + 1) * size).min(total), active: false }).collect()
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

pub(crate) fn with_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(ext);
    s.into()
}

// ponytail: ignores RFC 5987 `filename*=` and doesn't percent-decode URL names; add when names look wrong.
fn filename_from_cd(cd: &str) -> Option<String> {
    cd.split(';')
        .find_map(|p| p.trim().strip_prefix("filename="))
        .map(|f| f.trim_matches('"').to_string())
        .filter(|f| !f.is_empty())
}

fn filename_from_url(url: &Url) -> String {
    url.path_segments().and_then(|mut s| s.next_back()).unwrap_or("").to_string()
}

/// Server-supplied names are untrusted: keep only the last path component and strip characters Windows rejects.
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base.chars().map(|c| if r#"<>:"|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim_matches(['.', ' ']);
    if s.is_empty() { "download".into() } else { s.into() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serves `dir` with range support; returns the base URL.
    pub(crate) async fn serve(dir: &Path) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().fallback_service(tower_http::services::ServeDir::new(dir));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn segmented_download_and_resume() {
        let dir = std::env::temp_dir().join(format!("idm-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut x = 1u64;
        let data: Vec<u8> = (0..20 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        fs::write(dir.join("src.bin"), &data).unwrap();
        let url = serve(&dir).await + "/src.bin";

        let client = client(&[]).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let unlimited = Arc::new(Limiter::new(0));
        let info = probe(&client, &url).await.unwrap();
        assert!(info.resumable && info.total == Some(data.len() as u64) && info.filename == "src.bin");

        let out = dir.join("fresh.bin");
        let eight = || Arc::new(AtomicUsize::new(8));
        assert!(download(&client, info.clone(), &out, eight(), stop.clone(), unlimited.clone(), |_| {}).await.unwrap());
        assert!(fs::read(&out).unwrap() == data, "fresh download corrupted");

        // 20 MiB at 40 MiB/s must take ~0.5 s; the first chunk is free, so allow some slack.
        let out = dir.join("limited.bin");
        let t = Instant::now();
        let limited = Arc::new(Limiter::new(40 << 20));
        assert!(download(&client, info.clone(), &out, eight(), stop.clone(), limited, |_| {}).await.unwrap());
        assert!(t.elapsed() >= Duration::from_millis(450), "limit ignored: {:?}", t.elapsed());
        assert!(fs::read(&out).unwrap() == data, "limited download corrupted");

        // Half of each range "already downloaded", the rest garbage that must be overwritten.
        // Byte 0 is flipped in the done part: it survives only if the download really resumed.
        let out = dir.join("resumed.bin");
        let mut part = data.clone();
        part[0] ^= 1;
        let mut segs = split(data.len() as u64, 4);
        for s in &mut segs {
            s.pos += (s.end - s.pos) / 2;
            part[s.pos as usize..s.end as usize].fill(0xFF);
        }
        fs::write(with_ext(&out, ".part"), &part).unwrap();
        fs::write(with_ext(&out, ".part.json"), serde_json::to_vec(&State { info: info.clone(), segs }).unwrap()).unwrap();
        // Per-connection numbers must be relative to each range, also after a resume.
        let total = data.len() as u64;
        let check = |p: Progress| {
            let mut segs = p.segs;
            segs.sort_by_key(|s| s.start);
            assert_eq!(segs[0].start, 0);
            for w in segs.windows(2) {
                assert_eq!(w[0].end, w[1].start, "ranges must tile the file");
            }
            assert_eq!(segs.last().unwrap().end, total);
            assert!(segs.iter().all(|s| s.start <= s.pos && s.pos <= s.end));
        };
        assert!(download(&client, info, &out, eight(), stop, unlimited, check).await.unwrap());
        let mut expected = data;
        expected[0] ^= 1;
        assert!(fs::read(&out).unwrap() == expected, "resume re-downloaded or corrupted data");
        assert!(!with_ext(&out, ".part.json").exists());

        fs::remove_dir_all(&dir).ok();
    }
}
