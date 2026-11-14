//! Source picker shared by the native and browser composers.
use crate::localization::message as tr;
use crate::{
    assets::icon,
    input::TextInput,
    palette, settings_style as s,
    shell::{BORDER, MUTED, TEXT},
};
use gpui::{prelude::*, *};
use serde_json::{Value, json};
use std::collections::HashMap;
use zuse_i18n::messages as msg;
use zuse_protocol::creation_source::{self as wire, Resolution, Row, Source, Tab};

gpui::actions!(creation_source_picker, [DismissSourceMenu]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub repo_path: String,
    pub space_id: Option<String>,
    pub device_id: Option<String>,
    pub cloud: bool,
}
#[derive(Clone)]
pub struct Request {
    pub id: u64,
    pub method: &'static str,
    pub params: Value,
}
#[derive(Clone)]
pub enum Event {
    Changed(Option<Resolution>),
    ReturnFocus(Option<Point<Pixels>>),
}
enum Pending {
    List { generation: u64, tab: Tab },
    Resolve { generation: u64 },
}

/// In-memory canvas selection only; no pending request or credential is copied.
#[derive(Clone)]
pub struct SelectionSnapshot {
    target: Option<Target>,
    selection: Option<Resolution>,
}
impl SelectionSnapshot {
    pub fn new(target: Option<Target>, selection: Option<Resolution>) -> Self {
        Self { target, selection }
    }
}
pub struct SourcePicker {
    target: Option<Target>,
    generation: u64,
    next: u64,
    pending: HashMap<u64, Pending>,
    open: bool,
    tab: Tab,
    search: Option<Entity<TextInput>>,
    focus_search: bool,
    trigger: FocusHandle,
    active: usize,
    rows: Vec<Row>,
    loading: bool,
    error: Option<String>,
    preparing: bool,
    attempt: Option<(Source, String)>,
    selection: Option<Resolution>,
    scroll: ScrollHandle,
    bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
}
impl EventEmitter<Request> for SourcePicker {}
impl EventEmitter<Event> for SourcePicker {}
impl SourcePicker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            DismissSourceMenu,
            Some("CreationSourcePicker"),
        )]);
        Self {
            target: None,
            generation: 0,
            next: 0,
            pending: HashMap::new(),
            open: false,
            tab: Tab::default(),
            search: None,
            focus_search: false,
            trigger: cx.focus_handle().tab_stop(true),
            active: 0,
            rows: vec![],
            loading: false,
            error: None,
            preparing: false,
            attempt: None,
            selection: None,
            scroll: ScrollHandle::new(),
            bounds: Default::default(),
        }
    }
    pub fn set_target(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.generation += 1;
        self.pending.clear();
        self.open = false;
        self.rows.clear();
        self.loading = false;
        self.error = None;
        self.preparing = false;
        self.attempt = None;
        self.selection = None;
        self.clear_search(cx);
        cx.emit(Event::Changed(None));
        cx.notify();
    }
    pub fn selection_snapshot(&self) -> SelectionSnapshot {
        SelectionSnapshot::new(self.target.clone(), self.selection.clone())
    }
    pub fn restore_selection(&mut self, snapshot: SelectionSnapshot, cx: &mut Context<Self>) {
        self.generation += 1;
        self.pending.clear();
        self.target = snapshot.target;
        self.selection = snapshot.selection;
        self.open = false;
        self.rows.clear();
        self.loading = false;
        self.error = None;
        self.preparing = false;
        self.attempt = None;
        self.clear_search(cx);
        // Rehydration is not a new user issue selection or RPC request.
        cx.notify();
    }
    pub fn selection(&self) -> Option<&Resolution> {
        self.selection.as_ref()
    }
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.attempt = None;
        self.error = None;
        self.dismiss(cx);
        cx.emit(Event::Changed(None));
        cx.notify();
    }
    pub fn is_preparing(&self) -> bool {
        self.preparing
    }
    pub fn cloud_ref(&self, default_branch: &str) -> (String, Option<String>) {
        match self.selection.as_ref() {
            Some(Resolution::CloudRef {
                base_ref, branch, ..
            }) => (base_ref.clone(), branch.clone()),
            _ => (format!("refs/heads/{default_branch}"), None),
        }
    }
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.clear_search(cx);
            cx.notify();
        }
    }
    fn clear_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            search.update(cx, |s, cx| s.set_value(String::new(), cx));
        }
        self.active = 0;
        self.scroll.set_offset(Point::default());
    }
    fn request(
        &mut self,
        method: &'static str,
        mut params: Value,
        pending: Pending,
        cx: &mut Context<Self>,
    ) {
        if let Some(device) = self.target.as_ref().and_then(|t| t.device_id.as_ref()) {
            params["targetDeviceId"] = json!(device);
        }
        self.next += 1;
        self.pending.insert(self.next, pending);
        cx.emit(Request {
            id: self.next,
            method,
            params,
        });
    }
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if let Some(search) = &self.search {
            let placeholder = if self.tab == Tab::Branches {
                "Search by name"
            } else {
                "Search by title, number, or author"
            };
            search.update(cx, |input, cx| {
                input.set_placeholder(placeholder.into(), cx)
            });
        }
        self.rows.clear();
        self.loading = true;
        self.error = None;
        self.active = 0;
        self.scroll.set_offset(Point::default());
        self.request(
            wire::LIST,
            json!({"repoPath":target.repo_path,"tab":self.tab}),
            Pending::List {
                generation: self.generation,
                tab: self.tab,
            },
            cx,
        );
        cx.notify();
    }
    fn filtered(&self, cx: &App) -> Vec<Row> {
        let query = self
            .search
            .as_ref()
            .map(|s| s.read(cx).value(cx))
            .unwrap_or_default();
        self.rows
            .iter()
            .filter(|r| r.matches(&query))
            .cloned()
            .collect()
    }
    fn choose(&mut self, row: Row, cx: &mut Context<Self>) {
        if self.preparing {
            return;
        }
        self.attempt = Some((row.source, uuid::Uuid::new_v4().to_string()));
        self.open = false;
        self.clear_search(cx);
        self.resolve(cx);
    }
    fn resolve(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some((source, id))) = (self.target.clone(), self.attempt.clone()) else {
            return;
        };
        self.preparing = true;
        self.error = None;
        let mut params = json!({"repoPath":target.repo_path,"spaceId":target.space_id,"source":source,"requestId":id});
        if target.cloud {
            params["cloud"] = json!(true);
        }
        self.request(
            wire::RESOLVE,
            params,
            Pending::Resolve {
                generation: self.generation,
            },
            cx,
        );
        cx.notify();
    }
    pub fn receive(&mut self, id: u64, result: Result<Value, String>, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        match pending {
            Pending::List { generation, tab }
                if generation == self.generation && tab == self.tab =>
            {
                self.loading = false;
                match result.and_then(|v| {
                    serde_json::from_value(v)
                        .map_err(|_| "The source list could not be read.".into())
                }) {
                    Ok(rows) => {
                        self.rows = rows;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Pending::Resolve { generation } if generation == self.generation => {
                self.preparing = false;
                match result.and_then(|v| {
                    serde_json::from_value::<Resolution>(v).map_err(|_| {
                        "The source response could not be read. Retry to recover the checkout."
                            .into()
                    })
                }) {
                    Ok(resolution) => {
                        self.selection = Some(resolution.clone());
                        self.attempt = None;
                        self.error = None;
                        cx.emit(Event::Changed(Some(resolution)));
                        cx.emit(Event::ReturnFocus(None));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            _ => {}
        }
        cx.notify();
    }
    fn close(&mut self, pointer: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        self.open = false;
        self.clear_search(cx);
        cx.emit(Event::ReturnFocus(pointer));
        cx.notify();
    }
    fn key(&mut self, e: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        match e.keystroke.key.as_str() {
            "escape" => self.close(None, cx),
            "up" | "down" => {
                let count = self.filtered(cx).len();
                if count > 0 {
                    self.active = if e.keystroke.key == "up" {
                        (self.active + count - 1) % count
                    } else {
                        (self.active + 1) % count
                    };
                    self.scroll.scroll_to_item(self.active);
                    cx.notify();
                }
            }
            "enter" => {
                if let Some(row) = self.filtered(cx).get(self.active).cloned() {
                    self.choose(row, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}
impl Render for SourcePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.search.is_none() {
            let search = cx.new(|cx| {
                TextInput::new("Search by title, number, or author", window, cx)
                    .with_single_line_metrics(14., 20.)
            });
            cx.observe(&search, |this, _, cx| {
                this.active = 0;
                this.scroll.set_offset(Point::default());
                cx.notify();
            })
            .detach();
            self.search = Some(search);
        }
        if self.focus_search {
            self.focus_search = false;
            window.focus(&self.search.as_ref().unwrap().read(cx).focus_handle(cx), cx);
        }
        let label = self.selection.as_ref().map(|r| match r {
            Resolution::Checkout { label, .. } | Resolution::CloudRef { label, .. } => {
                label.clone()
            }
            Resolution::Issue { number, .. } => format!("#{number}"),
        });
        let mut view = div()
            .relative()
            .min_w_0()
            .flex_shrink(1.)
            .flex()
            .items_center()
            .gap(px(4.))
            .text_color(palette::rgb(MUTED));
        if self.preparing {
            view = view.child(icon("loading").size(px(14.))).child(
                div()
                    .text_size(px(12.))
                    .child(tr(cx, msg::native::preparing)),
            );
        } else if self.attempt.is_some() && self.error.is_some() && !self.open {
            view = view
                .child(
                    div()
                        .text_size(px(11.))
                        .max_w(px(180.))
                        .truncate()
                        .child(self.error.clone().unwrap()),
                )
                .child(
                    div()
                        .id("source-resolve-retry")
                        .role(Role::Button)
                        .aria_label(tr(cx, msg::native::retry_source_selection))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .child(tr(cx, msg::chat::attempts_retry))
                        .on_click(cx.listener(|this, _, _, cx| this.resolve(cx))),
                );
        } else if let Some(label) = label {
            view = view
                .child(
                    div()
                        .text_size(px(12.))
                        .min_w_0()
                        .max_w(px(200.))
                        .truncate()
                        .child(label),
                )
                .child(
                    div()
                        .id("source-clear")
                        .role(Role::Button)
                        .aria_label(tr(cx, msg::native::clear_selected_source))
                        .cursor_pointer()
                        .size(px(20.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon("close").size(px(12.)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.selection = None;
                            this.attempt = None;
                            this.error = None;
                            cx.emit(Event::Changed(None));
                            cx.emit(Event::ReturnFocus(None));
                            cx.notify();
                        })),
                );
        }
        let bounds = self.bounds.clone();
        view = view.child(
            div()
                .id("create-from")
                .debug_selector(|| "creation-source-trigger".into())
                .relative()
                .flex_none()
                .role(Role::Button)
                .aria_label(tr(cx, msg::chat::create_from_menu_create_from))
                .aria_expanded(self.open)
                .track_focus(&self.trigger)
                .h(px(28.))
                .border_1()
                .border_color(transparent_black())
                .px(px(6.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .hover(|v| v.bg(s::frame_bg()))
                .focus_visible(|v| v.bg(s::frame_bg()))
                .child(
                    canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .child(icon("site-github").size(px(16.)))
                .child(icon("alt-arrow-down").size(px(12.)).opacity(0.6))
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.preparing {
                        return;
                    }
                    if this.open {
                        this.close(None, cx)
                    } else {
                        this.open = true;
                        this.focus_search = true;
                        this.clear_search(cx);
                        this.load(cx);
                    }
                }))
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if !this.open && matches!(e.keystroke.key.as_str(), "enter" | "space") {
                        this.open = true;
                        this.focus_search = true;
                        this.clear_search(cx);
                        this.load(cx);
                        cx.stop_propagation();
                    }
                })),
        );
        if self.open {
            let bounds = self.bounds.clone();
            let mut card = div()
                .id("creation-source-menu")
                .debug_selector(|| "creation-source-menu".into())
                .relative()
                .role(Role::Dialog)
                .aria_label(tr(cx, msg::chat::create_from_menu_create_from))
                .occlude()
                .key_context("CreationSourcePicker")
                .on_action(cx.listener(|this, _: &DismissSourceMenu, _, cx| {
                    this.close(None, cx);
                    cx.stop_propagation();
                }))
                .w(px(340.).min(window.viewport_size().width - px(32.)))
                .h(px(256.).min(window.viewport_size().height - px(16.)))
                .rounded(px(14.))
                .bg(palette::tone(hsla(132. / 360., 0.05, 0.105, 0.72), white()))
                .shadow_lg()
                .overflow_hidden()
                .flex()
                .flex_col()
                .text_color(palette::rgb(TEXT))
                .text_size(px(14.))
                .line_height(px(18.9))
                .when(!palette::is_light(), |view| {
                    view.child(
                        div()
                            .absolute()
                            .inset_0()
                            .rounded(px(14.))
                            .bg(linear_gradient(
                                180.,
                                linear_color_stop(white().opacity(0.043), 0.),
                                linear_color_stop(white().opacity(0.008), 1.),
                            ))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(14.))
                                    .border_1()
                                    .border_color(white().opacity(0.065)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left(px(8.))
                                    .right(px(8.))
                                    .h(px(1.))
                                    .bg(white().opacity(0.09)),
                            ),
                    )
                })
                .capture_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| this.key(e, cx)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    if !bounds.get().is_some_and(|b| b.contains(&e.position)) {
                        this.close(Some(e.position), cx)
                    }
                }));
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .py(px(8.))
                    .border_b_1()
                    .border_color(palette::rgb(BORDER).opacity(0.5))
                    .child(icon("palette-search").size(px(16.)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(14.))
                            .child(self.search.clone().unwrap()),
                    ),
            );
            card = card.child(
                div()
                    .flex()
                    .gap(px(4.))
                    .px(px(8.))
                    .py(px(6.))
                    .border_b_1()
                    .border_color(palette::rgb(BORDER).opacity(0.5))
                    .children(Tab::ALL.into_iter().enumerate().map(|(i, tab)| {
                        div()
                            .id(("source-tab", i))
                            .role(Role::Tab)
                            .aria_label(tab.label())
                            .aria_selected(tab == self.tab)
                            .cursor_pointer()
                            .px(px(10.))
                            .py(px(4.))
                            .rounded(px(7.))
                            .text_size(px(13.))
                            .line_height(px(17.55))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(palette::rgb(if tab == self.tab { TEXT } else { MUTED }))
                            .when(tab == self.tab, |v| v.bg(source_accent()))
                            .child(tab.label())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tab = tab;
                                this.focus_search = true;
                                this.load(cx)
                            }))
                    })),
            );
            let rows = self.filtered(cx);
            let mut list = div()
                .id("creation-source-list")
                .flex_1()
                .min_h_0()
                .py(px(4.))
                .overflow_y_scroll()
                .track_scroll(&self.scroll);
            if self.loading {
                list = list.child(
                    div()
                        .px(px(12.))
                        .py(px(24.))
                        .text_center()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .child(tr(cx, msg::common::loading)),
                );
            } else if let Some(error) = self.error.clone() {
                list = list
                    .child(div().p(px(12.)).text_size(px(13.)).child(error))
                    .child(
                        div()
                            .id("source-list-retry")
                            .role(Role::Button)
                            .aria_label(tr(cx, msg::native::retry_source_list))
                            .cursor_pointer()
                            .px(px(12.))
                            .py(px(8.))
                            .text_size(px(13.))
                            .child(tr(cx, msg::chat::attempts_retry))
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    );
            } else if rows.is_empty() {
                list = list.child(
                    div()
                        .px(px(12.))
                        .py(px(24.))
                        .text_center()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(palette::rgb(MUTED))
                        .child(match self.tab {
                            Tab::Branches => "No other branches",
                            Tab::PullRequests => "No open PRs found",
                            Tab::Issues => "No open issues found",
                        }),
                );
            } else {
                list = list.children(rows.into_iter().enumerate().map(|(i, row)| {
                    let glyph = match row.source {
                        Source::Branch { .. } => "git-branch",
                        Source::Pr { .. } => "pull-request",
                        Source::Issue { .. } => "info-circle",
                    };
                    let mut item = div()
                        .id(("creation-source", i))
                        .role(Role::Button)
                        .aria_label(format!("{} {}", row.lead, row.title).trim().to_owned())
                        .cursor_pointer()
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .when(i == self.active, |v| v.bg(source_accent()))
                        .on_hover(cx.listener(move |this, hover, _, cx| {
                            if *hover {
                                this.active = i;
                                cx.notify()
                            }
                        }))
                        .child(if matches!(row.source, Source::Issue { .. }) {
                            div()
                                .size(px(16.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(11.66667))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(palette::rgb(MUTED)),
                                )
                                .into_any_element()
                        } else {
                            icon(glyph).size(px(16.)).flex_none().into_any_element()
                        });
                    if !row.lead.is_empty() {
                        item = item.child(
                            div()
                                .text_size(px(12.))
                                .line_height(px(16.))
                                .text_color(palette::rgb(MUTED))
                                .font_family("Geist Mono")
                                .child(row.lead.clone()),
                        );
                    }
                    item.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(16.))
                            .text_color(palette::rgb(MUTED))
                            .child(if row.in_use {
                                crate::localization::text("chat-create_from_menu_in_use", cx)
                            } else if i == self.active {
                                crate::localization::text("chat-create_from_menu_select", cx)
                            } else {
                                ("").to_string()
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(row.clone(), cx)))
                }));
            }
            card = card.child(list);
            let height = px(256.).min(window.viewport_size().height - px(16.));
            let below = self
                .bounds
                .get()
                .is_some_and(|bounds| bounds.top() < height + px(14.));
            view = view.child(
                div()
                    .absolute()
                    .when(below, |v| v.top_full())
                    .when(!below, |v| v.bottom_full())
                    .right_0()
                    .child(deferred(
                        anchored()
                            .anchor(if below {
                                Anchor::TopRight
                            } else {
                                Anchor::BottomRight
                            })
                            .offset(point(px(0.), px(if below { 6. } else { -6. })))
                            .snap_to_window_with_margin(px(8.))
                            .child(crate::glass::menu_with_radius(card, 14.)),
                    )),
            );
        }
        view
    }
}

fn source_accent() -> Hsla {
    palette::tone(
        hsla(130. / 360., 0.06, 0.165, 1.),
        hsla(106. / 360., 0.16, 0.89, 1.),
    )
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::path::Path;
    use std::prelude::v1::test;

    struct PickerFixture(Entity<SourcePicker>);

    impl Render for PickerFixture {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .items_end()
                .pb(px(40.))
                .child(self.0.clone())
        }
    }

    #[gpui::test]
    fn source_menu_opens_above_and_keeps_its_size_as_results_change(cx: &mut TestAppContext) {
        let (fixture, cx) = cx.add_window_view(|_, cx| {
            PickerFixture(cx.new(|cx| {
                let mut picker = SourcePicker::new(cx);
                picker.open = true;
                picker
            }))
        });
        cx.update(|window, cx| window.draw(cx).clear());
        cx.update(|window, cx| window.draw(cx).clear());
        let trigger = cx.debug_bounds("creation-source-trigger").unwrap();
        let empty = cx.debug_bounds("creation-source-menu").unwrap();
        assert!(
            empty.bottom() <= trigger.top(),
            "menu {empty:?}, trigger {trigger:?}"
        );
        assert_eq!(empty.size, size(px(340.), px(256.)));

        cx.update(|_, cx| {
            fixture.update(cx, |fixture, cx| {
                fixture.0.update(cx, |picker, cx| {
                    picker.rows.push(Row {
                        source: Source::Pr { number: 2 },
                        title: "Example pull request".into(),
                        lead: "#2".into(),
                        branch: None,
                        author: None,
                        in_use: false,
                    });
                    cx.notify();
                });
            });
        });
        cx.update(|window, cx| window.draw(cx).clear());
        let populated = cx.debug_bounds("creation-source-menu").unwrap();
        assert_eq!(populated.size, empty.size);
        assert!(populated.bottom() <= trigger.top());
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Source picker test",
                "-c",
                "user.email=source-picker@example.invalid",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    #[gpui::test]
    fn default_cloud_ref_fetches_remote_default_branch_without_rewriting_explicit_refs(
        cx: &mut TestAppContext,
    ) {
        let root =
            std::env::temp_dir().join(format!("zuse-source-picker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let source = root.join("source");
        let consumer = root.join("consumer");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&consumer).unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        std::fs::write(source.join("marker"), "main").unwrap();
        git(&source, &["add", "marker"]);
        git(&source, &["commit", "-m", "main"]);
        let main = git(&source, &["rev-parse", "HEAD"]);
        git(&source, &["checkout", "-b", "origin/main"]);
        std::fs::write(source.join("marker"), "literal origin/main branch").unwrap();
        git(&source, &["commit", "-am", "literal branch"]);
        let literal = git(&source, &["rev-parse", "HEAD"]);
        assert_ne!(main, literal);
        git(&consumer, &["init"]);
        git(
            &consumer,
            &["remote", "add", "origin", source.to_str().unwrap()],
        );
        let picker = cx.new(SourcePicker::new);
        let (default_ref, branch) = picker.update(cx, |picker, _| picker.cloud_ref("main"));
        assert!(branch.is_none());
        git(&consumer, &["fetch", "origin", &default_ref]);
        assert_eq!(
            git(&consumer, &["rev-parse", "FETCH_HEAD"]),
            main,
            "Default must fetch remote refs/heads/main, never a local tracking name"
        );
        picker.update(cx, |picker, _| {
            picker.selection = Some(Resolution::CloudRef {
                label: "Literal branch".into(),
                base_ref: "origin/main".into(),
                branch: Some("task-branch".into()),
            })
        });
        let (explicit_ref, branch) = picker.update(cx, |picker, _| picker.cloud_ref("main"));
        assert_eq!(explicit_ref, "origin/main");
        assert_eq!(branch.as_deref(), Some("task-branch"));
        git(&consumer, &["fetch", "origin", &explicit_ref]);
        assert_eq!(
            git(&consumer, &["rev-parse", "FETCH_HEAD"]),
            literal,
            "Explicit branch named origin/main retains exact remote meaning"
        );
    }
}
