//! The left panel: your other devices, your contacts (online or not) and
//! everyone else nearby, plus "Add by code".

use gpui::Focusable;

use super::*;

impl NeoView {
    pub(super) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mine: Vec<&Peer> = self.peers.iter().filter(|p| p.account == Some(self.account)).collect();
        let contact_of = |p: &Peer| p.account.and_then(|a| self.contacts.iter().find(|c| c.account == a));
        let strangers: Vec<&Peer> =
            self.peers.iter().filter(|p| p.account != Some(self.account) && contact_of(p).is_none()).collect();
        let all_selected = !self.peers.is_empty() && self.peers.iter().all(|p| self.selected.contains(&p.id));

        let mut list = div().flex().flex_col().gap(px(2.)).pb(px(12.));
        if !mine.is_empty() {
            list = list.child(group_label("Your devices"));
            for p in &mine {
                let sub = if p.remote { "Linked to you · over the internet" } else { "Linked to you" };
                list = list.child(self.device_row(p, p.name.clone(), sub.into(), false, cx));
            }
        }
        if !self.contacts.is_empty() {
            list = list.child(group_label("Contacts"));
            for c in &self.contacts {
                let online: Vec<&Peer> = self.peers.iter().filter(|p| p.account == Some(c.account)).collect();
                if online.is_empty() {
                    list = list.child(offline_row(c));
                }
                for p in online {
                    let sub = if p.remote { format!("{} · over the internet", p.name) } else { p.name.clone() };
                    list = list.child(self.device_row(p, c.name.clone(), sub, false, cx));
                }
            }
        }
        if !strangers.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .justify_between()
                    .pr(px(10.))
                    .child(group_label("Nearby"))
                    .child(div().pt(px(10.)).text_size(px(11.)).text_color(rgb(FAINT)).child("+ adds a contact")),
            );
            for p in &strangers {
                list = list.child(self.device_row(p, p.name.clone(), p.account.map(|a| a.code()).unwrap_or(p.id.code()), true, cx));
            }
        }
        let empty = self.peers.is_empty() && self.contacts.is_empty();

        div()
            .flex()
            .flex_col()
            .w(px(272.))
            .h_full()
            .border_r_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(18.))
                    .pt(px(16.))
                    .pb(px(6.))
                    .child(section_label("Send to"))
                    .child(
                        div()
                            .flex()
                            .gap(px(12.))
                            .when(!self.peers.is_empty(), |d| {
                                d.child(text_button("select-all", if all_selected { "Clear" } else { "Select all" }).on_click(
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        if all_selected {
                                            this.selected.clear();
                                        } else {
                                            this.selected = this.peers.iter().map(|p| p.id).collect();
                                        }
                                        cx.notify();
                                    }),
                                ))
                            })
                            .child(text_button("add-by-code", if self.adding { "Cancel" } else { "Add by code" }).on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.adding = !this.adding;
                                    if this.adding {
                                        window.focus(&this.code_input.focus_handle(cx));
                                    }
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .when(self.adding, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .px(px(14.))
                        .pb(px(10.))
                        .child(self.code_input.clone())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.))
                                .child(div().text_xs().text_color(rgb(FAINT)).child("They'll be asked to accept."))
                                .child(pill_button("add-go", "Add", true).on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_by_code(cx)))),
                        ),
                )
            })
            .child(
                div()
                    .id("peer-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .px(px(8.))
                    .when(empty, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(8.))
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .child(div().text_sm().text_color(rgb(MUTED)).child("Looking for devices…"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(FAINT))
                                        .child("Open Neo on another computer on this network and it will show up here."),
                                ),
                        )
                    })
                    .child(list),
            )
    }

    fn device_row(&self, peer: &Peer, title: String, subtitle: String, addable: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = peer.id;
        let on = self.selected.contains(&id);
        let seed = peer.account.map(|a| a.0[0]).unwrap_or(id.0[0]);
        div()
            .id(SharedString::from(format!("dev-{}", id.to_hex())))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .py(px(7.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(on, |d| d.bg(rgb(RAISED)))
            .hover(|s| s.bg(rgb(HOVER)))
            .child(avatar(&title, seed))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(div().text_sm().truncate().child(SharedString::from(title)))
                    .child(div().text_xs().text_color(rgb(FAINT)).truncate().child(SharedString::from(subtitle))),
            )
            .when(addable, |d| {
                d.child(icon_button(SharedString::from(format!("add-{}", id.to_hex())), "+").text_size(px(16.)).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.node.command(Command::AddContact { device: id });
                    },
                )))
            })
            .child(checkbox(on))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.selected.remove(&id) {
                    this.selected.insert(id);
                }
                cx.notify();
            }))
    }
}

fn group_label(text: &'static str) -> impl IntoElement {
    div().px(px(10.)).pt(px(10.)).pb(px(4.)).text_size(px(11.)).text_color(rgb(FAINT)).child(text)
}

fn offline_row(c: &Contact) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(10.))
        .py(px(7.))
        .opacity(0.5)
        .child(avatar(&c.name, c.account.0[0]))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .overflow_hidden()
                .child(div().text_sm().truncate().child(SharedString::from(c.name.clone())))
                .child(div().text_xs().text_color(rgb(FAINT)).child("Not nearby")),
        )
}
