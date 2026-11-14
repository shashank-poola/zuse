//! Conversation tabs and checkout controls.
use super::*;
use crate::localization::message as tr;
use zuse_i18n::messages as msg;

#[cfg(test)]
#[path = "top_bar_tests.rs"]
mod tests;

/// The conversation tab row under the top bar.
pub(super) const CONVERSATION_TAB_ROW_HEIGHT: f32 = 36.0;
/// The branch menu's state: the checkout's local branches, fetched when the
/// menu opens, and a switch in flight.
#[derive(Default)]
pub(super) struct BranchMenu {
    pub(super) menu: popover::Popup<()>,
    pub(super) branches: Vec<String>,
    loaded_for: Option<String>,
    current_branch: Option<String>,
    loading: bool,
    error: Option<SharedString>,
    switching: Option<String>,
    task: Option<Task<()>>,
}

/// Where the selected conversation's branch lives, for the branch RPCs.
struct BranchTarget {
    chat_id: String,
    branch: String,
    repo_path: Option<String>,
    target_device: Option<String>,
    dirty: u32,
}

fn format_branch_error(branch: &str, error: &str) -> String {
    if error.contains("already used by worktree") || error.contains("already checked out at") {
        let path = error.rsplit('\'').nth(1).unwrap_or("");
        let location = if path.is_empty() {
            String::new()
        } else {
            format!("\n{path}")
        };
        format!(
            "Branch already in use\n“{branch}” is checked out in another worktree.{location}\nSwitch that worktree to another branch first."
        )
    } else if error.contains("local changes") || error.contains("would be overwritten") {
        "Uncommitted changes\nCommit or stash your changes before switching branches.".to_owned()
    } else {
        format!("Couldn’t switch branch\n{error}")
    }
}

impl Shell {
    pub(super) fn render_session_branch(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let target = self.branch_target(cx)?;
        let open = self.branch_menu.menu.is_open();
        let mut row = super::session_info::action_row(theme, "session-info-branch")
            .debug_selector(|| "session-info-branch".into())
            .relative()
            .role(gpui::Role::Button)
            .aria_label(tr(cx, msg::chat::top_bar_switch_branch))
            .aria_expanded(open)
            .tip(SharedString::from(target.branch.clone()))
            .when(open, |row| row.bg(theme.ink(0.06)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.branch_menu.menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.branch_menu.menu.take_press_was_open() {
                    this.close_branch_menu(cx);
                } else {
                    this.close_summary_pr_menu(cx);
                    this.open_branch_menu(cx);
                }
            }))
            .child(popover::picker_icon_slot(super::session_info::row_glyph(
                icons::GIT_BRANCH,
                theme,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(target.branch.clone()),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            );
        if open {
            let menu = self.render_branch_menu(&target, theme, cx);
            row = row.child(popover::nested_menu("session-info-branch-menu", menu, true));
        }
        Some(row.into_any_element())
    }

    fn branch_target(&self, cx: &App) -> Option<BranchTarget> {
        let conversation = self.conversation_state(cx);
        let state = conversation.read(cx);
        let chat = state.selected_chat_row()?;
        let mut branch =
            crate::change_requests::conversation_branch(chat, &state.spaces)?.to_owned();
        let repo_path = chat
            .source_context
            .as_ref()
            .map(|source| source.repo_root.clone())
            .or_else(|| chat.cwd.clone());
        let target_device = (self.cloud_conversation.is_none()
            && state.local_device_id.as_deref() != Some(chat.device_id.as_str()))
        .then(|| chat.device_id.clone());
        if let Some(path) = &repo_path {
            let key = format!("{}:{path}", target_device.as_deref().unwrap_or("local"));
            if self.branch_menu.loaded_for.as_deref() == Some(key.as_str()) {
                if let Some(current) = &self.branch_menu.current_branch {
                    branch = current.clone();
                }
            }
        }
        Some(BranchTarget {
            chat_id: chat.id.clone(),
            branch,
            repo_path,
            target_device,
            dirty: state.dirty_files_for_chat(chat),
        })
    }

    /// Keep a separate tab row only when the expanded pane occupies the titlebar.
    pub(super) fn conversation_tab_row_visible(&self, cx: &App) -> bool {
        matches!(self.route, Route::Chat)
            && self.has_conversation(cx)
            && !self.state.read(cx).selected_chat_is_zot_coordinator()
            && self.titlebar_trailing_layout(false, cx).takeover
    }

    /// The conversation tabs on their own row below the top bar: main-area
    /// tabs, the thread's conversations and side chats, `+`, and history.
    pub(super) fn render_conversation_tab_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let tabs = if self.cloud_conversation.is_some() {
            self.render_cloud_tabs(cx)
                .unwrap_or_else(|| Empty.into_any_element())
        } else {
            self.render_thread_conversations(cx)
        };
        div()
            .id("conversation-tab-row")
            .debug_selector(|| "conversation-tab-row".into())
            .absolute()
            .top(px(Theme::TITLEBAR_HEIGHT))
            .left_0()
            .right_0()
            .h(px(CONVERSATION_TAB_ROW_HEIGHT))
            .pl(px((self.overview_content_left(cx)
                - self.sidebar_now()
                - self.workspace_gutter())
            .max(Theme::SPACE_SM)))
            .pr(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .text_size(crate::typography::text_xs())
            .text_color(theme.text)
            // The transcript fades out beneath this row; a wheel over the
            // tabs scrolls the strip, never the conversation behind it.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .occlude()
            .child(tabs)
            .into_any_element()
    }

    /// The checkout pill and its branch menu, absent without a checkout.
    pub(super) fn render_top_bar_branch(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(target) = self.branch_target(cx) else {
            let branch = self.cloud_conversation.as_ref()?.read(cx).branch_label()?;
            // Saved task metadata can identify the branch before the runtime
            // supplies a checkout context. Keep it read-only until then.
            return Some(
                div()
                    .debug_selector(|| "cloud-saved-branch".into())
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(theme.text_muted)
                    .child(icon(icons::GIT_BRANCH).size(px(14.)))
                    .child(div().max_w(px(180.)).truncate().child(branch))
                    .into_any_element(),
            );
        };
        let open = self.branch_menu.menu.get().is_some();
        let label: SharedString = target.branch.clone().into();
        let fade = "top-bar-branch";
        let mut trigger = div()
            .id("top-bar-branch")
            .debug_selector(|| "top-bar-branch".into())
            .role(gpui::Role::Button)
            .aria_label(tr(cx, msg::chat::top_bar_switch_branch))
            .aria_expanded(open)
            .h(px(24.0))
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(9.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_color(if open { theme.text } else { theme.text_muted })
            .bg(theme.surface_card.opacity(1.0))
            .hover(|style| style.bg(theme.element_hover))
            .on_hover(motion::hover_listener(fade))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.branch_menu.menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.branch_menu.menu.take_press_was_open() {
                    this.close_branch_menu(cx);
                } else {
                    this.open_branch_menu(cx);
                }
            }))
            .tip(label.clone())
            .tooltip_show_delay(Duration::from_millis(350))
            .child(
                icon(icons::GIT_BRANCH)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .max_w(px(180.0))
                    .truncate()
                    .child(label),
            )
            .when(target.dirty > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted)
                        .child(format!("· {}", target.dirty)),
                )
            })
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            );
        if open {
            let closing = self.branch_menu.menu.closing_since();
            let menu = self.render_branch_menu(&target, theme, cx);
            let popup = if self.cloud_conversation.is_some() {
                popover::anchored_menu_below("top-bar-branch-menu", menu, closing)
            } else {
                popover::anchored_menu_above_end("top-bar-branch-menu", menu, closing)
            };
            trigger = trigger.relative().child(popup);
        }
        Some(
            div()
                .id("top-bar-identity")
                .debug_selector(|| "top-bar-identity".into())
                .role(gpui::Role::Navigation)
                .aria_label(tr(cx, msg::projects::pr_pane_branch))
                .min_w_0()
                .w_full()
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(12.0))
                .child(trigger)
                .into_any_element(),
        )
    }

    fn copy_branch_name(&mut self, branch: String, cx: &mut Context<Self>) {
        self.close_branch_menu(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(branch));
        self.show_notice(crate::toast::ToastKind::Success, "Branch name copied", cx);
    }

    fn open_branch_menu(&mut self, cx: &mut Context<Self>) {
        self.branch_menu.menu.open(());
        self.branch_menu.error = None;
        self.refresh_branch_list(cx);
        cx.notify();
    }

    pub(super) fn close_branch_menu(&mut self, cx: &mut Context<Self>) {
        if self.branch_menu.menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.branch_menu.menu);
        }
        cx.notify();
    }

    /// Fetch the checkout's branches. The list shows what it had while the
    /// request runs; a different checkout starts from empty.
    fn refresh_branch_list(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.branch_target(cx) else {
            return;
        };
        let Some(repo_path) = target.repo_path.clone() else {
            return;
        };
        let Some(engine) = self.conversation_state(cx).read(cx).engine().cloned() else {
            return;
        };
        let key = format!(
            "{}:{repo_path}",
            target.target_device.as_deref().unwrap_or("local")
        );
        if self.branch_menu.loaded_for.as_deref() != Some(key.as_str()) {
            self.branch_menu.branches.clear();
            self.branch_menu.current_branch = None;
        }
        self.branch_menu.loaded_for = Some(key.clone());
        self.branch_menu.loading = true;
        let mut params = serde_json::Map::new();
        params.insert("repoPath".into(), repo_path.into());
        params.insert("chatId".into(), target.chat_id.into());
        if let Some(device) = target.target_device {
            params.insert("targetDeviceId".into(), device.into());
        }
        self.branch_menu.task = Some(cx.spawn(async move |this, cx| {
            let refs_params = serde_json::Value::Object(params.clone());
            let result = engine
                .client()
                .call(methods::LIST_BRANCHES, serde_json::Value::Object(params))
                .await;
            let refs = engine.client().call(methods::LIST_REFS, refs_params).await;
            let _ = this.update(cx, |this, cx| {
                if this.branch_menu.loaded_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                this.branch_menu.loading = false;
                if let Ok(value) = refs {
                    if let Ok(refs) = serde_json::from_value::<Vec<zuse_protocol::RepoRef>>(value) {
                        this.branch_menu.current_branch = refs
                            .into_iter()
                            .find(|item| item.current)
                            .map(|item| item.name);
                    }
                }
                match result {
                    Ok(value) => {
                        this.branch_menu.branches =
                            serde_json::from_value::<Vec<String>>(value).unwrap_or_default();
                    }
                    Err(error) => this.branch_menu.error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    /// Check out `branch` in the conversation's checkout. Git refuses when
    /// uncommitted changes conflict; its message shows in the menu.
    fn switch_top_bar_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        if self.branch_menu.switching.is_some() {
            return;
        }
        let Some(target) = self.branch_target(cx) else {
            return;
        };
        if target.branch == branch {
            self.close_branch_menu(cx);
            return;
        }
        let (Some(repo_path), Some(engine)) = (
            target.repo_path,
            self.conversation_state(cx).read(cx).engine().cloned(),
        ) else {
            return;
        };
        let mut params = serde_json::Map::new();
        let key = format!(
            "{}:{repo_path}",
            target.target_device.as_deref().unwrap_or("local")
        );
        params.insert("repoPath".into(), repo_path.into());
        params.insert("refName".into(), branch.clone().into());
        if let Some(device) = target.target_device {
            params.insert("targetDeviceId".into(), device.into());
        }
        self.branch_menu.error = None;
        self.branch_menu.switching = Some(branch.clone());
        self.branch_menu.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.branch_menu.loaded_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                this.branch_menu.switching = None;
                match result {
                    Ok(value) => {
                        this.branch_menu.current_branch = Some(
                            value
                                .get("branch")
                                .and_then(|value| value.as_str())
                                .unwrap_or(&branch)
                                .to_owned(),
                        );
                        this.close_branch_menu(cx)
                    }
                    Err(error) => {
                        this.branch_menu.error =
                            Some(format_branch_error(&branch, &error.to_string()).into())
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The branch menu: the current branch with a copy button, the
    /// checkout's local branches (current one checked), and copy.
    fn render_branch_menu(
        &mut self,
        target: &BranchTarget,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme.for_popup();
        let current = target.branch.clone();
        let section_label = |label: &'static str| {
            div()
                .px(px(8.0))
                .pt(px(4.0))
                .pb(px(2.0))
                .text_size(crate::typography::text_xs())
                .text_color(theme.text_muted)
                .child(label)
        };
        let loading = self.branch_menu.loading && self.branch_menu.branches.is_empty();
        let switching = self.branch_menu.switching.clone();
        let rows = self
            .branch_menu
            .branches
            .iter()
            .map(|name| {
                let is_current = *name == current;
                let fade = format!("branch-menu-row-{name}");
                let pick = name.clone();
                let busy = switching.as_deref() == Some(name.as_str());
                popover::menu_row(&theme, is_current, fade.clone())
                    .id(SharedString::from(fade))
                    .role(gpui::Role::MenuItem)
                    .aria_label(SharedString::from(name.clone()))
                    .aria_selected(is_current)
                    .gap(px(8.0))
                    .min_h(px(32.0))
                    .py(px(4.0))
                    .text_size(crate::typography::text_sm())
                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                    .when(busy, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(theme.text_muted)
                                .child(tr(cx, msg::native::switching)),
                        )
                    })
                    .child(
                        icon(icons::CHECK)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text)
                            .when(!is_current, |glyph| glyph.opacity(0.0)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.switch_top_bar_branch(pick.clone(), cx)
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let empty = rows.is_empty();
        let copy_branch = current.clone();
        let longest = self
            .branch_menu
            .branches
            .iter()
            .map(|name| name.chars().count())
            .chain(std::iter::once(current.chars().count()))
            .max()
            .unwrap_or(0);
        let width = (longest as f32 * 6.5 + 48.0).clamp(248.0, 320.0);
        popover::popover_card(&theme)
            .debug_selector(|| "branch-menu".into())
            .w(px(width))
            .bg(theme.surface_card.opacity(1.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_branch_menu(cx)))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pl(px(8.0))
                    .py(px(2.0))
                    .child(
                        icon(icons::GIT_BRANCH)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text)
                            .child(current.clone()),
                    )
                    .child(header_icon_button(
                        "branch-menu-copy",
                        icons::COPY,
                        "Copy branch name",
                        &theme,
                        cx.listener(move |this, _, _, cx| {
                            this.copy_branch_name(copy_branch.clone(), cx)
                        }),
                    )),
            )
            .child(popover::menu_separator())
            .when_some(self.branch_menu.error.clone(), |card, error| {
                let (title, detail) = error
                    .split_once('\n')
                    .map(|(title, detail)| (title.to_owned(), detail.to_owned()))
                    .unwrap_or_else(|| ("Couldn’t load branches".into(), error.to_string()));
                card.child(
                    div()
                        .px(px(8.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .bg(theme.danger.opacity(0.08))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .text_size(crate::typography::text_xs())
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.danger)
                                .child(title),
                        )
                        .child(div().text_color(theme.text).child(detail)),
                )
            })
            .child(section_label("Local branches"))
            .child(
                div()
                    .id("branch-menu-list")
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows)
                    .when(empty, |list| {
                        list.child(
                            div()
                                .px(px(8.0))
                                .py(px(6.0))
                                .text_size(crate::typography::text_xs())
                                .text_color(theme.text_muted)
                                .child(if loading {
                                    ("Loading branches…").to_string()
                                } else {
                                    crate::localization::text("chat-top_bar_no_local_branches", cx)
                                }),
                        )
                    }),
            )
            .when(target.dirty > 0, |card| {
                card.child(popover::menu_separator()).child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_size(crate::typography::text_xs())
                        .text_color(theme.text_muted)
                        .child(format!(
                            "{} uncommitted {}",
                            target.dirty,
                            if target.dirty == 1 {
                                "change"
                            } else {
                                "changes"
                            }
                        )),
                )
            })
            .into_any_element()
    }
}

#[cfg(feature = "browser-fixture")]
impl Shell {
    /// Open the branch menu over a fixed branch list (no engine to ask).
    pub(super) fn fixture_open_branch_menu(&mut self, cx: &mut Context<Self>) {
        self.branch_menu.branches = [
            "main",
            "alex/connection-handoff",
            "alex/file-tree-icons",
            "release/0.9",
        ]
        .map(str::to_owned)
        .to_vec();
        self.branch_menu.menu.open(());
        cx.notify();
    }
}
