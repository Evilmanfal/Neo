//! The Gofile tab: connect your own Gofile account, upload files and
//! folders with sharing options, and manage "My uploads".
//! Nothing here touches the network until you act (or open the tab, which
//! refreshes account details at most every few minutes).

use std::time::{Duration, Instant};

use neo_core::gofile::{region_label, AccountInfo, GofileCommand, GofileEvent, ShareOptions, UploadRecord, REGIONS};

use super::*;

/// An upload in progress.
pub(super) struct GUpload {
    pub upload: u64,
    pub name: String,
    pub total: u64,
    pub done: u64,
    pub rate: f64,
    /// Progress samples from the last few seconds, for a steady speed.
    pub samples: std::collections::VecDeque<(Instant, u64)>,
    pub paused: bool,
}

/// Speed is averaged over this window: folders upload in bursts (Gofile
/// answers each file before the next byte counts), so a short window jumps.
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// Everything the Gofile tab shows.
pub(super) struct GofileState {
    pub account: Option<AccountInfo>,
    pub records: Vec<UploadRecord>,
    pub busy: bool,
    pub email_sent: Option<String>,
    pub staged: Vec<Staged>,
    pub public: bool,
    pub uploads: Vec<GUpload>,
    /// Record showing its QR code / just uploaded.
    pub qr: Option<String>,
    pub renaming: Option<String>,
    pub confirm_delete: Option<String>,
    pub share_menu: Option<String>,
    pub copied: Option<String>,
    pub refreshed: Option<Instant>,
    /// Upload server: "" = Auto, "gofile" = Gofile's pick, or a region key.
    pub region: String,
    /// The region Auto last picked.
    pub fastest: Option<String>,
    pub region_menu: bool,
    pub email: Entity<TextInput>,
    pub token: Entity<TextInput>,
    pub password: Entity<TextInput>,
    pub expiry: Entity<TextInput>,
    pub description: Entity<TextInput>,
    pub rename: Entity<TextInput>,
}

impl GofileState {
    pub fn new(cx: &mut Context<NeoView>) -> Self {
        GofileState {
            account: None,
            records: Vec::new(),
            busy: false,
            email_sent: None,
            staged: Vec::new(),
            public: true,
            uploads: Vec::new(),
            qr: None,
            renaming: None,
            confirm_delete: None,
            share_menu: None,
            copied: None,
            refreshed: None,
            region: String::new(),
            fastest: None,
            region_menu: false,
            email: cx.new(|cx| TextInput::new(cx, "you@example.com", "")),
            token: cx.new(|cx| TextInput::new(cx, "Paste your Gofile API token", "").masked()),
            password: cx.new(|cx| TextInput::new(cx, "Password (optional)", "").masked()),
            expiry: cx.new(|cx| TextInput::new(cx, "Expires YYYY-MM-DD (optional)", "")),
            description: cx.new(|cx| TextInput::new(cx, "Description (optional)", "")),
            rename: cx.new(|cx| TextInput::new(cx, "New name", "")),
        }
    }
}

impl NeoView {
    pub(super) fn gofile_command(&self, cmd: GofileCommand) {
        self.node.command(Command::Gofile(cmd));
    }

    /// Opening the tab refreshes account details, at most every 5 minutes.
    pub(super) fn gofile_opened(&mut self) {
        let stale = self.g.refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(300));
        if self.g.account.is_some() && stale {
            self.g.refreshed = Some(Instant::now());
            self.gofile_command(GofileCommand::Refresh);
        }
    }

    pub(super) fn apply_gofile(&mut self, ev: GofileEvent, cx: &mut Context<Self>) {
        match ev {
            GofileEvent::Account(a) => {
                if a.is_some() {
                    self.g.token.update(cx, |i, cx| i.clear(cx));
                    self.g.email_sent = None;
                    if let Some(path) = self.demo_gofile.take() {
                        self.gofile_stage(vec![path]);
                        self.g.public = true;
                        if !self.demo_gofile_stage_only {
                            self.gofile_upload(cx);
                        }
                    }
                }
                self.g.account = a;
            }
            GofileEvent::Records(r) => self.g.records = r,
            GofileEvent::EmailSent(email) => self.g.email_sent = Some(email),
            GofileEvent::Busy(b) => self.g.busy = b,
            GofileEvent::Region { choice, fastest } => {
                self.g.region = choice;
                if fastest.is_some() {
                    self.g.fastest = fastest;
                }
            }
            GofileEvent::UploadStarted { upload, name, total, .. } => {
                self.g.uploads.push(GUpload { upload, name, total, done: 0, rate: 0.0, samples: [(Instant::now(), 0)].into(), paused: false })
            }
            GofileEvent::UploadProgress { upload, done } => {
                if let Some(u) = self.g.uploads.iter_mut().find(|u| u.upload == upload) {
                    let now = Instant::now();
                    u.samples.push_back((now, done));
                    while u.samples.len() > 2 && u.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW) {
                        u.samples.pop_front();
                    }
                    if let Some(&(t0, d0)) = u.samples.front() {
                        let dt = now.duration_since(t0).as_secs_f64();
                        if dt >= 1.0 {
                            u.rate = done.saturating_sub(d0) as f64 / dt;
                        }
                    }
                    u.done = done;
                }
            }
            GofileEvent::UploadPaused { upload, paused } => {
                if let Some(u) = self.g.uploads.iter_mut().find(|u| u.upload == upload) {
                    u.paused = paused;
                    u.rate = 0.0;
                    u.samples = [(Instant::now(), u.done)].into();
                }
            }
            GofileEvent::UploadFinished { upload, record, error, cancelled } => {
                let name = self.g.uploads.iter().find(|u| u.upload == upload).map(|u| u.name.clone()).unwrap_or_default();
                self.g.uploads.retain(|u| u.upload != upload);
                if let Some(r) = record {
                    self.g.qr = Some(r.id.clone());
                    self.show_info(format!("Uploaded {name} to Gofile"), cx);
                    self.alert("Gofile upload finished", &format!("{name} is ready to share."));
                } else if cancelled {
                    self.show_info(format!("Cancelled the upload of {name}"), cx);
                } else if let Some(e) = error {
                    self.notice = Some(format!("Upload of {name} failed: {e}").into());
                }
            }
        }
    }

    fn gofile_pick(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Choose files".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.gofile_stage(paths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn gofile_stage(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if self.g.staged.iter().any(|s| s.path == path) {
                continue;
            }
            let meta = std::fs::metadata(&path).ok();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
            let size = meta.as_ref().filter(|m| m.is_file()).map(|m| m.len());
            self.g.staged.push(Staged { path, name, size });
        }
    }

    fn gofile_upload(&mut self, cx: &mut Context<Self>) {
        if self.g.staged.is_empty() {
            return;
        }
        let expiry_text = self.g.expiry.read(cx).text().trim().to_string();
        let expiry = if expiry_text.is_empty() {
            None
        } else {
            match format::parse_date(&expiry_text) {
                Some(t) => Some(t),
                None => {
                    self.notice = Some("Write the expiry date as YYYY-MM-DD, e.g. 2026-12-31.".into());
                    return cx.notify();
                }
            }
        };
        let password = Some(self.g.password.read(cx).text().to_string()).filter(|p| !p.is_empty());
        let options = ShareOptions {
            public: self.g.public,
            password,
            expiry,
            description: self.g.description.read(cx).text().trim().to_string(),
        };
        let paths = self.g.staged.drain(..).map(|s| s.path).collect();
        self.gofile_command(GofileCommand::Upload { paths, options });
        for input in [&self.g.password, &self.g.expiry, &self.g.description] {
            input.update(cx, |i, cx| i.clear(cx));
        }
        cx.notify();
    }

    fn copy_link(&mut self, id: &str, link: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(link.to_string()));
        self.g.copied = Some(id.to_string());
        cx.notify();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1400)).await;
            let _ = this.update(cx, |this, cx| {
                if this.g.copied.as_deref() == Some(id.as_str()) {
                    this.g.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---------------------------------------------------------------- render

    pub(super) fn gofile_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.g.account {
            None => self.gofile_connect(cx).into_any_element(),
            Some(a) => div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .child(self.gofile_account_bar(a, cx))
                .child(self.gofile_composer(cx))
                .children(self.gofile_progress(cx))
                .child(self.gofile_records(cx))
                .into_any_element(),
        };
        div()
            .id("gofile-page")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(div().flex().flex_col().gap(px(14.)).pb(px(20.)).child(body))
            .into_any_element()
    }

    fn gofile_connect(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let step = |n: &'static str, title: &'static str, hint: &'static str| {
            div()
                .flex()
                .gap(px(12.))
                .child(
                    div()
                        .size(px(22.))
                        .flex_none()
                        .rounded_full()
                        .bg(rgb(RAISED))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(n),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(hint)),
                )
        };
        panel()
            .gap(px(16.))
            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("Gofile"))
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                "Upload files to your own Gofile account and share download links that anyone can open in a browser. \
                 Gofile is a separate service; you need a free account there (not a guest account).",
            ))
            .child(step("1", "Create or sign in to your Gofile account", "Sign up with your email on gofile.io, or let Gofile email you a sign-in link."))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(34.))
                    .child(pill_button("gf-open", "Open gofile.io", false).on_click(|_, _, _| {
                        let _ = std::process::Command::new("explorer").arg("https://gofile.io/myprofile").spawn();
                    }))
                    .child(div().text_xs().text_color(rgb(FAINT)).child("or")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(34.))
                    .child(div().flex_1().child(self.g.email.clone()))
                    .child(pill_button("gf-email", "Email me a sign-in link", false).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            let email = this.g.email.read(cx).text().trim().to_string();
                            this.gofile_command(GofileCommand::CreateAccount { email });
                        },
                    ))),
            )
            .when_some(self.g.email_sent.clone(), |d, email| {
                d.child(div().pl(px(34.)).text_xs().text_color(rgb(OK)).child(SharedString::from(format!(
                    "Gofile emailed a sign-in link to {email}. Open it, then copy the API token from your profile page."
                ))))
            })
            .child(step("2", "Paste your API token", "On gofile.io, open your profile and copy the API token. Neo keeps it encrypted on this PC."))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(34.))
                    .child(div().flex_1().child(self.g.token.clone()))
                    .child(pill_button("gf-connect", if self.g.busy { "Checking…" } else { "Connect" }, true).on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            let token = this.g.token.read(cx).text().trim().to_string();
                            if !token.is_empty() {
                                this.gofile_command(GofileCommand::Connect { token });
                            }
                        }),
                    )),
            )
    }

    fn gofile_account_bar(&self, a: &AccountInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let mut usage = format!("{} used · {} files · {} folders", format::bytes(a.storage), a.file_count, a.folder_count);
        for (k, v) in &a.extra {
            let key = k.to_ascii_lowercase();
            let shown = if key.contains("traffic") || key.contains("storage") { format::bytes(*v) } else { v.to_string() };
            usage.push_str(&format!(" · {k}: {shown}"));
        }
        let tier = if a.tier.is_empty() { "standard".to_string() } else { a.tier.clone() };
        panel()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .size(px(30.))
                    .flex_none()
                    .rounded(px(8.))
                    .bg(rgb(RAISED))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(MUTED))
                    .child("GO"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(div().text_sm().truncate().child(SharedString::from(format!("{} · {tier}", a.email))))
                    .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(SharedString::from(usage))),
            )
            .child(text_button("gf-refresh", if self.g.busy { "Refreshing…" } else { "Refresh" }).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.g.refreshed = Some(Instant::now());
                    this.gofile_command(GofileCommand::Refresh);
                    cx.notify();
                },
            )))
            .child(text_button("gf-disconnect", "Disconnect").on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                this.gofile_command(GofileCommand::Disconnect);
            })))
    }

    fn gofile_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total: u64 = self.g.staged.iter().filter_map(|s| s.size).sum();
        let ready = !self.g.staged.is_empty();
        let drop_zone = div()
            .id("gf-drop")
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .h(px(if ready { 70. } else { 110. }))
            .rounded(px(10.))
            .border_1()
            .border_dashed()
            .border_color(rgb(BORDER))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.border_color(rgb(ACCENT)).bg(rgba(0x7b6cff14)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.gofile_stage(paths.paths().to_vec());
                cx.notify();
            }))
            .child(div().text_sm().child(if ready { "Drop more here, or" } else { "Drop files or folders to upload to Gofile" }))
            .child(pill_button("gf-files", "Choose files…", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.gofile_pick(cx))));
        let staged = div().flex().flex_col().gap(px(2.)).children(self.g.staged.iter().enumerate().map(|(ix, s)| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(5.))
                .child(file_badge(&s.name, s.size.is_none()))
                .child(div().flex_1().text_sm().truncate().child(SharedString::from(s.name.clone())))
                .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(s.size.map(format::bytes).unwrap_or_else(|| "Folder".into()))))
                .child(icon_button(("gf-unstage", ix), "×").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if ix < this.g.staged.len() {
                        this.g.staged.remove(ix);
                    }
                    cx.notify();
                })))
        }));
        let public = self.g.public;
        panel()
            .gap(px(12.))
            .child(drop_zone)
            .when(ready, |d| {
                d.child(div().id("gf-staged").max_h(px(140.)).overflow_y_scroll().child(staged))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_1().child(self.g.password.clone()))
                            .child(div().w(px(220.)).child(self.g.expiry.clone())),
                    )
                    .child(self.g.description.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .id("gf-public")
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .cursor_pointer()
                                    .child(switch(public))
                                    .child(div().text_xs().text_color(rgb(MUTED)).child(if public {
                                        "Anyone with the link can download"
                                    } else {
                                        "Private: only you, signed in to Gofile"
                                    }))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.g.public = !this.g.public;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from({
                                        let n = self.g.staged.len();
                                        let items = format!("{n} item{}", if n == 1 { "" } else { "s" });
                                        if total > 0 { format!("{items} · {}", format::bytes(total)) } else { items }
                                    })))
                                    .child(pill_button("gf-upload", "Upload to Gofile", true).on_click(cx.listener(
                                        |this, _: &ClickEvent, _, cx| this.gofile_upload(cx),
                                    ))),
                            ),
                    )
                    .child(self.gofile_region(cx))
                    .child(div().text_xs().text_color(rgb(FAINT)).child(
                        "Files are uploaded as they are (not encrypted by Neo) so anyone with the link can open them. \
                         On free accounts, an expiry date deletes the upload, and Gofile removes files nobody downloads for 10 days.",
                    ))
            })
    }

    /// "Server: Auto (New York)" and, when opened, a row of choices.
    fn gofile_region(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = match self.g.region.as_str() {
            "" => match self.g.fastest.as_deref().and_then(region_label) {
                Some(l) => format!("Auto (fastest: {l})"),
                None => "Auto (fastest)".to_string(),
            },
            "gofile" => "Gofile's choice".to_string(),
            key => region_label(key).unwrap_or("Auto").to_string(),
        };
        let mut choices: Vec<(String, String)> = vec![(String::new(), "Auto (fastest)".into())];
        choices.extend(REGIONS.iter().map(|(k, l)| (k.to_string(), l.to_string())));
        choices.push(("gofile".into(), "Gofile's choice".into()));
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("Upload server"))
                    .child(text_button("gf-region", SharedString::from(format!("{current} ▾"))).on_click(cx.listener(
                        |this, _: &ClickEvent, _, cx| {
                            this.g.region_menu = !this.g.region_menu;
                            cx.notify();
                        },
                    ))),
            )
            .when(self.g.region_menu, |d| {
                d.child(div().flex().flex_wrap().gap(px(6.)).children(choices.into_iter().map(|(key, label)| {
                    let selected = key == self.g.region;
                    pill_button(SharedString::from(format!("gf-region-{key}")), label, selected).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.g.region = key.clone();
                            this.g.region_menu = false;
                            this.gofile_command(GofileCommand::SetRegion { region: key.clone() });
                            cx.notify();
                        },
                    ))
                })))
            })
    }

    fn gofile_progress(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.g
            .uploads
            .iter()
            .map(|u| {
                let upload = u.upload;
                let frac = if u.total == 0 { 1.0 } else { (u.done as f64 / u.total as f64).clamp(0.0, 1.0) };
                let paused = u.paused;
                let mut status = format!("{} of {}", format::bytes(u.done), format::bytes(u.total));
                if paused {
                    status = format!("Paused · {status}");
                } else if u.rate > 0.0 {
                    status.push_str(&format!(" · {} · {}", format::rate(u.rate), format::eta(u.total.saturating_sub(u.done) as f64 / u.rate)));
                }
                panel()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().flex_1().text_sm().truncate().child(SharedString::from(format!("Uploading {}", u.name))))
                            .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(status)))
                            .child(pause_button(("gf-pause", upload as usize), paused).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, _| this.gofile_command(GofileCommand::Pause { upload, paused: !paused }),
                            )))
                            .child(icon_button(("gf-cancel", upload as usize), "×").on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.gofile_command(GofileCommand::Cancel { upload });
                            }))),
                    )
                    .child(
                        div()
                            .h(px(4.))
                            .w_full()
                            .rounded_full()
                            .bg(rgb(TRACK))
                            .child(div().h_full().rounded_full().bg(rgb(if paused { FAINT } else { ACCENT })).w(relative(frac as f32))),
                    )
                    .when(paused, |d| {
                        d.child(div().text_xs().text_color(rgb(FAINT)).child(
                            "Gofile can't resume a file, so after a long pause the file in progress may start over. Finished files stay uploaded.",
                        ))
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn gofile_records(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let premium = self.g.account.as_ref().is_some_and(|a| a.is_premium());
        let mut list = panel().gap(px(4.)).child(
            div()
                .flex()
                .justify_between()
                .pb(px(6.))
                .child(section_label("My uploads"))
                .child(div().text_xs().text_color(rgb(FAINT)).child(if premium {
                    "From your Gofile account"
                } else {
                    "Uploaded from this PC"
                })),
        );
        if self.g.records.is_empty() {
            list = list.child(div().py(px(10.)).text_xs().text_color(rgb(FAINT)).child("Nothing uploaded yet."));
        }
        for r in &self.g.records {
            list = list.child(self.gofile_record(r, cx));
        }
        list
    }

    fn gofile_record(&self, r: &UploadRecord, cx: &mut Context<Self>) -> impl IntoElement {
        let id = r.id.clone();
        let key = |p: &str| SharedString::from(format!("{p}-{}", r.id));
        let mut tags = vec![format::bytes(r.size), format::date(r.created)];
        if r.files > 1 {
            tags.insert(1, format!("{} files", r.files));
        }
        if !r.public {
            tags.push("private".into());
        }
        if r.password {
            tags.push("password".into());
        }
        if let Some(e) = r.expiry {
            tags.push(format!("expires {}", format::date(e)));
        }
        let renaming = self.g.renaming.as_deref() == Some(r.id.as_str());
        let confirming = self.g.confirm_delete.as_deref() == Some(r.id.as_str());
        let sharing = self.g.share_menu.as_deref() == Some(r.id.as_str());
        let show_qr = self.g.qr.as_deref() == Some(r.id.as_str());
        let copied = self.g.copied.as_deref() == Some(r.id.as_str());
        let link = r.link.clone();

        let actions = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .when(!link.is_empty(), |d| {
                let (id, link) = (id.clone(), link.clone());
                d.child(pill_button(key("copy"), if copied { "Copied" } else { "Copy link" }, false).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| this.copy_link(&id, &link, cx),
                )))
            })
            .child({
                let id = id.clone();
                text_button(key("qr"), if show_qr { "Hide QR" } else { "QR" }).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.g.qr = if this.g.qr.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                    cx.notify();
                }))
            })
            .child({
                let id = id.clone();
                text_button(key("send"), "Send to…").on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.g.share_menu = if this.g.share_menu.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                    cx.notify();
                }))
            })
            .child({
                let (id, name) = (id.clone(), r.name.clone());
                text_button(key("rename"), "Rename").on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.g.renaming = Some(id.clone());
                    this.g.rename.update(cx, |i, cx| i.set_text(&name, cx));
                    window.focus(&gpui::Focusable::focus_handle(&this.g.rename, cx));
                    cx.notify();
                }))
            })
            .child({
                let id = id.clone();
                div()
                    .id(key("delete"))
                    .text_xs()
                    .cursor_pointer()
                    .text_color(rgb(if confirming { BAD } else { MUTED }))
                    .hover(|s| s.text_color(rgb(BAD)))
                    .child(if confirming { "Delete permanently?" } else { "Delete" })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if this.g.confirm_delete.as_deref() == Some(id.as_str()) {
                            this.g.confirm_delete = None;
                            this.gofile_command(GofileCommand::Delete { id: id.clone() });
                        } else {
                            this.g.confirm_delete = Some(id.clone());
                        }
                        cx.notify();
                    }))
            });

        let title: AnyElement = if renaming {
            let id = id.clone();
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().flex_1().child(self.g.rename.clone()))
                .child(pill_button(key("rename-save"), "Save", true).on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let name = this.g.rename.read(cx).text().trim().to_string();
                    this.g.renaming = None;
                    if !name.is_empty() {
                        this.gofile_command(GofileCommand::Rename { id: id.clone(), name });
                    }
                    cx.notify();
                })))
                .child(text_button(key("rename-cancel"), "Cancel").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.g.renaming = None;
                    cx.notify();
                })))
                .into_any_element()
        } else {
            div().text_sm().truncate().child(SharedString::from(r.name.clone())).into_any_element()
        };

        // Contacts (and your own devices) that are reachable right now.
        let targets: Vec<Peer> = self
            .peers
            .iter()
            .filter(|p| p.account.is_some_and(|a| a == self.account || self.contacts.iter().any(|c| c.account == a)))
            .cloned()
            .collect();

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .py(px(8.))
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(file_badge(&r.name, r.files > 1))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .child(title)
                            .child(div().text_xs().text_color(rgb(MUTED)).truncate().child(SharedString::from(tags.join(" · ")))),
                    ),
            )
            .child(actions)
            .when(sharing, |d| {
                let mut row = div().flex().flex_wrap().items_center().gap(px(6.)).child(div().text_xs().text_color(rgb(FAINT)).child("Send the link to:"));
                if targets.is_empty() {
                    row = row.child(div().text_xs().text_color(rgb(FAINT)).child("no contacts are reachable right now"));
                }
                for p in targets {
                    let id = id.clone();
                    let label = self
                        .contacts
                        .iter()
                        .find(|c| Some(c.account) == p.account)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| format!("your {}", p.name));
                    row = row.child(pill_button(SharedString::from(format!("send-{}-{}", r.id, p.id.to_hex())), label, false).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.g.share_menu = None;
                            this.gofile_command(GofileCommand::ShareLink { id: id.clone(), peer: p.clone() });
                            cx.notify();
                        }),
                    ));
                }
                d.child(row)
            })
            .when(show_qr && !link.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(settings::qr(&link))
                        .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(link.clone()))),
                )
            })
    }
}
