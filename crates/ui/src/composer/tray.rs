//! The composer's attached tray: one surface tucked behind the card's top
//! edge that stacks everything the next send depends on, in a fixed order —
//! no connection, staged annotations and attachments, provider sign-in, the
//! provider or send error, the agent's plan, and the queue. Each notice is a
//! [`TrayPill`] row (`min-h 32`, 12px text, a 16px icon slot); rows are
//! separated by hairlines, and the surface is the context tray's tonal tab
//! ([`super::context_tray_fill`], [`super::tray_inset`]) so the new-chat
//! selectors and these trays read as one object.
//!
//! The pure half (which error the tray shows, how it reads) is unit-tested
//! below; the rest renders on [`Composer`].

use crate::localization::message as tr;
use crate::tooltip::TooltipExt as _;
use std::collections::HashMap;
use std::time::Duration;
use zuse_i18n::messages as msg;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FontWeight, Hsla, ObjectFit, SharedString,
    Subscription, Task, Window, div, img, prelude::*, px, relative,
};
use zuse_doc::{MessagePart, MessageRole, SessionMessageEntry};
use zuse_protocol::{AgentLoginMode, AgentLoginPoll, AgentLoginStart, AgentLoginStatus, HarnessId};
use zuse_rpc::methods;

use super::{
    Composer, ComposerEvent, ComposerInput, ComposerInputEvent, WorkspaceCommand,
    composer_has_content,
};
use crate::icons::{self, icon};
use crate::theme::Theme;

/// `rounded-t-[1.2rem]`, the card's own radius.
pub(crate) const TRAY_RADIUS: f32 = super::COMPOSER_RADIUS;
const ROW_MIN_HEIGHT: f32 = 32.0;
const ROW_PAD_X: f32 = 12.0;
const TEXT_SIZE: f32 = 12.0;
const LINE_HEIGHT: f32 = 16.0;
const ICON_SLOT: f32 = 16.0;
const GLYPH: f32 = 14.0;
pub(crate) const ICON_ACTION: f32 = 24.0;
const TEXT_ACTION_HEIGHT: f32 = 28.0;
const THUMB: f32 = 40.0;
const THUMB_RADIUS: f32 = 7.0;

fn is_configuration_notice(
    message: &str,
    key: Option<&str>,
    notice: Option<&(String, String)>,
) -> bool {
    notice.is_some_and(|(chat, text)| key == Some(chat.as_str()) && message == text)
}
/// `ContextPill` hover intent: open after a short dwell, close after a grace
/// that carries the pointer across the gap to the preview card.
const PREVIEW_OPEN_DELAY: Duration = Duration::from_millis(200);
const PREVIEW_CLOSE_GRACE: Duration = Duration::from_millis(140);
const LOGIN_POLL: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// Pure: which provider failure the tray shows, and how it reads
// ---------------------------------------------------------------------------

/// What kind of provider failure stopped the chat; picks the row's tone,
/// icon and recovery actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderErrorKind {
    UsageLimit,
    Reconnecting,
    SignIn,
    Generic,
}

/// Harnesses only report failures as text, so classify by the phrases they
/// use for limits, reconnects and lost credentials.
pub(crate) fn classify_provider_error(message: &str) -> ProviderErrorKind {
    let text = message.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    if any(&[
        "usage limit",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "quota",
        "limit reached",
        "hit your limit",
        "too many requests",
        " 429",
        "429 ",
    ]) {
        ProviderErrorKind::UsageLimit
    } else if any(&[
        "not logged in",
        "not signed in",
        "please run /login",
        "please log in",
        "please sign in",
        "signed out",
        "log in again",
        "sign in again",
        "unauthorized",
        "unauthenticated",
        "authentication",
        "invalid api key",
        "invalid_api_key",
        "oauth token",
        " 401",
    ]) {
        ProviderErrorKind::SignIn
    } else if any(&["reconnecting", "retrying"]) {
        ProviderErrorKind::Reconnecting
    } else {
        ProviderErrorKind::Generic
    }
}

/// The error that ended the chat's last turn: the last error part of the
/// final entry, when that entry is the agent's (a later prompt supersedes
/// it). Returns a stable id for dismissal and the full message.
pub(crate) fn latest_transcript_error(entries: &[SessionMessageEntry]) -> Option<(String, String)> {
    let last = entries.last()?;
    if last.role != MessageRole::Assistant {
        return None;
    }
    last.parts.iter().rev().find_map(|part| match part {
        MessagePart::Error { id, message } if !message.trim().is_empty() => {
            Some((format!("{}#{id}", last.id), message.trim().to_string()))
        }
        _ => None,
    })
}

/// The prompt a Retry sends again: the text of the last user entry.
pub(crate) fn last_prompt_text(entries: &[SessionMessageEntry]) -> Option<String> {
    let entry = entries
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::User)?;
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn first_line(text: &str) -> SharedString {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
        .into()
}

fn provider_error_copy(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::UsageLimit => "Usage limit reached",
        ProviderErrorKind::Reconnecting => "Reconnecting",
        ProviderErrorKind::SignIn => "Sign-in expired",
        ProviderErrorKind::Generic => "Run failed",
    }
}

// ---------------------------------------------------------------------------
// TrayPill: the shared row
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayTone {
    Default,
    Warning,
    Accent,
    Danger,
}

impl TrayTone {
    fn icon(self, theme: &Theme) -> Hsla {
        match self {
            Self::Default => theme.text_muted,
            Self::Warning => theme.warning,
            Self::Accent => theme.accent,
            Self::Danger => theme.danger,
        }
    }

    fn title(self, theme: &Theme) -> Hsla {
        match self {
            Self::Warning => theme.warning_muted,
            _ => theme.text,
        }
    }
}

enum PillIcon {
    Glyph(&'static str),
    Element(AnyElement),
}

type PillClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// One tray row (`TrayPill flush`): icon, medium title and a muted, truncated
/// subtitle, trailing actions, and an optional section expanded below a
/// hairline.
pub(crate) struct TrayPill {
    id: SharedString,
    icon: Option<PillIcon>,
    tone: TrayTone,
    title: SharedString,
    subtitle: Option<SharedString>,
    actions: Vec<AnyElement>,
    expanded: Option<AnyElement>,
    on_click: Option<(PillClick, SharedString)>,
    fill: Option<(Hsla, Hsla)>,
}

impl TrayPill {
    pub(crate) fn new(id: impl Into<SharedString>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon: None,
            tone: TrayTone::Default,
            title: title.into(),
            subtitle: None,
            actions: Vec::new(),
            expanded: None,
            on_click: None,
            fill: None,
        }
    }

    pub(crate) fn glyph(mut self, path: &'static str) -> Self {
        self.icon = Some(PillIcon::Glyph(path));
        self
    }

    pub(crate) fn icon_element(mut self, element: impl IntoElement) -> Self {
        self.icon = Some(PillIcon::Element(element.into_any_element()));
        self
    }

    pub(crate) fn tone(mut self, tone: TrayTone) -> Self {
        self.tone = tone;
        self
    }

    pub(crate) fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub(crate) fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }

    pub(crate) fn expanded(mut self, section: Option<AnyElement>) -> Self {
        self.expanded = section;
        self
    }

    /// The icon and titles become one button (expand a message, open a plan).
    pub(crate) fn on_click(
        mut self,
        aria: impl Into<SharedString>,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some((Box::new(handler), aria.into()));
        self
    }

    /// A tinted row (the plan's `bg-primary/10 hover:bg-primary/15`).
    pub(crate) fn fill(mut self, rest: Hsla, hover: Hsla) -> Self {
        self.fill = Some((rest, hover));
        self
    }

    pub(crate) fn render(self, theme: &Theme, first: bool) -> AnyElement {
        let tone = self.tone;
        let icon = self.icon.map(|glyph| {
            div()
                .size(px(ICON_SLOT))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(match glyph {
                    PillIcon::Glyph(path) => icon(path)
                        .size(px(GLYPH))
                        .text_color(tone.icon(theme))
                        .into_any_element(),
                    PillIcon::Element(element) => element,
                })
        });
        let titles = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .max_w(relative(0.7))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tone.title(theme))
                    .child(self.title),
            )
            .children(self.subtitle.map(|subtitle| {
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(subtitle)
            }));
        let body = div()
            .id(SharedString::from(format!("{}-body", self.id)))
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .children(icon)
            .child(titles);
        let body = match self.on_click {
            Some((handler, aria)) => body
                .cursor_pointer()
                .role(gpui::Role::Button)
                .aria_label(aria)
                .on_click(handler),
            None => body,
        };
        let row = div()
            .min_h(px(ROW_MIN_HEIGHT))
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(ROW_PAD_X))
            .py(px(4.0))
            .child(body)
            .when(!self.actions.is_empty(), |row| {
                row.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .children(self.actions),
                )
            });
        div()
            .id(self.id)
            .w_full()
            .min_w_0()
            .text_size(px(TEXT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(theme.text)
            // GPUI clips rectangularly: a tinted first row keeps the tray's
            // rounded corners itself.
            .when(first, |el| el.rounded_t(px(TRAY_RADIUS - 1.0)))
            .when_some(self.fill, |el, (rest, hover)| {
                el.bg(rest).hover(move |s| s.bg(hover))
            })
            .child(row)
            .when_some(self.expanded, |el, section| {
                el.child(
                    div()
                        .border_t_1()
                        .border_color(theme.border.opacity(0.4))
                        .child(section),
                )
            })
            .into_any_element()
    }
}

/// `trayPillActionClass`: a 24px muted icon button.
pub(crate) fn tray_icon_action(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    label: impl Into<SharedString>,
    size: f32,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    let hover = theme.element_hover;
    div()
        .id(id.into())
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .hover(move |s| s.bg(hover))
        .tip(label)
        .child(icon(glyph).size(px(GLYPH)).text_color(theme.text_muted))
}

/// `trayPillTextActionClass`: an `h-7 px-2` medium label (Sign in, Retry).
pub(crate) fn tray_text_action(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    glyph: Option<&'static str>,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let hover = theme.element_hover;
    div()
        .id(id.into())
        .h(px(TEXT_ACTION_HEIGHT))
        .flex_none()
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .role(gpui::Role::Button)
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_strong)
        .hover(move |s| s.bg(hover))
        .children(glyph.map(|glyph| icon(glyph).size(px(GLYPH)).text_color(theme.text_muted)))
        .child(label.into())
}

/// The tray surface: the context tray's tonal fill and a faint hairline
/// with rounded top corners. The card overlaps its lower edge, so it pads
/// that strip clear. While a drag is in flight it stops occluding so drops
/// reach the conversation's drop zone beneath.
pub(crate) fn tray_surface(theme: &Theme, dragging: bool) -> gpui::Div {
    div()
        .when(!dragging, |el| el.occlude())
        .rounded_t(px(TRAY_RADIUS))
        .bg(super::context_tray_fill(theme))
        .border_1()
        .border_color(theme.hairline(0.06))
        .pb(px(super::QUEUE_COMPOSER_OVERLAP))
        .flex()
        .flex_col()
}

/// A row in the stack, before hairlines and corner rounding are applied.
pub(crate) enum TrayRow {
    Pill(TrayPill),
    Element(AnyElement),
}

/// The in-card drop target (`inset-1 rounded-lg border-dashed bg-popover`):
/// shown only while a file or workspace path is dragged over the card.
pub(crate) fn drop_overlay(theme: &Theme, cx: &gpui::App) -> gpui::Stateful<gpui::Div> {
    let theme = theme.for_popup();
    div()
        .id("composer-drop-overlay")
        .absolute()
        .top(px(-30.0))
        .left_0()
        .right_0()
        .bottom_0()
        .opacity(0.0)
        .drag_over::<gpui::ExternalPaths>(|style, _, _, _| style.opacity(1.0))
        .drag_over::<crate::files::WorkspacePathDrag>(|style, _, _, _| style.opacity(1.0))
        .child(
            div()
                .absolute()
                .top(px(32.0))
                .left(px(2.0))
                .right(px(2.0))
                .bottom(px(2.0))
                .rounded(px(super::COMPOSER_RADIUS - 2.0))
                .border_1()
                .border_dashed()
                .border_color(theme.accent),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(26.0))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(
                    icon(icons::UPLOAD)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(tr(cx, msg::chat::chat_composer_drop_files_to_attach)),
        )
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Tray presentation state on the composer.
#[derive(Default)]
pub(crate) struct TrayState {
    /// The full error message whose details are expanded (a new failure
    /// always starts collapsed).
    expanded_error: Option<String>,
    /// Per chat: the transcript error the user dismissed.
    dismissed_errors: HashMap<String, String>,
    annotations: AnnotationPreview,
    login: Option<TrayLogin>,
    login_attempts: u64,
}

#[derive(Default)]
struct AnnotationPreview {
    open: bool,
    /// Opened by a click: stays up after the pointer leaves.
    pinned: bool,
    trigger_hovered: bool,
    card_hovered: bool,
    task: Option<Task<()>>,
    editing: Option<AnnotationEdit>,
}

struct AnnotationEdit {
    id: String,
    input: Entity<ComposerInput>,
    _submit: Subscription,
}

enum LoginStep {
    Starting,
    Waiting { message: Option<String> },
    Failed(String),
}

/// A provider sign-in started from the tray: the accounts page's browser
/// flow (StartAgentLogin, then PollAgentLogin until it lands).
struct TrayLogin {
    chat: String,
    error_id: Option<String>,
    target: Option<String>,
    attempt: u64,
    login_id: Option<String>,
    url: Option<String>,
    step: LoginStep,
    _task: Option<Task<()>>,
}

fn login_params(target: &Option<String>, mut value: serde_json::Value) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(target));
    }
    value
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl Composer {
    /// The attached tray stack, or `None` when nothing needs showing.
    /// `embedded`: the rows go inside the new-chat selector tray (which owns
    /// the surface) above its chips; otherwise they get their own surface
    /// tucked behind the card.
    pub(crate) fn render_tray_stack(
        &mut self,
        show_queue_latest_shortcut: bool,
        embedded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut rows = Vec::new();
        // The prompt itself is already in the transcript; the tray only says
        // what the cloud computer is doing.
        if let Some(row) = self.cloud_status_row(&theme, cx) {
            let retry = self.cloud_launch_pending(cx)
                && self.cloud.task.is_none()
                && self.visible_failure().is_none();
            rows.push(TrayRow::Pill(if retry {
                row.action(
                    tray_text_action("retry-cloud-launch", "Retry", None, &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            let text = this.input.read(cx).text().to_owned();
                            this.send_cloud(text, cx);
                        }),
                    ),
                )
            } else {
                row
            }));
            if self.cloud_launch_pending(cx) {
                // Nothing else in the tray while the launch owns it.
            } else if let Some(notice) = self.cloud_model_notice_row(&theme, cx) {
                rows.push(TrayRow::Pill(notice));
            }
        } else if let Some(notice) = self.cloud_model_notice_row(&theme, cx) {
            rows.push(TrayRow::Pill(notice));
        }
        rows.extend(self.cloud_unknown_row(&theme, cx).map(TrayRow::Pill));
        rows.extend(self.cloud_runtime_update_row(&theme, cx).map(TrayRow::Pill));
        rows.extend(self.live_cloud_row(&theme, cx).map(TrayRow::Pill));
        rows.extend(self.connection_row(cx).map(TrayRow::Pill));
        rows.extend(self.project_creation_row(&theme, cx).map(TrayRow::Pill));
        rows.extend(
            self.attachment_row(&theme, window, cx)
                .map(TrayRow::Element),
        );
        let (sign_in, error) = self.provider_rows(&theme, cx);
        rows.extend(sign_in.map(TrayRow::Pill));
        rows.extend(error.map(TrayRow::Pill));
        rows.extend(self.todo_tray_pill(window, cx).map(TrayRow::Pill));
        rows.extend(
            self.render_queue_panel(show_queue_latest_shortcut, window, cx)
                .map(TrayRow::Element),
        );
        if rows.is_empty() {
            return None;
        }
        let hairline = theme.border.opacity(0.35);
        let rows = rows.into_iter().enumerate().map(|(ix, row)| {
            let row = match row {
                TrayRow::Pill(pill) => pill.render(&theme, ix == 0),
                TrayRow::Element(element) => element,
            };
            div()
                .w_full()
                .min_w_0()
                .when(ix > 0, |el| el.border_t_1().border_color(hairline))
                .child(row)
                .into_any_element()
        });
        if embedded {
            return Some(
                div()
                    .debug_selector(|| "composer-tray".into())
                    .flex()
                    .flex_col()
                    .border_b_1()
                    .border_color(hairline)
                    .children(rows)
                    .into_any_element(),
            );
        }
        let surface = tray_surface(&theme, cx.has_active_drag())
            .debug_selector(|| "composer-tray".into())
            .children(rows);
        Some(
            crate::frost::frosted(TRAY_RADIUS, crate::frost::MENU_BLUR, surface).into_any_element(),
        )
    }

    fn cloud_model_notice_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<TrayPill> {
        let (message, retry) = self.pickers.read(cx).cloud_model_notice(cx)?;
        let row = TrayPill::new("native-cloud-models", message).glyph(crate::icons::DITHER_CLOUD);
        Some(if retry {
            row.action(
                tray_text_action("retry-cloud-models", "Retry", None, theme).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.pickers
                            .update(cx, |picker, cx| picker.refresh_cloud_models(cx));
                    },
                )),
            )
        } else {
            row
        })
    }

    /// "No connection" while offline; a quiet reconnecting row while only the
    /// selected target's delivery path is degraded (sends will queue).
    fn connection_row(&self, cx: &App) -> Option<TrayPill> {
        use zuse_protocol::ConnectivityState as S;
        let state = self.state.read(cx);
        if state.connectivity.state == S::Offline {
            return Some(
                TrayPill::new("tray-no-connection", "No connection")
                    .glyph(icons::WIFI_OFF)
                    .subtitle("Cloud and remote computers reconnect when you're back online."),
            );
        }
        let degraded = match state.selected_chat.as_deref() {
            Some(id) => state.chat_delivery_degraded(id),
            None => {
                // New-chat canvas: judge by the picked target device.
                let remote_target = state
                    .effective_device_id()
                    .is_some_and(|id| state.local_device_id.as_deref() != Some(id.as_str()));
                remote_target
                    && (state.connectivity.state == S::Reconnecting
                        || state
                            .effective_device_id()
                            .is_some_and(|id| !state.device_online(&id, chrono::Utc::now())))
            }
        };
        degraded.then(|| {
            TrayPill::new("tray-reconnecting", "Reconnecting")
                .glyph(icons::REFRESH)
                .subtitle("Messages will send once the connection recovers.")
        })
    }

    // ---- attachments and annotations ----

    /// Annotation previews and attachments without an inline draft chip.
    fn attachment_row(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut comments: Vec<StagedAnnotation> = self
            .staged_comments(cx)
            .into_iter()
            .map(StagedAnnotation::Code)
            .collect();
        comments.extend(
            self.staged_contexts(cx)
                .into_iter()
                .filter(|item| item.browser_annotation.is_some())
                .map(StagedAnnotation::Browser),
        );
        let tiles: Vec<StagedTile> = self
            .tiled_attachments(cx)
            .into_iter()
            .filter(|att| att.image().is_none())
            .map(|att| StagedTile {
                id: att.id.clone(),
                name: att.name.clone(),
                image: att.image().cloned(),
            })
            .collect();
        if comments.is_empty() {
            self.tray.annotations = AnnotationPreview::default();
        }
        let contexts: Vec<_> = self
            .staged_contexts(cx)
            .into_iter()
            .filter(|context| context.browser_annotation.is_none())
            .collect();
        if comments.is_empty() && tiles.is_empty() && contexts.is_empty() {
            return None;
        }
        let context_pills: Vec<AnyElement> = contexts
            .iter()
            .enumerate()
            .map(|(ix, context)| self.context_pill(ix, context, theme, window, cx))
            .collect();
        let pill = (!comments.is_empty()).then(|| self.annotation_pill(&comments, theme, cx));
        Some(
            div()
                .id("tray-attachments")
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(6.0))
                .children(context_pills)
                .children(pill)
                .children(
                    tiles
                        .into_iter()
                        .enumerate()
                        .map(|(ix, tile)| self.attachment_thumb(ix, tile, THUMB, theme, cx)),
                )
                .into_any_element(),
        )
    }

    /// One staged attachment: a 40px thumbnail (click previews it full size)
    /// or a file glyph, with a remove × revealed on hover.
    fn attachment_thumb(
        &self,
        ix: usize,
        tile: StagedTile,
        size: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group: SharedString = format!("tray-att-{}", tile.id).into();
        let frame = div()
            .id(("tray-att-thumb", ix))
            .size(px(size))
            .rounded(px(THUMB_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(crate::theme::hairline(0.10))
            .tip(tile.name.clone());
        let frame = match tile.image {
            Some(image) => {
                let preview =
                    super::attachments::PreviewImage::new(tile.name.clone(), image.clone());
                frame
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        preview.viewer.reset();
                        this.preview = Some(preview.clone());
                        this.preview_focus_pending = true;
                        cx.notify();
                    }))
                    .child(
                        // Explicit dims: img honours the intrinsic aspect over
                        // a percent height, which would overflow the frame.
                        img(image)
                            .w(px(size - 2.0))
                            .h(px(size - 2.0))
                            .rounded(px(THUMB_RADIUS - 1.0))
                            .object_fit(ObjectFit::Cover),
                    )
            }
            None => frame
                .flex()
                .items_center()
                .justify_center()
                .bg(crate::theme::ink(0.04))
                .child(
                    crate::file_icons::icon(
                        crate::file_icons::FileIconIdentity::file(&tile.name),
                        theme.appearance,
                    )
                    .size(px(20.0)),
                ),
        };
        let remove_id = tile.id.clone();
        div()
            .group(group.clone())
            .flex_none()
            .relative()
            .child(frame)
            // Own layer: images paint last in the frosted surface, so the
            // button would otherwise sit under the thumbnail.
            .child(crate::frost::layered(
                div()
                    .id(("tray-att-remove", ix))
                    .absolute()
                    .top(px(-4.0))
                    .right(px(-4.0))
                    .size(px(16.0))
                    .rounded_full()
                    .bg(theme.bg)
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .opacity(0.0)
                    .group_hover(group, |s| s.opacity(1.0))
                    .role(gpui::Role::Button)
                    .aria_label(format!("Remove {}", tile.name))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_attachment(&remove_id, cx);
                    }))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            ))
            .into_any_element()
    }

    /// Image attachments stay visible even when their inline chip is present.
    pub(super) fn render_image_strip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tiles: Vec<_> = self
            .staged()
            .iter()
            .filter_map(|att| {
                Some(StagedTile {
                    id: att.id.clone(),
                    name: att.name.clone(),
                    image: Some(att.image()?.clone()),
                })
            })
            .collect();
        if tiles.is_empty() {
            return None;
        }
        Some(
            div()
                .id("composer-image-strip")
                .flex()
                .flex_wrap()
                .gap(px(super::STRIP_GAP))
                .px(px(super::STRIP_PAD_X))
                .pt(px(super::STRIP_PAD_TOP))
                .children(tiles.into_iter().enumerate().map(|(ix, tile)| {
                    self.attachment_thumb(ix, tile, super::STRIP_THUMB, theme, cx)
                }))
                .into_any_element(),
        )
    }

    /// One staged context item: the message badge it will become (hover or
    /// click shows its Markdown), with a remove ×.
    fn context_pill(
        &mut self,
        ix: usize,
        context: &crate::context_items::ContextItem,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let badge = crate::badges::MessageBadge {
            icon: icons::CHAT_ROUND_LINE,
            label: context.label.clone().into(),
            details: Vec::new(),
            markdown: Some(context.body.clone().into()),
        };
        let hover = theme.element_hover;
        let id = context.id.clone();
        let label = context.label.clone();
        let remove = div()
            .id(("tray-context-remove", ix))
            .size(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .role(gpui::Role::Button)
            .aria_label(format!("Remove {label}"))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                let key = this.current_key.clone();
                this.state.update(cx, |state, cx| {
                    state.remove_context_item(&key, &id);
                    cx.notify();
                });
                cx.notify();
            }))
            .child(
                icon(icons::CLOSE)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            )
            .into_any_element();
        let render = if context.source_key.starts_with("browser-element:") {
            crate::badges::render_browser_context
        } else {
            crate::badges::render_with
        };
        render(
            format!("tray-context-{}", context.id),
            &badge,
            theme,
            Some(remove),
            window,
            cx,
        )
    }

    /// `ContextPill`: "{n} Annotations" with a clear-all ×; hovering opens
    /// the preview card listing each one.
    fn annotation_pill(
        &mut self,
        comments: &[StagedAnnotation],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = comments.len();
        let label: SharedString = if count == 1 {
            "1 Annotation".into()
        } else {
            format!("{count} Annotations").into()
        };
        let hover = theme.element_hover;
        let card = self
            .tray
            .annotations
            .open
            .then(|| self.annotation_card(comments, theme, cx));
        div()
            .id("tray-annotations")
            .relative()
            .h(px(28.0))
            .max_w_full()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(10.0))
            .pr(px(4.0))
            .rounded_full()
            .border_1()
            .border_color(theme.border.opacity(0.5))
            .bg(theme.muted_fill().opacity(0.6))
            .text_size(px(TEXT_SIZE))
            .text_color(theme.text)
            .child(
                div()
                    .id("tray-annotations-trigger")
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .role(gpui::Role::Button)
                    .aria_label(label.clone())
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.annotation_trigger_hover(*hovered, cx)
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let preview = &mut this.tray.annotations;
                        if preview.open && preview.pinned {
                            this.close_annotation_preview(cx);
                        } else {
                            preview.open = true;
                            preview.pinned = true;
                            preview.task = None;
                            cx.notify();
                        }
                    }))
                    .child(
                        icon(icons::CHAT_ROUND_LINE)
                            .size(px(GLYPH))
                            .text_color(theme.text_muted),
                    )
                    .child(div().min_w_0().truncate().child(label.clone())),
            )
            .child(
                div()
                    .id("tray-annotations-clear")
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .role(gpui::Role::Button)
                    .aria_label(format!("Remove {label}"))
                    .tip("Remove all")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let key = this.current_key.clone();
                        this.state.update(cx, |state, cx| {
                            state.take_review_comments(&key);
                            let ids: Vec<_> = state
                                .context_items(&key)
                                .iter()
                                .filter(|item| item.browser_annotation.is_some())
                                .map(|item| item.id.clone())
                                .collect();
                            for id in ids {
                                state.remove_context_item(&key, &id);
                            }
                            cx.notify();
                        });
                        this.tray.annotations = AnnotationPreview::default();
                        cx.notify();
                    }))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(12.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .children(card.map(|card| {
                crate::popover::anchored_menu_above("tray-annotations-menu", card, None)
            }))
            .into_any_element()
    }

    /// The preview card: each annotation's file chip and comment, with a
    /// hover-revealed Edit (inline field; Enter saves, Esc cancels) and a
    /// remove ×.
    fn annotation_card(
        &mut self,
        comments: &[StagedAnnotation],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme.for_popup();
        let editing = self
            .tray
            .annotations
            .editing
            .as_ref()
            .map(|edit| (edit.id.clone(), edit.input.clone()));
        let hover = theme.element_hover;
        let rows = comments.iter().enumerate().map(|(ix, comment)| {
            let group: SharedString = format!("tray-annotation-{}", comment.id()).into();
            let editing_this = editing
                .as_ref()
                .filter(|(id, _)| *id == comment.id())
                .map(|(_, input)| input.clone());
            let id = comment.id().clone();
            let edit_button = if editing_this.is_some() {
                tray_icon_action(
                    ("tray-annotation-save", ix),
                    icons::CHECK,
                    "Save annotation",
                    ICON_ACTION,
                    &theme,
                )
                .on_click(cx.listener(|this, _, _, cx| this.commit_annotation_edit(cx)))
            } else {
                let body = comment.comment().to_owned();
                tray_icon_action(
                    ("tray-annotation-edit", ix),
                    icons::PEN,
                    "Edit annotation",
                    ICON_ACTION,
                    &theme,
                )
                .opacity(0.65)
                .group_hover(group.clone(), |s| s.opacity(1.0))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.begin_annotation_edit(id.clone(), body.clone(), window, cx)
                }))
            };
            let remove_id = comment.id().clone();
            let browser_annotation = matches!(comment, StagedAnnotation::Browser(_));
            let remove = tray_icon_action(
                ("tray-annotation-remove", ix),
                icons::CLOSE,
                "Remove annotation",
                ICON_ACTION,
                &theme,
            )
            .opacity(0.7)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .on_click(cx.listener(move |this, _, _, cx| {
                let key = this.current_key.clone();
                let id = remove_id.clone();
                this.state.update(cx, |state, cx| {
                    if browser_annotation {
                        state.remove_context_item(&key, &id);
                    } else {
                        state.remove_review_comment(&key, &id);
                    }
                    cx.notify();
                });
                if this
                    .tray
                    .annotations
                    .editing
                    .as_ref()
                    .is_some_and(|edit| edit.id == id)
                {
                    this.tray.annotations.editing = None;
                }
                cx.notify();
            }));
            let comment_body = match editing_this {
                Some(input) => div()
                    .w_full()
                    .min_h(px(28.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .bg(theme.bg.opacity(0.7))
                    .border_1()
                    .border_color(theme.border.opacity(0.5))
                    .text_size(px(12.0))
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.tray.annotations.editing = None;
                            cx.notify();
                        }
                    }))
                    .child(input)
                    .into_any_element(),
                None => div()
                    .w_full()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(theme.text)
                    .child(SharedString::from(comment.comment().to_owned()))
                    .into_any_element(),
            };
            div()
                .id(("tray-annotation-row", ix))
                .group(group)
                .min_w_0()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .hover(move |s| s.bg(hover))
                .when(ix > 0, |el| {
                    el.border_t_1().border_color(theme.border.opacity(0.35))
                })
                .child(
                    div()
                        .size(px(20.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.muted_fill())
                        .text_color(theme.text_muted)
                        .text_size(px(10.0))
                        .child((ix + 1).to_string()),
                )
                .child(annotation_anchor(comment, &theme))
                .child(div().flex_1())
                .child(edit_button)
                .child(remove)
                .child(comment_body)
        });
        crate::popover::popover_card(&theme)
            .id("tray-annotations-card")
            .w(px(416.0))
            .max_h(px(320.0))
            .p(px(8.0))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.tray.annotations.card_hovered = *hovered;
                if *hovered {
                    this.tray.annotations.task = None;
                } else {
                    this.schedule_annotation_close(cx);
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                // A press on the pill itself is the pill's click to handle.
                if !this.tray.annotations.trigger_hovered {
                    this.close_annotation_preview(cx);
                }
            }))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child("Annotations"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child("Review, edit, or remove before sending."),
                    ),
            )
            .child(
                div()
                    .id("tray-annotations-list")
                    .max_h(px(192.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .into_any_element()
    }

    fn annotation_trigger_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let preview = &mut self.tray.annotations;
        preview.trigger_hovered = hovered;
        if !hovered {
            self.schedule_annotation_close(cx);
            return;
        }
        if preview.open {
            preview.task = None;
            return;
        }
        preview.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_OPEN_DELAY).await;
            this.update(cx, |this, cx| {
                if this.tray.annotations.trigger_hovered {
                    this.tray.annotations.open = true;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn schedule_annotation_close(&mut self, cx: &mut Context<Self>) {
        let preview = &mut self.tray.annotations;
        if !preview.open || preview.pinned || preview.editing.is_some() {
            if !preview.open {
                preview.task = None;
            }
            return;
        }
        preview.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_CLOSE_GRACE).await;
            this.update(cx, |this, cx| {
                let preview = &this.tray.annotations;
                if !preview.pinned
                    && !preview.trigger_hovered
                    && !preview.card_hovered
                    && preview.editing.is_none()
                {
                    this.close_annotation_preview(cx);
                }
            })
            .ok();
        }));
    }

    fn close_annotation_preview(&mut self, cx: &mut Context<Self>) {
        let preview = &mut self.tray.annotations;
        preview.open = false;
        preview.pinned = false;
        preview.card_hovered = false;
        preview.task = None;
        preview.editing = None;
        cx.notify();
    }

    fn begin_annotation_edit(
        &mut self,
        id: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            let mut input = ComposerInput::new("Edit annotation", cx);
            input.set_text(body, cx);
            input
        });
        let submit = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.commit_annotation_edit(cx);
            }
        });
        window.focus(&input.read(cx).focus_handle.clone(), cx);
        let preview = &mut self.tray.annotations;
        preview.open = true;
        preview.task = None;
        preview.editing = Some(AnnotationEdit {
            id,
            input,
            _submit: submit,
        });
        cx.notify();
    }

    fn commit_annotation_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.tray.annotations.editing.take() else {
            return;
        };
        let body = edit.input.read(cx).text().trim().to_string();
        if !body.is_empty() {
            let key = self.current_key.clone();
            self.state.update(cx, |state, cx| {
                if let Some(mut context) = state
                    .context_items(&key)
                    .iter()
                    .find(|item| item.id == edit.id && item.browser_annotation.is_some())
                    .cloned()
                {
                    context.update_browser_comment(body);
                    state.add_context_item(&key, context);
                } else {
                    state.update_review_comment_body(&key, &edit.id, body);
                }
                cx.notify();
            });
        }
        cx.notify();
    }

    // ---- provider sign-in and errors ----

    /// The sign-in row (lost credentials, or a sign-in in flight) and the
    /// error row (a failed send, else the error that ended the last turn).
    fn provider_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (Option<TrayPill>, Option<TrayPill>) {
        let harness = self.pickers.read(cx).resolved(cx).harness;
        let label = harness.map_or("your provider", crate::settings::accounts::provider_name);
        // A failed send in this composer outranks the transcript's error.
        if let Some(message) = self.visible_failure() {
            let offline = message.as_ref() == "Engine not connected";
            let configuration = is_configuration_notice(
                message.as_ref(),
                self.failure_key.as_deref(),
                self.state.read(cx).configuration_notice().as_ref(),
            );
            let cloud_launch = self.cloud.request.is_some();
            let sign_in = if cloud_launch {
                None
            } else {
                self.sign_in_row(None, harness, label, theme, cx)
            };
            let actions = if configuration {
                vec![
                    tray_text_action("refresh-chat-choices", "Refresh access", None, theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state
                                .update(cx, |state, cx| state.refresh_selected_configuration(cx));
                            this.permission_open = false;
                            cx.notify();
                        }))
                        .into_any_element(),
                ]
            } else if cloud_launch {
                vec![
                    tray_text_action("retry-cloud-launch", "Retry", None, theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let text = this.input.read(cx).text().to_owned();
                            this.send_cloud(text, cx);
                        }))
                        .into_any_element(),
                ]
            } else {
                Vec::new()
            };
            let (title, subtitle) = if offline {
                ("Engine not connected", None)
            } else if configuration {
                ("Chat choices unavailable", Some(message.to_string()))
            } else {
                ("Couldn't send", Some(message.to_string()))
            };
            let pill = self.error_pill(
                "tray-send-error",
                if offline {
                    TrayTone::Warning
                } else {
                    TrayTone::Danger
                },
                if offline {
                    icons::WIFI_OFF
                } else {
                    icons::ALERT_CIRCLE
                },
                title,
                subtitle,
                message.to_string(),
                actions,
                theme,
                cx.listener(|this, _, _, cx| {
                    this.failure = None;
                    this.failure_key = None;
                    cx.notify();
                }),
                cx,
            );
            return (sign_in, Some(pill));
        }
        let live = self.run_live(cx);
        let (chat, error, entries_prompt) = {
            let state = self.state.read(cx);
            let Some(chat) = state.selected_chat.clone() else {
                return (None, None);
            };
            let error = latest_transcript_error(&state.transcript);
            (chat, error, last_prompt_text(&state.transcript))
        };
        let error =
            error.filter(|(id, _)| !live && self.tray.dismissed_errors.get(&chat) != Some(id));
        let kind = error
            .as_ref()
            .map(|(_, message)| classify_provider_error(message));
        let can_sign_in = harness.is_some_and(crate::settings::accounts::signs_in);
        if kind == Some(ProviderErrorKind::SignIn) && can_sign_in {
            let error_id = error.as_ref().map(|(id, _)| id.clone());
            return (self.sign_in_row(error_id, harness, label, theme, cx), None);
        }
        let sign_in = self.sign_in_row(None, harness, label, theme, cx);
        let Some(((error_id, message), kind)) = error.zip(kind) else {
            return (sign_in, None);
        };
        let (tone, glyph) = match kind {
            ProviderErrorKind::UsageLimit => (TrayTone::Warning, icons::DASHBOARD_SPEED),
            ProviderErrorKind::Reconnecting => (TrayTone::Default, icons::REFRESH),
            _ => (TrayTone::Danger, icons::ALERT_CIRCLE),
        };
        let mut actions = Vec::new();
        let draft_empty = !composer_has_content(
            self.input.read(cx).text(),
            self.staged().len() + self.staged_appshots().len(),
            self.staged_note_count(cx),
        );
        if let Some(prompt) =
            entries_prompt.filter(|_| draft_empty && kind != ProviderErrorKind::Reconnecting)
        {
            let retry_chat = chat.clone();
            let retry_id = error_id.clone();
            actions.push(
                tray_text_action("tray-error-retry", "Retry", None, theme)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tray
                            .dismissed_errors
                            .insert(retry_chat.clone(), retry_id.clone());
                        let queue = this.run_live(cx);
                        this.send(prompt.clone(), queue, false, false, cx);
                    }))
                    .into_any_element(),
            );
        }
        if kind == ProviderErrorKind::UsageLimit && self.has_other_accounts(harness, cx) {
            actions.push(
                tray_text_action("tray-error-switch", "Switch account", None, theme)
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(ComposerEvent::WorkspaceCommand(WorkspaceCommand::Accounts));
                    }))
                    .into_any_element(),
            );
        }
        let dismiss_chat = chat.clone();
        let dismiss_id = error_id.clone();
        let pill = self.error_pill(
            "tray-provider-error",
            tone,
            glyph,
            provider_error_copy(kind),
            Some(first_line(&message).to_string()),
            message,
            actions,
            theme,
            cx.listener(move |this, _, _, cx| {
                this.tray
                    .dismissed_errors
                    .insert(dismiss_chat.clone(), dismiss_id.clone());
                cx.notify();
            }),
            cx,
        );
        (sign_in, Some(pill))
    }

    /// An error row: click expands the full, selectable message; Copy and
    /// Dismiss always trail the row's own recovery actions.
    #[allow(clippy::too_many_arguments)]
    fn error_pill(
        &self,
        id: &'static str,
        tone: TrayTone,
        glyph: &'static str,
        title: &'static str,
        subtitle: Option<String>,
        message: String,
        actions: Vec<AnyElement>,
        theme: &Theme,
        dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> TrayPill {
        let expanded = self.tray.expanded_error.as_deref() == Some(message.as_str());
        let toggle = message.clone();
        let copy = message.clone();
        let mut pill = TrayPill::new(id, title).tone(tone).glyph(glyph);
        if let Some(subtitle) = subtitle.filter(|subtitle| subtitle != title) {
            pill = pill.subtitle(subtitle);
        }
        let weak = cx.weak_entity();
        pill = pill.on_click(
            if expanded {
                format!("Collapse {title}")
            } else {
                format!("Expand {title}")
            },
            move |_, _, cx| {
                let toggle = toggle.clone();
                weak.update(cx, |this, cx| {
                    this.tray.expanded_error = (!expanded).then_some(toggle);
                    cx.notify();
                })
                .ok();
            },
        );
        for action in actions {
            pill = pill.action(action);
        }
        pill.action(
            tray_icon_action(
                SharedString::from(format!("{id}-copy")),
                icons::COPY,
                "Copy details",
                28.0,
                theme,
            )
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()));
            }),
        )
        .action(
            tray_icon_action(
                SharedString::from(format!("{id}-dismiss")),
                icons::CLOSE,
                "Dismiss",
                28.0,
                theme,
            )
            .on_click(dismiss),
        )
        .expanded(expanded.then(|| {
            div()
                .id(SharedString::from(format!("{id}-message")))
                .max_h(px(160.0))
                .overflow_y_scroll()
                .px(px(ROW_PAD_X))
                .py(px(8.0))
                .text_size(px(TEXT_SIZE))
                .line_height(px(17.0))
                .text_color(theme.text_muted)
                // The full provider text; Copy puts it on the clipboard.
                .child(SharedString::from(message))
                .into_any_element()
        }))
    }

    fn has_other_accounts(&self, harness: Option<HarnessId>, cx: &App) -> bool {
        let Some(harness) = harness.filter(|h| crate::settings::accounts::switches_accounts(*h))
        else {
            return false;
        };
        let target = self.login_target(cx);
        cx.try_global::<crate::settings::accounts::AccountsSnapshotCache>()
            .and_then(|cache| cache.0.get(&target))
            .is_some_and(|snapshot| {
                crate::settings::accounts::provider_accounts(snapshot, harness).len() > 1
            })
    }

    /// The chat's device when it is another computer (logins are per device).
    fn login_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        state
            .selected_chat_row()
            .map(|chat| chat.device_id.clone())
            .filter(|device| state.local_device_id.as_ref() != Some(device))
    }

    /// "Signed out of {label}" for an auth failure (`error_id`), or while a
    /// sign-in started here is in flight or failed.
    fn sign_in_row(
        &self,
        error_id: Option<String>,
        harness: Option<HarnessId>,
        label: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<TrayPill> {
        let chat = self.state.read(cx).selected_chat.clone()?;
        let login = self.tray.login.as_ref().filter(|login| login.chat == chat);
        if error_id.is_none() && login.is_none() {
            return None;
        }
        let harness = harness?;
        let busy = login.is_some_and(|login| !matches!(login.step, LoginStep::Failed(_)));
        let subtitle: SharedString = match login.map(|login| &login.step) {
            Some(LoginStep::Starting) => format!("Starting {label} sign-in…").into(),
            Some(LoginStep::Waiting { message }) => message
                .clone()
                .unwrap_or_else(|| "Waiting for browser sign-in…".into())
                .into(),
            Some(LoginStep::Failed(reason)) => reason.clone().into(),
            None => "Sign in to continue this chat.".into(),
        };
        let mut pill =
            TrayPill::new("tray-sign-in", format!("Signed out of {label}")).subtitle(subtitle);
        pill = if busy {
            pill.icon_element(crate::loaders::mini_glyph_spinner(
                "tray-sign-in-spinner",
                2.5,
                theme.loader_glyph(),
                cx.entity_id(),
                cx,
            ))
        } else {
            pill.glyph(icons::LOGOUT_2)
        };
        if busy {
            if let Some(url) = login.and_then(|login| login.url.clone()) {
                pill = pill.action(
                    tray_text_action(
                        "tray-sign-in-reopen",
                        "Open browser again",
                        Some(icons::ARROW_UP_RIGHT),
                        theme,
                    )
                    .on_click(move |_, _, cx| crate::settings::accounts::open_login_url(&url, cx)),
                );
            }
            pill = pill.action(
                tray_text_action("tray-sign-in-cancel", "Cancel", None, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_tray_login(cx))),
            );
        } else {
            let failed = login.is_some();
            let error_id = error_id.or_else(|| login.and_then(|login| login.error_id.clone()));
            pill = pill.action(
                tray_text_action(
                    "tray-sign-in-start",
                    if failed { "Retry" } else { "Sign in" },
                    None,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.start_tray_login(harness, error_id.clone(), cx)
                })),
            );
        }
        Some(pill)
    }

    /// The accounts page's browser sign-in, driven from the tray. Paste-code
    /// providers need the accounts dialog, so they hand over to Settings.
    fn start_tray_login(
        &mut self,
        harness: HarnessId,
        error_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_error("Engine not connected", cx);
            return;
        };
        self.cancel_tray_login(cx);
        self.tray.login_attempts += 1;
        let attempt = self.tray.login_attempts;
        let target = self.login_target(cx);
        let params = login_params(&target, serde_json::json!({ "harness": harness }));
        let task = cx.spawn(async move |this, cx| {
            let start = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value).map_err(|e| e.to_string())
                });
            let poll = this
                .update(cx, |this, cx| {
                    this.apply_tray_login_start(attempt, start, cx)
                })
                .ok()
                .flatten();
            let Some(poll) = poll else {
                return;
            };
            loop {
                cx.background_executor().timer(LOGIN_POLL).await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, poll.clone())
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value)
                            .map_err(|_| "malformed reply".to_string())
                    });
                match this.update(cx, |this, cx| {
                    this.apply_tray_login_poll(attempt, result, cx)
                }) {
                    Ok(false) => {}
                    Ok(true) | Err(_) => break,
                }
            }
        });
        self.tray.login = Some(TrayLogin {
            chat,
            error_id,
            target,
            attempt,
            login_id: None,
            url: None,
            step: LoginStep::Starting,
            _task: Some(task),
        });
        cx.notify();
    }

    /// Fold StartAgentLogin in; returns the poll params when the browser
    /// flow should be polled.
    fn apply_tray_login_start(
        &mut self,
        attempt: u64,
        result: Result<AgentLoginStart, String>,
        cx: &mut Context<Self>,
    ) -> Option<serde_json::Value> {
        let login = self
            .tray
            .login
            .as_mut()
            .filter(|login| login.attempt == attempt)?;
        let poll = match result {
            Ok(start) => {
                if start.mode == AgentLoginMode::PasteCode {
                    // The paste-code step lives in the accounts dialog.
                    self.tray.login = None;
                    cx.emit(ComposerEvent::WorkspaceCommand(WorkspaceCommand::Accounts));
                    cx.notify();
                    return None;
                }
                if !start.url.is_empty() {
                    crate::settings::accounts::open_login_url(&start.url, cx);
                    login.url = Some(start.url.clone());
                }
                login.step = LoginStep::Waiting { message: None };
                login.login_id = Some(start.login_id.clone());
                Some(login_params(
                    &login.target,
                    serde_json::json!({ "loginId": start.login_id }),
                ))
            }
            Err(error) => {
                login.step = LoginStep::Failed(format!("Couldn't start the sign-in: {error}"));
                None
            }
        };
        cx.notify();
        poll
    }

    /// Fold one poll in; `true` once polling is over.
    fn apply_tray_login_poll(
        &mut self,
        attempt: u64,
        result: Result<AgentLoginPoll, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(login) = self
            .tray
            .login
            .as_mut()
            .filter(|login| login.attempt == attempt)
        else {
            return true;
        };
        let finished = match result {
            Ok(poll) => match poll.status {
                AgentLoginStatus::Done => {
                    // Signed in: the auth failure is resolved.
                    if let Some(login) = self.tray.login.take()
                        && let Some(error_id) = login.error_id
                    {
                        self.tray.dismissed_errors.insert(login.chat, error_id);
                    }
                    true
                }
                AgentLoginStatus::Error => {
                    login.step = LoginStep::Failed(
                        poll.message
                            .unwrap_or_else(|| "The sign-in failed.".to_string()),
                    );
                    true
                }
                AgentLoginStatus::Pending => {
                    if let Some(url) = poll.url.filter(|url| login.url.as_ref() != Some(url)) {
                        crate::settings::accounts::open_login_url(&url, cx);
                        login.url = Some(url);
                    }
                    login.step = LoginStep::Waiting {
                        message: poll.message,
                    };
                    false
                }
            },
            Err(error) => {
                login.step = LoginStep::Failed(format!("Lost track of the sign-in: {error}"));
                true
            }
        };
        cx.notify();
        finished
    }

    fn cancel_tray_login(&mut self, cx: &mut Context<Self>) {
        let Some(login) = self.tray.login.take() else {
            return;
        };
        if let (Some(login_id), false, Some(engine)) = (
            login.login_id.clone(),
            matches!(login.step, LoginStep::Failed(_)),
            self.state.read(cx).engine().cloned(),
        ) {
            let params = login_params(&login.target, serde_json::json!({ "loginId": login_id }));
            cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            })
            .detach();
        }
        cx.notify();
    }
}

/// What a thumbnail needs, copied out so rendering can borrow the composer.
enum StagedAnnotation {
    Code(crate::comments::ReviewComment),
    Browser(crate::context_items::ContextItem),
}
impl StagedAnnotation {
    fn id(&self) -> String {
        match self {
            Self::Code(item) => item.id.clone(),
            Self::Browser(item) => item.id.clone(),
        }
    }
    fn comment(&self) -> &str {
        match self {
            Self::Code(item) => &item.body,
            Self::Browser(item) => item
                .browser_annotation
                .as_ref()
                .map_or("", |annotation| annotation.comment.as_str()),
        }
    }
}
fn annotation_anchor(annotation: &StagedAnnotation, theme: &Theme) -> AnyElement {
    match annotation {
        StagedAnnotation::Code(item) => annotation_file_chip(item, theme),
        StagedAnnotation::Browser(item) => div()
            .id(gpui::SharedString::from(format!(
                "annotation-anchor-{}",
                item.id
            )))
            .min_w_0()
            .max_w(px(260.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(6.0))
            .py(px(3.0))
            .rounded(px(5.0))
            .bg(theme.muted_fill())
            .tip(
                item.browser_annotation
                    .as_ref()
                    .map(|annotation| {
                        format!("{}\n{}", annotation.page_url, annotation.target_details())
                    })
                    .unwrap_or_default(),
            )
            .child(
                icon(icons::BROWSER_SELECT)
                    .size(px(13.0))
                    .text_color(theme.accent),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(item.label.clone()),
            )
            .into_any_element(),
    }
}

struct StagedTile {
    id: String,
    name: String,
    image: Option<std::sync::Arc<gpui::Image>>,
}

/// The annotation's anchor: file glyph, name and line in a quiet chip.
fn annotation_file_chip(comment: &crate::comments::ReviewComment, theme: &Theme) -> AnyElement {
    let path = comment.cite_path();
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    div()
        .flex_none()
        .max_w(relative(0.44))
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border.opacity(0.45))
        .bg(theme.ink(0.05))
        .text_size(px(11.0))
        .child(
            crate::file_icons::icon(
                crate::file_icons::FileIconIdentity::file(&name),
                theme.appearance,
            )
            .size(px(14.0)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_strong)
                .child(SharedString::from(name)),
        )
        .child(
            div()
                .flex_none()
                .text_color(theme.text_muted)
                .child(SharedString::from(format!(":{}", comment.line))),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Review fixtures
// ---------------------------------------------------------------------------

#[cfg(feature = "browser-fixture")]
impl Composer {
    /// Seed one tray state over the worked transcript: `offline`, `error`,
    /// `error-open`, `error-danger`, `signin`, `annotations`, `attachments`,
    /// `todo`, `todo-open`, `queue`, or `stack` (several at once).
    pub fn fixture_seed_trays(&mut self, kind: &str, cx: &mut Context<Self>) {
        let usage_limit = "You've hit your usage limit for this plan. Your limit resets at 4:00 PM.\n\nAPI Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"This request would exceed your account's rate limit. Please try again later.\"}}";
        let failed = "The agent exited unexpectedly (exit status 1).\nstderr: error: could not read config file ~/.config/agent/settings.json: permission denied";
        self.fixture_reset_trays(cx);
        match kind {
            "offline" => self.fixture_offline(cx),
            "error" => self.fixture_error(usage_limit, cx),
            "error-open" => {
                self.fixture_error(usage_limit, cx);
                self.tray.expanded_error = Some(usage_limit.to_string());
            }
            "error-danger" => self.fixture_error(failed, cx),
            "signin" => self.fixture_error("Not logged in · Please run /login", cx),
            "annotations" => {
                self.fixture_annotations(cx);
                self.tray.annotations.open = true;
                self.tray.annotations.pinned = true;
            }
            "attachments" => self.fixture_attachments(cx),
            "todo" => self.fixture_todo(false, cx),
            "todo-open" => self.fixture_todo(true, cx),
            "queue" => self.fixture_queue(cx),
            "stack" => {
                self.fixture_offline(cx);
                self.fixture_annotations(cx);
                self.fixture_attachments(cx);
                self.fixture_error(failed, cx);
                self.fixture_todo(false, cx);
                self.fixture_queue(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Routes run in one window: undo the previous route's tray seed.
    fn fixture_reset_trays(&mut self, cx: &mut Context<Self>) {
        thread_local! {
            static CONNECTIVITY: std::cell::Cell<Option<zuse_protocol::ConnectivityState>> =
                const { std::cell::Cell::new(None) };
        }
        self.tray = TrayState::default();
        self.todo_panels.clear();
        self.attachments.remove(&self.current_key);
        let key = self.current_key.clone();
        self.state.update(cx, |state, cx| {
            let initial =
                CONNECTIVITY.with(|cell| *cell.get().get_or_insert(state.connectivity.state));
            CONNECTIVITY.with(|cell| cell.set(Some(initial)));
            state.connectivity.state = initial;
            state.take_review_comments(&key);
            state.queue.clear();
            cx.notify();
        });
    }

    fn fixture_offline(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.connectivity.state = zuse_protocol::ConnectivityState::Offline;
            cx.notify();
        });
    }

    fn fixture_push_entry(&mut self, parts: Vec<MessagePart>, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let mut entries = state.transcript.clone();
            let at = entries.last().map_or(0, |entry| entry.created_at) + 1_000;
            entries.push(SessionMessageEntry {
                id: format!("fixture-tray-{}", entries.len()),
                role: MessageRole::Assistant,
                parts,
                created_at: at,
                device_id: "local".into(),
                status: None,
                continuation_of: None,
                duration_ms: None,
            });
            state.apply_transcript(entries);
            cx.notify();
        });
    }

    fn fixture_error(&mut self, message: &str, cx: &mut Context<Self>) {
        self.fixture_push_entry(
            vec![MessagePart::Error {
                id: "e0".into(),
                message: message.into(),
            }],
            cx,
        );
    }

    fn fixture_todo(&mut self, expand: bool, cx: &mut Context<Self>) {
        use zuse_protocol::{TodoItem, TodoStatus, ToolCall};
        let items = vec![
            TodoItem::new("Read the auth redirect flow", TodoStatus::Completed),
            TodoItem::new(
                "Drop the token from the redirect target",
                TodoStatus::Completed,
            ),
            TodoItem::new(
                "Cover the expired-token path with a test",
                TodoStatus::InProgress,
            ),
            TodoItem::new("Update the onboarding copy", TodoStatus::Pending),
            TodoItem::new("Open a pull request", TodoStatus::Pending),
        ];
        // Keep the error (if any) last: the plan lands in an earlier entry.
        let error = self
            .state
            .read(cx)
            .transcript
            .last()
            .cloned()
            .filter(|entry| {
                entry
                    .parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Error { .. }))
            });
        if error.is_some() {
            self.state.update(cx, |state, _| {
                let mut entries = state.transcript.clone();
                entries.pop();
                state.apply_transcript(entries);
            });
        }
        self.fixture_push_entry(
            vec![MessagePart::Tool {
                id: "fixture-todo".into(),
                call: ToolCall::Todo { items },
                is_error: false,
                resolved: true,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: None,
                subagent_status: None,
                subagent_tail: None,
            }],
            cx,
        );
        if let Some(error) = error {
            self.fixture_push_entry(error.parts, cx);
        }
        // Unfinished lists open by default; the collapsed row is the
        // common sight.
        if !expand && let Some(chat) = self.state.read(cx).selected_chat.clone() {
            self.todo_panels.entry(chat).or_default().toggle(false);
        }
    }

    fn fixture_queue(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.queue = vec![
                zuse_doc::QueuedMessage::new(
                    "fixture-q1",
                    "Also cover the expired-token path",
                    "local",
                ),
                zuse_doc::QueuedMessage::new("fixture-q2", "Then open a pull request", "local"),
            ];
            cx.notify();
        });
    }

    fn fixture_annotations(&mut self, cx: &mut Context<Self>) {
        use crate::comments::{CommentSide, ReviewComment};
        let key = self.current_key.clone();
        self.state.update(cx, |state, cx| {
            for comment in [
                ReviewComment::new(
                    "crates/auth/src/redirect.rs",
                    CommentSide::New,
                    42,
                    "Strip the token before building the redirect target.",
                ),
                ReviewComment::file(
                    "apps/web/src/onboarding.tsx",
                    18,
                    "This copy still says \"workspace\"; use \"project\".",
                ),
                ReviewComment::new(
                    "crates/auth/tests/redirect.rs",
                    CommentSide::New,
                    7,
                    "Add a case for an expired token.",
                ),
            ] {
                state.add_review_comment(&key, comment);
            }
            cx.notify();
        });
    }

    fn fixture_attachments(&mut self, cx: &mut Context<Self>) {
        let colors = [[0x7c, 0xb3, 0x42], [0x3b, 0x82, 0xf6], [0xf5, 0x9e, 0x0b]];
        let staged: Vec<_> = colors
            .iter()
            .enumerate()
            .map(|(ix, base)| {
                let image = image::RgbImage::from_fn(96, 96, |x, y| {
                    let shade = |c: u8, d: u32| (c as u32 * (160 + d) / 255).min(255) as u8;
                    image::Rgb([
                        shade(base[0], x),
                        shade(base[1], y),
                        shade(base[2], (x + y) / 2),
                    ])
                });
                let mut bytes = Vec::new();
                image::DynamicImage::ImageRgb8(image)
                    .write_to(
                        &mut std::io::Cursor::new(&mut bytes),
                        image::ImageFormat::Png,
                    )
                    .unwrap();
                let mut att = super::attachments::stage_clipboard_image(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    bytes,
                ));
                att.id = format!("fixture-att-{ix}");
                att.name = format!("screenshot-{}.png", ix + 1);
                att
            })
            .collect();
        self.attachments
            .entry(self.current_key.clone())
            .or_default()
            .extend(staged);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn browser_annotations_focus_composer_and_edit_without_losing_other_notes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, handle) = super::super::tests::composer_focus_window(cx);
        handle.update(cx, |composer, window, cx| {
            composer.focus_pending = false;
            let other = crate::context_items::BrowserAnnotation::from_selection("https://example.com", &serde_json::json!({"comment":"Other draft", "elements":[{"tag":"button","selector":"button#other","text":"Other"}]}).to_string()).unwrap();
            composer.stage_browser_annotation("another-draft".into(), crate::context_items::ContextItem::from_browser_annotation(other), cx);
            assert!(!composer.focus_pending, "Another draft must not steal focus");
            for comment in ["First browser note", "Second browser note"] {
                let annotation = crate::context_items::BrowserAnnotation::from_selection("https://example.com", &serde_json::json!({"comment":comment,"elements":[{"tag":"button","selector":"button#save","text":"Save"}]}).to_string()).unwrap();
                composer.stage_browser_annotation(composer.current_key.clone(), crate::context_items::ContextItem::from_browser_annotation(annotation), cx);
            }
            assert!(composer.focus_pending, "Staging a browser comment must focus its composer");
            let contexts = composer.staged_contexts(cx);
            assert_eq!(contexts.len(), 2);
            let key = composer.current_key.clone();
            composer.state.update(cx, |state, _| {
                state.add_review_comment(&key, crate::comments::ReviewComment::new("src/app.rs", crate::comments::CommentSide::New, 4, "Code note"));
            });
            assert_eq!(composer.staged_note_count(cx), 3);
            let id = contexts[0].id.clone();
            composer.begin_annotation_edit(id.clone(), "First browser note".into(), window, cx);
            composer.tray.annotations.editing.as_ref().unwrap().input.update(cx, |input, cx| input.set_text("Updated browser note", cx));
            composer.commit_annotation_edit(cx);
            let contexts = composer.staged_contexts(cx);
            assert!(contexts[0].body.contains("Updated browser note"));
            assert!(!contexts[0].body.contains("First browser note"));
            assert!(contexts[1].body.contains("Second browser note"));
            assert_eq!(composer.staged_comments(cx).len(), 1);
            // The edit field may outlive an item removed or sent meanwhile.
            composer.begin_annotation_edit(id.clone(), "Stale edit".into(), window, cx);
            composer.state.update(cx, |state, _| state.remove_context_item(&key, &id));
            composer.commit_annotation_edit(cx);
            assert_eq!(composer.staged_contexts(cx).len(), 1);
        }).unwrap();
    }

    #[test]
    fn configuration_notice_title_never_relabels_an_actual_or_foreign_send_failure() {
        let notice = ("chat".to_owned(), "Read configuration failed".to_owned());
        assert!(is_configuration_notice(
            "Read configuration failed",
            Some("chat"),
            Some(&notice)
        ));
        assert!(!is_configuration_notice(
            "Actual send failed",
            Some("chat"),
            Some(&notice)
        ));
        assert!(!is_configuration_notice(
            "Read configuration failed",
            Some("different-chat"),
            Some(&notice)
        ));
        assert!(!is_configuration_notice(
            "Read configuration failed",
            None,
            Some(&notice)
        ));
        assert!(!is_configuration_notice(
            "Actual send failed",
            Some("chat"),
            None
        ));
    }

    fn entry(id: &str, role: MessageRole, parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn error(message: &str) -> MessagePart {
        MessagePart::Error {
            id: "e".into(),
            message: message.into(),
        }
    }

    fn text(text: &str) -> MessagePart {
        MessagePart::Text {
            id: "t".into(),
            text: text.into(),
        }
    }

    #[test]
    fn provider_errors_classify_by_their_wording() {
        use ProviderErrorKind::*;
        for (message, kind) in [
            ("You've hit your usage limit. Resets at 4pm", UsageLimit),
            ("API Error: 429 rate_limit_error", UsageLimit),
            ("Not logged in · Please run /login", SignIn),
            ("Invalid API key · Fix external API key", SignIn),
            ("Connection lost, reconnecting (2/5)", Reconnecting),
            ("The agent exited with status 1", Generic),
        ] {
            assert_eq!(classify_provider_error(message), kind, "{message}");
        }
    }

    #[test]
    fn only_an_error_ending_the_last_agent_turn_reaches_the_tray() {
        let failed = entry(
            "a",
            MessageRole::Assistant,
            vec![text("partial"), error("boom")],
        );
        assert_eq!(
            latest_transcript_error(std::slice::from_ref(&failed)),
            Some(("a#e".into(), "boom".into()))
        );
        // A later prompt supersedes it.
        let next = entry("u", MessageRole::User, vec![text("try again")]);
        assert_eq!(latest_transcript_error(&[failed, next]), None);
        let clean = entry("b", MessageRole::Assistant, vec![text("done")]);
        assert_eq!(latest_transcript_error(&[clean]), None);
    }

    #[test]
    fn retry_resends_the_last_prompt_text() {
        let entries = [
            entry("u1", MessageRole::User, vec![text("first")]),
            entry("a1", MessageRole::Assistant, vec![text("ok")]),
            entry("u2", MessageRole::User, vec![text("second")]),
            entry("a2", MessageRole::Assistant, vec![error("boom")]),
        ];
        assert_eq!(last_prompt_text(&entries).as_deref(), Some("second"));
        assert_eq!(last_prompt_text(&entries[1..2]), None);
    }
}
