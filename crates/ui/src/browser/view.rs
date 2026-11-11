use super::{BrowserEvent, BrowserSurface};
use crate::localization::message as tr;
use crate::{icons, surface_chrome, theme::Theme};
use gpui::{
    AnyElement, Context, Focusable, IntoElement, KeyDownEvent, MouseButton, Render, Window, div,
    prelude::*, px,
};
use zuse_i18n::messages as msg;

fn button(
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    enabled: bool,
    theme: &Theme,
    hovered_button: Option<&'static str>,
    cx: &mut Context<BrowserSurface>,
) -> gpui::Stateful<gpui::Div> {
    let show_tooltip = enabled && id != "browser-options" && hovered_button == Some(id);
    let card = div()
        .px(px(7.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border)
        .bg(crate::popover::surface_bg(theme))
        .font_family(theme.font_sans.clone())
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(11.0))
        .text_color(theme.text_strong)
        .child(label);
    div()
        .id(id)
        .debug_selector(move || id.into())
        .relative()
        .size(px(surface_chrome::CONTROL_SIZE))
        .flex_none()
        .rounded(px(surface_chrome::CONTROL_RADIUS))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(label)
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .hover(|style| style.bg(crate::theme::wash(0.14)))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            let next = if *hovered && enabled && id != "browser-options" {
                Some(id)
            } else if this.hovered_toolbar_button == Some(id) {
                None
            } else {
                this.hovered_toolbar_button
            };
            if next != this.hovered_toolbar_button {
                this.hovered_toolbar_button = next;
                cx.notify();
            }
        }))
        .when(!enabled, |el| el.cursor_default().opacity(0.35))
        .child(
            icons::icon(glyph)
                .size(px(surface_chrome::ICON_SIZE))
                .text_color(theme.text_muted),
        )
        .when(show_tooltip, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left(px(surface_chrome::CONTROL_SIZE / 2.0))
                    .size_0()
                    .child(
                        gpui::deferred(
                            gpui::anchored()
                                .anchor(gpui::Anchor::TopCenter)
                                .snap_to_window_with_margin(px(8.0))
                                .child(div().pt(px(5.0)).child(crate::frost::frosted(
                                    6.0,
                                    crate::frost::MENU_BLUR,
                                    card,
                                ))),
                        )
                        .priority(1),
                    ),
            )
        })
}

/// A text button: outlined when secondary, accent-filled when primary.
pub(super) fn action_button(
    theme: &Theme,
    id: &'static str,
    text: &'static str,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .rounded(px(8.0))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .whitespace_nowrap()
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .child(text);
    if primary {
        base.bg(theme.accent_strong)
            .text_color(theme.on_accent)
            .hover(|el| el.opacity(0.9))
    } else {
        base.border_1()
            .border_color(theme.border_strong)
            .bg(crate::theme::wash(0.04))
            .hover(|el| el.bg(crate::theme::wash(0.1)))
    }
}

/// The page's colour scheme: System follows Zuse's theme.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn page_dark(theme: &Theme, cx: &gpui::App) -> bool {
    match crate::settings::current(cx).browser_appearance {
        crate::appearance::AppearanceMode::System => theme.appearance.is_dark(),
        crate::appearance::AppearanceMode::Light => false,
        crate::appearance::AppearanceMode::Dark => true,
    }
}

const REVEAL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else {
    "Show in folder"
};

fn browser_menu_row(
    theme: &Theme,
    id: &'static str,
    icon: &'static str,
    label: &'static str,
) -> gpui::Stateful<gpui::Div> {
    crate::popover::menu_row(&theme.for_popup(), false, id)
        .id(id)
        .debug_selector(move || id.into())
        .font_family(theme.font_sans.clone())
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(12.0))
        .gap(px(8.0))
        .py(px(4.0))
        .child(
            icons::icon(icon)
                .size(px(14.0))
                .text_color(theme.text_muted),
        )
        .child(label)
}

impl BrowserSurface {
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        if event.keystroke.key == "escape" && self.selecting_element {
            self.cancel_selection();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        #[cfg(target_os = "linux")]
        if self.linux_menu_key(&event.keystroke.key, cx) {
            cx.stop_propagation();
            return;
        }
        if self.markup.is_some() && !self.address.focus_handle(cx).is_focused(window) {
            let keystroke = &event.keystroke;
            match keystroke.key.as_str() {
                "escape" => self.close_markup(window, cx),
                "enter" => self.save_markup(window, cx),
                "z" if keystroke.modifiers.secondary() => {
                    if let Some(markup) = &mut self.markup {
                        markup.undo();
                        cx.notify();
                    }
                }
                _ => {}
            }
            // The page is frozen: nothing else should reach it.
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.toolbar_menu.is_open() {
            self.close_toolbar_menu(cx);
            cx.stop_propagation();
            return;
        }
        let address_focused = self.address.focus_handle(cx).is_focused(window);
        #[cfg(target_os = "linux")]
        if !address_focused
            && self.focus.is_focused(window)
            && self.presentation == super::model::Presentation::Live
        {
            if event.prefer_character_input
                && let Some(text) = &event.keystroke.key_char
            {
                if let Some(native) = &self.native {
                    native.command(serde_json::json!({"cmd":"commit","text":text}));
                }
                cx.stop_propagation();
                return;
            }
            if event.keystroke.modifiers.control && !event.keystroke.modifiers.alt {
                if let Some(native) = &self.native {
                    match event.keystroke.key.as_str() {
                        "c" | "x" => {
                            native.command(serde_json::json!({"cmd":if event.keystroke.key=="c" {"copy"} else {"cut"}}));
                            cx.stop_propagation();
                            return;
                        }
                        "v" => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                native.command(serde_json::json!({"cmd":"text","text":text}));
                            }
                            cx.stop_propagation();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            self.linux_key(&event.keystroke, true);
            cx.stop_propagation();
            return;
        }
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        if address_focused && !primary && !mods.alt {
            match key {
                "enter" if !mods.shift => self.submit(window, cx),
                "escape" => {
                    let url = self.page.url.clone().unwrap_or_default();
                    self.address.update(cx, |input, cx| input.set_text(url, cx));
                    self.address_edited = false;
                    self.validation = None;
                    window.focus(&self.focus, cx);
                }
                _ => return,
            }
        } else {
            return;
        }
        cx.stop_propagation();
    }

    fn preview_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = &self.previews;
        let available = snapshot.error.is_none();
        let subtitle = if snapshot.remote {
            "Running on your device"
        } else {
            "Running locally"
        };
        let mut content = div()
            .w_full()
            .font_family(theme.font_sans.clone())
            .max_w(px(440.0))
            .flex_shrink_0()
            .pt(px(24.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_strong)
                    .child("Browser"),
            )
            .child(
                div()
                    .mb(px(16.0))
                    .text_size(crate::typography::text_sm())
                    .text_color(theme.text_muted)
                    .child("Search the web or open a local project."),
            )
            .child(
                div()
                    .mb(px(4.0))
                    .text_size(crate::typography::text_xs())
                    .text_color(theme.text_muted)
                    .child(subtitle),
            );
        for service in &snapshot.services {
            let url = service.url(snapshot.proxy_port);
            let row_url = url.clone();
            let border_strong = theme.border_strong;
            let label = if snapshot.remote {
                format!("{} · localhost:{}", service.device_name, service.port)
            } else {
                format!("localhost:{}", service.port)
            };
            content = content.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "preview-row-{}",
                        service.id
                    )))
                    .w_full()
                    .h(px(56.0))
                    .px(px(14.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.02))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .when(available, |el| {
                        el.cursor_pointer()
                            .hover(move |style| {
                                style
                                    .bg(crate::theme::ink(0.05))
                                    .border_color(border_strong)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(&row_url, window, cx)
                            }))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(crate::typography::text_sm())
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_strong)
                                    .truncate()
                                    .child(service.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::text_xs())
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .map(|el| {
                                #[cfg(feature = "browser-fixture")]
                                if snapshot
                                    .services
                                    .first()
                                    .is_some_and(|first| first.id == service.id)
                                {
                                    let position = self.fixture_preview_open.clone();
                                    return el.on_children_prepainted(move |bounds, _, _| {
                                        position.set(bounds.first().map(|bounds| bounds.center()));
                                    });
                                }
                                el
                            })
                            .id(gpui::SharedString::from(format!(
                                "open-preview-{}",
                                service.id
                            )))
                            .h(px(28.0))
                            .px(px(6.0))
                            .flex_shrink_0()
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_muted)
                            .role(gpui::Role::Button)
                            .aria_label(format!("Open {} preview", service.name))
                            .when(available, |el| {
                                el.cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::ink(0.05)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.navigate(&url, window, cx)
                                    }))
                            })
                            .when(!available, |el| el.opacity(0.4))
                            .child(tr(cx, msg::common::open)),
                    ),
            );
        }
        if snapshot.services.is_empty() {
            let message = if self.previews_loading {
                "Looking for dev servers…"
            } else if snapshot.remote {
                "Start a dev server in this project on your other device. Its preview will appear here when that device is online."
            } else {
                "Start a dev server in this project. It will appear here automatically, ready to open."
            };
            content = content.child(
                div()
                    .p(px(16.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::text_xs())
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(message),
            );
        }
        if let Some(error) = &snapshot.error {
            content = content.child(
                div()
                    .text_size(crate::typography::text_xs())
                    .line_height(px(17.0))
                    .text_color(theme.text_muted)
                    .child(error.clone()),
            );
        }
        let visits = self.context.recent.borrow().visits.clone();
        let mut heading = div()
            .mt(px(22.0))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(crate::typography::text_xs())
                    .text_color(theme.text_muted)
                    .child("Recently visited"),
            );
        if !visits.is_empty() {
            heading = heading.child(
                div()
                    .id("browser-clear-recent")
                    .cursor_pointer()
                    .text_size(crate::typography::text_xs())
                    .text_color(theme.text_muted)
                    .role(gpui::Role::Button)
                    .aria_label("Clear recently visited pages")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.context.recent.borrow_mut().clear();
                        cx.notify();
                    }))
                    .size(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .hover(|style| style.bg(crate::theme::wash(0.10)))
                    .child(
                        icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    ),
            );
        }
        content = content.child(heading);
        if visits.is_empty() {
            content = content.child(
                div()
                    .py(px(10.0))
                    .text_size(crate::typography::text_xs())
                    .text_color(theme.text_muted)
                    .child("Pages you visit will appear here."),
            );
        } else {
            for (index, visit) in visits.into_iter().enumerate() {
                let (title, host) = visit.display();
                let url = visit.url.clone();
                let favicon = self.context.recent.borrow().icon(&visit.url);
                let remove_url = visit.url;
                content = content.child(
                    div()
                        .id(("browser-recent", index))
                        .w_full()
                        .min_w_0()
                        .px(px(12.0))
                        .py(px(9.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .cursor_pointer()
                        .hover(|style| style.bg(crate::theme::wash(0.08)))
                        .role(gpui::Role::Button)
                        .aria_label(format!("Open {title}"))
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.navigate(&url, window, cx)),
                        )
                        .child(
                            div()
                                .size(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(match favicon {
                                    Some(image) => gpui::img(image)
                                        .size(px(20.0))
                                        .rounded(px(4.0))
                                        .into_any_element(),
                                    None => icons::icon(icons::GLOBE)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted)
                                        .into_any_element(),
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(crate::typography::text_sm())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text_strong)
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(crate::typography::text_xs())
                                        .text_color(theme.text_muted)
                                        .child(host),
                                ),
                        )
                        .child(
                            div()
                                .id(("browser-remove-recent", index))
                                .size(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .hover(|style| style.bg(crate::theme::wash(0.12)))
                                .role(gpui::Role::Button)
                                .aria_label("Remove from recently visited")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.context.recent.borrow_mut().remove(&remove_url);
                                    cx.notify();
                                }))
                                .child(
                                    icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                );
            }
        }
        div()
            .id("browser-previews")
            .size_full()
            .overflow_y_scroll()
            .p(px(16.0))
            .flex()
            .flex_col()
            .items_center()
            .child(content)
            .into_any_element()
    }

    fn empty_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.page.url.is_none() && self.previews_task.is_some() {
            return self.preview_body(theme, cx);
        }
        let external = !cfg!(any(target_os = "macos", target_os = "linux", windows));
        let has_error = self.page.error.is_some();
        #[cfg(target_os = "linux")]
        let starting = self.native_startup.is_some();
        #[cfg(not(target_os = "linux"))]
        let starting = false;
        let title = if has_error {
            "Couldn’t load this page"
        } else if starting {
            "Starting browser…"
        } else if external && self.page.url.is_some() {
            "Opened in your browser"
        } else {
            "Preview your work"
        };
        let description = if let Some(error) = &self.page.error {
            error.clone()
        } else if starting {
            "You can keep working while your page opens.".into()
        } else if external {
            "Open a website or local app in your default browser. Embedded browsing is available on macOS and Linux.".into()
        } else {
            "Preview your local app or keep a website beside your conversation.".into()
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(300.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .size(px(44.0))
                            .rounded(px(12.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised.opacity(0.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icons::icon(icons::GLOBE)
                                    .size(px(22.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::text_md())
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_strong)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_center()
                            .text_size(crate::typography::text_xs())
                            .line_height(px(19.0))
                            .text_color(theme.text_muted)
                            .child(description),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .id("browser-empty-action")
                            .h(px(28.0))
                            .px(px(10.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.surface_raised)
                            .cursor_pointer()
                            .role(gpui::Role::Button)
                            .aria_label(if has_error {
                                "Retry page"
                            } else {
                                "Enter an address"
                            })
                            .hover(|style| style.bg(crate::theme::wash(0.10)))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if has_error {
                                    this.reload(cx);
                                } else {
                                    this.focus_address(window, cx);
                                }
                            }))
                            .child(if has_error {
                                crate::localization::text("chat-review_guide_try_again", cx)
                            } else {
                                ("Enter an address").to_string()
                            })
                            .when(!has_error, |el| {
                                el.child(
                                    div()
                                        .text_size(crate::typography::text_xs())
                                        .text_color(theme.text_faint)
                                        .child(if cfg!(target_os = "macos") {
                                            "⌘L"
                                        } else {
                                            "Ctrl L"
                                        }),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(windows)]
impl BrowserSurface {
    /// The page composites natively beneath GPUI's overlay plane; this
    /// element only positions it and forwards pointer input that GPUI's hit
    /// testing delivers here, so popovers above the page occlude naturally.
    fn windows_page(
        &self,
        body: gpui::Stateful<gpui::Div>,
        host: std::rc::Rc<super::native::Host>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        use super::native::{PointerKind, wheel_units};
        let entity = cx.entity().downgrade();
        let forward = |kind: PointerKind| {
            move |this: &mut Self,
                  position: gpui::Point<gpui::Pixels>,
                  button: Option<MouseButton>,
                  modifiers: gpui::Modifiers,
                  clicks: usize| {
                if let Some(native) = &this.native {
                    native.pointer(kind, position, button, modifiers, clicks);
                }
            }
        };
        let right_occlusion = self.right_occlusion;
        let mut body = body.cursor(self.page_cursor).child(
            gpui::canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    // The Files panel slides over the page from the right.
                    let mut mask = window.content_mask().bounds;
                    let right = (window.viewport_size().width - right_occlusion).max(mask.left());
                    mask.size.width = mask.size.width.min(right - mask.left());
                    let scale = window.scale_factor();
                    let composition = window.native_composition();
                    let page = std::rc::Rc::downgrade(&host);
                    window.on_present(move || {
                        if let Some(page) = page.upgrade() {
                            page.sync(bounds, mask, scale, composition);
                        }
                    });
                    // A drag that began in the page keeps extending its
                    // selection outside the page, like a native child.
                    let capture = entity.clone();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Bubble
                            && !bounds.contains(&event.position)
                            && !cx.has_active_drag()
                        {
                            let _ = capture.update(cx, |this, _| {
                                if let Some(native) =
                                    this.native.as_ref().filter(|native| native.pressed())
                                {
                                    native.pointer(
                                        PointerKind::Move,
                                        event.position,
                                        None,
                                        event.modifiers,
                                        0,
                                    );
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        );
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let down = forward(PointerKind::Down);
            let up = forward(PointerKind::Up);
            let up_out = forward(PointerKind::Up);
            body = body
                .on_mouse_down(
                    button,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        if cx.has_active_drag() {
                            return;
                        }
                        window.focus(&this.focus, cx);
                        down(
                            this,
                            event.position,
                            Some(event.button),
                            event.modifiers,
                            event.click_count,
                        );
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                        up(this, event.position, Some(event.button), event.modifiers, 0);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(move |this, event: &gpui::MouseUpEvent, _, _| {
                        if this.native.as_ref().is_some_and(|native| native.pressed()) {
                            up_out(this, event.position, Some(event.button), event.modifiers, 0);
                        }
                    }),
                );
        }
        let moved = forward(PointerKind::Move);
        body.on_mouse_move(
            cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                if !cx.has_active_drag() {
                    moved(this, event.position, None, event.modifiers, 0);
                }
            }),
        )
        .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
            if let Some(native) = &this.native {
                native.wheel(event.position, wheel_units(event.delta), event.modifiers);
                cx.stop_propagation();
            }
        }))
        .on_hover(cx.listener(|this, hovered: &bool, _, _| {
            if !*hovered && let Some(native) = &this.native {
                native.leave();
            }
        }))
    }
}

impl Render for BrowserSurface {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        if self.selecting_element {
            let palette = super::annotation_palette(&theme).to_string();
            if self.annotation_theme.as_ref() != Some(&palette) {
                if let Some(native) = &self.native {
                    native.update_annotation_theme(&palette);
                }
                self.annotation_theme = Some(palette);
            }
        }
        #[cfg(target_os = "macos")]
        let persistence_notice = self.context.data.persistence_notice();
        #[cfg(not(target_os = "macos"))]
        let persistence_notice: Option<&str> = None;
        #[cfg(any(target_os = "linux", windows))]
        if let Some(native) = &self.native {
            let dark = match crate::settings::current(cx).browser_appearance {
                crate::appearance::AppearanceMode::System => theme.appearance.is_dark(),
                crate::appearance::AppearanceMode::Light => false,
                crate::appearance::AppearanceMode::Dark => true,
            };
            native.set_appearance(dark);
        }
        let focused = self.address.focus_handle(cx).is_focused(window);
        let external = !cfg!(any(target_os = "macos", target_os = "linux", windows));
        let has_page = self.page.url.is_some();
        let back = button(
            "browser-back",
            "Back",
            icons::ARROW_LEFT,
            self.page.can_back,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(self.page.can_back, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(false)))
        });
        let forward = button(
            "browser-forward",
            "Forward",
            icons::ARROW_RIGHT,
            self.page.can_forward,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(self.page.can_forward, |el| {
            el.on_click(cx.listener(|this, _, _, _| this.history(true)))
        });
        let reload = button(
            "browser-reload",
            "Reload page",
            icons::REFRESH,
            has_page,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
        });
        let address = surface_chrome::input()
            .id("browser-address")
            .when(self.validation.is_some(), |el| {
                el.border_1().border_color(theme.danger)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    #[cfg(any(target_os = "macos", windows))]
                    if let Some(native) = &this.native {
                        native.focus_chrome();
                    }
                    #[cfg(not(any(target_os = "macos", windows)))]
                    let _ = this;
                }),
            )
            .child(match self.favicon.clone() {
                Some(image) => gpui::img(image)
                    .size(px(12.0))
                    .flex_none()
                    .rounded(px(2.0))
                    .into_any_element(),
                None => icons::icon(icons::GLOBE)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_faint)
                    .into_any_element(),
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(16.0))
                    .overflow_hidden()
                    .child(self.address.clone()),
            )
            .when(focused || self.address_edited, |el| {
                el.child(
                    div()
                        .id("browser-go")
                        .size(px(18.0))
                        .flex_none()
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label(tr(cx, msg::native::go_to_address))
                        .hover(|s| s.bg(crate::theme::wash(0.10)))
                        .on_mouse_down(MouseButton::Left, |_, w, _| w.prevent_default())
                        .on_click(cx.listener(|this, _, w, cx| this.submit(w, cx)))
                        .tooltip(crate::settings::widgets::text_tooltip("Go to address"))
                        .child(
                            icons::icon(icons::RETURN)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        ),
                )
            });
        let open = button(
            "browser-external",
            "Open in default browser",
            icons::ARROW_UP_RIGHT,
            has_page,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.open_external(cx)))
        });
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        let select = button(
            "browser-select-element",
            "Annotate page",
            icons::BROWSER_SELECT,
            has_page,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(self.selecting_element, |el| el.bg(crate::theme::wash(0.10)))
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, window, cx| {
                this.close_markup(window, cx);
                window.focus(&this.focus, cx);
                this.toggle_select_element(cx);
            }))
        });
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        let devtools = button(
            "browser-developer-tools",
            "Developer tools",
            icons::BROWSER_CODE,
            has_page,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .when(has_page, |el| {
            el.on_click(cx.listener(|this, _, _, cx| this.toggle_devtools(cx)))
        });
        let menu_trigger = button(
            "browser-options",
            "Browser options",
            icons::MORE_HORIZONTAL,
            true,
            &theme,
            self.hovered_toolbar_button,
            cx,
        )
        .aria_expanded(self.toolbar_menu.is_open())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, _| {
                this.toolbar_menu.note_trigger_press();
            }),
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if this.toolbar_menu.take_press_was_open() {
                this.close_toolbar_menu(cx);
            } else {
                this.open_toolbar_menu(cx);
            }
        }));
        let menu_trigger = if self.toolbar_menu.get().is_some() {
            let copy =
                browser_menu_row(&theme, "browser-copy-url", icons::BROWSER_LINK, "Copy URL")
                    .when(!has_page, |el| el.opacity(0.4).cursor_default())
                    .when(has_page, |el| {
                        el.on_click(cx.listener(|this, _, _, cx| {
                            if let Some(url) = this.page.url.clone() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url));
                                crate::toast::success(cx, "URL copied");
                            }
                            this.close_toolbar_menu(cx);
                        }))
                    });
            let menu = crate::popover::popover_card(&theme.for_popup())
                .w(px(184.0))
                .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
                    if this
                        .appearance_menu_bounds
                        .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
                    let _ = event;
                    this.close_toolbar_menu(cx);
                }));
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let menu = menu.child(
                browser_menu_row(
                    &theme,
                    "browser-screenshot",
                    icons::BROWSER_CAMERA,
                    "Take screenshot",
                )
                .when(has_page, |el| {
                    el.on_click(cx.listener(|this, _, window, cx| this.take_screenshot(window, cx)))
                })
                .when(!has_page, |el| el.opacity(0.4).cursor_default()),
            );
            let menu = menu.child(copy);
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let menu = {
                use crate::appearance::AppearanceMode;
                let current = crate::settings::current(cx).browser_appearance;
                let mut appearance =
                    browser_menu_row(&theme, "browser-appearance", icons::MONITOR, "Appearance")
                        .relative()
                        .w_full()
                        .role(gpui::Role::MenuItem)
                        .child(div().flex_1())
                        .child(
                            icons::icon(icons::ALT_ARROW_LEFT)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        );
                if self.appearance_menu_open {
                    let mut submenu = crate::popover::popover_card(&theme.for_popup()).w(px(144.0));
                    for (mode, label, icon) in [
                        (AppearanceMode::System, "System", icons::MONITOR),
                        (AppearanceMode::Light, "Light", icons::SUN),
                        (AppearanceMode::Dark, "Dark", icons::MOON),
                    ] {
                        let id = match mode {
                            AppearanceMode::System => "browser-appearance-system",
                            AppearanceMode::Light => "browser-appearance-light",
                            AppearanceMode::Dark => "browser-appearance-dark",
                        };
                        let row = browser_menu_row(&theme, id, icon, label)
                            .w_full()
                            .role(gpui::Role::MenuItem)
                            .child(div().flex_1())
                            .when(mode == current, |row| {
                                row.child(
                                    icons::icon(icons::CHECK)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                crate::settings::update(
                                    crate::settings::SavePolicy::Immediate,
                                    cx,
                                    |settings| settings.browser_appearance = mode,
                                );
                                this.close_toolbar_menu(cx);
                                cx.refresh_windows();
                            }));
                        submenu = submenu.child(row);
                    }
                    let entity = cx.entity().downgrade();
                    submenu = submenu.child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let _ = entity.update(cx, |this, _| {
                                    this.appearance_menu_bounds = Some(bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    );
                    appearance = appearance.child(crate::popover::nested_menu(
                        "browser-appearance-menu",
                        submenu.into_any_element(),
                        true,
                    ));
                }
                let entity = cx.entity().downgrade();
                let appearance_open = self.appearance_menu_open;
                menu.child(
                    div()
                        .id("browser-appearance-hover")
                        .debug_selector(|| "browser-appearance-hover".into())
                        .w_full()
                        .relative()
                        .on_hover(cx.listener(|this, hovered: &bool, window, cx| {
                            if *hovered {
                                this.appearance_hover.record_origin(window.mouse_position());
                                if !this.appearance_menu_open {
                                    this.appearance_menu_open = true;
                                    cx.notify();
                                }
                            }
                        }))
                        .child(appearance)
                        .child(
                            gpui::canvas(
                                |_, _, _| (),
                                move |trigger, _, window, _| {
                                    if !appearance_open {
                                        return;
                                    }
                                    let entity = entity.clone();
                                    window.on_mouse_event(
                                        move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                            if phase != gpui::DispatchPhase::Bubble {
                                                return;
                                            }
                                            let _ = entity.update(cx, |this, cx| {
                                                if !this.appearance_hover.contains_pointer(
                                                    trigger,
                                                    this.appearance_menu_bounds,
                                                    event.position,
                                                    true,
                                                ) {
                                                    this.appearance_menu_open = false;
                                                    this.appearance_menu_bounds = None;
                                                    this.appearance_hover.reset();
                                                    cx.notify();
                                                }
                                            });
                                        },
                                    );
                                },
                            )
                            .absolute()
                            .inset_0(),
                        ),
                )
            };
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let menu = menu.child(div().h(px(1.0)).my(px(4.0)).bg(theme.border));
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            let menu = menu
                .child(
                    browser_menu_row(
                        &theme,
                        "browser-clear-cookies",
                        icons::BROWSER_COOKIE,
                        "Clear cookies",
                    )
                    .when(self.native.is_some(), |el| {
                        el.on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_clear_data(true, window, cx)
                        }))
                    })
                    .when(self.native.is_none(), |el| el.opacity(0.4).cursor_default()),
                )
                .child(
                    browser_menu_row(
                        &theme,
                        "browser-clear-cache",
                        icons::BROWSER_BRUSH,
                        "Clear cache",
                    )
                    .when(self.native.is_some(), |el| {
                        el.on_click(cx.listener(|this, _, window, cx| {
                            this.confirm_clear_data(false, window, cx)
                        }))
                    })
                    .when(self.native.is_none(), |el| el.opacity(0.4).cursor_default()),
                );
            menu_trigger
                .relative()
                .child(crate::popover::anchored_menu_below_end(
                    "browser-options-menu",
                    menu.into_any_element(),
                    self.toolbar_menu.closing_since(),
                ))
        } else {
            menu_trigger
        };
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        let menu_trigger = if let Some((path, bytes)) = &self.screenshot {
            let path_copy = path.to_string_lossy().into_owned();
            let path_reveal = path.clone();
            let bytes = bytes.clone();
            let action = |id, text, primary| action_button(&theme, id, text, primary);
            // Sized by its actions: the row of buttons sets the card's width, the
            // header spans it, and padding is even on every side.
            let notice = crate::popover::popover_card(&theme.for_popup())
                .min_w(px(300.0))
                .p(px(14.0))
                .gap(px(14.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            crate::icons::icon(crate::icons::CHECK_CIRCLE)
                                .size(px(18.0))
                                .text_color(theme.success),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Screenshot saved"),
                        )
                        .child(
                            div()
                                .id("browser-screenshot-dismiss")
                                .size(px(22.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .cursor_pointer()
                                .hover(|el| el.bg(crate::theme::wash(0.1)))
                                .child(
                                    crate::icons::icon(crate::icons::CLOSE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.screenshot = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            action("browser-screenshot-copy-path", "Copy path", false).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        path_copy.clone(),
                                    ));
                                    crate::toast::success(cx, "Path copied");
                                },
                            ),
                        )
                        .child(
                            action(
                                "browser-screenshot-reveal",
                                if cfg!(target_os = "macos") {
                                    "Reveal in Finder"
                                } else {
                                    "Show in folder"
                                },
                                false,
                            )
                            .on_click(move |_, _, cx| cx.reveal_path(&path_reveal)),
                        )
                        .child(
                            action("browser-screenshot-copy-image", "Copy image", true).on_click(
                                move |_, _, cx| {
                                    let image = gpui::Image::from_bytes(
                                        gpui::ImageFormat::Png,
                                        bytes.clone(),
                                    );
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
                                    crate::toast::success(cx, "Image copied");
                                },
                            ),
                        ),
                );
            menu_trigger
                .relative()
                .child(crate::popover::anchored_menu_below_end(
                    "browser-screenshot-notice",
                    notice.into_any_element(),
                    None,
                ))
        } else {
            menu_trigger
        };
        let toolbar = surface_chrome::toolbar(&theme)
            .when(!external, |el| el.child(back).child(forward).child(reload))
            .child(address);
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        let toolbar = toolbar.child(select).child(devtools);
        let toolbar = toolbar.child(open).child(menu_trigger);

        let body = div()
            .id("browser-page")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            // Empty tabs share the shell's glass, like the sidebar tab picker.
            // Keep an opaque backing while native web content is loading.
            .when(has_page, |body| body.bg(theme.bg));
        // The markup editor stands in for the page while it is open.
        let body = if self.markup.is_some() {
            body.child(self.markup_editor(&theme, cx))
        } else {
            self.page_body(body, &theme, cx)
        };

        let remote_loopback = self.remote
            && self
                .page
                .url
                .as_deref()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| {
                    super::model::loopback(&u)
                        && !(u.port() == Some(zuse_protocol::PREVIEW_PROXY_PORT)
                            && u.host_str()
                                .is_some_and(|host| host.ends_with(".localhost")))
                });
        div().id("browser-surface").size_full().flex().flex_col().track_focus(&self.focus)
            .key_context("Browser").on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(|this,event: &gpui::KeyUpEvent,w,cx| {
                #[cfg(target_os = "linux")]
                if this.focus.is_focused(w) {this.linux_key(&event.keystroke,false);cx.stop_propagation();}
                #[cfg(not(target_os = "linux"))]
                let _=(this,event,w,cx);
            }))
            .on_action(cx.listener(|this, _: &super::Reload, _, cx| this.reload(cx)))
            .on_action(cx.listener(|this, _: &super::FocusAddress, w, cx| this.focus_address(w, cx)))
            .on_action(cx.listener(|_, _: &super::NewTab, _, cx| cx.emit(BrowserEvent::NewTab(None))))
            .on_action(cx.listener(|_, _: &super::CloseTab, _, cx| cx.emit(BrowserEvent::Close)))
            .on_action(cx.listener(|this, _: &super::Back, _, _| this.history(false)))
            .on_action(cx.listener(|this, _: &super::Forward, _, _| this.history(true)))
            // Mouse side buttons drive the shell's route history everywhere
            // else; over the browser they are inert.
            .on_mouse_down(MouseButton::Navigate(gpui::NavigationDirection::Back), |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Navigate(gpui::NavigationDirection::Forward), |_, _, cx| cx.stop_propagation())
            .child(toolbar)
            .when_some(persistence_notice, |el, message| el.child(div().px(px(12.0)).py(px(8.0))
                .text_size(crate::typography::text_xs()).text_color(theme.text_muted).child(message.to_owned())))
            .when_some(self.validation.clone(), |el, message| el.child(div().px(px(12.0)).py(px(8.0)).text_size(crate::typography::text_xs()).text_color(theme.danger).child(message)))
            .when(remote_loopback, |el| el.child(div().px(px(12.0)).py(px(8.0)).border_b_1().border_color(theme.border)
                .text_size(crate::typography::text_xs()).text_color(theme.text_muted)
                .child(tr(cx, msg::native::localhost_opens_on_this_device_open_a_detected_preview_from_a_new_tab_to_reach_y_6c40a9dc))))
            .child(body)
            .when(external, |el| el.child(div().h(px(26.0)).px(px(10.0)).flex().items_center().gap(px(5.0)).border_t_1().border_color(theme.border)
                .text_size(crate::typography::text_xs()).text_color(theme.text_faint)
                .child(icons::icon(icons::ARROW_UP_RIGHT).size(px(11.0))).child(tr(cx, msg::native::opens_in_your_default_browser))))
    }
}

impl BrowserSurface {
    /// The live page for this platform, or the empty state.
    fn page_body(
        &self,
        body: gpui::Stateful<gpui::Div>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        #[cfg(target_os = "macos")]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(theme, cx))
            } else {
                let native = native.handle();
                let resize_inset = self.resize_inset;
                let dark = page_dark(theme, cx);
                let right_occlusion = self.right_occlusion;
                body.child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let native = std::rc::Rc::downgrade(&native);
                            let mut mask = window.content_mask().bounds;
                            let right =
                                (window.viewport_size().width - right_occlusion).max(mask.left());
                            mask.size.width = mask.size.width.min(right - mask.left());
                            let dragging = cx.has_active_drag();
                            window.on_present(move || {
                                if let Some(native) = native.upgrade() {
                                    native.borrow_mut().sync(
                                        bounds,
                                        mask,
                                        dragging,
                                        resize_inset,
                                        dark,
                                    );
                                }
                            });
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
            }
        } else {
            body.child(self.empty_body(theme, cx))
        };
        #[cfg(target_os = "linux")]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(theme, cx))
            } else {
                let image = native.image.clone();
                let image_scale = native.image_scale;
                let dark = page_dark(theme, cx);
                let entity = cx.entity().downgrade();
                body.child(gpui::canvas(|_,_,_| (), move |bounds,_,window,cx| {
                    let scale = window.scale_factor();
                    if let Some(view)=entity.upgrade() {
                        let focus=view.read(cx).focus.clone();
                        window.handle_input(&focus,gpui::ElementInputHandler::new(bounds,view),cx);
                    }
                    let _ = entity.update(cx, |this,_| {
                        if let Some(native)=&mut this.native { native.sync(bounds,scale); native.set_appearance(dark); }
                    });
                    let capture=entity.clone();
                    window.on_mouse_event(move |event: &gpui::MouseMoveEvent,phase,_,cx| {
                        if phase==gpui::DispatchPhase::Bubble && !bounds.contains(&event.position) && !cx.has_active_drag() {
                            let _=capture.update(cx,|this,_| {
                                if this.native.as_ref().is_some_and(|n|n.pressed.get().is_some()) {
                                    this.linux_pointer("move",event.position,event.pressed_button,event.modifiers);
                                }
                            });
                        }
                    });
                    if let Some(image)=&image {
                        if let Some(bytes)=image.as_bytes(0).filter(|b| b.len() >= 4) {
                            let color=gpui::rgb(((bytes[2] as u32)<<16)|((bytes[1] as u32)<<8)|bytes[0] as u32);
                            window.paint_quad(gpui::fill(bounds,color));
                        }
                        let dimensions=image.size(0);
                        // Keep the previous frame at its original scale while
                        // WebKit reflows; clipping never stretches the page.
                        let viewport=gpui::Bounds::new(bounds.origin,gpui::size(px(dimensions.width.0 as f32/image_scale),px(dimensions.height.0 as f32/image_scale)));
                        let _=window.paint_image(viewport,gpui::Corners::default(),image.clone(),0,false);
                    }
                }).absolute().inset_0())
                .on_mouse_down(MouseButton::Left,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {
                    if !cx.has_active_drag() { w.focus(&this.focus,cx); this.linux_pointer("down",event.position,Some(event.button),event.modifiers); cx.stop_propagation(); }
                }))
                .on_mouse_down(MouseButton::Right,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {
                    w.focus(&this.focus,cx);this.linux_pointer("down",event.position,Some(event.button),event.modifiers);cx.stop_propagation();
                }))
                .on_mouse_up(MouseButton::Left,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_up_out(MouseButton::Left,cx.listener(|this,event: &gpui::MouseUpEvent,_,_| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);}))
                .on_mouse_up(MouseButton::Right,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_down(MouseButton::Middle,cx.listener(|this,event: &gpui::MouseDownEvent,w,cx| {w.focus(&this.focus,cx);this.linux_pointer("down",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_up(MouseButton::Middle,cx.listener(|this,event: &gpui::MouseUpEvent,_,cx| {this.linux_pointer("up",event.position,Some(event.button),event.modifiers);cx.stop_propagation();}))
                .on_mouse_move(cx.listener(|this,event: &gpui::MouseMoveEvent,_,cx| {if !cx.has_active_drag(){this.linux_pointer("move",event.position,event.pressed_button,event.modifiers);}}))
                .on_scroll_wheel(cx.listener(|this,event: &gpui::ScrollWheelEvent,_,cx| {
                    if let Some(native)=&this.native {
                        let delta=event.delta.pixel_delta(px(16.));let p=event.position-native.bounds.origin;
                        native.command(serde_json::json!({"cmd":"scroll","x":f32::from(p.x),"y":f32::from(p.y),"dx":-f32::from(delta.x)/40.,"dy":-f32::from(delta.y)/40.,"mods":super::linux::modifiers_mask(event.modifiers)}));
                        cx.stop_propagation();
                    }
                }))
            }
        } else {
            body.child(self.empty_body(theme, cx))
        };
        #[cfg(windows)]
        let body = if let Some(native) = &self.native {
            if self.page.error.is_some() {
                body.child(self.empty_body(theme, cx))
            } else {
                self.windows_page(body, native.handle(), cx)
            }
        } else {
            body.child(self.empty_body(theme, cx))
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        let body = body.child(self.empty_body(theme, cx));

        #[cfg(target_os = "linux")]
        let body = body.when_some(self.linux_menu(theme, cx), |el, menu| el.child(menu));
        body
    }
}
