#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod format;
mod text_input;
mod tray;
mod ui;

use std::path::PathBuf;

use gpui::{px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

pub struct Args {
    pub profile: Option<String>,
    pub downloads: Option<PathBuf>,
    /// Testing aid: once `demo_peers` devices are visible, select them all,
    /// stage `demo_send` and press Send through the normal UI path.
    pub demo_send: Option<PathBuf>,
    pub demo_peers: usize,
    pub auto_accept: bool,
    /// Testing aid: start on the settings page.
    pub open_settings: bool,
    /// Testing aid: open the Gofile tab and upload this path once connected.
    pub demo_gofile_upload: Option<PathBuf>,
    /// Testing aid: start on the Gofile tab.
    pub open_gofile: bool,
    /// Testing aid: only stage the `demo_gofile_upload` path (don't upload).
    pub demo_gofile_stage_only: bool,
}

fn parse_args() -> Args {
    let mut a = Args { profile: None, downloads: None, demo_send: None, demo_peers: 1, auto_accept: false, open_settings: false, demo_gofile_upload: None, open_gofile: false, demo_gofile_stage_only: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--profile" => a.profile = it.next(),
            "--downloads" => a.downloads = it.next().map(PathBuf::from),
            "--demo-send" => a.demo_send = it.next().map(PathBuf::from),
            "--demo-peers" => a.demo_peers = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--auto-accept" => a.auto_accept = true,
            "--open-settings" => a.open_settings = true,
            "--open-gofile" => a.open_gofile = true,
            "--demo-gofile-stage" => {
                a.demo_gofile_upload = it.next().map(PathBuf::from);
                a.open_gofile = true;
                a.demo_gofile_stage_only = true;
            }
            "--demo-gofile-upload" => {
                a.demo_gofile_upload = it.next().map(PathBuf::from);
                a.open_gofile = true;
            }
            _ => {}
        }
    }
    a
}

fn main() {
    let args = parse_args();
    if let Ok(exe) = std::env::current_exe() {
        // Just updated: let the old copy finish quitting (it still holds the
        // network port), then tidy up after it.
        if std::env::args().any(|a| a == "--updated") {
            let old = exe.with_file_name(format!("{}.old.exe", exe.file_stem().unwrap_or_default().to_string_lossy()));
            for _ in 0..50 {
                if std::fs::remove_file(&old).is_ok() || !old.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        neo_core::update::cleanup(&exe);
    }
    let mut config = neo_core::Config::for_profile(args.profile.as_deref());
    if let Some(d) = &args.downloads {
        config.downloads = d.clone();
    }
    config.auto_accept = args.auto_accept;
    let node = match neo_core::start(config) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Neo couldn't start: {e:#}");
            std::process::exit(1);
        }
    };

    Application::new().run(move |cx: &mut App| {
        text_input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(940.), px(620.)), cx);
        let title = match &args.profile {
            Some(p) => format!("Neo — {p}"),
            None => "Neo".to_string(),
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                window_min_size: Some(size(px(720.), px(460.))),
                ..Default::default()
            },
            |window, cx| {
                let tray = tray::Tray::new();
                cx.new(|cx| ui::NeoView::new(node, args, tray, window, cx))
            },
        )
        .expect("open window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
