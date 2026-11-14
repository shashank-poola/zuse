//! The session card's pull request row and its menu: view the pull request,
//! have the chat's agent repair it, merge it, put it in the chat as context,
//! change its status, or open it on GitHub. The row also offers the one next
//! step worth a click (resolve, fix, mark ready, merge).
use super::session_info::{action_row, row_meta, sub_row};
use super::*;
use gpui::{Bounds, Pixels, WeakEntity, canvas};

use crate::pull_request_detail::{
    PullRequestRepair, chat_context_item, failing_check_count, open_feedback_count, repair_message,
};
use zuse_protocol::change_request_assessment::check_status;
use zuse_protocol::{
    ChangeRequestDetail, ChangeRequestMergeMethod as Method, ChangeRequestSummary,
    ChangeRequestUpdate as Update,
};

/// The menu's nested groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrSubmenu {
    Repair,
    Merge,
    Status,
}

/// The card's pull request: its details (fetched when the card shows it),
/// the menu's open state, and an update in flight or GitHub's refusal.
#[derive(Default)]
pub(super) struct SummaryPullRequest {
    url: Option<String>,
    target: Option<String>,
    detail: Option<ChangeRequestDetail>,
    pub(super) menu_open: bool,
    submenu: Option<PrSubmenu>,
    /// Where the open submenu and the row painted last frame: presses there
    /// are not "outside" the menu.
    submenu_bounds: Option<Bounds<Pixels>>,
    row_bounds: Option<Bounds<Pixels>>,
    busy: bool,
    notice: Option<SharedString>,
    fetch: Option<Task<()>>,
    update: Option<Task<()>>,
}

/// The one next step worth a button on the row, most pressing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextStep {
    Resolve,
    Fix,
    Ready,
    Merge,
}

impl NextStep {
    fn label(self) -> &'static str {
        match self {
            Self::Resolve => "Resolve",
            Self::Fix => "Fix",
            Self::Ready => "Mark ready",
            Self::Merge => "Merge",
        }
    }
}

fn next_step(detail: &ChangeRequestDetail) -> Option<NextStep> {
    if detail.state != "OPEN" {
        return None;
    }
    if detail.mergeable == "CONFLICTING" {
        return Some(NextStep::Resolve);
    }
    if failing_check_count(detail) > 0 {
        return Some(NextStep::Fix);
    }
    if detail.is_draft {
        return Some(NextStep::Ready);
    }
    let pending = detail.status_check_rollup.iter().any(|check| {
        matches!(
            check_status(check).as_str(),
            "PENDING" | "IN_PROGRESS" | "QUEUED" | "EXPECTED" | "WAITING" | "REQUESTED"
        )
    });
    let approved = !matches!(
        detail.review_decision.as_str(),
        "CHANGES_REQUESTED" | "REVIEW_REQUIRED"
    );
    (detail.mergeable == "MERGEABLE" && approved && !pending && detail.auto_merge_request.is_none())
        .then_some(NextStep::Merge)
}

/// The status a pull request's Status submenu names.
fn status_label(detail: &ChangeRequestDetail) -> &'static str {
    if detail.state == "CLOSED" {
        "Closed"
    } else if detail.is_draft {
        "Draft"
    } else {
        "Ready for review"
    }
}

/// Paints nothing; reports the element's bounds each frame.
fn bounds_probe(
    record: impl Fn(&mut Shell, Bounds<Pixels>) + 'static,
    entity: WeakEntity<Shell>,
) -> impl IntoElement {
    canvas(
        move |bounds, _, cx| {
            let _ = entity.update(cx, |shell, _| record(shell, bounds));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

impl Shell {
    /// Track the card's pull request: a new one resets the menu and loads
    /// its details on the device the chat runs on.
    fn sync_summary_pull_request(
        &mut self,
        url: &str,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.summary_pr.url.as_deref() == Some(url) {
            return;
        }
        self.summary_pr = SummaryPullRequest {
            url: Some(url.to_owned()),
            target,
            ..Default::default()
        };
        self.fetch_summary_pull_request(false, cx);
    }

    fn fetch_summary_pull_request(&mut self, refresh: bool, cx: &mut Context<Self>) {
        let Some(url) = self.summary_pr.url.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = serde_json::json!({ "url": url, "refresh": refresh });
        if let Some(target) = &self.summary_pr.target {
            params["targetDeviceId"] = target.clone().into();
        }
        self.summary_pr.fetch = Some(cx.spawn(async move |this, cx| {
            let detail = engine
                .client()
                .call(methods::GET_CHANGE_REQUEST, params)
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<ChangeRequestDetail>(value).ok());
            let _ = this.update(cx, |this, cx| {
                if this.summary_pr.url.as_deref() == Some(url.as_str()) && detail.is_some() {
                    this.summary_pr.detail = detail;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn close_summary_pr_menu(&mut self, cx: &mut Context<Self>) {
        self.summary_pr.menu_open = false;
        self.summary_pr.submenu = None;
        self.summary_pr.submenu_bounds = None;
        cx.notify();
    }

    /// Stage `text` in the chat's composer, after any draft already there;
    /// the user reviews and sends it.
    fn stage_in_composer(&mut self, text: String, cx: &mut Context<Self>) {
        self.composer.update(cx, |composer, cx| {
            composer.input.update(cx, |input, cx| {
                let draft = input.text().trim_end().to_owned();
                let text = if draft.is_empty() {
                    text
                } else {
                    format!("{draft}\n\n{text}")
                };
                input.set_text(text, cx)
            });
            composer.focus_pending = true;
            cx.notify();
        });
    }

    fn repair_summary_pull_request(&mut self, repair: PullRequestRepair, cx: &mut Context<Self>) {
        let (Some(detail), Some(url)) = (&self.summary_pr.detail, &self.summary_pr.url) else {
            return;
        };
        let (text, context) = repair_message(repair, detail, url);
        self.close_summary_pr_menu(cx);
        self.close_session_info_overlay(cx);
        self.composer
            .update(cx, |composer, cx| composer.stage_context(context, cx));
        self.stage_in_composer(text, cx);
    }

    /// Merge, toggle auto-merge or change the status on GitHub, through the
    /// chat's device. GitHub's refusal shows under the row.
    fn update_summary_pull_request(&mut self, update: Update, cx: &mut Context<Self>) {
        let Some(url) = self.summary_pr.url.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mut params = serde_json::json!({ "url": url, "update": update });
        if let Some(target) = &self.summary_pr.target {
            params["targetDeviceId"] = target.clone().into();
        }
        self.close_summary_pr_menu(cx);
        self.summary_pr.busy = true;
        self.summary_pr.notice = None;
        self.summary_pr.update = Some(cx.spawn(async move |this, cx| {
            let notice = match engine
                .client()
                .call(methods::UPDATE_CHANGE_REQUEST, params)
                .await
            {
                Ok(value) => value["refusal"].as_str().map(str::to_owned),
                Err(error) => Some(format!(
                    "Couldn’t update the pull request. {}",
                    crate::pull_request_detail::failure_reason(&error)
                )),
            };
            let _ = this.update(cx, |this, cx| {
                if this.summary_pr.url.as_deref() != Some(url.as_str()) {
                    return;
                }
                this.summary_pr.busy = false;
                this.summary_pr.notice = notice.map(SharedString::from);
                this.fetch_summary_pull_request(true, cx);
                cx.notify();
            });
        }));
    }

    /// The card's pull request row: its state glyph and title, the next
    /// step's button, and a chevron opening the menu beside the card.
    pub(super) fn render_summary_pull_request(
        &mut self,
        summary: ChangeRequestSummary,
        target: Option<String>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.sync_summary_pull_request(&summary.url, target, cx);
        let model = crate::change_requests::ChangeRequestBadgeModel::from_summary(&summary);
        let open = self.summary_pr.menu_open;
        let step = self.summary_pr.detail.as_ref().and_then(next_step);
        let entity = cx.entity().downgrade();
        let mut row = action_row(theme, "session-info-pull-request")
            .debug_selector(|| "session-info-pull-request".into())
            .relative()
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "Pull request {}: {}",
                model.number, model.title
            )))
            .aria_expanded(open)
            .when(open, |row| row.bg(theme.ink(0.06)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_branch_menu(cx);
                this.summary_pr.menu_open = !open;
                this.summary_pr.submenu = None;
                if !open {
                    // Opening shows the freshest state GitHub has.
                    this.fetch_summary_pull_request(true, cx);
                }
                cx.notify();
            }))
            .child(bounds_probe(
                |shell, bounds| shell.summary_pr.row_bounds = Some(bounds),
                entity,
            ))
            .child(popover::picker_icon_slot(
                icon(model.tone.glyph())
                    .size(px(16.0))
                    .text_color(model.tone.color(theme))
                    .into_any_element(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(model.title.clone()),
            )
            .child(
                row_meta(theme)
                    .when_some(step.filter(|_| !self.summary_pr.busy), |meta, step| {
                        meta.child(self.render_next_step(step, theme, cx))
                    })
                    .child(SharedString::from(format!("#{}", model.number)))
                    .child(if self.summary_pr.busy {
                        crate::loaders::retro_loader(
                            "session-info-pull-request-busy",
                            12.0,
                            theme.loader_glyph(),
                        )
                    } else {
                        icon(icons::ALT_ARROW_DOWN)
                            .size(px(12.0))
                            .text_color(theme.text_muted)
                            .into_any_element()
                    }),
            );
        if open {
            let menu = self.render_summary_pr_menu(&summary, theme, cx);
            row = row.child(popover::nested_menu(
                "session-info-pull-request-menu",
                menu,
                true,
            ));
        }
        let mut rows = vec![row.into_any_element()];
        if let Some(notice) = self.summary_pr.notice.clone() {
            rows.push(
                sub_row(theme)
                    .py(px(4.0))
                    .text_size(px(11.0))
                    .text_color(theme.danger)
                    .child(notice)
                    .into_any_element(),
            );
        }
        rows
    }

    /// The row's next-step button, toned like the step: resolving and fixing
    /// read as problems, merging as go.
    fn render_next_step(
        &mut self,
        step: NextStep,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tone = match step {
            NextStep::Resolve | NextStep::Fix => theme.danger,
            NextStep::Ready => theme.text,
            NextStep::Merge => theme.success,
        };
        div()
            .id("session-info-pull-request-next")
            .flex_none()
            .h(px(20.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .text_size(px(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .cursor_pointer()
            .hover(move |style| style.bg(tone.opacity(0.12)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                match step {
                    NextStep::Resolve => {
                        this.repair_summary_pull_request(PullRequestRepair::Conflicts, cx)
                    }
                    NextStep::Fix => {
                        this.repair_summary_pull_request(PullRequestRepair::Checks, cx)
                    }
                    NextStep::Ready => this.update_summary_pull_request(Update::MarkReady, cx),
                    // Merging asks how: open the menu at its Merge group.
                    NextStep::Merge => {
                        this.summary_pr.menu_open = true;
                        this.summary_pr.submenu = Some(PrSubmenu::Merge);
                        cx.notify();
                    }
                }
            }))
            .child(step.label())
            .into_any_element()
    }

    fn render_summary_pr_menu(
        &mut self,
        summary: &ChangeRequestSummary,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = self.summary_pr.detail.clone();
        let loaded = detail.is_some();
        let state = detail
            .as_ref()
            .map_or_else(|| summary_state(summary), |detail| detail.state.clone());
        let open = state == "OPEN";
        let draft = detail.as_ref().is_some_and(|detail| detail.is_draft);
        let problems = detail.as_ref().map_or(0, |detail| {
            open_feedback_count(detail)
                + failing_check_count(detail)
                + usize::from(detail.mergeable == "CONFLICTING")
        });
        let url = summary.url.clone();
        let device = self.summary_pr.target.clone();
        let github_url = summary.url.clone();
        let mut menu = popover::popover_card(theme)
            .w(px(248.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                // The submenu and the row are part of the menu's surface.
                let inside = [this.summary_pr.submenu_bounds, this.summary_pr.row_bounds]
                    .into_iter()
                    .flatten()
                    .any(|bounds| bounds.contains(&event.position));
                if !inside {
                    this.close_summary_pr_menu(cx);
                }
            }))
            .child(
                self.pr_menu_item(
                    theme,
                    "pr-menu-view",
                    icons::PR_VIEW,
                    "View pull request",
                    true,
                    None,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.close_summary_pr_menu(cx);
                    this.close_session_info_overlay(cx);
                    crate::pull_request_detail::open_on_device(&url, device.clone(), window, cx);
                })),
            )
            .child(self.pr_submenu_trigger(
                theme,
                PrSubmenu::Repair,
                icons::PR_REPAIR,
                "Repair",
                loaded,
                (problems > 0).then(|| {
                    div()
                        .text_color(theme.danger)
                        .child(SharedString::from(problems.to_string()))
                        .into_any_element()
                }),
                cx,
            ));
        if open && !draft {
            menu = menu.child(self.pr_submenu_trigger(
                theme,
                PrSubmenu::Merge,
                icons::GIT_MERGE,
                "Merge",
                loaded,
                None,
                cx,
            ));
        }
        menu = menu.child(
            self.pr_menu_item(
                theme,
                "pr-menu-add-to-chat",
                icons::PR_ADD_TO_CHAT,
                "Add to chat",
                loaded,
                None,
                cx,
            )
            .when(loaded, |row| {
                row.on_click(cx.listener(|this, _, _, cx| {
                    let (Some(detail), Some(url)) = (&this.summary_pr.detail, &this.summary_pr.url)
                    else {
                        return;
                    };
                    let context = chat_context_item(detail, url);
                    this.close_summary_pr_menu(cx);
                    this.close_session_info_overlay(cx);
                    this.composer
                        .update(cx, |composer, cx| composer.stage_context(context, cx));
                }))
            }),
        );
        if open {
            let auto_fix = self
                .composer
                .update(cx, |composer, cx| composer.pr_auto_fix_button(cx));
            menu = menu.children(auto_fix.map(|control| {
                div()
                    .min_h(px(32.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .child(control)
            }));
        }
        menu = menu.child(popover::menu_separator());
        if state != "MERGED" {
            let label = detail.as_ref().map_or("", status_label);
            menu = menu.child(self.pr_submenu_trigger(
                theme,
                PrSubmenu::Status,
                icons::PULL_REQUEST_DRAFT,
                "Status",
                loaded,
                (!label.is_empty()).then(|| {
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(label)
                        .into_any_element()
                }),
                cx,
            ));
        }
        menu.child(
            self.pr_menu_item(
                theme,
                "pr-menu-github",
                icons::PR_GITHUB,
                "Open on GitHub",
                true,
                None,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_summary_pr_menu(cx);
                cx.open_url(&github_url);
            })),
        )
        .into_any_element()
    }

    /// One menu row; hovering it closes another row's submenu.
    #[allow(clippy::too_many_arguments)]
    fn pr_menu_item(
        &mut self,
        theme: &Theme,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        enabled: bool,
        meta: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        popover::menu_row(theme, false, id)
            .id(id)
            .debug_selector(move || id.to_owned())
            .min_h(px(32.0))
            .py(px(4.0))
            .line_height(px(20.0))
            .text_size(crate::typography::text_sm())
            .when(!enabled, |row| row.opacity(0.4).cursor_default())
            // `menu_row` owns the hover listener (its fade); pointer
            // movement over the row is the same signal.
            .on_mouse_move(cx.listener(|this, _: &gpui::MouseMoveEvent, _, cx| {
                if this.summary_pr.submenu.is_some() {
                    this.summary_pr.submenu = None;
                    this.summary_pr.submenu_bounds = None;
                    cx.notify();
                }
            }))
            .child(icon(glyph).size(px(15.0)).text_color(theme.text_muted))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .children(meta)
    }

    /// A row opening one of the menu's groups beside it, on hover or click.
    #[allow(clippy::too_many_arguments)]
    fn pr_submenu_trigger(
        &mut self,
        theme: &Theme,
        kind: PrSubmenu,
        glyph: &'static str,
        label: &'static str,
        enabled: bool,
        meta: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id: &'static str = match kind {
            PrSubmenu::Repair => "pr-menu-repair",
            PrSubmenu::Merge => "pr-menu-merge",
            PrSubmenu::Status => "pr-menu-status",
        };
        let open = enabled && self.summary_pr.submenu == Some(kind);
        let mut row = popover::menu_row(theme, open, id)
            .id(id)
            .debug_selector(move || id.to_owned())
            .relative()
            .min_h(px(32.0))
            .py(px(4.0))
            .line_height(px(20.0))
            .text_size(crate::typography::text_sm())
            .when(!enabled, |row| row.opacity(0.4).cursor_default())
            .child(icon(glyph).size(px(15.0)).text_color(theme.text_muted))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .children(meta)
            .child(
                icon(icons::ALT_ARROW_RIGHT)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        if enabled {
            row = row
                .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _, cx| {
                    if this.summary_pr.submenu != Some(kind) {
                        this.summary_pr.submenu = Some(kind);
                        this.summary_pr.submenu_bounds = None;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.summary_pr.submenu = Some(kind);
                    cx.notify();
                }));
        }
        if open {
            let submenu = self.render_pr_submenu(kind, theme, cx);
            row = row.child(popover::nested_menu(
                SharedString::from(format!("{id}-submenu")),
                submenu,
                true,
            ));
        }
        row.into_any_element()
    }

    fn render_pr_submenu(
        &mut self,
        kind: PrSubmenu,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(detail) = self.summary_pr.detail.clone() else {
            return div().into_any_element();
        };
        let entity = cx.entity().downgrade();
        let item = |id: &'static str,
                    glyph: &'static str,
                    label: &'static str,
                    enabled: bool,
                    meta: Option<SharedString>| {
            popover::menu_row(theme, false, id)
                .id(id)
                .debug_selector(move || id.to_owned())
                .min_h(px(32.0))
                .py(px(4.0))
                .line_height(px(20.0))
                .text_size(crate::typography::text_sm())
                .when(!enabled, |row| row.opacity(0.4).cursor_default())
                .child(icon(glyph).size(px(15.0)).text_color(theme.text_muted))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .children(meta.map(|meta| {
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted)
                        .child(meta)
                }))
        };
        let repair = |this: &mut Self, repair: PullRequestRepair, cx: &mut Context<Self>| {
            this.repair_summary_pull_request(repair, cx)
        };
        let update = |this: &mut Self, update: Update, cx: &mut Context<Self>| {
            this.update_summary_pull_request(update, cx)
        };
        let count = |n: usize| (n > 0).then(|| SharedString::from(n.to_string()));
        let open = detail.state == "OPEN";
        let mut card = popover::popover_card(theme)
            .w(px(236.0))
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(bounds_probe(
                |shell, bounds| shell.summary_pr.submenu_bounds = Some(bounds),
                entity,
            ));
        match kind {
            PrSubmenu::Repair => {
                let feedback = open_feedback_count(&detail);
                let checks = failing_check_count(&detail);
                let conflicts = detail.mergeable == "CONFLICTING";
                let rows = [
                    (
                        "pr-repair-feedback",
                        icons::PR_COMMENTS,
                        "Address feedback",
                        feedback > 0,
                        count(feedback),
                        PullRequestRepair::Feedback,
                    ),
                    (
                        "pr-repair-checks",
                        icons::PR_CHECKS,
                        "Fix failing checks",
                        checks > 0,
                        count(checks),
                        PullRequestRepair::Checks,
                    ),
                    (
                        "pr-repair-conflicts",
                        icons::PR_CONFLICTS,
                        "Resolve conflicts",
                        conflicts,
                        None,
                        PullRequestRepair::Conflicts,
                    ),
                    (
                        "pr-repair-review",
                        icons::EYE,
                        "Review the changes",
                        true,
                        None,
                        PullRequestRepair::Review,
                    ),
                ];
                for (id, glyph, label, enabled, meta, kind) in rows {
                    card = card.child(item(id, glyph, label, enabled, meta).when(enabled, |row| {
                        row.on_click(cx.listener(move |this, _, _, cx| repair(this, kind, cx)))
                    }));
                }
            }
            PrSubmenu::Merge => {
                let mergeable = detail.mergeable != "CONFLICTING";
                for (id, label, method) in [
                    ("pr-merge-squash", "Squash and merge", Method::Squash),
                    ("pr-merge-commit", "Create a merge commit", Method::Merge),
                    ("pr-merge-rebase", "Rebase and merge", Method::Rebase),
                ] {
                    card = card.child(item(id, icons::GIT_MERGE, label, mergeable, None).when(
                        mergeable,
                        |row| {
                            row.on_click(cx.listener(move |this, _, _, cx| {
                                update(this, Update::Merge { method }, cx)
                            }))
                        },
                    ));
                }
                card = card.child(popover::menu_separator());
                card =
                    card.child(if detail.auto_merge_request.is_some() {
                        item(
                            "pr-merge-auto-off",
                            icons::CLOSE,
                            "Turn off auto-merge",
                            true,
                            None,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            update(this, Update::DisableAutoMerge, cx)
                        }))
                    } else {
                        item(
                            "pr-merge-auto",
                            icons::GIT_MERGE,
                            "Merge when ready",
                            true,
                            Some("Squash".into()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            update(
                                this,
                                Update::EnableAutoMerge {
                                    method: Method::Squash,
                                },
                                cx,
                            )
                        }))
                    });
            }
            PrSubmenu::Status => {
                let to_draft = open && !detail.is_draft;
                let to_ready = open && detail.is_draft;
                card = card
                    .child(
                        item(
                            "pr-status-draft",
                            icons::PULL_REQUEST_DRAFT,
                            "Convert to draft",
                            to_draft,
                            None,
                        )
                        .when(to_draft, |row| {
                            row.on_click(cx.listener(move |this, _, _, cx| {
                                update(this, Update::MarkDraft, cx)
                            }))
                        }),
                    )
                    .child(
                        item(
                            "pr-status-ready",
                            icons::PULL_REQUEST,
                            "Ready for review",
                            to_ready,
                            None,
                        )
                        .when(to_ready, |row| {
                            row.on_click(cx.listener(move |this, _, _, cx| {
                                update(this, Update::MarkReady, cx)
                            }))
                        }),
                    )
                    .child(if detail.state == "CLOSED" {
                        item(
                            "pr-status-reopen",
                            icons::PULL_REQUEST,
                            "Reopen",
                            true,
                            None,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| update(this, Update::Reopen, cx)),
                        )
                    } else {
                        item(
                            "pr-status-close",
                            icons::PULL_REQUEST_CLOSED,
                            "Close",
                            true,
                            None,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| update(this, Update::Close, cx)),
                        )
                    });
            }
        }
        card.into_any_element()
    }
}

/// A summary's state in the detail's spelling, until the detail arrives.
fn summary_state(summary: &ChangeRequestSummary) -> String {
    match summary.state {
        zuse_protocol::ChangeRequestState::Open => "OPEN",
        zuse_protocol::ChangeRequestState::Merged => "MERGED",
        zuse_protocol::ChangeRequestState::Closed => "CLOSED",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(state: &str, mergeable: &str) -> ChangeRequestDetail {
        ChangeRequestDetail {
            state: state.into(),
            mergeable: mergeable.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_row_offers_the_most_pressing_step() {
        assert_eq!(next_step(&detail("MERGED", "UNKNOWN")), None);
        assert_eq!(
            next_step(&detail("OPEN", "CONFLICTING")),
            Some(NextStep::Resolve)
        );
        let mut failing = detail("OPEN", "MERGEABLE");
        failing.status_check_rollup = vec![zuse_protocol::ChangeRequestCheck {
            name: "build".into(),
            conclusion: "FAILURE".into(),
            ..Default::default()
        }];
        assert_eq!(next_step(&failing), Some(NextStep::Fix));
        let mut draft = detail("OPEN", "MERGEABLE");
        draft.is_draft = true;
        assert_eq!(next_step(&draft), Some(NextStep::Ready));
        assert_eq!(
            next_step(&detail("OPEN", "MERGEABLE")),
            Some(NextStep::Merge)
        );
        // Waiting on reviewers, running checks, or armed auto-merge: no
        // button; the menu still offers everything.
        let mut blocked = detail("OPEN", "MERGEABLE");
        blocked.review_decision = "REVIEW_REQUIRED".into();
        assert_eq!(next_step(&blocked), None);
        let mut running = detail("OPEN", "MERGEABLE");
        running.status_check_rollup = vec![zuse_protocol::ChangeRequestCheck {
            name: "build".into(),
            status: "IN_PROGRESS".into(),
            ..Default::default()
        }];
        assert_eq!(next_step(&running), None);
        let mut armed = detail("OPEN", "MERGEABLE");
        armed.auto_merge_request = Some(Default::default());
        assert_eq!(next_step(&armed), None);
    }
}

#[cfg(feature = "browser-fixture")]
impl Shell {
    /// Review fixtures: the card's pull request with details (a failing
    /// check, open feedback), its menu open at `submenu` when asked.
    pub(super) fn fixture_seed_summary_pull_request(
        &mut self,
        url: &str,
        menu: bool,
        submenu: Option<PrSubmenu>,
    ) {
        let detail = ChangeRequestDetail {
            title: "Polish the command palette".into(),
            url: url.into(),
            number: 413,
            state: "OPEN".into(),
            mergeable: "MERGEABLE".into(),
            review_decision: "APPROVED".into(),
            base_ref_name: "main".into(),
            head_ref_name: "fieldnotes/polish-the-command-palette".into(),
            status_check_rollup: vec![zuse_protocol::ChangeRequestCheck {
                name: "test (ubuntu)".into(),
                conclusion: "FAILURE".into(),
                ..Default::default()
            }],
            review_threads: Some(vec![zuse_protocol::ChangeRequestReviewThread {
                path: "src/palette.rs".into(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        self.summary_pr = SummaryPullRequest {
            url: Some(url.to_owned()),
            detail: Some(detail),
            menu_open: menu,
            submenu,
            ..Default::default()
        };
    }
}
