//! The session card: where this chat runs (device, project, checkout,
//! branch, and the branch's pull request), a new side chat, and the
//! project's run actions, in one toggleable card. Its
//! rows follow the environment summary's idiom: 28px icon rows, 12px text,
//! a glass card with no dividers.
//! The card is a popover: its titlebar toggle opens it over the transcript,
//! and a press elsewhere or choosing one of its rows dismisses it. It never
//! takes room from the transcript or the composer.
use super::*;
use crate::localization::message as tr;
use zuse_i18n::messages as msg;

use crate::project_actions::{ProjectActionsStatus, action_icon};

/// The card's width, its inset from the column's right edge, and its drop
/// below the titlebar that holds its toggle.
const SESSION_CARD_WIDTH: f32 = 264.0;
const SESSION_CARD_INSET: f32 = 12.0;
const SESSION_CARD_DROP: f32 = 4.0;

/// The card's row height, padding and corner radius.
const SUMMARY_ROW_HEIGHT: f32 = 32.0;
const SUMMARY_CARD_PADDING: f32 = 6.0;
const SUMMARY_CARD_RADIUS: f32 = 12.0;
/// Where a row's label starts (padding + icon + gap): sub-rows indent to it.
const SUMMARY_LABEL_INSET: f32 = 34.0;

/// One summary row: 16px icon slot, 12px label, trailing meta by the caller.
pub(super) fn summary_row(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .w_full()
        .min_w_0()
        .min_h(px(SUMMARY_ROW_HEIGHT))
        .px(px(10.0))
        .rounded(px(6.0))
        .text_size(crate::typography::text_sm())
        .text_color(theme.text)
}

/// A non-interactive card row: icon slot and label.
fn info_row(theme: &Theme, glyph: AnyElement, label: SharedString) -> gpui::Div {
    summary_row(theme)
        .child(popover::picker_icon_slot(glyph))
        .child(div().flex_1().min_w_0().truncate().child(label))
}

/// A clickable card row, washed on hover.
pub(super) fn action_row(theme: &Theme, id: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    let wash = theme.ink(0.06);
    summary_row(theme)
        .id(id.into())
        .cursor_pointer()
        .hover(move |style| style.bg(wash))
}

/// The background processes still running, oldest first.
fn running_background(
    tasks: Vec<zuse_protocol::BackgroundTask>,
) -> Vec<zuse_protocol::BackgroundTask> {
    tasks
        .into_iter()
        .filter(zuse_protocol::BackgroundTask::running)
        .collect()
}

fn background_label(task: &zuse_protocol::BackgroundTask) -> SharedString {
    match task.description.trim() {
        "" => "Background process".into(),
        text => text.to_owned().into(),
    }
}

/// The command line shown under a task, unless it only repeats the label.
fn background_command(task: &zuse_protocol::BackgroundTask) -> Option<SharedString> {
    let command = task.command.as_deref()?.trim();
    (!command.is_empty() && command != background_label(task).as_ref())
        .then(|| command.to_owned().into())
}

/// A quiet sub-row under a section, indented to its labels.
pub(super) fn sub_row(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .min_h(px(24.0))
        .pl(px(SUMMARY_LABEL_INSET))
        .pr(px(10.0))
        .text_size(crate::typography::text_xs())
        .text_color(theme.text_muted)
}

pub(super) fn row_glyph(path: &'static str, theme: &Theme) -> AnyElement {
    icon(path)
        .size(px(16.0))
        .text_color(theme.text_muted)
        .into_any_element()
}

/// Trailing status on a row: counts, states, diff sizes.
pub(super) fn row_meta(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(11.0))
        .text_color(theme.text_muted)
}

/// The faint rule between the workspace rows and the session's activity:
/// a hairline that fades out toward both edges.
fn fade_rule(theme: &Theme) -> gpui::Div {
    let ink = theme.text.opacity(0.15);
    let half = |from: gpui::Hsla, to: gpui::Hsla| {
        div().flex_1().h_full().bg(gpui::linear_gradient(
            90.,
            gpui::linear_color_stop(from, 0.0),
            gpui::linear_color_stop(to, 1.0),
        ))
    };
    div()
        .flex_none()
        .mx(px(10.0))
        .my(px(6.0))
        .h(px(1.0))
        .flex()
        .child(half(ink.opacity(0.0), ink))
        .child(half(ink, ink.opacity(0.0)))
}

/// A spawned subagent of this chat, from its spawn chip.
struct SummaryAgent {
    doc_id: String,
    title: SharedString,
    status: zuse_doc::SubagentStatus,
}

/// The chat's subagents in spawn order, one per subagent doc.
fn chat_agents(entries: &[zuse_doc::SessionMessageEntry]) -> Vec<SummaryAgent> {
    use zuse_doc::{MessagePart, SubagentStatus};
    let mut agents: Vec<SummaryAgent> = Vec::new();
    for part in entries.iter().flat_map(|entry| &entry.parts) {
        let MessagePart::Tool {
            call,
            is_error,
            resolved,
            subagent_ref: Some(doc_id),
            subagent_status,
            ..
        } = part
        else {
            continue;
        };
        if !call.is_subagent_spawn() {
            continue;
        }
        let status = subagent_status.unwrap_or(if *is_error {
            SubagentStatus::Failed
        } else if *resolved {
            SubagentStatus::Done
        } else {
            SubagentStatus::Running
        });
        let agent = SummaryAgent {
            doc_id: doc_id.clone(),
            title: crate::transcript::subagent_tab_title(call),
            status,
        };
        match agents.iter_mut().find(|known| known.doc_id == agent.doc_id) {
            Some(known) => *known = agent,
            None => agents.push(agent),
        }
    }
    agents
}

/// A subagent's state at a glance: a live dot, a check, or a cross.
fn agent_status_glyph(status: zuse_doc::SubagentStatus, theme: &Theme) -> AnyElement {
    use zuse_doc::SubagentStatus;
    let slot = div()
        .size(px(12.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    match status {
        SubagentStatus::Running => slot.child(
            div()
                .size(px(9.0))
                .rounded_full()
                .bg(theme.success.opacity(0.3))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(5.0)).rounded_full().bg(theme.success)),
        ),
        SubagentStatus::Done => {
            slot.child(icon(icons::CHECK).size(px(12.0)).text_color(theme.success))
        }
        SubagentStatus::Failed => {
            slot.child(icon(icons::CLOSE).size(px(12.0)).text_color(theme.danger))
        }
    }
    .into_any_element()
}

impl Shell {
    /// Whether the card is open over the transcript.
    pub(super) fn session_info_visible(&self) -> bool {
        self.session_info_overlay
    }

    /// The toggle's mouse-down: note whether the card showed, so the click
    /// that follows closes it rather than reopening it. Reads the last
    /// painted state, which the overlay's click-outside dismissal (handled
    /// on the same press, in either order) leaves alone.
    pub(super) fn note_session_info_press(&mut self) {
        self.session_info_pressed_open = Some(self.session_info_shown);
    }

    pub(super) fn toggle_session_info(&mut self, cx: &mut Context<Self>) {
        let open = self
            .session_info_pressed_open
            .take()
            .unwrap_or(self.session_info_shown);
        if open {
            self.close_session_info_overlay(cx);
        } else {
            self.session_info_overlay = true;
        }
        cx.notify();
    }

    /// Dismiss the card.
    pub(super) fn close_session_info_overlay(&mut self, cx: &mut Context<Self>) {
        if self.session_info_overlay {
            self.session_info_overlay = false;
            self.close_summary_pr_menu(cx);
            self.close_branch_menu(cx);
            cx.notify();
        }
    }

    /// This frame's reveal of the card, 0 (hidden) → 1 (shown), eased on the
    /// shell's resize clock whenever it opens or closes.
    pub(super) fn session_info_reveal(&mut self, has_selection: bool) -> f32 {
        let shown = has_selection && self.session_info_visible();
        let target = if shown { 1.0 } else { 0.0 };
        if shown != self.session_info_shown {
            let from = self.eval_tween(self.session_info_tween, 1.0 - target);
            self.session_info_tween = Some(WidthTween::new(from, target));
            self.session_info_shown = shown;
        }
        self.eval_tween(self.session_info_tween, target)
    }

    /// The card, positioned within the main column; `None` while closed or
    /// with no chat selected.
    pub(super) fn render_session_info(
        &mut self,
        reveal: f32,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if reveal <= 0.001 || self.state.read(cx).selected_chat_is_zot_coordinator() {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let now = Utc::now();
        let (
            chat,
            device,
            local,
            online,
            project,
            checkout,
            branch,
            change_request,
            diff_stat,
            agents,
            background,
        ) = {
            let state = self.state.read(cx);
            let chat = state.selected_chat_row()?.clone();
            let device: SharedString = state
                .device_name(&chat.device_id)
                .unwrap_or("Unknown device")
                .to_owned()
                .into();
            let local = state.local_device_id.as_deref() == Some(chat.device_id.as_str());
            let online = state.device_online(&chat.device_id, now);
            let project = chat
                .space_id
                .as_deref()
                .and_then(|id| state.space_row(id))
                .map(|space| {
                    (
                        SharedString::from(space.display_name().to_owned()),
                        SharedString::from(space.path.clone()),
                    )
                });
            let branch = crate::change_requests::conversation_branch(&chat, &state.spaces)
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(|branch| SharedString::from(branch.to_owned()));
            // Git projects name the checkout kind: the project folder itself,
            // or a worktree of it.
            let checkout = chat
                .space_id
                .as_deref()
                .and_then(|id| state.space_row(id))
                .filter(|space| space.git_detected)
                .map(|space| {
                    let cwd = chat.cwd.clone().unwrap_or_else(|| space.path.clone());
                    (cwd != space.path, SharedString::from(cwd))
                });
            let change_request = state.change_request_for_chat(&chat).cloned();
            let diff_stat = state.diff_stat_for_chat(&chat);
            let agents = chat_agents(&state.transcript);
            // Detached agents are listed with the other agents; this lists
            // processes (shells, dev servers, monitors).
            let background: Vec<zuse_protocol::BackgroundTask> =
                zuse_protocol::view::background_tasks(state.session_for(&chat.id), now)
                    .iter()
                    .filter(|task| task.kind != zuse_protocol::BackgroundTaskKind::Agent)
                    .cloned()
                    .collect();
            (
                chat,
                device,
                local,
                online,
                project,
                checkout,
                branch,
                change_request,
                diff_stat,
                agents,
                background,
            )
        };

        // ── Where it runs: the device (a remote one shows whether it is
        // reachable; its tooltip names the project folder) and the branch.
        let device_row = info_row(
            &theme,
            row_glyph(if local { icons::LAPTOP } else { icons::MONITOR }, &theme),
            device,
        )
        .id("session-info-device")
        .when_some(project.map(|(_, path)| path), |row, path| row.tip(path))
        .when(!local, |row| {
            row.child(
                row_meta(&theme).child(div().size(px(6.0)).rounded_full().bg(if online {
                    theme.success
                } else {
                    theme.warning
                })),
            )
        });
        // Without a branch the checkout keeps a row of its own.
        let git = checkout.is_some();
        let checkout_path = checkout.as_ref().map(|(_, path)| path.clone());
        let branch_row = match branch {
            Some(branch) => self.render_session_branch(&theme, cx).or_else(|| {
                Some(
                    info_row(&theme, row_glyph(icons::GIT_BRANCH, &theme), branch.clone())
                        .id("session-info-branch")
                        .tip(checkout_path.clone().unwrap_or(branch))
                        .into_any_element(),
                )
            }),
            None => checkout.map(|(worktree, path)| {
                let (glyph, label) = if worktree {
                    (icons::WORKTREE, "Worktree")
                } else {
                    (icons::FOLDER, "Local checkout")
                };
                info_row(&theme, row_glyph(glyph, &theme), label.into())
                    .id("session-info-checkout")
                    .debug_selector(|| "session-info-checkout".into())
                    .tip(path)
                    .into_any_element()
            }),
        };

        // ── What it changed: the checkout's diff (opens the review) and the
        // branch's pull request.
        let changes_row = git.then(|| {
            action_row(&theme, "session-info-changes")
                .debug_selector(|| "session-info-changes".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_session_info_overlay(cx);
                    this.open_review_tab(window, cx);
                }))
                .child(popover::picker_icon_slot(row_glyph(
                    icons::GIT_COMPARE,
                    &theme,
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(tr(cx, msg::chat::environment_summary_branch_changes)),
                )
                .when_some(diff_stat, |row, (additions, deletions)| {
                    row.child(
                        sidebar_diff_stat(additions, deletions, 1.0, &theme).text_size(px(11.0)),
                    )
                })
        });
        // The branch's pull request, with its menu (view, repair, merge,
        // add to chat, status, GitHub) and its next step.
        let pull_request_rows = match change_request {
            Some(summary) => {
                let target = (!local).then(|| chat.device_id.clone());
                self.render_summary_pull_request(summary, target, &theme, cx)
            }
            None => Vec::new(),
        };

        // ── What runs for it: background processes, then the chat's
        // subagents, running ones first.
        let mut activity = self.render_session_info_background(&chat.id, background, &theme, cx);
        activity.extend(self.render_session_info_agents(&chat, agents, &theme, cx));

        // ── This conversation. Forking lives on each reply's footer.
        let busy = self.side_chat_creating;
        let side_chat = action_row(&theme, "session-info-side-chat")
            .debug_selector(|| "session-info-side-chat".into())
            .when(busy, |row| row.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_session_info_overlay(cx);
                this.create_child_chat(None, cx)
            }))
            .child(popover::picker_icon_slot(row_glyph(icons::PLUS, &theme)))
            .child(div().flex_1().child(tr(cx, msg::native::new_side_chat)));

        // ── The project's run actions.
        let actions = self.render_session_info_actions(&theme, cx);

        let body = div()
            .id("session-info-body")
            .max_h(px(max_height.max(120.0)))
            .overflow_y_scroll()
            .p(px(SUMMARY_CARD_PADDING))
            .flex()
            .flex_col()
            .child(device_row)
            .children(branch_row)
            .children(changes_row)
            .children(pull_request_rows)
            // The session's activity sits under a rule, apart from the
            // workspace it runs in.
            .when(!activity.is_empty(), |body| {
                body.child(fade_rule(&theme)).children(activity)
            })
            .child(fade_rule(&theme))
            .child(side_chat)
            .children(actions)
            .child(self.recovery.clone());

        let card = popover::popover_card(&theme)
            .w(px(SESSION_CARD_WIDTH))
            .p(px(0.0))
            .child(body);

        // It drops in from just under its toggle, placed from the column's
        // right edge so it rides that edge while panes animate.
        let ease = crate::motion::RESIZE.progress(reveal.clamp(0.0, 1.0));
        let dy = -6.0 * (1.0 - ease);
        Some(
            div()
                .id("session-info")
                .debug_selector(|| "session-info".into())
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + SESSION_CARD_DROP + dy))
                .right(px(SESSION_CARD_INSET))
                .opacity(ease)
                .occlude()
                // A press elsewhere, or choosing one of its rows, dismisses it.
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    // The pull request menu floats beside the card and
                    // dismisses itself first.
                    if !this.summary_pr.menu_open && !this.branch_menu.menu.is_open() {
                        this.close_session_info_overlay(cx);
                    }
                }))
                .child(crate::frost::frosted(
                    SUMMARY_CARD_RADIUS,
                    crate::frost::MENU_BLUR,
                    card,
                ))
                .into_any_element(),
        )
    }

    /// The environment summary for a cloud conversation: the computer and
    /// its state, the repository and branch it works on, and the actions a
    /// computer has (sleep or wake, its details, the synced folder).
    pub(super) fn render_cloud_session_info(
        &mut self,
        conversation: &Entity<super::cloud_conversation::CloudConversation>,
        reveal: f32,
        max_height: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if reveal <= 0.001 {
            return None;
        }
        let theme = Theme::of(cx).for_popup();
        let (summary, synced, resources) = {
            let view = conversation.read(cx);
            (
                view.computer_summary(),
                view.synced_folder(),
                view.resources().clone(),
            )
        };
        let running = matches!(summary.tone, super::cloud_conversation::Tone::Running);
        if running {
            conversation.update(cx, |view, cx| view.want_resources(cx));
        }
        let computer = summary
            .provider
            .clone()
            .map(|provider| format!("{provider} computer"))
            .unwrap_or_else(|| "Cloud computer".into());
        let computer_row = info_row(
            &theme,
            row_glyph(icons::DITHER_CLOUD, &theme),
            computer.into(),
        )
        .id("cloud-session-info-computer")
        .child(
            row_meta(&theme)
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(summary.tone.color(&theme)),
                )
                .child(summary.label),
        );
        let repository_row = summary.repository.clone().map(|repository| {
            info_row(&theme, row_glyph(icons::FOLDER, &theme), repository.into())
                .id("cloud-session-info-repository")
        });
        let branch_row = summary.branch.clone().map(|branch| {
            info_row(&theme, row_glyph(icons::GIT_BRANCH, &theme), branch.into())
                .id("cloud-session-info-branch")
        });
        // Its size, and when it sleeps on its own.
        let size_row = info_row(
            &theme,
            row_glyph(icons::CLOCK_CIRCLE, &theme),
            if summary.sleeping {
                "Wakes when you send a message".into()
            } else {
                "Sleeps after 10 min idle".into()
            },
        )
        .id("cloud-session-info-idle")
        .when_some(summary.size.clone(), |row, size| {
            row.child(row_meta(&theme).child(size))
        });
        // A computer that restarted on wake lost its terminals' processes and
        // background work; say so rather than look identical to a memory wake.
        // Only once running: while a new wake is pending, the stored result
        // still describes the previous one.
        let wake_row = summary.last_wake.clone().filter(|_| running).map(|wake| {
            info_row(&theme, row_glyph(icons::CLOCK_CIRCLE, &theme), wake.into())
                .id("cloud-session-info-wake")
        });
        // Performance: one row with CPU at a glance, unfolding to the bars.
        let cpu = resources["cpuPercent"].as_f64();
        let (performance_header, performance_open) = self.session_info_section(
            "cloud-performance",
            false,
            row_glyph(icons::MONITOR, &theme),
            "Performance",
            Some(SharedString::from(match (running, cpu) {
                (false, _) => "Not running".to_owned(),
                (true, Some(cpu)) => format!("{cpu:.0}% CPU"),
                (true, None) => "Measuring…".to_owned(),
            })),
            &theme,
            cx,
        );
        let mut performance = vec![performance_header];
        if performance_open && running {
            let bytes = |value: &serde_json::Value| value.as_u64().map(|v| v as f64 / 1e9);
            let meter = |label: &'static str, fraction: Option<f64>, detail: String| {
                let fraction = fraction.unwrap_or(0.0).clamp(0.0, 1.0) as f32;
                let tone = if fraction > 0.9 {
                    theme.danger
                } else if fraction > 0.7 {
                    theme.warning
                } else {
                    theme.success
                };
                sub_row(&theme)
                    .flex_col()
                    .items_start()
                    .gap(px(4.0))
                    .py(px(4.0))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_between()
                            .child(label)
                            .child(div().text_color(theme.text_muted).child(detail)),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(theme.text.opacity(0.08))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded_full()
                                    .bg(tone),
                            ),
                    )
                    .into_any_element()
            };
            let cores = resources["cores"].as_u64();
            performance.push(meter(
                "CPU",
                cpu.map(|cpu| cpu / 100.0),
                match (cpu, cores) {
                    (Some(cpu), Some(cores)) => format!("{cpu:.0}% of {cores} cores"),
                    (Some(cpu), None) => format!("{cpu:.0}%"),
                    _ => "—".into(),
                },
            ));
            for (label, used, total) in [
                ("Memory", "memoryUsedBytes", "memoryTotalBytes"),
                ("Disk", "diskUsedBytes", "diskTotalBytes"),
            ] {
                let (used, total) = (bytes(&resources[used]), bytes(&resources[total]));
                performance.push(meter(
                    label,
                    used.zip(total)
                        .filter(|(_, t)| *t > 0.0)
                        .map(|(u, t)| u / t),
                    match (used, total) {
                        (Some(used), Some(total)) => format!("{used:.1} of {total:.1} GB"),
                        _ => "—".into(),
                    },
                ));
            }
        }
        let sleep = {
            let conversation = conversation.clone();
            action_row(&theme, "cloud-session-info-sleep")
                .debug_selector(|| "cloud-session-info-sleep".into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    conversation.update(cx, |view, cx| view.toggle_sleep(cx));
                }))
                .child(popover::picker_icon_slot(row_glyph(
                    if summary.sleeping {
                        icons::ACTION_PLAY
                    } else {
                        icons::MOON
                    },
                    &theme,
                )))
                .child(div().flex_1().child(if summary.sleeping {
                    "Wake computer"
                } else {
                    "Put computer to sleep"
                }))
        };
        let details = {
            let conversation = conversation.clone();
            action_row(&theme, "cloud-session-info-details")
                .debug_selector(|| "cloud-session-info-details".into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    conversation.update(cx, |view, cx| view.open_computer(cx));
                }))
                .child(popover::picker_icon_slot(row_glyph(
                    icons::SETTINGS,
                    &theme,
                )))
                .child(div().flex_1().child("Computer details and sync"))
        };
        let folder = synced.map(|path| {
            let reveal_path = path.clone();
            action_row(&theme, "cloud-session-info-folder")
                .tip(path)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    cx.reveal_path(std::path::Path::new(&reveal_path));
                }))
                .child(popover::picker_icon_slot(row_glyph(
                    icons::FOLDER_OPEN,
                    &theme,
                )))
                .child(div().flex_1().child("Open synced folder"))
        });
        let body = div()
            .id("cloud-session-info-body")
            .max_h(px(max_height.max(120.0)))
            .overflow_y_scroll()
            .p(px(SUMMARY_CARD_PADDING))
            .flex()
            .flex_col()
            .child(computer_row)
            .children(repository_row)
            .children(branch_row)
            .child(size_row)
            .children(wake_row)
            .child(fade_rule(&theme))
            .children(performance)
            .child(fade_rule(&theme))
            .child(sleep)
            .children(folder)
            .child(details);
        let card = popover::popover_card(&theme)
            .w(px(SESSION_CARD_WIDTH))
            .p(px(0.0))
            .child(body);
        let ease = crate::motion::RESIZE.progress(reveal.clamp(0.0, 1.0));
        let dy = -6.0 * (1.0 - ease);
        Some(
            div()
                .id("session-info")
                .debug_selector(|| "session-info".into())
                .absolute()
                .top(px(Theme::TITLEBAR_HEIGHT + SESSION_CARD_DROP + dy))
                .right(px(SESSION_CARD_INSET))
                .opacity(ease)
                .occlude()
                .on_mouse_down_out(
                    cx.listener(|this, _, _, cx| this.close_session_info_overlay(cx)),
                )
                .child(crate::frost::frosted(
                    SUMMARY_CARD_RADIUS,
                    crate::frost::MENU_BLUR,
                    card,
                ))
                .into_any_element(),
        )
    }

    /// A collapsible section's header: an ordinary icon row whose chevron
    /// folds the rows beneath it. Returns the row and whether it is open.
    /// `session_info_folded` records the sections toggled away from
    /// `default_open`.
    #[allow(clippy::too_many_arguments)]
    fn session_info_section(
        &mut self,
        key: &'static str,
        default_open: bool,
        glyph: AnyElement,
        label: &'static str,
        meta: Option<SharedString>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, bool) {
        let open = default_open != self.session_info_folded.contains(key);
        let row = action_row(
            theme,
            SharedString::from(format!("session-info-section-{key}")),
        )
        .debug_selector(move || format!("session-info-section-{key}"))
        .role(gpui::Role::Button)
        .aria_label(label)
        .aria_expanded(open)
        .on_click(cx.listener(move |this, _, _, cx| {
            if !this.session_info_folded.remove(key) {
                this.session_info_folded.insert(key);
            }
            cx.notify();
        }))
        .child(popover::picker_icon_slot(glyph))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            row_meta(theme).children(meta).child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted)
                    .with_transformation(gpui::Transformation::rotate(gpui::radians(if open {
                        0.0
                    } else {
                        -std::f32::consts::FRAC_PI_2
                    }))),
            ),
        )
        .into_any_element();
        (row, open)
    }

    /// The Background work section, folded by default: the processes still
    /// running, each with its own stop control when the runtime offers one
    /// and its command line beneath. Finished processes drop out, and the
    /// section hides once nothing runs.
    fn render_session_info_background(
        &mut self,
        chat_id: &str,
        tasks: Vec<zuse_protocol::BackgroundTask>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let running = running_background(tasks);
        // A requested stop lasts until the task leaves the live set.
        self.background_stops
            .retain(|id| running.iter().any(|task| &task.id == id));
        if running.is_empty() {
            return Vec::new();
        }
        let now = Utc::now();
        let (header, open) = self.session_info_section(
            "background",
            false,
            row_glyph(icons::SUMMARY_BACKGROUND, theme),
            "Background work",
            Some(format!("{} running", running.len()).into()),
            theme,
            cx,
        );
        let mut rows = vec![header];
        if !open {
            return rows;
        }
        let elapsed = |task: &zuse_protocol::BackgroundTask| {
            SharedString::from(crate::transcript::format_elapsed(
                now.signed_duration_since(task.started_at).num_seconds(),
            ))
        };
        for task in &running {
            let stopping = self.background_stops.contains(&task.id);
            let control: AnyElement = if stopping {
                crate::loaders::retro_loader(
                    SharedString::from(format!("session-info-background-stopping-{}", task.id)),
                    12.0,
                    theme.loader_glyph(),
                )
            } else if task.stoppable {
                let (chat_id, id) = (chat_id.to_owned(), task.id.clone());
                let group = SharedString::from(format!("session-info-background-stop-{}", task.id));
                div()
                    .id(group.clone())
                    .group(group.clone())
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .tip("Stop")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stop_background_task(&chat_id, &id, cx);
                    }))
                    // A filled square: the stop mark, unmistakable at
                    // this size where an outlined one reads as a checkbox.
                    .child(
                        div()
                            .size(px(8.0))
                            .rounded(px(2.0))
                            .bg(theme.text_muted)
                            .group_hover(group, |style| style.bg(theme.text)),
                    )
                    .into_any_element()
            } else {
                agent_status_glyph(zuse_doc::SubagentStatus::Running, theme)
            };
            let label = background_label(task);
            let command = background_command(task);
            let tooltip = match &command {
                Some(command) => format!("{label}\n{command}"),
                None => label.to_string(),
            };
            rows.push(
                sub_row(theme)
                    .id(SharedString::from(format!(
                        "session-info-background-{}",
                        task.id
                    )))
                    .min_h(px(SUMMARY_ROW_HEIGHT))
                    .text_color(theme.text)
                    .gap(px(8.0))
                    .tip(tooltip)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .py(px(if command.is_some() { 4.0 } else { 0.0 }))
                            .child(div().truncate().child(label))
                            .children(command.map(|command| {
                                div()
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted)
                                    .child(command)
                            })),
                    )
                    .child(row_meta(theme).child(elapsed(task)).child(control))
                    .into_any_element(),
            );
        }
        let stoppable: Vec<String> = running
            .iter()
            .filter(|task| task.stoppable && !self.background_stops.contains(&task.id))
            .map(|task| task.id.clone())
            .collect();
        if stoppable.len() > 1 {
            let chat_id = chat_id.to_owned();
            rows.push(
                sub_row(theme)
                    .min_h(px(22.0))
                    .justify_end()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(
                        div()
                            .id("session-info-background-stop-all")
                            .flex_none()
                            .cursor_pointer()
                            .hover(|style| style.text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                for id in &stoppable {
                                    this.stop_background_task(&chat_id, id, cx);
                                }
                            }))
                            .child(tr(cx, msg::chat::background_stop_all)),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    /// The running tasks a stop request would reach right now: the runtime
    /// can stop them and no stop is already on its way. Shared by the
    /// section's Stop all and the stop-background-tasks shortcut.
    pub(super) fn stoppable_background_tasks(
        &self,
        tasks: &[zuse_protocol::BackgroundTask],
    ) -> Vec<String> {
        tasks
            .iter()
            .filter(|task| {
                task.running() && task.stoppable && !self.background_stops.contains(&task.id)
            })
            .map(|task| task.id.clone())
            .collect()
    }

    /// Ask the chat's host to stop one background task: a session command,
    /// so it reaches the device the chat runs on wherever this one is.
    pub(super) fn stop_background_task(
        &mut self,
        chat_id: &str,
        task_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if !self.background_stops.insert(task_id.to_owned()) {
            return;
        }
        cx.notify();
        let params = serde_json::json!({
            "chatId": chat_id,
            "command": { "kind": "stopBackgroundTask", "taskId": task_id },
        });
        let task_id = task_id.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(error) = engine.client().call(methods::QUEUE_COMMAND, params).await {
                tracing::warn!(%error, "background task stop failed");
                this.update(cx, |this, cx| {
                    this.background_stops.remove(&task_id);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// The Agents section: the chat's subagents, running first, each opening
    /// its transcript beside the chat. Settled ones past the first few fold
    /// behind "Show more".
    fn render_session_info_agents(
        &mut self,
        chat: &zuse_protocol::Chat,
        agents: Vec<SummaryAgent>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        /// Settled agents listed before the rest fold away.
        const SETTLED_PREVIEW: usize = 3;
        if agents.is_empty() {
            return Vec::new();
        }
        use zuse_doc::SubagentStatus;
        let (running, settled): (Vec<_>, Vec<_>) = agents
            .into_iter()
            .partition(|agent| agent.status == SubagentStatus::Running);
        let meta = if running.is_empty() {
            (running.len() + settled.len()).to_string()
        } else {
            format!("{} running", running.len())
        };
        let (header, open) = self.session_info_section(
            "agents",
            true,
            row_glyph(icons::TOOL_AGENT, theme),
            "Agents",
            Some(meta.into()),
            theme,
            cx,
        );
        let mut rows = vec![header];
        if !open {
            return rows;
        }
        // Native subagents run inside the chat's own provider.
        let brand = chat
            .config
            .as_ref()
            .map(|config| crate::pickers::harness_brand_icon(config.harness));
        let show_all = self.session_info_folded.contains("agents-all");
        let hidden = if show_all {
            0
        } else {
            settled.len().saturating_sub(SETTLED_PREVIEW)
        };
        let visible = running
            .into_iter()
            .chain(settled.into_iter().take(if show_all {
                usize::MAX
            } else {
                SETTLED_PREVIEW
            }));
        for agent in visible {
            let chat_id = chat.id.clone();
            let doc_id = agent.doc_id.clone();
            let title = agent.title.clone();
            let settled = agent.status != SubagentStatus::Running;
            rows.push(
                action_row(
                    theme,
                    SharedString::from(format!("session-info-agent-{doc_id}")),
                )
                .when(settled, |row| row.text_color(theme.text_muted))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    this.add_subagent_surface(
                        chat_id.clone(),
                        doc_id.clone(),
                        title.to_string(),
                        settled,
                        cx,
                    );
                }))
                .child(popover::picker_icon_slot(match brand {
                    Some((path, tint)) => {
                        icons::brand_icon(path, 14.0, tint.unwrap_or(theme.text_muted))
                    }
                    None => row_glyph(icons::TOOL_AGENT, theme),
                }))
                .child(div().flex_1().min_w_0().truncate().child(agent.title))
                .child(row_meta(theme).child(agent_status_glyph(agent.status, theme)))
                .into_any_element(),
            );
        }
        if hidden > 0 {
            rows.push(
                sub_row(theme)
                    .id("session-info-agents-more")
                    .cursor_pointer()
                    .hover(|style| style.text_color(theme.text))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.session_info_folded.insert("agents-all");
                        cx.notify();
                    }))
                    .child(SharedString::from(format!("Show {hidden} more")))
                    .into_any_element(),
            );
        }
        rows
    }

    /// The Actions section: a header row with the action count, a row per
    /// project action (click runs it; the gear edits it), imports from
    /// zuse.json, and a quiet Add action under them.
    fn render_session_info_actions(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.ensure_project_actions(cx);
        let (Some(status), Some(snapshot), Some(key)) = (
            self.project_actions.active_status().cloned(),
            self.project_actions.visible_snapshot(),
            self.project_actions.active.clone(),
        ) else {
            return Vec::new();
        };
        let count = snapshot.actions.len();
        let (header, open) = self.session_info_section(
            "actions",
            false,
            row_glyph(icons::ACTION_PLAY, theme),
            "Actions",
            (count > 0).then(|| SharedString::from(count.to_string())),
            theme,
            cx,
        );
        let mut rows: Vec<AnyElement> = vec![header];
        if !open {
            return rows;
        }
        let note = |text: SharedString, color| {
            sub_row(theme)
                .py(px(4.0))
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        match &status {
            ProjectActionsStatus::Idle | ProjectActionsStatus::Loading => {
                rows.push(note("Loading actions…".into(), theme.text_muted));
                return rows;
            }
            ProjectActionsStatus::Unavailable { message, .. } => {
                rows.push(note(message.clone().into(), theme.text_muted));
                rows.push(
                    sub_row(theme)
                        .id("session-info-actions-retry")
                        .cursor_pointer()
                        .hover(|style| style.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_project_actions(cx)))
                        .child(tr(cx, msg::chat::attempts_retry))
                        .into_any_element(),
                );
            }
            _ => {}
        }
        let can_run = status.can_run();
        for action in snapshot.actions.clone() {
            let row_id = SharedString::from(format!("session-info-action-{}", action.id));
            let group = SharedString::from(format!("session-info-action-group-{}", action.id));
            let key = key.clone();
            let run = action.clone();
            let edit = action.clone();
            let label: SharedString = if action.run_on_worktree_create {
                format!("{} (setup)", action.name).into()
            } else {
                action.name.clone().into()
            };
            rows.push(
                action_row(theme, row_id)
                    .group(group.clone())
                    .when(!can_run, |row| row.opacity(0.5).cursor_default())
                    .when(can_run, |row| {
                        row.on_click(cx.listener(move |this, _, _, cx| {
                            this.close_session_info_overlay(cx);
                            this.run_project_action(&key, run.clone(), cx)
                        }))
                    })
                    .child(popover::picker_icon_slot(row_glyph(
                        action_icon(action.icon),
                        theme,
                    )))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "session-info-edit-action-{}",
                                edit.id
                            )))
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .invisible()
                            .group_hover(group, |style| style.visible())
                            .hover(|style| style.bg(crate::theme::ink(0.08)))
                            .tip("Edit action")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_session_info_overlay(cx);
                                this.open_project_action_editor(Some(edit.clone()), None, cx)
                            }))
                            .child(
                                icon(icons::SETTINGS_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .into_any_element(),
            );
        }
        for draft in snapshot.importable_actions.clone() {
            let row_id = SharedString::from(format!("session-info-import-{}", draft.name));
            let import = draft.clone();
            rows.push(
                action_row(theme, row_id)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_session_info_overlay(cx);
                        this.open_project_action_editor(None, Some(import.clone()), cx)
                    }))
                    .child(popover::picker_icon_slot(row_glyph(
                        action_icon(draft.icon),
                        theme,
                    )))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(draft.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_muted)
                            .child(tr(cx, msg::settings::settings_page_import)),
                    )
                    .into_any_element(),
            );
        }
        if let Some(issue) = snapshot.project_file_issue.clone() {
            rows.push(note(issue.into(), theme.text_muted));
        }
        rows.push(
            sub_row(theme)
                .id("session-info-add-action")
                .cursor_pointer()
                .hover(|style| style.text_color(theme.text))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_session_info_overlay(cx);
                    this.open_project_action_editor(None, None, cx)
                }))
                .child(tr(cx, msg::native::add_action))
                .into_any_element(),
        );
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(
        id: &str,
        description: &str,
        command: Option<&str>,
        ended: bool,
    ) -> zuse_protocol::BackgroundTask {
        zuse_protocol::BackgroundTask {
            id: id.into(),
            kind: zuse_protocol::BackgroundTaskKind::Shell,
            description: description.into(),
            command: command.map(Into::into),
            started_at: Utc::now(),
            ended_at: ended.then(Utc::now),
            outcome: ended.then_some(zuse_protocol::BackgroundTaskOutcome::Completed),
            stoppable: !ended,
        }
    }

    #[test]
    fn the_background_section_lists_running_processes_only() {
        let running = running_background(vec![
            task("done", "npm run build", None, true),
            task("dev", "Start the dev server", Some("pnpm dev"), false),
        ]);
        let ids: Vec<&str> = running.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["dev"]);
        assert!(running_background(vec![task("done", "build", None, true)]).is_empty());
    }

    #[test]
    fn a_task_shows_its_command_unless_it_repeats_the_label() {
        let dev = task("dev", "Start the dev server", Some(" pnpm dev "), false);
        assert_eq!(background_command(&dev).as_deref(), Some("pnpm dev"));
        let same = task("w", "cargo watch", Some("cargo watch"), false);
        assert_eq!(background_command(&same), None);
        assert_eq!(background_command(&task("x", "x", None, false)), None);
        assert_eq!(background_command(&task("x", "x", Some("  "), false)), None);
        // An unnamed task still has a label for its command to sit under.
        let unnamed = task("u", " ", Some("make serve"), false);
        assert_eq!(background_label(&unnamed).as_ref(), "Background process");
        assert_eq!(background_command(&unnamed).as_deref(), Some("make serve"));
    }
}
