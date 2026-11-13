//! The Neo window: people and devices on the left, requests, the composer
//! and the activity feed on the right (or the settings page). The view only
//! re-renders when the engine sends an event or the user interacts, so an
//! idle window costs ~nothing.

mod gofile;
mod requests;
mod settings;
mod side;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    anchored, deferred, div, point, px, relative, rgb, rgba, AnyElement, AppContext, ClickEvent, ClipboardItem, Context, Entity, ExternalPaths,
    FontWeight, Hsla, InteractiveElement, IntoElement, MouseDownEvent, ParentElement, PathPromptOptions, Render, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use neo_core::{
    AccountId, Command, Contact, DeviceId, Direction, Event, NodeHandle, Peer, Reachability, TransferInfo, TransferState,
};

use crate::format;
use crate::text_input::{Submitted, TextInput};
use crate::tray::{self, Tray, TrayEvent};
use crate::Args;

mod theme {
    pub const BG: u32 = 0x0d0e11;
    pub const PANEL: u32 = 0x14161b;
    pub const RAISED: u32 = 0x1b1e25;
    pub const HOVER: u32 = 0x22262f;
    pub const BORDER: u32 = 0x252932;
    pub const TEXT: u32 = 0xe9eaee;
    pub const MUTED: u32 = 0x8d929e;
    pub const FAINT: u32 = 0x5d626d;
    pub const ACCENT: u32 = 0x7b6cff;
    pub const ACCENT_HOVER: u32 = 0x8d80ff;
    pub const OK: u32 = 0x3fcf8e;
    pub const BAD: u32 = 0xf2706d;
    pub const WARN: u32 = 0xf0b44c;
    pub const TRACK: u32 = 0x262a33;
}
use theme::*;

struct Staged {
    path: PathBuf,
    name: String,
    size: Option<u64>,
}

struct Transfer {
    info: TransferInfo,
    state: TransferState,
    done: u64,
    rate: f64,
    sample: (Instant, u64),
    /// Last time any bytes moved; drives the "stalled" hint.
    moved: Instant,
    /// (paused by you, paused by them).
    paused: (bool, bool),
}

impl Transfer {
    fn is_paused(&self) -> bool {
        !self.state.is_finished() && (self.paused.0 || self.paused.1)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Send,
    Gofile,
}

/// A Gofile link a contact sent you: (from, title, url).
struct SharedLink {
    from: String,
    title: String,
    url: String,
}

/// Something waiting for your answer.
enum Request {
    Pair { id: u64, name: String, account: AccountId, device_name: String },
    Link { id: u64, device_name: String, sas: String },
}

impl Request {
    fn id(&self) -> u64 {
        match self {
            Request::Pair { id, .. } | Request::Link { id, .. } => *id,
        }
    }
}

pub struct NeoView {
    node: NodeHandle,
    /// Your account code (what you share with people).
    code: SharedString,
    account: AccountId,
    person_name: String,
    contacts: Vec<Contact>,
    peers: Vec<Peer>,
    selected: HashSet<DeviceId>,
    staged: Vec<Staged>,
    /// Newest first.
    transfers: Vec<Transfer>,
    index: HashMap<u64, usize>,
    requests: Vec<Request>,
    /// This device asked to join an identity: (device name, number to compare).
    link_pending: Option<(String, String)>,
    notice: Option<SharedString>,
    info: Option<SharedString>,
    /// How this device can be reached from the internet.
    reach: Reachability,
    info_token: u64,
    copied: bool,
    demo: Option<(PathBuf, usize)>,
    /// A one-second re-render is scheduled (only while something is active).
    tick_pending: bool,
    /// The "Choose files / Choose a folder" menu is open.
    browse_menu: bool,
    /// When a click outside last closed that menu (so the same click on its
    /// button doesn't immediately reopen it).
    browse_menu_closed: Option<Instant>,
    name: String,
    downloads: PathBuf,
    settings_open: bool,
    /// "Add by code" field in the sidebar is open.
    adding: bool,
    name_input: Entity<TextInput>,
    person_input: Entity<TextInput>,
    code_input: Entity<TextInput>,
    link_input: Entity<TextInput>,
    pass_input: Entity<TextInput>,
    /// Speed limits (MB/s, None = unlimited) as saved, and their fields.
    up_limit: Option<f64>,
    down_limit: Option<f64>,
    up_input: Entity<TextInput>,
    down_input: Entity<TextInput>,
    close_to_tray: bool,
    tray: Option<Tray>,
    hwnd: Option<isize>,
    /// The window has focus; notifications only show when it doesn't.
    active: bool,
    /// Hidden in the tray.
    hidden: bool,
    tray_hint_shown: bool,
    _activation: Subscription,
    tab: Tab,
    g: gofile::GofileState,
    demo_gofile: Option<PathBuf>,
    demo_gofile_stage_only: bool,
    shared_links: Vec<SharedLink>,
}

impl NeoView {
    pub fn new(
        node: NodeHandle,
        args: Args,
        tray: Option<(Tray, async_channel::Receiver<TrayEvent>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tray = tray.map(|(tray, events)| {
            cx.spawn(async move |this, cx| {
                while let Ok(ev) = events.recv().await {
                    if this.update(cx, |this, cx| this.on_tray(ev, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            tray
        });
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| view.update(cx, |this, cx| this.on_close_requested(cx)).unwrap_or(true));
        let activation = cx.observe_window_activation(window, |this, window, _| this.active = window.is_window_active());
        let events = node.events.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                let alive = this.update(cx, |this, cx| {
                    this.apply(ev, cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let name_input = cx.new(|cx| TextInput::new(cx, "Device name", &node.name));
        let person_input = cx.new(|cx| TextInput::new(cx, "Your name", &node.person_name));
        let code_input = cx.new(|cx| TextInput::new(cx, "Their code, e.g. 7KQ3-M2XA-9FTB", ""));
        let link_input = cx.new(|cx| TextInput::new(cx, "Code shown on your other device", ""));
        let pass_input = cx.new(|cx| TextInput::new(cx, "Backup passphrase (8+ characters)", "").masked());
        cx.subscribe(&name_input, |this, _, _: &Submitted, cx| this.save_settings(cx)).detach();
        cx.subscribe(&person_input, |this, _, _: &Submitted, cx| this.save_settings(cx)).detach();
        cx.subscribe(&code_input, |this, _, _: &Submitted, cx| this.add_by_code(cx)).detach();
        cx.subscribe(&link_input, |this, _, _: &Submitted, cx| this.link(cx)).detach();
        let up_input = cx.new(|cx| TextInput::new(cx, "No limit", &settings::limit_text(node.upload_limit)));
        let down_input = cx.new(|cx| TextInput::new(cx, "No limit", &settings::limit_text(node.download_limit)));
        cx.subscribe(&up_input, |this, _, _: &Submitted, cx| this.save_limits(cx)).detach();
        cx.subscribe(&down_input, |this, _, _: &Submitted, cx| this.save_limits(cx)).detach();

        let g = gofile::GofileState::new(cx);
        NeoView {
            tab: if args.open_gofile { Tab::Gofile } else { Tab::Send },
            g,
            demo_gofile: args.demo_gofile_upload.clone(),
            demo_gofile_stage_only: args.demo_gofile_stage_only,
            shared_links: Vec::new(),
            code: node.account.code().into(),
            account: node.account,
            person_name: node.person_name.clone(),
            contacts: Vec::new(),
            close_to_tray: node.close_to_tray,
            tray,
            hwnd: tray::hwnd_of(window),
            active: window.is_window_active(),
            hidden: false,
            tray_hint_shown: false,
            _activation: activation,
            name: node.name.clone(),
            downloads: node.downloads.clone(),
            settings_open: args.open_settings,
            adding: false,
            tick_pending: false,
            browse_menu: false,
            browse_menu_closed: None,
            name_input,
            person_input,
            code_input,
            link_input,
            pass_input,
            up_limit: node.upload_limit,
            down_limit: node.download_limit,
            up_input,
            down_input,
            node,
            peers: Vec::new(),
            selected: HashSet::new(),
            staged: Vec::new(),
            transfers: Vec::new(),
            index: HashMap::new(),
            requests: Vec::new(),
            link_pending: None,
            notice: None,
            info: None,
            reach: Reachability::default(),
            info_token: 0,
            copied: false,
            demo: args.demo_send.map(|p| (p, args.demo_peers.max(1))),
        }
    }

    fn apply(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::Identity { account, person_name } => {
                self.account = account;
                self.code = account.code().into();
                self.person_name = person_name.clone();
                self.person_input.update(cx, |i, cx| i.set_text(&person_name, cx));
            }
            Event::Contacts(list) => self.contacts = list,
            Event::Peers(peers) => {
                self.selected.retain(|id| peers.iter().any(|p| p.id == *id));
                self.peers = peers;
                if let Some((_, wanted)) = &self.demo {
                    if self.peers.len() >= *wanted {
                        let (path, _) = self.demo.take().unwrap();
                        self.selected = self.peers.iter().map(|p| p.id).collect();
                        self.stage(vec![path]);
                        self.send();
                    }
                }
            }
            Event::Transfer(info) => {
                let t = Transfer {
                    info,
                    state: TransferState::Connecting,
                    done: 0,
                    rate: 0.0,
                    sample: (Instant::now(), 0),
                    moved: Instant::now(),
                    paused: (false, false),
                };
                self.transfers.insert(0, t);
                self.reindex();
            }
            Event::State { id, state } => {
                let mut alert = None;
                let mut who = String::new();
                if let Some(t) = self.transfers.get(*self.index.get(&id).unwrap_or(&usize::MAX)) {
                    who = self.display_name(&t.info);
                }
                if let Some(t) = self.get(id) {
                    if state == TransferState::Active {
                        t.sample = (Instant::now(), t.done);
                        t.moved = Instant::now();
                    }
                    if t.info.direction == Direction::Incoming && t.state != state {
                        let what = match t.info.files.len() {
                            1 => t.info.files[0].name.clone(),
                            n => format!("{n} files"),
                        };
                        alert = match &state {
                            TransferState::Waiting => Some((
                                format!("{who} wants to send you {what}"),
                                format!("{} · Click to open Neo and accept or decline.", format::bytes(t.info.total)),
                            )),
                            TransferState::Done { .. } => {
                                Some((format!("Received {what}"), format!("From {who} · {}", format::bytes(t.info.total))))
                            }
                            TransferState::Failed(e) => Some((format!("Couldn't receive {what}"), e.clone())),
                            _ => None,
                        };
                    }
                    t.state = state;
                }
                if let Some((title, body)) = alert {
                    self.alert(&title, &body);
                }
            }
            Event::Progress { id, done } => {
                if let Some(t) = self.get(id) {
                    let now = Instant::now();
                    let dt = now.duration_since(t.sample.0).as_secs_f64();
                    if dt >= 0.25 {
                        let inst = (done.saturating_sub(t.sample.1)) as f64 / dt;
                        t.rate = if t.rate == 0.0 { inst } else { t.rate * 0.7 + inst * 0.3 };
                        t.sample = (now, done);
                    }
                    if done != t.done {
                        t.moved = now;
                    }
                    t.done = done;
                }
            }
            Event::Paused { id, local, remote } => {
                if let Some(t) = self.get(id) {
                    t.paused = (local, remote);
                    t.rate = 0.0;
                    t.sample = (Instant::now(), t.done);
                    t.moved = Instant::now();
                }
            }
            Event::PairRequest { id, name, account, device_name } => {
                self.alert(&format!("{name} wants to add you as a contact"), "Open Neo to accept or decline.");
                self.requests.push(Request::Pair { id, name, account, device_name });
            }
            Event::LinkRequest { id, device_name, sas } => {
                self.alert(&format!("Link “{device_name}” to you?"), &format!("Open Neo and compare the number {sas}."));
                self.requests.push(Request::Link { id, device_name, sas });
            }
            Event::LinkPending { with, sas } => self.link_pending = Some((with, sas)),
            Event::LinkFinished => self.link_pending = None,
            Event::RequestClosed { id } => self.requests.retain(|r| r.id() != id),
            Event::Info(text) => self.show_info(text, cx),
            Event::Gofile(e) => self.apply_gofile(e, cx),
            Event::LinkShared { from, title, url } => {
                self.alert(&format!("{from} sent you a Gofile link"), &title);
                if !self.shared_links.iter().any(|l| l.url == url) {
                    self.shared_links.push(SharedLink { from, title, url });
                }
            }
            Event::Reachability(r) => self.reach = r,
            Event::Notice(n) => self.notice = Some(n.into()),
        }
    }

    /// The best name for whoever is on the other end of a transfer.
    fn display_name(&self, info: &TransferInfo) -> String {
        match info.peer_account {
            Some(a) if a == self.account => format!("your {}", info.peer_name),
            Some(a) => self.contacts.iter().find(|c| c.account == a).map(|c| c.name.clone()).unwrap_or(info.peer_name.clone()),
            None => info.peer_name.clone(),
        }
    }

    fn show_info(&mut self, text: String, cx: &mut Context<Self>) {
        self.info = Some(text.into());
        self.info_token += 1;
        let token = self.info_token;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(6)).await;
            let _ = this.update(cx, |this, cx| {
                if this.info_token == token {
                    this.info = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A Windows notification, only when Neo isn't the window in front.
    fn alert(&self, title: &str, body: &str) {
        if self.hidden || !self.active {
            if let Some(tray) = &self.tray {
                tray.notify(title, body);
            }
        }
    }

    fn on_tray(&mut self, ev: TrayEvent, cx: &mut Context<Self>) {
        match ev {
            TrayEvent::Open => {
                if let Some(h) = self.hwnd {
                    tray::show_window(h);
                }
                self.hidden = false;
                cx.notify();
            }
            TrayEvent::Quit => {
                self.tray = None; // removes the icon right away
                cx.quit();
            }
            TrayEvent::Readd => {
                if let Some(tray) = &self.tray {
                    tray.add();
                }
            }
        }
    }

    /// Closing the window hides it to the tray (if enabled) instead of quitting.
    fn on_close_requested(&mut self, _cx: &mut Context<Self>) -> bool {
        match (&self.tray, self.hwnd) {
            (Some(tray), Some(h)) if self.close_to_tray => {
                tray::hide_window(h);
                self.hidden = true;
                self.active = false;
                if !self.tray_hint_shown {
                    self.tray_hint_shown = true;
                    tray.notify(
                        "Neo is still running",
                        "It keeps receiving files in the background. Right-click the tray icon to quit.",
                    );
                }
                false
            }
            _ => {
                self.tray = None;
                true
            }
        }
    }

    fn get(&mut self, id: u64) -> Option<&mut Transfer> {
        let ix = *self.index.get(&id)?;
        self.transfers.get_mut(ix)
    }

    fn reindex(&mut self) {
        self.index = self.transfers.iter().enumerate().map(|(i, t)| (t.info.id, i)).collect();
    }

    fn stage(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if self.staged.iter().any(|s| s.path == path) {
                continue;
            }
            let meta = std::fs::metadata(&path).ok();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            let size = meta.as_ref().filter(|m| m.is_file()).map(|m| m.len());
            self.staged.push(Staged { path, name, size });
        }
    }

    fn can_send(&self) -> bool {
        !self.staged.is_empty() && !self.selected.is_empty()
    }

    fn send(&mut self) {
        if !self.can_send() {
            return;
        }
        let peers: Vec<Peer> = self.peers.iter().filter(|p| self.selected.contains(&p.id)).cloned().collect();
        let paths = self.staged.drain(..).map(|s| s.path).collect();
        self.node.command(Command::Send { paths, peers });
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        self.pick(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None }, cx);
    }

    fn toggle_browse_menu(&mut self, cx: &mut Context<Self>) {
        let just_closed = self.browse_menu_closed.is_some_and(|t| t.elapsed().as_millis() < 250);
        self.browse_menu = !self.browse_menu && !just_closed;
        cx.notify();
    }

    /// "Browse ▾" plus its little menu: choose files, or choose a folder.
    fn browse_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, title: &'static str, hint: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .px(px(10.))
                .py(px(7.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
                .child(div().text_sm().text_color(rgb(TEXT)).child(title))
                .child(div().text_xs().text_color(rgb(MUTED)).child(hint))
        };
        let menu = div()
            .id("browse-menu")
            .occlude()
            .w(px(220.))
            .p(px(4.))
            .flex()
            .flex_col()
            .rounded(px(9.))
            .bg(rgb(RAISED))
            .border_1()
            .border_color(rgb(BORDER))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if this.browse_menu {
                    this.browse_menu = false;
                    this.browse_menu_closed = Some(Instant::now());
                    cx.notify();
                }
            }))
            .child(item("menu-files", "Choose files…", "Pick one or more files").on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.browse_menu = false;
                    this.browse(cx);
                },
            )))
            .child(item("menu-folder", "Choose a folder…", "Sends everything inside it").on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.browse_menu = false;
                    this.browse_folder(cx);
                },
            )));

        div()
            .flex()
            .flex_col()
            .items_start()
            .child(
                div()
                    .id("browse")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .bg(rgb(if self.browse_menu { HOVER } else { PANEL }))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child("Browse")
                    .child(div().text_color(rgb(MUTED)).child("▾"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.toggle_browse_menu(cx);
                    })),
            )
            .when(self.browse_menu, |d| {
                d.child(deferred(anchored().offset(point(px(0.), px(34.))).snap_to_window().child(menu)).with_priority(1))
            })
    }

    /// Whole folders (with their subfolders) are sent as-is.
    fn browse_folder(&mut self, cx: &mut Context<Self>) {
        self.pick(PathPromptOptions { files: false, directories: true, multiple: true, prompt: Some("Choose folder".into()) }, cx);
    }

    fn pick(&mut self, options: PathPromptOptions, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(options);
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.stage(paths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        for (input, value) in [(&self.name_input, &mut self.name), (&self.person_input, &mut self.person_name)] {
            let typed = input.read(cx).text().trim().to_string();
            if typed.is_empty() {
                let v = value.clone();
                input.update(cx, |i, cx| i.set_text(&v, cx));
            } else {
                *value = typed.chars().take(64).collect();
            }
        }
        self.node.command(Command::UpdateSettings {
            name: self.name.clone(),
            person_name: self.person_name.clone(),
            downloads: self.downloads.clone(),
            close_to_tray: self.close_to_tray,
        });
        cx.notify();
    }

    /// Applies the speed-limit fields (Enter or Save).
    fn save_limits(&mut self, cx: &mut Context<Self>) {
        let up = settings::parse_limit(self.up_input.read(cx).text());
        let down = settings::parse_limit(self.down_input.read(cx).text());
        let (Ok(up), Ok(down)) = (up, down) else {
            self.notice = Some("Speed limits need a number of MB/s, like 5 or 2.5 (leave empty for no limit).".into());
            return cx.notify();
        };
        self.up_limit = up;
        self.down_limit = down;
        self.up_input.update(cx, |i, cx| i.set_text(&settings::limit_text(up), cx));
        self.down_input.update(cx, |i, cx| i.set_text(&settings::limit_text(down), cx));
        self.node.command(Command::SetLimits { up, down });
        cx.notify();
    }

    fn add_by_code(&mut self, cx: &mut Context<Self>) {
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        self.node.command(Command::AddByCode { code });
        self.code_input.update(cx, |i, cx| i.clear(cx));
        self.adding = false;
        cx.notify();
    }

    fn link(&mut self, cx: &mut Context<Self>) {
        let code = self.link_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        self.node.command(Command::LinkWith { code });
        self.link_input.update(cx, |i, cx| i.clear(cx));
        cx.notify();
    }

    fn change_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose download folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(dir) = paths.pop() {
                    let _ = this.update(cx, |this, cx| {
                        this.downloads = dir;
                        this.save_settings(cx);
                    });
                }
            }
        })
        .detach();
    }

    fn clear_finished(&mut self) {
        self.transfers.retain(|t| !t.state.is_finished());
        self.reindex();
    }

    fn copy_code(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.code.to_string()));
        self.copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1400)).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
    }

    // ---------------------------------------------------------------- render

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let copied = self.copied;
        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.))
            .h(px(56.))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    // The app icon's mark (assets/make_icon.py): violet
                    // gradient tile, white N.
                    .child(
                        div()
                            .size(px(24.))
                            .rounded(px(6.))
                            .bg(gpui::linear_gradient(
                                135.,
                                gpui::linear_color_stop(rgb(0x9a8cff), 0.),
                                gpui::linear_color_stop(rgb(0x5845e8), 1.),
                            ))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(14.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_color(rgb(0xffffff))
                            .child("N"),
                    )
                    .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("Neo")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(self.tabs(cx))
                    .child(pill_button("settings", if self.settings_open { "Close settings" } else { "Settings" }, false).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.settings_open = !this.settings_open;
                            if this.settings_open {
                                let (name, person) = (this.name.clone(), this.person_name.clone());
                                this.name_input.update(cx, |i, cx| i.set_text(&name, cx));
                                this.person_input.update(cx, |i, cx| i.set_text(&person, cx));
                            }
                            cx.notify();
                        }),
                    ))
                    .child(
                        div()
                            .id("my-code")
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(10.))
                            .py(px(5.))
                            .rounded(px(8.))
                            .bg(rgb(PANEL))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(self.person_name.clone())))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family("Cascadia Mono")
                                    .text_color(rgb(if copied { OK } else { TEXT }))
                                    .child(if copied { SharedString::from("Copied") } else { self.code.clone() }),
                            )
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_code(cx))),
                    ),
            )
    }

    /// The Send | Gofile switch.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, which: Tab, cx: &mut Context<Self>| {
            let on = self.tab == which && !self.settings_open;
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                .when(!on, |d| d.text_color(rgb(MUTED)).hover(|s| s.text_color(rgb(TEXT))))
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.tab = which;
                    this.settings_open = false;
                    if which == Tab::Gofile {
                        this.gofile_opened();
                    }
                    cx.notify();
                }))
        };
        div()
            .flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(tab("tab-send", "Send", Tab::Send, cx))
            .child(tab("tab-gofile", "Gofile", Tab::Gofile, cx))
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total: u64 = self.staged.iter().filter_map(|s| s.size).sum();
        let n_sel = self.selected.len();
        let ready = self.can_send();
        let send_label = match n_sel {
            0 => "Select a device".to_string(),
            1 => "Send".to_string(),
            n => format!("Send to {n} devices"),
        };

        let drop_zone = div()
            .id("drop-zone")
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .h(px(if self.staged.is_empty() { 130. } else { 64. }))
            .rounded(px(10.))
            .border_1()
            .border_dashed()
            .border_color(rgb(BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(RAISED)))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.border_color(rgb(ACCENT)).bg(rgba(0x7b6cff14)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.stage(paths.paths().to_vec());
                cx.notify();
            }))
            // Clicking anywhere in the drop area asks: files or a folder?
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_browse_menu(cx)))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .child(if self.staged.is_empty() { "Drop files or folders here" } else { "Drop more here, or" }),
            )
            .when(self.staged.is_empty(), |d| d.child(div().text_xs().text_color(rgb(MUTED)).child("any size · or")))
            .child(self.browse_button(cx));

        let staged = div().flex().flex_col().gap(px(2.)).children(self.staged.iter().enumerate().map(|(ix, s)| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(6.))
                .hover(|s| s.bg(rgb(RAISED)))
                .child(file_badge(&s.name, s.size.is_none()))
                .child(div().flex_1().text_sm().truncate().child(SharedString::from(s.name.clone())))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(SharedString::from(s.size.map(format::bytes).unwrap_or_else(|| "Folder".into()))),
                )
                .child(icon_button(("unstage", ix), "×").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if ix < this.staged.len() {
                        this.staged.remove(ix);
                    }
                    cx.notify();
                })))
        }));

        let summary = if self.staged.is_empty() {
            String::new()
        } else {
            let items = if self.staged.len() == 1 { "1 item".to_string() } else { format!("{} items", self.staged.len()) };
            if total > 0 { format!("{items} · {}", format::bytes(total)) } else { items }
        };

        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .rounded(px(12.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(drop_zone)
            .when(!self.staged.is_empty(), |d| d.child(div().id("staged").max_h(px(150.)).overflow_y_scroll().child(staged)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(summary)))
                    .child(
                        div()
                            .id("send")
                            .px(px(16.))
                            .py(px(8.))
                            .rounded(px(8.))
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .when(ready, |d| {
                                d.bg(rgb(ACCENT)).text_color(rgb(0xffffff)).cursor_pointer().hover(|s| s.bg(rgb(ACCENT_HOVER)))
                            })
                            .when(!ready, |d| d.bg(rgb(RAISED)).text_color(rgb(FAINT)))
                            .child(SharedString::from(send_label))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.send();
                                cx.notify();
                            })),
                    ),
            )
    }

    fn activity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Group outgoing transfers of one Send into a single card.
        let mut groups: Vec<(u64, Vec<usize>)> = Vec::new();
        for (ix, t) in self.transfers.iter().enumerate() {
            let key = if t.info.direction == Direction::Outgoing { t.info.job } else { u64::MAX - t.info.id };
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push(ix),
                None => groups.push((key, vec![ix])),
            }
        }
        let any_finished = self.transfers.iter().any(|t| t.state.is_finished());
        let cards: Vec<AnyElement> = groups.iter().map(|(_, ixs)| self.card(ixs, cx)).collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .gap(px(10.))
            .child(
                div().flex().items_center().justify_between().child(section_label("Activity")).when(any_finished, |d| {
                    d.child(
                        text_button("clear", "Clear finished")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.clear_finished();
                                cx.notify();
                            })),
                    )
                }),
            )
            .child(
                div()
                    .id("activity")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .when(cards.is_empty(), |d| {
                        d.child(div().pt(px(24.)).text_sm().text_color(rgb(FAINT)).child("Transfers you send and receive appear here."))
                    })
                    .children(cards),
            )
    }

    fn card(&self, ixs: &[usize], cx: &mut Context<Self>) -> AnyElement {
        let first = &self.transfers[ixs[0]];
        let info = &first.info;
        let incoming = info.direction == Direction::Incoming;
        let title = match info.files.len() {
            0 => "Nothing".to_string(),
            1 => info.files[0].name.clone(),
            n => format!("{} and {} more", info.files[0].name, n - 1),
        };
        let subtitle = if incoming {
            format!("From {} · {}", self.display_name(info), format::bytes(info.total))
        } else {
            let to = if ixs.len() == 1 { self.display_name(info) } else { format!("{} devices", ixs.len()) };
            format!("To {to} · {}", format::bytes(info.total))
        };

        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .rounded(px(12.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(file_badge(info.files.first().map(|f| f.name.as_str()).unwrap_or(""), info.files.len() > 1))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(SharedString::from(title)))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(subtitle))),
                    )
                    .child(div().text_xs().text_color(rgb(FAINT)).child(if incoming { "Incoming" } else { "Outgoing" })),
            );

        for &ix in ixs {
            card = card.child(self.row(ix, ixs.len() > 1 || incoming, cx));
        }
        card.into_any_element()
    }

    fn row(&self, ix: usize, show_peer: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = &self.transfers[ix];
        let id = t.info.id;
        let incoming = t.info.direction == Direction::Incoming;
        let who = self.display_name(&t.info);
        let frac = if t.info.total == 0 { 1.0 } else { (t.done as f64 / t.info.total as f64).clamp(0.0, 1.0) };

        let progress = format!("{} of {}", format::bytes(t.done), format::bytes(t.info.total));
        let (status, color): (String, u32) = match &t.state {
            _ if t.is_paused() && t.paused.0 => (format!("Paused · {progress}"), MUTED),
            _ if t.is_paused() => (format!("Paused by {who} · {progress}"), MUTED),
            TransferState::Connecting => ("Connecting…".into(), MUTED),
            TransferState::Waiting if incoming => ("Wants to send you this".into(), TEXT),
            TransferState::Waiting => ("Waiting for them to accept…".into(), MUTED),
            TransferState::Active if t.moved.elapsed().as_secs_f32() > 2.5 => (
                format!("{} of {} · stalled, waiting for {who}…", format::bytes(t.done), format::bytes(t.info.total)),
                WARN,
            ),
            TransferState::Active => {
                let left = t.info.total.saturating_sub(t.done) as f64;
                let eta = if t.rate > 0.0 { format::eta(left / t.rate) } else { String::new() };
                let mut s = format!("{} of {}", format::bytes(t.done), format::bytes(t.info.total));
                if t.rate > 0.0 {
                    s.push_str(&format!(" · {}", format::rate(t.rate)));
                }
                if !eta.is_empty() {
                    s.push_str(&format!(" · {eta}"));
                }
                (s, MUTED)
            }
            TransferState::Interrupted if incoming => {
                (format!("Interrupted at {} · resumes when {who} reconnects", format::bytes(t.done)), WARN)
            }
            TransferState::Interrupted => ("Connection lost · reconnecting…".into(), WARN),
            TransferState::Done { .. } => (if incoming { "Saved".into() } else { "Delivered".into() }, OK),
            TransferState::Declined => ("Declined".into(), MUTED),
            TransferState::Cancelled => ("Cancelled".into(), MUTED),
            TransferState::Failed(e) => (format!("Failed · {e}"), BAD),
        };

        let bar_color = match &t.state {
            _ if t.is_paused() => FAINT,
            TransferState::Done { .. } => OK,
            TransferState::Failed(_) => BAD,
            TransferState::Interrupted => WARN,
            TransferState::Declined | TransferState::Cancelled => FAINT,
            _ => ACCENT,
        };
        let show_bar = matches!(t.state, TransferState::Active | TransferState::Interrupted | TransferState::Done { .. })
            || (t.state.is_finished() && t.done > 0);

        let mut actions = div().flex().items_center().gap(px(6.));
        match &t.state {
            TransferState::Waiting if incoming => {
                actions = actions
                    .child(pill_button(("decline", id as usize), "Decline", false).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, _| this.node.command(Command::Respond { id, accept: false }),
                    )))
                    .child(pill_button(("accept", id as usize), "Accept", true).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, _| this.node.command(Command::Respond { id, accept: true }),
                    )));
            }
            TransferState::Done { saved } if incoming && !saved.is_empty() => {
                let target = saved[0].clone();
                actions = actions.child(pill_button(("show", id as usize), "Show", false).on_click(cx.listener(
                    move |_, _: &ClickEvent, _, _| {
                        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", target.display())).spawn();
                    },
                )));
            }
            s if !s.is_finished() => {
                // Pause works once the transfer is under way (either side).
                if matches!(s, TransferState::Active | TransferState::Interrupted) || t.paused.0 {
                    let paused = t.paused.0;
                    actions = actions.child(pause_button(("pause", id as usize), paused).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, _| this.node.command(Command::Pause { id, paused: !paused }),
                    )));
                }
                actions = actions.child(icon_button(("cancel", id as usize), "×").on_click(cx.listener(
                    move |this, _: &ClickEvent, _, _| this.node.command(Command::Cancel { id }),
                )));
            }
            _ => {}
        }

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .when(show_peer && !incoming, |d| d.child(avatar_small(&who, t.info.peer_id.0[0])))
                    .when(show_peer && !incoming, |d| {
                        d.child(div().text_sm().truncate().max_w(px(220.)).child(SharedString::from(who.clone())))
                    })
                    .child(div().flex_1().text_xs().text_color(rgb(color)).truncate().child(SharedString::from(status)))
                    .child(actions),
            )
            .when(show_bar, |d| {
                d.child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(rgb(TRACK))
                        .child(div().h_full().rounded_full().bg(rgb(bar_color)).w(relative(frac as f32))),
                )
            })
            .into_any_element()
    }
}

impl Render for NeoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // While a transfer is active, re-render once a second so a silent
        // connection shows as stalled. Idle windows schedule nothing.
        if !self.tick_pending && self.transfers.iter().any(|t| t.state == TransferState::Active && !t.is_paused()) {
            self.tick_pending = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    this.tick_pending = false;
                    cx.notify();
                });
            })
            .detach();
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .child(self.header(cx))
            .child(
                div().flex().flex_1().min_h(px(0.)).when(self.tab == Tab::Send || self.settings_open, |d| d.child(self.sidebar(cx))).child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .gap(px(14.))
                        .p(px(20.))
                        .children(self.banners(cx))
                        .when(self.settings_open, |d| d.child(self.settings(cx)))
                        .when(!self.settings_open && self.tab == Tab::Gofile, |d| d.child(self.gofile_page(cx)))
                        .when(!self.settings_open && self.tab == Tab::Send, |d| {
                            d.children(self.request_cards(cx)).child(self.composer(cx)).child(self.activity(cx))
                        }),
                ),
            )
    }
}

// ---------------------------------------------------------------- widgets

fn section_label(text: impl Into<SharedString>) -> impl IntoElement {
    div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(rgb(MUTED)).child(text.into())
}

fn hue(seed: u8) -> f32 {
    seed as f32 / 255.0
}

fn initial(name: &str) -> SharedString {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect::<String>()).unwrap_or_else(|| "?".into()).into()
}

fn avatar(name: &str, seed: u8) -> gpui::Div {
    div()
        .size(px(30.))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(Hsla { h: hue(seed), s: 0.45, l: 0.30, a: 1.0 })
        .text_color(Hsla { h: hue(seed), s: 0.80, l: 0.85, a: 1.0 })
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial(name))
}

fn avatar_small(name: &str, seed: u8) -> impl IntoElement {
    div()
        .size(px(20.))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(Hsla { h: hue(seed), s: 0.45, l: 0.30, a: 1.0 })
        .text_color(Hsla { h: hue(seed), s: 0.80, l: 0.85, a: 1.0 })
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial(name))
}

fn switch(on: bool) -> impl IntoElement {
    div()
        .w(px(36.))
        .h(px(20.))
        .flex_none()
        .rounded_full()
        .p(px(2.))
        .flex()
        .when(on, |d| d.bg(rgb(ACCENT)).justify_end())
        .when(!on, |d| d.bg(rgb(TRACK)).justify_start())
        .child(div().size(px(16.)).rounded_full().bg(rgb(0xffffff)))
}

fn checkbox(on: bool) -> impl IntoElement {
    div()
        .size(px(18.))
        .flex_none()
        .rounded(px(5.))
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .when(on, |d| d.bg(rgb(ACCENT)).border_color(rgb(ACCENT)).child(div().size(px(6.)).rounded_full().bg(rgb(0xffffff))))
        .when(!on, |d| d.border_color(rgb(FAINT)))
}

fn file_badge(name: &str, many: bool) -> impl IntoElement {
    let ext = if many {
        "•••".to_string()
    } else {
        name.rsplit_once('.').map(|(_, e)| e.to_uppercase()).filter(|e| e.len() <= 4).unwrap_or_else(|| "FILE".into())
    };
    div()
        .size(px(34.))
        .flex_none()
        .rounded(px(8.))
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(MUTED))
        .child(SharedString::from(ext))
}

fn icon_button(id: impl Into<gpui::ElementId>, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(22.))
        .flex_none()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(FAINT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
        .child(glyph)
}

/// Pause (two bars) or resume (a play triangle), sized like `icon_button`.
fn pause_button(id: impl Into<gpui::ElementId>, paused: bool) -> gpui::Stateful<gpui::Div> {
    let b = div()
        .id(id)
        .size(px(22.))
        .flex_none()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(3.))
        .text_color(rgb(FAINT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)));
    if paused {
        b.text_size(px(10.)).child("▶")
    } else {
        let bar = || div().w(px(3.)).h(px(10.)).rounded(px(1.)).bg(rgb(FAINT));
        b.child(bar()).child(bar())
    }
}

fn text_button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div().id(id).text_xs().text_color(rgb(MUTED)).cursor_pointer().hover(|s| s.text_color(rgb(TEXT))).child(label.into())
}

fn pill_button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, primary: bool) -> gpui::Stateful<gpui::Div> {
    let (bg, hover, fg): (Rgba, Rgba, Rgba) = if primary {
        (rgb(ACCENT), rgb(ACCENT_HOVER), rgb(0xffffff))
    } else {
        (rgb(RAISED), rgb(HOVER), rgb(TEXT))
    };
    div()
        .id(id)
        .flex_none()
        .px(px(12.))
        .py(px(4.))
        .rounded(px(7.))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(label.into())
}

/// A rounded panel used for cards and settings sections.
fn panel() -> gpui::Div {
    div().flex().flex_col().p(px(16.)).rounded(px(12.)).bg(rgb(PANEL)).border_1().border_color(rgb(BORDER))
}
