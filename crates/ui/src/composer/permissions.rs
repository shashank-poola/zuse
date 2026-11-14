//! Access picks configure the current chat and become the global new-chat default.
use super::*;
use crate::localization::message as tr;
use crate::{icons, popover};
use zuse_i18n::messages as msg;

use zuse_protocol::presentation::{ACCESS_LEVELS as LEVELS, access_details};
fn label(level: SandboxLevel) -> &'static str {
    access_details(level).label
}
fn description(level: SandboxLevel) -> &'static str {
    access_details(level).description
}
/// Why `level` cannot be picked for `harness`, if it cannot. Only "Approve
/// for me" depends on the agent: it needs a native automatic approver.
fn unavailable(level: SandboxLevel, harness: Option<&HarnessId>) -> Option<&'static str> {
    match harness {
        Some(harness) if level.automatic_review() => {
            SandboxLevel::automatic_review_support(harness).err()
        }
        _ => None,
    }
}
/// The level the agent actually runs with: "Approve for me" on an agent
/// without an automatic approver falls back to asking (the provider adapters
/// map it the same way), so the chip and menu show that instead.
fn effective(level: SandboxLevel, harness: Option<&HarnessId>) -> SandboxLevel {
    if unavailable(level, harness).is_some() {
        SandboxLevel::WorkspaceWrite
    } else {
        level
    }
}
impl Composer {
    /// Apply an access choice to the draft or selected chat and remember it globally.
    fn choose_permission(
        &mut self,
        level: SandboxLevel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if unavailable(level, self.pickers.read(cx).resolved(cx).harness.as_ref()).is_some() {
            return;
        }
        self.permission_open = false;
        window.focus(&self.input.focus_handle(cx), cx);
        if self.set_cloud_task_permission(level, cx) {
            self.remember_permission(level, cx);
            cx.notify();
            return;
        }
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            self.pickers
                .update(cx, |pickers, cx| pickers.set_draft_sandbox(level, cx));
            if self.pickers.read(cx).resolved(cx).sandbox == level {
                self.remember_permission(level, cx);
            }
            cx.notify();
            return;
        };
        let Some(config) = self.pickers.read(cx).resolved(cx).chat_config() else {
            self.failure = Some("Choose an agent before changing this chat's permissions.".into());
            self.failure_key = Some(chat_id);
            cx.notify();
            return;
        };
        let accepted = self.state.update(cx, |state, cx| {
            state.stage_chat_configuration(
                zuse_protocol::ChatConfigPatch::sandbox(level),
                config,
                cx,
            )
        });
        if accepted {
            self.remember_permission(level, cx);
        } else {
            self.failure = Some(
                self.state
                    .read(cx)
                    .configuration_notice()
                    .map(|(_, notice)| notice)
                    .unwrap_or_else(|| {
                        "Refresh this chat's saved choices before changing access.".into()
                    })
                    .into(),
            );
            self.failure_key = Some(chat_id);
        }
        cx.notify();
    }
    /// A cloud task keeps its access mode with its own run settings, which the
    /// next message sends; the computer's chat configuration (unreachable
    /// while it sleeps) is not involved. Returns whether it applied.
    pub(super) fn set_cloud_task_permission(
        &mut self,
        level: SandboxLevel,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.native_cloud_task() {
            return false;
        }
        // An unconfirmed message keeps its exact request (and access mode) for
        // a safe retry; changing access now would not apply to it.
        if self.cloud_send_unconfirmed() {
            self.failure = Some(
                "Your last message is still being confirmed with its original access mode. Change access after it's sent."
                    .into(),
            );
            cx.notify();
            return true;
        }
        self.pickers
            .update(cx, |pickers, cx| pickers.set_cloud_task_sandbox(level, cx))
    }
    /// Save the new-chat default immediately and refresh permission controls.
    fn remember_permission(&self, level: SandboxLevel, cx: &mut Context<Self>) {
        let mode = zuse_protocol::presentation::PermissionMode::from_sandbox(level);
        crate::settings::update(crate::settings::SavePolicy::Immediate, cx, |settings| {
            settings.default_permission_mode = mode;
        });
        cx.refresh_windows();
    }
    pub(super) fn permission_blocked(&self, cx: &App) -> bool {
        self.state.read(cx).configuration_blocked()
    }
    fn refresh_permission(&mut self, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.refresh_selected_configuration(cx));
        self.permission_open = false;
        cx.notify();
    }
    fn on_permission_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).configuration_saving() && event.keystroke.key != "escape" {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.permission_open = false;
                self.permission_inspector = None;
                self.permission_rules_task = None;
                window.focus(&self.input.focus_handle(cx), cx);
            }
            "enter" | "space" if !event.is_held => {
                if self.permission_open
                    && self.state.read(cx).configuration_blocked()
                    && !self.native_cloud_task()
                {
                    self.refresh_permission(cx);
                } else if self.permission_open {
                    self.choose_permission(LEVELS[self.permission_active], window, cx);
                } else {
                    self.permission_open = true;
                    let resolved = self.pickers.read(cx).resolved(cx);
                    let current = effective(resolved.sandbox, resolved.harness.as_ref());
                    self.permission_active = LEVELS
                        .iter()
                        .position(|level| *level == current)
                        .unwrap_or(0);
                }
            }
            "up" | "down" if self.permission_open => {
                self.permission_active = popover::menu_step(
                    Some(self.permission_active),
                    LEVELS.len(),
                    if event.keystroke.key == "up" { -1 } else { 1 },
                )
                .unwrap_or(0);
            }
            "home" if self.permission_open => self.permission_active = 0,
            "end" if self.permission_open => self.permission_active = LEVELS.len() - 1,
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
    pub(super) fn render_permission_control(
        &self,
        theme: &Theme,
        icon_only: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let resolved = self.pickers.read(cx).resolved(cx);
        let harness = resolved.harness;
        let selected = effective(resolved.sandbox, harness.as_ref());
        let pending = self.state.read(cx).configuration_saving();
        let unconfirmed = !self.cached_cloud()
            && !self.native_cloud_task()
            && !pending
            && self.state.read(cx).configuration_blocked();
        // Safe modes rest muted and brighten on hover; full access stays in
        // the warning tone so the unlocked state is never mistaken for them.
        let full_access = selected == SandboxLevel::DangerFullAccess;
        let hover_key: SharedString = format!("composer-permission-{}", cx.entity_id()).into();
        let tint = if full_access {
            theme.warning
        } else {
            motion::hover_blend(&hover_key, theme.text_muted, theme.text)
        };
        div()
            .id("composer-permissions")
            .relative()
            .flex_none()
            .when(self.permission_inspector.is_none(), |view| {
                view.track_focus(&self.permission_focus)
            })
            .on_key_down(cx.listener(Self::on_permission_key))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("composer-permission-trigger")
                    .role(Role::ComboBox)
                    .aria_label(format!("Permission mode: {}", label(selected)))
                    .aria_expanded(self.permission_open)
                    .tab_index(0)
                    .h(px(28.))
                    .px(px(if icon_only { 7. } else { 10. }))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(px(7.))
                    .text_size(crate::typography::text_xs())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(tint)
                    .cursor_pointer()
                    // An unconfirmed mode keeps its name, dimmed, so the
                    // control doesn't change width while the chat connects.
                    .when(pending || unconfirmed, |view| view.opacity(0.5))
                    .bg(if self.permission_open {
                        theme.muted_fill().opacity(0.7)
                    } else {
                        motion::hover_blend(
                            &hover_key,
                            theme.muted_fill().opacity(0.0),
                            theme.muted_fill().opacity(0.6),
                        )
                    })
                    .on_hover(motion::hover_listener(hover_key.clone()))
                    .child(
                        icons::icon(access_details(selected).icon)
                            .size(px(14.))
                            .text_color(tint),
                    )
                    .when(!icon_only, |el| {
                        el.child(if pending {
                            crate::localization::text("settings-wallpaper_saving", cx)
                        } else {
                            (label(selected)).to_string()
                        })
                    })
                    .when(icon_only, |el| el.tip(label(selected)))
                    .child(
                        icons::icon(icons::ALT_ARROW_DOWN)
                            .size(px(12.))
                            .text_color(tint.opacity(tint.a * 0.6)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !pending {
                            this.permission_inspector = None;
                            this.permission_rules_task = None;
                            this.permission_active = LEVELS
                                .iter()
                                .position(|level| *level == selected)
                                .unwrap_or(0);
                            this.permission_open = !this.permission_open;
                            window.focus(&this.permission_focus, cx);
                            cx.notify();
                        }
                    })),
            )
            .when(self.permission_open, |view| {
                let popup_theme = theme.for_popup();
                let row = |index: usize, level: SandboxLevel| {
                    let blocked = unavailable(level, harness.as_ref());
                    let checked = level == selected;
                    popover::menu_row_nav(
                        &popup_theme,
                        checked,
                        index == self.permission_active && blocked.is_none(),
                        format!("permission-row-{index}"),
                    )
                    .id(("permission-level", index))
                    .role(Role::ListBoxOption)
                    .aria_label(label(level))
                    .aria_selected(checked)
                    .aria_description(blocked.unwrap_or(description(level)))
                    .h(px(44.))
                    .py(px(0.))
                    .gap(px(10.))
                    .when(blocked.is_some(), |row| row.opacity(0.45).cursor_default())
                    .child(
                        icons::icon(access_details(level).icon)
                            .flex_none()
                            .size(px(16.))
                            .text_color(if checked {
                                popup_theme.text
                            } else {
                                popup_theme.text_muted
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .truncate()
                                    .line_height(px(17.))
                                    .text_size(crate::typography::text_sm())
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(popup_theme.text)
                                    .child(label(level)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .line_height(px(15.))
                                    .text_size(crate::typography::text_xs())
                                    .text_color(popup_theme.text_muted)
                                    .child(blocked.unwrap_or(description(level))),
                            ),
                    )
                    .child(div().flex_none().size(px(14.)).when(checked, |slot| {
                        slot.child(
                            icons::icon(icons::CHECK)
                                .size(px(14.))
                                .text_color(popup_theme.accent),
                        )
                    }))
                    .when(blocked.is_none(), |row| {
                        row.on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_permission(level, window, cx)
                        }))
                    })
                };
                // Plan mode is not an access level; it lives in the `+` menu.
                let rows = LEVELS
                    .into_iter()
                    .enumerate()
                    .map(|(index, level)| row(index, level).into_any_element());
                let menu = popover::popover_card(&popup_theme)
                    .id("composer-permission-menu")
                    .role(Role::ListBox)
                    .w(px(260.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(popover::menu_heading(&popup_theme, "Agent access"))
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.permission_open = false;
                        cx.notify();
                    }))
                    .children(rows)
                    .when(self.state.read(cx).selected_chat.is_some(), |menu| {
                        menu.child(popover::menu_separator()).child(
                            div()
                                .id("open-permission-rules")
                                .role(Role::Button)
                                .aria_label(tr(cx, msg::native::saved_permission_rules_c6bd67ad))
                                .px(px(10.))
                                .py(px(8.))
                                .text_size(crate::typography::text_xs())
                                .text_color(popup_theme.text)
                                .cursor_pointer()
                                .child(tr(cx, msg::native::saved_permission_rules))
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.open_permission_inspector(cx)
                                    }),
                                ),
                        )
                    });
                let menu = menu.when(unconfirmed, |view| {
                    view.child(
                        div()
                            .id("refresh-saved-permissions")
                            .role(Role::Button)
                            .aria_label(tr(cx, msg::native::refresh_saved_chat_choices))
                            .cursor_pointer()
                            .px(px(8.))
                            .py(px(8.))
                            .text_size(crate::typography::text_xs())
                            .text_color(popup_theme.accent)
                            .child(if pending {
                                ("Reading saved chat choices…").to_string()
                            } else {
                                crate::localization::text("native-refresh_saved_chat_choices", cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_permission(cx))),
                    )
                });
                view.child(popover::anchored_menu_above(
                    "composer-permission-popup",
                    menu.into_any_element(),
                    None,
                ))
            })
    }
}

#[cfg(feature = "browser-fixture")]
impl Composer {
    /// Review fixture: open the agent access menu.
    pub fn fixture_open_permissions(&mut self, cx: &mut Context<Self>) {
        let resolved = self.pickers.read(cx).resolved(cx);
        let current = effective(resolved.sandbox, resolved.harness.as_ref());
        self.permission_active = LEVELS
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        self.permission_open = true;
        cx.notify();
    }
    /// Review fixture: store `level` on the selected chat (or the draft).
    pub fn fixture_set_permission(&mut self, level: SandboxLevel, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let selected = state.selected_chat.clone();
            for chat in &mut state.chats {
                if Some(&chat.id) == selected.as_ref()
                    && let Some(config) = chat.config.as_mut()
                {
                    config.sandbox = level;
                }
            }
            cx.notify();
        });
        self.pickers
            .update(cx, |pickers, cx| pickers.set_draft_sandbox(level, cx));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::tests::composer_focus_window;

    #[test]
    fn approve_for_me_needs_an_automatic_approver() {
        assert_eq!(
            LEVELS.map(label),
            ["Ask for approval", "Approve for me", "Full access"]
        );
        for harness in [HarnessId::Codex, HarnessId::ClaudeCode] {
            assert_eq!(unavailable(SandboxLevel::Auto, Some(&harness)), None);
            assert_eq!(
                effective(SandboxLevel::Auto, Some(&harness)),
                SandboxLevel::Auto
            );
        }
        assert!(unavailable(SandboxLevel::Auto, Some(&HarnessId::Cursor)).is_some());
        assert_eq!(
            effective(SandboxLevel::Auto, Some(&HarnessId::Cursor)),
            SandboxLevel::WorkspaceWrite
        );
        assert_eq!(effective(SandboxLevel::Auto, None), SandboxLevel::Auto);
        for level in LEVELS {
            if level != SandboxLevel::Auto {
                assert_eq!(unavailable(level, Some(&HarnessId::Cursor)), None);
            }
        }
    }

    #[gpui::test]
    fn permission_choice_persists_globally_and_existing_chat_is_preserved_offline(
        cx: &mut gpui::TestAppContext,
    ) {
        let (directory, handle) = composer_focus_window(cx);
        handle
            .update(cx, |composer, window, cx| {
                composer.choose_permission(SandboxLevel::DangerFullAccess, window, cx);
                assert_eq!(
                    composer.pickers.read(cx).resolved(cx).sandbox,
                    SandboxLevel::DangerFullAccess
                );
                assert!(!composer.state.read(cx).configuration_saving());
                assert!(composer.failure.is_none());
                assert_eq!(
                    crate::settings::UiSettings::load(directory.path())
                        .default_permission_mode
                        .sandbox(),
                    SandboxLevel::DangerFullAccess
                );
                composer.state.update(cx, |state, cx| {
                    state.chats = vec![
                        serde_json::from_value(serde_json::json!({
                            "id":"saved", "deviceId":"local", "archived":false,
                            "createdAt":"2026-01-01T00:00:00Z",
                            "config":{"harness":"codex","sandbox":"auto"}
                        }))
                        .unwrap(),
                    ];
                    state.selected_chat = Some("saved".into());
                    cx.notify();
                });
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .update(cx, |composer, window, cx| {
                assert_eq!(
                    composer.pickers.read(cx).resolved(cx).sandbox,
                    SandboxLevel::Auto
                );
                composer.choose_permission(SandboxLevel::DangerFullAccess, window, cx);
                assert_eq!(
                    composer.pickers.read(cx).resolved(cx).sandbox,
                    SandboxLevel::Auto
                );
                assert!(
                    composer
                        .failure
                        .as_deref()
                        .unwrap()
                        .contains("Connect to the execution host")
                );
                assert_eq!(composer.failure_key.as_deref(), Some("saved"));
                assert_eq!(
                    crate::settings::default_permission_mode(cx).sandbox(),
                    SandboxLevel::DangerFullAccess
                );
                composer.state.update(cx, |state, cx| {
                    state.selected_chat = None;
                    cx.notify();
                });
            })
            .unwrap();
        cx.run_until_parked();
        handle
            .read_with(cx, |composer, cx| {
                assert_eq!(
                    composer.pickers.read(cx).resolved(cx).sandbox,
                    SandboxLevel::DangerFullAccess
                );
            })
            .unwrap();
        // Reload settings as on restart, then open a composer in another project.
        cx.update(|cx| {
            crate::settings::init(
                crate::settings::UiSettings::load(directory.path()),
                directory.path(),
                cx,
            );
        });
        let next = cx.add_window(|_, cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.selected_space = Some("another-project".into());
                state
            });
            Composer::new(state, cx)
        });
        next.update(cx, |composer, window, cx| {
            assert_eq!(
                composer.pickers.read(cx).resolved(cx).sandbox,
                SandboxLevel::DangerFullAccess
            );
            composer.choose_permission(SandboxLevel::WorkspaceWrite, window, cx);
            assert_eq!(
                crate::settings::UiSettings::load(directory.path())
                    .default_permission_mode
                    .sandbox(),
                SandboxLevel::WorkspaceWrite
            );
            composer.pickers.update(cx, |pickers, cx| {
                pickers.toggle_plan_mode(cx);
            });
            assert!(composer.pickers.read(cx).resolved(cx).plan);
            assert_eq!(
                crate::settings::default_permission_mode(cx).sandbox(),
                SandboxLevel::WorkspaceWrite
            );
        })
        .unwrap();
    }
}
