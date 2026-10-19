//! Download queue: runs up to `max_active` downloads at once, persists the list to a JSON file.

use crate::engine::{self, with_ext, Limiter, SegView};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::runtime::Handle;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Item {
    pub id: u64,
    pub url: String,
    /// Chosen once the server tells us the filename.
    pub out: Option<PathBuf>,
    pub status: Status,
    pub done: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    /// Cookie/Referer from the browser; dropped once the download is done.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(skip)]
    pub speed: u64,
    #[serde(skip)]
    pub conns: usize,
    /// Per-range progress of the last run, with per-connection speed.
    #[serde(skip)]
    pub segs: Vec<SegView>,
}

#[derive(Clone)]
pub struct Manager(Arc<Inner>);

struct Inner {
    rt: Handle,
    limit: Arc<Limiter>,
    dir: PathBuf,
    db: PathBuf,
    max_active: usize,
    conns: Arc<AtomicUsize>,
    st: Mutex<State>,
}

struct State {
    items: Vec<Item>,
    stops: HashMap<u64, Arc<AtomicBool>>,
}

impl Manager {
    pub fn new(rt: Handle, dir: PathBuf, db: PathBuf, max_active: usize, conns: usize) -> Result<Self> {
        let mut items: Vec<Item> = match fs::read(&db) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(_) => Vec::new(),
        };
        for it in &mut items {
            if it.status == Status::Downloading {
                it.status = Status::Queued; // interrupted by exit; picks up from its .part file
            }
        }
        let m = Manager(Arc::new(Inner {
            rt,
            limit: Arc::new(Limiter::new(0)),
            dir,
            db,
            max_active,
            conns: Arc::new(AtomicUsize::new(conns)),
            st: Mutex::new(State { items, stops: HashMap::new() }),
        }));
        m.pump();
        Ok(m)
    }

    pub fn items(&self) -> Vec<Item> {
        self.0.st.lock().unwrap().items.clone()
    }

    /// Bytes per second across all downloads; 0 = unlimited.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        self.0.limit.set(bytes_per_sec);
    }

    /// Connections per download; applies to running downloads too.
    pub fn set_conns(&self, n: usize) {
        self.0.conns.store(n, Relaxed);
    }

    pub fn add(&self, url: String, headers: Vec<(String, String)>) -> u64 {
        let mut st = self.0.st.lock().unwrap();
        let id = st.items.iter().map(|i| i.id + 1).max().unwrap_or(1);
        st.items.push(Item {
            id,
            url,
            out: None,
            status: Status::Queued,
            done: 0,
            total: None,
            error: None,
            headers,
            speed: 0,
            conns: 0,
            segs: Vec::new(),
        });
        drop(st);
        self.pump();
        id
    }

    pub fn pause(&self, id: u64) {
        let mut st = self.0.st.lock().unwrap();
        if let Some(stop) = st.stops.get(&id) {
            stop.store(true, Relaxed); // the task marks it Paused when it winds down
        } else if let Some(it) = st.items.iter_mut().find(|i| i.id == id && i.status == Status::Queued) {
            it.status = Status::Paused;
            self.save(&st);
        }
    }

    pub fn resume(&self, id: u64) {
        let mut st = self.0.st.lock().unwrap();
        if let Some(it) = st.items.iter_mut().find(|i| i.id == id && matches!(i.status, Status::Paused | Status::Failed)) {
            it.status = Status::Queued;
            it.error = None;
        }
        drop(st);
        self.pump();
    }

    /// Removes from the list and deletes unfinished partial files (finished files are kept).
    pub fn remove(&self, id: u64) {
        let mut st = self.0.st.lock().unwrap();
        let Some(i) = st.items.iter().position(|i| i.id == id) else { return };
        let it = st.items.remove(i);
        match st.stops.get(&id) {
            Some(stop) => stop.store(true, Relaxed), // file is still open; the task cleans up
            None if it.status != Status::Done => delete_partials(it.out.as_deref()),
            None => {}
        }
        self.save(&st);
    }

    /// Starts queued items until `max_active` are running.
    fn pump(&self) {
        let mut st = self.0.st.lock().unwrap();
        let mut active = st.stops.len();
        for i in 0..st.items.len() {
            if active >= self.0.max_active {
                break;
            }
            if st.items[i].status != Status::Queued {
                continue;
            }
            let it = &mut st.items[i];
            it.status = Status::Downloading;
            let (id, url, out, headers) = (it.id, it.url.clone(), it.out.clone(), it.headers.clone());
            let stop = Arc::new(AtomicBool::new(false));
            st.stops.insert(id, stop.clone());
            self.0.rt.spawn(self.clone().run(id, url, out, headers, stop));
            active += 1;
        }
        self.save(&st);
    }

    async fn run(self, id: u64, url: String, mut out: Option<PathBuf>, headers: Vec<(String, String)>, stop: Arc<AtomicBool>) {
        let r = self.run_inner(id, &url, &mut out, &headers, stop).await;
        let mut st = self.0.st.lock().unwrap();
        st.stops.remove(&id);
        match st.items.iter_mut().find(|i| i.id == id) {
            None => delete_partials(out.as_deref()), // removed while running
            Some(it) => {
                (it.speed, it.conns) = (0, 0);
                for s in &mut it.segs {
                    (s.active, s.speed) = (false, 0);
                }
                it.status = match r {
                    Ok(true) => {
                        it.headers.clear(); // no reason to keep cookies on disk
                        Status::Done
                    }
                    Ok(false) => Status::Paused,
                    Err(e) => {
                        it.error = Some(format!("{e:#}"));
                        Status::Failed
                    }
                };
            }
        }
        drop(st);
        self.pump();
    }

    async fn run_inner(
        &self,
        id: u64,
        url: &str,
        out: &mut Option<PathBuf>,
        headers: &[(String, String)],
        stop: Arc<AtomicBool>,
    ) -> Result<bool> {
        let client = engine::client(headers)?;
        let info = engine::probe(&client, url).await?;
        let path = match out {
            Some(p) => p.clone(),
            None => {
                let mut st = self.0.st.lock().unwrap();
                let p = self.unique(&st, &info.filename);
                if let Some(it) = st.items.iter_mut().find(|i| i.id == id) {
                    (it.out, it.total) = (Some(p.clone()), info.total);
                }
                self.save(&st);
                out.insert(p).clone()
            }
        };
        // Previous tick: time, total done, and each range's position (range indices are stable).
        let last = Mutex::new((Instant::now(), None::<u64>, Vec::<u64>::new()));
        engine::download(&client, info, &path, self.0.conns.clone(), stop, self.0.limit.clone(), |mut p| {
            let mut l = last.lock().unwrap();
            let dt = l.0.elapsed().as_secs_f64();
            let speed = l.1.map_or(0.0, |d0| p.done.saturating_sub(d0) as f64 / dt);
            for (s, &prev) in p.segs.iter_mut().zip(&l.2) {
                s.speed = (s.pos.saturating_sub(prev) as f64 / dt) as u64;
            }
            *l = (Instant::now(), Some(p.done), p.segs.iter().map(|s| s.pos).collect());
            drop(l);
            let mut st = self.0.st.lock().unwrap();
            if let Some(it) = st.items.iter_mut().find(|i| i.id == id) {
                (it.done, it.total, it.speed, it.conns) = (p.done, p.total, speed as u64, p.conns);
                it.segs = p.segs;
            }
        })
        .await
    }

    /// `name`, or `name (1).ext`, ... not taken on disk or by another item.
    fn unique(&self, st: &State, name: &str) -> PathBuf {
        let p = Path::new(name);
        let stem = p.file_stem().unwrap_or_default().to_string_lossy();
        let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        (0..)
            .map(|n| self.0.dir.join(if n == 0 { name.to_string() } else { format!("{stem} ({n}){ext}") }))
            .find(|c| !c.exists() && !with_ext(c, ".part").exists() && !st.items.iter().any(|i| i.out.as_ref() == Some(c)))
            .unwrap()
    }

    fn save(&self, st: &State) {
        let tmp = with_ext(&self.0.db, ".tmp");
        let r = serde_json::to_vec_pretty(&st.items)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(fs::write(&tmp, b)?))
            .and_then(|_| Ok(fs::rename(&tmp, &self.0.db)?));
        if let Err(e) = r {
            eprintln!("can't save {}: {e:#}", self.0.db.display());
        }
    }
}

fn delete_partials(out: Option<&Path>) {
    if let Some(out) = out {
        fs::remove_file(with_ext(out, ".part")).ok();
        fs::remove_file(with_ext(out, ".part.json")).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn queue_runs_one_at_a_time_and_persists() {
        let dir = std::env::temp_dir().join(format!("idm-queue-{}", std::process::id()));
        let srv = dir.join("srv");
        fs::create_dir_all(&srv).unwrap();
        let data: Vec<u8> = (0..3u32 << 20).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(srv.join("f.bin"), &data).unwrap();
        let url = engine::tests::serve(&srv).await + "/f.bin";

        let db = dir.join("downloads.json");
        let m = Manager::new(Handle::current(), dir.clone(), db.clone(), 1, 4).unwrap();
        for _ in 0..3 {
            m.add(url.clone(), vec![]);
        }
        for _ in 0..300 {
            let items = m.items();
            assert!(items.iter().filter(|i| i.status == Status::Downloading).count() <= 1);
            if items.iter().all(|i| i.status == Status::Done) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for name in ["f.bin", "f (1).bin", "f (2).bin"] {
            assert!(fs::read(dir.join(name)).unwrap() == data, "{name} wrong");
        }
        drop(m);
        let m = Manager::new(Handle::current(), dir.clone(), db, 1, 4).unwrap();
        assert!(m.items().len() == 3 && m.items().iter().all(|i| i.status == Status::Done));
        fs::remove_dir_all(&dir).ok();
    }
}
