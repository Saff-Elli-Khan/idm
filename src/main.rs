// Release builds are a GUI app (no console window); the CLI re-attaches to the terminal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod engine;
mod gui;
mod manager;

use anyhow::{bail, Context, Result};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// `idm` opens the GUI; `idm <url> [-n N] [-o out]` downloads in the terminal.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return gui::run();
    }
    attach_console();
    tokio::runtime::Runtime::new()?.block_on(cli(args))
}

#[cfg(windows)]
fn attach_console() {
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    unsafe { AttachConsole(u32::MAX) }; // ATTACH_PARENT_PROCESS
}

#[cfg(not(windows))]
fn attach_console() {}

async fn cli(args: Vec<String>) -> Result<()> {
    let (mut url, mut out, mut conns) = (None, None, 8usize);
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-n" => conns = args.next().context("-n needs a number")?.parse()?,
            "-o" => out = Some(PathBuf::from(args.next().context("-o needs a path")?)),
            _ => url = Some(a),
        }
    }
    let url = url.context("usage: idm <url> [-n connections (default 8, max 32)] [-o output]")?;
    let conns = conns.clamp(1, 32);

    let client = engine::client(&[])?;
    let info = engine::probe(&client, &url).await?;
    let out = out.unwrap_or_else(|| PathBuf::from(&info.filename));
    if out.exists() {
        bail!("{} already exists", out.display());
    }
    let resumable = info.resumable;
    eprintln!(
        "{} | {} | {}",
        out.display(),
        info.total.map_or("unknown size".into(), mb),
        if resumable { format!("{conns} connections") } else { "no range support: 1 connection, not resumable".into() }
    );

    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            s.store(true, Ordering::Relaxed);
        }
    });

    let start = Instant::now();
    let last = Cell::new((start, None::<u64>));
    let limit = Arc::new(engine::Limiter::new(0));
    let conns = Arc::new(AtomicUsize::new(conns));
    let finished = engine::download(&client, info, &out, conns, stop, limit, |p| {
        let (t0, d0) = last.replace((Instant::now(), Some(p.done)));
        let speed = d0.map_or(0.0, |d0| p.done.saturating_sub(d0) as f64 / t0.elapsed().as_secs_f64());
        let pct = p.total.map_or(String::new(), |t| format!("{:5.1}% ", p.done as f64 * 100.0 / t.max(1) as f64));
        let eta = match p.total {
            Some(t) if speed > 0.0 => format!("  ETA {}s", ((t - p.done) as f64 / speed) as u64),
            _ => String::new(),
        };
        eprint!("\r{pct}{} / {}  {}/s  {} conns{eta}    ", mb(p.done), p.total.map_or("?".into(), mb), mb(speed as u64), p.conns);
    })
    .await?;
    eprintln!();
    if finished {
        eprintln!("saved {} in {:.1}s", out.display(), start.elapsed().as_secs_f64());
    } else if resumable {
        eprintln!("paused; run the same command again to resume");
    } else {
        eprintln!("cancelled");
    }
    Ok(())
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1048576.0)
}
