//! The settings page: you (name, code, QR), this device, contacts, linking
//! your devices, and the encrypted identity backup.

use qrcodegen::{QrCode, QrCodeEcc};

use super::*;

impl NeoView {
    pub(super) fn settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let downloads = self.downloads.clone();
        let name_changed = changed(&self.name_input, &self.name, cx);
        let person_changed = changed(&self.person_input, &self.person_name, cx);

        let you = panel()
            .gap(px(14.))
            .child(heading("You"))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(18.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(14.))
                            .child(field("Your name", "Shown to people you add as contacts."))
                            .child(input_row(self.person_input.clone(), "save-person", person_changed, cx))
                            .child(field("Your code", "Share it so people can add you. Works for all your linked devices."))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().flex_1().text_size(px(18.)).font_family("Cascadia Mono").child(self.code.clone()))
                                    .child(pill_button("copy-code", if self.copied { "Copied" } else { "Copy" }, false).on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.copy_code(cx)),
                                    )),
                            ),
                    )
                    .child(qr(&format!("neo://add/{}", self.code))),
            );

        let device = panel()
            .gap(px(14.))
            .child(heading("This device"))
            .child(field("Device name", "How this computer appears to your contacts and your other devices."))
            .child(input_row(self.name_input.clone(), "save-name", name_changed, cx))
            .child(divider())
            .child(field("Download folder", "Where received files are saved."))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .px(px(10.))
                            .py(px(7.))
                            .rounded(px(8.))
                            .bg(rgb(RAISED))
                            .text_sm()
                            .truncate()
                            .child(SharedString::from(self.downloads.display().to_string())),
                    )
                    .child(pill_button("change-folder", "Change…", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.change_folder(cx))))
                    .child(pill_button("open-folder", "Open", false).on_click(move |_, _, _| {
                        let _ = std::fs::create_dir_all(&downloads);
                        let _ = std::process::Command::new("explorer").arg(&downloads).spawn();
                    })),
            )
            .child(divider())
            .child(field(
                "Speed limits",
                "Caps the total for everything Neo sends or receives, Gofile uploads included. Leave empty for no limit.",
            ))
            .child(self.limits_row(cx))
            .child(divider())
            .child(field("Connection", "How contacts far away reach this device. Nothing to set up."))
            .child(connection_status(&self.reach))
            .child(divider())
            .child(
                div()
                    .id("close-to-tray")
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.))
                    .cursor_pointer()
                    .child(field(
                        "Keep running in the tray",
                        "Closing the window keeps Neo receiving files in the background. Quit from the tray icon.",
                    ))
                    .child(switch(self.close_to_tray))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.close_to_tray = !this.close_to_tray;
                        this.save_settings(cx);
                    })),
            );

        let mut people = panel().gap(px(10.)).child(heading("Contacts"));
        if self.contacts.is_empty() {
            people = people.child(
                div().text_xs().text_color(rgb(MUTED)).child("No contacts yet. Use Add by code with their code, or let them scan your QR code above."),
            );
        }
        for c in &self.contacts {
            let account = c.account;
            let on = c.auto_accept;
            people = people.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(avatar(&c.name, c.account.0[0]))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().text_sm().truncate().child(SharedString::from(c.name.clone())))
                            .child(div().text_xs().font_family("Cascadia Mono").text_color(rgb(FAINT)).child(SharedString::from(c.account.code()))),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("auto-{}", account.to_hex())))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .cursor_pointer()
                            .child(div().text_xs().text_color(rgb(MUTED)).child("Accept files automatically"))
                            .child(switch(on))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.node.command(Command::SetAutoAccept { account, on: !on })
                            })),
                    )
                    .child(icon_button(SharedString::from(format!("remove-{}", account.to_hex())), "×").on_click(cx.listener(
                        move |this, _: &ClickEvent, _, _| this.node.command(Command::RemoveContact { account }),
                    ))),
            );
        }

        let link = panel()
            .gap(px(12.))
            .child(heading("Your devices"))
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                "Link your other computers so they all act as you: same code, same contacts, and files between them are accepted automatically.",
            ))
            .child(field(
                "Link this device to my identity",
                "On your main device, find its code in Settings. Enter it here, then accept on that device when both show the same number. This replaces this device's own identity.",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(self.link_input.clone()))
                    .child(pill_button("link-go", "Link", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.link(cx)))),
            );

        let backup = panel()
            .gap(px(12.))
            .child(heading("Backup"))
            .child(div().text_xs().text_color(rgb(MUTED)).child(
                "Save your identity and contacts to a file locked with a passphrase, so you can restore them if this computer is lost. Keep the file and passphrase safe; Neo can't recover a forgotten passphrase.",
            ))
            .child(self.pass_input.clone())
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(pill_button("backup-save", "Back up to file…", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.export(cx))))
                    .child(pill_button("backup-restore", "Restore from file…", false).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.import(cx)))),
            );

        div()
            .id("settings")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .pb(px(20.))
                    .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("Settings"))
                    .child(you)
                    .child(device)
                    .child(people)
                    .child(link)
                    .child(backup)
                    .child(div().text_xs().text_color(rgb(FAINT)).child(
                        "Neo never asks for personal information. Interrupted transfers pick up where they stopped, even after a restart.",
                    )),
            )
    }

    fn limits_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let up = parse_limit(self.up_input.read(cx).text());
        let down = parse_limit(self.down_input.read(cx).text());
        let changed = match (&up, &down) {
            (Ok(u), Ok(d)) => *u != self.up_limit || *d != self.down_limit,
            _ => true,
        };
        let (summary, color) = match (up, down) {
            (Ok(u), Ok(d)) => (format!("Upload {} · Download {}", describe(u), describe(d)), FAINT),
            _ => ("Type a number of MB/s, like 5 or 2.5".to_string(), WARN),
        };
        let unit = || div().text_xs().text_color(rgb(MUTED)).child("MB/s");
        let label = |t: &'static str| div().text_xs().text_color(rgb(MUTED)).child(t);
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(label("Upload"))
                    .child(div().w(px(96.)).child(self.up_input.clone()))
                    .child(unit())
                    .child(div().w(px(10.)))
                    .child(label("Download"))
                    .child(div().w(px(96.)).child(self.down_input.clone()))
                    .child(unit())
                    .child(div().flex_1())
                    .child(pill_button("save-limits", "Save", changed).when(changed, |d| {
                        d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_limits(cx)))
                    })),
            )
            .child(div().text_xs().text_color(rgb(color)).child(SharedString::from(summary)))
    }

    fn passphrase(&self, cx: &mut Context<Self>) -> Option<String> {
        let p = self.pass_input.read(cx).text().to_string();
        if p.chars().count() < 8 {
            None
        } else {
            Some(p)
        }
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(passphrase) = self.passphrase(cx) else {
            self.notice = Some("Type a backup passphrase of at least 8 characters first.".into());
            return cx.notify();
        };
        let dir = std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Documents")).unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some("Neo identity.neobackup"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.node.command(Command::ExportIdentity { path, passphrase });
                    this.pass_input.update(cx, |i, cx| i.clear(cx));
                });
            }
        })
        .detach();
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let Some(passphrase) = self.passphrase(cx) else {
            self.notice = Some("Type the backup's passphrase first.".into());
            return cx.notify();
        };
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Restore".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(path) = paths.pop() {
                    let _ = this.update(cx, |this, cx| {
                        this.node.command(Command::ImportIdentity { path, passphrase });
                        this.pass_input.update(cx, |i, cx| i.clear(cx));
                    });
                }
            }
        })
        .detach();
    }
}

/// A speed-limit field: empty (or "no limit") = None; MB/s otherwise.
pub(super) fn parse_limit(text: &str) -> Result<Option<f64>, ()> {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t == "no limit" || t == "unlimited" {
        return Ok(None);
    }
    let t = t.trim_end_matches("mb/s").trim().replace(',', ".");
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 && v <= 100_000.0 => Ok(Some((v * 100.0).round() / 100.0)),
        Ok(v) if v == 0.0 => Ok(None),
        _ => Err(()),
    }
}

/// How a saved limit appears in its field.
pub(super) fn limit_text(limit: Option<f64>) -> String {
    limit.map(|v| format!("{v}")).unwrap_or_default()
}

/// "10 MB/s (80 Mbit/s)" or "no limit".
fn describe(limit: Option<f64>) -> String {
    match limit {
        Some(v) => format!("{v} MB/s ({} Mbit/s)", (v * 8.0 * 10.0).round() / 10.0),
        None => "no limit".into(),
    }
}

fn changed(input: &Entity<TextInput>, current: &str, cx: &mut Context<NeoView>) -> bool {
    let typed = input.read(cx).text().trim().to_string();
    !typed.is_empty() && typed != current
}

/// One friendly line about internet reachability (with a colored dot).
fn connection_status(r: &Reachability) -> impl IntoElement {
    let (color, text) = if r.mapped.is_some() {
        (OK, "Reachable from anywhere. Your router opened a port for Neo automatically.")
    } else if !r.ipv6.is_empty() {
        (OK, "Reachable from anywhere over IPv6.")
    } else if r.public.is_some() && r.dht && r.nat.kind == neo_core::net::nat::NatKind::Hard {
        (
            WARN,
            if r.nat.step != 0 {
                "Your network uses a strict router, but a predictable one. Neo can usually still reach contacts directly."
            } else {
                "Your network uses a strict router. Contacts on easy networks can reach you; two strict networks may not connect."
            },
        )
    } else if r.public.is_some() && r.dht {
        (OK, "Reachable by your contacts. Neo opens a path through your router when needed.")
    } else if r.dht {
        (WARN, "Online, but your network hides this device; far-away contacts may not reach it.")
    } else {
        (MUTED, "Connecting to the internet directory… (contacts on this network work meanwhile)")
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().size(px(8.)).flex_none().rounded_full().bg(rgb(color)))
        .child(div().text_xs().text_color(rgb(TEXT)).child(text))
}

fn heading(text: &'static str) -> impl IntoElement {
    div().text_sm().font_weight(FontWeight::SEMIBOLD).child(text)
}

fn field(title: &'static str, hint: &'static str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
        .child(div().text_xs().text_color(rgb(MUTED)).child(hint))
}

fn divider() -> impl IntoElement {
    div().h(px(1.)).bg(rgb(BORDER))
}

fn input_row(input: Entity<TextInput>, id: &'static str, changed: bool, cx: &mut Context<NeoView>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(div().flex_1().child(input))
        .child(pill_button(id, "Save", changed).when(changed, |d| {
            d.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_settings(cx)))
        }))
}

/// Your code as a QR code (for scanning with a phone once there's a mobile app).
pub(super) fn qr(text: &str) -> impl IntoElement {
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return div();
    };
    let n = code.size();
    let code = std::rc::Rc::new(code);
    let module = px(3.);
    div()
        .flex_none()
        .p(px(8.))
        .rounded(px(8.))
        .bg(rgb(0xffffff))
        .flex()
        .flex_col()
        .children((0..n).map(|y| {
            let code = code.clone();
            div().flex().children((0..n).map(move |x| {
                div().size(module).when(code.get_module(x, y), |d| d.bg(rgb(0x0d0e11)))
            }))
        }))
}
