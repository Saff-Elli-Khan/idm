use crate::browser;
use crate::manager::{Item, Manager, Status};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

slint::include_modules!();

#[derive(Serialize, Deserialize)]
struct Settings {
    conns: usize,
    limit_mb: f64,
}

const MAX_CONNS: i32 = 32;

pub fn run() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    // Single instance: if our port is taken by a running copy (maybe hidden in the tray), show it and leave.
    // This must happen before the manager starts, or both copies would run the same downloads.
    let listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", browser::PORT)));
    if listener.is_err() && rt.block_on(browser::show_running()) {
        return Ok(());
    }
    let home = PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default());
    let data = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("idm");
    let downloads = home.join("Downloads");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&downloads)?;
    let settings_path = data.join("settings.json");
    let s = std::fs::read(&settings_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or(Settings { conns: 8, limit_mb: 0.0 });
    let m = Manager::new(rt.handle().clone(), downloads, data.join("downloads.json"), 3, s.conns)?;
    m.set_limit(mb_to_bytes(s.limit_mb));

    let ui = AppWindow::new()?;
    ui.set_conns(s.conns as i32);
    ui.set_limit_text(if s.limit_mb > 0.0 { s.limit_mb.to_string().into() } else { "".into() });
    let save = {
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                let s = Settings { conns: ui.get_conns() as usize, limit_mb: ui.get_limit_text().trim().parse().unwrap_or(0.0) };
                if let Err(e) = serde_json::to_vec(&s).map(|b| std::fs::write(&settings_path, b)) {
                    eprintln!("can't save settings: {e}");
                }
            }
        }
    };
    let show = {
        let ui = ui.as_weak();
        move || {
            let _ = ui.upgrade_in_event_loop(|ui| show_window(&ui));
        }
    };
    ui.set_browser_status(match listener {
        Ok(l) => {
            rt.spawn(browser::serve(m.clone(), l, Arc::new(show.clone())));
            "On".into()
        }
        Err(e) => format!("Off: {e}").into(),
    });

    // Tray: closing the window only hides it; downloads keep running until Quit.
    let open = MenuItem::new("Open IDM2", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;
    let tray = Rc::new(
        TrayIconBuilder::new()
            .with_icon(tray_image()?)
            .with_tooltip("IDM2")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?,
    );
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
    let show_from_menu = show.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == quit_id {
            let _ = slint::quit_event_loop();
        } else if e.id == open_id {
            show_from_menu();
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show();
        }
    }));

    let rows = Rc::new(VecModel::<Row>::default());
    ui.set_rows(ModelRc::from(rows.clone()));
    let refresh = {
        let (ui, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = ui.upgrade() {
                refresh(&ui, &m, &rows, &tray);
            }
        }
    };
    refresh();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(500), refresh.clone());
    ui.on_refresh(refresh.clone());

    let act = |f: fn(&Manager, u64)| {
        let (m, refresh) = (m.clone(), refresh.clone());
        move |id: i32| {
            f(&m, id as u64);
            refresh();
        }
    };
    ui.on_pause(act(Manager::pause));
    ui.on_resume(act(Manager::resume));
    ui.on_remove(act(Manager::remove));
    ui.on_add({
        let (m, refresh) = (m.clone(), refresh.clone());
        move |url| {
            let url = url.trim();
            if url.starts_with("http://") || url.starts_with("https://") {
                m.add(url.into(), vec![]);
                refresh();
            }
        }
    });
    ui.on_open({
        let m = m.clone();
        move |id| {
            if let Some(out) = m.items().into_iter().find(|i| i.id == id as u64).and_then(|i| i.out) {
                reveal(&out);
            }
        }
    });
    let win = ui.as_weak();
    ui.on_drag(move |dx, dy| {
        if let Some(ui) = win.upgrade() {
            let (w, s) = (ui.window(), ui.window().scale_factor());
            let p = w.position();
            w.set_position(slint::PhysicalPosition::new(p.x + (dx * s) as i32, p.y + (dy * s) as i32));
        }
    });
    let win = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = win.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let win = ui.as_weak();
    ui.on_toggle_maximize(move || {
        if let Some(ui) = win.upgrade() {
            ui.window().set_maximized(!ui.window().is_maximized());
        }
    });
    let win = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = win.upgrade() {
            let _ = ui.hide(); // keeps running in the tray
        }
    });
    ui.on_set_conns({
        let (m, win, save) = (m.clone(), ui.as_weak(), save.clone());
        move |n| {
            let n = n.clamp(1, MAX_CONNS);
            m.set_conns(n as usize);
            if let Some(ui) = win.upgrade() {
                ui.set_conns(n);
            }
            save();
        }
    });
    ui.on_set_limit(move |t| {
        m.set_limit(mb_to_bytes(t.trim().parse().unwrap_or(0.0)));
        save();
    });

    // Hidden windows (closed to tray, Alt+F4) don't end the event loop; only Quit does.
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn show_window(ui: &AppWindow) {
    // Hide + show brings it to the front even if it was merely behind other windows.
    let _ = ui.hide();
    let _ = ui.show();
    ui.window().set_minimized(false);
}

/// Tray icon pixels from the extension's 48 px PNG (RGBA, rendered from ui/logo.svg).
fn tray_image() -> Result<tray_icon::Icon> {
    let png = include_bytes!("../extension/icons/48.png");
    let mut r = png::Decoder::new(std::io::Cursor::new(&png[..])).read_info()?;
    let (w, h) = (r.info().width, r.info().height);
    anyhow::ensure!(r.info().color_type == png::ColorType::Rgba, "tray icon must be an RGBA PNG");
    let mut buf = vec![0; (w * h * 4) as usize];
    r.next_frame(&mut buf)?;
    Ok(tray_icon::Icon::from_rgba(buf, w, h)?)
}

fn refresh(ui: &AppWindow, m: &Manager, rows: &VecModel<Row>, tray: &TrayIcon) {
    let items = m.items();
    let active = items.iter().filter(|i| i.status == Status::Downloading);
    let (n, speed) = active.fold((0, 0), |(n, s), i| (n + 1, s + i.speed));
    let summary =
        if n == 0 { format!("{} download{}", items.len(), if items.len() == 1 { "" } else { "s" }) } else { format!("{n} downloading · {}/s", size(speed)) };
    if ui.get_summary() != summary.as_str() {
        let _ = tray.set_tooltip(Some(format!("IDM2 · {summary}")));
        ui.set_summary(summary.into());
    }
    let filter = ui.get_filter();
    let new: Vec<Row> = items
        .iter()
        .rev()
        .filter(|i| match filter {
            1 => i.status != Status::Done,
            2 => i.status == Status::Done,
            _ => true,
        })
        .map(row)
        .collect();
    // Update in place so hover state survives the refresh; rebuild only when rows come or go.
    if rows.row_count() == new.len() {
        for (i, r) in new.into_iter().enumerate() {
            if rows.row_data(i).as_ref() != Some(&r) {
                rows.set_row_data(i, r);
            }
        }
    } else {
        rows.set_vec(new);
    }
}

fn row(i: &Item) -> Row {
    let name = i.out.as_deref().and_then(Path::file_name).map_or_else(|| i.url.clone(), |n| n.to_string_lossy().into_owned());
    let ext = Path::new(&name)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .filter(|e| e.len() <= 4)
        .unwrap_or_else(|| "FILE".into());
    let of = || match i.total {
        Some(t) => format!("{} / {}", size(i.done), size(t)),
        None => size(i.done),
    };
    let (status, detail) = match i.status {
        Status::Downloading => {
            let eta = match i.total {
                Some(t) if i.speed > 0 => format!(" · {} left", dur(t.saturating_sub(i.done) / i.speed)),
                _ => String::new(),
            };
            ("Downloading", format!("{} · {}/s · {} connections{eta}", of(), size(i.speed), i.conns))
        }
        Status::Queued => ("Queued", "Waiting for a free slot".into()),
        Status::Paused => ("Paused", of()),
        Status::Done => ("Completed", size(i.total.unwrap_or(i.done))),
        Status::Failed => ("Failed", i.error.clone().unwrap_or_default()),
    };
    let progress = match (i.status, i.total) {
        (Status::Done, _) => 1.0,
        (_, Some(t)) if t > 0 => i.done as f32 / t as f32,
        _ => 0.0,
    };
    let mut live: Vec<_> = i.segs.iter().filter(|s| s.active).collect();
    live.sort_by_key(|s| s.start);
    let connections: Vec<Conn> = live
        .iter()
        .enumerate()
        .map(|(n, s)| Conn {
            label: format!("Connection {}", n + 1).into(),
            progress: (s.pos - s.start) as f32 / (s.end - s.start).max(1) as f32,
            detail: format!("{} / {} · {}/s", size(s.pos - s.start), size(s.end - s.start), size(s.speed)).into(),
        })
        .collect();
    Row {
        connections: ModelRc::new(VecModel::from(connections)),
        id: i.id as i32,
        name: name.into(),
        ext: ext.into(),
        status: status.into(),
        detail: detail.into(),
        progress,
        can_pause: matches!(i.status, Status::Downloading | Status::Queued),
        can_resume: matches!(i.status, Status::Paused | Status::Failed),
        done: i.status == Status::Done,
    }
}

fn mb_to_bytes(mb: f64) -> u64 {
    (mb.max(0.0) * 1048576.0) as u64
}

fn size(b: u64) -> String {
    let b = b as f64;
    if b >= 1073741824.0 {
        format!("{:.2} GB", b / 1073741824.0)
    } else if b >= 1048576.0 {
        format!("{:.1} MB", b / 1048576.0)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

fn dur(s: u64) -> String {
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(windows)]
fn reveal(p: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", p.display())).spawn();
}

#[cfg(not(windows))]
fn reveal(p: &Path) {
    if let Some(dir) = p.parent() {
        let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }).arg(dir).spawn();
    }
}
