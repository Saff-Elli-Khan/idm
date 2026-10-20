//! BitTorrent downloads (magnet links and local .torrent files), using librqbit.
//! Seeding stops as soon as a download completes.

use anyhow::{anyhow, bail, Result};
use librqbit::limits::LimitsConfig;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions, Session, SessionOptions, TorrentStatsState};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Magnet links and local .torrent files. An http(s) link to a .torrent file is not: it's
/// downloaded like any file (with the browser's cookies) and added once it's on disk.
pub fn is_torrent(src: &str) -> bool {
    src.starts_with("magnet:") || (!src.contains("://") && src.to_ascii_lowercase().ends_with(".torrent"))
}

#[derive(Clone, Debug)]
pub struct Peer {
    /// Client name, e.g. "qBittorrent 4.6.2".
    pub name: String,
    pub addr: String,
    pub fetched: u64,
    pub speed: u64,
}

pub struct Progress {
    /// Where the files are, once the torrent is live: the file, or the folder of a multi-file torrent.
    pub out: Option<PathBuf>,
    pub done: u64,
    pub total: Option<u64>,
    pub speed: u64,
    /// Fastest peers first, at most 10.
    pub peers: Vec<Peer>,
    pub live_peers: usize,
}

pub async fn session(dir: PathBuf, limit_bps: u64) -> Result<Arc<Session>> {
    Session::new_with_opts(
        dir,
        SessionOptions {
            // No DHT persistence, so librqbit leaves no files outside our own folders.
            dht: Some(DhtSessionConfig { persistence: None, ..Default::default() }),
            listen: Some(ListenerOptions { enable_upnp_port_forwarding: true, ..Default::default() }),
            ratelimits: LimitsConfig { download_bps: bps(limit_bps), upload_bps: None },
            client_name_and_version: Some(concat!("IDM2 ", env!("CARGO_PKG_VERSION")).into()),
            ..Default::default()
        },
    )
    .await
}

/// librqbit's rate limit; 0 = unlimited.
pub fn bps(bytes_per_sec: u64) -> Option<NonZeroU32> {
    NonZeroU32::new(bytes_per_sec.min(u32::MAX as u64) as u32)
}

/// Downloads until finished (`Ok(true)`) or `stop` is set (`Ok(false)`). Either way the torrent
/// then leaves the session, which ends seeding. If `removed()` says the user deleted it,
/// librqbit also deletes its unfinished files.
pub async fn run(
    session: &Arc<Session>,
    add: AddTorrent<'_>,
    opts: AddTorrentOptions,
    stop: &AtomicBool,
    removed: impl Fn() -> bool,
    mut on_progress: impl FnMut(Progress),
) -> Result<bool> {
    let handle = match session.add_torrent(add, Some(opts)).await? {
        AddTorrentResponse::Added(_, h) => h,
        AddTorrentResponse::AlreadyManaged(..) => bail!("this torrent is already downloading"),
        AddTorrentResponse::ListOnly(_) => bail!("torrent was only listed"),
    };
    let mut fetched_before: HashMap<String, u64> = HashMap::new();
    let mut t0 = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let result = loop {
        tick.tick().await;
        let st = handle.stats();
        if let Some(e) = &st.error {
            break Err(anyhow!("{e}"));
        }
        let dt = t0.elapsed().as_secs_f64().max(0.001);
        t0 = Instant::now();
        let mut peers = Vec::new();
        if let Some(live) = handle.live() {
            for (addr, p) in live.per_peer_stats_snapshot(Default::default()).peers {
                let fetched = p.counters.fetched_bytes;
                let prev = fetched_before.insert(addr.clone(), fetched).unwrap_or(fetched);
                let speed = (fetched.saturating_sub(prev) as f64 / dt) as u64;
                peers.push(Peer { name: p.client_name.unwrap_or_else(|| "Peer".into()), addr, fetched, speed });
            }
        }
        let live_peers = peers.len();
        peers.sort_by_key(|p| std::cmp::Reverse((p.speed, p.fetched)));
        peers.truncate(10);
        // Only report the path once librqbit has accepted the files (Live). A fresh torrent that
        // collides with existing files fails before that, and the caller only allows overwriting
        // when resuming a torrent that reached this point.
        let out = match st.state {
            TorrentStatsState::Live => handle.name().and_then(|name| {
                let files = handle.with_metadata(|m| m.file_infos.len()).ok()?;
                let folder = handle.output_folder();
                Some(if files > 1 { folder.to_path_buf() } else { folder.join(name) })
            }),
            _ => None,
        };
        on_progress(Progress {
            out,
            done: st.progress_bytes,
            total: (st.total_bytes > 0).then_some(st.total_bytes),
            speed: st.live.as_ref().map_or(0, |l| l.download_speed.as_bytes()),
            peers,
            live_peers,
        });
        if st.finished {
            break Ok(true);
        }
        if stop.load(Relaxed) {
            break Ok(false);
        }
    };
    let delete_files = removed() && !matches!(result, Ok(true));
    session.delete(handle.id().into(), delete_files).await.ok();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use librqbit::spawn_utils::BlockingSpawner;
    use librqbit::CreateTorrentOptions;
    use std::fs;

    fn offline(listen: Option<ListenerOptions>) -> SessionOptions {
        SessionOptions { dht: None, disable_trackers: true, disable_local_service_discovery: true, listen, ..Default::default() }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downloads_from_a_local_seeder_and_never_overwrites_on_a_fresh_add() {
        let dir = std::env::temp_dir().join(format!("idm-torrent-{}", std::process::id()));
        let (seed_dir, dl_dir) = (dir.join("seed"), dir.join("dl"));
        fs::create_dir_all(&seed_dir).unwrap();
        fs::create_dir_all(&dl_dir).unwrap();
        let data: Vec<u8> = (0..3u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        fs::write(seed_dir.join("file.bin"), &data).unwrap();
        let opts = CreateTorrentOptions { name: None, trackers: vec![], piece_length: None };
        let bytes = librqbit::create_torrent(&seed_dir.join("file.bin"), opts, &BlockingSpawner::new(4)).await.unwrap().as_bytes().unwrap();

        // A seeder on loopback (no firewall prompt), found through initial_peers instead of DHT/trackers.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let listen = ListenerOptions { listen_addr: ([127, 0, 0, 1], port).into(), announce_port: Some(port), ..Default::default() };
        let seeder = Session::new_with_opts(seed_dir.clone(), offline(Some(listen))).await.unwrap();
        let seeding = seeder.add_torrent(AddTorrent::from_bytes(bytes.clone()), Some(AddTorrentOptions { overwrite: true, ..Default::default() }));
        seeding.await.unwrap().into_handle().unwrap().wait_until_completed().await.unwrap();

        let leecher = Session::new_with_opts(dl_dir.clone(), offline(None)).await.unwrap();
        let peer_opts = || AddTorrentOptions { initial_peers: Some(vec![([127, 0, 0, 1], port).into()]), ..Default::default() };
        let stop = AtomicBool::new(false);
        let mut out = None;
        let run_once = run(&leecher, AddTorrent::from_bytes(bytes.clone()), peer_opts(), &stop, || false, |p| {
            if p.out.is_some() {
                out = p.out;
            }
        });
        assert!(tokio::time::timeout(Duration::from_secs(60), run_once).await.unwrap().unwrap());
        assert_eq!(out.as_deref(), Some(dl_dir.join("file.bin").as_path()));
        assert!(fs::read(dl_dir.join("file.bin")).unwrap() == data, "torrent download corrupted");

        // Adding the same torrent fresh again must refuse to touch the existing file.
        fs::write(dl_dir.join("file.bin"), b"user's own file").unwrap();
        let again = run(&leecher, AddTorrent::from_bytes(bytes), peer_opts(), &stop, || false, |_| {});
        assert!(tokio::time::timeout(Duration::from_secs(30), again).await.unwrap().is_err());
        assert_eq!(fs::read(dl_dir.join("file.bin")).unwrap(), b"user's own file");
        fs::remove_dir_all(&dir).ok();
    }
}
