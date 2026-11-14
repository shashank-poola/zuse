//! The composer's `+` menu (attach images, plan mode) and the plan chip.
//!
//! Plan mode is a per-chat flag beside the access level: the agent inspects
//! and proposes without changing files, whatever access the chat has.
use super::*;
use crate::localization::message as tr;
use crate::{icons, popover};
use zuse_i18n::messages as msg;

const PLAN_ICON: &str = icons::LIST;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AddMenuItem {
    AttachImages,
    PlanMode,
}
pub(super) const ADD_MENU_ITEMS: [AddMenuItem; 2] =
    [AddMenuItem::AttachImages, AddMenuItem::PlanMode];

impl Composer {
    /// Flip plan mode for the draft or the selected chat. A chat whose saved
    /// choices cannot be staged explains why, like the access picker.
    pub fn toggle_plan_mode(&mut self, cx: &mut Context<Self>) {
        let accepted = self
            .pickers
            .update(cx, |pickers, cx| pickers.toggle_plan_mode(cx));
        if !accepted && let Some(chat_id) = self.state.read(cx).selected_chat.clone() {
            self.failure = Some(
                self.state
                    .read(cx)
                    .configuration_notice()
                    .map(|(_, notice)| notice)
                    .unwrap_or_else(|| {
                        "Refresh this chat's saved choices before changing plan mode.".into()
                    })
                    .into(),
            );
            self.failure_key = Some(chat_id);
        }
        cx.notify();
    }

    pub(super) fn plan_mode(&self, cx: &App) -> bool {
        self.pickers.read(cx).resolved(cx).plan
    }

    pub(super) fn toggle_add_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu_open = !self.add_menu_open;
        if self.add_menu_open {
            self.permission_open = false;
            self.add_menu_active = 0;
            window.focus(&self.add_menu_focus, cx);
        }
        cx.notify();
    }

    fn close_add_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu_open = false;
        window.focus(&self.input.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn choose_add_item(
        &mut self,
        item: AddMenuItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if item == AddMenuItem::PlanMode && !self.plan_item_enabled(cx) {
            return;
        }
        self.close_add_menu(window, cx);
        match item {
            AddMenuItem::AttachImages => self.open_file_picker(cx),
            AddMenuItem::PlanMode => self.toggle_plan_mode(cx),
        }
    }

    /// Plan mode can always be turned off; turning it on needs an agent
    /// that offers it.
    fn plan_item_enabled(&self, cx: &App) -> bool {
        self.plan_mode(cx) || self.pickers.read(cx).plan_mode_available(cx)
    }

    /// Arrow/Enter/Escape while the `+` menu is open.
    pub(super) fn on_add_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.add_menu_open {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => self.close_add_menu(window, cx),
            "enter" | "space" if !event.is_held => {
                self.choose_add_item(ADD_MENU_ITEMS[self.add_menu_active], window, cx)
            }
            "up" | "down" => {
                self.add_menu_active = popover::menu_step(
                    Some(self.add_menu_active),
                    ADD_MENU_ITEMS.len(),
                    if event.keystroke.key == "up" { -1 } else { 1 },
                )
                .unwrap_or(0);
            }
            "home" => self.add_menu_active = 0,
            "end" => self.add_menu_active = ADD_MENU_ITEMS.len() - 1,
            _ => return false,
        }
        cx.notify();
        true
    }

    pub(super) fn render_add_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let popup_theme = theme.for_popup();
        let plan = self.plan_mode(cx);
        let plan_enabled = self.plan_item_enabled(cx);
        let rows = ADD_MENU_ITEMS.into_iter().enumerate().map(|(index, item)| {
            let (icon, label, checked, enabled) = match item {
                AddMenuItem::AttachImages => (
                    icons::PLUS,
                    tr(cx, msg::native::composer_attach_images),
                    false,
                    true,
                ),
                AddMenuItem::PlanMode => (
                    PLAN_ICON,
                    tr(cx, msg::chat::chat_composer_plan_mode),
                    plan,
                    plan_enabled,
                ),
            };
            popover::menu_row_nav(
                &popup_theme,
                false,
                index == self.add_menu_active && enabled,
                format!("composer-add-row-{index}"),
            )
            .id(("composer-add-item", index))
            .role(if item == AddMenuItem::PlanMode {
                Role::MenuItemCheckBox
            } else {
                Role::MenuItem
            })
            .aria_label(label.clone())
            .min_h(px(32.))
            .when(!enabled, |row| {
                row.tip(tr(cx, msg::native::composer_plan_mode_unavailable))
            })
            .when(item == AddMenuItem::PlanMode, |row| {
                row.aria_toggled(if checked {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
            })
            .when(!enabled, |row| row.opacity(0.45).cursor_default())
            .child(
                icons::icon(icon)
                    .flex_none()
                    .size(px(15.))
                    .text_color(popup_theme.text_muted),
            )
            .child(
                div().flex_1().min_w_0().child(
                    div()
                        .truncate()
                        .line_height(px(17.))
                        .text_size(crate::typography::text_sm())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(popup_theme.text)
                        .child(label),
                ),
            )
            .child(div().flex_none().size(px(14.)).when(checked, |slot| {
                slot.child(
                    icons::icon(icons::CHECK)
                        .size(px(14.))
                        .text_color(popup_theme.accent),
                )
            }))
            .when(enabled, |row| {
                row.on_click(
                    cx.listener(move |this, _, window, cx| this.choose_add_item(item, window, cx)),
                )
            })
        });
        let menu = popover::popover_card(&popup_theme)
            .id("composer-add-menu")
            .debug_selector(|| "composer-add-menu".into())
            .role(Role::Menu)
            .w(px(168.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.add_menu_open = false;
                cx.notify();
            }))
            .children(rows);
        popover::anchored_menu_above("composer-add-popup", menu.into_any_element(), None)
    }

    /// The visible plan-mode state: a quiet accent chip beside the access
    /// control while plan mode is on. Clicking it turns plan mode off.
    pub(super) fn render_plan_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if !self.plan_mode(cx) {
            return None;
        }
        let exit = tr(cx, msg::chat::chat_composer_exit_plan_mode);
        let hover_key: SharedString = format!("composer-plan-{}", cx.entity_id()).into();
        Some(
            div()
                .id("composer-plan-chip")
                .debug_selector(|| "composer-plan-chip".into())
                .role(Role::Button)
                .aria_label(exit.clone())
                .tab_index(0)
                .h(px(28.))
                .pl(px(9.))
                .pr(px(7.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .rounded(px(7.))
                .text_size(crate::typography::text_xs())
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.accent)
                .cursor_pointer()
                .bg(motion::hover_blend(
                    &hover_key,
                    theme.accent.opacity(0.10),
                    theme.accent.opacity(0.18),
                ))
                .on_hover(motion::hover_listener(hover_key))
                .tip(exit)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_plan_mode(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.toggle_plan_mode(cx);
                    }
                }))
                .child(tr(cx, msg::native::composer_plan_chip))
                .child(
                    icons::icon(icons::CLOSE)
                        .size(px(11.))
                        .text_color(theme.accent.opacity(0.7)),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::tests::composer_focus_window;

    #[gpui::test]
    fn plan_mode_is_a_draft_flag_beside_the_access_level(cx: &mut gpui::TestAppContext) {
        let (_directory, handle) = composer_focus_window(cx);
        handle
            .update(cx, |composer, window, cx| {
                composer.pickers.update(cx, |pickers, cx| {
                    pickers.set_draft_sandbox(SandboxLevel::DangerFullAccess, cx)
                });
                composer.toggle_add_menu(window, cx);
                assert!(composer.add_menu_open);
                composer.choose_add_item(AddMenuItem::PlanMode, window, cx);
                assert!(!composer.add_menu_open);
                let resolved = composer.pickers.read(cx).resolved(cx);
                assert!(resolved.plan);
                assert_eq!(resolved.sandbox, SandboxLevel::DangerFullAccess);
                assert!(resolved.chat_config().is_none_or(|config| config.plan));
                composer.toggle_plan_mode(cx);
                let resolved = composer.pickers.read(cx).resolved(cx);
                assert!(!resolved.plan);
                assert_eq!(resolved.sandbox, SandboxLevel::DangerFullAccess);
            })
            .unwrap();
    }

    #[gpui::test]
    fn existing_chats_read_plan_mode_from_their_saved_config(cx: &mut gpui::TestAppContext) {
        let (_directory, handle) = composer_focus_window(cx);
        handle
            .update(cx, |composer, _, cx| {
                composer.state.update(cx, |state, cx| {
                    state.chats = vec![
                        serde_json::from_value(serde_json::json!({
                            "id":"planned", "deviceId":"local", "archived":false,
                            "createdAt":"2026-01-01T00:00:00Z",
                            "config":{"harness":"codex","sandbox":"auto","plan":true}
                        }))
                        .unwrap(),
                        serde_json::from_value(serde_json::json!({
                            "id":"legacy", "deviceId":"local", "archived":false,
                            "createdAt":"2026-01-01T00:00:00Z",
                            "config":{"harness":"codex","sandbox":"read-only"}
                        }))
                        .unwrap(),
                    ];
                    state.selected_chat = Some("planned".into());
                    cx.notify();
                });
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .update(cx, |composer, _, cx| {
                let resolved = composer.pickers.read(cx).resolved(cx);
                assert!(resolved.plan);
                assert_eq!(resolved.sandbox, SandboxLevel::Auto);
                composer.state.update(cx, |state, cx| {
                    state.selected_chat = Some("legacy".into());
                    cx.notify();
                });
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, cx| {
                // The retired read-only level loads as ask-first access.
                let resolved = composer.pickers.read(cx).resolved(cx);
                assert!(!resolved.plan);
                assert_eq!(resolved.sandbox, SandboxLevel::WorkspaceWrite);
            })
            .unwrap();
    }
}
