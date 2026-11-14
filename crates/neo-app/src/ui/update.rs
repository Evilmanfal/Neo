//! The update prompt: "Neo vX is available" with Update now / Later, a small
//! "Update" pill in the header after Later, waiting for transfers to finish,
//! download progress, then swap the exe and restart (see `neo_core::update`).

use neo_core::update::{Release, UpdateEvent};

use super::*;

/// Where the update stands in the UI.
#[derive(Default)]
pub(super) struct UpdateState {
    pub release: Option<Release>,
    /// The full prompt is showing.
    pub prompt: bool,
    /// "Later" was picked for this version (until the next start).
    pub postponed: Option<String>,
    /// Update now was picked while transfers were running.
    pub busy: bool,
    /// Install as soon as the transfers finish.
    pub when_idle: bool,
    pub progress: Option<(u64, u64)>,
}

impl NeoView {
    pub(super) fn apply_update(&mut self, e: UpdateEvent, cx: &mut Context<Self>) {
        match e {
            UpdateEvent::Available(rel) => {
                // A version newer than the one put off brings the prompt back.
                let put_off = self.up.postponed.as_deref().is_some_and(|v| !neo_core::update::newer(&rel.version, v));
                self.up.prompt = !put_off;
                self.up.release = Some(rel);
            }
            UpdateEvent::Progress { done, total } => self.up.progress = Some((done, total)),
            UpdateEvent::Ready(path) => self.install_and_restart(path, cx),
            UpdateEvent::Failed(e) => {
                self.up.progress = None;
                self.notice = Some(format!("Update failed: {e}").into());
            }
        }
    }

    fn transfers_running(&self) -> usize {
        self.transfers.iter().filter(|t| !t.state.is_finished()).count()
    }

    fn start_update(&mut self, cx: &mut Context<Self>) {
        if self.transfers_running() > 0 {
            self.up.busy = true;
        } else {
            self.up.busy = false;
            self.up.when_idle = false;
            self.up.progress = Some((0, 0));
            self.node.command(Command::DownloadUpdate);
        }
        cx.notify();
    }

    /// Called after every event: starts a waiting update once transfers end.
    pub(super) fn update_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.up.when_idle && self.up.progress.is_none() && self.transfers_running() == 0 {
            self.start_update(cx);
        }
    }

    fn install_and_restart(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let result = std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| {
            neo_core::update::install(&path, &exe)?;
            let mut args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--updated").collect();
            args.push("--updated".into());
            std::process::Command::new(&exe).args(args).spawn()?;
            Ok(())
        });
        match result {
            Ok(()) => {
                self.tray = None; // remove the tray icon before going
                cx.quit();
            }
            Err(e) => {
                self.up.progress = None;
                self.notice = Some(format!("Couldn't install the update: {e:#}").into());
            }
        }
    }

    /// The small header button once the prompt was put off.
    pub(super) fn update_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rel = self.up.release.as_ref()?;
        if self.up.prompt {
            return None;
        }
        let label = if self.up.progress.is_some() { "Updating…".to_string() } else { format!("Update to v{}", rel.version) };
        Some(
            pill_button("update-pill", label, true)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.up.prompt = true;
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// The prompt card at the top of the page.
    pub(super) fn update_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rel = self.up.release.as_ref()?;
        if !self.up.prompt {
            return None;
        }
        let version = rel.version.clone();
        let card = panel().gap(px(10.)).border_color(rgb(ACCENT));
        let title = |t: String| div().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(t));
        let body = |t: String| div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(t));
        let row = || div().flex().justify_end().gap(px(8.));
        let later = || {
            pill_button("update-later", "Later", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.up.prompt = false;
                this.up.busy = false;
                this.up.postponed = this.up.release.as_ref().map(|r| r.version.clone());
                cx.notify();
            }))
        };
        let card = if let Some((done, total)) = self.up.progress {
            let frac = if total == 0 { 0.0 } else { (done as f64 / total as f64).clamp(0.0, 1.0) };
            let what = if total == 0 { "Starting…".to_string() } else { format!("{} of {}", format::bytes(done), format::bytes(total)) };
            card.child(title(format!("Downloading Neo v{version}…")))
                .child(body(format!("{what} · Neo restarts by itself when it's ready.")))
                .child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(rgb(TRACK))
                        .child(div().h_full().rounded_full().bg(rgb(ACCENT)).w(relative(frac as f32))),
                )
        } else if self.up.when_idle {
            card.child(title(format!("Neo v{version} will install when your transfers finish")))
                .child(body("Neo restarts by itself then.".into()))
                .child(row().child(pill_button("update-nowait", "Don't wait", false).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.up.when_idle = false;
                        this.up.prompt = false;
                        cx.notify();
                    },
                ))))
        } else if self.up.busy {
            let n = self.transfers_running();
            card.child(title(format!("{n} transfer{} still running", if n == 1 { " is" } else { "s are" })))
                .child(body(format!(
                    "Neo v{version} can install when they finish, or cancel them first. Interrupted transfers pick up again after the restart."
                )))
                .child(row().child(later()).child(pill_button("update-idle", "Update when they finish", true).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.up.busy = false;
                        this.up.when_idle = true;
                        this.update_if_idle(cx);
                        cx.notify();
                    },
                ))))
        } else {
            let mut c = card.child(title(format!("Neo v{version} is available")));
            if !rel.notes.is_empty() {
                c = c.child(body(rel.notes.clone()));
            }
            c.child(row().child(later()).child(pill_button("update-now", "Update now", true).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| this.start_update(cx),
            ))))
        };
        Some(card.into_any_element())
    }
}
