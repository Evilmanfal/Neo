//! Things that need your attention at the top of the main column: info and
//! error banners, contact requests, device-link requests, and the "compare
//! this number" card while this device is being linked.

use super::*;

impl NeoView {
    pub(super) fn banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some(n) = self.notice.clone() {
            out.push(
                banner(n, BAD, rgba(0xf2706d1f))
                    .child(icon_button("dismiss", "×").on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.notice = None;
                        cx.notify();
                    })))
                    .into_any_element(),
            );
        }
        if let Some(i) = self.info.clone() {
            out.push(banner(i, TEXT, rgba(0x7b6cff26)).into_any_element());
        }
        out
    }

    pub(super) fn request_cards(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if let Some((with, sas)) = &self.link_pending {
            out.push(
                request_card(
                    format!("Linking with {with}…"),
                    format!("Accept on {with} if it shows the same number. This device will then act as you."),
                )
                .child(big_number(sas))
                .into_any_element(),
            );
        }
        for (ix, l) in self.shared_links.iter().enumerate() {
            let (url, url2) = (l.url.clone(), l.url.clone());
            out.push(
                request_card(format!("{} sent you a Gofile link", l.from), format!("{} · {}", l.title, l.url))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(pill_button(("link-dismiss", ix), "Dismiss", false).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    if ix < this.shared_links.len() {
                                        this.shared_links.remove(ix);
                                    }
                                    cx.notify();
                                },
                            )))
                            .child(pill_button(("link-copy", ix), "Copy link", false).on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                    this.show_info("Link copied".into(), cx);
                                },
                            )))
                            .child(pill_button(("link-open", ix), "Open", true).on_click(move |_, _, _| {
                                let _ = std::process::Command::new("explorer").arg(&url2).spawn();
                            })),
                    )
                    .into_any_element(),
            );
        }
        for r in &self.requests {
            let card = match r {
                Request::Pair { id, name, account, device_name } => {
                    let id = *id;
                    request_card(
                        format!("{name} wants to add you as a contact"),
                        format!("From {device_name} · code {}. Contacts can see when you're nearby and send you files.", account.code()),
                    )
                    .child(answer_row(id, "Add contact", cx))
                }
                Request::Link { id, device_name, sas } => {
                    let id = *id;
                    request_card(
                        format!("Link “{device_name}” to you?"),
                        "It will get your identity and contacts and act as you. Only accept if it shows this number:".into(),
                    )
                    .child(big_number(sas))
                    .child(answer_row(id, "Link device", cx))
                }
            };
            out.push(card.into_any_element());
        }
        out
    }
}

fn banner(text: SharedString, color: u32, bg: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .px(px(12.))
        .py(px(8.))
        .rounded(px(8.))
        .bg(bg)
        .text_xs()
        .text_color(rgb(color))
        .child(div().flex_1().child(text))
}

fn request_card(title: String, body: String) -> gpui::Div {
    panel()
        .gap(px(10.))
        .border_color(rgb(ACCENT))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(SharedString::from(title)))
                .child(div().text_xs().text_color(rgb(MUTED)).child(SharedString::from(body))),
        )
}

fn big_number(sas: &str) -> impl IntoElement {
    div()
        .text_size(px(26.))
        .font_family("Cascadia Mono")
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT))
        .child(SharedString::from(sas.to_string()))
}

fn answer_row(id: u64, yes: &'static str, cx: &mut Context<NeoView>) -> impl IntoElement {
    div()
        .flex()
        .justify_end()
        .gap(px(8.))
        .child(pill_button(("req-no", id as usize), "Decline", false).on_click(cx.listener(
            move |this, _: &ClickEvent, _, _| this.node.command(Command::Respond { id, accept: false }),
        )))
        .child(pill_button(("req-yes", id as usize), yes, true).on_click(cx.listener(
            move |this, _: &ClickEvent, _, _| this.node.command(Command::Respond { id, accept: true }),
        )))
}
