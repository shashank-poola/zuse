//! The app shell (zuse `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is zuse's: collapsible drag-resizable sidebar (224–400px, default
//! 256) with a gentle 180–240ms width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360px floor, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use crate::localization::message as tr;
use crate::tooltip::TooltipExt as _;
use std::path::PathBuf;
use std::time::Duration;
use zuse_i18n::messages as msg;

use chrono::{DateTime, Utc};
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, Empty, Entity, FocusHandle, Focusable as _,
    IntoElement, KeyBinding, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseUpEvent, NavigationDirection, Pixels, Point, Render, SharedString, Subscription, Task,
    Window, WindowControlArea, actions, div, prelude::*, px,
};

use gpui_tokio::Tokio;
use zuse_engine::InstanceLock;
#[cfg(test)]
use zuse_protocol::HarnessId;
use zuse_protocol::{AuthState, WorkspaceScope};
use zuse_rpc::methods;

use crate::changes::{Changes, ChangesEvent, DiscardWorkingTreeRequest};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::files::{
    FileColumn, FilesCloseDisposition, FilesEvent, FilesSurface, WorkspacePathDrag,
};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{
    self, AnimationExt as _, MotionSpec, PANEL_RESIZE, RESIZE, SPLASH_OUT, TAB_SLIDE,
};
use crate::popover::{self, Loadable};
use crate::pull_requests::PullRequestsPage;
use crate::rail;
use crate::settings::accounts::AccountsPage;
use crate::settings::appearance::{AppearancePage, AppearanceSettingsEvent};
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::files::{FilesSettingsEvent, FilesSettingsPage};
use crate::settings::harnesses::HarnessesPage;
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::{
    self, CHAT_PANEL_MIN, ComposerSendBehavior, FILES_PANEL_MIN, JUMP_SLOTS, KeymapConfig,
    RIGHT_PANE_MIN, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, SavePolicy, ShortcutId,
    SidebarOrganization, SidebarSort, TERMINAL_DEFAULT_HEIGHT, TERMINAL_MAX_VH,
    TERMINAL_MIN_HEIGHT, UiSettings, badge_combo, jump_hints_visible, modifier_send_hint_visible,
    platform_combo, sidebar_pin_profile_key,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, EngineMode, GatePhase, Indicator, OrgRow,
    format_time_ago, org_name_valid, parse_orgs, sort_memberships,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::Theme;
use crate::transcript::{self, ForkTarget, Transcript, TranscriptEvent};

mod actions_ui;
mod chat_dropzone;
#[cfg(test)]
mod chat_dropzone_tests;
mod chat_pull_requests;
#[cfg(test)]
mod chat_rename_tests;
mod chat_shortcuts;
mod command_palette;
mod file_mutations;
mod files_panel;
mod harness_updates;
mod keyboard_commands;
mod main_tabs;
mod navigation_focus;
#[cfg(test)]
mod navigation_tests;
mod open_in;
mod panel_layout;
mod panel_menu;
mod pr_watch;
mod project_setup;
mod update_notes;
mod update_restart;
use panel_layout::{AuxiliaryPanel, FitInputs, HorizontalPanelFit, panel_content_tween};
mod archives;
#[cfg(test)]
mod panel_interaction_tests;
pub(crate) mod project_icon;
mod pull_request_pane;
mod search_warm;
mod session_info;
mod settings_cloud;
mod side_chats;
mod sidebar_controls;
mod sidebar_pins;
mod sidebar_sections;
pub(crate) mod spaces;
mod summary_pull_request;
mod thread_search;
mod usage_dashboard;
mod workspace_layout;
mod workspace_storage;
mod zot_forms;
mod zots;
use side_chats::SideChatTab;
mod cloud_conversation;
mod cloud_info;
mod cloud_sidebar;
mod cloud_start;
mod cloud_tabs;
mod tabs;
mod top_bar;
mod voice_stage;
use voice_stage::VOICE_STAGE_ORB_SCALE;

use spaces::{AddSpaceFlow, RenameSpaceDialog};

/// `connected` already includes the engine's degradation grace. A brief
/// focus-triggered dial needs no sidebar status; queued changes or a sustained
/// outage still deserve one.
fn chat_sync_pill_caption(chat: &zuse_protocol::ChatConnectivity) -> Option<&'static str> {
    use zuse_protocol::ChatSyncState as S;
    let sustained_or_queued = !chat.connected || chat.pending_pushes > 0;
    match chat.sync_state {
        S::Waiting if sustained_or_queued => Some("Sync queued — changes are saved"),
        S::Connecting if sustained_or_queued => Some("Syncing…"),
        S::Offline if !chat.connected => Some("Offline — changes are saved"),
        _ => None,
    }
}

actions!(
    shell,
    [
        SaveFile,
        RandomWallpaper,
        ToggleSidebar,
        ToggleChanges,
        ToggleFiles,
        AddSpacePalette,
        ToggleCommandPalette,
        OpenModelPicker,
        NewSession,
        OpenSettings,
        OpenDevices,
        OpenCloudSettings,
        NextSession,
        PrevSession,
        ArchiveSession
    ]
);

/// Restore a default focus only after an in-flight handoff has had a frame to
/// claim the window. A synchronous focus-lost fallback can otherwise steal
/// focus from controls that are mounting in response to the same input event.
#[cfg(test)]
pub(crate) fn restore_focus_if_empty_on_next_frame<T: 'static>(
    focus: FocusHandle,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    window.on_next_frame(move |window, cx| {
        if window.focused(cx).is_none() {
            window.focus(&focus, cx);
        }
    });
    cx.notify();
}

/// Check the completed dispatch tree, not just the lifetime of the focused
/// handle: a hidden editor can stay alive after its element has unmounted.
pub(crate) fn restore_mounted_focus(
    root: &FocusHandle,
    preferred: &FocusHandle,
    unfocused: &FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let preferred_mounted = root.contains(preferred, window);
    if !root.contains_focused(window, cx) || (root.is_focused(window) && preferred_mounted) {
        // Explicit blur keeps shortcuts active without returning the caret to
        // an input. The root remains the temporary fallback for stale handles.
        let target = if window.focused(cx).is_none() {
            unfocused
        } else if preferred_mounted {
            preferred
        } else {
            root
        };
        window.focus(target, cx);
    }
}

/// Tab groups order their descendants but do not trap focus. The trailing
/// non-tab-stop handle gives reverse traversal a stable end boundary even
/// when a page adds or removes controls.
fn move_settings_focus(
    start: &FocusHandle,
    end: &FocusHandle,
    reverse: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if reverse {
        window.focus_prev(cx);
    } else {
        window.focus_next(cx);
    }
    if !start.contains_focused(window, cx) {
        if reverse {
            window.focus(end, cx);
            window.focus_prev(cx);
        } else {
            window.focus(start, cx);
            window.focus_next(cx);
        }
    }
}

/// Whether the chat menu's Copy submenu is open beside it.
#[derive(Clone, Copy, PartialEq)]
enum ChatMenuPage {
    Root,
    Copy,
}

/// Hover-intent keys for the chat menu's rows; only Copy owns a submenu.
/// Height of the new-thread arcade band under the titlebar.
const NEW_THREAD_ARCADE_HEIGHT: f32 = 200.0;
/// Shorter windows have no empty space above the heading for the band.
const NEW_THREAD_ARCADE_MIN_WINDOW: f32 = 640.0;
const CHAT_MENU_ROW_RENAME: u8 = 0;
const CHAT_MENU_ROW_PIN: u8 = 1;
const CHAT_MENU_ROW_ARCHIVE: u8 = 2;
const CHAT_MENU_ROW_COPY: u8 = 3;
const CHAT_MENU_ROW_DELETE: u8 = 4;
const CHAT_MENU_ROW_TAB: u8 = 5;

#[derive(Clone, Copy)]
enum TabCloseAction {
    This,
    Others,
    Left,
    Right,
}

#[derive(Clone)]
struct ChatMenuState {
    // Empty for surfaces that have no underlying chat.
    chat_id: String,
    tab: Option<(String, RightSurface)>,
    position: Point<Pixels>,
    page: ChatMenuPage,
}

pub(super) use crate::motion::DisclosureMotion as SidebarDisclosureMotion;

/// Vertical pane resize hitboxes yield the global titlebar. Keeping this in
/// the shared constructor makes left/right seams mirror each other and avoids
/// relying on paint order when chrome crosses an animated pane boundary.
const PANE_RESIZE_HITBOX_HALF_WIDTH: f32 = 10.0;
const PANE_RESIZE_HITBOX_TOP: f32 = Theme::TITLEBAR_HEIGHT;
/// Corner radius of the floating Linux CSD window (macOS gets its native
/// curve from the platform; maximized/tiled Linux windows go square).
/// Chrome layers that paint full-bleed at a window edge round their own
/// backgrounds with it — see [`Shell::window_corner_radius`].
pub(crate) const LINUX_WINDOW_CORNER_RADIUS: f32 = 10.0;
const TERMINAL_RESIZE_HITBOX_HEIGHT: f32 = 10.0;

fn stable_panel_content_width(target: f32, transition: Option<(f32, f32)>) -> f32 {
    transition.map(|(from, to)| from.max(to)).unwrap_or(target)
}

fn right_panel_content_width(
    target: f32,
    transition: Option<(f32, f32)>,
    takeover_width: Option<f32>,
) -> f32 {
    takeover_width.unwrap_or_else(|| stable_panel_content_width(target, transition))
}

fn conversation_width(viewport: f32, sidebar: f32, right: f32) -> f32 {
    (viewport - sidebar - right).max(0.0)
}

/// The chat's working directory as the host device spells it — the folder the
/// harness runs in (a worktree chat's worktree). Projectless `~` chats have
/// none. Deliberately not `source_context.repo_root`: that is canonicalized
/// (symlinks resolved, `\\?\` verbatim prefix on Windows hosts).
fn chat_copy_path(chat: &zuse_protocol::Chat) -> Option<&str> {
    chat.cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| is_host_absolute_path(cwd))
}

/// Absolute on the HOST, whatever the viewer's OS: a remote engine may hand a
/// POSIX path to a Windows viewport (no drive, so `Path::is_absolute` says
/// no) or a drive path to a POSIX one.
fn is_host_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

fn composer_target_width(panel_width: f32, content_width: f32, docked: bool) -> f32 {
    let panel_width = panel_width.max(0.0);
    if !docked {
        return panel_width.min(crate::composer::COMPOSER_MAX_WIDTH);
    }
    // Share the configurable maximum, including the composer's outer padding.
    // Below that maximum, keep the original full-panel responsive width: using
    // transcript gutters here would remove 64px and wrap attachments too early.
    (content_width + 2.0 * crate::composer::COMPOSER_OUTER_PAD).min(panel_width)
}

fn titlebar_new_session_alpha(route: &Route, has_selected_chat: bool) -> f32 {
    if matches!(route, Route::PullRequests) || (matches!(route, Route::Chat) && has_selected_chat) {
        1.0
    } else {
        0.0
    }
}

/// Open the session at `slot` (zero-based) of the sidebar's active list. One
/// action carrying the slot, rather than nine near-identical action types.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct JumpSession(pub usize);

#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct DesktopCommand(pub settings::keybindings::Command);

/// Show the command palette's tab at this (zero-based) position. Bound to
/// mod-1…4 inside the palette's key context only, where it outranks the
/// session jumps on the same keys while the palette is open.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = shell, no_json)]
pub struct SelectPaletteTab(pub usize);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (zuse window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Within-group rhythm for Back/Forward.
pub const TITLEBAR_CONTROL_GAP: f32 = 2.0;
/// Structural separation between titlebar groups: sidebar, navigation,
/// transcript identity, and trailing actions.
pub const TITLEBAR_GROUP_GAP: f32 = Theme::SPACE_SM;
/// Breathing room between the navigation cluster and transcript identity.
pub const TITLEBAR_IDENTITY_GAP: f32 = Theme::SPACE_MD;
/// A 28px action centered in the 38px titlebar with its 2px downward optical
/// shift lands 6px from the top; use the same inset at the trailing edge.
pub const TITLEBAR_ACTION_EDGE_INSET: f32 = 6.0;
/// Width of the persistent top-left button cluster itself: a 24px sidebar
/// trigger, an 8px group gap, then two 24px history buttons on a 2px rhythm.
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + TITLEBAR_GROUP_GAP + TITLEBAR_CONTROL_GAP;
/// Extra width consumed when the collapsed-sidebar New Session action joins
/// the left controls as its own group.
pub const TITLEBAR_ACTION_SLOT_WIDTH: f32 = TITLEBAR_GROUP_GAP + 24.0;
/// Horizontal inset owned by the titlebar control row itself. Keep this value
/// paired with [`Self::titlebar_spacer`]: using a different number for the
/// spacer shifts every control while leaving the declared cluster geometry
/// unchanged.
const TITLEBAR_CLUSTER_PAD: f32 = 10.0;

/// Width of a row of `count` Linux caption buttons, drawn at the cluster's
/// own 24px-button / 2px-gap rhythm.
pub fn caption_buttons_width(count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    count as f32 * 24.0 + (count as f32 - 1.0) * 2.0
}

/// Where the cluster's first button starts, from the window's left edge.
/// `linux_left_captions` is the number of caption buttons zuse draws at the
/// top-left on Linux (GNOME `close:…` layouts) — the app cluster follows them
/// at the shared 2px rhythm.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool, linux_left_captions: usize) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else if linux_left_captions > 0 {
        10.0 + caption_buttons_width(linux_left_captions) + 2.0
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(
    is_macos: bool,
    fullscreen: bool,
    linux_left_captions: usize,
    container_pad: f32,
) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen, linux_left_captions) + CLUSTER_BUTTONS_WIDTH + 8.0
        - container_pad)
        .max(0.0)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(
    cx: &mut App,
    keymap: &KeymapConfig,
    composer_send_behavior: ComposerSendBehavior,
) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    crate::appshots::set_shortcut(&keymap.capture_appshot);
    cx.clear_key_bindings();
    // `clear_key_bindings` also removes the contextual editing actions that
    // gpui-base installed at startup. Reinitialize the component layer before
    // rebuilding Zuse's bindings so the file editor keymap remains active.
    gpui_base::init(cx);
    crate::composer::init_with_keymap(cx, composer_send_behavior, keymap);
    cx.bind_keys([KeyBinding::new(
        &valid_or_default(
            &keymap.toggle_dictation,
            ShortcutId::ToggleDictation.default_combo(),
        ),
        crate::composer::ToggleDictation,
        Some("MessageComposer"),
    )]);
    // Fixed app-level shortcuts (Settings on every platform; ⌘Q quit, ⌘W
    // close, ⌘M minimize, ⌘H hide on macOS) — these back the native menu
    // key equivalents and must survive keymap re-application.
    crate::app_menus::bind_keys(cx);
    // Native-only additions remain available alongside the desktop command contract.
    for id in [
        ShortcutId::RandomWallpaper,
        ShortcutId::ToggleFiles,
        ShortcutId::ArchiveSession,
    ] {
        let combo = platform_combo(keymap.get(id));
        if combo.is_empty() || Keystroke::parse(&combo).is_err() {
            continue;
        }
        let binding = match id {
            ShortcutId::RandomWallpaper => KeyBinding::new(&combo, RandomWallpaper, None),
            ShortcutId::ToggleFiles => KeyBinding::new(&combo, ToggleFiles, None),
            _ => KeyBinding::new(&combo, ArchiveSession, None),
        };
        cx.bind_keys([binding]);
    }
    crate::browser::bind_keys(cx, keymap);
    for slot in 0..JUMP_SLOTS {
        let combo = platform_combo(keymap.get(ShortcutId::JumpSession(slot)));
        if !combo.is_empty() && Keystroke::parse(&combo).is_ok() {
            cx.bind_keys([KeyBinding::new(&combo, JumpSession(slot), None)]);
        }
    }
    keyboard_commands::bind(cx, keymap);
    // Context-free bindings (the jumps above) match at the full focus depth,
    // so a palette binding must too: one for the palette's focused search
    // input, one for the card itself. Bound after the jumps, they win the tie.
    let palette_input = format!("{} > PaletteSearch", command_palette::KEY_CONTEXT);
    cx.bind_keys((0..command_palette::Tab::ALL.len()).flat_map(|ix| {
        let combo = platform_combo(&format!("mod-{}", ix + 1));
        [
            KeyBinding::new(&combo, SelectPaletteTab(ix), Some(&palette_input)),
            KeyBinding::new(
                &combo,
                SelectPaletteTab(ix),
                Some(command_palette::KEY_CONTEXT),
            ),
        ]
    }));
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    /// Cloud computer → Overview (slug `cloud`, the historical Cloud page).
    Cloud,
    CloudRepositories,
    CloudEnvironments,
    CloudMachine,
    /// Agent subscriptions cloud tasks sign in with.
    CloudAgents,
    /// Model usage on this computer's agents, plus Cloud compute.
    Usage,
    /// The Cloud plan, money left, credits and payment.
    Billing,
    Devices,
    /// Which harnesses the composer offers (enable/disable toggles) —
    /// labeled "Providers".
    Harnesses,
    Defaults,
    Plugins,
    /// Per-provider CLI accounts (login, usage) — labeled "Accounts".
    Agents,
    Appearance,
    Files,
    Notifications,
    Voice,
    Shortcuts,
    /// Composer and conversation behavior plus thread naming.
    #[default]
    General,
    Appshots,
    Archived,
}

impl SettingsSection {
    /// Sections shown in Settings. `Agents` is a legacy Accounts route alias.
    /// Grouped by where things run: agents on this computer, the cloud
    /// computer, what it all costs, then the app itself. See [`Self::heading`].
    pub const ALL: [SettingsSection; 19] = [
        SettingsSection::General,
        SettingsSection::Harnesses,
        SettingsSection::Defaults,
        SettingsSection::Plugins,
        SettingsSection::Cloud,
        SettingsSection::CloudRepositories,
        SettingsSection::CloudEnvironments,
        SettingsSection::CloudMachine,
        SettingsSection::CloudAgents,
        SettingsSection::Usage,
        SettingsSection::Billing,
        SettingsSection::Appearance,
        SettingsSection::Notifications,
        SettingsSection::Voice,
        SettingsSection::Shortcuts,
        SettingsSection::Files,
        SettingsSection::Devices,
        SettingsSection::Appshots,
        SettingsSection::Archived,
    ];

    /// The sidebar heading this section opens, if it starts a group.
    fn heading(self) -> Option<&'static str> {
        match self {
            SettingsSection::Harnesses => Some("Local agents"),
            SettingsSection::Cloud => Some("Cloud computer · Beta"),
            SettingsSection::Usage => Some("Plan & usage"),
            SettingsSection::Appearance => Some("App"),
            _ => None,
        }
    }

    /// The Cloud page a Cloud section shows.
    fn cloud_view(self) -> Option<settings::cloud_billing::CloudView> {
        use settings::cloud_billing::CloudView;
        Some(match self {
            SettingsSection::Cloud => CloudView::Overview,
            SettingsSection::CloudRepositories => CloudView::Repositories,
            SettingsSection::CloudEnvironments => CloudView::Environments,
            SettingsSection::CloudMachine => CloudView::Machine,
            SettingsSection::CloudAgents => CloudView::Agents,
            SettingsSection::Billing => CloudView::Billing,
            _ => return None,
        })
    }

    fn from_cloud_view(view: settings::cloud_billing::CloudView) -> Self {
        use settings::cloud_billing::CloudView;
        match view {
            CloudView::Overview => SettingsSection::Cloud,
            CloudView::Repositories => SettingsSection::CloudRepositories,
            CloudView::Environments => SettingsSection::CloudEnvironments,
            CloudView::Machine => SettingsSection::CloudMachine,
            CloudView::Agents => SettingsSection::CloudAgents,
            CloudView::Compute => SettingsSection::Usage,
            CloudView::Billing => SettingsSection::Billing,
        }
    }

    /// The former Accounts page is folded into Providers. Keep its route alias
    /// for old navigation entries and dev links without showing a hidden tab.
    fn canonical(self) -> Self {
        if self == Self::Agents {
            Self::Harnesses
        } else {
            self
        }
    }

    fn visible_in_nav(self) -> bool {
        self != Self::Agents
            && (self != Self::Appshots || crate::appshots::is_desktop())
            && (!self.cloud_computer() || crate::cloud_beta())
    }

    /// The Cloud computer group, shown only while cloud computers are in beta
    /// here.
    fn cloud_computer(self) -> bool {
        matches!(
            self,
            Self::Cloud
                | Self::CloudRepositories
                | Self::CloudEnvironments
                | Self::CloudMachine
                | Self::CloudAgents
        )
    }

    /// Where a generic "open Settings" lands for a remembered section: legacy
    /// aliases resolve to their page, and a section this build does not show
    /// (Appshots off macOS/Linux) falls back to General.
    pub(crate) fn reopenable(self) -> Self {
        let section = self.canonical();
        if section.visible_in_nav() {
            section
        } else {
            Self::General
        }
    }

    /// Stable name shared by `ui-settings.json` and `ZUSE_OPEN_ROUTE`.
    fn slug(self) -> &'static str {
        match self {
            SettingsSection::Cloud => "cloud",
            SettingsSection::CloudRepositories => "cloud-repositories",
            SettingsSection::CloudEnvironments => "cloud-environments",
            SettingsSection::CloudMachine => "cloud-machine",
            SettingsSection::CloudAgents => "cloud-agents",
            SettingsSection::Usage => "usage",
            SettingsSection::Billing => "billing",
            SettingsSection::Devices => "devices",
            SettingsSection::Harnesses => "providers",
            SettingsSection::Defaults => "defaults",
            SettingsSection::Plugins => "plugins",
            SettingsSection::Agents => "agents",
            SettingsSection::Appearance => "appearance",
            SettingsSection::Files => "files",
            SettingsSection::Notifications => "notifications",
            SettingsSection::Voice => "voice",
            SettingsSection::Shortcuts => "shortcuts",
            SettingsSection::General => "general",
            SettingsSection::Appshots => "appshots",
            SettingsSection::Archived => "archived",
        }
    }

    /// Inverse of [`Self::slug`], plus the pages' former names.
    fn from_slug(slug: &str) -> Option<Self> {
        Some(match slug {
            "cloud" => SettingsSection::Cloud,
            "cloud-repositories" | "repositories" => SettingsSection::CloudRepositories,
            "cloud-environments" | "environments" => SettingsSection::CloudEnvironments,
            "cloud-machine" | "machine" => SettingsSection::CloudMachine,
            "cloud-agents" => SettingsSection::CloudAgents,
            "usage" => SettingsSection::Usage,
            "billing" => SettingsSection::Billing,
            "devices" => SettingsSection::Devices,
            "providers" | "harnesses" => SettingsSection::Harnesses,
            "defaults" | "default-models" => SettingsSection::Defaults,
            "plugins" => SettingsSection::Plugins,
            "agents" => SettingsSection::Agents,
            "appearance" => SettingsSection::Appearance,
            "files" => SettingsSection::Files,
            "notifications" => SettingsSection::Notifications,
            "voice" => SettingsSection::Voice,
            "shortcuts" => SettingsSection::Shortcuts,
            "general" | "conversations" => SettingsSection::General,
            "appshots" => SettingsSection::Appshots,
            "archived" => SettingsSection::Archived,
            _ => return None,
        })
    }

    /// Sidebar + header label (zuse settings-sidebar.tsx SECTIONS / __root.tsx
    /// `settingsTitle` — the same strings in both places).
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Cloud => "Overview",
            SettingsSection::CloudRepositories => "Repositories",
            SettingsSection::CloudEnvironments => "Environments",
            SettingsSection::CloudMachine => "Machine",
            SettingsSection::CloudAgents => "Agent accounts",
            SettingsSection::Usage => "Usage",
            SettingsSection::Billing => "Billing",
            SettingsSection::Devices => "Remote access",
            SettingsSection::Harnesses => "Providers & accounts",
            SettingsSection::Defaults => "Default models",
            SettingsSection::Plugins => "Plugins & MCP",
            SettingsSection::Agents => "Accounts",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Files => "Files",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::Voice => "Voice",
            SettingsSection::Shortcuts => "Keyboard shortcuts",
            SettingsSection::General => "General",
            SettingsSection::Appshots => "Appshots",
            SettingsSection::Archived => "Archived chats",
        }
    }
}

/// Persisted as [`crate::settings::UiSettings::settings_section`].
impl serde::Serialize for SettingsSection {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.slug())
    }
}

/// Lenient: an unknown or malformed value reads as General instead of
/// failing — and so defaulting — the whole settings file.
impl<'de> serde::Deserialize<'de> for SettingsSection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(value.as_str().and_then(Self::from_slug).unwrap_or_default())
    }
}

/// The section a `ZUSE_OPEN_ROUTE` value opens: bare `settings` reopens the
/// remembered section, `settings/<slug>` names one (and so becomes the
/// remembered one). `None` for anything else, including unknown slugs.
fn settings_open_route(route: &str, remembered: SettingsSection) -> Option<SettingsSection> {
    if route == "settings" {
        return Some(remembered.reopenable());
    }
    route
        .strip_prefix("settings/")
        .and_then(SettingsSection::from_slug)
        .map(SettingsSection::canonical)
}

/// What the main outlet shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Chat,
    PullRequests,
    /// The plugin catalog, a main-area page beside the sidebar.
    Plugins,
    Archives,
    Usage,
    /// The New Zot page; [`zots::ZotUi`] holds the form.
    Zot,
    Settings(SettingsSection),
}

/// Maximum width the right pane may occupy while retaining the conversation
/// floor. The horizontal fit policy hides a lower-priority column before this
/// budget can fall below the right pane's minimum at rest.
fn right_pane_max_width(viewport: f32, sidebar: f32, chat_floor: f32) -> f32 {
    (viewport - sidebar - chat_floor).max(0.0)
}

/// Width used by right-pane takeover. Unlike manual resizing, takeover is
/// intentionally allowed to consume the conversation column completely.
fn right_pane_takeover_width(viewport: f32, sidebar: f32) -> f32 {
    (viewport - sidebar).max(0.0)
}

/// One right-pane surface tab: a workspace browser, an individual workspace
/// file editor, a Git diff or history page, an embedded terminal, or a
/// subagent transcript. `Picker` is the empty surface chooser.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RightSurface {
    #[default]
    Picker,
    File(u64),
    Browser(u64),
    Simulator(u64),
    Diff(u64),
    Terminal(u64),
    /// A subagent's transcript, read-only (per-subagent viz) — the handle
    /// keys [`Shell::subagent_tabs`].
    Subagent(u64),
    SideChat(u64),
    /// A pull request opened from the chat — keys [`Shell::chat_pull_requests`].
    PullRequest(u64),
    /// The chat's file tree (one per panel) — keys [`Shell::files`].
    Files,
}

/// Stage `prompt` in `composer` without sending it, after the composer has
/// swapped to its session's draft (which runs on a state observation the
/// caller just queued). A draft already waiting there stays, above it.
fn stage_prompt(
    composer: Entity<Composer>,
    prompt: String,
    context: Option<crate::context_items::ContextItem>,
    cx: &mut App,
) {
    cx.defer(move |cx| {
        composer.update(cx, |composer, cx| {
            if let Some(context) = context {
                composer.stage_context(context, cx);
            }
            composer.input.update(cx, |input, cx| {
                let draft = input.text().trim_end();
                let text = if draft.is_empty() {
                    prompt
                } else {
                    format!("{draft}\n\n{prompt}")
                };
                input.set_text(text, cx)
            });
            composer.focus_pending = true;
            cx.notify();
        });
    });
}

fn push_unique_right_surface(tabs: &mut Vec<RightSurface>, surface: RightSurface) -> bool {
    if tabs.contains(&surface) {
        false
    } else {
        tabs.push(surface);
        true
    }
}

fn workspace_file_title(path: &str) -> SharedString {
    path.rsplit('/').next().unwrap_or(path).to_string().into()
}

/// Per-chat panel open flags (zuse parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global).
///
/// Everything defaults CLOSED — the right pane included (user request,
/// revising the earlier default-open: it popped open on every session you
/// visited). Opening is an explicit act, remembered per chat for the rest of
/// the app run; a fresh open with no surface tabs lands on the picker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    /// The explorer portion of the right pane is docked.
    pub files_open: bool,
    files_opened_at: u64,
    pub terminal_open: bool,
    /// The surface host portion of the right pane is visible (historically
    /// the Changes pane). The pane itself shows when either portion does.
    pub changes_open: bool,
    /// Takeover is remembered with its owning chat, not inherited by the
    /// next conversation (or by the new-thread canvas).
    right_expanded: bool,
    /// Opening order, which the window fit ranks columns by. Zero predates
    /// an explicit open.
    changes_opened_at: u64,
    /// Which surface tab renders; validated against the live tab list each
    /// frame (a closed tab falls back gracefully).
    pub right_active: RightSurface,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
    /// Unique surface visits, oldest first, independent of the strip order.
    right_tab_history: std::collections::HashMap<String, Vec<RightSurface>>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }

    /// Mutate `key`'s flags in place (right-pane surface bookkeeping).
    pub fn update(&mut self, key: &str, f: impl FnOnce(&mut ChatPanels)) {
        let panel = self.map.entry(key.to_string()).or_default();
        let previous = panel.right_active;
        f(panel);
        let surface = panel.right_active;
        if surface != previous && surface != RightSurface::Picker {
            let history = self.right_tab_history.entry(key.to_string()).or_default();
            history.retain(|visited| *visited != surface);
            history.push(surface);
        }
    }

    fn forget_right_surface(&mut self, key: &str, surface: RightSurface) {
        if let Some(history) = self.right_tab_history.get_mut(key) {
            history.retain(|visited| *visited != surface);
            if history.is_empty() {
                self.right_tab_history.remove(key);
            }
        }
    }

    /// Resolve against live surfaces so stale terminal/entity ids are skipped.
    /// The strip's first tab remains the fallback for surfaces never visited.
    fn right_surface_fallback(
        &self,
        key: &str,
        mut available: impl Iterator<Item = RightSurface>,
    ) -> RightSurface {
        let Some(first) = available.next() else {
            return RightSurface::Picker;
        };
        let Some(history) = self.right_tab_history.get(key) else {
            return first;
        };
        let live: std::collections::HashSet<_> = std::iter::once(first).chain(available).collect();
        history
            .iter()
            .rev()
            .find(|surface| live.contains(surface))
            .copied()
            .unwrap_or(first)
    }
}

/// One route-history entry (zuse parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    PullRequests,
    /// One pull request's detail view over the board, and the device whose
    /// `gh` reads it (`None` = this device).
    PullRequest {
        url: String,
        device: Option<String>,
    },
    Settings(SettingsSection),
    Plugins,
    Archives(String),
    NewZot,
    Usage {
        project: Option<String>,
        project_scope: bool,
    },
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (zuse window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (zuse's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// The entry Back would land on.
    pub fn previous(&self) -> Option<&NavEntry> {
        self.index.checked_sub(1).map(|index| &self.entries[index])
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (zuse window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Sidebar resort glide (feature-inventory §1.6): 260ms
/// `cubic-bezier(0.22,1,0.36,1)` per-row translate, the View Transitions
/// equivalent.
pub const RESORT: MotionSpec = MotionSpec::new(260, motion::EASE_RESORT);

/// FLIP diff for a keyed list: given the previously rendered order and the new
/// order (key + row height), return each surviving key's paint-only start
/// offset `old_y - new_y` (only keys whose position actually moved). `gap` is
/// the flex gap between rows. Pure — drives the sidebar resort glide.
pub fn resort_offsets(
    old: &[(String, f32)],
    new: &[(String, f32)],
    gap: f32,
) -> std::collections::HashMap<String, f32> {
    let mut old_y = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in old {
        old_y.insert(key.as_str(), y);
        y += height + gap;
    }
    let mut offsets = std::collections::HashMap::new();
    let mut y = 0.0_f32;
    for (key, height) in new {
        if let Some(prev) = old_y.get(key.as_str()) {
            let dy = prev - y;
            if dy.abs() > 0.5 {
                offsets.insert(key.clone(), dy);
            }
        }
        y += height + gap;
    }
    offsets
}

/// Height changes do not constitute a list reorder. In particular, sidebar
/// disclosures animate their own height and must not also trigger FLIP offsets
/// on every following keyed section.
fn sidebar_key_order_changed(old: &[(String, f32)], new: &[(String, f32)]) -> bool {
    old.len() != new.len()
        || old
            .iter()
            .zip(new)
            .any(|((old_key, _), (new_key, _))| old_key != new_key)
}

/// Exact active-session row height. Harness identity lives on the title line
/// and the Working glyph lives in the status corner, so neither adds a third
/// line. Compact rows omit the metadata line and its preceding gap entirely;
/// branch / pull-request rows add the exact height of their tallest child.
/// Keeping this calculation beside the renderer's metrics prevents disclosure
/// clips when view options alter the row structure.
pub(super) fn chat_row_height(shows_branch: bool, shows_pull_request: bool) -> f32 {
    let mut metadata_height: f32 = 0.0;
    if shows_branch {
        metadata_height = metadata_height.max(14.0);
    }
    if shows_pull_request {
        metadata_height = metadata_height.max(16.0);
    }
    if metadata_height == 0.0 {
        45.0
    } else {
        47.0 + metadata_height
    }
}
/// Exact sidebar session row height. Rows nested under a project heading,
/// compact rows and rows without the project line are one line; one-list
/// rows add the project line beneath the title.
impl Shell {
    /// The Threads view's second line (project, PR, diff) is the point of the
    /// flat list, so Compact and a hidden project label only shape Projects.
    pub(crate) fn sidebar_compact_rows(&self) -> bool {
        self.settings.sidebar_compact
            && self.settings.sidebar_organization != SidebarOrganization::InOneList
    }

    pub(crate) fn sidebar_project_labels(&self) -> bool {
        self.settings.sidebar_show_project_label
            || self.settings.sidebar_organization == SidebarOrganization::InOneList
    }
}

/// Sidebar row ids for cloud tasks: `cloud:<workspace id>`.
pub(crate) const CLOUD_ROW_PREFIX: &str = "cloud:";

fn sidebar_row_height(compact: bool, show_label: bool, nested: bool) -> f32 {
    if compact || nested || !show_label {
        SIDEBAR_ROW_SINGLE_HEIGHT
    } else {
        SIDEBAR_ROW_DOUBLE_HEIGHT
    }
}
const SIDEBAR_ROW_SINGLE_HEIGHT: f32 = 28.0;
const SIDEBAR_ROW_DOUBLE_HEIGHT: f32 = 44.0;
/// Sidebar session rows round like the reference list rows.
const SIDEBAR_ROW_RADIUS: f32 = 7.0;

/// "+1.2K" style line counts for the sidebar's diff stat.
fn compact_line_count(count: u32) -> String {
    if count >= 1000 {
        let tenths = (count as f32 / 100.0).round() / 10.0;
        if tenths.fract() == 0.0 {
            format!("{}K", tenths as u32)
        } else {
            format!("{tenths:.1}K")
        }
    } else {
        count.to_string()
    }
}

/// A session's `+N −N` diff stat (10px, success / danger tones).
fn sidebar_diff_stat(additions: u32, deletions: u32, alpha: f32, theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .whitespace_nowrap()
        .text_size(crate::typography::text_xs())
        .when(additions > 0, |el| {
            el.child(
                div()
                    .text_color(theme.success.opacity(alpha))
                    .child(SharedString::from(format!(
                        "+{}",
                        compact_line_count(additions)
                    ))),
            )
        })
        .when(deletions > 0, |el| {
            el.child(
                div()
                    .text_color(theme.danger.opacity(alpha))
                    .child(SharedString::from(format!(
                        "\u{2212}{}",
                        compact_line_count(deletions)
                    ))),
            )
        })
}

/// Flex gap between sidebar list items.
const SIDEBAR_LIST_GAP: f32 = 2.0;
/// Space between the Pinned (or a custom) section and the Threads view's
/// unpinned rows, which carry no heading of their own.
const SIDEBAR_THREADS_GAP: f32 = 10.0;
/// Fixed vertical slot occupied by one active sidebar card.
#[cfg(test)]
const SIDEBAR_SESSION_SLOT: f32 = 61.0 + SIDEBAR_LIST_GAP;
const SIDEBAR_DRAG_SCROLL_BAND: f32 = 48.0;
const SIDEBAR_DRAG_SCROLL_MAX: f32 = 12.0;
const SIDEBAR_DRAG_SCROLL_FRAME_MS: u64 = 16;
const SIDEBAR_LIST_PAD_TOP: f32 = 4.0;

/// Active and archived sessions share harness/title geometry.
const SIDEBAR_ACTIVE_HARNESS_ICON_SIZE: f32 = 13.0;
/// Compact-row time slot while a text jump hint ("Ctrl+9") stands in for the
/// time: wide enough for the widest of them at 11px on one line. In
/// default-size pixels; scaled with the UI font via `ui_rems`.
const COMPACT_JUMP_HINT_WIDTH: f32 = 42.0;
const SIDEBAR_ACTIVE_HARNESS_TITLE_GAP: f32 = Theme::SPACE_SM;
/// The sidebar footer's profile and settings buttons share one hit target.
const SIDEBAR_FOOTER_BUTTON_SIZE: f32 = 28.0;
const SIDEBAR_FOOTER_AVATAR_SIZE: f32 = 18.0;

/// Keep the fade short so only the last few glyphs recede. Tracking clipped
/// content lets the shared paint-time overflow gate leave fitting labels intact.
pub(crate) fn sidebar_faded_label(
    id: SharedString,
    fill: bool,
    label: impl IntoElement,
) -> impl IntoElement {
    let overflow = gpui::ScrollHandle::new();
    crate::edge_fade::edge_faded(
        20.0,
        false,
        false,
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .when(fill, |el| el.flex_1())
            .min_w_0()
            .overflow_hidden()
            .track_scroll(&overflow)
            .flex()
            .child(div().flex_none().whitespace_nowrap().child(label)),
    )
    .fade_right(true)
    .fade_label_overflow(&overflow)
}
/// Breathing room between the account trigger and its adjacent action.
const SIDEBAR_FOOTER_ACTION_GAP: f32 = 4.0;

/// A chat row's inline title editor, swapped in for its faded title while
/// the chat is renamed in place: the title's 13/17 type in an accent-ringed
/// field.
pub(crate) fn chat_title_editor(
    id: SharedString,
    input: Entity<ComposerInput>,
    theme: &Theme,
) -> AnyElement {
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .flex_1()
        .min_w_0()
        .h(px(21.0))
        .my(px(-2.0))
        .px(px(4.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(theme.accent)
        .bg(theme.input_glass_bg())
        .text_color(theme.text)
        .cursor_text()
        // The field is not the row: clicks place the caret instead of
        // opening the chat or starting a drag.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(div().flex_1().min_w_0().overflow_hidden().child(input))
        .into_any_element()
}

/// Ramp height of the sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 24.0;

/// Target of the sidebar's "Star on GitHub" banner (same as the landing page).
const GITHUB_REPO_URL: &str = "https://github.com/opcodehq/zuse";

/// New-thread controls float over the tail of a top-anchored image hero. The
/// hero reaches below the composer, giving its lower mask room to dissolve
/// gradually into the otherwise empty lower canvas.
const NEW_THREAD_BACKGROUND_FROSTED_OPACITY: f32 = 0.84;
const NEW_THREAD_BACKGROUND_VIEWPORT_RATIO: f32 = 0.72;
const NEW_THREAD_BACKGROUND_MAX_HEIGHT: f32 = 760.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneResizeKind {
    Sidebar,
    Right,
    Terminal,
    PullRequests,
}

/// Resolve one pointer sample while keeping the persisted width legal. The
/// edge is latched by the caller, so a held pointer produces one nudge rather
/// than restarting the animation for every drag event.
fn sidebar_drag_sample(
    pointer_x: f32,
    latched_edge: Option<motion::ResizeEdge>,
    reduced_motion: bool,
) -> motion::ResizeDragSample {
    motion::resize_drag_sample(
        pointer_x,
        SIDEBAR_MIN,
        SIDEBAR_MAX,
        latched_edge,
        reduced_motion,
    )
}

/// The dragged surface-tab payload (strip reorder).
struct RightTabDrag {
    panel_key: String,
    from: usize,
    title: SharedString,
    workspace_path: Option<WorkspacePathDrag>,
}

/// Live drag-over state for the surface-tab strip — the terminal drawer's
/// [`crate::terminal::panel`] DragState, ported: `epoch` keys the 150ms
/// slide-animation restarts as the hovered slot changes.
struct RightTabDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// Sidebar-only drag payload. Regular sessions never acquire a manual order.
#[derive(Clone)]
struct SidebarSessionDrag {
    chat_id: String,
    visible_ids: std::sync::Arc<Vec<String>>,
    filter: Option<String>,
    profile_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarSessionDrop {
    Pinned(usize),
    Regular,
    Section(String),
}

struct SidebarSessionTransfer {
    payload: SidebarSessionDrag,
    origin: std::rc::Rc<std::cell::Cell<Point<Pixels>>>,
    cursor_offset: Point<Pixels>,
    pointer: Point<Pixels>,
    viewport: Option<gpui::Bounds<Pixels>>,
    /// The lifted row's window position: `slide` its top, `slide_x` its left.
    slide: SidebarSessionSlide,
    slide_x: SidebarSessionSlide,
    preview: Option<SidebarSessionGap>,
    source_group: String,
    source_index: usize,
    row_height: f32,
    source_collapse: SidebarSessionSlide,
    collapsed_height: f32,
    section_gaps: std::collections::HashMap<String, SidebarSessionSlide>,
    siblings: std::collections::HashMap<String, SidebarSessionSlide>,
}

#[derive(Clone)]
struct SidebarSessionGap {
    group: String,
    index: usize,
    pinned: bool,
    top: f32,
}

/// The same slot-to-slot easing as pinned reordering, applied to the actual row.
struct SidebarSessionSlide {
    from: f32,
    to: f32,
    epoch: u64,
    started: std::time::Instant,
}

impl SidebarSessionSlide {
    fn current(&self) -> f32 {
        let progress = TAB_SLIDE
            .progress(self.started.elapsed().as_secs_f32() / TAB_SLIDE.total().as_secs_f32());
        motion::lerp(self.from, self.to, progress)
    }

    fn retarget(&mut self, target: f32) {
        if (target - self.to).abs() < 0.5 {
            return;
        }
        self.from = self.current();
        self.to = target;
        self.epoch = self.epoch.wrapping_add(1);
        self.started = std::time::Instant::now();
    }
}

struct SidebarSessionReturn {
    transfer: SidebarSessionTransfer,
    epoch: u64,
    started: std::time::Instant,
}

/// Live destination for a pinned-session drag. The real row remains clipped
/// to the sidebar and slides between slots with its pinned siblings.
struct PinnedSessionDragState {
    chat_id: String,
    visible_ids: std::sync::Arc<Vec<String>>,
    from: usize,
    over: usize,
    prev_over: usize,
    epoch: usize,
    filter: Option<String>,
    profile_key: String,
    pointer_y: Option<f32>,
    viewport_top: f32,
    viewport_bottom: f32,
    generation: u64,
    autoscroll_active: bool,
}

type SidebarKeyedRow = (String, f32, AnyElement);

struct SidebarSessionRows {
    custom_count: usize,
    rows: Vec<SidebarKeyedRow>,
    pinned_count: usize,
    moving_row: Option<(AnyElement, f32)>,
}

/// Ghost chip following the pointer while a surface tab drags.
struct SurfaceTabGhost {
    title: SharedString,
}

impl Render for SurfaceTabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .w(px(112.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(crate::typography::text_xs())
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}

struct SurfaceTabTooltip {
    text: SharedString,
}

impl Render for SurfaceTabTooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::tooltip::chip(self.text.clone(), Theme::of(cx))
    }
}
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags and contained pinned-session reorders
/// render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween, driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
    duration: Duration,
    curve: motion::CubicBezier,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
            duration: RESIZE.total().mul_f32(motion::speed_scale()),
            curve: RESIZE.curve,
        }
    }

    /// A panel column moving from `from` to `to`. Longer moves take longer,
    /// so a wide pane travels at about the pace of the sidebar instead of
    /// covering twice the distance in the same time: 180ms up to the
    /// sidebar's width, growing with the square root of the distance to
    /// 240ms. The dialog entrance curve eases into the movement and settles
    /// without overshoot, rather than starting a large column at full speed.
    fn panel(from: f32, to: f32) -> Self {
        let scale = ((to - from).abs() / SIDEBAR_DEFAULT)
            .sqrt()
            .clamp(1.0, 4.0 / 3.0);
        Self {
            duration: PANEL_RESIZE.total().mul_f32(motion::speed_scale() * scale),
            curve: PANEL_RESIZE.curve,
            ..Self::new(from, to)
        }
    }
}

fn titlebar_island_vertical_geometry(progress: f32) -> (f32, f32) {
    // Match the padded flex row's center, not the raw titlebar center.
    // Keep the native 24px controls untouched and give them 4px of air.
    let height = 28.0 + 4.0 * progress.clamp(0.0, 1.0);
    let center = (Theme::TITLEBAR_HEIGHT + Theme::TITLEBAR_TOP_PAD) * 0.5;
    (center - height * 0.5, height)
}

fn bottom_stack_measurement_matches(
    measured_has_composer: bool,
    expected_has_composer: bool,
) -> bool {
    measured_has_composer == expected_has_composer
}

fn new_thread_background_opacity(is_frost: bool) -> f32 {
    if is_frost {
        NEW_THREAD_BACKGROUND_FROSTED_OPACITY
    } else {
        1.0
    }
}

pub(crate) fn new_thread_background_height(viewport_height: f32) -> f32 {
    (viewport_height.max(0.0) * NEW_THREAD_BACKGROUND_VIEWPORT_RATIO)
        .min(NEW_THREAD_BACKGROUND_MAX_HEIGHT)
}

fn new_thread_background(
    artwork: Option<std::sync::Arc<gpui::RenderImage>>,
    adjustment: settings::NewThreadBackgroundAdjustment,
    viewport_height: f32,
    hero_width: f32,
    composer_bounds: crate::new_thread_background_mask::SurfaceBounds,
    dissolve: f32,
    opacity: f32,
) -> AnyElement {
    let Some(artwork) = artwork else {
        return Empty.into_any_element();
    };
    let hero_height = new_thread_background_height(viewport_height);
    let dissolve = dissolve.clamp(0.0, 1.0);
    // Image and treatment share a fixed crop and fade together in place.
    // The hero uses the full conversation canvas even while the destination
    // right pane clips it. Navigation must never rescale the artwork.
    div()
        .absolute()
        .top_0()
        .left_0()
        .w(px(hero_width))
        .h(px(hero_height))
        .overflow_hidden()
        .opacity((1.0 - dissolve) * opacity)
        // Alpha resolves into the real canvas, including translucent themes;
        // no theme-colored overlay bleaches or darkens the source pixels.
        .children([false, true].into_iter().map(|cutout| {
            let artwork = artwork.clone();
            let composer_bounds = composer_bounds.clone();
            div()
                .absolute()
                .inset_0()
                .opacity(if cutout {
                    1.0
                } else {
                    crate::new_thread_background_mask::CUTOUT_REVEAL_OPACITY
                })
                .child(
                    gpui::canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _cx| {
                            if let Some(composer) = composer_bounds.get() {
                                crate::new_thread_background_mask::paint(
                                    artwork.clone(),
                                    bounds,
                                    composer,
                                    adjustment,
                                    cutout,
                                    window,
                                );
                            }
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
        }))
        .into_any_element()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// How long past a revealing disclosure's motion the sidebar keeps scrolling
/// a renamed row into view.
const CHAT_RENAME_REVEAL_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

/// Which list draws a chat's inline title editor: the sidebar's session row,
/// or a side chat's conversation tab in the titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRenameSurface {
    Sidebar,
    Tab,
}

/// A chat title edited in place on its row (double-click, or Rename in the
/// chat menu). Enter or blur commits, Escape cancels.
struct ChatRename {
    chat_id: String,
    surface: ChatRenameSurface,
    input: Entity<ComposerInput>,
    /// Focus the input on its first paint (opened without window access).
    focus_pending: bool,
    /// Until when the sidebar scrolls to keep the row in view — long
    /// enough to follow a disclosure that opened to reveal it.
    reveal_until: Option<std::time::Instant>,
    _events: Subscription,
    /// Commit-on-blur, armed once the input has taken focus.
    _blur: Option<Subscription>,
}

/// Account lifecycle owned by this process. Sign-in on a local workspace
/// flows through the in-place switch wizard (offer → switch → import → done);
/// `RestartPending` survives only as the fallback when the in-place swap
/// fails and a full quit is the safe way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncFlow {
    Idle,
    Enabling,
    Canceling,
    /// Signed in on a local runtime: the wizard's choice step (bring local
    /// work / start fresh / later). `notice_open: false` = postponed, badge
    /// in the account menu.
    SwitchOffer {
        notice_open: bool,
    },
    /// Stopping the local runtime and bootstrapping the synced one in-place.
    Switching {
        import: bool,
    },
    /// The one-time import stream is running on the new synced runtime.
    Importing {
        done: usize,
        total: usize,
    },
    /// Import finished; the success step stays until dismissed.
    ImportDone {
        imported: usize,
        skipped: usize,
    },
    /// The import stream reported errors or died early. Explicit retry step —
    /// structural idempotence makes re-running safe (only missing rows copy).
    /// Details ride `runtime_change_error`. `notice_open: false` = postponed:
    /// the dialog is hidden but the failure stays pending, reachable through
    /// the account menu — dismissal must never discard the only retry
    /// entry point (under Synced scope the menu otherwise offers just
    /// Sign out, and the local rows would be unreachable).
    ImportFailed {
        notice_open: bool,
    },
    RestartPending {
        notice_open: bool,
    },
    SignOutConfirm,
    SigningOut,
    SignedOutRestartRequired,
}

impl SyncFlow {
    /// States the in-place switch driver owns end-to-end — auth/scope edges
    /// must not reset them while the runtime is being replaced under the UI.
    fn is_switch_lifecycle(self) -> bool {
        matches!(
            self,
            SyncFlow::Switching { .. }
                | SyncFlow::Importing { .. }
                | SyncFlow::ImportDone { .. }
                | SyncFlow::ImportFailed { .. }
        )
    }

    fn has_visible_overlay(self) -> bool {
        match self {
            SyncFlow::Idle
            | SyncFlow::SwitchOffer { notice_open: false }
            | SyncFlow::ImportFailed { notice_open: false }
            | SyncFlow::RestartPending { notice_open: false }
            | SyncFlow::SignedOutRestartRequired => false,
            SyncFlow::Enabling
            | SyncFlow::Canceling
            | SyncFlow::SwitchOffer { notice_open: true }
            | SyncFlow::Switching { .. }
            | SyncFlow::Importing { .. }
            | SyncFlow::ImportDone { .. }
            | SyncFlow::ImportFailed { notice_open: true }
            | SyncFlow::RestartPending { notice_open: true }
            | SyncFlow::SignOutConfirm
            | SyncFlow::SigningOut => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellEscapeOutcome {
    OtherKey,
    Blocked,
    InterruptChat(String),
    Ignored,
}

/// When the shell next has render-only clock pixels to repaint, as of `now`:
/// every second while the selected chat is Working (the trailer's elapsed
/// timer) or holds running background tasks (their timers in the session
/// card), else the next wall-clock minute for
/// relative "5m"/"2h" labels. State transitions (staleness, device presence,
/// send grace) arrive as AppState notifications instead — see
/// [`AppState::watch_clock_transitions`]. Nothing else ticks: AwaitingInput
/// and Errored show no clock, and the connection pill has no countdown (its
/// spinner animates itself).
fn shell_clock_wake(state: &AppState, now: DateTime<Utc>) -> DateTime<Utc> {
    let working = state
        .selected_chat
        .as_deref()
        .is_some_and(|id| state.indicator_for(id, now) == Indicator::Working);
    // Running background tasks count up in the session card, turn or not.
    let background = state.selected_chat.as_deref().is_some_and(|id| {
        zuse_protocol::view::background_tasks(state.session_for(id), now)
            .iter()
            .any(|task| task.running())
    });
    if working || background {
        return now + chrono::TimeDelta::seconds(1);
    }
    DateTime::from_timestamp((now.timestamp().div_euclid(60) + 1) * 60, 0)
        .unwrap_or(now + chrono::TimeDelta::minutes(1))
}

/// A one-shot redraw timer, re-armed on every render for the view's next
/// clock-driven change. Idle views sleep until that deadline instead of
/// polling.
#[derive(Default)]
struct ClockRedraw {
    wake_at: Option<DateTime<Utc>>,
    task: Option<Task<()>>,
}

impl ClockRedraw {
    /// Notify the view at `wake` unless a redraw is already due no later.
    /// `slot` finds this timer on the view so the firing task can clear it.
    fn arm<V: 'static>(
        &mut self,
        wake: DateTime<Utc>,
        now: DateTime<Utc>,
        slot: fn(&mut V) -> &mut ClockRedraw,
        cx: &mut Context<V>,
    ) {
        if self.task.is_some() && self.wake_at.is_some_and(|at| at <= wake) {
            return;
        }
        let delay = (wake - now).to_std().unwrap_or_default();
        self.wake_at = Some(wake);
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |view, cx| {
                let clock = slot(view);
                clock.wake_at = None;
                clock.task = None;
                cx.notify();
            })
            .ok();
        }));
    }
}

fn resolve_shell_escape(
    key: &str,
    blocking_overlay: bool,
    escape_stops_active_agent: bool,
    route: Route,
    selected_chat: Option<&str>,
    indicator: Indicator,
    interrupting: bool,
) -> ShellEscapeOutcome {
    if key != "escape" {
        ShellEscapeOutcome::OtherKey
    } else if blocking_overlay {
        ShellEscapeOutcome::Blocked
    } else if !escape_stops_active_agent || !matches!(route, Route::Chat) || interrupting {
        ShellEscapeOutcome::Ignored
    } else if matches!(indicator, Indicator::Working | Indicator::AwaitingInput) {
        selected_chat
            .map(|chat_id| ShellEscapeOutcome::InterruptChat(chat_id.to_owned()))
            .unwrap_or(ShellEscapeOutcome::Ignored)
    } else {
        ShellEscapeOutcome::Ignored
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountMenuAction {
    EnableSync,
    SyncInProgress,
    /// Postponed switch wizard (or legacy restart fallback) — reopen it.
    RestartPending,
    SignOut,
}

#[derive(Debug, Clone)]
enum DiscardWorkingTreeFlow {
    Confirm(DiscardWorkingTreeRequest),
    Failed(SharedString),
}

const RUNTIME_CHANGE_TIMEOUT: Duration = Duration::from_secs(10);
const RUNTIME_CHANGE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Wait until a stopped daemon can no longer win the next bootstrap probe and
/// has released the data directory for the replacement runtime.
async fn wait_for_remote_engine_shutdown(
    ipc_port: u16,
    data_dir: &std::path::Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let port_closed = !matches!(
            tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
            )
            .await,
            Ok(Ok(_))
        );
        if port_closed && InstanceLock::holder(data_dir).is_none() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "the daemon did not finish stopping within {} seconds",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(RUNTIME_CHANGE_POLL_INTERVAL).await;
    }
}

/// Stop the engine that owns the synced profile and wait until a local runtime
/// can safely acquire both its IPC port and data-directory lock.
async fn stop_synced_runtime(
    engine: crate::state::EngineHandle,
    ipc_port: u16,
    data_dir: &std::path::Path,
) -> Result<(), String> {
    let stop_error = if matches!(engine.mode(), EngineMode::Remote { .. }) {
        engine
            .client()
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    engine.shutdown().await;
    match wait_for_remote_engine_shutdown(ipc_port, data_dir, RUNTIME_CHANGE_TIMEOUT).await {
        Ok(()) => Ok(()),
        Err(error) => match stop_error {
            Some(stop_error) => Err(format!("{stop_error}; {error}")),
            None => Err(error),
        },
    }
}

/// What an import-summary stream item means for the wizard: `Ok((imported,
/// skipped))` only when the engine reported zero errors; otherwise the
/// user-facing failure message. Pure so the partial-failure path is testable.
fn import_summary_outcome(item: &serde_json::Value) -> Result<(usize, usize), String> {
    let count = |key: &str| item.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let errors: Vec<&str> = item
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if errors.is_empty() {
        return Ok((count("importedChats"), count("skippedChats")));
    }
    let first = errors.first().copied().unwrap_or("unknown error");
    Err(if errors.len() == 1 {
        format!("{} imported, 1 failure: {first}", count("importedChats"))
    } else {
        format!(
            "{} imported, {} failures — first: {first}",
            count("importedChats"),
            errors.len()
        )
    })
}

/// The offer step's description of what a switch would bring along, or `None`
/// when the local profile holds nothing importable. Spaces count as work:
/// a projects-only profile must get the import choice too.
fn local_work_phrase(chats: usize, spaces: usize) -> Option<String> {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (chats, spaces) {
        (0, 0) => None,
        (c, 0) => Some(format!("the {}", plural(c, "session"))),
        (0, s) => Some(format!("the {}", plural(s, "project"))),
        (c, s) => Some(format!(
            "the {} and {}",
            plural(c, "session"),
            plural(s, "project")
        )),
    }
}

fn account_menu_action(scope: Option<WorkspaceScope>, flow: SyncFlow) -> Option<AccountMenuAction> {
    match scope {
        Some(WorkspaceScope::Local) => match flow {
            SyncFlow::Idle => Some(AccountMenuAction::EnableSync),
            SyncFlow::Enabling | SyncFlow::Canceling => Some(AccountMenuAction::SyncInProgress),
            SyncFlow::SwitchOffer { .. } | SyncFlow::RestartPending { .. } => {
                Some(AccountMenuAction::RestartPending)
            }
            SyncFlow::ImportFailed { .. } => Some(AccountMenuAction::RestartPending),
            SyncFlow::Switching { .. }
            | SyncFlow::Importing { .. }
            | SyncFlow::ImportDone { .. } => Some(AccountMenuAction::SyncInProgress),
            SyncFlow::SignOutConfirm
            | SyncFlow::SigningOut
            | SyncFlow::SignedOutRestartRequired => None,
        },
        Some(WorkspaceScope::Synced) => match flow {
            SyncFlow::SignedOutRestartRequired => None,
            // A pending import failure must stay reachable: this is the only
            // surface that can reopen the retry dialog on a synced runtime.
            SyncFlow::ImportFailed { .. } => Some(AccountMenuAction::RestartPending),
            _ if flow.is_switch_lifecycle() => Some(AccountMenuAction::SyncInProgress),
            _ => Some(AccountMenuAction::SignOut),
        },
        Some(WorkspaceScope::Development) | None => None,
    }
}

/// The sidebar footer's profile label and the account menu's identity line.
/// Anything without a signed-in account reads "Local"; the menu carries the
/// storage/sync detail.
fn sidebar_account_identity(
    scope: Option<WorkspaceScope>,
    flow: SyncFlow,
    user: Option<&zuse_protocol::UserProfile>,
) -> (SharedString, SharedString) {
    match scope {
        Some(WorkspaceScope::Local) => {
            let identity = if matches!(flow, SyncFlow::RestartPending { .. }) {
                "Sync ready after restart"
            } else {
                "Stored on this device"
            };
            ("Local".into(), identity.into())
        }
        Some(WorkspaceScope::Development) => ("Local".into(), "Stored on this device".into()),
        Some(WorkspaceScope::Synced) | None => match user {
            Some(user) => {
                let name = user
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&user.email);
                (name.to_owned().into(), user.email.clone().into())
            }
            None => ("Local".into(), "Not signed in".into()),
        },
    }
}

fn sync_flow_after_auth(
    flow: SyncFlow,
    scope: Option<WorkspaceScope>,
    auth: Option<&AuthState>,
) -> SyncFlow {
    match scope {
        Some(WorkspaceScope::Local) => match (flow, auth) {
            // The in-place switch owns its own lifecycle once started.
            (flow, _) if flow.is_switch_lifecycle() => flow,
            // AuthStatus belongs to the runtime, not to the Shell that opened
            // the browser. Every attached viewport must advertise the pending
            // profile switch once any of them completes sign-in.
            (SyncFlow::SwitchOffer { .. }, Some(AuthState::SignedOut)) => SyncFlow::Idle,
            (SyncFlow::RestartPending { .. }, Some(AuthState::SignedOut)) => SyncFlow::Idle,
            (SyncFlow::Canceling, Some(AuthState::SignedIn { .. })) => flow,
            (SyncFlow::SwitchOffer { .. }, Some(AuthState::SignedIn { .. })) => flow,
            (SyncFlow::RestartPending { .. }, Some(AuthState::SignedIn { .. })) => flow,
            (_, Some(AuthState::SignedIn { .. })) => SyncFlow::SwitchOffer { notice_open: true },
            _ => flow,
        },
        Some(WorkspaceScope::Synced) => match auth {
            // AuthStatus is shared by every viewport attached to the runtime.
            // Once a synced store loses its credentials, every Shell must stop:
            // letting another viewport sign in would authenticate a new account
            // while the engine still serves the previous account's fixed store.
            Some(AuthState::SignedOut) => SyncFlow::SignedOutRestartRequired,
            _ => match flow {
                SyncFlow::SignOutConfirm
                | SyncFlow::SigningOut
                | SyncFlow::SignedOutRestartRequired => flow,
                flow if flow.is_switch_lifecycle() => flow,
                _ => SyncFlow::Idle,
            },
        },
        Some(WorkspaceScope::Development) => SyncFlow::Idle,
        None => flow,
    }
}

/// The "Create your workspace" gate (feature-inventory §1.2 OrgGate).
struct OrgGateUi {
    name_input: Entity<ComposerInput>,
    orgs: Loadable<Vec<OrgRow>>,
    submitting: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _events: Subscription,
}

/// One right-pane subagent tab: the doc it shows, its strip title, and the
/// read-only transcript entity whose drop tears the view down.
struct SubagentTab {
    doc_id: String,
    harness: Option<zuse_protocol::HarnessId>,
    title: SharedString,
    transcript: Entity<Transcript>,
    /// Keeps a frozen-blob fetch alive (it falls back to a live doc watch).
    _fetch: Option<Task<()>>,
    /// Spawn chips INSIDE the subagent transcript open their own tabs.
    _events: Subscription,
    /// A workflow agent's conversation follows its run's progress.
    _feed: Option<Subscription>,
}

/// Sidebar render identity lets transcript/caret frames reuse its GPUI scene.
/// State and event handlers stay on Shell. Explicit Shell notifications still
/// invalidate the sidebar, including selection, menus, theme and navigation.
struct SidebarPane {
    shell: gpui::WeakEntity<Shell>,
    _observation: Subscription,
}

impl Render for SidebarPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::transcript::record_view_frame("sidebar");
        let Some(shell) = self.shell.upgrade() else {
            return div().into_any_element();
        };
        let inner = shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            shell.render_chat_sidebar(&theme, cx)
        });
        div().size_full().child(inner).into_any_element()
    }
}

/// A button in the "Check for Updates…" dialog.
#[derive(Debug, Clone, Copy)]
enum UpdatePromptButton {
    Close(&'static str),
    CheckAgain,
    Download,
    Restart,
    /// "Restart now" beside the safer choice, so it isn't the primary button.
    RestartAnyway,
    RestartWhenIdle,
    CancelRestart,
    OpenDownloads,
}

#[derive(Debug, Clone)]
enum PendingExit {
    CloseWindow,
    Quit,
    RuntimeChange,
    InstallUpdate(PathBuf),
}

/// A capture destination contains identity metadata only, never credentials.
#[derive(Clone, Default)]
pub(crate) struct AppshotDeliveryIdentity {
    pub(crate) account: Option<String>,
    pub(crate) organization: Option<String>,
    pub(crate) canvas: Option<String>,
}
fn dismiss_update_prompt(cx: &mut App) {
    if let Some(update) = crate::app_update::AppUpdate::global(cx) {
        update.update(cx, |update, cx| update.dismiss_prompt(cx));
    }
}

pub struct Shell {
    update_restart: Option<update_restart::RestartPlan>,
    update_restart_task: Option<Task<()>>,
    zot_ui: zots::ZotUi,
    state: Entity<AppState>,
    cloud_conversation: Option<Entity<cloud_conversation::CloudConversation>>,
    cloud_conversation_events: Option<Subscription>,
    cloud_tabs: cloud_tabs::CloudTabs,
    desktop_import: Entity<crate::desktop_import::Onboarding>,
    onboarding: Entity<crate::onboarding::Wizard>,
    /// A chat onboarding asked to open before its row synced in.
    pending_open_chat: Option<String>,
    sidebar_pane: Entity<SidebarPane>,
    sidebar_controls: Entity<zuse_viewport::sidebar_controls::SidebarControls>,
    workspace_sidebar_focus: FocusHandle,
    sidebar_control_tasks: std::collections::HashMap<u64, Task<()>>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// The window's voice orchestrator: independent of the selected chat.
    voice: Entity<crate::voice::VoiceController>,
    /// Sidebar footer orb (session live) and the full-window stage's orb.
    voice_stage_orb: Entity<crate::orb::Orb>,
    /// Above the composer, only while the orchestrator's own chat is open.
    voice_composer_orb: Entity<crate::orb::Orb>,
    /// The stage caption's streaming veil (shared with mobile).
    voice_caption: zuse_veil::CaptionVeil,
    voice_stage_focus: FocusHandle,
    voice_stage_was_open: bool,
    /// Last rendered route, used to distinguish navigation from staying in Settings.
    voice_stage_route: Route,
    voice_stage_changed_at: Option<std::time::Instant>,
    /// The chat selected when the stage opened; picking another closes it.
    voice_stage_selection: Option<String>,
    _voice_observation: gpui::Subscription,
    _voice_catalog: gpui::Subscription,
    /// Measured height of the bottom chrome stack (status strip + composer +
    /// terminal dock) the full-height transcript scrolls under. Paint-time
    /// measurement schedules another frame whenever this value changes.
    bottom_stack: std::rc::Rc<std::cell::Cell<f32>>,
    /// Whether `bottom_stack` was measured with the session composer present.
    /// A newly selected transcript stays hidden until this matches its route,
    /// preventing one frame at the blank canvas's stale bottom clearance.
    bottom_stack_has_composer: std::rc::Rc<std::cell::Cell<bool>>,
    /// Shared route clock and measured prepaint geometry for the persistent composer.
    composer_dock: crate::composer_dock::SharedDock,
    new_thread_artwork_ready: crate::new_thread_background_effects::Readiness,
    /// The Pinned disclosure: open by default, session-transient.
    pub(super) pinned_open: bool,
    /// Ephemeral collapsed project/device sections, keyed by organization + id.
    pub(super) sidebar_collapsed_groups: std::collections::HashSet<String>,
    /// In-flight disclosure tweens, shared by project/device groups and Pinned.
    pub(super) sidebar_disclosure_motion:
        std::collections::HashMap<String, SidebarDisclosureMotion>,
    /// Disclosures a reveal (an inline rename of a hidden row) just opened;
    /// each starts its motion on the render that knows its height.
    pub(super) sidebar_reveal_motions: std::collections::HashSet<String>,
    /// The jump-hint overlay: true while the held modifiers exactly match a
    /// jump shortcut, which swaps the first nine rows' time-ago for their
    /// key-cap chip. Frame-transient — window
    /// deactivation clears it, so a chip cannot stick after an app switch
    /// swallows the key-up.
    pub(super) jump_hints: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    /// Last resolved layout, for drag anchors and matching transcript clearance.
    terminal_geometry: crate::terminal::dock::SharedGeometry,
    /// Embedded terminal host for right-pane Terminal surfaces — a SEPARATE
    /// entity from the bottom drawer's (own PTYs, own grid geometry; one
    /// panel can only size one visible grid at a time).
    right_terminal: Option<Entity<TerminalPanel>>,
    /// The surface-tab strip's `+` menu (see `panel_menu`).
    right_plus: popover::Popup<()>,
    /// The titlebar's "Open in" menu and the apps it found.
    open_in: open_in::OpenIn,
    branch_menu: top_bar::BranchMenu,
    cloud_info: cloud_info::CloudInfo,
    /// Host-owned project Actions cached per (device, space).
    project_actions: crate::project_actions::ProjectActionsController,
    /// Diff surfaces by id — each tab its own [`Changes`] viewer with its own
    /// scope/base pick and diff watch (multiple diff panels, user request).
    diffs: std::collections::HashMap<u64, Entity<Changes>>,
    /// One workspace browser per chat/panel key. Dropping the entity closes
    /// its file watcher and every in-flight workspace request.
    files: std::collections::HashMap<String, Entity<FilesSurface>>,
    files_subs: std::collections::HashMap<String, Subscription>,
    /// One independent editor per opened workspace file. IDs are global
    /// while the lookup key keeps a file tab scoped to its chat panel.
    file_surfaces: std::collections::HashMap<u64, Entity<FilesSurface>>,
    file_surface_paths: std::collections::HashMap<u64, String>,
    /// Open editors by (pane, owning chat, path): a side chat's file links
    /// open editors bound to the side chat, beside the main chat's own.
    file_surface_keys: std::collections::HashMap<(String, String, String), u64>,
    file_surface_subs: std::collections::HashMap<u64, Subscription>,
    file_surface_seq: u64,
    pending_file_closes: std::collections::HashSet<RightSurface>,
    pending_exit: Option<PendingExit>,
    /// Event hookups for [`Self::diffs`] (History rows opening commit tabs).
    diff_subs: std::collections::HashMap<u64, Subscription>,
    diff_seq: u64,
    /// Review and commit-diff tabs in the main area, per chat panel key.
    main_tabs: std::collections::HashMap<String, main_tabs::MainTabs>,
    /// Subagent transcript surfaces by id — each tab a read-only
    /// [`Transcript`] pinned to its subagent doc.
    subagent_tabs: std::collections::HashMap<u64, SubagentTab>,
    subagent_seq: u64,
    side_chats: std::collections::HashMap<u64, SideChatTab>,
    side_chat_seq: u64,
    side_chat_creating: bool,
    thread_conversation_creating: bool,
    /// The tab strip's history menu: this thread's closed conversations.
    conversation_history: popover::Popup<()>,
    browsers: std::collections::HashMap<u64, Entity<crate::browser::BrowserSurface>>,
    browser_subs: std::collections::HashMap<u64, Subscription>,
    browser_seq: u64,
    simulators: std::collections::HashMap<u64, Entity<crate::simulator::SimulatorPane>>,
    simulator_seq: u64,
    /// Pull request tabs in session panes, keyed like the other surfaces.
    chat_pull_requests: std::collections::HashMap<u64, chat_pull_requests::ChatPullRequest>,
    chat_pull_request_seq: u64,
    browser_context: crate::browser::BrowserContext,
    browser_profile: Option<crate::browser::profile::BrowserProfile>,
    /// Ordered surface tabs per panel key (drag-reorderable; stale entries —
    /// closed terminals/diffs — are skipped at read time).
    right_tabs: std::collections::HashMap<String, Vec<RightSurface>>,
    /// In-flight surface-tab drag (slide animation state).
    right_tab_drag: Option<RightTabDragState>,
    /// Surface-tab strip scroll (the strip overflows horizontally, scroll-area
    /// style; drag drop-math reads the offset back out).
    right_tab_scroll: gpui::ScrollHandle,
    /// Chat outlet, pull request dashboard, or settings pages.
    route: Route,
    settings_focus: FocusHandle,
    settings_end_focus: FocusHandle,
    settings_nav_focus: Vec<FocusHandle>,
    settings_return_focus: Option<FocusHandle>,
    settings_focus_pending: bool,
    settings_restore_pending: bool,
    settings_return_route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    pull_requests_page: Option<Entity<PullRequestsPage>>,
    /// Opened pull requests, as tabs in a pane beside the board.
    pull_request_pane: pull_request_pane::PullRequestPane,
    /// A detail view a history step landed on, opened by the next render
    /// (creating it needs the window).
    pending_pull_request: Option<(String, Option<String>)>,
    pull_request_cache:
        std::rc::Rc<std::cell::RefCell<crate::pull_request_detail::PullRequestCache>>,
    cloud_billing_page: Option<Entity<settings::cloud_billing::CloudBillingPage>>,
    cloud_page_sub: Option<Subscription>,
    /// Settings → Usage shows Cloud compute instead of model usage.
    usage_settings_compute: bool,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    archive_project: Option<String>,
    usage_dashboard: Option<Entity<zuse_viewport::usage_dashboard::UsageDashboard>>,
    usage_tasks: std::collections::BTreeMap<u64, Task<()>>,
    usage_project: Option<String>,
    usage_project_scope: bool,
    usage_pending: bool,
    project_archives:
        std::collections::HashMap<String, Entity<crate::project_archive::ProjectArchive>>,
    project_archive_events: std::collections::HashMap<String, Subscription>,
    appearance_page: Option<Entity<AppearancePage>>,
    files_settings_page: Option<Entity<FilesSettingsPage>>,
    notifications_page: Option<Entity<NotificationsPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    harnesses_page: Option<Entity<HarnessesPage>>,
    defaults_page: Option<Entity<settings::defaults::DefaultsPage>>,
    shortcuts_sub: Option<Subscription>,
    notifications_sub: Option<Subscription>,
    files_settings_sub: Option<Subscription>,
    appearance_settings_sub: Option<Subscription>,
    /// Session-row context menu, including the Copy submenu.
    chat_menu: popover::Popup<ChatMenuState>,
    /// The Copy submenu's hover grace, its painted bounds, and which side
    /// of the menu it opens on (the sidebar options menu's recipe).
    chat_copy_intent: popover::HoverIntent<u8>,
    chat_copy_bounds: Option<gpui::Bounds<Pixels>>,
    chat_copy_left: bool,
    /// The chat title being edited in place on its row.
    chat_rename: Option<ChatRename>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    deleting_chat: bool,
    cloud_delete_commands: std::collections::HashMap<String, String>,
    /// Global confirmation/error dialog for the Changes-pane trash action. The
    /// RPC task is retained separately so rerenders do not cancel it.
    discard_working_tree: Option<DiscardWorkingTreeFlow>,
    discard_working_tree_task: Option<Task<()>>,
    /// Space-row context menu (dropdown rows): (space id, window position).
    space_menu: popover::Popup<(String, Point<Pixels>)>,
    space_menu_active: usize,
    rename_space_dialog: Option<RenameSpaceDialog>,
    sidebar_section_migration: Option<(String, crate::state::EngineHandle)>,
    section_dialog: Option<sidebar_sections::SectionDialog>,
    section_menu: Option<(String, Point<Pixels>)>,
    section_header_hover: Option<String>,
    section_menu_focus: FocusHandle,
    section_menu_active: Option<usize>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The keyboard shortcuts panel (Mod+/, or `?` in an empty composer).
    shortcuts_panel_open: bool,
    /// The first stop-background-tasks press, waiting for its confirmation.
    background_stop_armed_at: Option<chat_shortcuts::StopConfirmation>,
    /// The add-space palette (device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    project_setup: Option<project_setup::ProjectSetup>,
    project_parent_setup: Option<project_setup::ProjectSetup>,
    /// The New project palette's collapsed-breadcrumbs (`…`) menu.
    project_crumb_menu: popover::Popup<()>,
    command_palette: Option<command_palette::CommandPalette>,
    /// The focus whose workspace search index was last warmed (see `search_warm`).
    search_warm: Option<search_warm::SearchWarm>,
    /// Per chat, the files opened in editor tabs, most recent first.
    recent_files: std::collections::HashMap<String, std::collections::VecDeque<String>>,
    pending_workspace_command: Option<crate::composer::WorkspaceCommand>,
    /// A composer file chip's path, opened by the next render.
    pending_chip_file: Option<String>,
    /// The sidebar's space-filter dropdown.
    spaces_menu: popover::Popup<spaces::SpacesMenu>,
    /// Hover/drag + scroll-linger state of the dropdown's floating rail.
    spaces_menu_bar: popover::MenuScrollbarState,
    /// Persisted organization/sort/metadata controls beside the project filter.
    sidebar_view_menu: popover::Popup<spaces::SidebarViewMenu>,
    /// Natural-tab-order focus target for the icon-only view-options button.
    sidebar_view_trigger_focus: gpui::FocusHandle,
    /// Current pinned-row metrics shared by hit testing and displacement animations.
    sidebar_pinned_heights: Vec<f32>,
    project_icons: project_icon::ProjectIcons,
    /// Hovered row whose status is replaced by the archive control.
    chat_status_hover: Option<String>,
    /// Set by an archive-pill click until the pointer next moves. gpui only
    /// re-evaluates `on_hover` on mouse movement, so the row that slides up
    /// under a still pointer would otherwise never show its archive pill;
    /// while set, rows adopt the hover from a paint-time hit test instead.
    chat_hover_resync: bool,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// In-flight reorder for the pinned section only.
    pinned_session_drag: Option<PinnedSessionDragState>,
    pinned_session_drag_generation: u64,
    sidebar_session_transfer: Option<SidebarSessionTransfer>,
    sidebar_session_return: Option<SidebarSessionReturn>,
    /// Pending pin intents are scoped to the active profile and engine attachment.
    sidebar_pin_write: Option<sidebar_pins::PendingSidebarPins>,
    sidebar_pin_write_generation: u64,
    sidebar_pin_write_notice: Option<SharedString>,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, crate::sound::SessionNotificationState>,
    /// Startup-aware durable connectivity notification baseline.
    connectivity_notifications: crate::sound::ConnectivityNotificationState,
    /// Persistent across AppState observer callbacks so simultaneous session
    /// failures and connectivity degradation produce one attention sound.
    attention_sound_gate: crate::sound::AttentionSoundGate,
    /// `{device,harness,latest}` discoveries already delivered as a desktop
    /// banner during this viewport lifetime.
    harness_update_seen: std::collections::HashSet<String>,
    harness_update_dismissed: std::collections::HashSet<String>,
    /// Short debounce that aggregates providers finishing the same check at
    /// slightly different times into one banner.
    harness_update_banner_task: Option<Task<()>>,
    /// Independent watches retain device identity across selection changes.
    harness_update_devices: std::collections::BTreeMap<String, harness_updates::DeviceUpdates>,
    pr_watch_devices: std::collections::HashMap<String, pr_watch::DeviceWatch>,
    harness_update_expanded: bool,
    harness_update_transition: Option<WidthTween>,
    harness_update_geometry: [Option<WidthTween>; 2],
    harness_update_scroll: settings::widgets::PageScroll,
    user_menu: popover::Popup<()>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Repaints the update strip and dialog as the app-level update
    /// lifecycle ([`crate::app_update`]) moves. `None` when the app runs
    /// without a desktop checker (tests).
    _app_update_observation: Option<Subscription>,
    org: Option<OrgGateUi>,
    sync_flow: SyncFlow,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    runtime_change_task: Option<Task<()>>,
    runtime_change_error: Option<SharedString>,
    /// The one-time local→synced import stream (switch wizard progress step).
    import_task: Option<Task<()>>,
    /// Title of the chat the import stream is copying right now.
    import_current: Option<SharedString>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// The store's settings when `settings` last synced with it. Fields that
    /// differ from this are the Shell's edits; all others follow the store.
    settings_base: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    panel_open_sequence: u64,
    sidebar_opened_at: u64,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Last selected session survives opening the blank Appshot destination.
    last_appshot_chat: Option<String>,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`ZUSE_OPEN_DIALOG`, `ZUSE_FORCE_GATE`,
    /// `ZUSE_DEMO_UPLOAD`) — see [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    debug_upload: Option<String>,
    sidebar_tween: Option<WidthTween>,
    files_tween: Option<WidthTween>,
    recovery: Entity<crate::recovery::RecoveryCard>,
    /// The session card's reveal (0 closed → 1 open).
    session_info_tween: Option<WidthTween>,
    /// Whether the card showed last frame; a change starts the reveal tween.
    session_info_shown: bool,
    /// The session card is open over the transcript.
    session_info_overlay: bool,
    /// The card's sections the user toggled away from their default (by key).
    session_info_folded: std::collections::HashSet<&'static str>,
    /// The session card's pull request: details, menu and updates.
    summary_pr: summary_pull_request::SummaryPullRequest,
    /// Background tasks with a stop request on its way (by task id); their
    /// row shows a spinner until the task leaves the live set.
    background_stops: std::collections::HashSet<String>,
    /// Whether the card showed at the toggle's press (see
    /// `note_session_info_press`).
    session_info_pressed_open: Option<bool>,
    files_content_tween: Option<WidthTween>,
    /// The last tracked frame: its chat, the window fit, and for each of
    /// `[sidebar, surface host, Files]` the width painted and the width its
    /// content laid out at.
    painted_columns: Option<(String, HorizontalPanelFit, [(f32, f32); 3])>,
    /// Columns the fit just hid while still open, easing out from the width
    /// they last painted with their content held at its layout width (same
    /// order).
    fit_exits: [Option<(WidthTween, f32)>; 3],
    /// The last window fit and the inputs it was computed from, one slot
    /// per `closing` flag of `fit`: layout helpers ask for both many times
    /// per frame while a close runs. Not a per-frame value, since handlers
    /// re-read the fit right after changing what it depends on.
    fit_memo: [std::cell::Cell<Option<(FitInputs, HorizontalPanelFit)>>; 2],
    /// The closed surface host already released its image previews.
    right_images_suspended: bool,
    sidebar_edge_bounce: Option<motion::ResizeEdgeBounce>,
    /// Boundary currently held during a sidebar drag. Cleared on re-entry or
    /// release so the next genuine edge crossing can acknowledge the limit.
    sidebar_resize_edge: Option<motion::ResizeEdge>,
    /// Gesture-owned resize feedback. Unlike hover, this stays active while
    /// the seam moves away from the pointer and clears only on release or when
    /// a constrained edge takes over with its bounce cue.
    pane_resize_active: Option<PaneResizeKind>,
    pane_resize_dragging: Option<PaneResizeKind>,
    /// The window changed width since the previous frame. Live window
    /// resizing moves layout directly instead of through motion springs.
    viewport_resized: bool,
    right_tween: Option<WidthTween>,
    right_edge_bounce: Option<motion::ResizeEdgeBounce>,
    right_resize_edge: Option<motion::ResizeEdge>,
    /// Holds content geometry during masked open/close (including reversals),
    /// or resizes it during takeover. The outer mask has its own tween.
    right_content_tween: Option<WidthTween>,
    /// Takeover mask transition. `from` also retains the conversation's content
    /// width while covered, even after expansion settles. Ordinary panel
    /// open/close keeps the upstream flex behavior.
    main_takeover_tween: Option<WidthTween>,
    /// Changes-panel takeover (the header's expand button): the panel fills
    /// everything right of the sidebar and the conversation column collapses
    /// to zero. Session-local view state — never persisted, reset on close.
    right_pane_expanded: bool,
    /// Viewport width stamped each frame at render — the expanded panel's
    /// width target and the physical ceiling for free-form resizing
    /// ([`Self::right_target`] has no `Window`).
    viewport_width: f32,
    viewport_height: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    titlebar_island: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// The caption buttons zuse itself draws on Linux under client-side
    /// decorations, per side, already filtered to what the compositor
    /// supports — `None` off Linux or under server decorations (where the WM
    /// draws real buttons). Re-resolved every frame at the top of `render`.
    linux_captions: Option<gpui::WindowButtonLayout>,
    /// Re-renders when the desktop's button layout changes (GNOME
    /// `button-layout` gsetting). Registered on first paint — [`Shell::new`]
    /// has no window.
    button_layout_sub: Option<Subscription>,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    /// All pane masks and chrome evaluate animation at the same frame time.
    /// A slow render must not give the native page and its titlebar different widths.
    render_time: Option<std::time::Instant>,
    /// A deterministic clock for rendered integration tests.
    #[cfg(test)]
    test_time: Option<std::time::Instant>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with missing or unmounted focus they go dead. Recover after handoffs
    /// settle, preserving focus on mounted controls.
    focus_sub: Option<Subscription>,
    shortcut_focus: FocusHandle,
    navigation_focus: navigation_focus::NavigationFocus,
    /// Neutral shortcut target after clicking away from an input.
    unfocused: FocusHandle,
    /// Clears the jump hints when the window deactivates: a Cmd+Tab away
    /// swallows the key-up, so without this the chips stay on screen for good.
    activation_sub: Option<Subscription>,
    /// Redraw scheduled for the next clock-driven change (see [`shell_clock_wake`]).
    clock: ClockRedraw,
    _state_observation: Subscription,
    _composer_events: Subscription,
    /// The primary transcript's spawn-chip events (subagent tabs).
    _transcript_events: Subscription,
    _transcript_invalidation: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let desktop_import = cx.new(|cx| crate::desktop_import::Onboarding::new(state.clone(), cx));
        cx.subscribe(
            &desktop_import,
            |this: &mut Self, _, event: &crate::desktop_import::Applied, cx| {
                if let Some(id) = &event.open_chat {
                    this.state.update(cx, |state, cx| {
                        if let Some(chat) = state.chats.iter().find(|chat| &chat.id == id).cloned()
                        {
                            state.select_space(chat.space_id.clone(), cx);
                            state.select_chat(Some(chat.id), cx);
                        }
                    });
                }
                this.pull_settings(cx);
                if event.show_chats {
                    if event.open_chat.is_some() {
                        this.set_route(Route::Chat, cx);
                    } else {
                        this.open_settings(SettingsSection::Archived, cx);
                    }
                }
                let pickers = this.composer.read(cx).pickers().clone();
                pickers.update(cx, |pickers, cx| {
                    pickers.reload_defaults();
                    cx.notify();
                });
            },
        )
        .detach();

        let onboarding =
            cx.new(|cx| crate::onboarding::Wizard::new(state.clone(), desktop_import.clone(), cx));
        cx.subscribe(
            &onboarding,
            |this: &mut Self, _, event: &crate::onboarding::Finished, cx| {
                if let Some(id) = &event.open_chat {
                    this.pending_open_chat = Some(id.clone());
                    this.open_pending_chat(cx);
                } else if event.add_project {
                    this.open_add_space(cx);
                }
                this.pull_settings(cx);
                let pickers = this.composer.read(cx).pickers().clone();
                pickers.update(cx, |pickers, cx| {
                    pickers.reload_defaults();
                    cx.notify();
                });
                cx.notify();
            },
        )
        .detach();

        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        // A reopened window reuses AppState, so the engine may already be
        // ready. Only show the boot splash while it is actually connecting.
        let splash = match &state.read(cx).connection {
            ConnectionStatus::Connecting => SplashPhase::Visible,
            ConnectionStatus::Ready | ConnectionStatus::Failed(_) => SplashPhase::Gone,
        };
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        transcript.update(cx, |transcript, _| transcript.retain_for_route_exit());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        let voice = cx.new(|_| crate::voice::VoiceController::default());
        let voice_stage_orb = cx.new(|_| {
            crate::orb::Orb::new()
                .size(crate::orb::OrbSize::Hero)
                .state_transition(Duration::from_millis(300))
                .scale(VOICE_STAGE_ORB_SCALE)
                .visible(false)
        });
        let voice_composer_orb = cx.new(|_| {
            crate::orb::Orb::new()
                .size(crate::orb::OrbSize::Avatar)
                .state_transition(Duration::from_millis(300))
                .visible(false)
        });
        let voice_observation = cx.observe(&voice, |_: &mut Shell, _, cx| cx.notify());
        // Settings → Agents installed or toggled Codex: voice follows.
        let voice_catalog =
            cx.observe_global::<crate::pickers::HarnessCatalogChanged>(|this: &mut Shell, cx| {
                this.check_voice_codex(true, cx)
            });
        let links = Self::session_links(None, cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        // Every send glides the prompt to the viewport top and reserves the
        // reply's space below it (notes-app parity).
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::CloudAdmitted {
                    workspace,
                    account,
                    organization,
                    launch,
                } => {
                    let controller = this
                        .state
                        .read(cx)
                        .engine()
                        .map(|engine| engine.controller());
                    if let Some(controller) = controller {
                        let Some(id) = workspace["workspaceId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                        else {
                            return;
                        };
                        let Some(session) = workspace["initialSessionId"]
                            .as_str()
                            .filter(|id| !id.is_empty())
                        else {
                            return;
                        };
                        let target = crate::state::CloudTarget {
                            controller,
                            subject: account.clone(),
                            organization: organization.clone(),
                            workspace: id.into(),
                            initial_session: session.into(),
                        };
                        this.open_cloud_conversation(target, workspace.clone(), launch.clone(), cx);
                    }
                }
                ComposerEvent::CloudLaunchChanged => {
                    cx.notify();
                }
                ComposerEvent::ResumeCloud { .. }
                | ComposerEvent::CloudSleepRequested
                | ComposerEvent::CloudWakeRequested
                | ComposerEvent::CloudComputerRecreating { .. }
                | ComposerEvent::CloudForegroundAdmitted => {}
                ComposerEvent::WorkspaceCommand(command) => {
                    this.pending_workspace_command = Some(*command);
                    cx.notify();
                }
                ComposerEvent::NewThreadTransitionStarted => {
                    // Route observation drives the dock once selection commits.
                    cx.notify();
                }
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
                ComposerEvent::WorktreeSetup {
                    chat_id,
                    setup_action,
                    setup_error,
                    target_device_id,
                } => this.attach_worktree_setup(
                    chat_id.clone(),
                    setup_action.clone(),
                    setup_error.clone(),
                    target_device_id.clone(),
                    cx,
                ),
                ComposerEvent::Queued {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_queued_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
                // A file chip in the draft opens on the next frame, which has
                // the window the file surface needs.
                ComposerEvent::OpenFile(path) => {
                    this.pending_chip_file = Some(path.to_string());
                    cx.notify();
                }
            }
        });
        // Spawn chips open their subagent's transcript as a right-pane tab.
        let transcript_events = cx.subscribe(&transcript, Self::on_transcript_event);
        let data_dir = boot.data_dir.clone();
        let mut settings = settings::current(cx);
        state.update(cx, |state, cx| {
            state.watch_clock_transitions(cx);
            state.set_change_requests_visible(settings.sidebar_show_pull_request, cx)
        });
        crate::appshots::set_enabled(settings.appshots_enabled);
        crate::appshots::set_capture_sound_enabled(settings.appshot_sound_enabled);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap, settings.composer_send_behavior);
        // Dev/testing knob: `ZUSE_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors. Bare
        // `settings` reopens the remembered section; a named one is
        // remembered like any other link to a section.
        let open_route = std::env::var("ZUSE_OPEN_ROUTE").ok();
        let route = match open_route.as_deref() {
            Some("pull-requests") => Route::PullRequests,
            Some("plugins") => Route::Plugins,
            Some("zots/new") => Route::Zot,
            Some(route) if route == "settings" || route.starts_with("settings/") => {
                match settings_open_route(route, settings.settings_section) {
                    Some(section) => {
                        if settings.settings_section != section {
                            settings.settings_section = section;
                            settings::update(settings::SavePolicy::Debounced, cx, |s| {
                                s.settings_section = section;
                            });
                        }
                        Route::Settings(section)
                    }
                    None => Route::Chat,
                }
            }
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `ZUSE_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `ZUSE_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("ZUSE_OPEN_DIALOG").ok();
        // `ZUSE_DEMO_UPLOAD=<pct>:<image path>` fabricates an in-flight image
        // send on the selected chat (echo bubble + frozen thumbnail progress
        // ring) — display-only; a real upload can't be paused for a capture.
        let debug_upload = std::env::var("ZUSE_DEMO_UPLOAD").ok();
        let debug_gate = match std::env::var("ZUSE_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the zuse engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::PullRequests => NavEntry::PullRequests,
            Route::Plugins => NavEntry::Plugins,
            Route::Archives => NavEntry::Archives(String::new()),
            Route::Zot => NavEntry::NewZot,
            Route::Usage => NavEntry::Usage {
                project: None,
                project_scope: false,
            },
            Route::Settings(section) => NavEntry::Settings(section),
        });
        // Parent notifications carry presentation changes (session status,
        // elapsed labels, menus); sibling animation/caret ticks do not.
        let transcript_invalidation = cx.observe_self(|shell, cx| {
            shell.transcript.update(cx, |_, cx| cx.notify());
        });
        let shell = cx.entity();
        let sidebar_pane = cx.new(|cx| SidebarPane {
            shell: shell.downgrade(),
            _observation: cx.observe(&shell, |_, _, cx| cx.notify()),
        });
        let sidebar_controls = Self::create_sidebar_controls(state.clone(), cx);
        Self {
            state: state.clone(),
            cloud_conversation: None,
            cloud_conversation_events: None,
            cloud_tabs: Default::default(),
            desktop_import,
            onboarding,
            pending_open_chat: None,
            zot_ui: zots::ZotUi::default(),
            sidebar_pane,
            sidebar_controls,
            workspace_sidebar_focus: cx.focus_handle().tab_stop(true),
            sidebar_control_tasks: Default::default(),
            transcript,
            composer,
            voice,
            voice_stage_orb,
            voice_composer_orb,
            voice_caption: zuse_veil::CaptionVeil::new(voice_stage::STAGE_CAPTION_CHARS),
            voice_stage_focus: cx.focus_handle(),
            voice_stage_was_open: false,
            voice_stage_route: Route::Chat,
            voice_stage_changed_at: None,
            voice_stage_selection: None,
            _voice_observation: voice_observation,
            _voice_catalog: voice_catalog,
            // Seed with the compact composer stack's rough height so the
            // first frame's clearance isn't zero (the measure corrects it).
            bottom_stack: std::rc::Rc::new(std::cell::Cell::new(120.0)),
            bottom_stack_has_composer: std::rc::Rc::new(std::cell::Cell::new(false)),
            composer_dock: Default::default(),
            new_thread_artwork_ready: Default::default(),
            pinned_open: true,
            sidebar_collapsed_groups: std::collections::HashSet::new(),
            sidebar_reveal_motions: std::collections::HashSet::new(),
            sidebar_disclosure_motion: std::collections::HashMap::new(),
            jump_hints: false,
            terminal: None,
            right_terminal: None,
            right_plus: popover::Popup::default(),
            open_in: open_in::OpenIn::default(),
            branch_menu: top_bar::BranchMenu::default(),
            cloud_info: cloud_info::CloudInfo::default(),
            project_actions: crate::project_actions::ProjectActionsController::default(),
            diffs: std::collections::HashMap::new(),
            files: std::collections::HashMap::new(),
            files_subs: std::collections::HashMap::new(),
            file_surfaces: std::collections::HashMap::new(),
            file_surface_paths: std::collections::HashMap::new(),
            file_surface_keys: std::collections::HashMap::new(),
            file_surface_subs: std::collections::HashMap::new(),
            file_surface_seq: 0,
            pending_file_closes: std::collections::HashSet::new(),
            pending_exit: None,
            diff_subs: std::collections::HashMap::new(),
            diff_seq: 0,
            main_tabs: std::collections::HashMap::new(),
            subagent_tabs: std::collections::HashMap::new(),
            subagent_seq: 0,
            side_chats: std::collections::HashMap::new(),
            side_chat_seq: 0,
            side_chat_creating: false,
            thread_conversation_creating: false,
            conversation_history: popover::Popup::default(),
            browsers: std::collections::HashMap::new(),
            browser_subs: std::collections::HashMap::new(),
            browser_seq: 0,
            simulators: std::collections::HashMap::new(),
            simulator_seq: 0,
            chat_pull_requests: std::collections::HashMap::new(),
            chat_pull_request_seq: 0,
            browser_context: crate::browser::BrowserContext::default(),
            browser_profile: None,
            right_tabs: std::collections::HashMap::new(),
            right_tab_drag: None,
            right_tab_scroll: gpui::ScrollHandle::new(),
            route,
            settings_focus: cx.focus_handle(),
            settings_end_focus: cx.focus_handle(),
            settings_nav_focus: SettingsSection::ALL
                .iter()
                .map(|_| cx.focus_handle())
                .collect(),
            settings_return_focus: None,
            settings_focus_pending: matches!(route, Route::Settings(_)),
            settings_restore_pending: false,
            settings_return_route: Route::Chat,
            nav,
            pull_requests_page: None,
            pull_request_pane: Default::default(),
            pending_pull_request: None,
            pull_request_cache: Default::default(),
            cloud_billing_page: None,
            cloud_page_sub: None,
            usage_settings_compute: false,
            devices_page: None,
            archived_page: None,
            archive_project: None,
            usage_dashboard: None,
            usage_tasks: Default::default(),
            usage_project: None,
            usage_project_scope: false,
            usage_pending: route == Route::Settings(SettingsSection::Usage),
            project_archives: std::collections::HashMap::new(),
            project_archive_events: std::collections::HashMap::new(),
            appearance_page: None,
            files_settings_page: None,
            notifications_page: None,
            shortcuts_page: None,
            accounts_page: None,
            harnesses_page: None,
            defaults_page: None,
            shortcuts_sub: None,
            notifications_sub: None,
            files_settings_sub: None,
            appearance_settings_sub: None,
            chat_menu: popover::Popup::default(),
            chat_copy_intent: Default::default(),
            chat_copy_bounds: None,
            chat_copy_left: false,
            chat_rename: None,
            delete_confirm: None,
            deleting_chat: false,
            cloud_delete_commands: std::collections::HashMap::new(),
            discard_working_tree: None,
            discard_working_tree_task: None,
            space_menu: popover::Popup::default(),
            space_menu_active: 0,
            rename_space_dialog: None,
            sidebar_section_migration: None,
            section_dialog: None,
            section_menu: None,
            section_header_hover: None,
            section_menu_focus: cx.focus_handle(),
            section_menu_active: None,
            delete_space_confirm: None,
            shortcuts_panel_open: false,
            background_stop_armed_at: None,
            add_space: None,
            project_setup: None,
            project_parent_setup: None,
            project_crumb_menu: popover::Popup::default(),
            command_palette: None,
            search_warm: None,
            recent_files: std::collections::HashMap::new(),
            pending_workspace_command: None,
            pending_chip_file: None,
            spaces_menu: popover::Popup::default(),
            spaces_menu_bar: popover::MenuScrollbarState::default(),
            sidebar_view_menu: popover::Popup::default(),
            sidebar_view_trigger_focus: cx.focus_handle().tab_stop(true),
            sidebar_pinned_heights: Vec::new(),
            project_icons: Default::default(),
            chat_status_hover: None,
            chat_hover_resync: false,
            sidebar_scroll: gpui::ScrollHandle::new(),
            pinned_session_drag: None,
            pinned_session_drag_generation: 0,
            sidebar_session_transfer: None,
            sidebar_session_return: None,
            sidebar_pin_write: None,
            sidebar_pin_write_generation: 0,
            sidebar_pin_write_notice: None,
            space_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            connectivity_notifications: Default::default(),
            attention_sound_gate: Default::default(),
            harness_update_seen: std::collections::HashSet::new(),
            harness_update_dismissed: std::collections::HashSet::new(),
            harness_update_banner_task: None,
            harness_update_devices: Default::default(),
            pr_watch_devices: Default::default(),
            harness_update_expanded: false,
            harness_update_transition: None,
            harness_update_geometry: [None; 2],
            harness_update_scroll: settings::widgets::PageScroll::default(),
            user_menu: popover::Popup::default(),
            sidebar_notice: None,
            update_restart: None,
            update_restart_task: None,
            _app_update_observation: crate::app_update::AppUpdate::global(cx)
                .map(|update| cx.observe(&update, |_, _, cx| cx.notify())),
            org: None,
            sync_flow: SyncFlow::Idle,
            mutate_task: None,
            auth_task: None,
            runtime_change_task: None,
            runtime_change_error: None,
            import_task: None,
            import_current: None,
            boot,
            data_dir,
            settings_base: settings::current(cx),
            settings,
            panels: SessionPanels::default(),
            panel_open_sequence: 0,
            sidebar_opened_at: 0,
            active_chat: String::new(),
            last_appshot_chat: None,
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            debug_upload,
            sidebar_tween: None,
            files_tween: None,
            recovery: cx.new(|cx| crate::recovery::RecoveryCard::new(state.clone(), cx)),
            session_info_tween: None,
            session_info_shown: false,
            session_info_overlay: false,
            session_info_folded: Default::default(),
            background_stops: Default::default(),
            summary_pr: Default::default(),
            session_info_pressed_open: None,
            files_content_tween: None,
            painted_columns: None,
            fit_exits: [None; 3],
            fit_memo: Default::default(),
            right_images_suspended: false,
            sidebar_edge_bounce: None,
            sidebar_resize_edge: None,
            pane_resize_active: None,
            pane_resize_dragging: None,
            viewport_resized: false,
            right_tween: None,
            right_edge_bounce: None,
            right_resize_edge: None,
            right_content_tween: None,
            main_takeover_tween: None,
            right_pane_expanded: false,
            viewport_width: 1280.0,
            viewport_height: 880.0,
            terminal_tween: None,
            terminal_geometry: Default::default(),
            fullscreen: None,
            titlebar_tween: None,
            titlebar_island: None,
            titlebar_should_move: false,
            linux_captions: None,
            button_layout_sub: None,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            render_time: None,
            #[cfg(test)]
            test_time: None,
            splash,
            splash_task: None,
            focus_sub: None,
            shortcut_focus: cx.focus_handle(),
            navigation_focus: navigation_focus::NavigationFocus::new(cx),
            unfocused: cx.focus_handle(),
            activation_sub: None,
            clock: ClockRedraw::default(),
            _state_observation: observation,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
            _transcript_invalidation: transcript_invalidation,
        }
    }

    pub(crate) fn appshot_capture_identity(
        &mut self,
        cx: &mut Context<Self>,
    ) -> AppshotDeliveryIdentity {
        self.sync_cloud_tab_scope(cx);
        let (account, organization) = {
            let state = self.state.read(cx);
            (
                state.cloud_user_id().map(str::to_owned),
                state.cloud_organization_id().map(str::to_owned),
            )
        };
        let canvas = if self.settings.appshot_destination
            == crate::appshots::AppshotDestination::Automatic
        {
            if let Some(view) = &self.cloud_conversation {
                Some(view.read(cx).target.initial_session.clone())
            } else if self.state.read(cx).selected_chat.is_none()
                && self.state.read(cx).cloud_project.is_some()
            {
                self.track_current_cloud_canvas(cx);
                Some(self.composer.read(cx).current_key.clone())
            } else {
                None
            }
        } else {
            None
        };
        AppshotDeliveryIdentity {
            account,
            organization,
            canvas,
        }
    }
    pub(crate) fn accepts_appshot_identity(
        &self,
        identity: &AppshotDeliveryIdentity,
        cx: &App,
    ) -> bool {
        let state = self.state.read(cx);
        state.cloud_user_id() == identity.account.as_deref()
            && state.cloud_organization_id() == identity.organization.as_deref()
            && identity.canvas.as_deref().is_none_or(|key| {
                self.retains_cloud_canvas(key)
                    || self.cloud_tabs.composer_for_session(key, cx).is_some()
            })
    }
    pub(crate) fn receive_scoped_appshot(
        &mut self,
        identity: AppshotDeliveryIdentity,
        appshot: crate::appshots::CapturedAppshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_cloud_tab_scope(cx);
        if !self.accepts_appshot_identity(&identity, cx) {
            return false;
        }
        if let Some(key) = identity.canvas {
            let composer = self
                .cloud_tabs
                .composer_for_session(&key, cx)
                .unwrap_or_else(|| self.composer.clone());
            let accepted = composer.update(cx, |composer, cx| {
                composer.stage_appshot_for(key.clone(), appshot, cx)
            });
            if composer.read(cx).current_key == key {
                window.focus(&composer.focus_handle(cx), cx);
            }
            cx.notify();
            return accepted;
        }
        self.receive_appshot(appshot, window, cx);
        true
    }
    /// Route a completed viewer-side capture only after its source window is
    /// safely captured. The explicit target key avoids relying on the
    /// state-observation/draft-swap effect ordering when opening the canvas.
    pub fn receive_appshot(
        &mut self,
        appshot: crate::appshots::CapturedAppshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::appshots::AppshotDestination;

        let selected = self.state.read(cx).selected_chat.clone();
        let target = match self.settings.appshot_destination {
            AppshotDestination::Automatic if selected.is_some() => selected,
            AppshotDestination::LastSession if selected.is_some() => selected,
            AppshotDestination::LastSession => self
                .last_appshot_chat
                .clone()
                .filter(|id| self.state.read(cx).chats.iter().any(|chat| &chat.id == id)),
            AppshotDestination::Automatic | AppshotDestination::NewSession => None,
        };
        if let Some(chat_id) = &target {
            self.open_chat(chat_id.clone(), cx);
        } else if self.settings.appshot_destination == AppshotDestination::NewSession
            || self.state.read(cx).selected_chat.is_some()
        {
            // Reuse upstream's project-filter and device defaults for a new
            // canvas. Automatic capture on an existing canvas keeps its pick.
            self.open_new_session(None, cx);
        } else {
            self.set_route(Route::Chat, cx);
        }
        if target.is_none() && self.state.read(cx).cloud_project.is_some() {
            self.track_current_cloud_canvas(cx);
        }
        let key = target.unwrap_or_else(|| self.composer.read(cx).current_key.clone());
        self.composer.update(cx, |composer, cx| {
            composer.stage_appshot_for(key, appshot, cx)
        });
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn show_appshot_error(
        &mut self,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_route(Route::Chat, cx);
        self.composer
            .update(cx, |composer, cx| composer.show_appshot_error(message, cx));
        window.focus(&self.composer.focus_handle(cx), cx);
        cx.notify();
    }

    // ---- splash ----

    /// Open the chat onboarding asked for once the chat watch delivers it.
    fn open_pending_chat(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_open_chat.clone() else {
            return;
        };
        let Some(chat) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == id)
            .cloned()
        else {
            return;
        };
        self.pending_open_chat = None;
        self.state.update(cx, |state, cx| {
            state.select_space(chat.space_id.clone(), cx);
            state.select_chat(Some(chat.id), cx);
        });
        self.set_route(Route::Chat, cx);
    }

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        self.refresh_pr_watches(cx);
        self.open_pending_chat(cx);
        // Voice is an orchestrator over every thread: navigation keeps it,
        // only losing the engine that hosts it ends the session.
        if self.voice.read(cx).chat_id.is_some()
            && state
                .read(cx)
                .engine()
                .is_none_or(|engine| !self.voice.read(cx).belongs_to(engine))
        {
            self.voice.update(cx, |voice, cx| voice.cancel(cx));
        }
        self.prune_file_explorers(cx);
        self.refresh_harness_update_watch(cx);
        self.sync_search_warm(cx);
        if state.read(cx).engine().is_none() {
            self.side_chats.clear();
            self.side_chat_creating = false;
            self.thread_conversation_creating = false;
        }
        if let Some(notice) = state.update(cx, |state, _| state.take_deep_link_notice()) {
            self.show_notice(crate::toast::ToastKind::Info, notice, cx);
        }
        let next_sync_flow = {
            let state = state.read(cx);
            sync_flow_after_auth(self.sync_flow, state.workspace_scope, state.auth.as_ref())
        };
        if next_sync_flow != self.sync_flow {
            self.sync_flow = next_sync_flow;
            if matches!(
                self.sync_flow,
                SyncFlow::RestartPending { .. } | SyncFlow::SwitchOffer { .. }
            ) {
                self.org = None;
            }
        }
        // The in-place local→synced switch: once the replacement runtime is
        // attached and Ready, kick the import (or finish) from here.
        self.drive_sync_switch(cx);
        let signed_out_synced = {
            let state = state.read(cx);
            state.workspace_scope == Some(WorkspaceScope::Synced)
                && matches!(state.auth, Some(AuthState::SignedOut))
        };
        // AuthStatus is shared by every viewport. Whichever viewport owns the
        // embedded runtime drains it; remote viewports request daemon shutdown
        // and all of them independently reattach to the new local runtime.
        if signed_out_synced && self.runtime_change_task.is_none() {
            self.start_local_runtime_transition(false, cx);
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Capture knob: `ZUSE_DEMO_UPLOAD=<pct>:<image path>` — once a chat
        // is selected, push a fake sending echo carrying that image as a
        // pending attachment and freeze upload progress at <pct>, so the
        // thumbnail progress ring can be styled/screenshotted (a real upload
        // is too fast to pause).
        if let Some(spec) = self.debug_upload.clone()
            && let Some(chat_id) = state.read(cx).selected_chat.clone()
        {
            self.debug_upload = None;
            if let Some((pct, img_path)) = spec.split_once(':')
                && let Ok(pct) = pct.parse::<u64>()
                && let Ok(att) = crate::attachments::stage_file(std::path::Path::new(img_path))
            {
                let pending_path = format!("pending/{}/{}", att.id, att.name);
                let device_ids: Vec<String> = {
                    let s = state.read(cx);
                    s.selected_chat_row()
                        .map(|c| c.device_id.clone())
                        .into_iter()
                        .chain(s.local_device_id.clone())
                        .chain(Some("local".to_string()))
                        .collect()
                };
                for device_id in &device_ids {
                    crate::attachments::seed_staged(device_id, &pending_path, &att);
                }
                let text = crate::attachments::with_attachments(
                    "Here is the screenshot of the bug.",
                    std::slice::from_ref(&pending_path),
                );
                let echo = zuse_doc::SessionMessageEntry {
                    id: "demo-upload-echo".into(),
                    role: zuse_doc::MessageRole::User,
                    parts: vec![zuse_doc::MessagePart::Text {
                        id: "t0".into(),
                        text,
                    }],
                    created_at: chrono::Utc::now().timestamp_millis(),
                    device_id: "local".into(),
                    status: None,
                    continuation_of: None,
                    duration_ms: None,
                };
                state.update(cx, |s, cx| {
                    s.push_echo(&chat_id, echo);
                    s.begin_upload_progress(
                        100,
                        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pct)),
                    );
                    cx.notify();
                });
            }
        }
        // Banners and chimes share one session detector. Completion markers survive
        // queue handoffs and never advance for interrupts or stale activity.
        // A row's first appearance seeds the baseline silently (boot/replay).
        // Pending sends consume completion changes silently, while questions
        // still ring immediately. Output settings do not affect the baseline.
        {
            let now = Utc::now();
            type Ping = (
                String,
                crate::sound::SessionNotificationState,
                bool,
                Option<String>,
                bool,
            );
            let (sessions, connectivity, connectivity_observed) = {
                let state = state.read(cx);
                let sessions: Vec<Ping> = state
                    .sessions
                    .iter()
                    .map(|s| {
                        let status = crate::sound::SessionNotificationState::new(s, now);
                        let send_pending = state.send_pending(&s.chat_id, now);
                        let chat = state.chats.iter().find(|c| c.id == s.chat_id);
                        let title = chat.and_then(|c| c.title.clone());
                        let notify = chat.is_some_and(|c| c.is_top_level());
                        (s.chat_id.clone(), status, send_pending, title, notify)
                    })
                    .collect();
                (
                    sessions,
                    state.connectivity.state,
                    state.connectivity_observed,
                )
            };
            // Background-only banners: `active_window()` is app-level (any
            // Zuse window being key), so a ping for a *background chat* in a
            // focused app still stays a chime — you're already looking at
            // Zuse; the sidebar dot carries the rest.
            let app_focused = cx.active_window().is_some();
            for (chat_id, status, send_pending, title, notify) in sessions {
                let prev = self.sound_prev.insert(chat_id.clone(), status.clone());
                // Keep side-chat baselines current, but never emit their
                // completion, input-request, or failure sounds/banners.
                if notify
                    && let Some(prev) = prev
                    && let Some(sound) = status.sound_since(&prev, send_pending)
                {
                    if self.settings.session_sound_enabled(sound) {
                        let should_play = sound != crate::sound::Sound::Attention
                            || self
                                .attention_sound_gate
                                .should_play(std::time::Instant::now());
                        if should_play {
                            crate::sound::play_with_completion_preset(
                                sound,
                                self.settings.completion_sound_preset,
                            );
                        }
                    }
                    if self.settings.notifications_enabled
                        && !(self.settings.notifications_background_only && app_focused)
                    {
                        let title = title.unwrap_or_else(|| "New session".into());
                        let body = match sound {
                            crate::sound::Sound::Done => "Run finished",
                            crate::sound::Sound::Request => "Waiting on your input",
                            crate::sound::Sound::Attention => "Run failed",
                        };
                        crate::notify::post(&title, body, Some(&chat_id));
                    }
                }
            }
            if let Some(sound) = self.connectivity_notifications.update(
                connectivity,
                connectivity_observed,
                std::time::Instant::now(),
            ) {
                if self.settings.session_sound_enabled(sound)
                    && self
                        .attention_sound_gate
                        .should_play(std::time::Instant::now())
                {
                    crate::sound::play(sound);
                }
                if self.settings.notifications_enabled
                    && !(self.settings.notifications_background_only && app_focused)
                {
                    let body = match connectivity {
                        zuse_protocol::ConnectivityState::Offline => "Your device is offline",
                        _ => "Zuse is trying to reconnect",
                    };
                    crate::notify::post("Connection unavailable", body, None);
                }
            }
        }
        // Agent release discoveries finish independently (bounded provider
        // concurrency), so debounce the transition and deliver one aggregate
        // banner. Versioned releases deduplicate by version. Versionless
        // availability deduplicates until a confirmed Current/Updated status;
        // transient checks, disconnections and cancellation do not reset it.
        let has_unseen_harness_update = {
            let state = state.read(cx);
            let device = state.local_device_id.as_deref().unwrap_or("local");
            for status in &state.harness_updates {
                if matches!(
                    status.phase,
                    zuse_protocol::HarnessUpdatePhase::Current
                        | zuse_protocol::HarnessUpdatePhase::Updated
                ) {
                    self.harness_update_seen.remove(
                        &harness_updates::versionless_notification_key(device, status.harness),
                    );
                }
            }
            state.harness_updates.iter().any(|status| {
                harness_updates::notification_key(device, status)
                    .is_some_and(|key| !self.harness_update_seen.contains(&key))
            })
        };
        if has_unseen_harness_update && self.harness_update_banner_task.is_none() {
            self.harness_update_banner_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                this.update(cx, |this, cx| {
                    this.harness_update_banner_task = None;
                    let new: Vec<String> = {
                        let state = this.state.read(cx);
                        let device = state.local_device_id.as_deref().unwrap_or("local");
                        state
                            .harness_updates
                            .iter()
                            .filter_map(|status| harness_updates::notification_key(device, status))
                            .filter(|key| !this.harness_update_seen.contains(key))
                            .collect()
                    };
                    if new.is_empty() {
                        return;
                    }
                    let count = new.len();
                    for key in &new {
                        this.harness_update_seen.insert(key.clone());
                    }
                    let focused = cx.active_window().is_some();
                    if this.settings.notifications_enabled
                        && this.settings.agent_update_notifications
                        && !(this.settings.notifications_background_only && focused)
                    {
                        let body = if count == 1 {
                            "A coding agent update is ready"
                        } else {
                            "Coding agent updates are ready"
                        };
                        crate::notify::post(
                            &format!(
                                "{count} agent update{} available",
                                if count == 1 { "" } else { "s" }
                            ),
                            body,
                            Some(crate::notify::AGENT_UPDATES_TARGET),
                        );
                    }
                })
                .ok();
            }));
        }
        // An explicit projectless canvas must be visible in the sidebar:
        // retaining a project filter would hide the session on its first send.
        if state.read(cx).no_project
            && state.read(cx).selected_chat.is_none()
            && self.settings.space_filter.take().is_some()
        {
            self.schedule_save(cx);
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // canvas defaults (project AND its device) follow it, even
                // over a remembered "no project" opt-out. Otherwise the canvas's
                // own last pick stands (`last_space_id` also follows opened
                // chats, so it is only the fallback), unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(&exists);
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        crate::settings::composer::ComposerDefaults::load(&self.data_dir)
                            .project
                            .filter(&exists)
                            .or_else(|| self.settings.last_space_id.clone().filter(&exists))
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        self.reconcile_sidebar_pins(cx);
        if !self.pinned_session_drag_is_valid(cx) {
            self.cancel_pinned_session_drag(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat). The
        // new-chat canvas is the exception: it always lands with the terminal
        // hidden.
        let selected = self
            .conversation_state(cx)
            .read(cx)
            .selected_chat
            .clone()
            .unwrap_or_default();
        if !selected.is_empty() {
            self.last_appshot_chat = Some(selected.clone());
        }
        if selected != self.active_chat {
            self.suspend_file_images(cx);
            if !self.active_chat.is_empty() {
                let expanded = self.right_pane_expanded;
                self.panels
                    .update(&self.active_chat, |panels| panels.right_expanded = expanded);
            }
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // zuse's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.clear_panel_transitions();
            self.terminal_tween = None;
            self.fit_exits = [None; 3];
            let key = self.panel_key(cx);
            // Entering the new-chat canvas always lands with the terminal
            // hidden (user request) — a previously opened canvas drawer must
            // not pop open on a fresh canvas. The source chat's flag stays in
            // the map, so returning restores it.
            let panels = if self.active_chat.is_empty() {
                self.panels.update(&key, |panels| {
                    panels.terminal_open = false;
                });
                self.panels.get(&key)
            } else {
                self.panels.get(&key)
            };
            self.right_pane_expanded =
                !self.active_chat.is_empty() && panels.changes_open && panels.right_expanded;
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        let fit = self.horizontal_fit();
        if self.settings.sidebar_collapsed || !fit.sidebar {
            0.0
        } else {
            fit.sidebar_limit
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.conversation_state(cx).read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            crate::state::canvas_panel_key(self.state.read(cx).selected_space.as_deref())
        } else {
            self.active_chat.clone()
        }
    }

    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. Still hidden on the
    /// new-session canvas, where the titlebar carries no toggle to close it
    /// again (an earlier user request).
    fn right_pane_open(&self, cx: &App) -> bool {
        !self.active_chat.is_empty() && self.panels.get(&self.panel_key(cx)).changes_open
    }

    /// Open and on screen: the window fit is not hiding it.
    fn right_pane_shown(&self, cx: &App) -> bool {
        self.right_pane_open(cx) && self.horizontal_fit().right
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.cloud_conversation.is_none() && self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        self.right_target_for_columns(cx, self.sidebar_now(), self.files_reserved_width(cx))
    }

    /// The surface host's width once every running transition has landed.
    /// The initial target uses the sidebar and Files' settled allocation;
    /// the painted pane follows its live target while its neighbours move.
    fn right_settled_target(&self, cx: &App) -> f32 {
        self.right_target_for_columns(cx, self.sidebar_target(), self.files_settled_width(cx))
    }

    fn right_target_for_columns(&self, cx: &App, sidebar: f32, files: f32) -> f32 {
        if !self.right_pane_shown(cx) {
            0.0
        } else {
            // Manual sizing preserves a usable conversation column. Takeover
            // intentionally consumes it completely. Both ride the sidebar
            // tween so toggling it remains seamless.
            if self.right_pane_expanded {
                right_pane_takeover_width(self.viewport_width - files, sidebar)
            } else {
                self.settings.right_pane_width.min(right_pane_max_width(
                    self.viewport_width - files,
                    sidebar,
                    CHAT_PANEL_MIN,
                ))
            }
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        if !self.settings.sidebar_collapsed
            && self.promote_hidden_panel(AuxiliaryPanel::Sidebar, cx)
        {
            return;
        }
        let from = self.sidebar_now();
        let before = self.painted_panels(cx);
        self.sidebar_edge_bounce = None;
        self.sidebar_resize_edge = None;
        self.pane_resize_active = None;
        self.pane_resize_dragging = None;
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        // Record the open before targeting: the fit ranks columns by recency,
        // and an unrecorded sidebar would target the width of a hidden one.
        if !self.settings.sidebar_collapsed {
            self.record_panel_open(AuxiliaryPanel::Sidebar, &self.panel_key(cx));
        }
        self.sidebar_tween = Some(self.panel_tween(from, self.sidebar_target()));
        self.schedule_save(cx);
        self.resplit_panels(before, None, cx);
        cx.notify();
    }

    /// The user's pane toggle (titlebar button, keyboard). It drives only the
    /// surface host portion of the right pane: with just the explorer docked
    /// it opens the surface host beside it, and it never hides the explorer —
    /// only the explorer's own toggle undocks that portion.
    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        if matches!(self.route, Route::Chat) && self.selected_zot(cx).is_some() {
            self.zot_ui.panel_hidden = !self.zot_ui.panel_hidden;
            cx.notify();
            return;
        }
        if self.right_pane_open(cx) && self.promote_hidden_panel(AuxiliaryPanel::Right, cx) {
            return;
        }
        self.set_surfaces_open(!self.right_pane_open(cx), cx);
    }

    /// Closing the last surface tab closes the surface host; a docked
    /// explorer keeps the pane open on its own.
    fn collapse_surfaces_if_empty(&mut self, panel_key: &str, cx: &mut Context<Self>) {
        if panel_key == self.panel_key(cx)
            && self.right_tabs.get(panel_key).is_none_or(Vec::is_empty)
        {
            self.set_surfaces_open(false, cx);
        }
    }

    /// Show or hide the surface host portion of the right pane. A no-op when
    /// already in the requested state, so programmatic opens (a file, a
    /// browser link, a subagent chip) never close a pane the user has open.
    /// An open pane the window fit hides is brought forward instead, so the
    /// requested content is reachable.
    fn set_surfaces_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.active_chat.is_empty() {
            return;
        }
        if self.right_pane_open(cx) == open {
            if open {
                self.promote_hidden_panel(AuxiliaryPanel::Right, cx);
            }
            return;
        }
        // Reverse from the visible width when toggled during an animation.
        let before = self.painted_panels(cx);
        let from = before.right;
        let content_from = before.right_content;
        self.right_edge_bounce = None;
        self.right_resize_edge = None;
        self.finish_pane_resize(PaneResizeKind::Right);
        let sidebar_now = self.sidebar_now();
        let from_main = self.main_content_width(self.main_target_width(from, cx), cx);
        let was_expanded = self.right_pane_expanded || self.tween_active(self.main_takeover_tween);
        let key = self.panel_key(cx);
        if open {
            self.record_panel_open(AuxiliaryPanel::Right, &key);
            // Its previews load again; the next hide releases them anew.
            self.right_images_suspended = false;
        } else {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        self.panels.update(&key, |p| p.changes_open = open);
        self.resplit_panels(before, Some(AuxiliaryPanel::Right), cx);
        let to = self.right_settled_target(cx);
        let pane = self.panel_tween(from, to);
        self.right_tween = Some(pane);
        let (content_start, content_end) = panel_content_tween(content_from, from, to);
        self.right_content_tween = Some(WidthTween {
            from: content_start,
            to: content_end,
            ..pane
        });
        // The conversation's return rides the pane's own tween.
        self.main_takeover_tween = was_expanded.then(|| WidthTween {
            from: from_main,
            to: conversation_width(
                self.viewport_width - self.files_reserved_width(cx),
                sidebar_now,
                to,
            ),
            ..pane
        });
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }

    fn right_terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(view) = self.cloud_conversation.clone() {
            return view.update(cx, |view, cx| {
                if let Some(terminal) = &view.terminal {
                    return terminal.clone();
                }
                let terminal = cx.new(|cx| TerminalPanel::new_embedded(view.state.clone(), cx));
                view.terminal = Some(terminal.clone());
                terminal
            });
        }
        if let Some(terminal) = &self.right_terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new_embedded(self.state.clone(), cx));
        self.right_terminal = Some(terminal.clone());
        terminal
    }

    /// The right pane's surface tabs in the STORED (drag-reorderable) order —
    /// `(surface, title)`; entries whose backing tab/entity is gone are
    /// skipped.
    fn right_surface_rows(
        &self,
        cx: &App,
    ) -> Vec<(RightSurface, SharedString, bool, Option<SharedString>)> {
        let key = self.panel_key(cx);
        let stored: &[RightSurface] = self
            .right_tabs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let terminal = self
            .cloud_conversation
            .as_ref()
            .and_then(|view| view.read(cx).terminal.clone())
            .or_else(|| {
                self.cloud_conversation
                    .is_none()
                    .then(|| self.right_terminal.clone())
                    .flatten()
            });
        let terminals: Vec<(u64, SharedString, bool)> = terminal
            .as_ref()
            .map(|t| t.read(cx).tab_summaries(cx))
            .unwrap_or_default();
        stored
            .iter()
            .filter_map(|surface| match surface {
                RightSurface::File(id) => self.file_surfaces.get(id).map(|file| {
                    let path = self.file_surface_paths.get(id);
                    let title = path
                        .map(|path| workspace_file_title(path))
                        .unwrap_or_else(|| SharedString::from("File"));
                    (
                        *surface,
                        title,
                        file.read(cx).has_unsaved_changes(),
                        path.cloned().map(Into::into),
                    )
                }),
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    // Contextual title (user request): the pane's scope
                    // label, or the pinned commit's subject.
                    .map(|changes| (*surface, changes.read(cx).tab_title(), false, None)),
                RightSurface::Terminal(tab) => terminals
                    .iter()
                    .find(|(k, _, _)| k == tab)
                    .map(|(_, title, _)| (*surface, title.clone(), false, None)),
                RightSurface::SideChat(id) => self.side_chats.get(id).map(|tab| {
                    let title = tab
                        .state
                        .read(cx)
                        .selected_chat_row()
                        .and_then(|c| c.title.clone())
                        .unwrap_or_else(|| "Side chat".into());
                    (*surface, title.into(), false, None)
                }),
                RightSurface::Subagent(id) => self
                    .subagent_tabs
                    .get(id)
                    .map(|tab| (*surface, tab.title.clone(), false, None)),
                RightSurface::Simulator(id) => self
                    .simulators
                    .get(id)
                    .map(|_| (*surface, SharedString::from("Simulator"), false, None)),
                RightSurface::Browser(id) => self.browsers.get(id).map(|browser| {
                    let browser = browser.read(cx);
                    (
                        *surface,
                        browser.title(),
                        false,
                        browser.page.url.clone().map(Into::into),
                    )
                }),
                RightSurface::PullRequest(id) => self
                    .chat_pull_request_title(*id, cx)
                    .map(|(title, detail)| (*surface, title, false, Some(detail))),
                RightSurface::Files => self
                    .files
                    .contains_key(&key)
                    .then(|| (*surface, SharedString::from("Files"), false, None)),
                RightSurface::Picker => None,
            })
            .collect()
    }

    /// Attach a workspace path only while the surface it came from still
    /// shows this chat's workspace; a drag outliving a session switch is dropped.
    fn attach_workspace_drag(
        &mut self,
        payload: &WorkspacePathDrag,
        composer: &Entity<Composer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(origin) = &payload.origin else {
            return;
        };
        let current = crate::files::client::FilesRequestContext::for_chat(
            self.state.read(cx),
            &self.panel_key(cx),
        );
        if current.as_ref() != Some(&origin.context) || !matches!(self.route, Route::Chat) {
            return;
        }
        let valid = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .any(|files| {
                files.entity_id() == origin.surface_id && files.read(cx).accepts_origin(origin, cx)
            });
        if valid {
            composer.update(cx, |composer, cx| {
                for item in payload.items() {
                    composer.add_workspace_path(&item.path, item.is_directory, window, cx)
                }
            });
        }
    }

    fn workspace_path_for_surface(
        &self,
        surface: RightSurface,
        cx: &App,
    ) -> Option<WorkspacePathDrag> {
        let path = match surface {
            RightSurface::File(id) => self.file_surface_paths.get(&id)?.clone(),
            RightSurface::Picker
            | RightSurface::Files
            | RightSurface::Diff(_)
            | RightSurface::Terminal(_)
            | RightSurface::SideChat(_)
            | RightSurface::Subagent(_)
            | RightSurface::Simulator(_)
            | RightSurface::Browser(_)
            | RightSurface::PullRequest(_) => {
                return None;
            }
        };
        let RightSurface::File(id) = surface else {
            return None;
        };
        let origin = self.file_surfaces.get(&id)?.read(cx).interaction_origin(cx);
        Some(WorkspacePathDrag::new(path, false).with_origin(
            origin,
            crate::files::WorkspacePathSource::FileTab,
            None,
        ))
    }

    /// Drag-reorder a surface tab within this chat's strip.
    fn reorder_right_tabs(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key)
            && from < tabs.len()
            && to < tabs.len()
            && from != to
        {
            let surface = tabs.remove(from);
            tabs.insert(to, surface);
            cx.notify();
        }
    }

    /// Track the hovered drop slot mid-drag (the terminal drawer's
    /// `update_drag_over`, ported: epoch bumps restart the slide tween).
    fn update_right_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.right_tab_drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.right_tab_drag = Some(RightTabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// The surface that actually renders: the stored pick when it still
    /// exists, else the most recently visited live tab, else the picker.
    /// Terminal keys go stale when their tab closes/exits — never render a dead surface.
    fn resolved_right_active(&self, cx: &App) -> RightSurface {
        let picked = self.panels.get(&self.panel_key(cx)).right_active;
        let rows = self.right_surface_rows(cx);
        let exists = match picked {
            RightSurface::Picker => true,
            surface => rows.iter().any(|(s, _, _, _)| *s == surface),
        };
        if exists {
            picked
        } else {
            self.panels.right_surface_fallback(
                &self.panel_key(cx),
                rows.iter().map(|(surface, _, _, _)| *surface),
            )
        }
    }

    /// Release image previews of the right pane's surfaces. The file tab on
    /// screen in the main area keeps its own.
    fn suspend_file_images(&mut self, cx: &mut Context<Self>) {
        let on_screen = self
            .active_main_file(cx)
            .and_then(|id| self.file_surfaces.get(&id))
            .map(|file| file.entity_id());
        for files in self.files.values().chain(self.file_surfaces.values()) {
            if Some(files.entity_id()) != on_screen {
                files.update(cx, |files, cx| files.suspend_images(cx));
            }
        }
    }

    /// Release image previews once the surface host stops painting: after
    /// its close mask lands, while the window fit hides it, or off the chat
    /// route. Once per hide, so a quick reversal never unloads the frame.
    fn suspend_hidden_right_images(&mut self, cx: &mut Context<Self>) {
        let painting = matches!(self.route, Route::Chat)
            && ((self.horizontal_fit().right
                && (self.right_pane_open(cx) || self.tween_active(self.right_tween)))
                || self.fit_exit_width(1).is_some());
        if painting {
            self.right_images_suspended = false;
        } else if !self.right_images_suspended {
            self.suspend_file_images(cx);
            self.right_images_suspended = true;
        }
    }

    fn set_right_active(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        if let RightSurface::Subagent(id) = surface
            && let Some(tab) = self.subagent_tabs.get(&id)
        {
            let doc_id = tab.doc_id.clone();
            self.state
                .update(cx, |state, cx| state.focus_subagent_sync(&doc_id, cx));
        }
        if self.resolved_right_active(cx) != surface {
            self.suspend_file_images(cx);
        }
        let key = self.panel_key(cx);
        self.panels.update(&key, |p| p.right_active = surface);
        match surface {
            RightSurface::File(id) => {
                if let Some(file) = self.file_surfaces.get(&id).cloned() {
                    file.update(cx, |file, cx| file.ensure_loaded(cx));
                    if let Some(path) = self.file_surface_paths.get(&id).cloned() {
                        let chat_id = file.read(cx).chat_id().to_owned();
                        self.note_recent_file(&chat_id, &path);
                    }
                }
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                self.composer
                    .update(cx, |composer, _| composer.focus_pending = false);
                panel.update(cx, |panel, cx| {
                    panel.select_tab_by_key(tab, cx);
                    panel.request_focus(cx);
                });
            }
            RightSurface::Diff(id) => {
                if let Some(changes) = self.diffs.get(&id).cloned() {
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                }
            }
            // The tab's feed (watch or snapshot) runs from open to close —
            // activation needs no revalidation.
            RightSurface::SideChat(id) => {
                if let Some(tab) = self.side_chats.get(&id) {
                    tab.composer.update(cx, |composer, cx| {
                        composer.focus_pending = true;
                        cx.notify();
                    });
                }
            }
            RightSurface::Files => {
                if let Some(files) = self.files.get(&key).cloned() {
                    files.update(cx, |files, cx| files.ensure_loaded(cx));
                }
            }
            RightSurface::Simulator(_)
            | RightSurface::Subagent(_)
            | RightSurface::Browser(_)
            | RightSurface::PullRequest(_) => {}
            RightSurface::Picker => {}
        }
        self.sync_explorer_selection(cx);
        cx.notify();
    }

    fn focus_right_file_editor(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let RightSurface::Browser(id) = surface {
            if let Some(browser) = self.browsers.get(&id).cloned() {
                browser.update(cx, |browser, cx| browser.focus_address(window, cx));
            }
            return;
        }
        let files = match surface {
            RightSurface::File(id) => self.file_surfaces.get(&id).cloned(),
            _ => None,
        };
        if let Some(files) = files {
            files.update(cx, |files, cx| {
                files.focus_editor(window, cx);
            });
        }
    }

    fn set_files_word_wrap(
        &mut self,
        word_wrap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.files_word_wrap = word_wrap;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_word_wrap(word_wrap, cx));
        }
        let surfaces = self.file_surfaces.values().cloned().collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_word_wrap(word_wrap, window, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Push a new code size into every open file surface. Called by the
    /// Appearance settings page, which owns the control. The typography
    /// global is the canonical store and persists on its own; this only
    /// propagates the change to already-open surfaces.
    pub(crate) fn set_code_font_size(&mut self, code_font_size: f32, cx: &mut Context<Self>) {
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_editor_font_size(code_font_size, cx)
            });
        }
        cx.notify();
    }

    fn set_files_show_all(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        self.settings.files_show_all = show_all_files;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_show_all_files(show_all_files, cx));
        }
        let surfaces = self
            .file_surfaces
            .values()
            .chain(self.files.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_show_all_files(show_all_files, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn session_links(
        source_session: Option<String>,
        cx: &Context<Self>,
    ) -> crate::markdown::render::LinkUi {
        let shell = cx.weak_entity();
        crate::markdown::render::LinkUi {
            source_session,
            source_local: false,
            file_roots: None,
            handler: std::rc::Rc::new(move |activation, window, cx| {
                shell
                    .update(cx, |shell, cx| {
                        shell.activate_session_link(activation, window, cx)
                    })
                    .unwrap_or(crate::markdown::render::LinkOutcome::Rejected)
            }),
        }
    }

    /// Whether `chat_id` is a side chat open among the current conversation's
    /// right-pane tabs.
    fn side_chat_open_here(&self, chat_id: &str, cx: &App) -> bool {
        self.right_tabs
            .get(&self.panel_key(cx))
            .is_some_and(|tabs| {
                tabs.iter().any(|tab| match tab {
                    RightSurface::SideChat(id) => self.side_chats.get(id).is_some_and(|side| {
                        side.state.read(cx).selected_chat.as_deref() == Some(chat_id)
                    }),
                    _ => false,
                })
            })
    }

    fn activate_session_link(
        &mut self,
        activation: &crate::markdown::render::LinkActivation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> crate::markdown::render::LinkOutcome {
        use crate::markdown::render::{LinkAction, LinkOutcome};
        let Some(source) = activation.source_session.clone() else {
            return LinkOutcome::Rejected;
        };
        let from_main = !self.active_chat.is_empty()
            && source == self.active_chat
            && self.state.read(cx).selected_chat.as_deref() == Some(self.active_chat.as_str());
        if !from_main && !self.side_chat_open_here(&source, cx) {
            return LinkOutcome::Rejected;
        }
        if activation.target.navigation.is_err() {
            return if matches!(
                activation.action,
                LinkAction::Primary | LinkAction::Internal
            ) && self.open_workspace_file_link(
                &source,
                &activation.target.original,
                window,
                cx,
            ) {
                LinkOutcome::Internal
            } else {
                LinkOutcome::Rejected
            };
        }
        // A pull request link opens Zuse's own view in this session's pane,
        // unless Settings sends pull requests to the browser.
        if matches!(
            activation.action,
            LinkAction::Primary | LinkAction::Internal
        ) && crate::pull_request_detail::opens_natively(cx)
            && self.chat_pull_requests_available()
            && let Some(url) = activation
                .target
                .navigation
                .as_deref()
                .ok()
                .and_then(crate::pull_request_detail::pull_request_link)
        {
            let target = self.chat_pull_request_device(None, cx);
            self.open_chat_pull_request(url, target, window, cx);
            return LinkOutcome::Internal;
        }
        let mut resolved = activation.clone();
        if resolved.action == LinkAction::Primary {
            resolved.action = if crate::settings::current(cx).open_web_links_in_zuse {
                LinkAction::Internal
            } else {
                LinkAction::External
            };
        }
        let outcome =
            resolved.web_outcome(cfg!(any(target_os = "macos", target_os = "linux", windows)));
        if outcome == LinkOutcome::Internal {
            self.set_surfaces_open(true, cx);
            self.add_browser_surface(activation.target.navigation.clone().ok(), window, cx);
        }
        outcome
    }

    fn sync_browser_profile(&mut self, cx: &mut Context<Self>) {
        let browser_profile = {
            let state = self.state.read(cx);
            crate::browser::profile::BrowserProfile::for_workspace(
                &self.data_dir,
                state.workspace_scope,
                state.auth.as_ref(),
                state.local_device_id.as_deref(),
            )
        };
        if browser_profile != self.browser_profile {
            for browser in self.browsers.values() {
                browser.update(cx, |browser, cx| browser.close(cx));
            }
            self.browsers.clear();
            self.browser_subs.clear();
            self.browser_context = browser_profile
                .clone()
                .map(crate::browser::BrowserContext::for_profile)
                .unwrap_or_default();
            self.pull_request_cache = Default::default();
            self.pull_requests_page = None;
            self.reset_pull_request_pane();
            // Their tabs read as closed; rows skip a missing surface.
            self.chat_pull_requests.clear();
            self.browser_profile = browser_profile;
        }
    }

    /// Each chat owns one simulator picker/viewer tab.
    fn add_simulator_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(surface) = self
            .right_tabs
            .get(&key)
            .and_then(|tabs| {
                tabs.iter()
                    .find(|s| matches!(s, RightSurface::Simulator(_)))
            })
            .copied()
        {
            self.set_right_active(surface, cx);
            return;
        }
        let state = self.state.read(cx);
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let Some(chat) = state.selected_chat_row() else {
            return;
        };
        let (chat_id, host_id) = (chat.id.clone(), chat.device_id.clone());
        self.simulator_seq += 1;
        let id = self.simulator_seq;
        let pane =
            cx.new(|cx| crate::simulator::SimulatorPane::new(engine, chat_id, host_id, window, cx));
        self.simulators.insert(id, pane);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Simulator(id));
        self.set_right_active(RightSurface::Simulator(id), cx);
    }

    /// Browser tabs are independent instances owned by the current session.
    fn add_browser_surface(
        &mut self,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_chat.is_empty() {
            return;
        }
        self.sync_browser_profile(cx);
        let key = self.panel_key(cx);
        let remote = {
            let state = self.state.read(cx);
            state.selected_chat_row().is_some_and(|chat| {
                Some(chat.device_id.as_str()) != state.local_device_id.as_deref()
            })
        };
        self.browser_seq += 1;
        let id = self.browser_seq;
        let browser = cx.new(|cx| {
            crate::browser::BrowserSurface::new(self.browser_context.clone(), remote, window, cx)
        });
        if let Some(handle) = self.state.read(cx).engine().cloned() {
            let chat_id = self.active_chat.clone();
            browser.update(cx, |browser, cx| {
                browser.watch_previews(handle, chat_id, cx)
            });
        }
        let owner = key.clone();
        let sub = cx.subscribe_in(&browser, window, move |this, _, event, window, cx| {
            match event {
                crate::browser::BrowserEvent::Changed => cx.notify(),
                crate::browser::BrowserEvent::NewTab(url) => {
                    // A background page cannot open a tab in the wrong session.
                    if this.panel_key(cx) == owner
                        && this.resolved_right_active(cx) == RightSurface::Browser(id)
                    {
                        this.add_browser_surface(url.clone(), window, cx);
                    }
                }
                crate::browser::BrowserEvent::ElementSelected(details) => {
                    if this.panel_key(cx) == owner
                        && this.resolved_right_active(cx) == RightSurface::Browser(id)
                    {
                        let composer = this.composer.clone();
                        let details = details.clone();
                        let draft_key = composer.read(cx).current_key.clone();
                        cx.defer(move |cx| {
                            composer.update(cx, |composer, cx| {
                                composer.stage_browser_annotation(draft_key, details, cx);
                            })
                        });
                    }
                }
                crate::browser::BrowserEvent::AnnotationRemoved(id) => {
                    if this.panel_key(cx) == owner {
                        let composer = this.composer.clone();
                        let draft_key = composer.read(cx).current_key.clone();
                        let id = id.clone();
                        cx.defer(move |cx| {
                            composer.update(cx, |composer, cx| {
                                composer.remove_browser_annotation(draft_key, &id, cx)
                            })
                        });
                    }
                }
                crate::browser::BrowserEvent::Close => {
                    this.close_right_surface(RightSurface::Browser(id), window, cx)
                }
            }
        });
        self.browsers.insert(id, browser.clone());
        self.browser_subs.insert(id, sub);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Browser(id));
        self.set_right_active(RightSurface::Browser(id), cx);
        browser.update(cx, |browser, cx| {
            if let Some(url) = url {
                browser.navigate(&url, window, cx);
            } else {
                browser.focus_address(window, cx);
            }
        });
    }

    /// A right-pane diff tab with its own scope/base selection. The picker's
    /// Diffs card and the `+` menu open the main-area review instead (see
    /// [`Self::open_review_tab`]); tests still drive the pane's diff tabs.
    #[cfg(test)]
    fn add_diff_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.register_diff_surface(changes, window, cx);
    }

    /// Open or focus a session-owned editor tab. The explorer is independent.
    fn add_file_surface(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let owner = (self.active_chat.clone(), self.conversation_state(cx));
        self.add_file_surface_at(owner, path, None, window, cx);
    }

    /// The chat and state a transcript link resolves in: the main chat's, or
    /// an open side chat's own (its chat row may not reach the main list yet).
    fn link_owner(&self, chat_id: &str, cx: &App) -> Option<(String, Entity<AppState>)> {
        if chat_id == self.active_chat {
            return Some((chat_id.to_owned(), self.conversation_state(cx)));
        }
        self.side_chats
            .values()
            .find(|side| side.state.read(cx).selected_chat.as_deref() == Some(chat_id))
            .map(|side| (chat_id.to_owned(), side.state.clone()))
    }

    /// Open or focus the editor for `path` owned by `owner` (chat id + the
    /// state that resolves it): reads and saves go to that chat's checkout.
    fn add_file_surface_at(
        &mut self,
        owner: (String, Entity<AppState>),
        path: String,
        location: Option<(u32, Option<FileColumn>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_chat.is_empty() || owner.0.is_empty() {
            return;
        }
        let (owner_chat, owner_state) = owner;
        // Files open as main-area tabs beside the conversation; the right
        // pane keeps the tree.
        let panel_key = self.panel_key(cx);
        let lookup = (panel_key.clone(), owner_chat.clone(), path.clone());
        if let Some(id) = self.file_surface_keys.get(&lookup).copied() {
            self.push_main_file_tab(&panel_key, id, cx);
            if let Some((line, column)) = location
                && let Some(file) = self.file_surfaces.get(&id).cloned()
            {
                file.update(cx, |file, cx| file.navigate_to_line(line, column, cx));
            }
            self.focus_main_tab(Some(main_tabs::MainTab::File(id)), window, cx);
            return;
        }

        self.file_surface_seq += 1;
        let id = self.file_surface_seq;
        let file = cx.new(|cx| {
            FilesSurface::new_editor(
                owner_state.clone(),
                owner_chat.clone(),
                path.clone(),
                self.settings.files_autosave_enabled,
                self.settings.files_autosave_delay_ms,
                crate::typography::code_font_size(cx),
                self.settings.files_word_wrap,
                self.settings.files_show_all,
                cx,
            )
        });
        let event_panel_key = panel_key.clone();
        let sub = cx.subscribe_in(
            &file,
            window,
            move |this: &mut Self, source, event, window, cx| {
                if matches!(event, FilesEvent::OpenFile(_) | FilesEvent::RevealFile(_))
                    && !this.accepts_file_navigation(&event_panel_key, &source, cx)
                    && !(this.active_main_file(cx) == Some(id)
                        && source.read(cx).is_current_target(cx))
                {
                    return;
                }
                match event {
                    FilesEvent::HoldMutation { origin, paths } => {
                        let surfaces = this
                            .files
                            .values()
                            .chain(this.file_surfaces.values())
                            .filter(|s| s.read(cx).shares_workspace(origin))
                            .cloned()
                            .collect::<Vec<_>>();
                        for surface in surfaces {
                            surface.update(cx, |files, cx| files.hold_mutation(paths.clone(), cx));
                        }
                    }
                    FilesEvent::AddToChat {
                        path,
                        is_directory,
                        origin,
                    } => {
                        let payload = WorkspacePathDrag::new(path.clone(), *is_directory)
                            .with_origin(
                                Some(origin.clone()),
                                crate::files::WorkspacePathSource::Tree,
                                None,
                            );
                        let composer = this.composer.clone();
                        this.attach_workspace_drag(&payload, &composer, window, cx);
                    }
                    FilesEvent::Mutate(intent) => {
                        this.start_file_mutation(source.clone(), intent.clone(), cx)
                    }
                    FilesEvent::MutateMany(intents) => {
                        this.start_file_mutations(source.clone(), intents.clone(), cx)
                    }
                    // Navigation from an editor stays in its own chat.
                    FilesEvent::OpenFile(path) => {
                        let owner = (source.read(cx).chat_id().to_owned(), owner_state.clone());
                        this.add_file_surface_at(owner, path.clone(), None, window, cx)
                    }
                    FilesEvent::RevealFile(path) => {
                        this.add_files_surface(window, cx);
                        if let Some(files) = this.files.get(&this.panel_key(cx)).cloned() {
                            files.update(cx, |files, cx| {
                                files.reveal_file_explicit(path.clone(), cx)
                            });
                        }
                    }
                    FilesEvent::OpenWebLink(activation) => {
                        if let crate::markdown::render::LinkOutcome::External(url) =
                            this.activate_session_link(activation, window, cx)
                        {
                            cx.open_url(&url);
                        }
                    }
                    FilesEvent::TitleChanged => cx.notify(),
                    FilesEvent::FileRenamed { old_path, new_path } => {
                        this.rename_file_surface(id, &event_panel_key, old_path, new_path, cx)
                    }
                    FilesEvent::WordWrapChanged(word_wrap) => {
                        this.set_files_word_wrap(*word_wrap, window, cx)
                    }
                    FilesEvent::ShowAllFilesChanged(show_all_files) => {
                        this.set_files_show_all(*show_all_files, cx)
                    }
                    FilesEvent::CloseReady => {
                        this.on_file_close_ready(RightSurface::File(id), &event_panel_key, cx)
                    }
                    FilesEvent::CloseCancelled => {
                        this.cancel_file_close(RightSurface::File(id), cx)
                    }
                }
            },
        );
        self.file_surfaces.insert(id, file);
        self.file_surface_paths.insert(id, path);
        self.file_surface_keys.insert(lookup, id);
        self.file_surface_subs.insert(id, sub);
        self.push_main_file_tab(&panel_key, id, cx);
        if let Some((line, column)) = location
            && let Some(file) = self.file_surfaces.get(&id).cloned()
        {
            file.update(cx, |file, cx| file.navigate_to_line(line, column, cx));
        }
        self.focus_main_tab(Some(main_tabs::MainTab::File(id)), window, cx);
    }

    /// Open a transcript's file link in the linking chat's own checkout: a
    /// side chat's link resolves against, and edits, the side chat's files.
    /// An absolute path the linking checkout cannot own falls through the
    /// chat's parent and this device's project roots before it is treated
    /// as a host file; the first known root that owns the path wins. A chat
    /// root opens the file in that chat's context; a project root and a host
    /// file open by absolute path through the linking chat.
    fn open_workspace_file_link(
        &mut self,
        chat_id: &str,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let owner_state = self
            .link_owner(chat_id, cx)
            .map(|(_, state)| state)
            .unwrap_or_else(|| self.state.clone());
        let roots = owner_state.read(cx).file_link_roots(chat_id);
        let root_refs: Vec<&str> = roots.iter().map(|root| root.root.as_str()).collect();
        use crate::workspace_links::FileLinkResolution;
        let (owner_chat, link) = match crate::workspace_links::first_root_owning(target, root_refs)
        {
            Some(FileLinkResolution::Owned { root, link }) => match roots[root].chat.as_deref() {
                Some(owner) => (owner.to_owned(), link),
                // A linking chat without a checkout of its own resolves in
                // its own file context, as before.
                None if root == 0 => (chat_id.to_owned(), link),
                // A project root past the linking chat's own has no chat of
                // its own, and its relative path would name a different file
                // under the linking chat's checkout: read it by absolute path
                // through the linking chat, which keeps it editable only
                // inside that checkout.
                None => (
                    chat_id.to_owned(),
                    crate::workspace_links::WorkspaceFileLink {
                        path: roots[root].absolute(&link).to_string_lossy().into_owned(),
                        outside: true,
                        ..link
                    },
                ),
            },
            // An absolute path no known root owns is still a file link: it
            // opens read-only through the linking chat's own file context.
            Some(FileLinkResolution::Outside(link)) => (chat_id.to_owned(), link),
            None => return false,
        };
        let Some(owner) = self.link_owner(&owner_chat, cx) else {
            return false;
        };

        self.add_file_surface_at(
            owner,
            link.path,
            link.line
                .map(|line| (line, link.column.map(FileColumn::Character))),
            window,
            cx,
        );
        true
    }

    fn rename_file_surface(
        &mut self,
        id: u64,
        panel_key: &str,
        old_path: &str,
        new_path: &str,
        cx: &mut Context<Self>,
    ) {
        if self.file_surface_paths.get(&id).map(String::as_str) != Some(old_path) {
            return;
        }
        self.file_surface_paths.insert(id, new_path.to_string());
        let Some(owner) = self
            .file_surfaces
            .get(&id)
            .map(|file| file.read(cx).chat_id().to_owned())
        else {
            return;
        };
        self.file_surface_keys.remove(&(
            panel_key.to_string(),
            owner.clone(),
            old_path.to_string(),
        ));
        self.file_surface_keys
            .entry((panel_key.to_string(), owner, new_path.to_string()))
            .or_insert(id);
        cx.notify();
    }

    /// The dedicated History surface. Keeping it as its own tab preserves its
    /// graph/search state while Diff tabs retain their ordinary scope picker.
    fn add_history_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let history = cx.new(|cx| Changes::for_history(self.state.clone(), cx));
        self.register_diff_surface(history, window, cx);
    }

    fn register_diff_surface(
        &mut self,
        changes: Entity<Changes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.subscribe_diff(changes, window, cx);
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Diff(id));
        self.set_right_active(RightSurface::Diff(id), cx);
    }

    /// Keep `changes` under a fresh diff handle (a key of [`Self::diffs`]),
    /// wired to the shell. Right-pane tabs and main-area tabs share this.
    fn subscribe_diff(
        &mut self,
        changes: Entity<Changes>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> u64 {
        self.diff_seq += 1;
        let id = self.diff_seq;
        let sub =
            cx.subscribe_in(
                &changes,
                window,
                |this: &mut Self, _, event, window, cx| match event {
                    // A commit's diff reads like a review: it opens beside
                    // the conversation in the main area.
                    ChangesEvent::OpenCommit(commit) => {
                        this.open_commit_diff_tab(commit.clone(), window, cx);
                    }
                    ChangesEvent::OpenFile(path) => {
                        this.add_file_surface(path.clone(), window, cx);
                    }
                    ChangesEvent::DiscardWorkingTree(request) => {
                        if this.discard_working_tree_task.is_none() {
                            this.discard_working_tree =
                                Some(DiscardWorkingTreeFlow::Confirm(request.clone()));
                            cx.notify();
                        }
                    }
                },
            );
        self.diffs.insert(id, changes);
        self.diff_subs.insert(id, sub);
        id
    }

    /// The picker's Terminal card / the `+` menu's Terminal row: every click
    /// opens a fresh embedded terminal tab.
    fn add_terminal_surface(&mut self, cx: &mut Context<Self>) {
        if self
            .cloud_conversation
            .as_ref()
            .is_some_and(|view| !view.read(cx).ready(cx))
        {
            return;
        }
        let panel = self.right_terminal_panel(cx);
        let opened = panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.open_tab_for_selected(cx)
        });
        if let Some(tab) = opened {
            let key = self.panel_key(cx);
            self.right_tabs
                .entry(key)
                .or_default()
                .push(RightSurface::Terminal(tab));
            self.set_right_active(RightSurface::Terminal(tab), cx);
        }
    }

    /// Spawn-chip events from the primary transcript AND from subagent-tab
    /// transcripts (nested spawns open their own tabs).
    fn on_transcript_event(
        &mut self,
        transcript: Entity<Transcript>,
        event: &TranscriptEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TranscriptEvent::OpenSubagent {
                chat_id,
                doc_id,
                title,
                frozen,
            } => {
                self.add_subagent_surface(
                    chat_id.clone(),
                    doc_id.clone(),
                    title.clone(),
                    *frozen,
                    cx,
                );
            }
            TranscriptEvent::OpenWorkflowAgent {
                row_id,
                agent_index,
                title,
            } => {
                self.add_workflow_agent_surface(
                    transcript,
                    row_id.clone(),
                    *agent_index,
                    title.clone(),
                    cx,
                );
            }
            // A reply's Fork menu: a new tab in this thread, or a new thread.
            TranscriptEvent::ForkChat { chat_id, target } => {
                let source = self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|chat| &chat.id == chat_id)
                    .cloned();
                if let Some(source) = source {
                    self.fork_chat(source, *target, cx);
                }
            }
            TranscriptEvent::FindClosed => self.focus_transcript_composer(&transcript, cx),
            TranscriptEvent::SendUiBlockReply { row_id, text } => {
                let sent = self.send_transcript_reply(&transcript, text.clone(), cx);
                transcript.update(cx, |transcript, cx| {
                    transcript.ui_block_reply_sent(row_id, sent, cx)
                });
            }
        }
    }

    /// A workflow board's agent row: open that agent as a subagent tab. A
    /// workflow agent has no doc of its own, so its conversation (what it was
    /// asked, what it returned) is built from the run's progress and kept in
    /// step with it; the tab then reads exactly like any other transcript.
    fn add_workflow_agent_surface(
        &mut self,
        transcript: Entity<Transcript>,
        row_id: SharedString,
        index: u32,
        title: String,
        cx: &mut Context<Self>,
    ) {
        self.set_surfaces_open(true, cx);
        let doc_id = format!("workflow-agent:{row_id}:{index}");
        if let Some((&id, _)) = self
            .subagent_tabs
            .iter()
            .find(|(_, tab)| tab.doc_id == doc_id)
        {
            self.set_right_active(RightSurface::Subagent(id), cx);
            return;
        }
        let Some(snapshot) = transcript.read(cx).workflow_agent(&row_id, index) else {
            return;
        };
        let device = self
            .state
            .read(cx)
            .local_device_id
            .clone()
            .unwrap_or_default();
        let entries = crate::workflow_board::agent_entries(&snapshot, &device);
        let live = matches!(
            snapshot.agent.status,
            crate::workflow_board::AgentStatus::Working
                | crate::workflow_board::AgentStatus::Queued
        );
        self.state.update(cx, |state, cx| {
            state.set_subagent_snapshot(doc_id.clone(), entries.clone());
            cx.notify();
        });
        let last = std::cell::RefCell::new(entries);
        let state = self.state.clone();
        let feed_doc = doc_id.clone();
        let feed = cx.observe(&transcript, move |_, source, cx| {
            let Some(snapshot) = source.read(cx).workflow_agent(&row_id, index) else {
                return;
            };
            let entries = crate::workflow_board::agent_entries(&snapshot, &device);
            if *last.borrow() == entries {
                return;
            }
            last.replace(entries.clone());
            state.update(cx, |state, cx| {
                state.set_subagent_snapshot(feed_doc.clone(), entries);
                cx.notify();
            });
        });
        self.subagent_seq += 1;
        let id = self.subagent_seq;
        let view = cx.new(|cx| Transcript::for_doc(self.state.clone(), doc_id.clone(), live, cx));
        let links = Self::session_links(Some(self.active_chat.clone()), cx);
        view.update(cx, |view, _| view.set_workspace_link_handler(links));
        let events = cx.subscribe(&view, Self::on_transcript_event);
        self.subagent_tabs.insert(
            id,
            SubagentTab {
                doc_id,
                // Workflows are a Claude Code feature.
                harness: Some(zuse_protocol::HarnessId::ClaudeCode),
                title: title.into(),
                transcript: view,
                _fetch: None,
                _events: events,
                _feed: Some(feed),
            },
        );
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Subagent(id));
        self.set_right_active(RightSurface::Subagent(id), cx);
    }

    /// A spawn chip's "Open subagent": focus the existing tab for that doc,
    /// or open one. `frozen` (subagent done/failed) tries the uploaded
    /// transcript blob first and falls back to the live doc watch; running
    /// subagents watch the doc directly.
    fn add_subagent_surface(
        &mut self,
        chat_id: String,
        doc_id: String,
        title: String,
        frozen: bool,
        cx: &mut Context<Self>,
    ) {
        // The chip lives in the conversation column — the pane it opens into
        // may still be closed.
        self.set_surfaces_open(true, cx);
        if let Some((&id, _)) = self
            .subagent_tabs
            .iter()
            .find(|(_, tab)| tab.doc_id == doc_id)
        {
            self.set_right_active(RightSurface::Subagent(id), cx);
            return;
        }
        self.subagent_seq += 1;
        let id = self.subagent_seq;
        let harness = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
            .or_else(|| {
                self.side_chats.values().find_map(|tab| {
                    tab.state
                        .read(cx)
                        .selected_chat_row()
                        .filter(|chat| chat.id == chat_id)
                        .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
                })
            })
            .or_else(|| {
                // Nested spawn chips identify their parent subagent doc.
                self.subagent_tabs
                    .values()
                    .find(|tab| tab.doc_id == chat_id)
                    .and_then(|tab| tab.harness)
            });
        // A live subagent follows its streaming end (main-transcript feel);
        // a frozen one reads top-down.
        let transcript =
            cx.new(|cx| Transcript::for_doc(self.state.clone(), doc_id.clone(), !frozen, cx));
        let links = Self::session_links(Some(self.active_chat.clone()), cx);
        transcript.update(cx, |transcript, _| {
            transcript.set_workspace_link_handler(links)
        });
        let events = cx.subscribe(&transcript, Self::on_transcript_event);
        let fetch = if frozen {
            self.spawn_subagent_snapshot_fetch(&chat_id, &doc_id, cx)
        } else {
            self.state
                .update(cx, |s, cx| s.watch_subagent_doc(doc_id.clone(), cx));
            None
        };
        self.subagent_tabs.insert(
            id,
            SubagentTab {
                doc_id,
                harness,
                title: title.into(),
                transcript,
                _fetch: fetch,
                _events: events,
                _feed: None,
            },
        );
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Subagent(id));
        self.set_right_active(RightSurface::Subagent(id), cx);
    }

    /// Fetch a finished subagent's frozen transcript blob
    /// (`{chat_id}/{doc_id}`); on ANY failure fall back to watching the doc
    /// — the blob upload is best-effort engine-side.
    fn spawn_subagent_snapshot_fetch(
        &self,
        chat_id: &str,
        doc_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.state
                .update(cx, |s, cx| s.watch_subagent_doc(doc_id.to_string(), cx));
            return None;
        };
        // Copied spawn chips retain their original subagent doc namespace.
        let source_chat = doc_id
            .split_once("--sub--")
            .map(|(source, _)| source)
            .unwrap_or(chat_id);
        let blob_ref = format!("{source_chat}/{doc_id}");
        let target = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == source_chat)
            .map(|chat| chat.device_id.clone());
        let state = self.state.clone();
        let doc_id = doc_id.to_string();
        Some(cx.spawn(async move |_, cx| {
            let reply = crate::attachments::call_with_timeout(
                &engine,
                cx.background_executor(),
                methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": blob_ref, "targetDeviceId": target }),
                Duration::from_secs(20),
            )
            .await;
            let snapshot = cx
                .background_executor()
                .spawn(async move {
                    let value = reply.ok()?;
                    let entries: Vec<zuse_doc::SessionMessageEntry> =
                        serde_json::from_str(value.get("text")?.as_str()?).ok()?;
                    let update = zuse_doc::TranscriptUpdate {
                        replay_baseline: Some(zuse_doc::TranscriptBaseline::capture(&entries)),
                        frame: zuse_doc::TranscriptFrame::Reset {
                            reset: entries.into(),
                        },
                        context_usage: None,
                    };
                    let prepared = crate::transcript::TranscriptPreparation::default()
                        .prepare(&update)
                        .ok()?;
                    let zuse_doc::TranscriptFrame::Reset { reset } = update.frame else {
                        unreachable!()
                    };
                    Some((std::sync::Arc::unwrap_or_clone(reset), prepared))
                })
                .await;
            state.update(cx, |s, cx| {
                match snapshot {
                    Some((entries, prepared)) => {
                        s.set_prepared_subagent_snapshot(doc_id, entries, prepared);
                    }
                    None => s.watch_subagent_doc(doc_id, cx),
                }
                cx.notify();
            });
        }))
    }

    /// Snapshot the displayed order before closing tabs mutates it.
    fn tabs_to_close(
        &self,
        surface: RightSurface,
        action: TabCloseAction,
        cx: &App,
    ) -> Vec<RightSurface> {
        let tabs: Vec<_> = self
            .right_surface_rows(cx)
            .into_iter()
            .map(|(tab, _, _, _)| tab)
            .collect();
        let Some(index) = tabs.iter().position(|tab| *tab == surface) else {
            return Vec::new();
        };
        tabs.into_iter()
            .enumerate()
            .filter_map(|(i, tab)| {
                let close = match action {
                    TabCloseAction::This => i == index,
                    TabCloseAction::Others => i != index,
                    TabCloseAction::Left => i < index,
                    TabCloseAction::Right => i > index,
                };
                close.then_some(tab)
            })
            .collect()
    }

    /// Close one surface through its normal lifecycle, including unsaved-file prompts.
    fn close_right_surface(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self.resolved_right_active(cx) == surface;
        let restore_focus = was_active && self.navigation_focus.in_right(window, cx);
        self.navigation_focus
            .remember(&self.shortcut_focus, window, cx);
        let key = self.panel_key(cx);
        // File editors live in the main area; their close asks there.
        if let RightSurface::File(id) = surface {
            self.close_main_tab(main_tabs::MainTab::File(id), cx);
            return;
        }
        if let Some(tabs) = self.right_tabs.get_mut(&key) {
            tabs.retain(|s| *s != surface);
        }
        match surface {
            RightSurface::File(_) => {}
            RightSurface::Simulator(id) => {
                if let Some(pane) = self.simulators.remove(&id) {
                    pane.update(cx, |pane, cx| pane.close(cx));
                }
            }
            RightSurface::Browser(id) => {
                if let Some(browser) = self.browsers.remove(&id) {
                    browser.update(cx, |browser, cx| browser.close(cx));
                }
                self.browser_subs.remove(&id);
            }
            RightSurface::Diff(id) => {
                // Dropping the entity tears down its diff watch.
                self.diffs.remove(&id);
                self.diff_subs.remove(&id);
            }
            RightSurface::PullRequest(id) => {
                // A comment being posted outlives its view; see
                // `PullRequestCache::submissions`.
                self.chat_pull_requests.remove(&id);
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.close_tab_by_key(tab, window, cx));
            }
            RightSurface::SideChat(id) => {
                // An unsent draft outlives the tab: the side chat stays
                // loaded, detached, and reopening it restores the draft. An
                // unsaved side chat has no row to reopen it from.
                if self.side_chats.get(&id).is_none_or(|side| {
                    side.state.read(cx).side_chat_unsaved() || !side.composer.read(cx).has_draft(cx)
                }) {
                    self.side_chats.remove(&id);
                }
            }
            RightSurface::Subagent(id) => {
                // Unwatch drops the watch task — that cancels the engine-side
                // watch and unpins the subagent doc from the engine LRU.
                if let Some(tab) = self.subagent_tabs.remove(&id) {
                    self.state
                        .update(cx, |s, _| s.unwatch_subagent_doc(&tab.doc_id));
                }
            }
            // The tree keeps its expansion and scroll for the next open.
            RightSurface::Files => {
                if let Some(files) = self.files.get(&key).cloned() {
                    files.update(cx, |files, cx| files.suspend_tree_interactions(cx));
                }
            }
            RightSurface::Picker => {}
        }
        self.close_empty_right_pane(&key, cx);
        self.panels.forget_right_surface(&key, surface);
        let fallback = self.panels.right_surface_fallback(
            &key,
            self.right_tabs.get(&key).into_iter().flatten().copied(),
        );
        self.panels.update(&key, |p| {
            if p.right_active == surface {
                p.right_active = fallback;
            }
        });
        self.collapse_surfaces_if_empty(&key, cx);
        if restore_focus {
            self.restore_right_focus_after_close(window, cx);
        }
        cx.notify();
    }

    fn on_file_close_ready(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pending_file_closes.contains(&surface) {
            self.complete_file_close(surface, panel_key, cx);
        } else {
            if self.pending_exit.is_some() {
                self.reveal_unsaved_file(cx);
            }
            cx.notify();
        }
    }

    fn cancel_file_close(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        self.pending_file_closes.remove(&surface);
        self.pending_exit = None;
        cx.notify();
    }

    pub(crate) fn cloud_pause_can_close(&self, cx: &App) -> bool {
        !self.cloud_pause_has_unsaved_work(cx) && !self.composer.read(cx).cloud_has_draft(cx)
    }

    pub(crate) fn cloud_pause_has_unsaved_work(&self, cx: &App) -> bool {
        !self.all_file_edits_flushed(cx)
            || self
                .side_chats
                .values()
                .any(|side| side.composer.read(cx).has_draft(cx))
    }

    pub fn prepare_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.prepare_exit(PendingExit::CloseWindow, cx)
    }

    /// The first rung of `⌘W` / Window > Close Window: when the right pane is
    /// open on a real surface (the file / diff / terminal / browser tab the
    /// user just opened), close THAT and leave the window alone. Returns true
    /// when the close was consumed by the pane.
    ///
    /// The pane's empty picker state and a closed pane both yield false, so the
    /// caller falls through to [`Self::prepare_window_close`] and the window
    /// closes — the same cascade browsers use. The native traffic-light close
    /// deliberately skips this rung: it always closes the window.
    pub fn close_active_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // The Pull requests route's pane holds pull request tabs instead.
        if matches!(self.route, Route::PullRequests) {
            return self.close_active_pull_request_tab(cx);
        }
        let right = self.closable_right_surface(cx);
        // The focused side wins: the right pane's surface while focus is in
        // it, else the main area's file or diff tab, else the pane's.
        if let Some(surface) = right
            && self.navigation_focus.in_right(window, cx)
        {
            self.close_right_surface(surface, window, cx);
            return true;
        }
        if let Some(tab) = self.active_main_tab(cx) {
            self.close_main_tab(tab, cx);
            return true;
        }
        let Some(surface) = right else {
            return false;
        };
        self.close_right_surface(surface, window, cx);
        true
    }

    /// The right pane's closable surface for the `⌘W` cascade: the resolved
    /// active surface while the pane is open, or `None` on the picker empty
    /// state / a closed pane / the new-session canvas.
    fn closable_right_surface(&self, cx: &App) -> Option<RightSurface> {
        if !self.right_pane_shown(cx) {
            return None;
        }
        match self.resolved_right_active(cx) {
            RightSurface::Picker => None,
            surface => Some(surface),
        }
    }

    pub fn prepare_quit(&mut self, cx: &mut Context<Self>) -> bool {
        self.prepare_exit(PendingExit::Quit, cx)
    }

    fn prepare_exit(&mut self, action: PendingExit, cx: &mut Context<Self>) -> bool {
        let surfaces = self.file_surfaces.values().cloned().collect::<Vec<_>>();
        if surfaces
            .iter()
            .all(|surface| !surface.read(cx).has_unsaved_changes())
        {
            self.pending_exit = None;
            return true;
        }
        self.pending_exit = Some(action);
        let mut all_ready = true;
        for surface in surfaces {
            let disposition = surface.update(cx, |surface, cx| surface.prepare_close(cx));
            all_ready &= disposition == FilesCloseDisposition::Allow;
        }
        if all_ready {
            self.pending_exit = None;
        } else {
            self.reveal_unsaved_file(cx);
        }
        cx.notify();
        all_ready
    }

    fn reveal_unsaved_file(&mut self, cx: &mut Context<Self>) {
        let editors = self
            .file_surface_keys
            .iter()
            .filter_map(|((key, _, _), id)| {
                self.file_surfaces
                    .get(id)
                    .filter(|files| files.read(cx).has_unsaved_changes())
                    .map(|_| (key.clone(), RightSurface::File(*id)))
            });
        let current = self.panel_key(cx);
        let mut dirty = editors.collect::<Vec<_>>();
        dirty.sort_by_key(|(key, _)| (key != &current, key.clone()));
        if let Some((key, RightSurface::File(id))) = dirty.into_iter().next() {
            // Its tab stands in for the conversation of the chat it belongs to.
            self.select_main_file_tab(&key, id, cx);
            self.apply_nav(NavEntry::Chat(key), cx);
        }
    }

    fn all_file_edits_flushed(&self, cx: &App) -> bool {
        self.file_surfaces
            .values()
            .all(|surface| !surface.read(cx).has_unsaved_changes())
    }

    /// Drop a file editor once it may close. File editors are main-area
    /// tabs: closing one never touches the right pane.
    fn complete_file_close(
        &mut self,
        surface: RightSurface,
        _panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        let RightSurface::File(id) = surface else {
            return;
        };
        self.file_surfaces.remove(&id);
        self.file_surface_paths.remove(&id);
        self.file_surface_subs.remove(&id);
        self.file_surface_keys.retain(|_, value| *value != id);
        self.pending_file_closes.remove(&surface);
        self.forget_main_file_tab(id, cx);
        cx.notify();
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (zuse `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Cloud tools belong to the task's right sidebar, never the local
        // controller's bottom terminal. Reuse its existing terminal tab.
        if let Some(conversation) = &self.cloud_conversation {
            if !conversation.read(cx).ready(cx) {
                return;
            }
            let terminal =
                self.right_surface_rows(cx)
                    .into_iter()
                    .find_map(|(surface, _, _, _)| {
                        matches!(surface, RightSurface::Terminal(_)).then_some(surface)
                    });
            if let Some(terminal) = terminal {
                if self.right_pane_open(cx) && self.resolved_right_active(cx) == terminal {
                    self.toggle_right_pane(cx);
                } else {
                    self.set_right_active(terminal, cx);
                }
            } else {
                self.add_terminal_surface(cx);
            }
            return;
        }
        let from = self.terminal_geometry.get().height;
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            self.composer
                .update(cx, |composer, _| composer.focus_pending = false);
            panel.update(cx, |panel, cx| panel.request_focus(cx));
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (zuse terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in zuse's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        let requested = anchor_h + dy;
        let max = self
            .terminal_geometry
            .get()
            .limit
            .min(viewport_h * TERMINAL_MAX_VH);
        let min = TERMINAL_MIN_HEIGHT.min(max);
        self.settings.terminal_height = clamp_terminal_height(requested.min(max), viewport_h);
        self.pane_resize_dragging = Some(PaneResizeKind::Terminal);
        self.pane_resize_active =
            (requested > min && requested < max).then_some(PaneResizeKind::Terminal);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        let sample = sidebar_drag_sample(x, self.sidebar_resize_edge, self.reduced_motion);
        let was_collapsed = self.settings.sidebar_collapsed;
        self.settings.sidebar_width = sample.width;
        self.settings.sidebar_collapsed = false;
        self.pane_resize_dragging = Some(PaneResizeKind::Sidebar);
        self.sidebar_tween = None; // live drag tracks the pointer directly
        if sample.starts_bounce {
            self.sidebar_edge_bounce = sample.edge.map(motion::ResizeEdgeBounce::new);
        } else if sample.edge.is_none() {
            self.sidebar_edge_bounce = None;
        }
        self.pane_resize_active = sample.edge.is_none().then_some(PaneResizeKind::Sidebar);
        self.sidebar_resize_edge = sample.edge;
        self.schedule_save(cx);
        if was_collapsed {
            self.record_panel_open(AuxiliaryPanel::Sidebar, &self.panel_key(cx));
        }
        cx.notify();
    }

    fn contain_pinned_session_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarSessionDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(transfer) = self.sidebar_session_transfer.as_mut() {
            transfer.pointer = event.event.position;
            cx.notify();
        }
        let inside_window = event.bounds.contains(&event.event.position);
        let pointer_x = f32::from(event.event.position.x);
        let sidebar_left = f32::from(event.bounds.left());
        let inside_sidebar =
            pointer_x >= sidebar_left && pointer_x <= sidebar_left + self.sidebar_now();
        if !inside_window || !inside_sidebar {
            if let Some(transfer) = self.sidebar_session_transfer.as_mut() {
                transfer.preview = None;
            }
            self.cancel_pinned_session_drag(cx);
        }
    }

    fn finish_pane_resize(&mut self, kind: PaneResizeKind) {
        if self.pane_resize_active == Some(kind) {
            self.pane_resize_active = None;
        }
        if self.pane_resize_dragging == Some(kind) {
            self.pane_resize_dragging = None;
        }
        match kind {
            PaneResizeKind::Sidebar => self.sidebar_resize_edge = None,
            PaneResizeKind::Terminal => self.terminal_drag_anchor = None,
            PaneResizeKind::Right => self.right_resize_edge = None,
            PaneResizeKind::PullRequests => self.pull_request_pane.resize_edge = None,
        }
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - self.files_reserved_width(cx) - f32::from(event.event.position.x);
        // Use the same shared budget as rendering, including compact windows.
        let max = self.surface_max_width(cx);
        let sample = if max >= RIGHT_PANE_MIN {
            motion::resize_drag_sample(
                width,
                RIGHT_PANE_MIN,
                max,
                self.right_resize_edge,
                self.reduced_motion,
            )
        } else {
            motion::ResizeDragSample {
                width: max,
                edge: None,
                starts_bounce: false,
            }
        };
        self.settings.right_pane_width = sample.width;
        self.pane_resize_dragging = Some(PaneResizeKind::Right);
        if sample.starts_bounce {
            self.right_edge_bounce = sample.edge.map(motion::ResizeEdgeBounce::new);
        } else if sample.edge.is_none() {
            self.right_edge_bounce = None;
        }
        self.pane_resize_active = sample.edge.is_none().then_some(PaneResizeKind::Right);
        self.right_resize_edge = sample.edge;
        self.right_tween = None;
        self.right_content_tween = None;
        self.main_takeover_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Publish this view's working copy to the central settings store. The
    /// store owns the single debounce task and the only production writer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.git_history_columns = crate::history::configured_columns(cx);
        self.settings.git_history_column_widths = crate::history::configured_column_widths(cx);
        self.settings.git_history_column_order = crate::history::configured_column_order(cx);
        self.settings.git_history_author_display = crate::history::configured_author_display(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        self.pull_settings(cx);
        settings::replace(self.settings.clone(), SavePolicy::Debounced, cx);
        self.settings_base = settings::current(cx);
    }

    /// Controls outside the Shell (Voice, typography, diff and transcript
    /// toggles, …) save to the central store directly. Fold those into the
    /// working copy while keeping the Shell's own edits, so a geometry save
    /// can never publish an older value over another surface's choice.
    fn pull_settings(&mut self, cx: &App) {
        let current = settings::current(cx);
        self.settings =
            UiSettings::merge_changes(&self.settings_base, &self.settings, current.clone());
        self.settings_base = current;
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        if self
            .state
            .read(cx)
            .engine()
            .is_some_and(|engine| engine.retry_connection())
        {
            return;
        }
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    fn set_route(&mut self, route: Route, cx: &mut Context<Self>) {
        if matches!(self.route, Route::Settings(SettingsSection::Archived))
            && !matches!(route, Route::Settings(SettingsSection::Archived))
        {
            if let Some(page) = self.archived_page.clone() {
                page.update(cx, |page, cx| page.on_hidden(cx));
            }
        }
        self.command_palette = None;
        let was_pull_requests = matches!(self.route, Route::PullRequests);
        let will_show_pull_requests = matches!(route, Route::PullRequests);
        if was_pull_requests && !will_show_pull_requests {
            if let Some(page) = self.pull_requests_page.as_ref().cloned() {
                page.update(cx, |page, _| page.on_hidden());
            }
        }
        if matches!(self.route, Route::Archives) && !matches!(route, Route::Archives) {
            if let Some(page) = self
                .archive_project
                .as_ref()
                .and_then(|id| self.project_archives.get(id))
                .cloned()
            {
                page.update(cx, |page, cx| page.on_hidden(cx));
            }
        }
        // The model-usage dashboard shows on the Usage route and on
        // Settings → Usage → Models; it loads on entry and stops on exit.
        let shows_usage = |route: Route, compute: bool| {
            matches!(route, Route::Usage)
                || (route == Route::Settings(SettingsSection::Usage) && !compute)
        };
        let compute = self.usage_settings_compute;
        if shows_usage(self.route, compute) && !shows_usage(route, compute) {
            if let Some(dashboard) = self.usage_dashboard.clone() {
                dashboard.update(cx, |dashboard, cx| dashboard.hide(cx));
            }
        }
        if shows_usage(route, compute) && !shows_usage(self.route, compute)
            || matches!(route, Route::Usage) && !matches!(self.route, Route::Usage)
        {
            self.usage_pending = true;
        }
        self.route = route;
        if !was_pull_requests && will_show_pull_requests {
            if let Some(page) = self.pull_requests_page.as_ref().cloned() {
                page.update(cx, |page, cx| page.on_visible(cx));
            }
        }
    }

    /// Close the user menu through the exit animation (no-op when closed).
    fn close_user_menu(&mut self, cx: &mut Context<Self>) {
        if self.user_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.user_menu);
            cx.notify();
        }
    }

    /// Close the session-row context menu through the exit animation.
    /// Whether the open chat menu belongs to `chat_id`'s row (not a tab).
    pub(super) fn chat_menu_targets(&self, chat_id: &str) -> bool {
        self.chat_menu
            .get()
            .is_some_and(|menu| menu.tab.is_none() && menu.chat_id == chat_id)
    }

    fn close_chat_menu(&mut self, cx: &mut Context<Self>) {
        self.chat_copy_intent.reset();
        if self.chat_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.chat_menu);
            cx.notify();
        }
    }

    /// A sidebar-level notice (a failed mutation, a copied link) as a toast.
    /// The latest message is kept so a pin-write failure can be matched later.
    pub(super) fn show_notice(
        &mut self,
        kind: crate::toast::ToastKind,
        message: impl Into<SharedString>,
        cx: &mut App,
    ) {
        let message = message.into();
        self.sidebar_notice = Some(message.clone());
        crate::toast::show(cx, kind, message);
    }

    fn show_archive_error(&mut self, chat_id: String, message: String, cx: &mut Context<Self>) {
        self.sidebar_notice = Some(message.clone().into());
        let shell = cx.weak_entity();
        crate::toast::error_with_action(
            cx,
            message,
            chat_id.clone(),
            "Force delete…",
            move |_, cx| {
                let _ = shell.update(cx, |this, cx| {
                    if !this.deleting_chat
                        && this
                            .state
                            .read(cx)
                            .chats
                            .iter()
                            .any(|chat| chat.id == chat_id)
                    {
                        this.delete_confirm = Some(chat_id.clone());
                        cx.notify();
                    }
                });
            },
        );
    }

    fn set_chat_copy_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let page = if open {
            ChatMenuPage::Copy
        } else {
            ChatMenuPage::Root
        };
        if let Some(menu) = self.chat_menu.open_mut()
            && menu.page != page
        {
            menu.page = page;
            if !open {
                self.chat_copy_bounds = None;
            }
            cx.notify();
        }
    }

    /// Pointer intent over a chat-menu row: hovering Copy opens its submenu,
    /// any other row closes it — after a grace while the pointer is heading
    /// into the open submenu.
    fn hover_chat_menu_row(
        &mut self,
        key: u8,
        pointer: Point<Pixels>,
        moved: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .chat_menu
            .as_open()
            .filter(|menu| menu.page == ChatMenuPage::Copy)
            .map(|_| CHAT_MENU_ROW_COPY);
        let (bounds, left) = (self.chat_copy_bounds, self.chat_copy_left);
        let action = if moved {
            self.chat_copy_intent
                .moved(current.as_ref(), &key, pointer, bounds, left)
        } else {
            self.chat_copy_intent
                .enter(current.as_ref(), &key, pointer, bounds, left)
        };
        match action {
            popover::HoverAction::None => {}
            popover::HoverAction::Open => {
                self.set_chat_copy_open(key == CHAT_MENU_ROW_COPY, cx);
                self.chat_copy_intent.record_origin(pointer);
            }
            popover::HoverAction::Defer => {
                self.chat_copy_intent.defer(cx, move |this, cx| {
                    if this.chat_menu.is_open() && this.chat_copy_intent.pending() == Some(&key) {
                        this.chat_copy_intent.cancel();
                        this.set_chat_copy_open(key == CHAT_MENU_ROW_COPY, cx);
                        this.chat_copy_intent.record_origin(pointer);
                    }
                });
            }
        }
    }

    /// Wrap a chat-menu row in the hover surface that feeds
    /// [`Self::hover_chat_menu_row`]. The row keeps its own hover fade.
    fn chat_menu_hover_row(
        &self,
        key: u8,
        row: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(("chat-menu-hover", key as usize))
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                if *hovered {
                    this.hover_chat_menu_row(key, window.mouse_position(), false, cx);
                } else {
                    this.chat_copy_intent.leave(&key);
                }
            }))
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.hover_chat_menu_row(key, event.position, true, cx);
                }),
            )
            .child(row)
            .into_any_element()
    }

    /// The chat menu's Copy row: opens its submenu beside the menu on hover
    /// (or click), on whichever side has room.
    fn render_chat_copy_trigger(
        &mut self,
        chat_id: &str,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity().downgrade();
        let exit_entity = entity.clone();
        let mut row =
            popover::menu_row_nav(theme, open, false, format!("chat-menu-copy-{chat_id}"))
                .id("chat-menu-copy")
                .relative()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chat_copy_intent.reset();
                    this.set_chat_copy_open(true, cx);
                    cx.stop_propagation();
                }))
                .child(
                    icon(icons::COPY)
                        .size(px(16.0))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().child(SharedString::from("Copy")))
                .child(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(14.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    gpui::canvas(
                        move |bounds, window, cx| {
                            let left = bounds.right() + px(244.0) > window.viewport_size().width;
                            let _ = entity.update(cx, |this, cx| {
                                if this.chat_copy_left != left {
                                    this.chat_copy_left = left;
                                    cx.notify();
                                }
                            });
                        },
                        move |trigger, _, window, _| {
                            if !open {
                                return;
                            }
                            // Leaving the trigger anywhere but toward the submenu
                            // closes it, as in the sidebar options menu.
                            window.on_mouse_event(
                                move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                    if phase != gpui::DispatchPhase::Bubble {
                                        return;
                                    }
                                    let _ = exit_entity.update(cx, |this, cx| {
                                        let (bounds, left) =
                                            (this.chat_copy_bounds, this.chat_copy_left);
                                        if this.chat_copy_intent.contains_pointer(
                                            trigger,
                                            bounds,
                                            event.position,
                                            left,
                                        ) {
                                            return;
                                        }
                                        this.chat_copy_intent.reset();
                                        this.set_chat_copy_open(false, cx);
                                    });
                                },
                            );
                        },
                    )
                    .absolute()
                    .inset_0(),
                );
        if open {
            let submenu = self.render_chat_copy_submenu(chat_id, theme, cx);
            row = row.child(popover::nested_menu(
                format!("chat-copy-submenu-{chat_id}"),
                submenu,
                self.chat_copy_left,
            ));
        }
        row.into_any_element()
    }

    /// The Copy submenu's card: what can be copied for this chat.
    fn render_chat_copy_submenu(
        &mut self,
        chat_id: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chat = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .cloned();
        let harness_link = chat
            .as_ref()
            .and_then(crate::links::harness_conversation_link);
        let session_id = chat
            .as_ref()
            .and_then(|chat| chat.harness_session_id.as_deref())
            .is_some_and(|id| !id.trim().is_empty());
        let has_path = chat.as_ref().and_then(chat_copy_path).is_some();
        let zuse_id = chat_id.to_owned();
        let harness_id = chat_id.to_owned();
        let session_chat_id = chat_id.to_owned();
        let path_chat_id = chat_id.to_owned();
        let item = |id: &'static str, label: SharedString| {
            popover::menu_row(theme, false, format!("{id}-{chat_id}"))
                .id(id)
                .debug_selector(move || id.to_owned())
                .child(
                    icon(icons::COPY)
                        .size(px(16.0))
                        .text_color(theme.text_muted),
                )
                .child(label)
        };
        let entity = cx.entity().downgrade();
        popover::popover_card(theme)
            .w(px(232.0))
            .relative()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .when(has_path, |menu| {
                menu.child(item("chat-copy-path", "Path".into()).on_click(
                    cx.listener(move |this, _, _, cx| this.copy_chat_path(&path_chat_id, cx)),
                ))
            })
            .child(
                item("chat-copy-zuse", "Zuse conversation link".into()).on_click(cx.listener(
                    move |this, _, _, cx| this.copy_zuse_conversation_link(&zuse_id, cx),
                )),
            )
            .when_some(harness_link, |menu, link| {
                menu.child(
                    item("chat-copy-harness", link.label.into()).on_click(cx.listener(
                        move |this, _, _, cx| this.copy_harness_conversation_link(&harness_id, cx),
                    )),
                )
            })
            .when(session_id, |menu| {
                menu.child(
                    item("chat-copy-session", "Harness session ID".into()).on_click(cx.listener(
                        move |this, _, _, cx| this.copy_harness_session_id(&session_chat_id, cx),
                    )),
                )
            })
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        let _ = entity.update(cx, |this, _| this.chat_copy_bounds = Some(bounds));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    fn copy_zuse_conversation_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let link = {
            let state = self.state.read(cx);
            crate::links::workspace_locator(
                state.workspace_scope,
                state.auth.as_ref(),
                state.local_device_id.as_deref(),
            )
            .map(|workspace| crate::links::zuse_conversation_link(chat_id, &workspace))
        };
        if let Some(link) = link {
            cx.write_to_clipboard(ClipboardItem::new_string(link));
            self.show_notice(
                crate::toast::ToastKind::Success,
                "Zuse conversation link copied",
                cx,
            );
        } else {
            self.show_notice(
                crate::toast::ToastKind::Error,
                "Conversation link is not ready yet",
                cx,
            );
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_harness_conversation_link(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let link = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(crate::links::harness_conversation_link);
        if let Some(link) = link {
            cx.write_to_clipboard(ClipboardItem::new_string(link.url));
            self.show_notice(
                crate::toast::ToastKind::Success,
                format!("{} copied", link.label),
                cx,
            );
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_harness_session_id(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let id = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.harness_session_id.clone());
        if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(id));
            self.show_notice(
                crate::toast::ToastKind::Success,
                "Harness session ID copied",
                cx,
            );
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_chat_path(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let path = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(chat_copy_path)
            .map(str::to_owned);
        if let Some(path) = path {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
            self.show_notice(crate::toast::ToastKind::Success, "Path copied", cx);
        }
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn ensure_appearance_page(&mut self, cx: &mut Context<Self>) {
        if self.appearance_page.is_none() {
            let page = cx.new(AppearancePage::new);
            self.appearance_settings_sub = Some(cx.subscribe(
                &page,
                |this: &mut Shell, _, event: &AppearanceSettingsEvent, cx| match *event {
                    AppearanceSettingsEvent::CodeFontSizeChanged(size) => {
                        this.set_code_font_size(size, cx);
                    }
                },
            ));
            self.appearance_page = Some(page);
        }
    }

    fn random_wallpaper(&mut self, cx: &mut Context<Self>) {
        self.ensure_appearance_page(cx);
        if settings::current(cx).wallpaper_folder.is_none() {
            self.open_settings(SettingsSection::Appearance, cx);
            if let Some(page) = &self.appearance_page {
                page.update(cx, |page, cx| page.choose_wallpaper_folder(cx));
            }
            return;
        }
        let task = settings::wallpaper::randomize(cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| {
                    this.open_settings(SettingsSection::Appearance, cx);
                    if let Some(page) = &this.appearance_page {
                        page.update(cx, |page, cx| page.show_wallpaper_error(error, cx));
                    }
                });
            }
        })
        .detach();
    }

    pub(crate) fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        let section = section.canonical();
        self.command_palette = None;
        // Re-probe installed providers on each visit while preserving active sign-ins.
        if section == SettingsSection::Harnesses {
            if let Some(page) = &self.harnesses_page {
                page.update(cx, |page, cx| page.refresh_on_visit(cx));
            }
        }
        if !matches!(self.route, Route::Settings(_)) {
            self.settings_focus_pending = true;
            self.settings_return_route = self.route;
        }
        self.set_route(Route::Settings(section), cx);
        self.remember_settings_section(section, cx);
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Generic entry points (⌘, / Ctrl+,, the footer gear, the palette,
    /// `/settings`) reopen the section last viewed. Links that name a
    /// section go through [`Self::open_settings`] instead.
    fn open_last_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings(self.settings.settings_section.reopenable(), cx);
    }

    /// Every section shown is the one to reopen. Written through the shell's
    /// own settings copy: [`Self::schedule_save`] replaces the whole file
    /// from it, so a write that bypassed it would be undone by the next save.
    fn remember_settings_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        let section = section.canonical();
        if self.settings.settings_section != section {
            self.settings.settings_section = section;
            self.schedule_save(cx);
        }
    }

    /// Escape bubbling out of the settings page. Focused controls — open
    /// dropdowns, the shortcut recorder, a dialog's own input — consume it
    /// before it gets here. What remains open without holding focus closes
    /// first: the account menu, a sync prompt, then the routed page's dialog
    /// or login flow. Returns whether one did; only then does Settings stay.
    fn dismiss_settings_escape_surface(&mut self, cx: &mut Context<Self>) -> bool {
        if self.sync_flow.has_visible_overlay() {
            return true;
        }
        if self.user_menu.is_open() {
            self.close_user_menu(cx);
            return true;
        }
        if self.user_menu.get().is_some() {
            return true;
        }
        let Route::Settings(section) = self.route else {
            return false;
        };
        match section.canonical() {
            SettingsSection::Defaults => self
                .defaults_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_on_escape(cx))),
            SettingsSection::Devices => self
                .devices_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_on_escape(cx))),
            SettingsSection::Harnesses => self
                .harnesses_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_on_escape(cx))),
            SettingsSection::Appearance => self
                .appearance_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_on_escape(cx))),
            SettingsSection::Archived => self
                .archived_page
                .as_ref()
                .is_some_and(|page| page.update(cx, |page, cx| page.dismiss_on_escape(cx))),
            _ => false,
        }
    }

    /// The Pull requests entry: from elsewhere, return to the board as it
    /// was left, its pane included; on the board already, show it alone.
    fn open_pull_requests(&mut self, cx: &mut Context<Self>) {
        let entry = if matches!(self.route, Route::PullRequests) {
            self.dismiss_pull_request_detail(cx);
            NavEntry::PullRequests
        } else {
            self.pull_request_pane_entry()
        };
        self.enter_pull_requests_route(cx);
        if self.nav.current() != &entry {
            self.nav.push(entry);
        }
        cx.notify();
    }

    /// The sidebar's Plugins entry: the plugin catalog in the main area.
    pub(crate) fn open_plugins_page(&mut self, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.set_voice_stage_open(false, cx);
        self.set_route(Route::Plugins, cx);
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        if self.nav.current() != &NavEntry::Plugins {
            self.nav.push(NavEntry::Plugins);
        }
        cx.notify();
    }

    /// Show the board's route without recording a navigation.
    fn enter_pull_requests_route(&mut self, cx: &mut Context<Self>) {
        if matches!(self.route, Route::PullRequests) {
            return;
        }
        self.set_route(Route::PullRequests, cx);
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        // Through `close_add_space`, so the host's home index is released.
        self.close_add_space(cx);
        self.close_right_plus(cx);
        if self.spaces_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.spaces_menu);
        }
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.space_menu);
        }
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_focus_pending = false;
        self.settings_restore_pending = true;
        let route = self.settings_return_route;
        self.set_route(route, cx);
        cx.notify();
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        if matches!(self.route, Route::Settings(_)) {
            self.close_settings(cx);
        } else {
            self.open_last_settings(cx);
        }
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        self.suspend_file_images(cx);
        match entry {
            NavEntry::Chat(chat_id) => {
                self.set_route(Route::Chat, cx);
                self.focus_composer(cx);
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::PullRequests => {
                self.enter_pull_requests_route(cx);
                self.dismiss_pull_request_detail(cx);
            }
            NavEntry::PullRequest { url, device } => {
                self.enter_pull_requests_route(cx);
                // The detail view needs the window; the next render opens it.
                self.pending_pull_request = Some((url, device));
            }
            NavEntry::Plugins => self.set_route(Route::Plugins, cx),
            NavEntry::NewZot => self.show_new_zot(cx),
            NavEntry::Usage {
                project,
                project_scope,
            } => self.enter_usage(project, project_scope, cx),
            NavEntry::Archives(project) => {
                if self.archive_project.as_ref() != Some(&project) {
                    if let Some(page) = self
                        .archive_project
                        .as_ref()
                        .and_then(|id| self.project_archives.get(id))
                        .cloned()
                    {
                        page.update(cx, |page, cx| page.on_hidden(cx));
                    }
                }
                self.archive_project = Some(project.clone());
                self.set_route(Route::Archives, cx);
                if let Some(page) = self.project_archives.get(&project).cloned() {
                    page.update(cx, |page, cx| page.on_visible(cx));
                }
            }
            NavEntry::Settings(section) => {
                if !matches!(self.route, Route::Settings(_)) {
                    self.settings_return_route = self.route;
                }
                self.set_route(Route::Settings(section.canonical()), cx);
                self.remember_settings_section(section, cx);
            }
        }
        self.close_user_menu(cx);
        self.close_chat_menu(cx);
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match section {
            SettingsSection::Defaults => {
                if self.defaults_page.is_none() {
                    let state = self.state.clone();
                    self.defaults_page =
                        Some(cx.new(|cx| settings::defaults::DefaultsPage::new(state, cx)));
                }
                self.defaults_page
                    .as_ref()
                    .unwrap()
                    .clone()
                    .into_any_element()
            }

            SettingsSection::Plugins => {
                let card = settings::plugins::PluginsCard::shared(self.state.clone(), cx);
                let theme = Theme::of(cx).for_settings_surface();
                div()
                    .id("plugins-settings")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        settings::widgets::settings_page_column(window)
                            .child(settings::widgets::section_title(
                                &theme,
                                "Plugins & MCP",
                                "Turn plugins and MCP servers on or off for your agents, or import the ones already on this computer.",
                            ))
                            .child(settings::widgets::section_stack().child(card)),
                    )
                    .into_any_element()
            }

            SettingsSection::Cloud
            | SettingsSection::CloudRepositories
            | SettingsSection::CloudEnvironments
            | SettingsSection::CloudMachine
            | SettingsSection::CloudAgents
            | SettingsSection::Billing => {
                let view = section.cloud_view().unwrap_or_default();
                let page = self.ensure_cloud_page(window, cx);
                page.update(cx, |page, cx| page.set_view(view, cx));
                page.into_any_element()
            }
            SettingsSection::Usage => self.usage_settings(window, cx),
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    self.devices_page = Some(self.create_devices_page(cx));
                }
                let storage = self.workspace_storage_properties(cx);
                if let Some(page) = &self.devices_page {
                    page.update(cx, |page, cx| page.set_workspace_storage(storage, cx));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Harnesses => {
                if self.harnesses_page.is_none() {
                    let state = self.state.clone();
                    self.harnesses_page = Some(cx.new(|cx| HarnessesPage::new(state, cx)));
                }
                match &self.harnesses_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                self.ensure_appearance_page(cx);
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Files => {
                if self.files_settings_page.is_none() {
                    let page = cx.new(|cx| {
                        FilesSettingsPage::new(
                            self.settings.files_autosave_enabled,
                            self.settings.files_autosave_delay_ms,
                            self.settings.files_word_wrap,
                            self.settings.files_show_all,
                            cx,
                        )
                    });
                    self.files_settings_sub = Some(cx.subscribe_in(
                        &page,
                        window,
                        |this: &mut Shell, _, event: &FilesSettingsEvent, window, cx| match *event {
                            FilesSettingsEvent::AutosaveChanged(autosave_enabled) => {
                                this.settings.files_autosave_enabled = autosave_enabled;
                                for surface in
                                    this.files.values().chain(this.file_surfaces.values())
                                {
                                    surface.update(cx, |surface, cx| {
                                        surface.set_autosave_enabled(autosave_enabled, cx)
                                    });
                                }
                                this.schedule_save(cx);
                                cx.notify();
                            }
                            FilesSettingsEvent::AutosaveDelayChanged(autosave_delay_ms) => {
                                this.settings.files_autosave_delay_ms = autosave_delay_ms;
                                for surface in
                                    this.files.values().chain(this.file_surfaces.values())
                                {
                                    surface.update(cx, |surface, cx| {
                                        surface.set_autosave_delay_ms(autosave_delay_ms, cx)
                                    });
                                }
                                this.schedule_save(cx);
                                cx.notify();
                            }
                            FilesSettingsEvent::WordWrapChanged(word_wrap) => {
                                this.set_files_word_wrap(word_wrap, window, cx);
                            }
                            FilesSettingsEvent::ShowAllFilesChanged(show_all_files) => {
                                this.set_files_show_all(show_all_files, cx);
                            }
                        },
                    ));
                    self.files_settings_page = Some(page);
                }
                match &self.files_settings_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Notifications => {
                if self.notifications_page.is_none() {
                    let page = cx.new(|cx| {
                        NotificationsPage::new(
                            self.settings.sound_enabled,
                            self.settings.sound_completion_enabled,
                            self.settings.sound_input_enabled,
                            self.settings.sound_attention_enabled,
                            self.settings.notifications_enabled,
                            self.settings.notifications_background_only,
                            self.settings.agent_update_notifications,
                            cx,
                        )
                    });
                    // Persist the flags whenever the page flips one.
                    self.notifications_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &NotificationsEvent, cx| {
                            let NotificationsEvent::Changed {
                                sound,
                                completion_sound,
                                input_sound,
                                attention_sound,
                                desktop,
                                background_only,
                                agent_updates,
                            } = *event;
                            this.settings.sound_enabled = sound;
                            this.settings.sound_completion_enabled = completion_sound;
                            this.settings.sound_input_enabled = input_sound;
                            this.settings.sound_attention_enabled = attention_sound;
                            this.settings.notifications_enabled = desktop;
                            this.settings.notifications_background_only = background_only;
                            this.settings.agent_update_notifications = agent_updates;
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.notifications_page = Some(page);
                }
                match &self.notifications_page {
                    Some(page) => {
                        page.update(cx, |page, cx| page.sync_settings(&self.settings, cx));
                        page.clone().into_any_element()
                    }
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Voice => {
                let card = crate::dictation::card(cx);
                card.into_any_element()
            }
            SettingsSection::Shortcuts | SettingsSection::General | SettingsSection::Appshots => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let escape_stops_active_agent = self.settings.escape_stops_active_agent;
                    let composer_send_behavior = self.settings.composer_send_behavior;
                    let appshots_enabled = self.settings.appshots_enabled;
                    let appshot_sound_enabled = self.settings.appshot_sound_enabled;
                    let appshot_destination = self.settings.appshot_destination;
                    let page = cx.new(|cx| {
                        ShortcutsPage::new(
                            state,
                            keymap,
                            escape_stops_active_agent,
                            composer_send_behavior,
                            appshots_enabled,
                            appshot_sound_enabled,
                            appshot_destination,
                            cx,
                        )
                    });
                    // Persist + re-apply shortcut preferences whenever the page changes them.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            match event {
                                ShortcutsEvent::KeymapChanged(keymap) => {
                                    this.settings.keymap = keymap.clone();
                                }
                                ShortcutsEvent::EscapeStopsActiveAgentChanged(enabled) => {
                                    this.settings.escape_stops_active_agent = *enabled;
                                }
                                ShortcutsEvent::ComposerSendBehaviorChanged(behavior) => {
                                    this.settings.composer_send_behavior = *behavior;
                                }
                                ShortcutsEvent::QuitGestureChanged => this.pull_settings(cx),
                                ShortcutsEvent::AppshotsChanged {
                                    enabled,
                                    sound_enabled,
                                    destination,
                                } => {
                                    this.settings.appshots_enabled = *enabled;
                                    this.settings.appshot_sound_enabled = *sound_enabled;
                                    crate::appshots::set_capture_sound_enabled(*sound_enabled);
                                    this.settings.appshot_destination = *destination;
                                    crate::appshots::set_enabled(*enabled);
                                }
                            }
                            apply_keymap(
                                cx,
                                &this.settings.keymap,
                                this.settings.composer_send_behavior,
                            );
                            // The menu's ⌘Q label follows the binding.
                            let menus = crate::app_menus::app_menus(cx);
                            cx.set_menus(menus);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => {
                        page.update(cx, |page, _| {
                            page.show_section(
                                section == SettingsSection::Appshots,
                                section == SettingsSection::General,
                            )
                        });
                        page.clone().into_any_element()
                    }
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.show_notice(crate::toast::ToastKind::Error, format!("{err}"), cx);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Start editing `chat_id`'s title in place on its row: the sidebar's
    /// session row, or — for a side chat — its conversation tab.
    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        if self.chat_rename.is_some() {
            self.finish_rename_chat(true, cx);
        }
        let Some((current, parent, archived)) = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| {
                (
                    c.title.clone().unwrap_or_default(),
                    c.parent_chat_id.clone(),
                    c.archived,
                )
            })
        else {
            return;
        };
        // The sidebar lists only top-level chats; side chats list as tabs
        // beside their parent's conversation tabs. Either way the row must be
        // on screen first: a field on a hidden row would take keystrokes unseen.
        let surface = if parent.is_some() {
            ChatRenameSurface::Tab
        } else {
            ChatRenameSurface::Sidebar
        };
        let revealed = match surface {
            ChatRenameSurface::Sidebar => self.reveal_sidebar_chat(&chat_id, cx),
            // The titlebar lists the open chat's live side chats as tabs.
            ChatRenameSurface::Tab => {
                !archived
                    && matches!(self.route, Route::Chat)
                    && parent.as_deref() == Some(self.active_chat.as_str())
            }
        };
        if !revealed {
            // The sidebar explains its own refusals.
            if surface == ChatRenameSurface::Tab {
                self.show_notice(
                    crate::toast::ToastKind::Error,
                    "Open this side chat's parent to rename it",
                    cx,
                );
            }
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            ComposerInput::new("Session title", cx)
                .with_single_line()
                .with_text_metrics(crate::typography::TEXT_SM, 17.0)
                .with_accessibility_role(gpui::Role::TextInput)
        });
        input.update(cx, |input, cx| {
            input.set_text(current, cx);
            input.select_all_text(cx);
        });
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.finish_rename_chat(true, cx);
            }
        });
        // A pending composer refocus (the first click of the double-click
        // opened the chat, or a side chat's tab) must not steal the field
        // before it is typed in.
        for composer in
            std::iter::once(&self.composer).chain(self.side_chats.values().map(|tab| &tab.composer))
        {
            composer.update(cx, |composer, _| composer.focus_pending = false);
        }
        self.chat_rename = Some(ChatRename {
            chat_id,
            surface,
            input,
            focus_pending: true,
            reveal_until: Some(
                std::time::Instant::now() + motion::COLLAPSE.total() + CHAT_RENAME_REVEAL_GRACE,
            ),
            _events: events,
            _blur: None,
        });
        cx.notify();
    }

    /// Focus a just-opened inline rename and arm its commit-on-blur.
    fn focus_rename_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.chat_rename.as_mut() else {
            return;
        };
        if !std::mem::take(&mut rename.focus_pending) {
            return;
        }
        let focus = rename.input.focus_handle(cx);
        window.focus(&focus, cx);
        rename._blur = Some(cx.on_blur(&focus, window, |this, _, cx| {
            this.finish_rename_chat(true, cx);
        }));
    }

    /// Close the inline rename, saving a changed, non-empty title when
    /// `commit` (Enter or blur) and dropping it otherwise (Escape).
    fn finish_rename_chat(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(rename) = self.chat_rename.take() else {
            return;
        };
        let title = rename.input.read(cx).text().trim().to_string();
        let unchanged = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == rename.chat_id)
            .is_none_or(|c| c.title.as_deref() == Some(title.as_str()));
        if commit && !title.is_empty() && !unchanged {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": rename.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    /// Scroll the sidebar so the renamed row (painted at `row`) clears the
    /// list's edge fades, while the rename is still revealing it.
    fn keep_rename_row_in_view(&mut self, row: gpui::Bounds<Pixels>, cx: &mut Context<Self>) {
        if !self.chat_rename_revealing() {
            return;
        }
        let viewport = self.sidebar_scroll.bounds();
        let band = px(SIDEBAR_GLASS_FADE_BAND);
        let delta = if row.top() < viewport.top() + band {
            row.top() - (viewport.top() + band)
        } else if row.bottom() > viewport.bottom() - band {
            row.bottom() - (viewport.bottom() - band)
        } else {
            return;
        };
        let offset = self.sidebar_scroll.offset();
        let max = self.sidebar_scroll.max_offset().y;
        let y = (offset.y - delta).clamp(-max, px(0.0));
        if y != offset.y {
            self.sidebar_scroll.set_offset(gpui::point(offset.x, y));
            cx.notify();
        }
    }

    fn chat_rename_revealing(&self) -> bool {
        self.chat_rename
            .as_ref()
            .and_then(|rename| rename.reveal_until)
            .is_some_and(|until| std::time::Instant::now() < until)
    }

    /// The inline title editor for `chat_id`'s row on `surface`, if one is open.
    fn rename_input_for(
        &self,
        chat_id: &str,
        surface: ChatRenameSurface,
    ) -> Option<Entity<ComposerInput>> {
        self.chat_rename
            .as_ref()
            .filter(|rename| rename.chat_id == chat_id && rename.surface == surface)
            .map(|rename| rename.input.clone())
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.set_chat_archived(chat_id, true, cx);
    }

    /// Move the row hover (wash + archive pill) onto or off `row_id` to match a
    /// pointer that has not moved since the last archive-pill click. Releasing
    /// matters as much as adopting: an archived row keeps its hover key when it
    /// lands in the Archived section, and would otherwise stay lit there.
    fn sync_chat_row_hover(&mut self, row_id: String, hovered: bool, cx: &mut Context<Self>) {
        let current = self.chat_status_hover.as_deref() == Some(row_id.as_str());
        if !self.chat_hover_resync || current == hovered {
            return;
        }
        if hovered {
            if let Some(previous) = self.chat_status_hover.replace(row_id.clone()) {
                motion::set_hover(&format!("{previous}-hover"), false, self.reduced_motion);
            }
        } else {
            self.chat_status_hover = None;
        }
        motion::set_hover(&format!("{row_id}-hover"), hovered, self.reduced_motion);
        cx.notify();
    }

    fn reconcile_sidebar_pins(&mut self, cx: &mut Context<Self>) {
        self.discard_stale_sidebar_pin_writes(cx);
        self.migrate_sidebar_sections(cx);
        let Some(profile_key) = self.active_sidebar_pin_profile_key(cx) else {
            return;
        };
        let state = self.state.read(cx);
        if state.workspace_scope == Some(WorkspaceScope::Local) {
            if !state.chats_synced {
                return;
            }
            let known = state.chats.iter().map(|chat| chat.id.clone()).collect();
            let changed = self
                .settings
                .sidebar_pinned_session_ids_by_profile
                .get_mut(&profile_key)
                .is_some_and(|pins| spaces::retain_known_pins(pins, &known));
            if changed {
                self.schedule_save(cx);
            }
            return;
        }
        // Remote pins come exclusively from per-pin registry records.
    }

    fn active_sidebar_pin_profile_key(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        sidebar_pin_profile_key(
            state.workspace_scope,
            state.auth.as_ref(),
            self.boot.org_id.as_deref(),
        )
    }

    fn active_sidebar_pins(&self, cx: &App) -> Vec<String> {
        let mut pins = self.raw_sidebar_pins(cx);
        let sections = self.active_sidebar_sections(cx);
        pins.retain(|id| {
            !sections
                .iter()
                .any(|section| section.session_ids.contains(id))
        });
        pins
    }

    fn raw_sidebar_pins(&self, cx: &App) -> Vec<String> {
        if let Some(pins) = self.optimistic_sidebar_pins(cx) {
            return pins;
        }
        let state = self.state.read(cx);
        match state.workspace_scope {
            Some(WorkspaceScope::Local) => self
                .active_sidebar_pin_profile_key(cx)
                .map(|key| self.settings.sidebar_pins(&key).to_vec())
                .unwrap_or_default(),
            Some(WorkspaceScope::Synced | WorkspaceScope::Development) => {
                state.sidebar_preferences.pinned_session_ids.clone()
            }
            None => Vec::new(),
        }
    }

    fn sidebar_pins_for_profile(&self, profile_key: &str, cx: &App) -> Vec<String> {
        if self.active_sidebar_pin_profile_key(cx).as_deref() != Some(profile_key) {
            return Vec::new();
        }
        self.active_sidebar_pins(cx)
    }

    fn validate_sidebar_pin_change(
        &mut self,
        profile_key: &str,
        pins: &[String],
        cx: &mut Context<Self>,
    ) -> bool {
        if self.active_sidebar_pin_profile_key(cx).as_deref() != Some(profile_key) {
            return false;
        }
        let state = self.state.read(cx);
        let remote = matches!(
            state.workspace_scope,
            Some(WorkspaceScope::Synced | WorkspaceScope::Development)
        );
        let result = if remote && !state.sidebar_preferences.can_edit() {
            Err("Pins are still syncing")
        } else {
            let current = self.active_sidebar_pins(cx);
            zuse_protocol::validate_sidebar_pin_update(&current, pins)
        };
        if let Err(message) = result {
            self.show_notice(crate::toast::ToastKind::Error, message, cx);
            cx.notify();
            return false;
        }
        true
    }

    fn apply_sidebar_pin_change(
        &mut self,
        profile_key: String,
        change: zuse_protocol::SidebarPinChange,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut pinned_session_ids = self.raw_sidebar_pins(cx);
        change.project(&mut pinned_session_ids);
        if !self.validate_sidebar_pin_change(&profile_key, &pinned_session_ids, cx)
            || self.raw_sidebar_pins(cx) == pinned_session_ids
        {
            return false;
        }
        match self.state.read(cx).workspace_scope {
            Some(WorkspaceScope::Local) => {
                if pinned_session_ids.is_empty() {
                    self.settings
                        .sidebar_pinned_session_ids_by_profile
                        .remove(&profile_key);
                } else {
                    self.settings
                        .sidebar_pinned_session_ids_by_profile
                        .insert(profile_key, pinned_session_ids);
                }
                self.schedule_save(cx);
            }
            Some(WorkspaceScope::Synced | WorkspaceScope::Development) => {
                return self.queue_sidebar_pin_write(profile_key, change, cx);
            }
            None => return false,
        }
        true
    }

    fn set_chat_pinned(&mut self, chat_id: String, pinned: bool, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        self.cancel_pinned_session_drag(cx);
        let Some(profile_key) = self.active_sidebar_pin_profile_key(cx) else {
            return;
        };
        if !self
            .state
            .read(cx)
            .chats
            .iter()
            .any(|chat| chat.id == chat_id)
        {
            return;
        }
        let pins = self.active_sidebar_pins(cx);
        if pins.contains(&chat_id) == pinned {
            return;
        }
        if pinned && self.raw_sidebar_pins(cx).contains(&chat_id) {
            self.assign_sidebar_section(&chat_id, None, cx);
            cx.notify();
            return;
        }
        let change = if pinned {
            zuse_protocol::SidebarPinChange::Pin {
                session_id: chat_id.clone(),
                after: pins.last().cloned(),
                before: None,
            }
        } else {
            zuse_protocol::SidebarPinChange::Unpin {
                session_id: chat_id.clone(),
            }
        };
        if self.apply_sidebar_pin_change(profile_key, change, cx)
            && pinned
            && self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local)
        {
            self.assign_sidebar_section(&chat_id, None, cx);
        }
        cx.notify();
    }

    /// The Archive session shortcut. With no chat open, or with an already
    /// archived one, it does nothing — the shortcut archives, it never
    /// unarchives.
    fn archive_selected_chat(&mut self, cx: &mut Context<Self>) {
        let Some(chat_id) = self
            .state
            .read(cx)
            .archivable_selected_chat()
            .map(str::to_string)
        else {
            return;
        };
        self.archive_chat(chat_id, cx);
    }

    pub(super) fn set_chat_archived(
        &mut self,
        chat_id: String,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        // An archived row leaves the sidebar under the pointer, so it never
        // hears the hover end; drop its wash now, or it would come back lit
        // if the session is unarchived.
        if archived {
            motion::set_hover(&format!("chat-{chat_id}-hover"), false, true);
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        let start = self.state.update(cx, |state, cx| {
            let start = state.begin_chat_archive(&chat_id, archived);
            cx.notify();
            start
        });
        if let Some(request_id) = start {
            let state = self.state.clone();
            cx.spawn(async move |this, cx| {
                let mut archived = archived;
                loop {
                    let result = engine.client().call(methods::MUTATE,
                        serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
                    ).await;
                    let next = state.update(cx, |state, cx| {
                        if !state.engine().is_some_and(|current| current.same_connection(&engine)) {
                            return None;
                        }
                        let next = state.finish_chat_archive(&chat_id, request_id, archived, result.is_ok());
                        cx.notify();
                        next
                    });
                    if let Err(error) = result {
                        let _ = this.update(cx, |shell, cx| {
                            if !shell.state.read(cx).engine().is_some_and(|current| current.same_connection(&engine)) {
                                return;
                            }
                            if archived {
                                shell.show_archive_error(chat_id.clone(), format!("{error}"), cx);
                            } else {
                                shell.show_notice(crate::toast::ToastKind::Error, format!("{error}"), cx);
                            }
                            cx.notify();
                        });
                    }
                    match next {
                        Some(next) => archived = next,
                        _ => break,
                    }
                }
            }).detach();
        }
        cx.notify();
    }

    /// A jump shortcut: open the sidebar row at `slot`. A slot past the end of
    /// a short list does nothing. Reads the DISPLAYED order — sort and
    /// grouping view options permute the list, and the chip on a row must
    /// name the key that opens it.
    fn jump_to_session(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(chat_id) = self.sidebar_visible_order(cx).into_iter().nth(slot) else {
            return;
        };
        // Same path a click on that row takes.
        self.open_chat(chat_id, cx);
    }

    /// Whether an overlay that owns the keyboard is up — the add-space
    /// palette or a composer picker popover (model selector, traits, repo,
    /// branch…). Session-nav shortcuts (cycle/jump/archive) go quiet
    /// underneath one: gpui runs a matched binding before any `on_key_down`,
    /// so an unguarded jump would switch sessions UNDER the open popover,
    /// stranding it over a session the user never picked. The Settings page
    /// is not one: navigating leaves it, and its shortcut recorder intercepts
    /// the keys it records before they can dispatch.
    pub(super) fn overlay_owns_keyboard(&self, cx: &App) -> bool {
        self.command_palette.is_some()
            || self.shortcuts_panel_open
            || self.voice.read(cx).stage_open
            || self.section_dialog.is_some()
            || self.section_menu.is_some()
            || self.add_space.is_some()
            || self.composer.read(cx).pickers().read(cx).is_open()
            || self.open_side_chat_pickers(cx).is_some()
    }

    /// The pickers of a side chat whose picker popover is open.
    fn open_side_chat_pickers(&self, cx: &App) -> Option<Entity<crate::pickers::Pickers>> {
        self.side_chats
            .values()
            .map(|side| side.composer.read(cx).pickers().clone())
            .find(|pickers| pickers.read(cx).is_open())
    }

    /// Track held modifiers for sidebar jump hints and the queue's submit hint.
    /// Only a visibility change repaints; modifier traffic is otherwise constant.
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        self.update_jump_hints(&event.modifiers, cx);
    }

    fn update_jump_hints(&mut self, mods: &gpui::Modifiers, cx: &mut Context<Self>) {
        let primary = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };
        // No hints while an overlay owns the keyboard — the jumps they
        // advertise are suppressed there.
        let visible = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && jump_hints_visible(&self.settings.keymap, primary, mods.alt, mods.shift);
        self.set_jump_hints(visible, cx);
        let queue_shortcut_revealed = matches!(self.route, Route::Chat)
            && !self.overlay_owns_keyboard(cx)
            && modifier_send_hint_visible(primary, mods.alt, mods.shift);
        self.composer.update(cx, |composer, cx| {
            composer.set_queue_shortcut_revealed(queue_shortcut_revealed, cx)
        });
    }

    pub(super) fn set_jump_hints(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.jump_hints != visible {
            self.jump_hints = visible;
            cx.notify();
        }
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.deleting_chat {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.show_notice(crate::toast::ToastKind::Error, "Engine not connected", cx);
            cx.notify();
            return;
        };
        // Take a Zapper thread off its Zapper first, so the Zapper stops
        // counting it and sending it work.
        if let Some(zot) = self.zot_of_thread(&chat_id, cx) {
            self.send_zot_command(
                zot.id.clone(),
                Some(zot.revision),
                zuse_protocol::zots::ZotAction::ReleaseWorker {
                    chat_id: chat_id.clone(),
                },
                zot_forms::ZotAfter::Stay,
                cx,
            );
        }
        let owner = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .map(|chat| chat.device_id.clone());
        let cloud_request = engine.cloud_target().map(|target| {
            let command_id = self
                .cloud_delete_commands
                .entry(chat_id.clone())
                .or_insert_with(|| uuid::Uuid::new_v4().to_string())
                .clone();
            (target, command_id)
        });
        self.deleting_chat = true;
        cx.spawn(async move |this, cx| {
            let result = if let Some((target, command_id)) = cloud_request {
                target.delete_session(&chat_id, &command_id).await
            } else {
                engine
                    .client()
                    .call(
                        methods::MUTATE,
                        serde_json::json!({
                            "op": "forceDeleteChat", "chatId": chat_id, "targetDeviceId": owner,
                        }),
                    )
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                this.deleting_chat = false;
                if !this
                    .state
                    .read(cx)
                    .engine()
                    .is_some_and(|current| current.same_connection(&engine))
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(_) => this.finish_delete_chat(chat_id, cx),
                    Err(error) => {
                        this.show_notice(crate::toast::ToastKind::Error, format!("{error}"), cx)
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.cloud_delete_commands.remove(&chat_id);
        if self.delete_confirm.as_deref() == Some(chat_id.as_str()) {
            self.delete_confirm = None;
        }
        if let Some(tabs) = self.right_tabs.get(&chat_id) {
            for surface in tabs {
                if let RightSurface::Browser(id) = surface {
                    if let Some(browser) = self.browsers.remove(id) {
                        browser.update(cx, |browser, cx| browser.close(cx));
                    }
                    self.browser_subs.remove(id);
                }
                if let RightSurface::Simulator(id) = surface {
                    if let Some(pane) = self.simulators.remove(id) {
                        pane.update(cx, |pane, cx| pane.close(cx));
                    }
                }
                if let RightSurface::PullRequest(id) = surface {
                    self.chat_pull_requests.remove(id);
                }
            }
        }
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        cx.notify();
    }

    fn confirm_discard_working_tree(&mut self, cx: &mut Context<Self>) {
        if self.discard_working_tree_task.is_some() {
            return;
        }
        let Some(DiscardWorkingTreeFlow::Confirm(request)) = self.discard_working_tree.clone()
        else {
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.discard_working_tree = Some(DiscardWorkingTreeFlow::Failed(
                "Engine is not connected.".into(),
            ));
            cx.notify();
            return;
        };

        let mut params = serde_json::Map::new();
        params.insert("chatId".into(), serde_json::Value::String(request.chat_id));
        params.insert(
            "checkoutId".into(),
            serde_json::Value::String(request.checkout_id),
        );
        params.insert(
            "expectedChecksum".into(),
            serde_json::Value::String(request.expected_checksum),
        );
        if let Some(target) = request.target_device_id {
            params.insert("targetDeviceId".into(), serde_json::Value::String(target));
        }

        // Dismiss the confirmation immediately. The task remains retained so
        // repeat clicks cannot issue a second destructive request.
        self.discard_working_tree = None;
        self.discard_working_tree_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DISCARD_WORKING_TREE,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |shell, cx| {
                shell.discard_working_tree_task = None;
                shell.discard_working_tree = match result {
                    Ok(_) => None,
                    Err(error) => Some(DiscardWorkingTreeFlow::Failed(format!("{error}").into())),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn request_sign_out(&mut self, cx: &mut Context<Self>) {
        self.close_user_menu(cx);
        if self.state.read(cx).workspace_scope != Some(WorkspaceScope::Synced) {
            return;
        }
        self.sync_flow = SyncFlow::SignOutConfirm;
        cx.notify();
    }

    fn confirm_sign_out(&mut self, cx: &mut Context<Self>) {
        self.start_local_runtime_transition(true, cx);
    }

    fn start_local_runtime_transition(&mut self, sign_out: bool, cx: &mut Context<Self>) {
        if self.runtime_change_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            self.sync_flow = SyncFlow::SignedOutRestartRequired;
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::SigningOut;
        self.runtime_change_error = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let shutdown_dir = data_dir.clone();
        let transition = Tokio::spawn(cx, async move {
            if sign_out {
                engine
                    .client()
                    .call(methods::SIGN_OUT, serde_json::json!({}))
                    .await
                    .map_err(|error| format!("Sign out failed: {error}"))?;
            }
            stop_synced_runtime(engine, ipc_port, &shutdown_dir).await
        });
        let state = self.state.clone();
        let boot = self.boot.clone();
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match transition.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(()) => {
                        shell.sync_flow = SyncFlow::Idle;
                        shell.runtime_change_error = None;
                        shell.org = None;
                        // An explicit sign-out returns to the workspace. The
                        // boot-time fallback from a signed-out synced runtime
                        // keeps an open Settings page (and `ZUSE_OPEN_ROUTE`).
                        if sign_out {
                            shell.set_route(Route::Chat, cx);
                        }
                        shell.space_boot_applied = false;
                        state.update(cx, |state, cx| state.prepare_runtime_replacement(cx));
                        AppState::bootstrap(state.clone(), boot, cx);
                    }
                    Err(error) => {
                        shell.sync_flow = SyncFlow::SignedOutRestartRequired;
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn cancel_auth_setup(&mut self, cx: &mut Context<Self>) {
        let local = self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let pending_auth = self.auth_task.take();
        let pending_org = self.org.as_mut().and_then(|org| org.task.take());
        if local {
            self.sync_flow = SyncFlow::Canceling;
        }
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            // Do not race SignOut against an exchange or organization write
            // that can still persist a session after credentials were cleared.
            if let Some(task) = pending_auth {
                task.await;
            }
            if let Some(task) = pending_org {
                task.await;
            }
            let result = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.org = None;
                        if local {
                            shell.sync_flow = SyncFlow::Idle;
                        }
                    }
                    Err(err) => {
                        if local {
                            shell.sync_flow = SyncFlow::Enabling;
                        }
                        shell.show_notice(
                            crate::toast::ToastKind::Error,
                            format!("Could not cancel sign-in: {err}"),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn postpone_sync_restart(&mut self, cx: &mut Context<Self>) {
        match self.sync_flow {
            SyncFlow::RestartPending { .. } => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: false };
            }
            SyncFlow::SwitchOffer { .. } => {
                self.sync_flow = SyncFlow::SwitchOffer { notice_open: false };
            }
            SyncFlow::ImportFailed { .. } => {
                self.sync_flow = SyncFlow::ImportFailed { notice_open: false };
            }
            _ => return,
        }
        cx.notify();
    }

    fn reopen_sync_notice(&mut self, cx: &mut Context<Self>) {
        self.close_user_menu(cx);
        match self.sync_flow {
            SyncFlow::RestartPending { .. } => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            }
            SyncFlow::SwitchOffer { .. } => {
                self.sync_flow = SyncFlow::SwitchOffer { notice_open: true };
            }
            SyncFlow::ImportFailed { .. } => {
                self.sync_flow = SyncFlow::ImportFailed { notice_open: true };
            }
            _ => return,
        }
        cx.notify();
    }

    /// The wizard's choice step chose a path: stop the local runtime, boot the
    /// synced one in-place (mirror of the sign-out transition), then let
    /// [`Self::drive_sync_switch`] run the import once the runtime is ready.
    /// Failure falls back to the quit-and-reopen dialog — the local profile is
    /// untouched, so the old path is always a safe exit.
    fn start_synced_switch(&mut self, import: bool, cx: &mut Context<Self>) {
        if self.runtime_change_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::Switching { import };
        self.runtime_change_error = None;
        self.import_current = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let transition = Tokio::spawn(cx, async move {
            stop_synced_runtime(engine, ipc_port, &data_dir).await
        });
        let state = self.state.clone();
        let boot = self.boot.clone();
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match transition.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(()) => {
                        // Keep `Switching { import }`: the state observer sees
                        // the replacement runtime reach Ready and advances the
                        // wizard from there.
                        shell.org = None;
                        shell.set_route(Route::Chat, cx);
                        shell.space_boot_applied = false;
                        state.update(cx, |state, cx| state.prepare_runtime_replacement(cx));
                        AppState::bootstrap(state.clone(), boot, cx);
                    }
                    Err(error) => {
                        shell.sync_flow = SyncFlow::RestartPending { notice_open: true };
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Advance the in-place switch when the replacement runtime lands: Ready +
    /// Synced starts the import stream (or finishes immediately when the user
    /// chose a fresh start); a runtime that comes back non-synced fell out of
    /// the swap — surface the quit fallback rather than pretend.
    fn drive_sync_switch(&mut self, cx: &mut Context<Self>) {
        let SyncFlow::Switching { import } = self.sync_flow else {
            return;
        };
        if self.runtime_change_task.is_some() {
            return; // still stopping the local runtime
        }
        let (ready, scope) = {
            let state = self.state.read(cx);
            (
                matches!(state.connection, ConnectionStatus::Ready),
                state.workspace_scope,
            )
        };
        if !ready {
            if let ConnectionStatus::Failed(error) = &self.state.read(cx).connection {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
                self.runtime_change_error = Some(error.clone().into());
                cx.notify();
            }
            return;
        }
        match scope {
            Some(WorkspaceScope::Synced) => {
                if import {
                    self.spawn_local_import(cx);
                } else {
                    self.sync_flow = SyncFlow::Idle;
                    cx.notify();
                }
            }
            Some(_) => {
                self.sync_flow = SyncFlow::RestartPending { notice_open: true };
                self.runtime_change_error =
                    Some("The synced workspace did not come up — restart to finish.".into());
                cx.notify();
            }
            None => {}
        }
    }

    /// Subscribe to the engine's one-time import stream and mirror its
    /// progress into the wizard.
    fn spawn_local_import(&mut self, cx: &mut Context<Self>) {
        if self.import_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sync_flow = SyncFlow::RestartPending { notice_open: true };
            self.runtime_change_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.sync_flow = SyncFlow::Importing { done: 0, total: 0 };
        self.runtime_change_error = None;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let stream = Tokio::spawn(cx, async move {
            let mut items = engine
                .client()
                .subscribe(methods::IMPORT_LOCAL_WORKSPACE, serde_json::json!({}))
                .await
                .map_err(|error| error.to_string())?;
            while let Some(item) = items.recv().await {
                let _ = tx.send(item);
            }
            Ok::<(), String>(())
        });
        self.import_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let item = rx.recv().await;
                let ended = item.is_none();
                this.update(cx, |shell, cx| {
                    if let Some(item) = &item {
                        shell.apply_import_event(item, cx);
                    }
                    if ended {
                        shell.import_task = None;
                        shell.import_current = None;
                        // A stream that died before its summary is a failure —
                        // offer the in-place retry (idempotent).
                        if matches!(shell.sync_flow, SyncFlow::Importing { .. }) {
                            shell.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                            shell.runtime_change_error =
                                Some("The import stream ended before it finished.".into());
                        }
                        cx.notify();
                    }
                })
                .ok();
                if ended {
                    break;
                }
            }
            if let Ok(Err(error)) = stream.await {
                this.update(cx, |shell, cx| {
                    shell.import_task = None;
                    if matches!(shell.sync_flow, SyncFlow::Importing { .. }) {
                        shell.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                        shell.runtime_change_error = Some(error.into());
                        cx.notify();
                    }
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn apply_import_event(&mut self, item: &serde_json::Value, cx: &mut Context<Self>) {
        match item.get("kind").and_then(|k| k.as_str()) {
            Some("start") => {
                let total = item.get("chats").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                self.sync_flow = SyncFlow::Importing { done: 0, total };
            }
            Some("chat") => {
                let index = item.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let total = item.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                self.import_current = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|t| SharedString::from(t.to_string()));
                self.sync_flow = SyncFlow::Importing { done: index, total };
            }
            Some("summary") => {
                self.import_current = None;
                // A summary with errors is a FAILED import, however normally
                // the stream ended — never present a partial migration as
                // complete (the engine keeps collecting per-item failures
                // precisely so this can be surfaced).
                match import_summary_outcome(item) {
                    Ok((imported, skipped)) => {
                        self.sync_flow = SyncFlow::ImportDone { imported, skipped };
                    }
                    Err(message) => {
                        self.sync_flow = SyncFlow::ImportFailed { notice_open: true };
                        self.runtime_change_error = Some(message.into());
                    }
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn quit_for_runtime_change(&mut self, cx: &mut Context<Self>) {
        if !self.prepare_exit(PendingExit::RuntimeChange, cx) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.runtime_change_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        if engine.mode() == EngineMode::InProcess {
            crate::app_menus::quit_after_save(cx);
            return;
        }
        if self.runtime_change_task.is_some() {
            return;
        }

        self.runtime_change_error = None;
        let ipc_port = self.boot.ipc_port;
        let data_dir = self.data_dir.clone();
        let shutdown = Tokio::spawn(cx, async move {
            engine
                .client()
                .call(methods::STOP_ENGINE, serde_json::json!({}))
                .await
                .map_err(|err| err.to_string())?;
            wait_for_remote_engine_shutdown(ipc_port, &data_dir, RUNTIME_CHANGE_TIMEOUT).await
        });
        self.runtime_change_task = Some(cx.spawn(async move |this, cx| {
            let result = match shutdown.await {
                Ok(result) => result,
                Err(err) => Err(err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.runtime_change_task = None;
                match result {
                    Ok(_) => {
                        if shell.prepare_quit(cx) {
                            crate::app_menus::quit_after_save(cx);
                        }
                    },
                    Err(err) => {
                        shell.runtime_change_error = Some(format!(
                            "Could not stop the remote engine: {err}. Run `zuse daemon stop`, then quit and reopen Zuse."
                        ).into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let scope = self.state.read(cx).workspace_scope;
        if scope == Some(WorkspaceScope::Development) {
            return;
        }
        self.close_user_menu(cx);
        if scope == Some(WorkspaceScope::Local) {
            self.sync_flow = SyncFlow::Enabling;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                    cx.notify();
                }
                Err(err) => {
                    if scope == Some(WorkspaceScope::Local) && shell.sync_flow == SyncFlow::Enabling
                    {
                        shell.sync_flow = SyncFlow::Idle;
                    }
                    shell.show_notice(
                        crate::toast::ToastKind::Error,
                        format!("Sign in failed: {err}"),
                        cx,
                    );
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- org gate ----

    fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        let name_input = cx.new(|cx| {
            ComposerInput::new("Workspace name", cx).with_accessibility_role(gpui::Role::TextInput)
        });
        let events = cx.subscribe(&name_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.create_org(cx);
            }
        });
        self.org = Some(OrgGateUi {
            name_input,
            orgs: Loadable::Idle,
            submitting: false,
            error: None,
            task: None,
            _events: events,
        });
        self.load_orgs(cx);
    }

    fn load_orgs(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.orgs = Loadable::Loading;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ORGS, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.orgs = match result {
                        Ok(value) => Loadable::Ready(sort_memberships(parse_orgs(&value))),
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn create_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        let name = org.name_input.read(cx).text().trim().to_string();
        if !org_name_valid(&name) {
            org.error = Some("Enter a workspace name".into());
            cx.notify();
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CREATE_ORG, serde_json::json!({ "name": name }))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                    // Success: the AuthStatus stream flips to SignedIn and the
                    // gate falls away on its own.
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select_org(&mut self, organization_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SELECT_ORG,
                    serde_json::json!({ "organizationId": organization_id }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- render pieces ----

    fn motion_time(&self) -> std::time::Instant {
        #[cfg(test)]
        if let Some(now) = self.test_time {
            return now;
        }
        std::time::Instant::now()
    }

    fn panel_tween(&self, from: f32, to: f32) -> WidthTween {
        WidthTween {
            started: self.render_time.unwrap_or_else(|| self.motion_time()),
            ..WidthTween::panel(from, to)
        }
    }

    fn tween_elapsed(&self, started: std::time::Instant) -> Duration {
        self.render_time
            .unwrap_or_else(|| self.motion_time())
            .saturating_duration_since(started)
    }

    /// Evaluate a width tween at the frame time (see [`WidthTween`]).
    /// Mid-flight: lerp on the tween's curve, flagging `motion_active` so
    /// render schedules the next frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `ZUSE_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween {
            from,
            to,
            started,
            duration,
            curve,
        }) = tween
        else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let raw = self.tween_elapsed(started).as_secs_f32() / duration.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, curve.eval(raw))
    }

    /// [`Self::eval_tween`] heading for `target` as it is this frame, not
    /// the endpoint the tween saw when it started. A column's target follows
    /// its neighbours while they move; easing toward it lands there instead
    /// of snapping to it as the tween ends.
    fn ease_toward(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        self.eval_tween(
            tween.map(|tween| WidthTween {
                to: target,
                ..tween
            }),
            target,
        )
    }

    fn eval_resize_edge_bounce(
        &self,
        bounce: Option<motion::ResizeEdgeBounce>,
        enabled: bool,
    ) -> f32 {
        let Some(bounce) = bounce else {
            return 0.0;
        };
        if self.reduced_motion || !enabled {
            return 0.0;
        }
        let total =
            Duration::from_millis(motion::RESIZE_EDGE_BOUNCE_MS).mul_f32(motion::speed_scale());
        let raw = self.tween_elapsed(bounce.started).as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return 0.0;
        }
        self.motion_active.set(true);
        motion::resize_bounce_offset(bounce.edge, raw)
    }

    pub(super) fn sidebar_now(&self) -> f32 {
        let fit = self.horizontal_fit();
        if !fit.sidebar {
            return self.fit_exit_width(0).unwrap_or(0.0);
        }
        let limit = self.sidebar_tween_limit(fit.sidebar_limit);
        // The edge bounce applies past the limit so the drag cue at the
        // maximum width stays visible.
        (self
            .ease_toward(self.sidebar_tween, self.sidebar_target())
            .min(limit)
            + self.eval_resize_edge_bounce(
                self.sidebar_edge_bounce,
                !self.settings.sidebar_collapsed,
            ))
        .max(0.0)
    }

    /// The surface host's eased width within `max`, plus its edge bounce,
    /// which may cross `max` like the sidebar's.
    fn right_now(&self, max: f32, cx: &App) -> f32 {
        self.ease_toward(self.right_tween, self.right_target(cx))
            .min(max)
            + self.eval_resize_edge_bounce(
                self.right_edge_bounce,
                self.right_pane_open(cx) && !self.right_pane_expanded,
            )
    }

    fn tween_active(&self, tween: Option<WidthTween>) -> bool {
        tween.is_some_and(|tween| {
            !self.reduced_motion && self.tween_elapsed(tween.started) < tween.duration
        })
    }

    fn active_tween_endpoints(&self, tween: Option<WidthTween>) -> Option<(f32, f32)> {
        tween
            .filter(|transition| {
                !self.reduced_motion && self.tween_elapsed(transition.started) < transition.duration
            })
            .map(|transition| (transition.from, transition.to))
    }

    /// Right-anchored variant for the changes pane. The outer width follows the
    /// existing shell tween, while descendants retain the larger endpoint's
    /// geometry throughout the transition. This mirrors the sidebar's stable
    /// inner/clipped outer behavior without changing the center column's
    /// upstream flex layout.
    fn right_pane_container(
        &self,
        target: f32,
        visible: f32,
        edge_offset: f32,
        inner: AnyElement,
    ) -> AnyElement {
        let content_width = self.right_content_width(target) + edge_offset;
        div()
            .id("right-panel")
            .debug_selector(|| "right-panel".into())
            .h_full()
            .flex_none()
            .relative()
            .overflow_hidden()
            .w(px(visible))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(content_width))
                    .child(inner),
            )
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of zuse __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (zuse remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen, self.linux_left_caption_count()),
        );
        cluster + CLUSTER_BUTTONS_WIDTH + TITLEBAR_IDENTITY_GAP
    }

    /// The session titlebar, or on the Pull requests route the bar carrying
    /// the pull request pane's tabs.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The Plugins page has no tabs: a bare strip that still drags.
        if matches!(self.route, Route::Plugins) {
            return self
                .titlebar_drag_region("plugins-titlebar", div().size_full(), cx)
                .into_any_element();
        }
        if matches!(self.route, Route::Archives | Route::Usage | Route::Zot) {
            return self
                .titlebar_drag_region("plugins-titlebar", div().size_full(), cx)
                .into_any_element();
        }
        if !matches!(self.route, Route::PullRequests) {
            return self.render_session_title_bar(cx);
        }
        self.render_pull_request_title_bar(cx)
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (zuse's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    fn render_titlebar_navigation(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .debug_selector(|| "titlebar-navigation".into())
            .flex_none()
            .flex()
            .items_center()
            .gap(px(TITLEBAR_CONTROL_GAP))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                "Back",
                self.nav.can_back(),
                theme,
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                "Forward",
                self.nav.can_forward(),
                theme,
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .into_any_element()
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// zuse window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        // The titlebar is the single owner of the new-session action in both
        // sidebar states. Hide it on the new-session canvas: opening another
        // blank canvas from an already blank canvas has no effect and used to
        // leave two competing + placements across the responsive variants.
        let plus_alpha = self.titlebar_plus_alpha(cx);
        let show_plus = plus_alpha > 0.01;
        let over_artwork = settings::current(cx)
            .new_thread_composer_background
            .as_ref()
            .is_some_and(|background| std::path::Path::new(&background.path).is_file());
        let island_target = if matches!(self.route, Route::Chat)
            && (self.state.read(cx).selected_chat.is_none() || self.voice.read(cx).stage_open)
            && self.settings.sidebar_collapsed
            && over_artwork
        {
            1.0
        } else {
            0.0
        };
        // Persistent manual tween: reversals start from the painted value,
        // initial presentation is settled, and reduced motion snaps.
        match self.titlebar_island {
            None => self.titlebar_island = Some(WidthTween::new(island_target, island_target)),
            Some(previous) if previous.to != island_target => {
                let from = self.eval_tween(Some(previous), previous.to);
                self.titlebar_island = Some(WidthTween::new(from, island_target));
            }
            _ => {}
        }
        let island = self.eval_tween(self.titlebar_island, island_target);
        let (island_top, island_height) = titlebar_island_vertical_geometry(island);
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .px(px(TITLEBAR_CLUSTER_PAD))
            .child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .right_0()
                    .top(px(island_top))
                    .h(px(island_height))
                    .opacity(island)
                    .children((island > 0.001).then(|| {
                        crate::frost::frosted(
                            12.0,
                            20.0,
                            div()
                                .size_full()
                                .rounded(px(12.0))
                                .bg(theme.glass_overlay())
                                .shadow_sm(),
                        )
                    })),
            )
            .children(self.titlebar_spacer(TITLEBAR_CLUSTER_PAD))
            // Left-side Linux captions (GNOME `close:…` layouts): the
            // root-level caption overlay owns the buttons; the cluster row
            // just starts past them, at the shared 2px rhythm.
            .children((self.linux_left_caption_count() > 0).then(|| {
                div()
                    .flex_none()
                    .h_full()
                    .w(px(caption_buttons_width(self.linux_left_caption_count())))
            }))
            .child(window_control_button_with(
                "toggle-sidebar",
                icons::sidebar_glyph(
                    motion::state_t(
                        "toggle-sidebar",
                        !self.settings.sidebar_collapsed,
                        motion::GLYPH_STATE,
                        self.reduced_motion,
                    ),
                    false,
                    16.0,
                    theme.text_muted,
                ),
                ShortcutId::ToggleSidebar.label(),
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child({
                div()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .child(self.render_titlebar_navigation(&theme, cx))
            })
            .children(show_plus.then(|| {
                div()
                    .flex_none()
                    .ml(px(TITLEBAR_GROUP_GAP))
                    .opacity(plus_alpha)
                    .child(window_control_button(
                        "titlebar-new-session",
                        icons::PLUS,
                        "New tab",
                        &theme,
                        cx.listener(|this, _, _, cx| this.open_new_conversation_tab(cx)),
                    ))
            }))
            .into_any_element()
    }

    /// The app label rides the titlebar at the sidebar's trailing edge,
    /// sliding and fading with it as it collapses; it never crosses back
    /// over the control cluster on a narrow sidebar.
    fn render_sidebar_options_titlebar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sidebar = self.sidebar_now();
        let reveal = (sidebar / self.sidebar_content_width().max(1.0)).clamp(0.0, 1.0);
        if reveal <= 0.01 {
            return None;
        }
        let cluster_end = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(
                cfg!(target_os = "macos"),
                self.fullscreen.unwrap_or(false),
                self.linux_left_caption_count(),
            ),
        ) + CLUSTER_BUTTONS_WIDTH
            + TITLEBAR_ACTION_SLOT_WIDTH * self.titlebar_plus_alpha(cx);
        let left = cluster_end + TITLEBAR_GROUP_GAP;
        let width = sidebar - Theme::SPACE_MD - left;
        if width < 24.0 {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .top_0()
                .left(px(left))
                .w(px(width))
                .h(px(Theme::TITLEBAR_HEIGHT))
                .pt(px(Theme::TITLEBAR_TOP_PAD))
                .flex()
                .items_center()
                .justify_end()
                .opacity(reveal)
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_size(crate::typography::text_xs())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from("Zuse")),
                )
                .into_any_element(),
        )
    }

    /// The titlebar owns new-session creation regardless of sidebar state. It
    /// remains available on PR screens, even without a selected session.
    /// While the sidebar is open its "New chat" row owns the action, so the
    /// titlebar `+` fades in only as the sidebar collapses.
    pub(super) fn titlebar_plus_alpha(&self, cx: &App) -> f32 {
        let sidebar_reveal =
            (self.sidebar_now() / self.sidebar_content_width().max(1.0)).clamp(0.0, 1.0);
        titlebar_new_session_alpha(&self.route, self.has_conversation(cx)) * (1.0 - sidebar_reveal)
    }

    /// Native Windows caption controls integrated into Zuse's unified
    /// titlebar. `WindowControlArea` maps these hit targets to HTMINBUTTON,
    /// HTMAXBUTTON, and HTCLOSE, so Windows owns their behavior (including
    /// Snap Layouts) while GPUI renders the system Segoe caption glyphs.
    fn render_windows_caption_controls(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        if !cfg!(target_os = "windows") {
            return None;
        }

        let theme = Theme::of(cx);
        let (maximize_id, maximize_glyph) = if window.is_maximized() {
            ("window-restore", "\u{e923}")
        } else {
            ("window-maximize", "\u{e922}")
        };
        Some(
            div()
                .id("windows-window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .font_family("Segoe Fluent Icons")
                .child(windows_caption_button(
                    "window-minimize",
                    "\u{e921}",
                    WindowControlArea::Min,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    maximize_id,
                    maximize_glyph,
                    WindowControlArea::Max,
                    theme,
                    false,
                ))
                .child(windows_caption_button(
                    "window-close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    theme,
                    true,
                ))
                .into_any_element(),
        )
    }

    /// Which caption buttons zuse itself must draw on Linux: under
    /// client-side decorations (the Wayland default) nobody else will —
    /// without these the window has NO minimize/maximize/close at all.
    /// Server-side decorations (X11 WMs, KDE with SSD) already draw real
    /// buttons, so `None` there. The desktop's layout (GNOME's
    /// `button-layout` gsetting via `cx.button_layout()`) decides side and
    /// order — min/max/close on the right by default; controls the
    /// compositor can't do (e.g. minimize on some Wayland compositors) drop
    /// out, close always stays.
    #[cfg(target_os = "linux")]
    fn resolve_linux_captions(window: &Window, cx: &App) -> Option<gpui::WindowButtonLayout> {
        use gpui::{MAX_BUTTONS_PER_SIDE, WindowButton, WindowButtonLayout};
        if !matches!(
            window.window_decorations(),
            gpui::Decorations::Client { .. }
        ) {
            return None;
        }
        let layout = cx
            .button_layout()
            .unwrap_or_else(WindowButtonLayout::linux_default);
        let supported = window.window_controls();
        let filter_side = |side: [Option<WindowButton>; MAX_BUTTONS_PER_SIDE]| {
            let mut out = [None; MAX_BUTTONS_PER_SIDE];
            let mut i = 0;
            for button in side.into_iter().flatten() {
                let keep = match button {
                    WindowButton::Minimize => supported.minimize,
                    WindowButton::Maximize => supported.maximize,
                    WindowButton::Close => true,
                };
                if keep {
                    out[i] = Some(button);
                    i += 1;
                }
            }
            out
        };
        let layout = WindowButtonLayout {
            left: filter_side(layout.left),
            right: filter_side(layout.right),
        };
        (layout.left[0].is_some() || layout.right[0].is_some()).then_some(layout)
    }

    #[cfg(not(target_os = "linux"))]
    fn resolve_linux_captions(_window: &Window, _cx: &App) -> Option<gpui::WindowButtonLayout> {
        None
    }

    pub(super) fn linux_left_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.left.iter().flatten().count())
    }

    pub(super) fn linux_right_caption_count(&self) -> usize {
        self.linux_captions
            .map_or(0, |l| l.right.iter().flatten().count())
    }

    /// Right padding titlebar content needs to clear the platform's caption
    /// controls (native Windows cluster / zuse-drawn Linux buttons).
    pub(super) fn titlebar_right_pad(&self, base: f32) -> f32 {
        titlebar_right_padding(
            cfg!(target_os = "windows"),
            self.linux_right_caption_count(),
            base,
        )
    }

    /// Zuse-drawn Linux caption controls, one overlay per populated side.
    /// Shell-level chrome like the Windows cluster: mounted at the root so
    /// they stay above the splash and every auth/org/error gate.
    fn render_linux_caption_controls(&self, window: &Window, cx: &App) -> Vec<AnyElement> {
        let Some(layout) = self.linux_captions else {
            return Vec::new();
        };
        let theme = Theme::of(cx);
        let is_maximized = window.is_maximized();
        // Ids can be per-button (not per-side): the layout parser dedups, so
        // a button never appears on both sides at once.
        let strip = |buttons: &[Option<gpui::WindowButton>]| {
            div()
                .absolute()
                .top_0()
                .h(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .pt(px(Theme::TITLEBAR_TOP_PAD))
                .gap(px(2.0))
                .px(px(10.0))
                .children(buttons.iter().flatten().map(|button| {
                    match button {
                        gpui::WindowButton::Minimize => linux_caption_button(
                            "window-minimize",
                            icons::WINDOW_MINIMIZE,
                            false,
                            theme,
                            |_, window, _| window.minimize_window(),
                        )
                        .into_any_element(),
                        gpui::WindowButton::Maximize => {
                            let (id, icon_path) = if is_maximized {
                                ("window-restore", icons::WINDOW_RESTORE)
                            } else {
                                ("window-maximize", icons::WINDOW_MAXIMIZE)
                            };
                            linux_caption_button(id, icon_path, false, theme, |_, window, _| {
                                window.zoom_window()
                            })
                            .into_any_element()
                        }
                        gpui::WindowButton::Close => linux_caption_button(
                            "window-close",
                            icons::CLOSE,
                            true,
                            theme,
                            |_, window, _| window.remove_window(),
                        )
                        .into_any_element(),
                    }
                }))
        };
        let mut out = Vec::new();
        if layout.left[0].is_some() {
            out.push(strip(&layout.left).left_0().into_any_element());
        }
        if layout.right[0].is_some() {
            out.push(strip(&layout.right).right_0().into_any_element());
        }
        out
    }

    /// Linux CSD chrome state: which edges the compositor has taken (tiled or
    /// maximized). A free edge can carry a resize strip; a fully floating
    /// window (nothing tiled) also gets rounded corners. Server decorations
    /// (e.g. KDE SSD) hand the frame back to the compositor — no CSD chrome.
    #[cfg(target_os = "linux")]
    fn linux_csd_edges(window: &Window) -> (bool, bool, bool, bool) {
        match window.window_decorations() {
            gpui::Decorations::Client { tiling } => {
                (!tiling.top, !tiling.bottom, !tiling.left, !tiling.right)
            }
            gpui::Decorations::Server => (false, false, false, false),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn linux_csd_edges(_window: &Window) -> (bool, bool, bool, bool) {
        (false, false, false, false)
    }

    /// True when the Linux window floats (CSD, nothing tiled or maximized) —
    /// the state that gets macOS-style rounded corners.
    fn linux_window_floating(window: &Window) -> bool {
        let (top, bottom, left, right) = Self::linux_csd_edges(window);
        top && bottom && left && right && cfg!(target_os = "linux")
    }

    /// The corner radius chrome layers must PAINT for this frame. gpui's
    /// content masks are rectangles — `.rounded()` never clips children, so
    /// every full-bleed layer that can occupy a window corner rounds its own
    /// background with this radius (the layer beneath shows through the
    /// curve, down to the transparent window corners). Zero everywhere the
    /// window must be square: macOS rounds via the window server, and a
    /// tiled/maximized Linux window sits flush with the screen.
    pub(crate) fn window_corner_radius(window: &Window) -> f32 {
        if Self::linux_window_floating(window) {
            LINUX_WINDOW_CORNER_RADIUS
        } else {
            0.0
        }
    }

    /// Invisible edge strips that turn pointer presses into compositor
    /// resizes — the CSD contract on X11 and Wayland, where the platform
    /// draws no frame for us. Each strip mounts only along a free edge (a
    /// maximized or snapped window exposes just its untiled edges, like
    /// GTK). The strips paint last so they sit above all content chrome.
    fn render_linux_resize_borders(window: &Window) -> Vec<AnyElement> {
        let (top, bottom, left, right) = Self::linux_csd_edges(window);
        const EDGE: f32 = 6.0;
        const CORNER: f32 = 14.0;
        let mut out = Vec::new();
        macro_rules! strip {
            ($position:expr, $cursor:ident, $edge:expr) => {
                out.push(
                    $position
                        .$cursor()
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            window.start_window_resize($edge);
                        })
                        .into_any_element(),
                );
            };
        }
        // Cardinal edges: full-length strips inset by the corner squares.
        if top {
            strip!(
                div()
                    .absolute()
                    .top_0()
                    .left(px(CORNER))
                    .right(px(CORNER))
                    .h(px(EDGE)),
                cursor_ns_resize,
                gpui::ResizeEdge::Top
            );
        }
        if bottom {
            strip!(
                div()
                    .absolute()
                    .bottom_0()
                    .left(px(CORNER))
                    .right(px(CORNER))
                    .h(px(EDGE)),
                cursor_ns_resize,
                gpui::ResizeEdge::Bottom
            );
        }
        if left {
            strip!(
                div()
                    .absolute()
                    .left_0()
                    .top(px(CORNER))
                    .bottom(px(CORNER))
                    .w(px(EDGE)),
                cursor_ew_resize,
                gpui::ResizeEdge::Left
            );
        }
        if right {
            strip!(
                div()
                    .absolute()
                    .right_0()
                    .top(px(CORNER))
                    .bottom(px(CORNER))
                    .w(px(EDGE)),
                cursor_ew_resize,
                gpui::ResizeEdge::Right
            );
        }
        // Corners: squares over both adjacent edges, diagonal cursors.
        if top && left {
            strip!(
                div().absolute().top_0().left_0().size(px(CORNER)),
                cursor_nwse_resize,
                gpui::ResizeEdge::TopLeft
            );
        }
        if top && right {
            strip!(
                div().absolute().top_0().right_0().size(px(CORNER)),
                cursor_nesw_resize,
                gpui::ResizeEdge::TopRight
            );
        }
        if bottom && left {
            strip!(
                div().absolute().bottom_0().left_0().size(px(CORNER)),
                cursor_nesw_resize,
                gpui::ResizeEdge::BottomLeft
            );
        }
        if bottom && right {
            strip!(
                div().absolute().bottom_0().right_0().size(px(CORNER)),
                cursor_nwse_resize,
                gpui::ResizeEdge::BottomRight
            );
        }
        out
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The sidebar is part of the resolved theme. A second fixed-Zuse
        // palette here made imported families look split in half and froze
        // activity/glyph personality independently of the selected variant.
        let inner = self.sidebar_pane.clone().cached(
            gpui::StyleRefinement::default()
                .w(px(
                    (self.sidebar_content_width() - self.workspace_gutter()).max(0.)
                ))
                .h_full()
                .flex_none(),
        );
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation. The content row spans the
        // full window height (the titlebar overlays it), so the column pads
        // itself below the chrome.
        div()
            .id("sidebar-column")
            .debug_selector(|| "sidebar-column".into())
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.sidebar_now()))
            .flex()
            .flex_col()
            .pl(px(if self.sidebar_now() > 0.5 {
                self.workspace_gutter()
            } else {
                0.
            }))
            .py(px(self.workspace_gutter()))
            .child(
                self.workspace_surface(cx).child(
                    div()
                        .h_full()
                        .pt(px(if self.workspace_gutter() > 0. {
                            36.
                        } else {
                            Theme::TITLEBAR_HEIGHT
                        }))
                        .child(inner),
                ),
            )
            .into_any_element()
    }

    /// The sidebar column's tone, spanning the FULL window height (under the
    /// traffic lights, through the titlebar, down to the bottom edge), with a
    /// hairline on its right edge so the column reads as its own surface.
    /// The tone carries the window's left corners when the CSD window floats
    /// — with one caveat: a corner radius is clamped to the element's own
    /// size, and the COLLAPSED sidebar is a ~1px border sliver (the grab
    /// affordance). macOS trims that hairline with the window server's native
    /// corner clip; we reproduce the same trim by insetting the sliver
    /// vertically to where the curve begins, so its tips never float over the
    /// transparent corner cutouts.
    fn sidebar_tone(width: f32, border_color: gpui::Hsla, window: &Window) -> gpui::Div {
        let window_corner = Self::window_corner_radius(window);
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(px(width))
            .when(window_corner > 0.0, |el| {
                if width >= 2.0 * window_corner {
                    el.rounded_tl(px(window_corner))
                        .rounded_bl(px(window_corner))
                } else {
                    el.top(px(window_corner)).bottom(px(window_corner))
                }
            })
            .bg(crate::theme::wash(0.05))
            .border_r_1()
            .border_color(border_color)
    }

    /// Settings takes over the window: the section list stands where the
    /// chat sidebar was, the page fills the rest. Opening a preference never
    /// adds a route-history entry or changes the selected conversation.
    fn render_settings_page(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section = section.canonical();
        let theme = Theme::of(cx).for_settings_surface();
        if self.settings_focus_pending {
            self.settings_focus_pending = false;
            self.settings_return_focus = window.focused(cx);
            // Keep keyboard dispatch in settings without selecting a control.
            // Tab remains the explicit way to enter its navigation or content.
            window.focus(&self.settings_focus, cx);
        }
        let sidebar_width = if window.viewport_size().width < px(800.) {
            48.
        } else {
            208.
        };
        settings::widgets::set_sidebar_width(sidebar_width, cx);
        let nav = self.render_settings_nav(section, &theme, window, cx);
        let outlet = self.settings_outlet(section, window, cx);
        let back = div()
            .id("settings-back")
            .role(gpui::Role::Button)
            .aria_label(tr(cx, msg::settings::settings_page_back_to_app))
            .tab_index(0)
            .flex()
            .items_center()
            .gap(px(4.))
            .p(px(4.))
            .rounded(px(4.))
            .cursor_pointer()
            .text_size(crate::typography::text_xs())
            .text_color(theme.text_muted)
            .hover(|style| {
                style
                    .bg(theme.settings_frame_bg().opacity(0.4))
                    .text_color(theme.text)
            })
            .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
            .child(
                icon(icons::ALT_ARROW_LEFT)
                    .size(px(14.))
                    .text_color(theme.text_muted),
            )
            .child(tr(cx, msg::settings::settings_page_back_to_app));
        div()
            .id("settings-page")
            .role(gpui::Role::Group)
            .aria_label(tr(cx, msg::common::settings))
            .track_focus(&self.settings_focus)
            .tab_group()
            .tab_stop(false)
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        if !this.dismiss_settings_escape_surface(cx) {
                            this.close_settings(cx);
                        }
                        cx.stop_propagation();
                    }
                    "tab" => {
                        move_settings_focus(
                            &this.settings_focus,
                            &this.settings_end_focus,
                            event.keystroke.modifiers.shift,
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(12.))
                    .child(div().w(px(64.)).flex_none())
                    .child(back)
                    .child(div().flex_1()),
            )
            // Same frame as a chat: the navigation sits on the window glass
            // and the section is a rounded card inset from the window edges.
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .pr(px(zuse_viewport::workspace_style::GUTTER))
                    .pb(px(zuse_viewport::workspace_style::GUTTER))
                    .child(div().w(px(sidebar_width)).h_full().flex_none().child(nav))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .rounded(px(zuse_viewport::workspace_style::RADIUS))
                            .bg(theme.bg)
                            .border_1()
                            .border_color(crate::theme::hairline(0.05))
                            .opacity(self.voice_stage_underlay_opacity(cx))
                            .child(outlet),
                    ),
            )
            .child(
                div()
                    .id("settings-focus-end")
                    .track_focus(&self.settings_end_focus)
                    .tab_index(0)
                    .tab_stop(false),
            )
            .into_any_element()
    }

    /// Roving section tabs, grouped by spacing alone: preferences, agents and
    /// devices, then workspace data.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = window.viewport_size().width < px(800.);
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Cloud => icons::DITHER_CLOUD_FILLED,
            SettingsSection::Devices => icons::DITHER_MONITOR,
            SettingsSection::Harnesses => icons::DITHER_TERMINAL,
            SettingsSection::Defaults => icons::DITHER_SLIDERS,
            SettingsSection::Plugins => icons::DITHER_PLUG,
            SettingsSection::Agents => icons::DITHER_KEY,
            SettingsSection::Appearance => icons::DITHER_PEN,
            SettingsSection::Files => icons::DITHER_FOLDER,
            SettingsSection::Notifications => icons::DITHER_BELL,
            SettingsSection::Voice => icons::DITHER_MIC,
            SettingsSection::Shortcuts => icons::DITHER_KEYBOARD,
            SettingsSection::General => icons::DITHER_GEAR,
            SettingsSection::Appshots => icons::DITHER_DOCUMENT,
            SettingsSection::Archived => icons::DITHER_ARCHIVE,
            SettingsSection::CloudRepositories => icons::GIT_BRANCH,
            SettingsSection::CloudEnvironments => icons::DITHER_TERMINAL,
            SettingsSection::CloudMachine => icons::DITHER_MONITOR,
            SettingsSection::CloudAgents => icons::DITHER_KEY,
            SettingsSection::Usage => icons::DASHBOARD_SPEED,
            SettingsSection::Billing => icons::TAG,
        };
        settings::widgets::scroll_faded(
            "settings-nav-scroll",
            div()
                .id("settings-sections")
                .role(gpui::Role::TabList)
                .aria_label(tr(cx, msg::native::settings_sections))
                .w_full()
                .overflow_y_scroll()
                .h_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .px(px(if compact { 6. } else { 10. }))
                        .pt(px(12.))
                        .pb(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(
                            SettingsSection::ALL
                                .into_iter()
                                .filter(|item| item.visible_in_nav())
                                .flat_map(|item| {
                                    let heading = item.heading().map(|heading| {
                                        if compact {
                                            div()
                                                .mx(px(8.))
                                                .my(px(8.))
                                                .h(px(1.))
                                                .bg(theme.border)
                                                .into_any_element()
                                        } else {
                                            div()
                                                .px(px(8.))
                                                .pt(px(14.))
                                                .pb(px(4.))
                                                .text_size(crate::typography::text_xs())
                                                .text_color(theme.text_muted)
                                                .child(heading)
                                                .into_any_element()
                                        }
                                    });
                                    let index = SettingsSection::ALL
                                        .iter()
                                        .position(|s| *s == item)
                                        .unwrap();
                                    let selected = item == section;
                                    let key = format!("settings-nav-{}", item.label());
                                    let hover_key = format!("{key}-hover");
                                    let selection_t = settings::widgets::tab_selection_t(
                                        window,
                                        format!("{key}-selection"),
                                        selected,
                                        motion::reduced_motion(cx),
                                    );
                                    let tab_text = motion::hover_blend(
                                        &hover_key,
                                        motion::mix(
                                            theme.text_muted,
                                            theme.text_strong,
                                            selection_t,
                                        ),
                                        theme.text_strong,
                                    );
                                    let tab = settings::widgets::section_tab(
                                        theme,
                                        selected,
                                        selection_t,
                                        key.clone(),
                                        hover_key,
                                    )
                                    .when(compact, |view| view.justify_center().px(px(6.)))
                                    .role(gpui::Role::Tab)
                                    .aria_label(item.label())
                                    .aria_selected(selected)
                                    .track_focus(
                                        &self.settings_nav_focus[index].clone().tab_stop(selected),
                                    )
                                    .tab_index(0)
                                    .tab_stop(selected)
                                    .on_key_down(cx.listener(
                                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                                            let items: Vec<_> = SettingsSection::ALL
                                                .into_iter()
                                                .filter(|s| s.visible_in_nav())
                                                .collect();
                                            let current =
                                                items.iter().position(|s| *s == item).unwrap_or(0);
                                            let next = match event.keystroke.key.as_str() {
                                                "up" | "left" => {
                                                    (current + items.len() - 1) % items.len()
                                                }
                                                "down" | "right" => (current + 1) % items.len(),
                                                "home" => 0,
                                                "end" => items.len() - 1,
                                                _ => return,
                                            };
                                            let target = items[next];
                                            this.open_settings(target, cx);
                                            let index = SettingsSection::ALL
                                                .iter()
                                                .position(|s| *s == target)
                                                .unwrap();
                                            window.focus(&this.settings_nav_focus[index], cx);
                                            cx.stop_propagation();
                                        },
                                    ))
                                    .focus_visible(|s| s.border_2().border_color(theme.accent))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_settings(item, cx)
                                    }))
                                    .child(
                                        icon(section_icon(item))
                                            .size(px(14.0))
                                            .text_color(tab_text),
                                    )
                                    .when(!compact, |view| {
                                        view.child(SharedString::from(item.label()))
                                    });
                                    heading.into_iter().chain([tab.into_any_element()])
                                }),
                        ),
                ),
        )
        .into_any_element()
    }

    /// One session row: context + status on line one, harness + title on line
    /// two, and source metadata below. Working uses the live thread glyph in
    /// the status corner. Click selects; right-click opens the context menu.
    #[allow(clippy::too_many_arguments)]
    fn render_chat_row(
        &self,
        id: String,
        title: SharedString,
        time_ago: SharedString,
        space_name: SharedString,
        branch: Option<SharedString>,
        change_request: Option<zuse_protocol::ChangeRequestSummary>,
        harness: Option<zuse_protocol::HarnessId>,
        status: zuse_protocol::ChatIndicator,
        selected: bool,
        archived: bool,
        preview: bool,
        drag: Option<SidebarSessionDrag>,
        // This row's jump combo while the hint overlay is up. It takes the
        // corner outright — above hover and above the status word — so all
        // nine chips appear together instead of leaving a hole on whichever
        // row is busy or under the pointer.
        jump_label: Option<SharedString>,
        // Whether the row wears its project icon. Rows inside a project
        // group leave it to the group header.
        project_icon: bool,
        // A one-line row under a project heading: branch glyph, title, size.
        nested: bool,
        search_query: Option<&str>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The Plugins page owns the main area, so no chat reads as current.
        let selected = selected
            && !matches!(
                self.route,
                Route::Plugins | Route::Archives | Route::Usage | Route::Zot
            );
        // Activity, not position: status is a small colored
        // word + glyph in the row's top-right corner — Working animates the
        // composer-strip spinner, Done wears a check; Idle rows show the
        // relative time instead. Hovering the ROW swaps the corner for the
        // ARCHIVE button. Compact rows keep status first and elapsed time last;
        // their archive control occupies the remote-icon slot on hover.
        // A chat can appear on both surfaces at once. Namespace every hover
        // key and child id so the palette never animates the sidebar copy.
        let row_id = if search_query.is_some() {
            format!("palette-chat-{id}")
        } else {
            format!("chat-{id}")
        };
        let pr_error = {
            let state = self.state.read(cx);
            state
                .chats
                .iter()
                .find(|chat| chat.id == id)
                .and_then(|chat| state.change_request_error_for_chat(chat))
                .map(str::to_owned)
        };
        let pr_device = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == id)
            .map(|chat| chat.device_id.clone());
        // The sidebar's own row layout; the palette keeps its three-line card.
        let sidebar = search_query.is_none();
        let compact = sidebar && self.sidebar_compact_rows();
        let show_label = search_query.is_some() || self.sidebar_project_labels();
        let diff_stat = sidebar
            .then(|| {
                let state = self.state.read(cx);
                let chat = state.chats.iter().find(|chat| chat.id == id)?;
                state.diff_stat_for_chat(chat)
            })
            .flatten();
        let remote = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == id)
            .is_some_and(|chat| {
                self.state.read(cx).local_device_id.as_deref() != Some(chat.device_id.as_str())
            });
        let project_icon = (project_icon && search_query.is_none() && !nested).then(|| {
            self.render_project_icon(&id, spaces::SIDEBAR_PROJECT_AVATAR_SIZE, selected, cx)
        });
        // A cloud task row (`cloud:<workspace>`) opens its conversation; the
        // local-only actions (archive, rename, the chat menu) don't apply.
        let cloud = id.starts_with(CLOUD_ROW_PREFIX);
        let corner_hovered =
            !preview && !cloud && self.chat_status_hover.as_deref() == Some(row_id.as_str());
        let archived_muted = archived && search_query.is_none() && !selected && !corner_hovered;
        let project_icon = project_icon.map(|icon| {
            div()
                .flex_none()
                .opacity(if archived_muted { 0.4 } else { 1.0 })
                .child(icon)
                .into_any_element()
        });
        let content_id = id.clone();
        // Send-truth overrides: a send unadopted past the grace window is
        // FAILED (explicit, with the transcript's retry affordance); a send
        // whose delivery path is degraded is QUEUED, not Working — the
        // pending pill tells the truth instead of faking a spinner.
        let (queued, undelivered, awaiting) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            (
                state.send_queued(&id, now),
                state.send_undelivered(&id, now),
                state
                    .session_for(&id)
                    .and_then(|session| session.awaiting_input),
            )
        };
        let status_color = if undelivered {
            theme.danger
        } else if queued {
            theme.warning
        } else {
            spaces::status_dot_color(status, theme)
        };
        let status_label: Option<&'static str> = if undelivered {
            Some("Failed")
        } else if queued {
            Some("Queued")
        } else {
            match status {
                zuse_protocol::ChatIndicator::Working => Some("Working"),
                zuse_protocol::ChatIndicator::AwaitingInput => Some("Input"),
                zuse_protocol::ChatIndicator::Errored => Some("Failed"),
                zuse_protocol::ChatIndicator::Completed => Some("Done"),
                zuse_protocol::ChatIndicator::Idle => None,
            }
        };
        let shows_metadata = branch.is_some() || change_request.is_some();
        let attention_mark = |size: f32| -> AnyElement {
            match spaces::status_glyph(status, queued, undelivered, awaiting, theme) {
                Some((glyph, tone)) => icon(glyph)
                    .size(px(size))
                    .flex_none()
                    .text_color(tone)
                    .into_any_element(),
                None => div().size(px(size)).flex_none().into_any_element(),
            }
        };
        let queued = queued && !undelivered;
        let working = status == zuse_protocol::ChatIndicator::Working && !queued && !undelivered;
        let compact_status = sidebar.then(|| {
            let glyph = if working {
                loaders::retro_loader(format!("{row_id}-working"), 13.0, theme.loader_glyph())
                    .into_any_element()
            } else if status == zuse_protocol::ChatIndicator::Completed && !queued && !undelivered {
                icon(icons::CHECK)
                    .size(px(11.0))
                    .text_color(status_color)
                    .into_any_element()
            } else {
                attention_mark(13.0)
            };
            div()
                .id(SharedString::from(format!("{row_id}-status")))
                .debug_selector({
                    let id = id.clone();
                    move || format!("chat-status-{id}")
                })
                .size(px(13.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .aria_label(status_label.unwrap_or("Idle"))
                .child(glyph)
                .into_any_element()
        });
        let compact_jump_label = sidebar.then(|| jump_label.clone()).flatten();
        let corner_body: AnyElement = if let Some(label) = jump_label.filter(|_| !compact) {
            // The jump hint replaces the status/time corner while the modifier
            // is held, in the shared legend voice (the compact rows' plain
            // muted text, `popover::kbd_hint`).
            div()
                .flex_none()
                .whitespace_nowrap()
                .font_family(crate::popover::KEY_GLYPH_FONT)
                .text_size(crate::typography::text_xs())
                .text_color(theme.text_muted.opacity(0.5))
                .child(label)
                .into_any_element()
        } else if corner_hovered {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .h(px(18.0))
                .when(!compact, |el| {
                    el.px(px(4.0))
                        .mr(px(-4.0))
                        .rounded(px(5.0))
                        .bg(crate::theme::wash(0.10))
                        .hover(|s| s.bg(crate::theme::wash(0.18)))
                })
                .child(
                    icon(if archived {
                        icons::ARCHIVE_UP_MINIMALISTIC
                    } else {
                        icons::ARCHIVE_MINIMALISTIC
                    })
                    .size(px(if compact {
                        SIDEBAR_ACTIVE_HARNESS_ICON_SIZE
                    } else {
                        11.0
                    }))
                    .flex_none()
                    .text_color(theme.text_muted),
                )
                .when(!compact, |el| {
                    el.child(
                        div()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_muted)
                            .child(SharedString::from(if archived {
                                "Unarchive"
                            } else {
                                "Archive"
                            })),
                    )
                })
                .into_any_element()
        } else if compact {
            if remote {
                icon(icons::REMOTE_SERVER)
                    .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                    .text_color(theme.text_muted.opacity(0.5))
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        } else {
            match status_label {
                Some(label) => {
                    // Glyph slot: Working wears the preset's animated pixel
                    // glyph beside its label, Done wears the check, and the
                    // remaining statuses use their attention mark.
                    let glyph: AnyElement = if status == zuse_protocol::ChatIndicator::Completed {
                        icon(icons::CHECK)
                            .size(px(11.0))
                            .flex_none()
                            .text_color(status_color)
                            .into_any_element()
                    } else if working {
                        loaders::retro_loader(
                            format!("{row_id}-working"),
                            13.0,
                            theme.loader_glyph(),
                        )
                        .into_any_element()
                    } else {
                        attention_mark(13.0)
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.0))
                                .child(glyph)
                                .child(
                                    div()
                                        .text_size(crate::typography::text_xs())
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(status_color)
                                        .child(SharedString::from(label)),
                                ),
                        )
                        .into_any_element()
                }
                None => div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(crate::typography::text_xs())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(time_ago.clone())
                    .into_any_element(),
            }
        };
        // One stable wrapper across both states (identity keeps the hover
        // from flickering as the content swaps); the swap is driven by the
        // ROW's hover (user request — corner-only felt undiscoverable), but
        // archiving only clicks on the corner itself, so the row's own click
        // stays the selector.
        let corner: AnyElement = {
            let archive_id = id.clone();
            div()
                .id(SharedString::from(format!("{row_id}-corner")))
                .debug_selector({
                    let row_id = row_id.clone();
                    move || format!("{row_id}-corner")
                })
                .aria_label(if corner_hovered {
                    if archived { "Unarchive" } else { "Archive" }
                } else {
                    if compact {
                        if remote {
                            "Remote session"
                        } else {
                            "Session actions"
                        }
                    } else {
                        status_label.unwrap_or("Idle")
                    }
                })
                .when(compact, |el| el.w(px(18.0)).justify_center())
                .flex_none()
                // Pin the corner to line 1's text height so the archive pill
                // (taller, padded) overflows vertically instead of growing the
                // row — the swap must not shift the card's content.
                // NO occlude: the ROW's hover drives the swap, and an
                // occluding corner un-hovered the row underneath it —
                // pill mounts, steals the pointer, row un-hovers, pill
                // unmounts, repeat (user-reported flicker). The pill's
                // stop_propagation click is separation enough.
                .h(px(14.0))
                .flex()
                .items_center()
                .when(!preview, |el| el.cursor_pointer())
                .when(corner_hovered, |el| {
                    el.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.chat_hover_resync = true;
                            this.set_chat_archived(archive_id.clone(), !archived, cx);
                        }))
                        // Above the pill: below it the chip would cover the next row.
                        .tip_above(if archived {
                            "Unarchive session"
                        } else {
                            ShortcutId::ArchiveSession.label()
                        })
                })
                .child(corner_body)
                .into_any_element()
        };
        let mut corner = Some(corner);
        let hover = theme.glass_hover();
        let selected_wash = crate::theme::glass_selected_bg();
        // Metadata reads in the subtle text role, as do read titles;
        // titles turn strong once selected or unread.
        let subline = theme.text_muted;
        let select_id = id.clone();
        let menu_id = id.clone();
        let search_query_none = search_query.is_none();
        let rename_input = (search_query_none && !preview && !cloud)
            .then(|| self.rename_input_for(&id, ChatRenameSurface::Sidebar))
            .flatten();
        // Hover fades over transition-colors (zuse session-row.tsx) — both
        // the wash and the title brighten ride the same 150ms blend.
        let fade_key = format!("{row_id}-hover");
        let rest_bg = if selected {
            selected_wash
        } else {
            crate::theme::wash(0.0)
        };
        let hover_bg = if selected { selected_wash } else { hover };
        // The row a context menu was opened on stays lit while the menu is
        // up; the pointer leaving it for the menu must not drop the wash.
        let menu_target = !preview && self.chat_menu_targets(&id);
        // Brightness is the read state: titles rest gray and an unseen finish
        // stands out in full white, with no status mark beside it.
        let finished_unseen =
            status == zuse_protocol::ChatIndicator::Completed && !queued && !undelivered;
        let rest_text = if selected || (finished_unseen && !archived) {
            theme.text_strong
        } else {
            theme.text_muted
        };
        let row = div()
            .id(SharedString::from(row_id.clone()))
            .group("sidebar-session-row")
            .debug_selector({
                let row_id = row_id.clone();
                move || row_id.clone()
            })
            .h(px(if sidebar {
                sidebar_row_height(compact, show_label, nested)
            } else {
                chat_row_height(branch.is_some(), change_request.is_some())
            }))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(if search_query.is_some() {
                popover::PALETTE_ITEM_RADIUS
            } else {
                SIDEBAR_ROW_RADIUS
            }))
            .px(px(Theme::SPACE_SM))
            .when(sidebar, |el| el.justify_center())
            .when(!sidebar, |el| el.py(px(6.0)))
            .text_color(if menu_target {
                theme.text_strong
            } else {
                motion::hover_blend(&fade_key, rest_text, theme.text_strong)
            })
            .bg(if menu_target {
                hover_bg
            } else {
                motion::hover_blend(&fade_key, rest_bg, hover_bg)
            })
            // No selection ring (user request) — the wash alone marks the
            // active row.
            // Row hover drives BOTH the wash blend and the corner's
            // status→Archive swap (one listener — gpui allows a single
            // hover listener per element).
            .when(!preview, |el| {
                el.on_hover({
                    let fade_hover = motion::hover_listener(fade_key.clone());
                    let hover_id = row_id.clone();
                    cx.listener(move |this, hovered: &bool, window, cx| {
                        fade_hover(hovered, window, cx);
                        this.chat_hover_resync = false;
                        if *hovered {
                            if this.chat_status_hover.as_deref() != Some(hover_id.as_str()) {
                                this.chat_status_hover = Some(hover_id.clone());
                                cx.notify();
                            }
                        } else if this.chat_status_hover.as_deref() == Some(hover_id.as_str()) {
                            this.chat_status_hover = None;
                            cx.notify();
                        }
                    })
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                    if cloud {
                        this.open_cloud_sidebar_row(&select_id, cx);
                        return;
                    }
                    // The palette's copy of the row only opens; the sidebar's
                    // double-click edits the title in place.
                    if event.click_count() >= 2 && search_query_none {
                        this.open_rename_chat(select_id.clone(), cx);
                    } else {
                        this.open_chat(select_id.clone(), cx);
                    }
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if cloud {
                            return;
                        }
                        this.chat_menu.open(ChatMenuState {
                            tab: None,
                            chat_id: menu_id.clone(),
                            position: event.position,
                            page: ChatMenuPage::Root,
                        });
                        cx.notify();
                    }),
                )
            })
            .when_some(drag, |el, payload| {
                let shell = cx.entity();
                el.on_drag(payload, move |payload, point, window, cx| {
                    shell.update(cx, |shell, cx| {
                        shell.begin_sidebar_session_transfer(payload, point, window, cx);
                    });
                    cx.stop_propagation();
                    cx.new(|_| DragGhost)
                })
            })
            // A row revealed for an inline rename reports where it painted,
            // frame by frame while its disclosure opens, so the list can
            // scroll it clear of the edges.
            .when(
                rename_input.is_some() && self.chat_rename_revealing(),
                |el| {
                    let shell = cx.weak_entity();
                    el.child(
                        gpui::canvas(
                            |_, _, _| {},
                            move |bounds, _, window, cx| {
                                window.defer(cx, move |_, cx| {
                                    shell
                                        .update(cx, |this, cx| {
                                            this.keep_rename_row_in_view(bounds, cx)
                                        })
                                        .ok();
                                });
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                },
            )
            .when(self.chat_hover_resync && !preview, |el| {
                let shell = cx.weak_entity();
                let row_id = row_id.clone();
                el.child(
                    gpui::canvas(
                        |bounds, window, _| {
                            window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
                        },
                        move |_, hitbox, window, cx| {
                            // The hit test is final by paint, so popovers
                            // over the row still occlude it.
                            let hovered = hitbox.is_hovered(window) && !cx.has_active_drag();
                            if hovered == corner_hovered {
                                return;
                            }
                            window.defer(cx, move |_, cx| {
                                shell
                                    .update(cx, |this, cx| {
                                        this.sync_chat_row_hover(row_id, hovered, cx)
                                    })
                                    .ok();
                            });
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
            });
        let row = if sidebar {
            // Sidebar rows: one line under a project heading (branch glyph,
            // title, size or age), or the one-list card (avatar; title + age;
            // project + size). The trailing slot swaps to the archive control
            // on hover and to the jump legend while the modifier is held.
            let two_line = !(compact || nested || !show_label);
            // Threads view cards also badge attention (input, failure,
            // queued) on the avatar and set an unseen finish in bold, as the
            // reference thread list does.
            let avatar_badge = !nested && working && project_icon.is_some();
            let unread = two_line && finished_unseen;
            let attention_badge = two_line
                && !working
                && !finished_unseen
                && status_label.is_some()
                && project_icon.is_some();
            // Waiting on the user reads amber on the card, apart from the
            // routine activity tones.
            let attention_color =
                if status == zuse_protocol::ChatIndicator::AwaitingInput && !queued && !undelivered
                {
                    theme.warning
                } else {
                    status_color
                };
            let faint = theme.text_muted.opacity(0.6);
            let show_legend = compact_jump_label.is_some();
            let show_archive = corner_hovered && !show_legend && !preview;
            let text_hint = compact_jump_label
                .as_ref()
                .is_some_and(|label| label.chars().count() > 3);
            let archive_id = id.clone();
            let trailing_info: AnyElement = if show_archive {
                icon(if archived {
                    icons::ARCHIVE_UP_MINIMALISTIC
                } else {
                    icons::ARCHIVE_MINIMALISTIC
                })
                .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                .flex_none()
                .text_color(theme.text_muted)
                .into_any_element()
            } else if let (false, false, Some((additions, deletions))) =
                (show_legend, two_line, diff_stat)
            {
                sidebar_diff_stat(additions, deletions, 1.0, theme).into_any_element()
            } else if let (false, true, true, Some(label)) =
                (show_legend, two_line, attention_badge, status_label)
            {
                // A card waiting on the user names why in place of its age.
                div()
                    .whitespace_nowrap()
                    .text_size(crate::typography::text_xs())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(attention_color)
                    .child(if label == "Input" {
                        "Needs input"
                    } else {
                        label
                    })
                    .into_any_element()
            } else {
                let label: SharedString = match compact_jump_label.clone() {
                    Some(label) => label,
                    None if nested && time_ago.as_ref() != "now" => {
                        format!("{time_ago} ago").into()
                    }
                    None => time_ago.clone(),
                };
                div()
                    .whitespace_nowrap()
                    .text_right()
                    .text_size(crate::typography::text_xs())
                    .text_color(faint)
                    .child(label)
                    .into_any_element()
            };
            let trailing = div()
                .id(SharedString::from(format!("{row_id}-corner")))
                .debug_selector({
                    let id = id.clone();
                    move || format!("chat-time-{id}")
                })
                .when(text_hint, |el| {
                    el.min_w(crate::typography::ui_rems(COMPACT_JUMP_HINT_WIDTH))
                })
                // Holds "17m" so swapping in the archive glyph never moves it.
                .when(!text_hint, |el| el.min_w(px(30.0)))
                .h(px(14.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .aria_label(if show_archive {
                    if archived { "Unarchive" } else { "Archive" }
                } else {
                    "Session time"
                })
                .when(show_archive, |el| {
                    el.cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.chat_hover_resync = true;
                            this.set_chat_archived(archive_id.clone(), !archived, cx);
                        }))
                        .tip_above(if archived {
                            "Unarchive session"
                        } else {
                            ShortcutId::ArchiveSession.label()
                        })
                })
                .child(trailing_info);
            // Activity (working, input, done, failed) takes the lead glyph's
            // place; idle rows under a project wear their branch, tinted by
            // the pull request's state.
            // A working card wears its spinner as a badge on the avatar (the
            // title stays clean); other activity leads the title.
            let status_mark = status_label
                .filter(|_| !finished_unseen)
                .and(compact_status);
            let (status_glyph, title_glyph) = if nested {
                (status_mark, None)
            } else if avatar_badge || attention_badge || unread {
                (None, None)
            } else {
                (None, status_mark)
            };
            let lead: Option<AnyElement> = if nested {
                Some(
                    div()
                        .size(px(20.0))
                        .ml(px(6.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(match status_glyph {
                            Some(glyph) => glyph,
                            None => {
                                let (glyph, tone) = crate::change_requests::branch_state_glyph(
                                    change_request.as_ref(),
                                    theme,
                                );
                                icon(glyph)
                                    .size(px(12.0))
                                    .text_color(tone.opacity(if archived_muted {
                                        0.4
                                    } else {
                                        0.85
                                    }))
                                    .into_any_element()
                            }
                        })
                        .into_any_element(),
                )
            } else {
                None
            };
            let project_icon = project_icon.map(|icon| {
                if attention_badge {
                    return div()
                        .relative()
                        .flex_none()
                        .child(icon)
                        .child(
                            div()
                                .id(SharedString::from(format!("{row_id}-attention-badge")))
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("chat-status-{id}")
                                })
                                .aria_label(status_label.unwrap_or("Idle"))
                                .absolute()
                                .right(px(-4.0))
                                .bottom(px(-4.0))
                                .size(px(13.0))
                                .rounded_full()
                                .bg(theme.bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(attention_mark(10.0)),
                        )
                        .into_any_element();
                }
                if !avatar_badge {
                    return icon;
                }
                div()
                    .relative()
                    .flex_none()
                    .child(icon)
                    .child(
                        div()
                            .id(SharedString::from(format!("{row_id}-working-badge")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("chat-status-{id}")
                            })
                            .aria_label(tr(cx, msg::projects::masters_working))
                            .absolute()
                            .right(px(-4.0))
                            .bottom(px(-4.0))
                            .size(px(13.0))
                            .rounded_full()
                            .bg(theme.bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(loaders::retro_loader(
                                format!("{row_id}-working"),
                                10.0,
                                theme.loader_glyph(),
                            )),
                    )
                    .into_any_element()
            });
            let avatar = (!nested)
                .then(|| {
                    project_icon.or_else(|| {
                        harness
                            .map(crate::pickers::harness_brand_icon)
                            .map(|(path, tint)| {
                                div()
                                    .size(px(spaces::SIDEBAR_PROJECT_AVATAR_SIZE))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icons::brand_icon(
                                        path,
                                        14.0,
                                        tint.unwrap_or(subline).opacity(0.8),
                                    ))
                                    .into_any_element()
                            })
                    })
                })
                .flatten();
            let title_el = match rename_input {
                Some(input) => chat_title_editor(
                    format!("chat-title-editor-{content_id}").into(),
                    input,
                    theme,
                ),
                None => sidebar_faded_label(
                    format!("chat-title-{content_id}").into(),
                    true,
                    div()
                        .text_size(crate::typography::text_sm())
                        .line_height(crate::typography::ui_rems(18.0))
                        .when(unread, |el| {
                            el.font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_strong)
                        })
                        .child(title),
                )
                .into_any_element(),
            };
            let title_line = div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .children(title_glyph)
                .child(title_el)
                .child(trailing);
            let body = if two_line {
                // Line 2: project, then the pull request (state glyph,
                // number, state word), then the diff stat at the far right.
                let meta = div()
                    .w_full()
                    .min_w_0()
                    .h(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(crate::typography::text_xs())
                    .line_height(px(14.0))
                    .text_color(faint)
                    .child(sidebar_faded_label(
                        format!("chat-device-{content_id}").into(),
                        false,
                        div().child(space_name),
                    ))
                    .children(change_request.clone().map(|summary| {
                        crate::change_requests::pull_request_state_inline(
                            format!("{row_id}-pr").into(),
                            summary,
                            !preview,
                            pr_device.clone(),
                            faint,
                            theme,
                        )
                    }))
                    .child(div().flex_1())
                    .children(diff_stat.map(|(additions, deletions)| {
                        sidebar_diff_stat(additions, deletions, 0.75, theme)
                            .text_size(crate::typography::text_xs())
                    }));
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(title_line)
                    .child(meta)
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(title_line)
                    .into_any_element()
            };
            row.child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(if two_line { 10.0 } else { 8.0 }))
                    .children(lead)
                    .children(avatar)
                    .child(body),
            )
        } else {
            row
                // Line 1: "project @ device", status word / time-ago right.
                .when(!compact && show_label, |el| {
                    el.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_SM))
                            .child(sidebar_faded_label(
                                format!("chat-device-{content_id}").into(),
                                true,
                                div()
                                    .text_size(crate::typography::text_xs())
                                    .line_height(px(14.0))
                                    .text_color(subline)
                                    .child(popover::search_highlight(
                                        space_name,
                                        search_query,
                                        theme,
                                    )),
                            ))
                            .child(div().text_color(subline).children(corner.take())),
                    )
                })
                // Line 2: harness identity belongs directly with the title,
                // instead of floating as unrelated metadata below it.
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(if compact {
                            4.0
                        } else {
                            SIDEBAR_ACTIVE_HARNESS_TITLE_GAP
                        }))
                        .children(compact_status)
                        .when_some(
                            harness.map(crate::pickers::harness_brand_icon),
                            |el, (path, tint)| {
                                el.child(icons::brand_icon(
                                    path,
                                    SIDEBAR_ACTIVE_HARNESS_ICON_SIZE,
                                    tint.unwrap_or(subline).opacity(if archived_muted {
                                        0.4
                                    } else {
                                        0.8
                                    }),
                                ))
                            },
                        )
                        .children(project_icon)
                        .child(match rename_input {
                            Some(input) => chat_title_editor(
                                format!("chat-title-editor-{content_id}").into(),
                                input,
                                theme,
                            ),
                            None => sidebar_faded_label(
                                format!("chat-title-{content_id}").into(),
                                true,
                                div()
                                    .text_size(crate::typography::text_sm())
                                    .line_height(px(17.0))
                                    .child(popover::search_highlight(title, search_query, theme)),
                            )
                            .into_any_element(),
                        })
                        .when(!compact && !show_label && remote, |el| {
                            el.child(
                                icon(icons::REMOTE_SERVER)
                                    .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                    .flex_none()
                                    .text_color(subline),
                            )
                        })
                        .when(!compact && !show_label, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_color(subline)
                                    .children(corner.take()),
                            )
                        })
                        // Compact trailing cluster, right-packed: the PR badge sits
                        // LEFT of one fixed slot that shows the relative time, swaps
                        // to the archive affordance while the row is hovered, and
                        // shows the Cmd+N jump legend while the modifier is held.
                        // The legend wins over the hover archive (user request).
                        // There is no remote globe — the slot's fixed width keeps
                        // the column aligned across rows regardless of a PR badge.
                        .when(compact, |el| {
                            // Cmd-held legend beats the hover archive; preview rows
                            // (the palette) never archive.
                            let show_legend = compact_jump_label.is_some();
                            let show_archive = corner_hovered && !show_legend && !preview;
                            // The slot is 30px (holds "17m"); a text-length jump
                            // hint widens it to a floor so "Ctrl+1" vs "Ctrl+2"
                            // never nudges the column.
                            let text_hint = compact_jump_label
                                .as_ref()
                                .is_some_and(|label| label.chars().count() > 3);
                            let archive_id = id.clone();
                            el.children(change_request.clone().map(|summary| {
                                if preview {
                                    crate::change_requests::pull_request_badge_preview(
                                        format!("{row_id}-compact-pr").into(),
                                        summary,
                                        crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                        theme,
                                    )
                                } else {
                                    crate::change_requests::pull_request_badge(
                                        format!("{row_id}-compact-pr").into(),
                                        summary,
                                        crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                        pr_device.clone(),
                                        theme,
                                    )
                                }
                            }))
                            .children(pr_error.clone().filter(|_| !preview).map(|error| {
                                crate::change_requests::unavailable_badge(
                                    format!("{row_id}-pr-error").into(),
                                    error,
                                    theme,
                                )
                            }))
                            .child(
                                div()
                                    .id(SharedString::from(format!("{row_id}-corner")))
                                    .debug_selector({
                                        let id = id.clone();
                                        move || format!("chat-time-{id}")
                                    })
                                    .when(text_hint, |el| {
                                        el.min_w(crate::typography::ui_rems(
                                            COMPACT_JUMP_HINT_WIDTH,
                                        ))
                                    })
                                    .when(!text_hint, |el| el.min_w(px(30.0)))
                                    // Fixed height so swapping the time text for the
                                    // archive glyph never resizes the slot.
                                    .h(px(14.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .aria_label(if show_archive {
                                        if archived { "Unarchive" } else { "Archive" }
                                    } else {
                                        "Session time"
                                    })
                                    .when(show_archive, |el| {
                                        el.cursor_pointer()
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.chat_hover_resync = true;
                                                this.set_chat_archived(
                                                    archive_id.clone(),
                                                    !archived,
                                                    cx,
                                                );
                                            }))
                                            .tip_above(if archived {
                                                "Unarchive session"
                                            } else {
                                                ShortcutId::ArchiveSession.label()
                                            })
                                    })
                                    .child(if show_archive {
                                        icon(if archived {
                                            icons::ARCHIVE_UP_MINIMALISTIC
                                        } else {
                                            icons::ARCHIVE_MINIMALISTIC
                                        })
                                        .size(px(SIDEBAR_ACTIVE_HARNESS_ICON_SIZE))
                                        .flex_none()
                                        .text_color(theme.text_muted)
                                        .into_any_element()
                                    } else {
                                        div()
                                            .whitespace_nowrap()
                                            .text_right()
                                            .text_size(crate::typography::text_xs())
                                            .text_color(subline)
                                            .child(compact_jump_label.unwrap_or(time_ago))
                                            .into_any_element()
                                    }),
                            )
                        }),
                )
                // Line 3 is structural, not reserved whitespace: compact states
                // omit it completely when both Branch and Pull request are hidden.
                .when(!compact && shows_metadata, |row| {
                    row.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .when_some(branch, |el, branch| {
                                el.child(
                                    icon(icons::GIT_STATE_BRANCH)
                                        .size(px(11.0))
                                        .flex_none()
                                        .text_color(subline),
                                )
                                .child(sidebar_faded_label(
                                    format!("chat-branch-{content_id}").into(),
                                    false,
                                    div()
                                        .text_size(crate::typography::text_xs())
                                        .line_height(px(14.0))
                                        .text_color(subline)
                                        .child(popover::search_highlight(
                                            branch,
                                            search_query,
                                            theme,
                                        )),
                                ))
                            })
                            // Stable invisible spring keeps the optional PR badge
                            // pinned right without changing no-PR paint.
                            .child(div().flex_1().min_w_0())
                            .when_some(change_request, |el, summary| {
                                el.child(if preview {
                                    crate::change_requests::pull_request_badge_preview(
                                        format!("{row_id}-pr").into(),
                                        summary,
                                        crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                        theme,
                                    )
                                } else {
                                    crate::change_requests::pull_request_badge_with_query(
                                        format!("{row_id}-pr").into(),
                                        summary,
                                        crate::change_requests::ChangeRequestBadgeSurface::Sidebar,
                                        search_query,
                                        pr_device.clone(),
                                        theme,
                                    )
                                })
                            })
                            .children(pr_error.clone().filter(|_| !preview).map(|error| {
                                crate::change_requests::unavailable_badge(
                                    format!("{row_id}-pr-error").into(),
                                    error,
                                    theme,
                                )
                            })),
                    )
                })
        };
        row.into_any_element()
    }

    fn render_pinned_session_group(
        items: Vec<AnyElement>,
        extra_gap: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = items.len();
        let row_centers = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Pixels>::new()));
        div()
            .on_children_prepainted({
                let row_centers = row_centers.clone();
                move |bounds, _, _| {
                    *row_centers.borrow_mut() = bounds.iter().map(|row| row.center().y).collect();
                }
            })
            .id("sidebar-pinned-sessions")
            .flex()
            .flex_col()
            .gap(px(SIDEBAR_LIST_GAP))
            .on_drag_move::<SidebarSessionDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<SidebarSessionDrag>, _, cx| {
                    let payload = event.drag(cx).clone();
                    if !event.bounds.contains(&event.event.position)
                        || !this.sidebar_scroll.bounds().contains(&event.event.position)
                        || payload.visible_ids.len() != count
                    {
                        return;
                    }
                    let rel_y = f32::from(event.event.position.y) - f32::from(event.bounds.top());
                    if !payload.visible_ids.contains(&payload.chat_id) {
                        let top =
                            f32::from(event.bounds.bottom() - this.sidebar_scroll.bounds().top())
                                - extra_gap
                                + SIDEBAR_LIST_GAP
                                - f32::from(this.sidebar_scroll.offset().y);
                        if let Some(drag) = this.sidebar_session_transfer.as_mut() {
                            drag.preview = Some(SidebarSessionGap {
                                group: "pinned".into(),
                                index: count,
                                pinned: true,
                                top,
                            });
                        }
                    }
                    if payload.visible_ids.contains(&payload.chat_id)
                        && let Some(over) =
                            spaces::row_drop_index(rel_y, &this.sidebar_pinned_heights, false)
                    {
                        this.update_pinned_session_drag(&payload, over, cx);
                    }
                },
            ))
            .on_drop::<SidebarSessionDrag>(cx.listener(
                move |this, payload: &SidebarSessionDrag, window, cx| {
                    if payload.visible_ids.len() == count {
                        let index = this
                            .sidebar_session_transfer
                            .as_ref()
                            .filter(|_| !payload.visible_ids.contains(&payload.chat_id))
                            .and_then(|drag| drag.preview.as_ref())
                            .filter(|gap| gap.pinned)
                            .map(|gap| gap.index)
                            .or_else(|| this.pinned_session_drag.as_ref().map(|drag| drag.over))
                            .unwrap_or_else(|| {
                                row_centers
                                    .borrow()
                                    .iter()
                                    .position(|center| window.mouse_position().y < *center)
                                    .unwrap_or(count)
                            });
                        this.finish_sidebar_session_transfer(
                            payload,
                            SidebarSessionDrop::Pinned(index),
                            cx,
                        );
                    } else {
                        this.cancel_sidebar_session_transfer(cx);
                    }
                },
            ))
            .children(items)
            .when(extra_gap > 0.0, |el| {
                let height = extra_gap - if count > 0 { SIDEBAR_LIST_GAP } else { 0.0 };
                el.child(
                    div()
                        .flex_none()
                        .h(px(height.max(0.0)))
                        .mb(px(height.min(0.0))),
                )
            })
            .into_any_element()
    }

    /// Chat-mode sidebar (spaces overhaul): window-control strip, the Spaces
    /// section (folder + device rows, add-space), the global Active sessions
    /// list, the notice strip, and the UserMenu (§1.6).
    /// Global connection health, with selected-chat queue and storage status.
    /// Persistence failures take precedence even when the network is offline.
    /// No surface,
    /// no border (v0.2.12 feedback): a bare spinner + faint caption while
    /// reconnecting; an amber dot only when the OS says offline. The
    /// transport error belongs in logs, not the sidebar.
    fn render_connection_pill(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        use zuse_protocol::ConnectivityState as S;
        let conn = self.state.read(cx).connectivity.clone();
        let selected = self.state.read(cx).selected_chat.as_deref();
        let chat = conn
            .chats
            .iter()
            .find(|c| Some(c.chat_id.as_str()) == selected);
        let chat_state = chat.map(|c| c.sync_state);
        let (label, glyph): (SharedString, AnyElement) = match conn.state {
            _ if chat_state == Some(zuse_protocol::ChatSyncState::StorageError) => (
                "Changes could not be saved".into(),
                div()
                    .size(px(5.0))
                    .rounded_full()
                    .bg(theme.warning)
                    .into_any_element(),
            ),
            S::Disabled => return None,
            S::Connected => {
                let caption = chat_sync_pill_caption(chat?)?;
                (
                    caption.into(),
                    loaders::mini_mono_spinner(
                        "chat-sync-spinner",
                        2.0,
                        theme.text_muted,
                        self.sidebar_pane.entity_id(),
                        cx,
                    )
                    .into_any_element(),
                )
            }
            S::Offline => (
                "Offline — sends are saved".into(),
                div()
                    .size(px(5.0))
                    .rounded_full()
                    .bg(theme.warning)
                    .into_any_element(),
            ),
            S::Reconnecting => (
                "Reconnecting…".into(),
                loaders::mini_mono_spinner(
                    "connection-spinner",
                    2.0,
                    theme.text_muted,
                    self.sidebar_pane.entity_id(),
                    cx,
                )
                .into_any_element(),
            ),
        };
        Some(
            crate::motion::fade_in(
                "connection-pill",
                div()
                    .id("connection-pill")
                    .mx(px(Theme::SPACE_SM + 4.0))
                    .mb(px(Theme::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(glyph)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_faint)
                            .child(label),
                    ),
            )
            .into_any_element(),
        )
    }

    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.sidebar_session_transfer.as_ref().is_some_and(|drag| {
            !cx.has_active_drag() || !self.sidebar_session_transfer_is_valid(&drag.payload, cx)
        }) {
            self.cancel_sidebar_session_transfer(cx);
        }
        // A release outside the sidebar ends GPUI's drag without calling the
        // sidebar drop handler. Heal the ephemeral slide state here.
        if self.pinned_session_drag.is_some()
            && (!cx.has_active_drag() || !self.pinned_session_drag_is_valid(cx))
        {
            self.cancel_pinned_session_drag(cx);
        }

        // A row archived (or deleted) under the pointer unmounts without its
        // hover ending: forget it, so the next row the pointer rests on can
        // take the hover.
        if let Some(chat_id) = self
            .chat_status_hover
            .as_deref()
            .and_then(|row| row.strip_prefix("chat-"))
        {
            let listed = self
                .state
                .read(cx)
                .chats
                .iter()
                .any(|chat| chat.id == chat_id && !chat.archived);
            if !listed {
                self.chat_status_hover = None;
            }
        }

        // Keyed rows: (stable key, estimated height, element) — the key + height
        // list drives the §1.6 resort FLIP diff below (attention-bucket
        // promotions glide; cleared rows just go).
        let session_rows = self.render_active_rows(theme, cx);
        let moving_row = session_rows
            .moving_row
            .map(|(row, height)| self.render_moving_sidebar_session(row, height, theme));
        let pinned_count = session_rows.pinned_count;
        let custom_count = session_rows.custom_count;
        let regular_start = pinned_count + custom_count;
        let keyed = session_rows.rows;
        let ungrouped = self.settings.sidebar_organization == SidebarOrganization::InOneList;
        let pinned_body_height = spaces::SIDEBAR_DISCLOSURE_BODY_INSET
            + self.sidebar_transfer_extra_gap("pinned")
            + keyed
                .iter()
                .take(pinned_count)
                .map(|(_, height, _)| height)
                .sum::<f32>()
            + SIDEBAR_LIST_GAP * pinned_count.saturating_sub(1) as f32;

        // Resort glide (§1.6 View Transitions parity): when the ORDER of a live
        // list changes (new activity resort, grouping flip), surviving rows
        // glide from their old y to the new one — layout is already at the new
        // position; the offset is a paint-only relative inset animated to 0
        // over 260ms cubic-bezier(0.22,1,0.36,1). New rows fade in; removals
        // just go (matching the original). First fill and chat switches (which
        // don't reorder) never animate.
        let mut order: Vec<(String, f32)> = Vec::with_capacity(keyed.len() + 2);
        let show_pinned_section = pinned_count > 0 || self.sidebar_session_transfer.is_some();
        if show_pinned_section {
            order.push((
                "sidebar-pinned-header".to_string(),
                spaces::SIDEBAR_DISCLOSURE_HEADER_HEIGHT
                    + if self.pinned_open {
                        spaces::SIDEBAR_DISCLOSURE_BODY_INSET - SIDEBAR_LIST_GAP
                    } else {
                        0.0
                    },
            ));
        }
        // Threads read as one list: no heading over the unpinned rows, just
        // a section's breath below Pinned or a custom section.
        let regular_gap = ungrouped && (show_pinned_section || custom_count > 0);
        for (ix, (key, height, _)) in keyed.iter().enumerate() {
            if regular_gap && ix == regular_start {
                order.push((
                    "sidebar-regular-gap".into(),
                    SIDEBAR_THREADS_GAP - SIDEBAR_LIST_GAP,
                ));
            }
            if ix < pinned_count && !self.pinned_open {
                continue;
            }
            order.push((key.clone(), *height));
        }
        if self.pinned_session_drag.is_none()
            && self.sidebar_session_transfer.is_none()
            && self.sidebar_prev_order != order
        {
            let key_order_changed = sidebar_key_order_changed(&self.sidebar_prev_order, &order);
            if !self.sidebar_prev_order.is_empty() {
                // A disclosure already animates its own body height. Applying
                // FLIP offsets when only keyed heights change double-counts
                // that movement, leaving gaps and momentary overlaps between
                // the first group and the groups that follow.
                let offsets = if key_order_changed {
                    resort_offsets(&self.sidebar_prev_order, &order, SIDEBAR_LIST_GAP)
                } else {
                    std::collections::HashMap::new()
                };
                let prev_keys: std::collections::HashSet<&str> = self
                    .sidebar_prev_order
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect();
                let new_keys: std::collections::HashSet<String> = order
                    .iter()
                    .filter(|(k, _)| !prev_keys.contains(k.as_str()))
                    .map(|(k, _)| k.clone())
                    .collect();
                if key_order_changed && (!offsets.is_empty() || !new_keys.is_empty()) {
                    self.resort_epoch += 1;
                    self.sidebar_resort = offsets;
                    self.sidebar_new_keys = new_keys;
                }
            }
            self.sidebar_prev_order = order;
        }
        let epoch = self.resort_epoch;
        let pinned_drag = self
            .pinned_session_drag
            .as_ref()
            .filter(|_| {
                self.sidebar_session_transfer
                    .as_ref()
                    .is_none_or(|drag| drag.preview.as_ref().is_none_or(|gap| gap.pinned))
            })
            .map(|drag| (drag.from, drag.over, drag.prev_over, drag.epoch));
        let list_items: Vec<AnyElement> = keyed
            .into_iter()
            .enumerate()
            .map(|(ix, (key, _, element))| {
                if ix < pinned_count
                    && let Some((from, over, prev_over, drag_epoch)) = pinned_drag
                {
                    // Its actual row is drawn once in the sidebar's movement layer.
                    if ix == from {
                        return element;
                    }
                    let start = crate::terminal::panel::slide_offset(ix, from, prev_over)
                        * (self
                            .sidebar_pinned_heights
                            .get(from)
                            .copied()
                            .unwrap_or(61.0)
                            + SIDEBAR_LIST_GAP);
                    let target = crate::terminal::panel::slide_offset(ix, from, over)
                        * (self
                            .sidebar_pinned_heights
                            .get(from)
                            .copied()
                            .unwrap_or(61.0)
                            + SIDEBAR_LIST_GAP);
                    if self.reduced_motion {
                        return div()
                            .relative()
                            .top(px(target))
                            .child(element)
                            .into_any_element();
                    }
                    return div()
                        .child(element)
                        .with_animation(
                            (
                                "pinned-session-slide",
                                (ix as u64) | ((drag_epoch as u64) << 32),
                            ),
                            TAB_SLIDE.animation(),
                            move |el, t| el.relative().top(px(motion::lerp(start, target, t))),
                        )
                        .into_any_element();
                }
                if self.sidebar_session_transfer.is_some() {
                    return element;
                }
                if let Some(dy) = self.sidebar_resort.get(&key).copied() {
                    let id = SharedString::from(format!("resort-{epoch}-{key}"));
                    div()
                        .child(element)
                        .with_animation(id, RESORT.animation(), move |el, t| {
                            el.relative().top(px(dy * (1.0 - t)))
                        })
                        .into_any_element()
                } else if self.sidebar_new_keys.contains(&key) {
                    let id = SharedString::from(format!("row-in-{epoch}-{key}"));
                    motion::fade_quick(id, div().child(element)).into_any_element()
                } else {
                    element
                }
            })
            .collect();

        // The space filter lives ABOVE the scroll region (fixed) so its
        // dropdown can float without being clipped by the list's overflow.
        let filter_row = self.render_spaces_filter(theme, cx);
        let active_list = if !list_items.is_empty() {
            let mut pinned_items = list_items;
            let mut custom_items = pinned_items.split_off(pinned_count);
            let regular_items = custom_items.split_off(custom_count);
            let regular_empty = regular_items.is_empty();
            let pinned_group = show_pinned_section
                .then(|| self.render_pinned_section(pinned_items, pinned_body_height, theme, cx));
            div()
                .id("sidebar-active-sessions")
                .flex()
                .flex_col()
                .gap(px(SIDEBAR_LIST_GAP))
                .pb(px(Theme::SPACE_SM))
                .when_some(pinned_group, |el, group| el.child(group))
                .children(custom_items)
                .when(
                    !regular_items.is_empty() || self.sidebar_session_transfer.is_some(),
                    |el| {
                        el.when(regular_gap, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .h(px(SIDEBAR_THREADS_GAP - 2.0 * SIDEBAR_LIST_GAP)),
                            )
                        })
                        .child(
                            div()
                                .id("sidebar-regular-sessions")
                                .debug_selector(|| "sidebar-regular-sessions".into())
                                .on_drag_move::<SidebarSessionDrag>(cx.listener(
                                    move |this,
                                          event: &gpui::DragMoveEvent<SidebarSessionDrag>,
                                          _,
                                          _| {
                                        if regular_empty
                                            && event.bounds.contains(&event.event.position)
                                        {
                                            let top = f32::from(
                                                event.bounds.top()
                                                    - this.sidebar_scroll.bounds().top()
                                                    - this.sidebar_scroll.offset().y,
                                            );
                                            if let Some(drag) =
                                                this.sidebar_session_transfer.as_mut()
                                            {
                                                drag.preview = Some(SidebarSessionGap {
                                                    group: "regular".into(),
                                                    index: 0,
                                                    pinned: false,
                                                    top,
                                                });
                                            }
                                            return;
                                        }
                                        if event.bounds.contains(&event.event.position)
                                            && let Some(drag) =
                                                this.sidebar_session_transfer.as_mut()
                                            && drag.preview.as_ref().is_some_and(|gap| gap.pinned)
                                        {
                                            drag.preview = None;
                                        }
                                    },
                                ))
                                .on_drop::<SidebarSessionDrag>(cx.listener(
                                    |this, payload, _, cx| {
                                        this.finish_sidebar_session_transfer(
                                            payload,
                                            SidebarSessionDrop::Regular,
                                            cx,
                                        );
                                    },
                                ))
                                .flex()
                                .flex_col()
                                .gap(px(SIDEBAR_LIST_GAP))
                                .when(regular_items.is_empty(), |el| {
                                    el.h(px(48.0 + self.sidebar_transfer_extra_gap("regular")))
                                        .justify_center()
                                        .px(px(10.0))
                                        .text_color(theme.text_muted)
                                        .text_size(crate::typography::text_xs())
                                        .child(tr(cx, msg::native::drop_here_to_unpin))
                                })
                                .children(regular_items),
                        )
                    },
                )
                .into_any_element()
        } else {
            div()
                .px(px(Theme::SPACE_SM))
                .pb(px(Theme::SPACE_SM))
                .text_size(crate::typography::text_xs())
                .text_color(theme.text_faint)
                .child(SharedString::from("No sessions yet"))
                .into_any_element()
        };

        // The (filtered) Sessions list scrolls inside an EdgeFade scope —
        // a true per-glyph gradient at active overflow edges. Glass-safe
        // (no painted overlay can fade content over see-through blur) and
        // equivalent on opaque themes: alpha→0 reveals the surface tone
        // underneath, same as the gradient overlays it replaced. Overflow
        // is read at PAINT time via the scroll handle — render-time gating
        // rode the previous frame's offset, so the last frame of a content
        // shrink (row archived while scrolled) left a phantom fade stuck
        // over an unscrollable list (user report).
        let sidebar_lists = crate::edge_fade::edge_faded(
            SIDEBAR_GLASS_FADE_BAND,
            true,
            true,
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(crate::overscroll::overscroll(
                    "sidebar-overscroll",
                    crate::overscroll::OverscrollSource::Handle(self.sidebar_scroll.clone()),
                    div()
                        .id("sidebar-lists")
                        .relative()
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.sidebar_scroll)
                        .on_drag_move::<SidebarSessionDrag>(cx.listener(
                            move |this, event: &gpui::DragMoveEvent<SidebarSessionDrag>, _, cx| {
                                if let Some(transfer) = this.sidebar_session_transfer.as_mut() {
                                    transfer.viewport = Some(event.bounds);
                                }
                                if !event.bounds.contains(&event.event.position) {
                                    if let Some(transfer) = this.sidebar_session_transfer.as_mut() {
                                        transfer.preview = None;
                                    }
                                    return;
                                }
                                let payload = event.drag(cx).clone();
                                this.track_pinned_session_drag_pointer(
                                    payload,
                                    f32::from(event.event.position.y),
                                    f32::from(event.bounds.top()),
                                    f32::from(event.bounds.bottom()),
                                    cx,
                                );
                            },
                        ))
                        // Empty space is not a transfer target.
                        .on_drop::<SidebarSessionDrag>(cx.listener(
                            |this, _: &SidebarSessionDrag, _, cx| {
                                this.cancel_sidebar_session_transfer(cx);
                            },
                        ))
                        .px(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        // No "Sessions" header (user request) — the list
                        // is the whole column; a little air stands in.
                        .pt(px(SIDEBAR_LIST_PAD_TOP))
                        .child(active_list)
                        .children(moving_row),
                )),
        )
        .fade_overflow_y(&self.sidebar_scroll);

        let github_star_banner = self.render_github_star_banner(theme, cx);

        div()
            .w(px(
                (self.sidebar_content_width() - self.workspace_gutter()).max(0.)
            ))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            .child(self.render_sidebar_nav(theme, cx))
            .child(self.render_zot_sidebar(theme, cx))
            .child(self.render_sidebar_modes(theme, cx))
            .children(self.render_cloud_sidebar_status(theme, cx))
            .child(filter_row)
            .child(sidebar_lists)
            // Global connection pill (durable-by-design UI truth): appears
            // whenever the edge posture is degraded; hidden while healthy —
            // appearing IS the signal.
            .when_some(self.render_connection_pill(theme, cx), |el, pill| {
                el.child(pill)
            })
            // "Star on GitHub" banner until dismissed (persisted), above the
            // footer, below the lists. App updates live in the footer itself.
            .when_some(github_star_banner, |el, banner| el.child(banner))
            .child(
                div()
                    .px(px(8.))
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(self.render_sidebar_footer_row(theme, cx)),
            )
            .into_any_element()
    }

    /// Compact New chat, New project and Plugins actions.
    fn render_sidebar_nav(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Workspace routes expose account controls in Remote access.
        let workspace =
            (self.workspace_gutter() == 0.).then(|| self.render_workspace_switcher(theme, cx));
        let new_chat_hint: SharedString =
            crate::settings::badge_combo(self.settings.keymap.get(ShortcutId::NewSession)).into();
        let plugins_open = matches!(self.route, Route::Plugins);
        let pull_requests_open = matches!(self.route, Route::PullRequests);
        let row = |id: &'static str, glyph: &'static str, label: &'static str, active: bool| {
            let fade = format!("{id}-hover");
            div()
                .id(id)
                .debug_selector(move || id.into())
                .role(gpui::Role::Button)
                .tab_index(0)
                .min_h(px(28.0))
                .py(px(2.0))
                .line_height(crate::typography::ui_rems(18.0))
                .px(px(Theme::SPACE_SM))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .rounded(px(SIDEBAR_ROW_RADIUS))
                .cursor_pointer()
                .text_size(crate::typography::text_sm())
                .bg(if active {
                    crate::theme::glass_selected_bg()
                } else {
                    motion::hover_blend(
                        &fade,
                        theme.glass_hover().opacity(0.0),
                        theme.glass_hover(),
                    )
                })
                .text_color(if active {
                    theme.text_strong
                } else {
                    motion::hover_blend(&fade, theme.text_muted, theme.text)
                })
                .on_hover(motion::hover_listener(fade.clone()))
                .focus_visible(|el| el.border_1().border_color(theme.border_strong))
                .child(
                    icon(glyph)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(if active {
                            theme.text
                        } else {
                            motion::hover_blend(&fade, theme.text_muted, theme.text)
                        }),
                )
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px(px(6.0))
            .pt(px(4.0))
            .pb(px(4.0))
            .when_some(workspace, |view, workspace| {
                view.child(div().px(px(2.0)).pb(px(6.0)).child(workspace))
            })
            .child(
                row(
                    "sidebar-new-chat-row",
                    icons::DITHER_CHAT,
                    "New thread",
                    false,
                )
                .aria_label("New thread")
                .on_click(cx.listener(|this, _, _, cx| this.open_new_session(None, cx)))
                .when(!new_chat_hint.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_none()
                            .font_family(crate::popover::KEY_GLYPH_FONT)
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(new_chat_hint),
                    )
                }),
            )
            .child(
                row(
                    "sidebar-new-project-row",
                    icons::DITHER_FOLDER,
                    "New project",
                    false,
                )
                .aria_label(tr(cx, msg::projects::projects_sidebar_new_project))
                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
            )
            .child(
                row(
                    "sidebar-plugins-row",
                    icons::DITHER_PLUG,
                    "Plugins",
                    plugins_open,
                )
                .aria_label(tr(cx, msg::plugins::plugins_back))
                .on_click(cx.listener(|this, _, _, cx| this.open_plugins_page(cx))),
            )
            // Pull requests span every chat, so the entry lives with the
            // other global destinations rather than in a chat's top bar.
            .child(
                row(
                    "sidebar-pull-requests-row",
                    icons::PULL_REQUEST,
                    "Pull requests",
                    pull_requests_open,
                )
                .aria_label(tr(cx, msg::native::pull_requests))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings_focus_pending = false;
                    this.open_pull_requests(cx);
                })),
            )
            .into_any_element()
    }

    /// "Star on GitHub" banner: a low-emphasis accent chip. Clicking it
    /// opens the repository; either that or the close button dismisses it for
    /// good — the choice is persisted so it never returns on the next launch.
    fn render_github_star_banner(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.settings.github_star_banner_dismissed {
            return None;
        }
        let tone = theme.accent;
        let chip_bg_hover = theme.accent.opacity(0.16);
        let close_bg_hover = theme.accent.opacity(0.22);
        Some(
            div()
                .id("github-star-banner")
                .mx(px(Theme::SPACE_SM))
                .pl(px(Theme::SPACE_SM))
                .pr(px(Theme::SPACE_XS))
                .py(px(4.0))
                .rounded(px(Theme::CONTROL_RADIUS))
                .bg(theme.accent_wash)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .text_size(crate::typography::text_xs())
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(tone)
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.open_url(GITHUB_REPO_URL);
                    this.dismiss_github_star_banner(cx);
                }))
                // Glyphs sit below the line box's optical center: nudge the
                // star half a pixel down so it centers on the cap height.
                .child(
                    icon(icons::STAR_BOLD)
                        .flex_none()
                        .relative()
                        .top(px(0.5))
                        .size(px(12.0))
                        .text_color(tone),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(tr(cx, msg::projects::projects_sidebar_star_on_github)),
                )
                .child(
                    div()
                        .id("github-star-banner-dismiss")
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(4.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(move |s| s.bg(close_bg_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.dismiss_github_star_banner(cx);
                        }))
                        .tip("Dismiss")
                        .child(icon(icons::CLOSE).size(px(10.0)).text_color(tone)),
                )
                .into_any_element(),
        )
    }

    fn dismiss_github_star_banner(&mut self, cx: &mut Context<Self>) {
        self.settings.github_star_banner_dismissed = true;
        self.schedule_save(cx);
        cx.notify();
    }

    /// App update pill: sits in the footer beside the running-agent count
    /// whenever this app's release checker ([`crate::app_update`]) has found a
    /// newer version. Self-updating installs (macOS bundles, Windows installs,
    /// Linux managed installs) go Update → Updating → Restart; installs that
    /// can't replace themselves explain why; advisory installs point at
    /// `zuse update` or the GitHub releases page and dismiss per version. The
    /// full status line lives in the tooltip so the footer stays one row.
    fn render_update_pill(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::app_update::{Flow, StripAction};
        let update = crate::app_update::AppUpdate::global(cx)?;
        if let Some(problem) = update.read(cx).control_problem() {
            return Some(
                div()
                    .id("control-update-retry")
                    .mx(px(8.0))
                    .p(px(8.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger)
                    .cursor_pointer()
                    .tip(problem)
                    .child("Control tools update failed · Retry")
                    .on_click(cx.listener(|_, _, _, cx| {
                        if let Some(update) = crate::app_update::AppUpdate::global(cx) {
                            update.update(cx, |update, cx| update.retry_control(cx));
                        }
                    }))
                    .into_any_element(),
            );
        }
        let (status, action) = update.read(cx).strip()?;
        let flow = update.read(cx).flow().clone();
        let notes = update
            .read(cx)
            .release_notes()
            .map(|(version, sections)| (version.to_owned(), sections.to_vec()));
        let working = self.working_agent_count(cx);

        enum Tone {
            Quiet,
            Accent,
            Solid,
            Danger,
        }
        let (glyph, label, tone, tip): (Option<&'static str>, &'static str, Tone, SharedString) =
            match (&flow, action) {
                (Flow::Installed, _) => (None, "Restarting", Tone::Quiet, status),
                (Flow::Downloading { .. }, _) => (
                    None,
                    "Updating",
                    Tone::Quiet,
                    format!("{status} You can keep working.").into(),
                ),
                (_, StripAction::Restart) => (
                    Some(icons::RESTART),
                    "Restart",
                    Tone::Solid,
                    match working {
                        0 => format!("{status} — restart to apply").into(),
                        1 => format!("{status} — 1 running agent resumes after restart").into(),
                        n => format!("{status} — {n} running agents resume after restart").into(),
                    },
                ),
                (Flow::Failed { .. }, _) => (Some(icons::RESTART), "Retry", Tone::Danger, status),
                (_, StripAction::None) => (None, "Updating", Tone::Quiet, status),
                _ => (Some(icons::ARROW_DOWN), "Update", Tone::Accent, status),
            };
        let (glyph, label, tone, tip, action) =
            match self.update_restart.as_ref().map(|plan| plan.stage()) {
                Some(update_restart::RestartStage::WaitingForAgents) => (
                    None,
                    "Waiting",
                    Tone::Quiet,
                    SharedString::from("Restarts when your agents finish. Click for options."),
                    StripAction::Restart,
                ),
                Some(update_restart::RestartStage::UpdatingAgents) => (
                    None,
                    "Updating",
                    Tone::Quiet,
                    SharedString::from("Updating agents, then restarting."),
                    StripAction::None,
                ),
                None => (glyph, label, tone, tip, action),
            };
        let (bg, bg_hover, fg) = match tone {
            Tone::Quiet => (theme.glass_hover(), theme.glass_hover(), theme.text_muted),
            Tone::Accent => (theme.accent_wash, theme.accent.opacity(0.18), theme.accent),
            Tone::Solid => (theme.text, theme.text.opacity(0.85), theme.on_solid),
            Tone::Danger => (
                theme.danger.opacity(0.14),
                theme.danger.opacity(0.22),
                theme.danger,
            ),
        };
        let interactive = action != StripAction::None;
        let focus_ring = theme.accent;
        let lead = match glyph {
            Some(glyph) => icon(glyph)
                .size(px(12.0))
                .flex_none()
                .text_color(fg)
                .into_any_element(),
            None => loaders::mini_glyph_spinner(
                "sidebar-update-progress",
                1.5,
                theme.loader_glyph(),
                cx.entity_id(),
                cx,
            )
            .into_any_element(),
        };
        Some(
            div()
                .id("sidebar-update")
                .debug_selector(|| "sidebar-update".into())
                .role(if interactive {
                    gpui::Role::Button
                } else {
                    gpui::Role::Status
                })
                .aria_label(tip.clone())
                .h(px(22.0))
                .px(px(8.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .rounded_full()
                .border_1()
                .border_color(gpui::transparent_black())
                .bg(bg)
                .text_size(crate::typography::text_xs())
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(fg)
                .map(|el| match notes {
                    // Hoverable so the pointer can reach the card's link. It
                    // sits at the window's bottom edge, so gpui flips it up.
                    Some((version, sections)) => el.hoverable_tooltip(move |_, cx| {
                        cx.new(|_| update_notes::UpdateNotesCard {
                            status: tip.clone(),
                            version: version.clone(),
                            sections: sections.clone(),
                        })
                        .into()
                    }),
                    None => el.tip_above(tip),
                })
                .when(interactive, |el| {
                    el.tab_index(0)
                        .cursor_pointer()
                        .focus_visible(move |el| el.border_color(focus_ring))
                        .hover(move |el| el.bg(bg_hover))
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.on_update_strip_click(action, cx)
                            }),
                        )
                })
                .child(lead)
                .child(label)
                .into_any_element(),
        )
    }

    /// Chats with a turn in flight — the same count the footer shows.
    fn working_agent_count(&self, cx: &App) -> usize {
        self.state
            .read(cx)
            .sidebar_chats(Utc::now(), None)
            .into_iter()
            .filter(|(indicator, _)| *indicator == zuse_protocol::ChatIndicator::Working)
            .count()
    }

    /// Download → the background stage; Restart → swap + relaunch; Explain →
    /// the update dialog; advisory installs open their destination (if any),
    /// then dismiss for this version.
    fn on_update_strip_click(
        &mut self,
        action: crate::app_update::StripAction,
        cx: &mut Context<Self>,
    ) {
        use crate::app_update::{AppUpdate, StripAction};
        let Some(update) = AppUpdate::global(cx) else {
            return;
        };
        match action {
            StripAction::None => {}
            StripAction::Download => update.update(cx, |update, cx| update.start_download(cx)),
            StripAction::Restart => self.request_update_restart(cx),
            StripAction::Explain => update.update(cx, |update, cx| update.show_result(cx)),
            StripAction::Advise { open_releases } => {
                if open_releases {
                    cx.open_url(zuse_update::RELEASES_PAGE);
                }
                update.update(cx, |update, cx| update.dismiss_advisory(cx));
            }
        }
    }

    /// Swap the staged update over the installed one, arm the detached
    /// relauncher, and quit — the relauncher starts the new version once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        if !self.prepare_exit(PendingExit::InstallUpdate(staged.clone()), cx) {
            return;
        }
        let Some(update) = crate::app_update::AppUpdate::global(cx) else {
            return;
        };
        let engine = self.state.read(cx).engine().cloned();
        if let Some(engine) = &engine
            && let Err(error) = engine.prepare_update_restart()
        {
            self.show_notice(
                crate::toast::ToastKind::Error,
                format!("Could not save running agents: {error}"),
                cx,
            );
            return;
        }
        if update
            .update(cx, |update, cx| update.install_for_restart(&staged, cx))
            .is_ok()
        {
            crate::app_menus::quit_after_save(cx);
        } else if let Some(engine) = engine {
            engine.cancel_update_restart();
        }
    }

    /// The "Check for Updates…" dialog: checking, then the outcome, which
    /// stays live (download progress, ready to restart) while it is open.
    fn render_update_prompt(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use crate::app_update::{AppUpdate, Flow, Prompt};
        let update = AppUpdate::global(cx)?;
        update.read(cx).prompt()?;
        let theme = Theme::of(cx).for_popup();
        let current = zuse_update::current_version();
        let working = self.restart_blockers(cx);
        let waiting_for_agents = self
            .update_restart
            .as_ref()
            .is_some_and(|plan| plan.stage() == update_restart::RestartStage::WaitingForAgents);
        let agent_updates = match self.agent_update_count() {
            0 => String::new(),
            1 => " 1 agent update is included.".to_owned(),
            n => format!(" {n} agent updates are included."),
        };
        let (title, body, buttons): (SharedString, SharedString, Vec<UpdatePromptButton>) = {
            let update = update.read(cx);
            match update.prompt()? {
                Prompt::Checking => (
                    "Checking for updates…".into(),
                    format!("You're on Zuse {current}.").into(),
                    vec![UpdatePromptButton::Close("Cancel")],
                ),
                Prompt::CheckFailed(message) => (
                    "Couldn't check for updates".into(),
                    message.clone(),
                    vec![
                        UpdatePromptButton::Close("Close"),
                        UpdatePromptButton::CheckAgain,
                    ],
                ),
                Prompt::Result => match update.available() {
                    None => (
                        "Zuse is up to date".into(),
                        format!("Version {current} is the newest release.").into(),
                        vec![UpdatePromptButton::Close("OK")],
                    ),
                    Some(latest) => {
                        let title: SharedString = format!("Zuse {latest} is available").into();
                        if let Some(blocker) = update.blocker() {
                            (
                                title,
                                blocker.to_string().into(),
                                vec![
                                    UpdatePromptButton::Close("Later"),
                                    UpdatePromptButton::OpenDownloads,
                                ],
                            )
                        } else if update.install().supports_desktop_update() {
                            match update.flow() {
                                Flow::Idle => (
                                    title,
                                    format!("You're on {current}.").into(),
                                    vec![
                                        UpdatePromptButton::Close("Later"),
                                        UpdatePromptButton::Download,
                                    ],
                                ),
                                Flow::Downloading { .. } => (
                                    title,
                                    "Downloading in the background — keep working. You'll get a Restart button when it's ready."
                                        .into(),
                                    vec![UpdatePromptButton::Close("Hide")],
                                ),
                                Flow::Installed => (
                                    "Restarting Zuse…".into(),
                                    "Your update is installed. Zuse will reopen shortly.".into(),
                                    vec![],
                                ),
                                Flow::Ready { version, .. } if waiting_for_agents => (
                                    "Waiting for agents to finish".into(),
                                    format!("Zuse restarts to {version} as soon as they're done. Keep working in the meantime.").into(),
                                    vec![
                                        UpdatePromptButton::CancelRestart,
                                        UpdatePromptButton::Close("Keep waiting"),
                                        UpdatePromptButton::Restart,
                                    ],
                                ),
                                Flow::Ready { version, .. } if working > 0 => (
                                    format!("Zuse {version} is ready").into(),
                                    format!(
                                        "{}. Restart when {} done, or restart now and {} pick up where {} left off. Background tasks stop either way.{agent_updates}",
                                        if working == 1 { "1 agent is active".to_owned() } else { format!("{working} agents are active") },
                                        if working == 1 { "it's" } else { "they're" },
                                        if working == 1 { "it will" } else { "they'll" },
                                        if working == 1 { "it" } else { "they" },
                                    )
                                    .into(),
                                    vec![
                                        UpdatePromptButton::Close("Later"),
                                        UpdatePromptButton::RestartAnyway,
                                        UpdatePromptButton::RestartWhenIdle,
                                    ],
                                ),
                                Flow::Ready { version, .. } => (
                                    format!("Zuse {version} is ready").into(),
                                    format!("Restart to finish updating — it only takes a moment.{agent_updates}").into(),
                                    vec![
                                        UpdatePromptButton::Close("Later"),
                                        UpdatePromptButton::Restart,
                                    ],
                                ),
                                Flow::Failed { message, .. } => (
                                    title,
                                    format!("The download failed: {message}").into(),
                                    vec![
                                        UpdatePromptButton::Close("Later"),
                                        UpdatePromptButton::Download,
                                    ],
                                ),
                            }
                        } else if matches!(
                            update.install(),
                            zuse_update::InstallKind::Managed { .. }
                        ) {
                            (
                                title,
                                "Run `zuse update` in a terminal to install it.".into(),
                                vec![UpdatePromptButton::Close("OK")],
                            )
                        } else {
                            (
                                title,
                                "This copy of Zuse wasn't set up by an installer (for example, a source build), so it can't update itself."
                                    .into(),
                                vec![
                                    UpdatePromptButton::Close("Later"),
                                    UpdatePromptButton::OpenDownloads,
                                ],
                            )
                        }
                    }
                },
            }
        };
        let mut row = popover::dialog_footer(&theme);
        for button in buttons {
            row = row.child(match button {
                UpdatePromptButton::Close(label) => {
                    popover::btn_ghost(&theme, label, "update-prompt-close")
                        .id("update-prompt-close")
                        .on_click(cx.listener(|_, _, _, cx| {
                            if let Some(update) = crate::app_update::AppUpdate::global(cx) {
                                update.update(cx, |update, cx| update.dismiss_prompt(cx));
                            }
                        }))
                }
                UpdatePromptButton::CheckAgain => popover::btn_primary(&theme, "Try again")
                    .id("update-prompt-check")
                    .on_click(cx.listener(|_, _, _, cx| {
                        if let Some(update) = crate::app_update::AppUpdate::global(cx) {
                            update.update(cx, |update, cx| update.check_for_updates(cx));
                        }
                    })),
                UpdatePromptButton::Download => popover::btn_primary(&theme, "Update")
                    .id("update-prompt-download")
                    .on_click(cx.listener(|_, _, _, cx| {
                        if let Some(update) = crate::app_update::AppUpdate::global(cx) {
                            update.update(cx, |update, cx| update.start_download(cx));
                        }
                    })),
                UpdatePromptButton::Restart => popover::btn_primary(&theme, "Restart now")
                    .id("update-prompt-restart")
                    .on_click(cx.listener(|this, _, _, cx| {
                        dismiss_update_prompt(cx);
                        this.begin_update_restart(false, cx);
                    })),
                UpdatePromptButton::RestartAnyway => {
                    popover::btn_ghost(&theme, "Restart now", "update-prompt-restart")
                        .id("update-prompt-restart")
                        .on_click(cx.listener(|this, _, _, cx| {
                            dismiss_update_prompt(cx);
                            this.begin_update_restart(false, cx);
                        }))
                }
                UpdatePromptButton::RestartWhenIdle => {
                    popover::btn_primary(&theme, "When agents finish")
                        .id("update-prompt-when-idle")
                        .on_click(cx.listener(|this, _, _, cx| {
                            dismiss_update_prompt(cx);
                            this.begin_update_restart(true, cx);
                        }))
                }
                UpdatePromptButton::CancelRestart => {
                    popover::btn_ghost(&theme, "Don't restart", "update-prompt-cancel")
                        .id("update-prompt-cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            dismiss_update_prompt(cx);
                            this.cancel_update_restart_plan(cx);
                        }))
                }
                UpdatePromptButton::OpenDownloads => {
                    popover::btn_primary(&theme, "Open download page")
                        .id("update-prompt-downloads")
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.open_url(zuse_update::LATEST_RELEASE_PAGE);
                            if let Some(update) = crate::app_update::AppUpdate::global(cx) {
                                update.update(cx, |update, cx| update.dismiss_prompt(cx));
                            }
                        }))
                }
            });
        }
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, &title))
            .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, body)))
            .child(row)
            .into_any_element();
        Some(popover::modal("update-prompt-dialog", viewport, card))
    }

    /// The sidebar's bottom row: account menu on the left, settings toggle on
    /// the right. Shared by the chat sidebar and the settings page so both
    /// sit in one position. Local runtimes advertise their storage boundary
    /// and offer sync; synced runtimes offer sign-out.
    /// Account, settings, and the Pull requests entry. Chat and Settings both
    /// render this exact row so the footer holds still across the swap.
    fn render_sidebar_footer_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-footer-row")
            .debug_selector(|| "sidebar-footer-row".into())
            .w_full()
            .flex()
            .items_center()
            .gap(px(SIDEBAR_FOOTER_ACTION_GAP))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_sidebar_footer(theme, cx)),
            )
            .into_any_element()
    }

    /// General-account plan, provider limits and the running-agent count,
    /// then the app update pill (beside that count, so "restart" reads
    /// against how many agents it would pause) and Settings.
    fn render_sidebar_footer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.update_sidebar_controls(cx);
        let update = self.render_update_pill(theme, cx);
        let settings = self.render_settings_trigger(theme, cx);
        div()
            .w_full()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.sidebar_controls.clone()),
            )
            .when_some(update, |el, update| el.child(update))
            .child(settings)
            .into_any_element()
    }

    /// The workspace switcher at the top of the sidebar: a colored square
    /// with the workspace's initial and its name. It opens the account menu
    /// (sync, sign out, updates) below it.
    fn render_workspace_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (user_line, menu_identity) = {
            let state = self.state.read(cx);
            let (line, identity) =
                sidebar_account_identity(state.workspace_scope, self.sync_flow, state.auth_user());
            // A local workspace is the user's own: "Personal" names it, while
            // the footer says where it is stored.
            let line = if state.auth_user().is_some()
                && matches!(state.workspace_scope, Some(WorkspaceScope::Synced) | None)
            {
                line
            } else {
                SharedString::from("Personal")
            };
            (line, identity)
        };
        self.render_user_menu(user_line, menu_identity, theme, cx)
    }

    fn render_user_menu(
        &mut self,
        user_line: SharedString,
        menu_identity: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = &theme.for_popup();
        let open = self.user_menu.is_open();
        let action = account_menu_action(self.state.read(cx).workspace_scope, self.sync_flow);
        // The profile pill hugs avatar + name (shrinking so long names fade
        // out); the gap between it and the settings button is not interactive.
        let initial: SharedString = user_line
            .trim()
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let mut trigger = div()
            .id("user-menu")
            .debug_selector(|| "user-menu".into())
            .role(gpui::Role::Button)
            .aria_label(format!("Account menu: {user_line}"))
            .aria_expanded(open)
            .tab_index(0)
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .relative()
            .h(px(SIDEBAR_FOOTER_BUTTON_SIZE))
            .min_w_0()
            .w_full()
            .rounded(px(7.0))
            .px(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .text_size(crate::typography::text_sm())
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(if open {
                theme.text
            } else {
                motion::hover_blend("user-menu-trigger", theme.text, theme.text_strong)
            })
            .cursor_pointer()
            // user-menu.tsx trigger: hover `bg-white/[0.04]`, open state
            // (`data-[state=open]`) the slightly stronger `bg-white/[0.06]`;
            // the hover wash fades over `transition-colors`.
            .bg(if open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "user-menu-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover().opacity(0.8),
                )
            })
            .on_hover(motion::hover_listener("user-menu-trigger"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.user_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                // A press that found the menu open closes it (the card's
                // mouse-down-out already began the close) — never reopen.
                if this.user_menu.take_press_was_open() {
                    this.close_user_menu(cx);
                } else {
                    this.user_menu.open(());
                }
                cx.notify();
            }))
            .child({
                // Avatar: a colored square with the workspace's initial.
                let mut tone = project_icon::monogram_tone(&user_line, theme);
                tone.l = tone.l.min(0.5);
                div()
                    .size(px(SIDEBAR_FOOTER_AVATAR_SIZE))
                    .flex_none()
                    .rounded(px(4.0))
                    .bg(tone)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .line_height(px(SIDEBAR_FOOTER_AVATAR_SIZE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(gpui::white())
                    .child(div().w_full().text_center().child(initial))
            })
            .child(sidebar_faded_label(
                "user-menu-label".into(),
                false,
                div().line_height(px(17.0)).child(user_line),
            ));
        if self.user_menu.get().is_some() {
            let closing = self.user_menu.closing_since();

            let menu = popover::popover_card(theme)
                .w(px(self.sidebar_content_width() - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_user_menu(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .text_size(crate::typography::text_xs())
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(menu_identity),
                )
                .when_some(action, |menu, action| {
                    let row = match action {
                        AccountMenuAction::EnableSync => {
                            popover::menu_row(theme, false, "user-menu-enable-sync")
                                .id("user-menu-enable-sync")
                                .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                                .child(
                                    icon(icons::GLOBAL)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Enable sync"))
                                .into_any_element()
                        }
                        AccountMenuAction::SyncInProgress => {
                            popover::menu_row(theme, false, "user-menu-sync-progress")
                                .id("user-menu-sync-progress")
                                .opacity(0.6)
                                .child(
                                    icon(icons::GLOBAL)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Sync setup in progress"))
                                .into_any_element()
                        }
                        AccountMenuAction::RestartPending => {
                            popover::menu_row(theme, false, "user-menu-sync-restart")
                                .id("user-menu-sync-restart")
                                .on_click(cx.listener(|this, _, _, cx| this.reopen_sync_notice(cx)))
                                .child(
                                    icon(icons::RESTART)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Finish sync setup"))
                                .into_any_element()
                        }
                        AccountMenuAction::SignOut => {
                            popover::menu_row(theme, false, "user-menu-signout")
                                .id("user-menu-signout")
                                .on_click(cx.listener(|this, _, _, cx| this.request_sign_out(cx)))
                                .child(
                                    icon(icons::LOGOUT_2)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Sign out"))
                                .into_any_element()
                        }
                    };
                    menu.child(row)
                })
                // macOS keeps "Check for Updates…" in the app menu under
                // About; elsewhere there is no app menu, so it lives here.
                .when(!cfg!(target_os = "macos"), |menu| {
                    menu.child(
                        popover::menu_row(theme, false, "user-menu-check-updates")
                            .id("user-menu-check-updates")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_user_menu(cx);
                                crate::app_update::check_for_updates(cx);
                            }))
                            .child(
                                icon(icons::REFRESH)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Check for updates")),
                    )
                })
                .into_any_element();
            // Opens downward, as wide as the sidebar's content column.
            trigger = trigger.child(popover::anchored_menu_below(
                "user-menu-popover",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    /// Settings is a toggle: while its page is up the button keeps the
    /// hover wash + full-strength glyph, and a click returns to chat.
    fn render_settings_trigger(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let settings_open = matches!(self.route, Route::Settings(_));
        let settings_tooltip: SharedString = match (settings_open, cfg!(target_os = "macos")) {
            (false, true) => "Settings · ⌘,".into(),
            (false, false) => "Settings · Ctrl+,".into(),
            (true, true) => "Close settings · ⌘,".into(),
            (true, false) => "Close settings · Ctrl+,".into(),
        };
        div()
            .id("settings-trigger")
            .debug_selector(|| "settings-trigger".into())
            .role(gpui::Role::Button)
            .aria_label(if settings_open {
                ("Close settings").to_string()
            } else {
                crate::localization::text("common-settings", cx)
            })
            .aria_toggled(if settings_open {
                gpui::Toggled::True
            } else {
                gpui::Toggled::False
            })
            .anchored_tooltip(crate::tooltip::Placement::Below, move |_, cx| {
                let text = settings_tooltip.clone();
                cx.new(|_| SurfaceTabTooltip { text }).into()
            })
            .tab_index(0)
            .size(px(SIDEBAR_FOOTER_BUTTON_SIZE))
            .flex_none()
            .rounded(px(8.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(if settings_open {
                theme.glass_hover()
            } else {
                motion::hover_blend(
                    "settings-trigger",
                    theme.glass_hover().opacity(0.0),
                    theme.glass_hover(),
                )
            })
            .on_hover(motion::hover_listener("settings-trigger"))
            .focus_visible(|s| s.border_2().border_color(theme.accent))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx)))
            .child(
                icon(icons::DITHER_GEAR)
                    .size(px(14.0))
                    .text_color(if settings_open {
                        theme.text
                    } else {
                        motion::hover_blend("settings-trigger", theme.text_muted, theme.text)
                    }),
            )
            .into_any_element()
    }

    fn render_sync_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let needs_org = matches!(
            self.state.read(cx).auth.as_ref(),
            Some(AuthState::NeedsOrganization { .. })
        );
        let remote_engine = self
            .state
            .read(cx)
            .engine()
            .is_some_and(|engine| matches!(engine.mode(), EngineMode::Remote { .. }));
        let runtime_change_label = if self.runtime_change_task.is_some() {
            "Stopping engine…"
        } else if remote_engine {
            "Stop daemon and quit"
        } else {
            "Quit Zuse"
        };

        if self.sync_flow == SyncFlow::Enabling && needs_org {
            return Some(self.render_org_gate(cx));
        }

        let signed_in_email: Option<SharedString> = match self.state.read(cx).auth.as_ref() {
            Some(AuthState::SignedIn { user, .. }) => Some(SharedString::from(user.email.clone())),
            _ => None,
        };
        // Spaces count as local work too: a projects-only profile must get
        // the import choice, not a bare "Switch now".
        let (local_chats, local_spaces) = {
            let state = self.state.read(cx);
            (state.chats.len(), state.spaces.len())
        };
        let work_phrase = local_work_phrase(local_chats, local_spaces);

        let card = match self.sync_flow {
            SyncFlow::Enabling => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Enable sync"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Finish signing in in your browser. Zuse will keep using this local workspace until you quit and reopen.",
                    )),
                )
                .child(
                    popover::dialog_footer(&theme)
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::common::cancel), "sync-enable-cancel")
                                .id("sync-enable-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_auth_setup(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Open browser again")
                                .id("sync-enable-open-browser")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_sign_in(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::Canceling => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Canceling sync setup…"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Removing the partial sign-in before returning to your local workspace.",
                    )),
                )
                .into_any_element(),
            // ── in-place switch wizard ────────────────────────────────────
            SyncFlow::SwitchOffer { notice_open: true } => {
                let has_local_work = work_phrase.is_some();
                let body: SharedString = match (&signed_in_email, &work_phrase) {
                    (Some(email), Some(phrase)) => format!(
                        "You're signed in as {email}. Bring {phrase} from this device into your synced workspace, or start it fresh."
                    )
                    .into(),
                    (Some(email), None) => format!(
                        "You're signed in as {email}. Zuse can switch to your synced workspace now."
                    )
                    .into(),
                    (None, Some(phrase)) => format!(
                        "Bring {phrase} from this device into your synced workspace, or start it fresh."
                    )
                    .into(),
                    (None, None) => "Zuse can switch to your synced workspace now.".into(),
                };
                let mut actions = popover::dialog_footer(&theme)
                    .child(
                        popover::btn_ghost(&theme, &tr(cx, msg::native::later), "sync-switch-later")
                            .id("sync-switch-later")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.postpone_sync_restart(cx)
                            })),
                    );
                if has_local_work {
                    actions = actions
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::native::start_fresh), "sync-switch-fresh")
                                .id("sync-switch-fresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_synced_switch(false, cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Bring my work")
                                .id("sync-switch-import")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.start_synced_switch(true, cx)
                                })),
                        );
                } else {
                    actions = actions.child(
                        popover::btn_primary(&theme, "Switch now")
                            .id("sync-switch-now")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_synced_switch(false, cx)
                            })),
                    );
                }
                popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "Sync is ready"))
                    .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, body)))
                    .child(actions)
                    .into_any_element()
            }
            SyncFlow::Switching { import } => popover::dialog_card(&theme)
                .child(popover::dialog_title(
                    &theme,
                    "Switching to your synced workspace…",
                ))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    if import {
                        "Handing the engine over to your account. Your local sessions come along next."
                    } else {
                        "Handing the engine over to your account."
                    },
                )))
                .into_any_element(),
            SyncFlow::Importing { done, total } => {
                let fraction = if total == 0 {
                    0.0
                } else {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                };
                let label: SharedString = if total == 0 {
                    "Looking for local sessions…".into()
                } else {
                    format!("Importing session {} of {total}", (done + 1).min(total)).into()
                };
                let mut card = popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "Bringing your work over"))
                    .child(
                        div()
                            .mt(px(6.0))
                            .child(popover::dialog_body(&theme, label)),
                    );
                if let Some(current) = self.import_current.clone() {
                    card = card.child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::text_xs())
                            .line_height(px(17.0))
                            .text_color(theme.text_muted)
                            .overflow_hidden()
                            .child(current),
                    );
                }
                card.child(
                    // Determinate progress: a hairline track with an accent fill.
                    div()
                        .mt(px(14.0))
                        .h(px(4.0))
                        .w_full()
                        .rounded(px(2.0))
                        .bg(theme.border)
                        .child(
                            div()
                                .h_full()
                                .rounded(px(2.0))
                                .bg(theme.accent_strong)
                                .w(gpui::relative(fraction.max(0.04))),
                        ),
                )
                .into_any_element()
            }
            SyncFlow::ImportDone { imported, skipped } => {
                let body: SharedString = match (imported, skipped) {
                    (0, 0) => "Your synced workspace is ready.".into(),
                    (n, 0) => format!(
                        "{n} session{} moved into your synced workspace.",
                        if n == 1 { "" } else { "s" },
                    )
                    .into(),
                    (n, s) => format!(
                        "{n} session{} imported, {s} already present.",
                        if n == 1 { "" } else { "s" },
                    )
                    .into(),
                };
                popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "You're all set"))
                    .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, body)))
                    .child(
                        popover::dialog_footer(&theme)
                            .child(
                                popover::btn_primary(&theme, "Continue")
                                    .id("sync-switch-done")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sync_flow = SyncFlow::Idle;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            SyncFlow::ImportFailed { notice_open: true } => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Import didn't finish"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    "Anything already imported is kept; retrying only copies what's missing.",
                )))
                .when_some(self.runtime_change_error.clone(), |card, error| {
                    card.child(
                        div()
                            .mt(px(10.0))
                            .text_size(crate::typography::text_xs())
                            .line_height(px(17.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    popover::dialog_footer(&theme)
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::native::later), "import-failed-dismiss")
                                .id("import-failed-dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.postpone_sync_restart(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Retry import")
                                .id("import-failed-retry")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.spawn_local_import(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::RestartPending { notice_open: true } => popover::dialog_card(&theme)
                .child(popover::dialog_title(
                    &theme,
                    "Sync needs a restart",
                ))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        if remote_engine {
                            "Zuse is using a background daemon. Stop it and quit Zuse, then reopen to start the synced workspace. Existing local sessions stay on this device and will not be uploaded."
                        } else {
                            "Quit and reopen Zuse to start the synced workspace. Existing local sessions stay on this device and will not be uploaded."
                        },
                    )),
                )
                .when_some(self.runtime_change_error.clone(), |card, error| {
                    card.child(
                        div()
                            .mt(px(10.0))
                            .text_size(crate::typography::text_xs())
                            .line_height(px(17.0))
                            .text_color(theme.danger)
                            .child(error),
                    )
                })
                .child(
                    popover::dialog_footer(&theme)
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::native::later), "sync-restart-later")
                                .id("sync-restart-later")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.postpone_sync_restart(cx)
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, runtime_change_label)
                                .id("sync-restart-quit")
                                .when(self.runtime_change_task.is_some(), |button| {
                                    button.opacity(0.6)
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.quit_for_runtime_change(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::SignOutConfirm => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Sign out?"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Zuse will remove your credentials, close the synced workspace, and continue in local mode.",
                    )),
                )
                .child(
                    popover::dialog_footer(&theme)
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::common::cancel), "signout-cancel")
                                .id("signout-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sync_flow = SyncFlow::Idle;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Sign out")
                                .id("signout-confirm")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_sign_out(cx)
                                })),
                        ),
                )
                .into_any_element(),
            SyncFlow::SigningOut => popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Signing out…"))
                .child(
                    div().mt(px(6.0)).child(popover::dialog_body(
                        &theme,
                        "Removing account credentials and closing the synced workspace.",
                    )),
                )
                .into_any_element(),
            SyncFlow::Idle
            | SyncFlow::SwitchOffer { notice_open: false }
            | SyncFlow::ImportFailed { notice_open: false }
            | SyncFlow::RestartPending { notice_open: false }
            | SyncFlow::SignedOutRestartRequired => return None,
        };

        Some(popover::modal("sync-lifecycle-dialog", viewport, card))
    }

    fn active_changes(&self, cx: &App) -> Option<Entity<Changes>> {
        if !self.right_pane_shown(cx) {
            return None;
        }
        let RightSurface::Diff(id) = self.resolved_right_active(cx) else {
            return None;
        };
        self.diffs.get(&id).cloned()
    }

    /// Resolve shell-owned Escape surfaces in capture phase, before focused
    /// descendants such as an integrated terminal can consume the key.
    fn capture_escape_surface(&mut self, cx: &mut Context<Self>) -> bool {
        // Modals and context menus sit above the rest of the shell. Preserve
        // their existing behavior: only surfaces that already have a Cancel
        // path close here; the others remain explicit blockers.
        if self.delete_confirm.take().is_some() {
            cx.notify();
            return true;
        }
        if self.sync_flow.has_visible_overlay()
            || self.delete_space_confirm.is_some()
            || self.chat_menu.get().is_some()
            || self.space_menu.get().is_some()
            || self.user_menu.get().is_some()
        {
            return true;
        }
        if self.zot_ui.dialog.take().is_some() {
            cx.notify();
            return true;
        }
        if self.dismiss_zot_overlay(cx) {
            return true;
        }
        if self.harness_update_expanded {
            self.set_harness_updates_expanded(false, cx);
            return true;
        }
        if self.chat_rename.is_some() {
            self.finish_rename_chat(false, cx);
            return true;
        }
        if self.rename_space_dialog.is_some() {
            self.rename_space_dialog = None;
            cx.notify();
            return true;
        }
        if self.discard_working_tree.is_some() {
            self.discard_working_tree = None;
            cx.notify();
            return true;
        }
        if let Some(update) = crate::app_update::AppUpdate::global(cx)
            && update.read(cx).prompt().is_some()
        {
            update.update(cx, |update, cx| update.dismiss_prompt(cx));
            return true;
        }
        // The folded-breadcrumbs menu floats over the palette; it closes first.
        if self.add_space.is_some() && self.project_crumb_menu.is_open() {
            self.close_project_crumb_menu(cx);
            return true;
        }
        if self.add_space.is_some() {
            self.close_add_space(cx);
            return true;
        }
        if self.spaces_menu.is_open() {
            self.close_spaces_menu(cx);
            return true;
        }
        if self.spaces_menu.get().is_some() {
            return true;
        }

        if self.right_plus.is_open() {
            self.close_right_plus(cx);
            return true;
        }
        if self.open_in.menu.is_open() {
            self.close_open_in(cx);
            return true;
        }
        if self.branch_menu.menu.is_open() {
            self.close_branch_menu(cx);
            return true;
        }
        if self.right_plus.get().is_some() {
            return true;
        }
        if self.conversation_history.is_open() {
            self.close_conversation_history(cx);
            return true;
        }
        if matches!(self.route, Route::Zot) || self.zot_editor_open(cx) {
            self.leave_zot_page(cx);
            return true;
        }
        self.active_changes(cx)
            .is_some_and(|changes| changes.update(cx, |changes, cx| changes.handle_escape(cx)))
    }

    fn on_key_down_capture(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape"
            && self
                .sidebar_controls
                .update(cx, |controls, cx| controls.dismiss_on_escape(window, cx))
        {
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.close_shortcuts_panel(cx) {
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.transcript.read(cx).find_open() {
            self.transcript
                .update(cx, |transcript, cx| transcript.close_find(cx));
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.voice.read(cx).stage_open {
            self.set_voice_stage_open(false, cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.sidebar_session_transfer.is_some() {
            cx.stop_active_drag(window);
            self.cancel_sidebar_session_transfer(cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.command_palette.is_some() {
            self.close_command_palette(window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some((project, _)) = self.space_menu.as_open().cloned() {
            match event.keystroke.key.as_str() {
                "escape" => self.close_space_menu(cx),
                "up" => self.space_menu_active = (self.space_menu_active + 3) % 4,
                "down" => self.space_menu_active = (self.space_menu_active + 1) % 4,
                "home" => self.space_menu_active = 0,
                "end" => self.space_menu_active = 3,
                "enter" | "space" => match self.space_menu_active {
                    0 => self.open_rename_space(project, cx),
                    1 => self.open_project_archives(project, window, cx),
                    2 => self.open_usage(Some(project), true, cx),
                    _ => {
                        self.close_space_menu(cx);
                        self.delete_space_confirm = Some(project);
                    }
                },
                _ => return,
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && matches!(self.route, Route::Usage) {
            if let Some(dashboard) = self.usage_dashboard.clone() {
                dashboard.update(cx, |dashboard, cx| dashboard.escape(window, cx));
            }
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && matches!(self.route, Route::Archives) {
            if let Some(page) = self
                .archive_project
                .as_ref()
                .and_then(|id| self.project_archives.get(id))
                .cloned()
            {
                page.update(cx, |page, cx| page.dismiss_on_escape(cx));
            }
            cx.stop_propagation();
            return;
        }
        if matches!(self.route, Route::Settings(_)) {
            return;
        }
        if self.open_in_key(event, cx) {
            cx.stop_propagation();
            return;
        }
        if event.keystroke.key == "escape" && self.capture_escape_surface(cx) {
            cx.stop_propagation();
        }
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inputs and completion menus consume Tab first. Unhandled Tab walks
        // accessible controls, including individual transcript link ranges.
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "tab"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            if modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
            return;
        }
        // Escape targets the conversation that owns the focused composer: a
        // side chat's when its composer has focus, the main one otherwise.
        let (state, composer) = self
            .side_chats
            .values()
            .find(|tab| {
                tab.composer
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
            .map(|tab| (tab.state.clone(), tab.composer.clone()))
            .unwrap_or_else(|| (self.state.clone(), self.composer.clone()));
        let selected_chat = state.read(cx).selected_chat.clone();
        let indicator = selected_chat
            .as_deref()
            .map(|chat_id| state.read(cx).indicator_for(chat_id, Utc::now()))
            .unwrap_or(Indicator::None);
        let interrupting = selected_chat
            .as_deref()
            .is_some_and(|chat_id| composer.read(cx).is_interrupting(chat_id));
        let escape_stops_active_agent = self.settings.escape_stops_active_agent;

        match resolve_shell_escape(
            &event.keystroke.key,
            false,
            escape_stops_active_agent,
            self.route,
            selected_chat.as_deref(),
            indicator,
            interrupting,
        ) {
            ShellEscapeOutcome::Blocked => cx.stop_propagation(),
            ShellEscapeOutcome::InterruptChat(chat_id) => {
                cx.stop_propagation();
                composer.update(cx, |composer, cx| composer.interrupt_chat(chat_id, cx));
            }
            ShellEscapeOutcome::OtherKey | ShellEscapeOutcome::Ignored => {}
        }
    }

    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).for_popup();
        let mut overlays: Vec<AnyElement> = Vec::new();
        overlays.push(self.desktop_import.clone().into_any_element());
        overlays.extend(self.render_shortcuts_panel(viewport, cx));
        overlays.extend(self.render_zot_dialog(viewport, window, cx));
        overlays.extend(self.render_zot_overlays(viewport, cx));

        if let Some(menu_state) = self.chat_menu.get().cloned() {
            let chat_id = menu_state.chat_id;
            let position = menu_state.position;
            let chat_menu_closing = self.chat_menu.closing_since();
            let is_side_chat = matches!(menu_state.tab, Some((_, RightSurface::SideChat(_))))
                || self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .any(|chat| chat.id == chat_id && chat.parent_chat_id.is_some());
            let is_pinned = self.active_sidebar_pins(cx).contains(&chat_id);
            // A Zapper thread lives under its Zapper, so it has no pin, and
            // archiving goes through the Zapper so it stops sending work.
            let thread_zot = self.zot_of_thread(&chat_id, cx);
            let rename_id = chat_id.clone();
            let pin_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(216.0))
                .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    // The Copy submenu floats outside this card: a press on
                    // it is inside the menu.
                    if this
                        .chat_copy_bounds
                        .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    this.close_chat_menu(cx);
                }))
                .flex()
                .flex_col();
            let copy_open = menu_state.page == ChatMenuPage::Copy;
            let menu = if chat_id.is_empty() {
                menu
            } else {
                let rename =
                    popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename"));
                let menu = menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_RENAME, rename, cx));
                let menu = if is_side_chat {
                    menu
                } else {
                    let pin = popover::menu_row(&theme, false, format!("chat-menu-pin-{chat_id}"))
                        .id("chat-menu-pin")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_chat_pinned(pin_id.clone(), !is_pinned, cx)
                        }))
                        .child(icon(icons::PIN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from(if is_pinned { "Unpin" } else { "Pin" }));
                    let archive =
                        popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                            .id("chat-menu-archive")
                            .on_click({
                                let thread_zot = thread_zot.clone();
                                cx.listener(move |this, _, _, cx| match &thread_zot {
                                    Some(zot) => {
                                        this.close_chat_menu(cx);
                                        this.archive_zot_thread(zot, archive_id.clone(), cx);
                                    }
                                    None => this.archive_chat(archive_id.clone(), cx),
                                })
                            })
                            .child(
                                icon(icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Archive"));
                    let copy = self.render_chat_copy_trigger(&chat_id, copy_open, &theme, cx);
                    let menu = if thread_zot.is_some() {
                        menu
                    } else {
                        menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_PIN, pin, cx))
                    };
                    menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_ARCHIVE, archive, cx))
                        .child(self.chat_menu_hover_row(CHAT_MENU_ROW_COPY, copy, cx))
                };
                let delete =
                    popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_chat_menu(cx);
                            this.delete_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Force delete…"));
                menu.child(popover::menu_separator())
                    .child(self.chat_menu_hover_row(CHAT_MENU_ROW_DELETE, delete, cx))
            };
            let mut menu = menu;
            if let Some((key, surface)) = menu_state.tab {
                if !chat_id.is_empty() {
                    menu = menu.child(popover::menu_separator());
                }
                for (ix, (action, label)) in [
                    (TabCloseAction::This, "Close tab"),
                    (TabCloseAction::Others, "Close other tabs"),
                    (TabCloseAction::Left, "Close tabs to the left"),
                    (TabCloseAction::Right, "Close tabs to the right"),
                ]
                .into_iter()
                .enumerate()
                {
                    let enabled = !self.tabs_to_close(surface, action, cx).is_empty();
                    let key = key.clone();
                    let row = popover::menu_row(&theme, false, label)
                        .id(SharedString::from(label))
                        .when(!enabled, |el| el.opacity(0.4).cursor_default())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if !enabled {
                                return;
                            }
                            this.close_chat_menu(cx);
                            if this.panel_key(cx) == key {
                                for tab in this.tabs_to_close(surface, action, cx) {
                                    this.close_right_surface(tab, window, cx);
                                }
                            }
                        }))
                        .child(label);
                    menu =
                        menu.child(self.chat_menu_hover_row(CHAT_MENU_ROW_TAB + ix as u8, row, cx));
                }
            }
            let menu = menu.into_any_element();
            overlays.push(popover::menu_at(
                "chat-context-menu",
                position,
                menu,
                chat_menu_closing,
            ));
        }
        if let Some(menu) = self.render_pull_request_tab_menu(cx) {
            overlays.push(menu);
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        overlays.extend(self.render_section_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_command_palette(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }
        if let Some(overlay) = self.render_project_action_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let cloud = self
                .state
                .read(cx)
                .engine()
                .is_some_and(|engine| engine.cloud_target().is_some());
            let card = popover::dialog_card(&theme)
                .w(px(440.))
                .overflow_hidden()
                .child(
                    div().flex().flex_col().gap(px(10.))
                        .child(popover::dialog_title(&theme, "Force delete session?"))
                        .child(popover::dialog_body(&theme,
                            format!("“{title}” and its conversations and agents will be permanently deleted.")))
                        .child(popover::dialog_body(&theme,
                            if cloud {
                                "Its cloud computer, disk, stored history and recovery data will be permanently deleted."
                            } else {
                                "Its private worktree and unsaved files will also be removed. Shared worktrees and your main project folder are kept."
                            })),
                )
                .child(
                    popover::dialog_footer(&theme)
                        .child(div().flex_1().text_size(px(11.)).text_color(theme.text_muted).child("This can’t be undone."))
                        .child(
                            popover::btn_ghost(&theme, &tr(cx, msg::common::cancel), "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(
                                &theme,
                                if self.deleting_chat { "Deleting…" } else { "Force delete" },
                            )
                                .id("delete-chat-confirm")
                                .opacity(if self.deleting_chat { 0.5 } else { 1. })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        if let Some(flow) = self.discard_working_tree.clone() {
            let card = match flow {
                DiscardWorkingTreeFlow::Confirm(_) => {
                    popover::dialog_card(&theme)
                        .child(popover::dialog_title(
                            &theme,
                            "Discard working tree changes?",
                        ))
                        .child(div().mt(px(6.0)).child(popover::dialog_body(
                            &theme,
                            "Discard all uncommitted changes in this working tree? This can’t be undone.",
                        )))
                        .child(
                            popover::dialog_footer(&theme)
                                .child(
                                    popover::btn_ghost(
                                        &theme,
                                        &tr(cx, msg::common::cancel),
                                        "discard-working-tree-cancel",
                                    )
                                    .id("discard-working-tree-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.discard_working_tree = None;
                                        cx.notify();
                                    })),
                                )
                                .child(
                                    popover::btn_danger(&theme, "Discard changes")
                                        .id("discard-working-tree-confirm")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_discard_working_tree(cx)
                                        })),
                                ),
                        )
                        .into_any_element()
                }
                DiscardWorkingTreeFlow::Failed(error) => popover::dialog_card(&theme)
                    .child(popover::dialog_title(&theme, "Couldn’t discard changes"))
                    .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, error)))
                    .child(
                        popover::dialog_footer(&theme).child(
                            popover::btn_primary(&theme, "Close")
                                .id("discard-working-tree-error-close")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.discard_working_tree = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .into_any_element(),
            };
            overlays.push(popover::modal(
                "discard-working-tree-dialog",
                viewport,
                card,
            ));
        }

        if let Some(sync) = self.render_sync_overlay(viewport, cx) {
            overlays.push(sync);
        }
        if let Some(update) = self.render_update_prompt(viewport, cx) {
            overlays.push(update);
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        kind: PaneResizeKind,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let theme = Theme::of(cx);
        let fade_key = format!("pane-resize-{id}");
        let hover_highlight = motion::hover_blend(
            &fade_key,
            theme.border_strong.opacity(0.0),
            theme.border_strong,
        );
        let active = self.pane_resize_active == Some(kind);
        let constrained = self.pane_resize_dragging == Some(kind) && !active;
        let highlight = if constrained {
            theme.border_strong.opacity(0.0)
        } else if active {
            theme.border_strong
        } else {
            hover_highlight
        };
        let clear = highlight.opacity(0.0);
        let release_key = fade_key.clone();
        let release_out_key = fade_key.clone();
        div()
            .id(id)
            .absolute()
            .top(px(PANE_RESIZE_HITBOX_TOP))
            .bottom_0()
            .w(px(PANE_RESIZE_HITBOX_HALF_WIDTH * 2.0))
            .flex_none()
            .occlude()
            .cursor_col_resize()
            .on_hover(motion::hover_listener(fade_key))
            // Codex-style seam feedback: the existing 1px panel border stays
            // visible at rest; hover adds a stronger center highlight that
            // fades back into that border toward both ends.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(PANE_RESIZE_HITBOX_HALF_WIDTH))
                    .w(px(1.0))
                    .flex()
                    .flex_col()
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(clear, 0.0),
                        gpui::linear_color_stop(highlight, 1.0),
                    )))
                    .child(div().flex_1().bg(gpui::linear_gradient(
                        180.0,
                        gpui::linear_color_stop(highlight, 0.0),
                        gpui::linear_color_stop(clear, 1.0),
                    ))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.pane_resize_dragging = Some(kind);
                    this.pane_resize_active = Some(kind);
                    cx.notify();
                }),
            )
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                    this.finish_pane_resize(kind);
                    motion::set_hover(&release_key, false, this.reduced_motion);
                    window.refresh();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, window, _| {
                    this.finish_pane_resize(kind);
                    motion::set_hover(&release_out_key, false, this.reduced_motion);
                    window.refresh();
                }),
            )
    }

    fn render_main(
        &mut self,
        window: &mut Window,
        main_content_width: f32,
        transcript_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Active cloud tabs share the normal header's tab row. Draft tabs
        // retain their canvas strip until a conversation is admitted.
        let strip = self
            .cloud_conversation
            .is_none()
            .then(|| self.render_cloud_tabs(cx))
            .flatten();
        let content = self.render_main_content(window, main_content_width, transcript_width, cx);
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .children(strip)
            .child(content)
            .into_any_element()
    }

    fn render_main_content(
        &mut self,
        window: &mut Window,
        main_content_width: f32,
        transcript_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        if matches!(self.route, Route::Zot) {
            return self.render_zot_page(window, cx);
        }

        if matches!(self.route, Route::PullRequests) {
            if self.pull_requests_page.is_none() {
                self.pull_requests_page =
                    Some(cx.new(|cx| PullRequestsPage::new(self.state.clone(), cx)));
            }
            let outlet = self
                .pull_requests_page
                .as_ref()
                .cloned()
                .map(IntoElement::into_any_element)
                .unwrap_or_else(|| Empty.into_any_element());
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        if matches!(self.route, Route::Usage) {
            return self.usage_view(window, cx);
        }
        if matches!(self.route, Route::Archives) {
            return self
                .archive_project
                .as_ref()
                .and_then(|id| self.project_archives.get(id))
                .map(|page| page.clone().into_any_element())
                .unwrap_or_else(|| {
                    zuse_viewport::project_archive::centered(
                        "Select a project to view archived chats.",
                    )
                    .into_any_element()
                });
        }
        if matches!(self.route, Route::Plugins) {
            let page = settings::plugins::PluginsPage::shared(self.state.clone(), cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(page)
                .into_any_element();
        }

        if let Some(conversation) = self.cloud_conversation.clone() {
            // The environment summary toggle works for cloud chats too.
            let reveal = self.session_info_reveal(true);
            let card = self.render_cloud_session_info(
                &conversation,
                reveal,
                self.viewport_height - Theme::TITLEBAR_HEIGHT - 160.0,
                cx,
            );
            // Dropped files and dragged workspace paths attach to the cloud
            // chat's own composer, like any conversation.
            let composer = conversation.read(cx).composer.clone();
            return self
                .chat_dropzone("chat-dropzone", composer, cx)
                .debug_selector(|| "chat-dropzone".into())
                .flex_1()
                .min_w_0()
                .h_full()
                .child(conversation)
                .children(card)
                .into_any_element();
        }
        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty()
            || !self.state.read(cx).cloud_sidebar_projects().is_empty();
        let cloud_starting = !has_selection && self.composer.read(cx).cloud_launch_pending(cx);
        let has_appshots = !self.composer.read(cx).staged_appshots().is_empty();
        let no_project = self.state.read(cx).no_project;
        let transcript_geometry_ready = bottom_stack_measurement_matches(
            self.bottom_stack_has_composer.get(),
            (has_spaces || no_project || has_appshots) && has_selection,
        );
        let ui_settings = settings::current(cx);
        let new_thread_background_setting = ui_settings.new_thread_composer_background;
        let new_thread_background_effect = ui_settings.new_thread_background_effect;
        let new_thread_background_adjustment = new_thread_background_setting
            .as_ref()
            .map(|background| background.adjustment)
            .unwrap_or_default();
        let frame_time = self.render_time.unwrap_or_else(std::time::Instant::now);
        // Prewarm even in an established thread. Decode/effect work is not
        // contingent on a hero measurement or a navigation gesture.
        let artwork = new_thread_background_setting
            .as_ref()
            .and_then(|background| {
                crate::new_thread_background_effects::prepare(
                    new_thread_background_effect,
                    theme,
                    std::path::Path::new(&background.path),
                    cx,
                )
            });
        let artwork_frame = self.new_thread_artwork_ready.frame(
            artwork,
            new_thread_background_setting
                .as_ref()
                .map(|background| std::path::Path::new(&background.path)),
            new_thread_background_adjustment,
            new_thread_background_setting.is_some(),
            self.reduced_motion,
            frame_time,
        );
        let dock_frame = self.composer_dock.borrow_mut().tick(
            has_selection || cloud_starting,
            self.reduced_motion,
            frame_time,
        );
        if dock_frame.active {
            self.motion_active.set(true);
        }
        self.composer
            .update(cx, |composer, cx| composer.set_dock_frame(dock_frame, cx));
        // The session card floats over the transcript once opened.
        let session_reveal = self.session_info_reveal(has_selection);
        // Panel open/close glides the composer width; a window resize or a
        // seam drag is direct manipulation, so the width tracks the pointer.
        // The Zot panel leaves a narrow column; keep the composer off its edges.
        let zot_coordinator = self.state.read(cx).selected_chat_is_zot_coordinator();
        let direct_width =
            self.reduced_motion || self.viewport_resized || self.pane_resize_dragging.is_some();
        let composer_width = self.composer_dock.borrow_mut().layout_width(
            composer_target_width(
                main_content_width
                    - if (has_selection && !zot_coordinator) || cloud_starting {
                        0.
                    } else {
                        2. * if self.viewport_width <= 800. {
                            16.
                        } else {
                            24.
                        }
                    },
                if cloud_starting {
                    zuse_viewport::composer_style::CONVERSATION_WIDTH
                } else {
                    ui_settings.transcript_width
                },
                has_selection || cloud_starting,
            ),
            direct_width,
            frame_time,
        );
        self.composer.update(cx, |composer, cx| {
            composer.set_available_width(composer_width, cx)
        });
        let terminal_geometry =
            std::rc::Rc::new(std::cell::Cell::new(crate::terminal::dock::Geometry::new(
                self.eval_tween(self.terminal_tween, self.terminal_target(cx)),
                self.settings.terminal_height,
                (self.viewport_height * TERMINAL_MAX_VH).min(
                    (self.viewport_height - Theme::TITLEBAR_HEIGHT - Theme::STATUS_STRIP_HEIGHT)
                        .max(0.0),
                ),
            )));
        let term_h = self.terminal_geometry.get().height;
        let new_thread_background_layer =
            (!cloud_starting && (!has_selection || dock_frame.active)).then(|| {
                if artwork_frame.active {
                    window.request_animation_frame();
                }
                let width = (self.viewport_width - self.sidebar_now()).max(0.0);
                let bounds = self.composer.read(cx).surface_bounds();
                let opacity = new_thread_background_opacity(theme.is_frost());
                let has_artwork = artwork_frame.current.is_some();
                div()
                    .absolute()
                    .inset_0()
                    .child(new_thread_background(
                        artwork_frame.previous,
                        artwork_frame.previous_adjustment,
                        self.viewport_height,
                        width,
                        bounds.clone(),
                        dock_frame.dissolve(),
                        (1.0 - artwork_frame.mix) * opacity,
                    ))
                    .child(new_thread_background(
                        artwork_frame.current,
                        artwork_frame.current_adjustment,
                        self.viewport_height,
                        width,
                        bounds,
                        dock_frame.dissolve(),
                        artwork_frame.mix * opacity,
                    ))
                    // Without artwork, invaders march across a dim band
                    // under the titlebar; it leaves with the canvas on send.
                    .when(
                        !has_artwork && self.viewport_height >= NEW_THREAD_ARCADE_MIN_WINDOW,
                        |layer| {
                            layer.child(
                                div()
                                    .absolute()
                                    .top(px(Theme::TITLEBAR_HEIGHT))
                                    .left_0()
                                    .right_0()
                                    .h(px(NEW_THREAD_ARCADE_HEIGHT))
                                    .opacity(1.0 - dock_frame.dissolve())
                                    .child(crate::arcade::invaders_header(
                                        format!("new-thread-invaders-{}", cx.entity_id()),
                                        NEW_THREAD_ARCADE_HEIGHT,
                                        theme.text,
                                    )),
                            )
                        },
                    )
                    .into_any_element()
            });

        // Content outlet: selected chat → transcript; nothing selected → the
        // centered new-thread composition; no spaces at all → the onboarding
        // card. New-chat mode mints the chat id on first send.
        let departing_transcript = !has_selection && dock_frame.transcript() > 0.0;
        if !has_selection && !departing_transcript {
            self.transcript
                .update(cx, |transcript, cx| transcript.finish_route_exit(cx));
        }
        let outlet: AnyElement = if has_selection || departing_transcript {
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .top(px(8.0 * (1.0 - dock_frame.transcript())))
                        .size_full()
                        .when(departing_transcript, |el| el.w(px(transcript_width)))
                        .opacity(if transcript_geometry_ready || departing_transcript {
                            dock_frame.transcript()
                        } else {
                            0.0
                        })
                        .child(self.transcript.clone()),
                )
                // A departing transcript is visual history, not an active
                // interaction surface bound to the newly blank route.
                .when(departing_transcript, |el| {
                    el.child(div().absolute().inset_0().occlude())
                })
                .into_any_element()
        } else if cloud_starting {
            self.render_cloud_launch_outlet(cx)
        } else if !has_spaces && !no_project {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::ZUSE_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(crate::typography::text_lg())
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text_strong)
                                .child(SharedString::from("Where should agents work?")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .max_w(px(360.))
                                .text_center()
                                .text_size(crate::typography::text_sm())
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "Add a folder from one of your devices, or run agents on a cloud computer with a GitHub repository.",
                                )),
                        )
                        .child(
                            div()
                                .mt(px(20.0))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    popover::btn_primary(&theme_owned, "Add a project")
                                        .id("onboarding-add-space")
                                        .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                                )
                                .when(crate::cloud_beta(), |row| row.child(
                                    popover::btn_ghost(&theme_owned, "Set up Cloud", "onboarding-cloud")
                                        .id("onboarding-cloud")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.open_settings(SettingsSection::Cloud, cx)
                                        })),
                                )),
                        )
                        .child(
                            div()
                                .id("onboarding-without-project")
                                .mt(px(12.))
                                .cursor_pointer()
                                .text_size(crate::typography::text_sm())
                                .text_color(theme.text_muted)
                                .hover(|view| view.text_color(theme.text))
                                .child(tr(cx, msg::chat::start_without_project))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state
                                        .update(cx, |state, cx| state.select_space(None, cx));
                                    this.focus_composer(cx);
                                })),
                        ),
                ))
                .into_any_element()
        } else {
            let state = self.state.read(cx);
            let project = state
                .cloud_project
                .as_ref()
                .and_then(|project| project["displayName"].as_str())
                .map(str::to_owned)
                .or_else(|| {
                    state
                        .selected_space_row()
                        .map(|space| space.display_name().to_owned())
                });
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.))
                .pb(px(self
                    .composer
                    .read(cx)
                    .surface_bounds()
                    .get()
                    .map(|bounds| f32::from(bounds.size.height))
                    .unwrap_or(106.)
                    + 68.))
                .child(
                    div()
                        .text_size(crate::typography::text_2xl())
                        .line_height(crate::typography::ui_rems(32.))
                        .relative()
                        .top(px(-1.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_strong)
                        .child(project.as_ref().map_or_else(
                            || "What should we build today?".to_owned(),
                            |name| format!("What should we build in {name}?"),
                        )),
                )
                .when(project.is_some(), |view| {
                    view.child(
                        div()
                            .id("landing-without-project")
                            .role(gpui::Role::Button)
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .hover(|d| d.text_color(theme.text))
                            .child(tr(cx, msg::chat::start_without_project))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let pickers = this.composer.read(cx).pickers().clone();
                                pickers.update(cx, |pickers, cx| pickers.start_without_project(cx));
                                this.focus_composer(cx);
                            })),
                    )
                })
                .into_any_element()
        };

        let status = self.render_status_strip(composer_width, cx);
        let session_card = self.render_session_info(
            session_reveal,
            self.viewport_height - Theme::TITLEBAR_HEIGHT - self.bottom_stack.get() - 24.0,
            cx,
        );
        self.chat_dropzone("chat-dropzone", self.composer.clone(), cx)
            .debug_selector(|| "chat-dropzone".into())
            .track_focus(&self.navigation_focus.main)
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.capture_navigation_focus(false, false, window, cx);
            }))
            .on_key_down(|event, _, cx| navigation_focus::copy_transcript_selection(event, cx))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            // The hero is deliberately outside the transcript EdgeFade below:
            // it must paint under the overlaid titlebar instead of becoming
            // fully transparent across the titlebar's inset band.
            .children(new_thread_background_layer)
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The terminal dock is NOT glass the transcript may slide
                    // under: with the dock's translucent fill, transcript text
                    // ghosted through the grid (user report). The underlay
                    // ends at the dock's top instead, riding the same height
                    // tween the dock animates with; `stack_h` below is only
                    // the chrome that still overlaps the transcript (status
                    // strip + composer).
                    let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .child(crate::terminal::dock::above_terminal(
                            if has_selection || departing_transcript {
                                // The transcript fades its scrolling content;
                                // its find bar retains normal clipping and stacking.
                                self.transcript.update(cx, |transcript, cx| {
                                    transcript.set_chrome_fade_bottom(bottom_band, cx);
                                });
                                div().size_full().child(outlet).into_any_element()
                            } else {
                                crate::edge_fade::edge_faded(
                                    Theme::TRANSCRIPT_FADE_BAND,
                                    true,
                                    true,
                                    div().size_full().child(outlet),
                                )
                                .inset_top(Theme::TITLEBAR_HEIGHT)
                                .band_bottom(bottom_band)
                                .into_any_element()
                            },
                            terminal_geometry.clone(),
                        ))
                },
            )
            .children(session_card)
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts), composer, terminal dock. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                let measured_has_composer = self.bottom_stack_has_composer.clone();
                let contains_composer = (has_spaces || no_project || has_appshots) && has_selection;
                let composer = self.composer.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, window, cx| {
                                // Reserve the destination footprint, never the animated height.
                                let next_height = f32::from(bounds.size.height)
                                    + composer.read(cx).dock_clearance_correction();
                                let changed = (measured.get() - next_height).abs() > 0.5
                                    || measured_has_composer.get() != contains_composer;
                                measured.set(next_height);
                                measured_has_composer.set(contains_composer);
                                if changed {
                                    window.request_animation_frame();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .children(self.render_voice_composer_orb(composer_width, cx))
                    .child(status)
                    .when(has_spaces || no_project || has_appshots, |el| {
                        let composer_opacity = self.composer_dock.borrow().opacity();
                        el.child(
                            crate::composer_dock::docked_composer(
                                div()
                                    .id("persistent-composer")
                                    .relative()
                                    .w(px(composer_width))
                                    .pb(px(if cloud_starting {
                                        16.
                                    } else if zot_coordinator {
                                        12.
                                    } else {
                                        0.
                                    }))
                                    .opacity(composer_opacity)
                                    .mx_auto()
                                    .child(self.composer.clone())
                                    .children(if has_selection {
                                        self.render_jump_to_bottom(cx)
                                    } else {
                                        None
                                    }),
                                self.composer_dock.clone(),
                                self.viewport_height - self.workspace_gutter(),
                                self.reduced_motion,
                                frame_time,
                            )
                            .floating_clearance(16.)
                            .reserve_terminal(terminal_geometry.clone()),
                        )
                    })
                    .child(self.render_terminal_container(terminal_geometry, window, cx))
            })
            .child(Self::mention_drop_overlay(theme))
            .into_any_element()
    }

    /// The "Jump to latest" pill: a small `rounded-md` card chip centred over
    /// the transcript column, floating just above the composer. It shares the
    /// composer's measured dock transform and paints after it, outside the
    /// transcript fade.
    fn render_jump_to_bottom(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        Some(
            div()
                .absolute()
                // Share the composer's measured translation, not its final
                // bottom-stack target. Paint after the composer so it cannot
                // pass over this control during docking.
                .top(px(-32.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(self.jump_pill("jump-to-bottom", "jump-pill", self.transcript.clone(), cx))
                .into_any_element(),
        )
    }

    /// The jump pill itself — shared between the conversation overlay and
    /// the subagent pane so both read as one control. `anim_key`/`hover_key`
    /// must be distinct per instance (they key global animation state).
    ///
    /// `min-h-6 rounded-md border-border/60 bg-card px-2 text-[10px]`: a
    /// chevron, or a pulsing dot while the reply is still streaming in out
    /// of view.
    fn jump_pill(
        &self,
        anim_key: &'static str,
        hover_key: &'static str,
        transcript: Entity<Transcript>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let glass = theme.is_frost();
        let base = if glass {
            popover::surface_bg(&theme)
        } else {
            theme.surface_raised
        };
        let streaming = {
            let state = self.state.read(cx);
            state.selected_chat.as_deref().is_some_and(|chat| {
                state.indicator_for(chat, Utc::now()) == crate::state::Indicator::Working
            })
        };
        let lead = if streaming {
            let pulse = motion::activity_pulse(cx.entity_id(), cx);
            div()
                .size(px(6.0))
                .rounded_full()
                .bg(theme.text_muted)
                .opacity(pulse.opacity(0.0, 0.35))
                .into_any_element()
        } else {
            crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                .size(px(12.0))
                .text_color(motion::hover_blend(hover_key, theme.text_muted, theme.text))
                .into_any_element()
        };
        let pill = div()
            .id(anim_key)
            .min_h(px(24.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(motion::hover_blend(
                hover_key,
                theme.border.opacity(0.6),
                theme.border,
            ))
            .when(!glass, |el| el.shadow_sm())
            .cursor_pointer()
            .bg(base)
            .text_size(px(10.0))
            .text_color(motion::hover_blend(hover_key, theme.text_muted, theme.text))
            .role(gpui::Role::Button)
            .aria_label(tr(cx, msg::chat::jump_to_latest_pill_jump_to_latest))
            .on_hover(motion::hover_listener(hover_key))
            .on_click(cx.listener(move |_, _, _, cx| {
                transcript.update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
            }))
            .child(lead)
            .child(SharedString::from("Jump to latest"));
        // Frost OUTSIDE the entry animation (the composer pill's exact
        // composition): one scene layer — blur, then the pill's quads, then
        // glyphs — so the pill always composes over the transcript content
        // scrolling under it (frost.rs module docs).
        crate::frost::frosted(
            6.0,
            crate::frost::MENU_BLUR,
            motion::dialog_in(anim_key, pill),
        )
        .into_any_element()
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(
        &mut self,
        geometry: crate::terminal::dock::SharedGeometry,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            self.terminal_geometry
                .set(crate::terminal::dock::Geometry::default());
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        // The dock spans the main column's bottom: when the sidebar is fully
        // closed the column IS the window's left edge (and likewise the right
        // edge when the right pane is closed) — the panel's fill then carries
        // the CSD window's bottom corners.
        let window_corner = Self::window_corner_radius(window) > 0.0;
        {
            let bl = window_corner && self.sidebar_now() < 0.5;
            let br = window_corner && !self.right_pane_open(cx);
            panel.update(cx, |panel, cx| panel.set_window_corners(bl, br, cx));
        }
        let border = Theme::of(cx).border;
        let handle_key = "pane-resize-terminal-resize";
        let handle_hover = motion::hover_blend(
            handle_key,
            Theme::of(cx).border_strong.opacity(0.0),
            Theme::of(cx).border_strong,
        );
        let terminal_active = self.pane_resize_active == Some(PaneResizeKind::Terminal);
        let terminal_constrained =
            self.pane_resize_dragging == Some(PaneResizeKind::Terminal) && !terminal_active;
        let handle_highlight = if terminal_constrained {
            Theme::of(cx).border_strong.opacity(0.0)
        } else if terminal_active {
            Theme::of(cx).border_strong
        } else {
            handle_hover
        };
        let handle = div()
            .id("terminal-resize")
            .h(px(TERMINAL_RESIZE_HITBOX_HEIGHT))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .on_hover(motion::hover_listener(handle_key))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(handle_highlight),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.terminal_drag_anchor = Some((
                        f32::from(event.position.y),
                        this.terminal_geometry.get().height,
                    ));
                    this.pane_resize_dragging = Some(PaneResizeKind::Terminal);
                    this.pane_resize_active = Some(PaneResizeKind::Terminal);
                    cx.notify();
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                    this.finish_pane_resize(PaneResizeKind::Terminal);
                    motion::set_hover(handle_key, false, this.reduced_motion);
                    window.refresh();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    this.finish_pane_resize(PaneResizeKind::Terminal);
                    motion::set_hover(handle_key, false, this.reduced_motion);
                    window.refresh();
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its hitbox would read as
        // dead air between the seam and the tab bar (user report).
        crate::terminal::dock::terminal(geometry, self.terminal_geometry.clone(), move |geometry| {
            let inner = div()
                .h(px(geometry.content_height))
                .w_full()
                .relative()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(panel))
                .child(handle.absolute().top_0().left_0().right_0());

            div()
                .w_full()
                .flex_none()
                .overflow_hidden()
                .when(geometry.height > 0.0, |el| el.border_t_1())
                .border_color(border)
                .h(px(geometry.height))
                .child(inner)
                .into_any_element()
        })
        .into_any_element()
    }

    /// Working indicator strip: working mark + status + elapsed,
    /// staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, composer_width: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Keep notices aligned with the current composer width, including
        // the route glide. The inner gutter sits just inside the pill edge.
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(composer_width))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(crate::typography::text_xs());

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::mini_glyph_spinner(
                    format!("sending-indicator-{}", cx.entity_id()),
                    2.5,
                    theme.loader_glyph(),
                    cx.entity_id(),
                    cx,
                ))
                .child(
                    div()
                        .text_size(crate::typography::text_xs())
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right pane — the surface host: hidden by
    /// default, drag-resizable. Content is the ACTIVE surface — the Diff
    /// page (its options row + the lazy [`Changes`] viewer), workspace Files,
    /// an embedded terminal, or the surface picker when no tabs exist.
    fn render_right_pane(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let exiting = self.fit_exit_width(1).is_some();
        let visible = self.horizontal_fit().right || exiting;
        let showing =
            visible && (self.right_pane_open(cx) || self.tween_active(self.right_tween) || exiting);
        // Only the tree on screen keeps its git status watch.
        let shown_tree = (showing
            && self.right_pane_open(cx)
            && self.resolved_right_active(cx) == RightSurface::Files)
            .then(|| self.panel_key(cx));
        for (key, files) in &self.files {
            if shown_tree.as_ref() != Some(key) {
                files.update(cx, |files, _| files.release_git_status());
            }
        }
        if !visible {
            return Empty.into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let content: AnyElement = if showing {
            match self.resolved_right_active(cx) {
                RightSurface::Files => match self.files.get(&self.panel_key(cx)).cloned() {
                    Some(files) => {
                        if self.right_pane_open(cx) {
                            files.update(cx, |files, cx| {
                                files.ensure_loaded(cx);
                                files.ensure_git_status(cx);
                            });
                        }
                        self.sync_explorer_selection(cx);
                        files.into_any_element()
                    }
                    None => self.render_surface_picker(cx),
                },
                RightSurface::File(id) => {
                    if let Some(file) = self.file_surfaces.get(&id).cloned() {
                        // Keep the last file frame behind the closing mask. Loading
                        // while closing would reactivate suspended image resources.
                        if self.right_pane_open(cx) {
                            file.update(cx, |file, cx| file.ensure_loaded(cx));
                        }
                        file.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::Diff(id) if self.diffs.contains_key(&id) => {
                    let changes = self.diffs.get(&id).cloned().expect("checked");
                    // Idempotent — also covers a persisted-open pane on boot.
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                    // The diff options (scope dropdown, ref selector,
                    // fold-all) moved DOWN from the titlebar band — the
                    // surface tabs own that row now; the expand/close
                    // buttons stayed up there (user request).
                    let controls =
                        changes.update(cx, |changes, cx| changes.render_header_controls(cx));
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(crate::surface_chrome::toolbar(&theme).child(controls))
                        .child(div().flex_1().min_h_0().child(changes))
                        .into_any_element()
                }
                RightSurface::Simulator(id) => self
                    .simulators
                    .get(&id)
                    .cloned()
                    .map(|pane| pane.into_any_element())
                    .unwrap_or_else(|| self.render_surface_picker(cx)),
                RightSurface::Browser(id) => self
                    .browsers
                    .get(&id)
                    .cloned()
                    .map(|browser| browser.into_any_element())
                    .unwrap_or_else(|| self.render_surface_picker(cx)),
                RightSurface::Terminal(tab) => {
                    let panel = self.right_terminal_panel(cx);
                    // Keep the embedded panel's own active tab aligned with
                    // the resolved surface (fallbacks can move it).
                    let resize_suspended = self.tween_active(self.right_tween)
                        || self.tween_active(self.files_tween)
                        || self.tween_active(self.sidebar_tween);
                    panel.update(cx, |panel, cx| {
                        panel.set_resize_suspended(resize_suspended);
                        panel.select_tab_by_key(tab, cx);
                    });
                    panel.into_any_element()
                }
                RightSurface::SideChat(id) => self.render_side_chat(id, cx),
                RightSurface::PullRequest(id) => self
                    .render_chat_pull_request(id, cx)
                    .unwrap_or_else(|| self.render_surface_picker(cx)),
                RightSurface::Subagent(id) if self.subagent_tabs.contains_key(&id) => {
                    let transcript = self
                        .subagent_tabs
                        .get(&id)
                        .expect("checked")
                        .transcript
                        .clone();
                    // The pane hosts its own jump pill: the conversation
                    // overlay's is bound to the PRIMARY transcript, and this
                    // one anchors to the pane (no composer stack to clear).
                    let pill = transcript.read(cx).jump_button_shown().then(|| {
                        div()
                            .absolute()
                            .bottom(px(16.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .child(self.jump_pill(
                                "subagent-jump-to-bottom",
                                "subagent-jump-pill",
                                transcript.clone(),
                                cx,
                            ))
                    });
                    // Read-only surface: the transcript fills the pane — no
                    // composer, no status strip.
                    div()
                        .size_full()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(div().flex_1().min_h_0().child(transcript))
                        .children(pill)
                        .into_any_element()
                }
                _ => self.render_surface_picker(cx),
            }
        } else {
            gpui::Empty.into_any_element()
        };
        // Where the workspace runs a gutter the panel is an inset card that
        // matches the conversation card: same radius, same hairline, the
        // gutter on every outer side. Without a gutter it stays flush: full
        // window height with a left hairline. The resize grabber lives outside
        // this clipped container, on the root layout's seam.
        let panel_bg = theme.panel_bg();
        let gutter = self.workspace_gutter();
        let inset = gutter > 0.0;
        let panel = div()
            .id("right-pane-focus")
            .debug_selector(|| "right-pane".into())
            .track_focus(&self.navigation_focus.right)
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.capture_navigation_focus(true, false, window, cx);
            }))
            .on_key_down(|event, _, cx| navigation_focus::copy_transcript_selection(event, cx))
            .size_full()
            .flex()
            .flex_col()
            // In takeover the panel's left edge IS the sidebar seam, which
            // already carries the sidebar tone's right hairline — a second
            // border there doubled up (user report).
            .when(inset, |el| {
                el.rounded(px(zuse_viewport::workspace_style::RADIUS))
                    .border_1()
                    .border_color(crate::theme::hairline(0.05))
            })
            .when(!inset && !self.right_pane_expanded, |el| {
                el.border_l_1().border_color(theme.border)
            })
            // With the explorer undocked the panel's right edge IS the
            // window's right edge: it carries the CSD window's rounded corners
            // directly (gpui cannot clip children rounded — each full-bleed
            // layer rounds itself; see [`Self::window_corner_radius`]). Docked,
            // the explorer column is the rightmost layer and rounds instead.
            .when(
                !inset
                    && Self::window_corner_radius(window) > 0.0
                    && self.files_visible_width(cx) <= 0.0,
                |el| {
                    let corner = Self::window_corner_radius(window);
                    el.rounded_tr(px(corner)).rounded_br(px(corner))
                },
            )
            .bg(panel_bg)
            .overflow_hidden()
            // The titlebar is a glass overlay over the full-height content
            // row; the panel's own chrome starts below it.
            .when(
                !matches!(self.resolved_right_active(cx), RightSurface::SideChat(_)),
                |el| el.pt(px(Theme::TITLEBAR_HEIGHT - gutter)),
            )
            .child(content);
        let target = self.right_target(cx);
        let edge_offset = self.eval_resize_edge_bounce(
            self.right_edge_bounce,
            self.right_pane_open(cx) && !self.right_pane_expanded,
        );
        self.right_pane_container(
            target,
            self.right_visible_width(cx),
            edge_offset,
            div()
                .h_full()
                .relative()
                .when(inset, |el| el.py(px(gutter)).pr(px(gutter)))
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_signed_out_restart(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let runtime_change_label = if self.runtime_change_task.is_some() {
            "Stopping engine…"
        } else {
            "Retry local mode"
        };
        let card = div()
            .w(px(380.0))
            .px(px(32.0))
            .py(px(40.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .child(
                icon(icons::ZUSE_LOGO)
                    .w(px(31.4))
                    .h(px(36.0))
                    .text_color(theme.text),
            )
            .child(
                div()
                    .mt(px(24.0))
                    .text_size(crate::typography::text_xl())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_strong)
                    .child(SharedString::from("Signed out")),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(24.0))
                    .text_size(crate::typography::text_sm())
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(
                        "Zuse removed your credentials but could not finish closing the previous synced workspace. Retry before continuing in local mode.",
                    )),
            )
            .when_some(self.runtime_change_error.clone(), |card, error| {
                card.child(
                    div()
                        .mb(px(16.0))
                        .text_size(crate::typography::text_xs())
                        .line_height(px(17.0))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                popover::btn_primary(&theme, runtime_change_label)
                    .id("signed-out-quit")
                    .when(self.runtime_change_task.is_some(), |button| {
                        button.opacity(0.6)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_local_runtime_transition(false, cx)
                    })),
            );

        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("signed-out-restart", card)),
            )
            .into_any_element()
    }

    fn close_right_plus(&mut self, cx: &mut Context<Self>) {
        if self.right_plus.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.right_plus);
        }
        cx.notify();
    }

    /// The titlebar strip over the right pane: one chip per surface tab
    /// (icon · title · ✕) plus the `+` menu — the right-pane tab bar,
    /// living in the top row; the diff options moved into the pane below.
    pub(crate) fn render_right_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::surface_chrome::{
            TAB_CHIP_HEIGHT as CHIP_H, TAB_CHIP_SLOT as CHIP_SLOT, TAB_CHIP_WIDTH as CHIP_W,
        };

        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the strip.
        if self.right_tab_drag.is_some() && !cx.has_active_drag() {
            self.right_tab_drag = None;
        }
        let rows = self.right_surface_rows(cx);
        let count = rows.len();
        let active = self.resolved_right_active(cx);
        let drag = self
            .right_tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        // The old session-tab strip's proven scroll shape: the flex row IS
        // the scroller (id + overflow_x_scroll + track_scroll), wrapped in a
        // relative min_w_0 region below; drop math runs in CONTENT
        // coordinates (viewport-relative x plus the scrolled-off width).
        let scroll_for_drag = self.right_tab_scroll.clone();
        let mut strip = div()
            .id("right-surface-strip")
            .track_focus(&self.navigation_focus.tabs)
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.capture_navigation_focus(true, true, window, cx);
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.right_tab_scroll)
            // Windows caption hit-testing includes the scroll-only hitboxes
            // behind each chip. Stop at the scroller so the titlebar cannot
            // claim tab clicks, while wheel events still reach this scroller.
            .when(cfg!(target_os = "windows"), |strip| strip.occlude())
            .on_drag_move::<RightTabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<RightTabDrag>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.panel_key != this.panel_key(cx) {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x)
                        - f32::from(event.bounds.left())
                        - f32::from(scroll_for_drag.offset().x);
                    let over = crate::terminal::panel::drop_index(rel_x, CHIP_SLOT, count);
                    this.update_right_tab_drag_over(from, over, cx);
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(move |this, payload: &RightTabDrag, _, cx| {
                if payload.panel_key != this.panel_key(cx) {
                    this.right_tab_drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .right_tab_drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from);
                this.right_tab_drag = None;
                this.reorder_right_tabs(payload.from, to, cx);
            }));
        for (ix, (surface, title, dirty, detail)) in rows.into_iter().enumerate() {
            let is_active = surface == active;
            let file_identity_path = detail.as_ref().cloned().unwrap_or_else(|| title.clone());
            let harness_icon = match surface {
                RightSurface::SideChat(id) => self.side_chats.get(&id).and_then(|tab| {
                    tab.state
                        .read(cx)
                        .selected_chat_row()
                        .and_then(|chat| chat.config.as_ref().map(|config| config.harness))
                }),
                RightSurface::Subagent(id) => {
                    self.subagent_tabs.get(&id).and_then(|tab| tab.harness)
                }
                _ => None,
            }
            .map(crate::pickers::harness_brand_icon);
            let icon_path = match surface {
                RightSurface::File(_) => icons::DOCUMENT,
                RightSurface::Diff(id) => self
                    .diffs
                    .get(&id)
                    .map(|changes| {
                        if changes.read(cx).is_history() {
                            icons::GIT_BRANCH
                        } else {
                            icons::LIST
                        }
                    })
                    .unwrap_or(icons::LIST),
                RightSurface::SideChat(_) => icons::CHAT_ROUND_LINE,
                RightSurface::Subagent(_) => icons::BOT,
                RightSurface::Terminal(_) => icons::TERMINAL,
                RightSurface::Simulator(_) => icons::SMARTPHONE,
                RightSurface::Browser(_) => icons::GLOBE,
                RightSurface::PullRequest(_) => icons::PULL_REQUEST,
                RightSurface::Files => icons::FOLDER,
                RightSurface::Picker => icons::PLUS,
            };
            // Working chats and subagents swap their harness icon for the
            // mini spinner; subagent liveness follows the doc's streaming tail.
            let browser_favicon = match surface {
                RightSurface::Browser(id) => self
                    .browsers
                    .get(&id)
                    .and_then(|b| b.read(cx).favicon.clone()),
                _ => None,
            };
            let subagent_running = match surface {
                RightSurface::SideChat(id) => self.side_chats.get(&id).is_some_and(|tab| {
                    let state = tab.state.read(cx);
                    state
                        .selected_chat
                        .as_deref()
                        .is_some_and(|id| state.indicator_for(id, Utc::now()) == Indicator::Working)
                }),
                RightSurface::Browser(id) => self
                    .browsers
                    .get(&id)
                    .is_some_and(|b| b.read(cx).page.loading),
                RightSurface::PullRequest(id) => self.chat_pull_request_loading(id, cx),
                RightSurface::Subagent(id) => self.subagent_tabs.get(&id).is_some_and(|tab| {
                    self.state
                        .read(cx)
                        .sub_transcript(&tab.doc_id)
                        .last()
                        .is_some_and(|e| e.status == Some(zuse_doc::MessageStatus::Streaming))
                }),
                _ => false,
            };
            let ghost_title = title.clone();
            let workspace_path = self.workspace_path_for_surface(surface, cx);
            let accessible_name = detail.as_ref().unwrap_or(&title);
            let accessible_label = if dirty {
                format!("{accessible_name}, unsaved changes")
            } else {
                accessible_name.to_string()
            };
            let leading = if subagent_running {
                loaders::mini_glyph_spinner(
                    format!("subagent-tab-{ix}"),
                    2.0,
                    theme.loader_glyph(),
                    cx.entity_id(),
                    cx,
                )
                .into_any_element()
            } else if let Some(favicon) = browser_favicon {
                gpui::img(favicon).size(px(12.0)).into_any_element()
            } else if let RightSurface::PullRequest(id) = surface {
                let (glyph, color) = self
                    .chat_pull_requests
                    .get(&id)
                    .map(|tab| tab.page.read(cx).lifecycle_icon(&theme))
                    .unwrap_or((icons::PULL_REQUEST, theme.text_muted));
                icons::icon(glyph)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(color)
                    .into_any_element()
            } else if matches!(surface, RightSurface::File(_)) {
                crate::file_icons::icon(
                    crate::file_icons::FileIconIdentity::file(file_identity_path.as_ref()),
                    theme.appearance,
                )
                .size(px(14.0))
                .when(!is_active, |icon| icon.opacity(0.78))
                .into_any_element()
            } else if let Some((icon_path, Some(brand_color))) = harness_icon {
                icons::brand_icon(
                    icon_path,
                    12.0,
                    brand_color.opacity(if is_active { 1.0 } else { 0.7 }),
                )
            } else {
                let icon_path = harness_icon.map_or(icon_path, |(path, _)| path);
                crate::surface_chrome::tab_chip_icon(icon_path, is_active, &theme)
            };
            let chip = crate::surface_chrome::tab_chip(
                crate::surface_chrome::TabChip {
                    id: "right-surface-tab",
                    close_id: "right-surface-close",
                    ix,
                    active: is_active,
                    workspace_style: true,
                    title,
                    dirty,
                    leading,
                },
                &theme,
                cx.listener(move |this, _, window, cx| {
                    this.close_right_surface(surface, window, cx);
                }),
                cx,
            )
            .aria_label(accessible_label)
            .when_some(detail, |chip, detail| {
                chip.anchored_tooltip(crate::tooltip::Placement::Below, move |_, cx| {
                    cx.new(|_| SurfaceTabTooltip {
                        text: detail.clone(),
                    })
                    .into()
                })
                .tooltip_show_delay(Duration::from_millis(350))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.activate_right_surface(surface, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    let chat_id = match surface {
                        RightSurface::SideChat(id) => this
                            .side_chats
                            .get(&id)
                            .and_then(|tab| tab.state.read(cx).selected_chat.clone())
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    this.chat_menu.open(ChatMenuState {
                        chat_id,
                        tab: Some((this.panel_key(cx), surface)),
                        position: event.position,
                        page: ChatMenuPage::Root,
                    });
                    cx.notify();
                }),
            )
            // Middle-click closes, like every tab strip.
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _, window, cx| {
                    this.close_right_surface(surface, window, cx);
                }),
            )
            .on_drag(
                RightTabDrag {
                    panel_key: self.panel_key(cx),
                    from: ix,
                    title: ghost_title,
                    workspace_path,
                },
                |payload, _point, _, cx| {
                    let title = payload.title.clone();
                    cx.stop_propagation();
                    cx.new(|_| SurfaceTabGhost { title })
                },
            )
            // The chip's BlockMouse hitbox (the titlebar/scroll carve-out)
            // cuts the strip out of the hover stack, so the strip's own
            // on_drop can never fire while the pointer is over a chip — tabs
            // tile the strip. Receiving the drop on the chip itself keeps
            // drag-reorder working without giving up the carve-out. The
            // bubble dispatch reaches the chip before the strip, and the
            // handler consumes the drag, so the two never double-apply.
            .on_drop::<RightTabDrag>(cx.listener(
                move |this, payload: &RightTabDrag, _, cx| {
                    if payload.panel_key != this.panel_key(cx) {
                        this.right_tab_drag = None;
                        cx.notify();
                        return;
                    }
                    let to = this
                        .right_tab_drag
                        .as_ref()
                        .map(|d| d.over)
                        .unwrap_or(payload.from);
                    this.right_tab_drag = None;
                    this.reorder_right_tabs(payload.from, to, cx);
                },
            ));

            // Sliding transform while a sibling drags over (the terminal
            // drawer's exact recipe): animate 150ms between committed
            // offsets; the dragged tab leaves an invisible spacer — the
            // ghost carries it.
            let wrapped: AnyElement = match drag {
                Some((from, over, epoch, prev_over)) if ix != from => {
                    let target = crate::terminal::panel::slide_offset(ix, from, over) * CHIP_SLOT;
                    let start =
                        crate::terminal::panel::slide_offset(ix, from, prev_over) * CHIP_SLOT;
                    div()
                        .relative()
                        .child(chip.with_animation(
                            ("right-tab-slide", (ix as u64) | ((epoch as u64) << 32)),
                            TAB_SLIDE.animation(),
                            move |el, t| el.left(px(motion::lerp(start, target, t))),
                        ))
                        .into_any_element()
                }
                Some((from, ..)) if ix == from => div()
                    .w(px(CHIP_W))
                    .h(px(CHIP_H))
                    .flex_none()
                    .into_any_element(),
                _ => chip.into_any_element(),
            };
            strip = strip.child(wrapped);
        }
        // The `+` — a small menu offering the available surfaces; mirrors
        // the picker cards.
        let plus_open = self.right_plus.get().is_some();
        let plus_fade = "right-surface-add-fade";
        let mut plus = div()
            .id("right-surface-add")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                plus_fade,
                crate::theme::wash(0.0),
                crate::theme::wash(0.11),
            ))
            .on_hover(motion::hover_listener(plus_fade))
            .block_mouse_except_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.right_plus.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.right_plus.take_press_was_open() {
                    this.close_right_plus(cx);
                } else {
                    this.right_plus.open(());
                    cx.notify();
                }
            }))
            .role(gpui::Role::Button)
            .aria_label(tr(cx, msg::chat::right_pane_add_panel))
            .aria_expanded(plus_open)
            .when(plus_open, |el| el.bg(crate::theme::wash(0.11)))
            .tip("Add panel")
            .child(
                icon(icons::PLUS)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            );
        if plus_open {
            let closing = self.right_plus.closing_since();
            let menu = self.render_right_plus_menu(&theme, cx).into_any_element();
            // Align the menu's left edge with the trigger so it opens into
            // the surface pane instead of covering the conversation.
            plus = plus.relative().child(popover::anchored_menu_below(
                "right-plus-menu",
                menu,
                closing,
            ));
        }
        strip = strip.child(plus);
        crate::surface_chrome::tab_strip_region(strip, &self.right_tab_scroll, &theme)
    }

    /// Toggle the changes-panel takeover (the header's expand button): the
    /// panel grows to fill everything right of the sidebar,
    /// hiding the conversation column; toggling back restores the saved
    /// width. Rides the same width tween as open/close so the jump glides.
    fn toggle_right_pane_expand(&mut self, cx: &mut Context<Self>) {
        // The outgoing header stays painted during close, but it must not
        // turn an already closed surface into a zero-width takeover.
        if !self.right_pane_shown(cx) {
            return;
        }
        // A second click during the first expansion starts at the painted
        // seam, not the previous destination.
        let from = self.right_visible_width(cx);
        let content_from = self.right_content_width(self.right_target(cx));
        self.right_edge_bounce = None;
        self.right_resize_edge = None;
        self.finish_pane_resize(PaneResizeKind::Right);
        let sidebar_now = self.sidebar_now();
        let from_main = self.main_content_width(self.main_target_width(from, cx), cx);
        self.right_pane_expanded = !self.right_pane_expanded;
        let to = self.right_target(cx);
        let right_transition = self.panel_tween(from, to);
        self.right_tween = Some(right_transition);
        self.right_content_tween = Some(WidthTween {
            from: content_from,
            ..right_transition
        });
        self.main_takeover_tween = Some(WidthTween {
            from: from_main,
            to: conversation_width(
                self.viewport_width - self.files_reserved_width(cx),
                sidebar_now,
                to,
            ),
            ..right_transition
        });
        cx.notify();
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (zuse Gate `Failed`),
            // plus a Retry affordance (account sockets can also retry in place).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(crate::typography::text_md())
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(crate::typography::text_sm())
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.glass_hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card (zuse App.tsx Gate): centered card on the grid —
            // logo, "Log in to Zuse", copy, full-width white Log in button.
            _ => div()
                .w(px(360.0))
                .px(px(32.0))
                .py(px(40.0))
                .rounded(px(12.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface_card)
                .shadow_lg()
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(
                    icon(icons::ZUSE_LOGO)
                        .w(px(31.4))
                        .h(px(36.0))
                        .text_color(theme.text),
                )
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(crate::typography::text_xl())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_strong)
                        .child(SharedString::from("Log in to Zuse")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(crate::typography::text_sm())
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens your browser to finish logging in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .map(|el| popover::dither_key(el, &theme, popover::DitherKey::Primary, 6.0))
                        .text_size(crate::typography::text_md())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Log in")),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (zuse App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            GatePhase::SignIn => "gate-card-signin",
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }

    /// Organization onboarding used by the synced gate and, for a local
    /// runtime, only after the user explicitly starts the sync opt-in.
    fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let local_setup = self.state.read(cx).workspace_scope == Some(WorkspaceScope::Local);
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let submitting = org.submitting;
        let error = org.error.clone();
        let name_input = org.name_input.clone();
        let orgs = org.orgs.clone();

        let email: Option<SharedString> = self
            .state
            .read(cx)
            .auth_user()
            .map(|u| u.email.clone().into());

        let memberships: AnyElement =
            match &orgs {
                Loadable::Idle | Loadable::Loading => div()
                    .mt(px(24.0))
                    .child(popover::skeleton_rows(
                        "org-skeleton",
                        &theme,
                        2,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element(),
                Loadable::Error(message) => div()
                    .mt(px(24.0))
                    .child(
                        popover::error_row(&theme, message).child(
                            div()
                                .id("orgs-retry")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::CONTROL_RADIUS))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.glass_hover()))
                                .on_click(cx.listener(|this, _, _, cx| this.load_orgs(cx)))
                                .child(SharedString::from("Retry")),
                        ),
                    )
                    .into_any_element(),
                Loadable::Ready(rows) if rows.is_empty() => Empty.into_any_element(),
                Loadable::Ready(rows) => div()
                    .mt(px(24.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb(px(8.0))
                            .text_size(crate::typography::text_xs())
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Or continue in a workspace you belong to",
                            )),
                    )
                    .child(div().flex().flex_col().gap(px(4.0)).children(
                        rows.iter().enumerate().map(|(ix, row)| {
                            let org_id = row.organization_id.clone();
                            div()
                                .id(("org-row", ix))
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .text_size(crate::typography::text_sm())
                                .text_color(theme.text)
                                .when(submitting, |el| el.opacity(0.5))
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::wash(0.11)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_org(org_id.clone(), cx);
                                }))
                                .child(SharedString::from(row.name.clone()))
                        }),
                    ))
                    .into_any_element(),
            };

        // zuse App.tsx OrgGate: w-400 card on the grid — logo, headline,
        // explainer (+ signed-in email), name form with a white Create button,
        // then existing memberships and the account escape hatch.
        let blurb: SharedString = match email {
            Some(email) => format!(
                "Zuse is organized around workspaces — create one for yourself or your team. Signed in as {email}."
            )
            .into(),
            None => {
                "Zuse is organized around workspaces — create one for yourself or your team."
                    .into()
            }
        };
        let card = div()
            .w(px(400.0))
            .px(px(32.0))
            .py(px(36.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_card)
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                icon(icons::ZUSE_LOGO)
                    .w(px(24.4))
                    .h(px(28.0))
                    .text_color(theme.text),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .text_size(crate::typography::text_xl())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_strong)
                    .child(SharedString::from("Create your workspace")),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(24.0))
                    .text_size(crate::typography::text_sm())
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(blurb),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .px(px(12.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .text_size(crate::typography::text_sm())
                            .child(name_input),
                    )
                    .child(
                        div()
                            .id("create-org")
                            .h(px(36.0))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .rounded(px(6.0))
                            .map(|el| {
                                popover::dither_key(el, &theme, popover::DitherKey::Primary, 6.0)
                            })
                            .text_size(crate::typography::text_md())
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .when(submitting, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| this.create_org(cx)))
                            .child(SharedString::from(if submitting {
                                "Creating…"
                            } else {
                                "Create"
                            })),
                    ),
            )
            .child(memberships)
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(16.0))
                        .text_size(crate::typography::text_xs())
                        .line_height(px(17.0))
                        .text_color(theme.danger_muted.opacity(0.9)) // red-300
                        .child(message),
                )
            })
            .child(
                div().mt(px(24.0)).flex().flex_row().child(
                    div()
                        .id("org-signout")
                        .text_size(crate::typography::text_xs())
                        .text_color(theme.text_muted.opacity(0.6))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_auth_setup(cx)))
                        .child(SharedString::from(if local_setup {
                            "Cancel sync setup"
                        } else {
                            "Use a different account"
                        })),
                ),
            );

        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("org-gate-card", card)),
            )
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (zuse styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = crate::theme::hairline(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (zuse window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    label: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let glyph = icon(icon_path).size(px(16.0)).text_color(theme.text_muted);
    window_control_button_with(id, glyph, label, theme, on_click)
}

/// [`window_control_button`] around a caller-drawn glyph (the morphing
/// sidebar glyph).
fn window_control_button_with(
    id: &'static str,
    glyph: impl IntoElement,
    label: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // zuse window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(glyph)
        .tip(label)
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;

/// Right padding for titlebar content: past the native Windows caption
/// cluster, or past zuse's own Linux caption buttons (10px edge inset +
/// the button row) when the layout puts any on the right.
fn titlebar_right_padding(is_windows: bool, linux_right_captions: usize, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else if linux_right_captions > 0 {
        10.0 + caption_buttons_width(linux_right_captions)
    } else {
        0.0
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(crate::typography::text_xs())
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A Linux caption button in zuse's own cluster style (24px, rounded-6,
/// 16px linear icon). gpui's `WindowControlArea` hit-testing is inert on
/// Linux, so unlike the Windows cluster these carry explicit click handlers
/// (`minimize_window` / `zoom_window` / `remove_window`), the same calls
/// zed's Linux titlebar makes. `occlude` + `prevent_default` keep them out
/// of the drag strip's event surface (see [`window_control_button`]).
fn linux_caption_button(
    id: &'static str,
    icon_path: &'static str,
    close: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (muted, hover_bg, hover_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (theme.text_muted, red, gpui::white())
    } else {
        (theme.text_muted, theme.glass_hover(), theme.text)
    };
    div()
        .id(id)
        // gpui svgs don't inherit the div's text color — recolor the glyph
        // on hover through the group instead (zed's WindowControl idiom).
        .group("linux-caption-button")
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg))
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(
            icon(icon_path)
                .size(px(16.0))
                .text_color(muted)
                .group_hover("linux-caption-button", move |style| {
                    style.text_color(hover_fg)
                }),
        )
}

/// A titlebar history button (zuse window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    label: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, label, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (zuse __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    label: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let glyph = icon(icon_path).size(px(14.0)).text_color(theme.text_muted);
    header_icon_button_with(id, glyph, label, on_click)
}

/// [`header_icon_button`] around a caller-drawn glyph (the morphing sidebar
/// glyph).
fn header_icon_button_with(
    id: &'static str,
    glyph: impl IntoElement,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    header_icon_button_base(id, glyph, on_click).tip(label)
}

fn header_icon_button_base(
    id: &'static str,
    glyph: impl IntoElement,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let fade_key = format!("header-icon-{id}");
    // Reference top bar: `size-6 rounded-sm` with 14px glyphs.
    div()
        .id(id)
        .debug_selector(|| id.into())
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        // zuse __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(glyph)
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_cloud_tab_scope(cx);
        // Every repaint (state frame, input, or the clock firing) reschedules
        // the next clock-driven one; the sidebar and transcript follow this
        // view's notifications.
        let simulator_active = matches!(self.route, Route::Chat) && self.right_pane_shown(cx);
        let selected = self.resolved_right_active(cx);
        for (id, pane) in &self.simulators {
            pane.update(cx, |pane, cx| {
                pane.set_active(
                    simulator_active && selected == RightSurface::Simulator(*id),
                    cx,
                )
            });
        }
        let source_control_active = window.is_window_active();
        let active_review = self.active_main_diff(cx);
        for (id, changes) in &self.diffs {
            changes.update(cx, |changes, cx| {
                changes.set_git_active(
                    source_control_active
                        && (active_review == Some(*id)
                            || (simulator_active && selected == RightSurface::Diff(*id))),
                    cx,
                )
            });
        }
        let active_pr = self.pull_request_pane.active_index();
        let pr_visible = source_control_active && self.pull_request_pane_open();
        for (index, tab) in self.pull_request_pane.tabs.iter().enumerate() {
            tab.page.update(cx, |page, cx| {
                page.set_active(pr_visible && active_pr == Some(index), cx)
            });
        }
        let now = Utc::now();
        let wake = shell_clock_wake(self.state.read(cx), now);
        self.clock
            .arm(wake, now, |shell: &mut Shell| &mut shell.clock, cx);
        let active_files_key = self.panel_key(cx);
        let hidden_explorers = self
            .files
            .iter()
            .filter(|(key, _)| {
                key.as_str() != active_files_key || !matches!(self.route, Route::Chat)
            })
            .map(|(_, files)| files.clone())
            .collect::<Vec<_>>();
        for files in hidden_explorers {
            files.update(cx, |files, cx| files.suspend_tree_interactions(cx));
        }
        settings::wallpaper::preload(cx);
        self.sync_voice_stage(window, cx);
        self.navigation_focus
            .remember(&self.shortcut_focus, window, cx);
        if let Some(path) = self.pending_chip_file.take()
            && !self.active_chat.is_empty()
        {
            let chat = self.active_chat.clone();
            self.open_workspace_file_link(&chat, &path, window, cx);
        }
        if let Some(command) = self.pending_workspace_command.take() {
            use crate::composer::WorkspaceCommand;
            match command {
                WorkspaceCommand::Model => self
                    .conversation_composer(cx)
                    .update(cx, |c, cx| c.open_model_menu(window, cx)),
                WorkspaceCommand::New => self.open_new_session(None, cx),
                WorkspaceCommand::Resume => self.toggle_command_palette(window, cx),
                WorkspaceCommand::Settings => self.open_last_settings(cx),
                WorkspaceCommand::Plugins => self.open_plugins_page(cx),
                WorkspaceCommand::Accounts => self.open_settings(SettingsSection::Harnesses, cx),
                WorkspaceCommand::Diff if !self.active_chat.is_empty() => {
                    self.open_review_tab(window, cx)
                }
                WorkspaceCommand::Files if !self.active_chat.is_empty() => {
                    self.add_files_surface(window, cx)
                }
                WorkspaceCommand::Terminal if !self.active_chat.is_empty() => {
                    self.add_terminal_surface(cx)
                }
                WorkspaceCommand::Rename if !self.active_chat.is_empty() => {
                    self.open_rename_chat(self.active_chat.clone(), cx)
                }
                WorkspaceCommand::Stop => self
                    .conversation_composer(cx)
                    .update(cx, |c, cx| c.interrupt_selected(cx)),
                _ => {}
            }
        }
        self.focus_rename_chat(window, cx);

        self.render_time = Some(self.motion_time());
        if self.all_file_edits_flushed(cx)
            && let Some(action) = self.pending_exit.take()
        {
            let shell = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                if matches!(action, PendingExit::Quit) {
                    crate::app_menus::request_quit(cx);
                } else {
                    shell
                        .update(cx, |shell, cx| match action {
                            PendingExit::CloseWindow => {
                                if shell.prepare_window_close(cx) {
                                    window.remove_window();
                                }
                            }
                            PendingExit::RuntimeChange => shell.quit_for_runtime_change(cx),
                            PendingExit::InstallUpdate(staged) => {
                                shell.apply_staged_update(staged, cx)
                            }
                            PendingExit::Quit => unreachable!(),
                        })
                        .ok();
                }
            });
        }
        crate::transcript::record_view_frame("shell");
        self.observe_viewport_width(f32::from(window.viewport_size().width), cx);
        // Appearance actions persist independently of the shell. Mirror the
        // globals before any later debounced settings save can overwrite them.
        self.settings.appearance = crate::appearance::mode(cx);
        self.settings.theme_selection = crate::appearance::themes(cx);
        self.settings.accent = crate::appearance::accent(cx);
        self.settings.surface = crate::appearance::surface(cx);
        self.pull_settings(cx);
        let theme = Theme::of(cx);
        // The shell frost sits over native desktop blur on macOS and Windows.
        // Content surfaces add their own backgrounds over this shared tint.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let (workspace_scope, auth) = {
            let state = self.state.read(cx);
            (state.workspace_scope, state.auth.clone())
        };
        self.sync_flow = sync_flow_after_auth(self.sync_flow, workspace_scope, auth.as_ref());
        let restart_required = self.sync_flow == SyncFlow::SignedOutRestartRequired;
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        self.sync_browser_profile(cx);
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);
        self.track_horizontal_fit(cx);
        self.suspend_hidden_right_images(cx);
        // After the fit tracking, which starts a fit exit this frame.
        let browser_active = matches!(gate, GatePhase::Ready)
            && !restart_required
            && matches!(self.route, Route::Chat)
            // A pane the fit evicts keeps painting while it slides out.
            && (self.horizontal_fit().right || self.fit_exit_width(1).is_some())
            && (self.right_pane_open(cx) || self.tween_active(self.right_tween));
        // Native clipping follows the animated GPUI mask. Drags only transfer
        // pointer ownership; the browser continues rendering and reflowing.
        let browser_dragging = cx.has_active_drag();
        #[cfg(target_os = "macos")]
        let browser_resize_inset = if self.right_pane_open(cx)
            && !self.right_pane_expanded
            && !self.tween_active(self.right_tween)
        {
            // The browser starts inside the panel's one-point left border.
            px(PANE_RESIZE_HITBOX_HALF_WIDTH - 1.0)
        } else {
            px(0.0)
        };
        #[cfg(any(target_os = "macos", windows))]
        let browser_overlay_width = px(if self.files_visible_width(cx) > 0.0 {
            self.files_visible_width(cx) + PANE_RESIZE_HITBOX_HALF_WIDTH
        } else {
            0.0
        });
        let selected_surface = self.resolved_right_active(cx);
        for (id, browser) in &self.browsers {
            let presentation = crate::browser::model::presentation(
                browser_active && selected_surface == RightSurface::Browser(*id),
                browser_dragging,
            );
            browser.update(cx, |browser, cx| {
                #[cfg(target_os = "macos")]
                browser.set_resize_inset(browser_resize_inset, cx);
                #[cfg(any(target_os = "macos", windows))]
                browser.set_right_occlusion(browser_overlay_width, cx);
                browser.set_shortcuts(&self.settings.keymap);
                browser.set_presentation(presentation, cx);
            });
        }

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Linux CSD: (re-)resolve which caption buttons we draw and on which
        // side — decorations can flip server↔client at runtime and the
        // desktop's button layout is user configuration.
        self.linux_captions = Self::resolve_linux_captions(window, cx);
        if cfg!(target_os = "linux") && self.button_layout_sub.is_none() {
            self.button_layout_sub =
                Some(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        }

        if self.activation_sub.is_none() {
            self.activation_sub = Some(cx.observe_window_activation(
                window,
                |this: &mut Shell, window, cx| {
                    motion::window_activation_changed(window.is_window_active(), cx);
                    if window.is_window_active() {
                        crate::localization::refresh_system(cx);
                    }
                    if window.is_window_active()
                        && let Some(update) = crate::app_update::AppUpdate::global(cx)
                    {
                        // Coming back to the app (often after sleep) checks
                        // at once when a check is due by the wall clock.
                        update.read(cx).poke();
                    }
                    if window.is_window_active() && !this.voice.read(cx).offered() {
                        this.check_voice_codex(true, cx);
                    }
                    if !window.is_window_active() {
                        this.reset_command_palette_key_state();
                        this.set_jump_hints(false, cx);
                        this.composer.update(cx, |composer, cx| {
                            composer.set_queue_shortcut_revealed(false, cx)
                        });
                    }
                },
            ));
        }

        // A live handle can refer to an unmounted element. Recover against
        // the completed frame so newly mounted dialogs can claim focus first.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                let root = this.shortcut_focus.clone();
                let (preferred, unfocused) = this.navigation_focus_fallback(cx);
                window.on_next_frame(move |window, cx| {
                    restore_mounted_focus(&root, &preferred, &unfocused, window, cx);
                });
                cx.notify();
            }));
        }
        if self.settings_restore_pending {
            self.settings_restore_pending = false;
            let target = self
                .settings_return_focus
                .take()
                .unwrap_or_else(|| self.composer.focus_handle(cx));
            window.on_next_frame(move |window, cx| window.focus(&target, cx));
        }
        let shortcut_focus = self.shortcut_focus.clone();
        let (preferred_focus, unfocused) = self.navigation_focus_fallback(cx);
        window.defer(cx, move |window, cx| {
            restore_mounted_focus(&shortcut_focus, &preferred_focus, &unfocused, window, cx);
        });

        // Modifier events follow focus too. Reconcile from the window's input
        // snapshot so a missed release (or pointer event after it) heals hints.
        if !window.is_window_active() {
            self.set_jump_hints(false, cx);
        } else if self.jump_hints {
            // Only a fresh modifier event may turn hints on: activation can
            // retain the snapshot from before Cmd+Tab.
            self.update_jump_hints(&window.modifiers(), cx);
        }

        let root = div()
            .id("shell-root")
            .track_focus(&self.shortcut_focus)
            .child(div().track_focus(&self.unfocused))
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            // Linux CSD: a floating window rounds its corners against the
            // desktop (the window composites with alpha — see
            // `Theme::window_background_appearance`); tiled/maximized keeps
            // square edges flush with the screen. Everything, including the
            // splash and caption chrome, clips to the curve.
            .when(Self::linux_window_floating(window), |el| {
                el.rounded(px(LINUX_WINDOW_CORNER_RADIUS)).overflow_hidden()
            })
            .text_color(text)
            .font(gpui::Font {
                family: font,
                fallbacks: Some(zuse_viewport::language_fonts::fallbacks(
                    crate::localization::localizer(cx).locale(),
                )),
                ..Default::default()
            })
            .text_size(crate::typography::text_base())
            .on_drag_move::<SidebarSessionDrag>(cx.listener(Self::contain_pinned_session_drag))
            .on_drop::<SidebarSessionDrag>(cx.listener(|this, _, _, cx| {
                this.cancel_sidebar_session_transfer(cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.sidebar_session_transfer.is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.sidebar_session_transfer.is_some() {
                        cx.notify();
                    }
                }),
            )
            // Mouse side buttons walk the same history as the titlebar
            // back/forward arrows (the browser surface keeps them to itself).
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            )
            .capture_key_down(cx.listener(Self::on_key_down_capture))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_pull_request_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (zuse __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(
                cx.listener(|this, _: &crate::composer::SetupDictation, _, cx| {
                    this.open_settings(SettingsSection::Voice, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::desktop_import::OpenDesktopImport, window, cx| {
                    this.desktop_import
                        .update(cx, |import, cx| import.open(window, cx));
                },
            ))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| {
                if matches!(this.route, Route::Archives | Route::Usage) {
                    return;
                }
                if matches!(this.route, Route::Chat) {
                    let file = this
                        .active_main_file(cx)
                        .and_then(|id| this.file_surfaces.get(&id).cloned())
                        .or_else(|| match this.resolved_right_active(cx) {
                            RightSurface::File(id) => this.file_surfaces.get(&id).cloned(),
                            _ => None,
                        });
                    if let Some(file) = file {
                        file.update(cx, |file, cx| file.save_active_document(cx));
                    }
                }
            }))
            .on_action(
                cx.listener(|this, _: &settings::plugins::OpenPluginsPage, _, cx| {
                    this.open_plugins_page(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                if !matches!(this.route, Route::Settings(_)) {
                    this.toggle_sidebar(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &RandomWallpaper, _, cx| {
                this.random_wallpaper(cx);
            }))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_card::SendToChat, _, cx| {
                    let text = action.0.clone();
                    this.composer
                        .update(cx, |composer, cx| composer.send_prompt(text, cx));
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_detail::AddPullRequestContext, _, cx| {
                    this.set_route(Route::Chat, cx);
                    stage_prompt(this.composer.clone(), action.0.clone(), None, cx);
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_card::SendContextToChat, _, cx| {
                    let (text, context) = (action.text.clone(), action.context.clone());
                    this.composer.update(cx, |composer, cx| {
                        composer.send_with_context(text, context, cx)
                    });
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_detail::OpenPullRequest, window, cx| {
                    // From a chat (a sidebar badge, the composer's badge), the
                    // pull request opens beside the focused session, so
                    // several can be compared in one conversation.
                    if this.chat_pull_requests_available() {
                        let target = this.chat_pull_request_device(action.1.clone(), cx);
                        this.open_chat_pull_request(action.0.clone(), target, window, cx);
                        return;
                    }
                    let target = if let Some(device) = action.1.clone() {
                        (Some(device.as_str()) != this.state.read(cx).local_device_id.as_deref())
                            .then_some(device)
                    } else if matches!(this.route, Route::PullRequests) {
                        this.pull_requests_page
                            .as_ref()
                            .and_then(|page| page.read(cx).target_device())
                    } else {
                        let state = this.state.read(cx);
                        state.selected_chat_row().and_then(|chat| {
                            (Some(chat.device_id.as_str()) != state.local_device_id.as_deref())
                                .then(|| chat.device_id.clone())
                        })
                    };
                    // Back from a pull request returns to wherever it was
                    // opened from: a chat, the board, or another pull request.
                    this.enter_pull_requests_route(cx);
                    this.nav.push(NavEntry::PullRequest {
                        url: action.0.clone(),
                        device: target.clone(),
                    });
                    this.show_pull_request_detail(action.0.clone(), target, window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::pull_request_detail::ClosePullRequest, _, cx| {
                    if let Some(ix) = this.pull_request_pane.active_index() {
                        this.close_pull_request_tab(ix, cx);
                    }
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_detail::AddPullRequestProject, _, cx| {
                    this.open_add_space_on_device(&action.0, cx);
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::pull_request_detail::StartPullRequestSession, _, cx| {
                    // A handoff must never inherit the sidebar or canvas target.
                    if !this
                        .state
                        .read(cx)
                        .space_row(&action.project_id)
                        .is_some_and(|space| space.device_id == action.device)
                    {
                        this.open_add_space_on_device(&action.device, cx);
                        return;
                    }
                    // On the chat's own pull request the session opens beside it
                    // as a side chat tab: it shares the chat's checkout, which
                    // is already on the pull request's branch. Any other pull
                    // request gets a new session, so checking it out never
                    // moves this chat's branch.
                    if this.pull_request_is_selected_chats(action, cx)
                        && let Some(composer) = this.open_side_chat_for_prompt(cx)
                    {
                        stage_prompt(composer, action.prompt.clone(), action.context.clone(), cx);
                        return;
                    }
                    this.open_new_session(Some(action.project_id.clone()), cx);
                    stage_prompt(
                        this.composer.clone(),
                        action.prompt.clone(),
                        action.context.clone(),
                        cx,
                    );
                },
            ))
            // New session works from anywhere — `open_new_session` routes back
            // to chat itself, so Settings is not a dead spot.
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(None, cx)))
            // Native Settings menu item and the platform convention (Cmd+, on
            // macOS, Ctrl+, elsewhere) toggle the modal from any section.
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.toggle_settings(cx)))
            .on_action(cx.listener(|this, _: &OpenDevices, _, cx| {
                this.open_settings(SettingsSection::Devices, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenCloudSettings, _, cx| {
                this.open_settings(SettingsSection::Cloud, cx)
            }))
            .on_action(cx.listener(|this, command: &DesktopCommand, window, cx| {
                this.desktop_command(command.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextSession, window, cx| {
                this.cycle_navigation(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevSession, window, cx| {
                this.cycle_navigation(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleChanges, window, cx| {
                if matches!(this.route, Route::PullRequests) {
                    this.toggle_pull_request_pane(cx);
                } else if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx);
                    if !this.right_pane_open(cx) {
                        // The hidden editor can retain a focus handle after unmounting.
                        // Restore a mounted target so the next shortcut can reopen it.
                        window.focus(&this.composer.focus_handle(cx), cx);
                    }
                }
            }))
            // The explorer's own toggle (the titlebar tree button): docks or
            // undocks the explorer portion without touching the surface host.
            .on_action(cx.listener(|this, _: &ToggleFiles, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_files_panel(window, cx);
                }
            }))
            // Chat-scoped like the panel toggles: Settings has no current
            // session to archive. Quiet under an open popover, like the other
            // session-nav shortcuts.
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if matches!(this.route, Route::Chat) && !this.overlay_owns_keyboard(cx) {
                    this.archive_selected_chat(cx)
                }
            }))
            // A jump routes back to chat itself, so Settings is not a dead
            // spot — the same call a click on that sidebar row makes. But an
            // open picker/palette owns the keyboard: no jumping underneath
            // it. The MODEL menu advertises these same slots on its rows and
            // this matched binding beats its key handler to the dispatch —
            // forward the slot instead of eating it.
            .on_action(cx.listener(|this, jump: &JumpSession, _, cx| {
                // An open model menu — the main composer's or a side
                // chat's — takes the digit as its model shortcut.
                let pickers = this
                    .open_side_chat_pickers(cx)
                    .unwrap_or_else(|| this.composer.read(cx).pickers().clone());
                let handled = pickers.update(cx, |pickers, cx| pickers.jump_model_slot(jump.0, cx));
                if !handled && !this.overlay_owns_keyboard(cx) {
                    this.jump_to_session(jump.0, cx)
                }
            }))
            .on_modifiers_changed(
                cx.listener(|this, event, _, cx| this.on_modifiers_changed(event, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
                this.toggle_command_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenModelPicker, window, cx| {
                if matches!(this.route, Route::Chat) && !this.overlay_owns_keyboard(cx) {
                    let composer = this.shortcut_conversation(window, cx).2;
                    let pickers = composer.read(cx).pickers().clone();
                    pickers.update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if matches!(this.route, Route::Settings(_)) {
                    return;
                }
                if this.add_space.is_some() {
                    this.close_add_space(cx);
                } else {
                    this.open_add_space(cx);
                }
            }));

        let render_gate = if restart_required {
            GatePhase::Loading
        } else {
            gate.clone()
        };
        let root = match &render_gate {
            GatePhase::Ready => 'ready: {
                // Focus is a sync signal: on the rising edge of window
                // activation, nudge every open room to verify liveness — a
                // broadcast-deaf socket (accepted writes, runtime pongs,
                // nothing delivered; 2026-08-04 incident) then heals within
                // seconds of the user looking at the app rather than waiting
                // out the background probe cadence.
                let window_active = window.is_window_active();
                if window_active && !self.was_window_active {
                    self.state.update(cx, |s, cx| s.probe_sync(cx));
                }
                self.was_window_active = window_active;
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window_active {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `ZUSE_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                let viewport = f32::from(window.viewport_size().width);
                self.viewport_height = f32::from(window.viewport_size().height);
                // Stamped for `right_target` — the expanded changes panel
                // sizes itself to the viewport.
                self.viewport_width = viewport;
                // Settings replaces the whole workspace, sidebar included. The
                // chat layout stays unmounted; its entities keep their state
                // for the return trip.
                if let Route::Settings(section) = self.route {
                    let settings_page = self.render_settings_page(section, window, cx);
                    // A call started from Settings opens its stage here too.
                    let voice_stage = self.render_voice_stage(window, cx);
                    let overlays = self.render_overlays(window.viewport_size(), window, cx);
                    let drag = self.titlebar_drag_region(
                        "settings-titlebar",
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(Theme::TITLEBAR_HEIGHT)),
                        cx,
                    );
                    let page = div()
                        .size_full()
                        .relative()
                        .child(settings_page)
                        .children(voice_stage)
                        .child(drag)
                        .children(overlays);
                    break 'ready root.child(motion::fade_in("phase-app", page));
                }
                // The Pull requests route shares this layout with its own
                // side pane: opened pull requests as tabs beside the board.
                let on_chat = matches!(self.route, Route::Chat);
                let on_pull_requests = matches!(self.route, Route::PullRequests);
                if on_pull_requests && let Some((url, device)) = self.pending_pull_request.take() {
                    self.show_pull_request_detail(url, device, window, cx);
                }
                let right_target_width = if on_chat {
                    self.right_visible_width(cx)
                } else if on_pull_requests {
                    self.pull_request_pane_visible_width()
                } else {
                    0.0
                };
                // The Zot panel replaces the right pane but takes the same room.
                let on_zot = on_chat && self.selected_zot(cx).is_some();
                let right_target_width = if on_zot {
                    if self.zot_ui.panel_hidden {
                        0.0
                    } else {
                        zots::ZOT_PANEL_WIDTH
                    }
                } else {
                    right_target_width
                };
                let main_target_width = self.main_target_width(right_target_width, cx);
                // Navigation can change either edge: Files and responsive
                // sidebar fitting matter just as much as the right surface.
                self.composer_dock
                    .borrow_mut()
                    .observe_conversation(&self.panel_key(cx));
                let panel_handoff = self.composer_dock.borrow_mut().observe_column(
                    self.state.read(cx).selected_chat.is_some(),
                    (self.sidebar_now(), main_target_width),
                    on_chat && !self.reduced_motion,
                    self.render_time.unwrap_or_else(std::time::Instant::now),
                );
                if panel_handoff {
                    self.motion_active.set(true);
                }
                let main_takeover = self.main_takeover_active();
                let main_content_width = self.main_content_width(main_target_width, cx);
                let transcript_width = self.composer_dock.borrow_mut().transcript_width(
                    main_content_width,
                    self.state.read(cx).selected_chat.is_some(),
                    panel_handoff,
                );
                let main_width = (transcript_width - 10.0).max(0.0);
                // Clearance excludes the terminal dock: the transcript
                // viewport ends at the dock's top (see the underlay in
                // `render_main`), so only the chrome above it overlaps.
                let term_h = self.terminal_geometry.get().height;
                let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                let expected_has_composer = {
                    let state = self.state.read(cx);
                    (!state.spaces.is_empty() || state.no_project) && state.selected_chat.is_some()
                };
                let bottom_stack_ready = bottom_stack_measurement_matches(
                    self.bottom_stack_has_composer.get(),
                    expected_has_composer,
                );
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx);
                    if bottom_stack_ready && expected_has_composer {
                        t.set_bottom_clearance(stack_h, cx);
                    }
                });

                let sidebar = self.render_sidebar(cx);
                // A collapsed or fit-hidden sidebar has no seam: the window's
                // own left edge sits there, and a press must resize the window
                // rather than drag the sidebar open.
                let sidebar_handle = (self.sidebar_now() > 0.5).then(|| {
                    self.resize_handle(
                        "sidebar-resize",
                        PaneResizeKind::Sidebar,
                        || SidebarResize,
                        |shell, cx| shell.reset_panel_widths(PaneResizeKind::Sidebar, cx),
                        cx,
                    )
                });
                // A review or commit-diff tab stands in for the conversation.
                let main = match matches!(self.route, Route::Chat)
                    .then(|| self.active_main_tab(cx))
                    .flatten()
                {
                    Some(main_tabs::MainTab::Diff(id)) => self.render_main_diff(id, cx),
                    Some(main_tabs::MainTab::File(id)) => self.render_main_file(id, cx),
                    None => self.render_main(window, main_content_width, transcript_width, cx),
                };
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (zuse __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let right_open = on_chat && !on_zot && self.right_pane_shown(cx);
                // Takeover mode derives its width from the viewport, so a
                // manual drag handle would fight the expanded target.
                let right_handle = (right_open
                    && !panel_handoff
                    && !self.right_pane_expanded
                    && !self.tween_active(self.right_tween))
                .then(|| {
                    self.resize_handle(
                        "right-pane-resize",
                        PaneResizeKind::Right,
                        || RightPaneResize,
                        |shell, cx| shell.reset_panel_widths(PaneResizeKind::Right, cx),
                        cx,
                    )
                    // A forgiving transparent hit target centered on the
                    // seam; the panel's 1px border remains the visual divider.
                    .left(px(-PANE_RESIZE_HITBOX_HALF_WIDTH))
                })
                .or_else(|| {
                    on_pull_requests
                        .then(|| self.pull_request_pane_handle(cx))
                        .flatten()
                });
                let right: AnyElement = if on_zot && self.zot_ui.panel_hidden {
                    Empty.into_any_element()
                } else if on_zot {
                    self.render_zot_inspector(window, cx)
                } else if on_chat {
                    self.render_right_pane(window, cx)
                } else if on_pull_requests {
                    self.render_pull_request_pane(window, cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // Full-window voice stage: above the page chrome, below dialogs.
                let voice_stage = self.render_voice_stage(window, cx);
                let under_stage = self.voice_stage_underlay_opacity(cx);
                // Copied out (not held) — `render_title_bar` needs `cx` mutable.
                let border_color = Theme::of(cx).border;
                // No inset cards (user request): the conversation column sits
                // flush and unbordered, the transcript directly on the frost
                // glass; the changes pane is a flush left-bordered glass panel
                // (built inside `render_right_pane`).
                let main = if main_takeover {
                    div()
                        .h_full()
                        .w(px(main_content_width))
                        .flex_none()
                        .flex()
                        .child(main)
                        .into_any_element()
                } else {
                    main
                };
                // The conversation tabs sit on their own row below the top
                // bar: the column's content moves down by the row's height
                // (its own titlebar clearance then fades the transcript out
                // beneath the row), and the row overlays that band.
                let tab_row = (on_chat && self.conversation_tab_row_visible(cx))
                    .then(|| self.render_conversation_tab_row(cx));
                let card: AnyElement = div()
                    .id("conversation-column")
                    .debug_selector(|| "conversation-column".into())
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .opacity(under_stage)
                    .when(self.workspace_gutter() > 0., |view| {
                        view.rounded(px(zuse_viewport::workspace_style::RADIUS))
                            .bg(Theme::of(cx).panel_bg())
                            .border_1()
                            .border_color(crate::theme::hairline(0.05))
                    })
                    // Flush, the conversation still reads as its own panel:
                    // the content tone against the translucent sidebar,
                    // parted by one hairline.
                    .when(self.workspace_gutter() <= 0., |view| {
                        view.bg(Theme::of(cx).panel_bg())
                            .when(self.sidebar_content_width() > 0., |view| {
                                view.border_l_1().border_color(crate::theme::hairline(0.06))
                            })
                    })
                    .when(tab_row.is_some(), |column| {
                        column.pt(px(top_bar::CONVERSATION_TAB_ROW_HEIGHT))
                    })
                    .child(main)
                    .children(tab_row)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (zuse
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(self.workspace_gutter()))
                    .h_full()
                    .flex_none()
                    .relative()
                    .children(
                        sidebar_handle
                            .map(|handle| handle.left(px(-PANE_RESIZE_HITBOX_HALF_WIDTH))),
                    );
                // Keep the right resize target outside the pane's
                // overflow-hidden width container. This mirrors the sidebar
                // seam and lets the target straddle both adjacent panes.
                // Paint it after the page so page input cannot occlude the
                // inner half. A deferred draw would capture all native input.
                let right_seam: AnyElement = if let Some(handle) = right_handle {
                    div()
                        .w(px(0.0))
                        .h_full()
                        .flex_none()
                        .absolute()
                        .left_0()
                        .top_0()
                        .child(handle)
                        .into_any_element()
                } else {
                    Empty.into_any_element()
                };
                let show_main_chrome = self.main_chrome_visible(cx);
                let title_bar = show_main_chrome.then(|| self.render_title_bar(cx));
                // Sidebar tone: a slightly lighter column behind the sidebar.
                // Its width rides the same tween as the sidebar, so the tone
                // melts away with the collapse instead of vanishing in a frame.
                let sidebar_now = self.sidebar_now();
                let sidebar_tone = Self::sidebar_tone(sidebar_now, border_color, window);
                // The content row spans the FULL window height — the titlebar
                // overlays it (glass, no fill), so the transcript can scroll
                // under the header and fade out at its edge. Columns that
                // must NOT underlap (sidebar content, the changes panel,
                // settings) pad themselves down by the titlebar height.
                let page = div()
                    .size_full()
                    .relative()
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .flex()
                                    .flex_col()
                                    .py(px(self.workspace_gutter()))
                                    .pr(px(self.workspace_gutter()))
                                    .child(card),
                            )
                            // The right pane is ONE container: the surface
                            // host column and the docked explorer column
                            // sit side by side under a shared titlebar
                            // strip; the resize seam straddles its left edge.
                            .child(
                                div()
                                    .h_full()
                                    .flex_none()
                                    .relative()
                                    .opacity(under_stage)
                                    .child(div().h_full().flex().flex_row().child(right))
                                    .child(right_seam),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .opacity(under_stage)
                            .children(title_bar),
                    )
                    // The stage covers the conversation's titlebar but never
                    // the cluster: the sidebar toggle and navigation stay live.
                    .children(voice_stage)
                    .when(
                        self.workspace_gutter() == 0. || self.sidebar_now() < 1.,
                        |view| {
                            view.child(self.render_titlebar_cluster(cx))
                                .children(self.render_sidebar_options_titlebar(cx))
                        },
                    )
                    .child(self.workspace_sidebar_header(cx))
                    .children(overlays);
                let update_card = self.render_harness_update_card(
                    window,
                    f32::from(window.viewport_size().width),
                    cx,
                );
                let root = root.when_some(update_card, |view, card| {
                    view.child(
                        div()
                            .absolute()
                            .top(px(Theme::TITLEBAR_HEIGHT + 12.0))
                            .right(px(16.0))
                            .child(card),
                    )
                });
                root.when(self.workspace_gutter() == 0., |view| {
                    view.child(sidebar_tone)
                })
                .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::OrgGate => {
                let card = self.render_org_gate(cx);
                root.child(card)
            }
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                if matches!(phase, GatePhase::Failed(_)) && self.has_offline_cloud_navigation(cx) {
                    let sidebar = self.render_offline_cloud_sidebar(cx);
                    root.child(
                        div()
                            .size_full()
                            .flex()
                            .child(sidebar)
                            .child(div().flex_1().min_w_0().h_full().child(card)),
                    )
                } else {
                    root.child(card)
                }
            }
        };
        let root = if restart_required {
            let restart = self.render_signed_out_restart(cx);
            root.child(restart)
        } else {
            root
        };
        // Toasts float over every phase and route.
        let root = root.children(crate::toast::render_toaster(window, cx));

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() | motion::state_morphs_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        let root = match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, false, cx.entity_id(), cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                root.child(loaders::splash_overlay(&theme, true, cx.entity_id(), cx))
            }
            SplashPhase::Gone => root,
        };
        // Onboarding fills the window below the caption controls and drag strip.
        let onboarding = self.onboarding.read(cx).visible();
        let root = if onboarding {
            root.child(self.onboarding.clone())
        } else {
            root
        };

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above the splash and every auth/org/error gate as well as
        // the full application. Gate pages also need a drag surface because
        // they do not render the unified tabs/settings titlebar — on Windows
        // the native `Drag` control area, on Linux the explicit
        // `start_window_move` strip (the control-area hit-test is inert
        // there); macOS drags gate windows natively.
        let root = if (!restart_required && matches!(gate, GatePhase::Ready) && !onboarding)
            || cfg!(target_os = "macos")
        {
            root
        } else {
            root.child(
                self.titlebar_drag_region(
                    "gate-titlebar-drag",
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(Theme::TITLEBAR_HEIGHT)),
                    cx,
                ),
            )
        };
        let root = root
            .children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx))
            // Last so the invisible CSD resize strips sit above every other
            // element at the window edges.
            .children(Self::render_linux_resize_borders(window));
        self.render_time = None;
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn chat_with_path(
        cwd: Option<&str>,
        source: Option<(&str, &str)>,
    ) -> zuse_protocol::Chat {
        zuse_protocol::Chat {
            id: "chat".into(),
            device_id: "remote-device".into(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_owned),
            branch: None,
            checkout_id: None,
            source_context: source.map(|(source_cwd, repo_root)| {
                zuse_protocol::ConversationSourceContext {
                    checkout_id: "checkout".into(),
                    repo_root: repo_root.into(),
                    cwd: source_cwd.into(),
                    branch: "main".into(),
                    head_sha: None,
                    observed_at: chrono::Utc::now(),
                }
            }),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            parent_chat_id: None,
            thread_id: None,
        }
    }

    #[test]
    fn copy_path_copies_the_chat_cwd_not_the_canonical_repo_root() {
        let chat = chat_with_path(
            Some("/remote/repo/packages/app"),
            Some(("/remote/repo/packages/app", "/remote/repo")),
        );
        assert_eq!(chat_copy_path(&chat), Some("/remote/repo/packages/app"));

        let windows = chat_with_path(
            Some(r"C:\Users\me\repo"),
            Some((r"C:\Users\me\repo", r"\\?\C:\Users\me\repo")),
        );
        assert_eq!(chat_copy_path(&windows), Some(r"C:\Users\me\repo"));
    }

    #[test]
    fn copy_path_accepts_host_absolute_paths_from_any_os() {
        for cwd in [
            "/home/me/repo",
            r"C:\Users\me\repo",
            "D:/work/repo",
            r"\\server\share\repo",
        ] {
            let chat = chat_with_path(Some(cwd), None);
            assert_eq!(chat_copy_path(&chat), Some(cwd));
        }
    }

    #[test]
    fn copy_path_is_unavailable_without_an_absolute_path() {
        for cwd in [
            None,
            Some(""),
            Some("  "),
            Some("~"),
            Some("~/repo"),
            Some("."),
            Some("C:"),
        ] {
            let chat = chat_with_path(cwd, None);
            assert_eq!(chat_copy_path(&chat), None);
        }
    }

    #[test]
    fn sidebar_sync_status_waits_for_grace_or_queued_changes() {
        use zuse_protocol::{ChatConnectivity, ChatSyncState as S};

        let mut chat = ChatConnectivity {
            chat_id: "remote".into(),
            sync_state: S::Local,
            connected: false,
            delivery_live: false,
            pending_pushes: 0,
        };
        assert_eq!(chat_sync_pill_caption(&chat), None);

        for state in [S::Waiting, S::Connecting, S::Offline] {
            chat.sync_state = state;
            chat.connected = true;
            assert_eq!(chat_sync_pill_caption(&chat), None, "transient {state:?}");
        }

        chat.sync_state = S::Waiting;
        chat.connected = false;
        assert_eq!(
            chat_sync_pill_caption(&chat),
            Some("Sync queued — changes are saved")
        );
        chat.sync_state = S::Connecting;
        assert_eq!(chat_sync_pill_caption(&chat), Some("Syncing…"));
        chat.sync_state = S::Offline;
        assert_eq!(
            chat_sync_pill_caption(&chat),
            Some("Offline — changes are saved")
        );

        // Real pending pushes remain visible even with a live room.
        chat.connected = true;
        chat.pending_pushes = 1;
        chat.sync_state = S::Waiting;
        assert_eq!(
            chat_sync_pill_caption(&chat),
            Some("Sync queued — changes are saved")
        );
        chat.sync_state = S::Connecting;
        assert_eq!(chat_sync_pill_caption(&chat), Some("Syncing…"));
    }

    #[test]
    fn sidebar_drag_nudges_each_edge_once_until_rearmed() {
        let min = sidebar_drag_sample(SIDEBAR_MIN, None, false);
        assert_eq!(min.width, SIDEBAR_MIN);
        assert_eq!(min.edge, Some(motion::ResizeEdge::Min));
        assert!(min.starts_bounce);

        let held_min = sidebar_drag_sample(SIDEBAR_MIN - 80.0, min.edge, false);
        assert_eq!(held_min.width, SIDEBAR_MIN);
        assert_eq!(held_min.edge, min.edge);
        assert!(!held_min.starts_bounce);

        let inside = sidebar_drag_sample(SIDEBAR_MIN + 1.0, held_min.edge, false);
        assert_eq!(inside.edge, None);
        assert!(!inside.starts_bounce);

        let rearmed_min = sidebar_drag_sample(SIDEBAR_MIN - 1.0, inside.edge, false);
        assert!(rearmed_min.starts_bounce);

        let max = sidebar_drag_sample(SIDEBAR_MAX, rearmed_min.edge, false);
        assert_eq!(max.width, SIDEBAR_MAX);
        assert_eq!(max.edge, Some(motion::ResizeEdge::Max));
        assert!(max.starts_bounce);

        let held_max = sidebar_drag_sample(SIDEBAR_MAX + 80.0, max.edge, false);
        assert_eq!(held_max.width, SIDEBAR_MAX);
        assert!(!held_max.starts_bounce);
    }

    #[test]
    fn sidebar_drag_stays_exact_in_range_and_reduced_motion_never_nudges() {
        let inside = (SIDEBAR_MIN + SIDEBAR_MAX) / 2.0;
        let middle = sidebar_drag_sample(inside, None, false);
        assert_eq!(middle.width, inside);
        assert_eq!(middle.edge, None);
        assert!(!middle.starts_bounce);

        for pointer_x in [
            SIDEBAR_MIN - 100.0,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            SIDEBAR_MAX + 100.0,
        ] {
            let sample = sidebar_drag_sample(pointer_x, None, true);
            assert!((SIDEBAR_MIN..=SIDEBAR_MAX).contains(&sample.width));
            assert!(!sample.starts_bounce);
        }
    }

    #[test]
    fn right_pane_uses_the_shared_clamp_and_edge_latch() {
        let min =
            motion::resize_drag_sample(RIGHT_PANE_MIN - 40.0, RIGHT_PANE_MIN, 820.0, None, false);
        assert_eq!(min.width, RIGHT_PANE_MIN);
        assert_eq!(min.edge, Some(motion::ResizeEdge::Min));
        assert!(min.starts_bounce);

        let held = motion::resize_drag_sample(
            RIGHT_PANE_MIN - 80.0,
            RIGHT_PANE_MIN,
            820.0,
            min.edge,
            false,
        );
        assert!(!held.starts_bounce);

        let max = motion::resize_drag_sample(900.0, RIGHT_PANE_MIN, 820.0, None, false);
        assert_eq!(max.width, 820.0);
        assert_eq!(max.edge, Some(motion::ResizeEdge::Max));
        assert!(max.starts_bounce);
    }

    #[test]
    fn sidebar_bounce_has_rounded_out_and_return_phases() {
        assert_eq!(
            motion::resize_bounce_offset(motion::ResizeEdge::Max, 0.0),
            0.0
        );
        assert_eq!(
            motion::resize_bounce_offset(
                motion::ResizeEdge::Max,
                motion::RESIZE_EDGE_BOUNCE_OUT_FRACTION
            ),
            motion::RESIZE_EDGE_NUDGE
        );
        assert_eq!(
            motion::resize_bounce_offset(motion::ResizeEdge::Max, 1.0),
            0.0
        );

        let gentle_start = motion::resize_bounce_offset(motion::ResizeEdge::Max, 0.01);
        let outbound = motion::resize_bounce_offset(motion::ResizeEdge::Max, 0.2);
        let returning = motion::resize_bounce_offset(motion::ResizeEdge::Max, 0.7);
        assert!(gentle_start > 0.0 && gentle_start < 0.1);
        assert!(outbound > gentle_start && outbound < motion::RESIZE_EDGE_NUDGE);
        assert!(returning > 0.0 && returning < motion::RESIZE_EDGE_NUDGE);
        assert_eq!(
            motion::resize_bounce_offset(motion::ResizeEdge::Min, 0.2),
            -outbound
        );
    }

    #[test]
    fn every_default_shortcut_binds_on_this_platform() {
        // `apply_keymap` silently falls back on an unparseable combo, so a
        // default gpui cannot parse would ship as a dead shortcut.
        for id in crate::settings::ShortcutId::ALL {
            let combo = platform_combo(id.default_combo());
            if combo.is_empty() {
                continue;
            }
            assert!(
                Keystroke::parse(&combo).is_ok(),
                "{} default {combo:?} does not parse",
                id.label()
            );
        }
    }

    #[test]
    fn island_stays_centered_on_controls_while_expanding() {
        let center = (Theme::TITLEBAR_HEIGHT + Theme::TITLEBAR_TOP_PAD) * 0.5;
        for step in 0..=20 {
            let (top, height) = titlebar_island_vertical_geometry(step as f32 / 20.0);
            assert_eq!(top + height * 0.5, center);
            assert!((28.0..=32.0).contains(&height));
        }
        let (top, height) = titlebar_island_vertical_geometry(1.0);
        assert_eq!(center, 21.0);
        assert_eq!(center - 12.0 - top, 4.0);
        assert_eq!(top + height - (center + 12.0), 4.0);
    }

    #[test]
    fn new_thread_handoff_is_continuous_and_staged() {
        assert!(bottom_stack_measurement_matches(false, false));
        assert!(bottom_stack_measurement_matches(true, true));
        assert!(!bottom_stack_measurement_matches(false, true));
        assert!(!bottom_stack_measurement_matches(true, false));
        assert_eq!(new_thread_background_opacity(false), 1.0);
        assert_eq!(
            new_thread_background_opacity(true),
            NEW_THREAD_BACKGROUND_FROSTED_OPACITY
        );
        assert_eq!(new_thread_background_height(400.0), 288.0);
        assert!((new_thread_background_height(600.0) - 432.0).abs() < 0.001);
        assert_eq!(new_thread_background_height(1_000.0), 720.0);
        assert_eq!(new_thread_background_height(1_200.0), 760.0);
        assert!(new_thread_background_height(848.0) > 848.0 / 2.0);
    }

    #[test]
    fn right_pane_ceiling_preserves_the_chat_floor() {
        assert_eq!(right_pane_max_width(1200.0, 256.0, CHAT_PANEL_MIN), 644.0);
        assert_eq!(1200.0 - 256.0 - 644.0, CHAT_PANEL_MIN);
        // The chat floor wins over the right pane's preferred 360px minimum
        // when the whole window is unusually narrow.
        assert_eq!(right_pane_max_width(800.0, 256.0, CHAT_PANEL_MIN), 244.0);
        assert_eq!(800.0 - 256.0 - 244.0, CHAT_PANEL_MIN);
    }

    #[test]
    fn right_pane_takeover_consumes_the_chat_column() {
        assert_eq!(right_pane_takeover_width(1200.0, 256.0), 944.0);
        assert_eq!(1200.0 - 256.0 - 944.0, 0.0);
    }

    #[test]
    fn escape_interrupts_only_the_active_live_chat() {
        assert_eq!(
            resolve_shell_escape(
                "escape",
                false,
                true,
                Route::Chat,
                Some("chat-a"),
                Indicator::Working,
                false,
            ),
            ShellEscapeOutcome::InterruptChat("chat-a".to_owned())
        );
        assert_eq!(
            resolve_shell_escape(
                "escape",
                false,
                true,
                Route::Chat,
                Some("chat-b"),
                Indicator::AwaitingInput,
                false,
            ),
            ShellEscapeOutcome::InterruptChat("chat-b".to_owned())
        );
    }

    /// A minute boundary.
    fn clock_epoch() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn clock_session(
        chat_id: &str,
        status: zuse_protocol::SessionStatus,
        updated_at: DateTime<Utc>,
    ) -> zuse_protocol::Session {
        zuse_protocol::Session {
            last_completed_turn: None,
            chat_id: chat_id.into(),
            device_id: "remote".into(),
            status,
            started_at: Some(updated_at),
            updated_at,
            running_subagents: 0,
            awaiting_input: None,
            background_tasks: Vec::new(),
        }
    }

    struct ClockProbe {
        clock: ClockRedraw,
    }

    fn clock_probe(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<ClockProbe>,
        std::rc::Rc<std::cell::Cell<usize>>,
        gpui::Subscription,
    ) {
        use gpui::AppContext as _;
        let probe = cx.new(|_| ClockProbe {
            clock: ClockRedraw::default(),
        });
        let redraws = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = redraws.clone();
        let subscription =
            cx.update(|cx| cx.observe(&probe, move |_, _| counter.set(counter.get() + 1)));
        (probe, redraws, subscription)
    }

    fn arm_probe(
        cx: &mut gpui::TestAppContext,
        probe: &gpui::Entity<ClockProbe>,
        wake: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        probe.update(cx, |probe, cx| {
            probe
                .clock
                .arm(wake, now, |probe: &mut ClockProbe| &mut probe.clock, cx)
        });
    }

    /// Schedules the shell clock for `state` at `now` and proves the redraw
    /// lands exactly at `expected` on the simulated clock — not a millisecond
    /// earlier, and without any intermediate polling.
    fn assert_redraw_at(
        cx: &mut gpui::TestAppContext,
        state: &AppState,
        now: DateTime<Utc>,
        expected: DateTime<Utc>,
    ) {
        let wake = shell_clock_wake(state, now);
        assert_eq!(wake, expected);
        let (probe, redraws, _subscription) = clock_probe(cx);
        arm_probe(cx, &probe, wake, now);
        let delay = (expected - now).to_std().unwrap();
        cx.executor()
            .advance_clock(delay - Duration::from_millis(1));
        assert_eq!(redraws.get(), 0, "no redraw before the deadline");
        cx.executor().advance_clock(Duration::from_millis(1));
        assert_eq!(redraws.get(), 1, "one redraw at the deadline");
    }

    #[gpui::test]
    fn idle_shell_sleeps_until_the_minute(cx: &mut gpui::TestAppContext) {
        let now = clock_epoch() + chrono::TimeDelta::seconds(20);
        let mut state = AppState::new();
        // A failed run, a pending question and a reconnecting pill show no
        // clock: only the relative-time minute remains.
        state.sessions = vec![
            clock_session("failed", zuse_protocol::SessionStatus::Errored, now),
            clock_session("idle", zuse_protocol::SessionStatus::Idle, now),
        ];
        state.selected_chat = Some("failed".into());
        state.connectivity.state = zuse_protocol::ConnectivityState::Reconnecting;
        assert_redraw_at(
            cx,
            &state,
            now,
            clock_epoch() + chrono::TimeDelta::minutes(1),
        );
    }

    #[gpui::test]
    fn selected_working_chat_redraws_every_second(cx: &mut gpui::TestAppContext) {
        let now = clock_epoch();
        let mut state = AppState::new();
        state.sessions = vec![clock_session(
            "chat",
            zuse_protocol::SessionStatus::Working,
            now,
        )];
        state.selected_chat = Some("chat".into());
        assert_redraw_at(cx, &state, now, now + chrono::TimeDelta::seconds(1));
        // Awaiting input shows no elapsed timer; its staleness cutoff is an
        // AppState notification, not a shell tick.
        state.sessions[0].status = zuse_protocol::SessionStatus::AwaitingInput;
        assert_redraw_at(cx, &state, now, now + chrono::TimeDelta::minutes(1));
    }

    #[gpui::test]
    fn clock_redraw_keeps_the_earliest_deadline(cx: &mut gpui::TestAppContext) {
        let now = clock_epoch();
        let (probe, redraws, _subscription) = clock_probe(cx);
        arm_probe(cx, &probe, now + chrono::TimeDelta::seconds(10), now);
        // An earlier deadline replaces the pending wake; a later one does not.
        arm_probe(cx, &probe, now + chrono::TimeDelta::seconds(5), now);
        arm_probe(cx, &probe, now + chrono::TimeDelta::seconds(30), now);
        cx.executor().advance_clock(Duration::from_secs(5));
        assert_eq!(redraws.get(), 1);
        // Fired timers are one-shot: nothing else is pending.
        cx.executor().advance_clock(Duration::from_secs(60));
        assert_eq!(redraws.get(), 1);
        assert!(probe.read_with(cx, |probe, _| probe.clock.task.is_none()));
    }

    #[test]
    fn escape_ignores_non_live_or_ineligible_views() {
        assert_eq!(
            resolve_shell_escape(
                "escape",
                true,
                true,
                Route::Chat,
                Some("chat-a"),
                Indicator::Working,
                false,
            ),
            ShellEscapeOutcome::Blocked
        );
        for (route, selected, indicator, interrupting) in [
            (Route::Chat, Some("chat-a"), Indicator::None, false),
            (Route::Chat, None, Indicator::Working, false),
            (Route::Chat, Some("chat-a"), Indicator::Working, true),
            (
                Route::Settings(SettingsSection::Devices),
                Some("chat-a"),
                Indicator::Working,
                false,
            ),
        ] {
            assert_eq!(
                resolve_shell_escape(
                    "escape",
                    false,
                    true,
                    route,
                    selected,
                    indicator,
                    interrupting,
                ),
                ShellEscapeOutcome::Ignored
            );
        }
        assert_eq!(
            resolve_shell_escape(
                "enter",
                true,
                true,
                Route::Chat,
                Some("chat-a"),
                Indicator::Working,
                false,
            ),
            ShellEscapeOutcome::OtherKey
        );
    }

    #[test]
    fn escape_interrupt_is_opt_in() {
        assert_eq!(
            resolve_shell_escape(
                "escape",
                false,
                false,
                Route::Chat,
                Some("chat-a"),
                Indicator::Working,
                false,
            ),
            ShellEscapeOutcome::Ignored
        );
    }

    #[test]
    fn only_visible_sync_steps_block_escape() {
        assert!(!SyncFlow::Idle.has_visible_overlay());
        assert!(!SyncFlow::SwitchOffer { notice_open: false }.has_visible_overlay());
        assert!(SyncFlow::SwitchOffer { notice_open: true }.has_visible_overlay());
        assert!(SyncFlow::Importing { done: 1, total: 3 }.has_visible_overlay());
        assert!(SyncFlow::SignOutConfirm.has_visible_overlay());
    }

    #[test]
    fn right_pane_takeover_control_reverses_direction() {
        assert_eq!(tabs::right_pane_expand_icon(false), icons::EXPAND_ARROWS);
        assert_eq!(tabs::right_pane_expand_icon(true), icons::COLLAPSE_ARROWS);
    }

    #[test]
    fn pane_resize_hitboxes_yield_the_titlebar_chrome() {
        assert_eq!(PANE_RESIZE_HITBOX_TOP, Theme::TITLEBAR_HEIGHT);
        assert_eq!(PANE_RESIZE_HITBOX_HALF_WIDTH * 2.0, 20.0);
        assert_eq!(TERMINAL_RESIZE_HITBOX_HEIGHT, 10.0);
    }

    #[test]
    fn new_session_action_lives_in_the_titlebar_only_when_useful() {
        assert_eq!(titlebar_new_session_alpha(&Route::Chat, true), 1.0);
        assert_eq!(titlebar_new_session_alpha(&Route::Chat, false), 0.0);
        assert_eq!(titlebar_new_session_alpha(&Route::PullRequests, false), 1.0);
        assert_eq!(titlebar_new_session_alpha(&Route::PullRequests, true), 1.0);
        assert_eq!(
            titlebar_new_session_alpha(&Route::Settings(SettingsSection::Devices), true),
            0.0
        );
        assert_eq!(
            titlebar_new_session_alpha(&Route::Settings(SettingsSection::Devices), false),
            0.0
        );
    }

    #[test]
    fn composer_width_shares_the_maximum_only_in_established_threads() {
        for (setting, outer) in [(560.0, 584.0), (736.0, 760.0), (1200.0, 1224.0)] {
            assert_eq!(composer_target_width(1600.0, setting, true), outer);
            assert_eq!(composer_target_width(1600.0, setting, false), 768.0);
            // Narrow panes keep the composer's original gutters and usable width.
            assert_eq!(composer_target_width(500.0, setting, true), 500.0);
            assert_eq!(composer_target_width(500.0, setting, false), 500.0);
            assert_eq!(composer_target_width(0.0, setting, true), 0.0);
        }
    }

    #[test]
    fn default_composer_width_preserves_main_resizing_and_many_attachment_rows() {
        for panel_width in (0..=1600).step_by(8) {
            let panel_width = panel_width as f32;
            assert_eq!(
                composer_target_width(panel_width, settings::TRANSCRIPT_WIDTH_DEFAULT, true),
                panel_width.min(
                    settings::TRANSCRIPT_WIDTH_DEFAULT + 2.0 * crate::composer::COMPOSER_OUTER_PAD
                ),
                "default width must preserve main's responsive layout at {panel_width}px"
            );
        }
        // At the same 300px pane width, main fits three thumbnails per row.
        // Applying transcript gutters reduced this to two and turned 60 images
        // from a 1284px strip into a 1924px strip, pushing controls off-screen.
        for setting in [560.0, 736.0, 1200.0] {
            let width = composer_target_width(300.0, setting, true);
            let inner = width - 2.0 * Theme::SPACE_LG - 2.0;
            for (count, expected_height) in [(3, 68.0), (60, 1284.0), (120, 2564.0)] {
                assert_eq!(
                    crate::composer::attachment_strip_height(count, inner),
                    expected_height
                );
            }
        }
    }

    #[test]
    fn right_panel_content_keeps_the_larger_width_only_during_transition() {
        assert_eq!(right_panel_content_width(520.0, None, None), 520.0);
        assert_eq!(
            right_panel_content_width(0.0, Some((520.0, 0.0)), None),
            520.0
        );
        assert_eq!(
            right_panel_content_width(760.0, Some((520.0, 760.0)), None),
            760.0
        );
        assert_eq!(
            right_panel_content_width(1064.0, Some((520.0, 1064.0)), Some(760.0)),
            760.0
        );

        let conversation = conversation_width(1320.0, 256.0, 520.0);
        let takeover = conversation_width(1320.0, 256.0, 1064.0);
        assert_eq!(conversation, 544.0);
        assert_eq!(takeover, 0.0);
        assert_eq!(
            stable_panel_content_width(takeover, Some((conversation, takeover))),
            conversation
        );
        assert_eq!(
            stable_panel_content_width(conversation, Some((takeover, conversation))),
            conversation
        );
    }

    #[tokio::test]
    async fn remote_shutdown_waits_for_ipc_release() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(listener);
        });

        wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
            .await
            .unwrap();
        release.await.unwrap();
    }

    #[tokio::test]
    async fn signed_out_synced_runtime_stops_and_reboots_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session.json"),
            r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let boot = EngineBootConfig {
            target: None,
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: zuse_protocol::HarnessId::Mock,
        };
        let synced = crate::state::EngineHandle::bootstrap(boot.clone())
            .await
            .expect("saved session opens its synced profile");
        assert_eq!(synced.engine_info().workspace_scope, WorkspaceScope::Synced);

        synced
            .client()
            .call(methods::SIGN_OUT, serde_json::json!({}))
            .await
            .expect("sign out clears credentials");
        stop_synced_runtime(synced, port, dir.path())
            .await
            .expect("synced runtime drains and releases ownership");

        assert!(!dir.path().join("session.json").exists());
        let local = crate::state::EngineHandle::bootstrap(boot)
            .await
            .expect("same process can continue locally");
        assert_eq!(local.engine_info().workspace_scope, WorkspaceScope::Local);
        local.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_shutdown_waits_for_engine_lock_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let lock_released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_by_task = lock_released.clone();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(lock);
            released_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_secs(2))
            .await
            .unwrap();
        assert!(lock_released.load(std::sync::atomic::Ordering::SeqCst));
        release.await.unwrap();
    }

    #[tokio::test]
    async fn remote_shutdown_times_out_while_ipc_remains_open() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let error = wait_for_remote_engine_shutdown(port, dir.path(), Duration::from_millis(100))
            .await
            .unwrap_err();

        assert!(error.contains("did not finish stopping"));
        drop(listener);
    }

    #[test]
    fn account_actions_follow_the_attached_workspace_scope() {
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Local), SyncFlow::Idle),
            Some(AccountMenuAction::EnableSync)
        );
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
            Some(AccountMenuAction::SignOut)
        );
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Development), SyncFlow::Idle),
            None
        );
    }

    #[test]
    fn sidebar_footer_names_the_account_or_falls_back_to_local() {
        let named = zuse_protocol::UserProfile {
            id: "u".into(),
            email: "wing@example.com".into(),
            name: Some("Wing".into()),
        };
        let unnamed = zuse_protocol::UserProfile {
            name: Some("  ".into()),
            ..named.clone()
        };
        let label = |scope, user| sidebar_account_identity(scope, SyncFlow::Idle, user).0;
        assert_eq!(label(Some(WorkspaceScope::Synced), Some(&named)), "Wing");
        assert_eq!(
            label(Some(WorkspaceScope::Synced), Some(&unnamed)),
            "wing@example.com"
        );
        assert_eq!(label(None, None), "Local");
        assert_eq!(label(Some(WorkspaceScope::Synced), None), "Local");
        // Local-only storage stays "Local" even with a cached profile.
        assert_eq!(label(Some(WorkspaceScope::Local), Some(&named)), "Local");
    }

    #[test]
    fn local_sign_in_offers_the_in_place_switch() {
        let signed_in = AuthState::SignedIn {
            user: zuse_protocol::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };

        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Enabling,
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: true }
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Idle,
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: true },
            "another viewport derives the pending switch from AuthStatus"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SwitchOffer { notice_open: false },
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::SwitchOffer { notice_open: false },
            "shared auth updates do not reopen a postponed wizard"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::RestartPending { notice_open: false },
                Some(WorkspaceScope::Local),
                Some(&signed_in),
            ),
            SyncFlow::RestartPending { notice_open: false },
            "the quit fallback survives shared auth updates too"
        );
        assert_eq!(
            account_menu_action(
                Some(WorkspaceScope::Local),
                SyncFlow::SwitchOffer { notice_open: false },
            ),
            Some(AccountMenuAction::RestartPending)
        );
        for notice_open in [true, false] {
            assert_eq!(
                sync_flow_after_auth(
                    SyncFlow::SwitchOffer { notice_open },
                    Some(WorkspaceScope::Local),
                    Some(&AuthState::SignedOut),
                ),
                SyncFlow::Idle,
                "revoked credentials cancel the pending switch"
            );
        }
    }

    #[test]
    fn import_summary_errors_are_a_failure_not_a_success() {
        // Clean summary → done with counts.
        let clean = serde_json::json!({
            "kind": "summary", "importedChats": 2, "skippedChats": 1, "errors": []
        });
        assert_eq!(import_summary_outcome(&clean), Ok((2, 1)));

        // Any error means the wizard must NOT say "all set" — partial
        // migrations surface as an explicit failure with the first cause.
        let partial = serde_json::json!({
            "kind": "summary", "importedChats": 1, "skippedChats": 0,
            "errors": ["chat c2: journal copy failed"]
        });
        let message = import_summary_outcome(&partial).expect_err("errors must fail");
        assert!(message.contains("journal copy failed"), "{message}");
        assert!(message.contains("1 imported"), "{message}");

        let many = serde_json::json!({
            "kind": "summary", "importedChats": 0, "skippedChats": 0,
            "errors": ["a", "b", "c"]
        });
        let message = import_summary_outcome(&many).expect_err("errors must fail");
        assert!(message.contains("3 failures"), "{message}");

        // A summary missing the errors field entirely (older engine) is
        // treated as clean rather than failing every import.
        let legacy = serde_json::json!({ "kind": "summary", "importedChats": 4 });
        assert_eq!(import_summary_outcome(&legacy), Ok((4, 0)));
    }

    #[test]
    fn spaces_only_local_work_still_gets_the_import_offer() {
        assert_eq!(local_work_phrase(0, 0), None, "nothing to bring");
        assert_eq!(local_work_phrase(2, 0).as_deref(), Some("the 2 sessions"));
        assert_eq!(
            local_work_phrase(0, 1).as_deref(),
            Some("the 1 project"),
            "a projects-only profile must be offered the import, not a bare switch"
        );
        assert_eq!(
            local_work_phrase(1, 2).as_deref(),
            Some("the 1 session and 2 projects")
        );
    }

    #[test]
    fn dismissed_import_failure_stays_reachable_on_a_synced_runtime() {
        let signed_in = AuthState::SignedIn {
            user: zuse_protocol::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };

        // "Later" postpones the failure notice; it must not evaporate.
        let dismissed = SyncFlow::ImportFailed { notice_open: false };
        assert_eq!(
            sync_flow_after_auth(dismissed, Some(WorkspaceScope::Synced), Some(&signed_in)),
            dismissed,
            "a postponed import failure survives auth/scope updates"
        );

        // …and the account menu on the SYNCED runtime still exposes the
        // re-entry point. This is the whole point: after the switch there is
        // no local runtime left to re-derive an offer from, so this menu row
        // is the only path back to the retry dialog.
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), dismissed),
            Some(AccountMenuAction::RestartPending),
            "retry must remain reachable after dismissal"
        );
        assert_eq!(
            account_menu_action(
                Some(WorkspaceScope::Synced),
                SyncFlow::ImportFailed { notice_open: true },
            ),
            Some(AccountMenuAction::RestartPending)
        );

        // Resolving the failure restores the normal synced menu.
        assert_eq!(
            account_menu_action(Some(WorkspaceScope::Synced), SyncFlow::Idle),
            Some(AccountMenuAction::SignOut)
        );
    }

    #[test]
    fn switch_lifecycle_survives_the_runtime_replacement_window() {
        let signed_in = AuthState::SignedIn {
            user: zuse_protocol::UserProfile {
                id: "user-1".into(),
                email: "user@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        };
        for flow in [
            SyncFlow::Switching { import: true },
            SyncFlow::Importing { done: 1, total: 3 },
            SyncFlow::ImportDone {
                imported: 3,
                skipped: 0,
            },
            SyncFlow::ImportFailed { notice_open: true },
            SyncFlow::ImportFailed { notice_open: false },
        ] {
            // Local (before the stop), detached (mid-replacement), and synced
            // (replacement runtime up): the driver owns these states — auth
            // and scope edges must never reset them.
            assert_eq!(
                sync_flow_after_auth(flow, Some(WorkspaceScope::Local), Some(&signed_in)),
                flow
            );
            assert_eq!(sync_flow_after_auth(flow, None, None), flow);
            assert_eq!(
                sync_flow_after_auth(flow, Some(WorkspaceScope::Synced), Some(&signed_in)),
                flow
            );
        }
    }

    #[test]
    fn synced_sign_out_blocks_every_viewport_and_cannot_switch_accounts() {
        let signed_in_as_another_user = AuthState::SignedIn {
            user: zuse_protocol::UserProfile {
                id: "user-2".into(),
                email: "other@example.com".into(),
                name: None,
            },
            org_id: Some("org-2".into()),
        };

        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SigningOut,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            SyncFlow::SignedOutRestartRequired,
            "the viewport that requested sign-out is blocked by AuthStatus"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::Idle,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            SyncFlow::SignedOutRestartRequired,
            "another viewport observing the same runtime is also blocked"
        );
        assert_eq!(
            sync_flow_after_auth(
                SyncFlow::SignedOutRestartRequired,
                Some(WorkspaceScope::Synced),
                Some(&signed_in_as_another_user),
            ),
            SyncFlow::SignedOutRestartRequired,
            "new credentials cannot reopen the previous account's store"
        );
    }

    #[test]
    fn titlebar_cluster_matches_zuse_window_controls() {
        // zuse window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
        assert_eq!(TITLEBAR_CONTROL_GAP, 2.0);
        assert_eq!(TITLEBAR_GROUP_GAP, Theme::SPACE_SM);
        assert_eq!(TITLEBAR_IDENTITY_GAP, Theme::SPACE_MD);
        assert_eq!(CLUSTER_BUTTONS_WIDTH, 82.0);
        assert_eq!(TITLEBAR_ACTION_SLOT_WIDTH, 32.0);
        assert_eq!(TITLEBAR_ACTION_EDGE_INSET, 6.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
        assert_eq!(
            TITLEBAR_CLUSTER_PAD + titlebar_spacer_width(true, false, TITLEBAR_CLUSTER_PAD),
            titlebar_cluster_start(false),
            "the rendered row padding and spacer must land on the declared cluster start"
        );
    }

    #[test]
    fn windows_caption_controls_reserve_titlebar_space() {
        assert_eq!(titlebar_right_padding(true, 0, 16.0), 124.0);
        assert_eq!(titlebar_right_padding(false, 0, 16.0), 16.0);
    }

    #[test]
    fn linux_caption_controls_reserve_titlebar_space() {
        // 24px buttons on the cluster's 2px rhythm.
        assert_eq!(caption_buttons_width(0), 0.0);
        assert_eq!(caption_buttons_width(1), 24.0);
        assert_eq!(caption_buttons_width(3), 76.0);
        // Right-side captions (the Linux default: minimize,maximize,close):
        // content pads past the 10px edge inset + the button row.
        assert_eq!(titlebar_right_padding(false, 3, 16.0), 16.0 + 10.0 + 76.0);
        // GNOME-vanilla ":close" — a single right button.
        assert_eq!(titlebar_right_padding(false, 1, 16.0), 16.0 + 10.0 + 24.0);
        // Left-side captions ("close:…" layouts) shift the app cluster right
        // by the button row + one 2px gap.
        assert_eq!(cluster_buttons_start(false, false, 0), 10.0);
        assert_eq!(cluster_buttons_start(false, false, 1), 10.0 + 24.0 + 2.0);
        assert_eq!(cluster_buttons_start(false, false, 3), 10.0 + 76.0 + 2.0);
        // macOS ignores the Linux caption count entirely.
        assert_eq!(cluster_buttons_start(true, false, 3), 88.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..92; a 16px-padded header needs 84 more px to
        // put content at 92 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 0, 16.0), 84.0);
        assert_eq!(cluster_clearance(false, false, 0, 10.0), 90.0);
        // Linux with a left-side close caption: everything shifts one slot.
        assert_eq!(cluster_clearance(false, false, 1, 16.0), 84.0 + 26.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 0, 16.0),
            88.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 0, 16.0),
            12.0 + CLUSTER_BUTTONS_WIDTH + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: zuse sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        // Everything closed until explicitly opened (user request — the
        // brief default-open popped the pane on every visited session).
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        assert_eq!(panels.get("a").right_active, RightSurface::Picker);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true,
                ..Default::default()
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
        // The right pane round-trips back closed.
        assert!(!panels.toggle_changes("a"));
        assert!(!panels.get("a").changes_open);
    }

    #[test]
    fn session_panels_update_tracks_right_surfaces() {
        let mut panels = SessionPanels::default();
        panels.update("a", |p| p.right_active = RightSurface::Diff(3));
        assert_eq!(panels.get("a").right_active, RightSurface::Diff(3));
        // Other chats keep the picker default.
        assert_eq!(panels.get("b").right_active, RightSurface::Picker);
        panels.update("a", |p| p.right_active = RightSurface::Terminal(7));
        assert_eq!(panels.get("a").right_active, RightSurface::Terminal(7));
        panels.update("a", |p| p.right_active = RightSurface::File(0));
        assert_eq!(panels.get("a").right_active, RightSurface::File(0));
    }

    #[test]
    fn session_panels_right_history_tracks_visits_without_duplicates() {
        let mut panels = SessionPanels::default();
        let [a, b, c, d] = [1, 2, 3, 4].map(RightSurface::Browser);
        for surface in [a, b, c, d, c, b, d, d] {
            panels.update("chat", |p| p.right_active = surface);
        }
        assert_eq!(panels.right_tab_history["chat"], vec![a, c, b, d]);
        panels.forget_right_surface("chat", d);
        let next = panels.right_surface_fallback("chat", [a, b, c].into_iter());
        assert_eq!(next, b);
        panels.update("chat", |p| p.right_active = next);
        panels.forget_right_surface("chat", b);
        assert_eq!(panels.right_surface_fallback("chat", [a, c].into_iter()), c);
        // Dragging the strip into another order cannot change recency.
        assert_eq!(panels.right_surface_fallback("chat", [c, a].into_iter()), c);
    }

    #[test]
    fn session_panels_right_history_is_scoped_and_skips_stale_surfaces() {
        let mut panels = SessionPanels::default();
        let a = RightSurface::File(1);
        let b = RightSurface::SideChat(2);
        let stale = RightSurface::Terminal(3);
        for surface in [a, b, stale] {
            panels.update("first", |p| p.right_active = surface);
        }
        for surface in [b, a] {
            panels.update("second", |p| p.right_active = surface);
        }
        assert_eq!(
            panels.right_surface_fallback("first", [a, b].into_iter()),
            b
        );
        assert_eq!(
            panels.right_surface_fallback("second", [a, b].into_iter()),
            a
        );
        assert_eq!(panels.right_surface_fallback("new", [a, b].into_iter()), a);
        assert_eq!(
            panels.right_surface_fallback("first", [].into_iter()),
            RightSurface::Picker
        );
        // Changing visibility or showing the picker is not a surface visit.
        panels.update("first", |p| p.changes_open = true);
        panels.update("first", |p| p.right_active = RightSurface::Picker);
        assert_eq!(panels.right_tab_history["first"], vec![a, b, stale]);
        for surface in [a, b, stale] {
            panels.forget_right_surface("first", surface);
        }
        assert!(!panels.right_tab_history.contains_key("first"));
        assert_eq!(panels.right_tab_history["second"], vec![b, a]);
    }

    #[gpui::test]
    fn right_tabs_close_in_visit_order_across_surface_types(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, window, cx| {
                shell.active_chat = "owner".into();
                let mut tabs = Vec::new();
                for _ in 0..4 {
                    shell.add_browser_surface(None, window, cx);
                    tabs.push(RightSurface::Browser(shell.browser_seq));
                }
                let [a, b, c, d] = <[_; 4]>::try_from(tabs).unwrap();
                for surface in [c, b, d] {
                    shell.set_right_active(surface, cx);
                }
                shell.reorder_right_tabs(2, 0, cx);
                shell.close_right_surface(d, window, cx);
                assert_eq!(shell.resolved_right_active(cx), b);
                shell.close_right_surface(a, window, cx);
                assert_eq!(shell.resolved_right_active(cx), b, "inactive close");
                shell.close_right_surface(b, window, cx);
                assert_eq!(shell.resolved_right_active(cx), c);

                shell.add_browser_surface(None, window, cx);
                let recent = RightSurface::Browser(shell.browser_seq);
                shell.add_file_surface("README.md".into(), window, cx);
                let file = RightSurface::File(shell.file_surface_seq);
                shell.close_right_surface(file, window, cx);
                assert_eq!(shell.resolved_right_active(cx), recent);

                // A save may complete after switching to another session.
                shell.add_file_surface("README.md".into(), window, cx);
                let file = RightSurface::File(shell.file_surface_seq);
                let owner = shell.panel_key(cx);
                shell.active_chat = "other".into();
                shell.add_browser_surface(None, window, cx);
                let other = RightSurface::Browser(shell.browser_seq);
                shell.complete_file_close(file, &owner, cx);
                assert_eq!(shell.resolved_right_active(cx), other);
                shell.active_chat = "owner".into();
                assert_eq!(shell.resolved_right_active(cx), recent);
                shell.close_right_surface(recent, window, cx);
                shell.close_right_surface(c, window, cx);
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Picker);
                assert!(!shell.panels.right_tab_history.contains_key(&owner));
                assert!(!shell.right_pane_open(cx));
            })
            .unwrap();
    }

    #[test]
    fn file_surface_is_single_instance_per_tab_list() {
        let mut tabs = vec![RightSurface::Terminal(1)];
        assert!(push_unique_right_surface(&mut tabs, RightSurface::File(0)));
        assert!(!push_unique_right_surface(&mut tabs, RightSurface::File(0)));
        assert_eq!(tabs, vec![RightSurface::Terminal(1), RightSurface::File(0)]);
    }

    #[test]
    fn file_editors_are_distinct_surface_tabs_with_stable_titles() {
        let mut tabs = vec![RightSurface::File(0)];
        assert!(push_unique_right_surface(&mut tabs, RightSurface::File(1)));
        assert!(push_unique_right_surface(&mut tabs, RightSurface::File(2)));
        assert!(!push_unique_right_surface(&mut tabs, RightSurface::File(1)));
        assert_eq!(workspace_file_title("src/árbol.rs"), "árbol.rs");
    }

    // ---- sidebar resort FLIP diff (§1.6) ----

    fn keys(list: &[(&str, f32)]) -> Vec<(String, f32)> {
        list.iter().map(|(k, h)| (k.to_string(), *h)).collect()
    }

    #[test]
    fn sidebar_chat_height_tracks_visible_metadata() {
        assert_eq!(chat_row_height(false, false), 45.0);
        assert_eq!(chat_row_height(true, false), 61.0);
        assert_eq!(chat_row_height(false, true), 63.0);
        assert_eq!(chat_row_height(true, true), 63.0);
    }

    #[test]
    fn sidebar_harness_geometry_reflects_row_hierarchy() {
        assert_eq!(SIDEBAR_ACTIVE_HARNESS_TITLE_GAP, Theme::SPACE_SM);
    }

    #[test]
    fn sidebar_height_change_is_not_a_reorder() {
        let open = keys(&[("first-group", 105.0), ("second-group", 240.0)]);
        let collapsed = keys(&[("first-group", 40.0), ("second-group", 240.0)]);
        assert!(!sidebar_key_order_changed(&open, &collapsed));

        let reordered = keys(&[("second-group", 240.0), ("first-group", 40.0)]);
        assert!(sidebar_key_order_changed(&collapsed, &reordered));
    }

    #[test]
    fn resort_offsets_empty_when_order_unchanged() {
        let order = keys(&[("a", 29.0), ("b", 29.0), ("c", 45.0)]);
        assert!(resort_offsets(&order, &order, 2.0).is_empty());
    }

    #[test]
    fn resort_offsets_activity_moves_row_to_top() {
        // c (bottom, y=62) jumps to top: c glides down-from-above? No — c's
        // old y is 62, new y is 0 → starts +62 below… offset = old - new = +62,
        // painted at +62 decaying to 0 (a glide UP into place). a and b shift
        // down by c's height + gap (31).
        let old = keys(&[("a", 29.0), ("b", 29.0), ("c", 29.0)]);
        let new = keys(&[("c", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        assert_eq!(offsets.get("c"), Some(&62.0));
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_respect_heights_and_gap() {
        // Tall row (45px) swaps with a short one (29px).
        let old = keys(&[("tall", 45.0), ("short", 29.0)]);
        let new = keys(&[("short", 29.0), ("tall", 45.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // short: old y 47 → new y 0; tall: old y 0 → new y 31.
        assert_eq!(offsets.get("short"), Some(&47.0));
        assert_eq!(offsets.get("tall"), Some(&-31.0));
    }

    #[test]
    fn resort_offsets_ignore_added_and_removed_keys() {
        let old = keys(&[("a", 29.0), ("gone", 29.0), ("b", 29.0)]);
        let new = keys(&[("new", 29.0), ("a", 29.0), ("b", 29.0)]);
        let offsets = resort_offsets(&old, &new, 2.0);
        // "new" has no old position (fades in instead); "gone" just goes.
        assert!(!offsets.contains_key("new"));
        assert!(!offsets.contains_key("gone"));
        // a: old 0 → new 31 (pushed down by the insert); b: 62 → 62 (gone's
        // slot replaced by "new" of equal height — no move, no entry).
        assert_eq!(offsets.get("a"), Some(&-31.0));
        assert_eq!(offsets.get("b"), None);
    }

    #[test]
    fn resort_glide_spec_matches_original() {
        // §1.6: 260ms cubic-bezier(0.22, 1, 0.36, 1).
        assert_eq!(RESORT.duration_ms, 260);
        assert_eq!(RESORT.curve, motion::EASE_RESORT);
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Harnesses));
        nav.push(NavEntry::Settings(SettingsSection::Harnesses));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — zuse's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    #[test]
    fn pull_request_route_participates_in_browser_history() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::PullRequests);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(NavEntry::PullRequests));

        nav.push(NavEntry::PullRequests);
        assert_eq!(nav.len(), 2, "reopening the active dashboard deduplicates");
        nav.push(chat("a"));
        assert_eq!(nav.back(), Some(NavEntry::PullRequests));
    }

    #[test]
    fn pull_request_route_can_open_from_and_return_to_settings() {
        let settings = NavEntry::Settings(SettingsSection::Devices);
        let mut nav = NavHistory::new(settings.clone());
        nav.push(NavEntry::PullRequests);
        assert_eq!(nav.back(), Some(settings));
    }

    #[test]
    fn sidebar_disclosure_motion_lands_exactly_on_its_target() {
        let mut tween = SidebarDisclosureMotion::new(1, 240.0, 0.0);
        tween.started = std::time::Instant::now() - motion::COLLAPSE.total().mul_f32(2.0);
        assert_eq!(tween.current(), 0.0);
        assert!(!tween.animating());
    }
}

#[cfg(test)]
mod exit_regressions {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[gpui::test]
    fn new_shell_only_shows_boot_splash_while_connecting(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let boot = EngineBootConfig {
            target: None,
            data_dir: dir.path().into(),
            ipc_port: 0,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: zuse_protocol::HarnessId::Mock,
        };
        for (connection, expected) in [
            (ConnectionStatus::Connecting, SplashPhase::Visible),
            (ConnectionStatus::Ready, SplashPhase::Gone),
            (
                ConnectionStatus::Failed("offline".into()),
                SplashPhase::Gone,
            ),
        ] {
            let window = cx.add_window(|_, cx| {
                let state = cx.new(|_| {
                    let mut state = AppState::new();
                    state.connection = connection;
                    state
                });
                Shell::new(state, boot.clone(), cx)
            });
            window
                .update(cx, |shell, _, _| assert_eq!(shell.splash, expected))
                .unwrap();
        }
    }

    #[gpui::test]
    fn appshot_destinations_retain_last_session_and_use_new_canvas_defaults(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, window, cx| {
                shell.state.update(cx, |state, cx| {
                    state.spaces = ["a", "b"]
                        .into_iter()
                        .map(|id| {
                            serde_json::from_value(serde_json::json!({
                                "id": id, "deviceId": "local", "path": "/tmp", "gitDetected": false,
                                "createdAt": Utc::now(),
                            }))
                            .unwrap()
                        })
                        .collect();
                    state.chats =
                        vec![serde_json::from_value(serde_json::json!({
                    "id": "last", "deviceId": "local", "spaceId": "a", "archived": false,
                    "createdAt": Utc::now(),
                })).unwrap()];
                    state.select_chat(Some("last".into()), cx);
                });
                shell.on_state_changed(&shell.state.clone(), cx);
                shell.settings.space_filter = Some("b".into());
                shell.open_new_session(None, cx);
                shell.on_state_changed(&shell.state.clone(), cx);
                assert!(shell.active_chat.is_empty());
                shell.settings.appshot_destination =
                    crate::appshots::AppshotDestination::LastSession;
                shell.receive_appshot(crate::appshots::tests::shot(), window, cx);
                assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("last"));
                assert_eq!(shell.composer.read(cx).appshots["last"].len(), 1);
                shell.settings.appshot_destination =
                    crate::appshots::AppshotDestination::NewSession;
                shell.receive_appshot(crate::appshots::tests::shot(), window, cx);
                assert!(shell.state.read(cx).selected_chat.is_none());
                assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("b"));
                assert_eq!(shell.composer.read(cx).appshots[""].len(), 1);
            })
            .unwrap();
    }

    #[gpui::test]
    fn right_panel_opens_at_default_and_remembers_a_resized_width(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                shell.active_chat = "preview".into();
                shell.viewport_width = 1400.0;
                shell.set_surfaces_open(true, cx);
                assert_eq!(shell.right_settled_target(cx), 520.0);

                // A drag updates this preference; closing and reopening the
                // panel must not replace it with the default or an even split.
                shell.settings.right_pane_width = 400.0;
                shell.set_surfaces_open(false, cx);
                shell.set_surfaces_open(true, cx);
                assert_eq!(shell.right_settled_target(cx), 400.0);
                shell.observe_viewport_width(1500.0, cx);
                assert_eq!(shell.right_settled_target(cx), 400.0);
            })
            .unwrap();
    }

    #[gpui::test]
    fn pane_geometry_uses_one_animation_time_per_frame(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                let duration = RESIZE.total().mul_f32(motion::speed_scale());
                let started = std::time::Instant::now() - duration.mul_f32(2.);
                let tween = Some(WidthTween {
                    from: 520.,
                    to: 0.,
                    started,
                    duration,
                    curve: RESIZE.curve,
                });
                shell.reduced_motion = false;
                // The frame started halfway through the transition but rendering
                // crossed its deadline. Every region must still use that frame.
                shell.render_time = Some(started + duration.mul_f32(0.5));
                assert!(shell.tween_active(tween));
                assert_eq!(shell.active_tween_endpoints(tween), Some((520., 0.)));
                let width = shell.eval_tween(tween, 0.);
                assert!(width > 0. && width < 520.);
                assert_eq!(shell.eval_tween(tween, 0.), width);
                shell.active_chat = "preview".into();
                shell.viewport_width = 1000.;
                shell.right_tween = tween;
                shell.toggle_right_pane(cx);
                assert_eq!(
                    shell.right_tween.unwrap().from,
                    width,
                    "reversing must not restart from zero"
                );
                let files = cx.new(|cx| {
                    FilesSurface::new(
                        shell.state.clone(),
                        "preview".into(),
                        false,
                        1000,
                        13.0,
                        false,
                        false,
                        cx,
                    )
                });
                let key = shell.panel_key(cx);
                shell.file_surfaces.insert(0, files.clone());
                shell
                    .right_tabs
                    .insert(key.clone(), vec![RightSurface::File(0)]);
                shell
                    .panels
                    .update(&key, |panel| panel.right_active = RightSurface::File(0));
                assert!(files.read(cx).test_images_visible());
                shell.toggle_right_pane(cx);
                assert!(!shell.right_pane_open(cx));
                assert!(shell.tween_active(shell.right_tween));
                assert!(
                    files.read(cx).test_images_visible(),
                    "file content stays painted through the closing mask"
                );
                let frame_time = shell.render_time;
                shell.render_time = Some(shell.right_tween.unwrap().started + duration);
                shell.suspend_hidden_right_images(cx);
                assert!(
                    !files.read(cx).test_images_visible(),
                    "image resources suspend once the close animation ends"
                );
                shell.render_time = frame_time;
                shell.settings.sidebar_collapsed = true;
                shell.sidebar_tween = tween;
                shell.toggle_sidebar(cx);
                assert_eq!(shell.sidebar_tween.unwrap().from, width);
                shell.render_time = None;
                assert!(!shell.tween_active(tween));
                assert_eq!(shell.active_tween_endpoints(tween), None);
                assert_eq!(shell.eval_tween(tween, 0.), 0.);
            })
            .unwrap();
    }

    #[gpui::test]
    fn panel_saves_preserve_settings_selected_outside_the_shell(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        for (index, effect) in settings::NewThreadBackgroundEffect::ALL
            .into_iter()
            .enumerate()
        {
            let open_links_in_zuse = index % 2 == 0;
            let terminal_family = if open_links_in_zuse {
                crate::typography::UiFontFamily::System
            } else {
                crate::typography::UiFontFamily::Geist
            };
            let code_family = if open_links_in_zuse {
                crate::typography::UiFontFamily::Geist
            } else {
                crate::typography::UiFontFamily::System
            };
            let terminal_size = 15.0 + index as f32;
            let code_size = 11.0 + index as f32;
            let transcript_width = 736.0 + 16.0 * index as f32;
            let geometry = Some(settings::WindowGeometry {
                display_uuid: Some(uuid::Uuid::from_u128(7)),
                x: 80.0 + index as f32,
                y: 60.0,
                width: 1100.0,
                height: 750.0,
            });
            window
                .update(cx, |shell, _, cx| {
                    // Selection changes in Appearance, independently of the shell's
                    // cached snapshot. Include a previously queued geometry save.
                    shell.settings.sidebar_width = 280.0;
                    shell.schedule_save(cx);
                    settings::set_new_thread_background_effect(effect, cx);
                    settings::update(settings::SavePolicy::Immediate, cx, |settings| {
                        settings.wallpaper_folder = Some(dir.path().join("wallpapers"));
                        settings.wallpaper_source = Some(dir.path().join("wallpapers/current.png"));
                        settings.wallpaper_history =
                            vec![dir.path().join("wallpapers/current.png")];
                        settings.window_geometry = geometry;
                        settings.open_web_links_in_zuse = open_links_in_zuse;
                        settings.pull_request_destination =
                            settings::PullRequestDestination::External;
                        settings.last_pull_request_repository = Some("acme/zuse".into());
                        settings.last_pull_request_device = Some("remote-pr-device".into());
                        settings.terminal_font_family = terminal_family.clone();
                        settings.terminal_font_size = terminal_size;
                        settings.code_font_family = code_family.clone();
                        settings.code_font_size = code_size;
                        settings.transcript_width = transcript_width;
                        settings.skill_completion_by_harness.insert(
                            zuse_protocol::HarnessId::ClaudeCode,
                            settings::SkillCompletionSettings {
                                dollar: open_links_in_zuse,
                                separate_from_slash: true,
                            },
                        );
                    });
                    for step in 0..3 {
                        shell.settings.sidebar_width = 290.0 + step as f32;
                        shell.settings.right_pane_width = 540.0 + step as f32;
                        shell.settings.terminal_height = 300.0 + step as f32;
                        shell.schedule_save(cx);
                        let current = settings::current(cx);
                        assert_eq!(
                            current.wallpaper_history,
                            vec![dir.path().join("wallpapers/current.png")]
                        );
                        assert_eq!(
                            current.wallpaper_folder,
                            Some(dir.path().join("wallpapers"))
                        );
                        assert_eq!(
                            current.wallpaper_source,
                            Some(dir.path().join("wallpapers/current.png"))
                        );
                        assert_eq!(
                            current.last_pull_request_repository.as_deref(),
                            Some("acme/zuse")
                        );
                        assert_eq!(
                            current.last_pull_request_device.as_deref(),
                            Some("remote-pr-device")
                        );
                        assert_eq!(current.window_geometry, geometry);
                        assert_eq!(current.new_thread_background_effect, effect);
                        assert_eq!(current.open_web_links_in_zuse, open_links_in_zuse);
                        assert_eq!(
                            current.pull_request_destination,
                            settings::PullRequestDestination::External
                        );
                        assert_eq!(current.terminal_font_family, terminal_family);
                        assert_eq!(current.terminal_font_size, terminal_size);
                        assert_eq!(current.code_font_family, code_family);
                        assert_eq!(current.code_font_size, code_size);
                        assert_eq!(current.transcript_width, transcript_width);
                        assert_eq!(
                            current
                                .skill_completion(zuse_protocol::HarnessId::ClaudeCode)
                                .dollar,
                            open_links_in_zuse
                        );
                        assert!(
                            current
                                .skill_completion(zuse_protocol::HarnessId::ClaudeCode)
                                .separate_from_slash
                        );
                    }
                    settings::flush(cx);
                    let loaded = settings::UiSettings::load(dir.path());
                    assert_eq!(
                        loaded.last_pull_request_repository.as_deref(),
                        Some("acme/zuse")
                    );
                    assert_eq!(
                        loaded.last_pull_request_device.as_deref(),
                        Some("remote-pr-device")
                    );
                    assert_eq!(loaded.window_geometry, geometry);
                    assert_eq!(
                        loaded.wallpaper_history,
                        vec![dir.path().join("wallpapers/current.png")]
                    );
                    assert_eq!(loaded.wallpaper_folder, Some(dir.path().join("wallpapers")));
                    assert_eq!(
                        loaded.wallpaper_source,
                        Some(dir.path().join("wallpapers/current.png"))
                    );
                    assert_eq!(loaded.new_thread_background_effect, effect);
                    assert_eq!(loaded.open_web_links_in_zuse, open_links_in_zuse);
                    assert_eq!(
                        loaded.pull_request_destination,
                        settings::PullRequestDestination::External
                    );
                    assert_eq!(loaded.terminal_font_family, terminal_family);
                    assert_eq!(loaded.terminal_font_size, terminal_size);
                    assert_eq!(loaded.code_font_family, code_family);
                    assert_eq!(loaded.code_font_size, code_size);
                    assert_eq!(loaded.transcript_width, transcript_width);
                    assert_eq!(loaded.sidebar_width, 292.0);
                    assert_eq!(loaded.right_pane_width, 542.0);
                    assert_eq!(loaded.terminal_height, 302.0);
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn shell_saves_never_revert_settings_written_outside_the_shell(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let before = cx.update(|cx| settings::current(cx));
        window
            .update(cx, |shell, _, cx| {
                // Toggles that write the store directly (the General page's
                // Compact mode, notification and sidebar switches, ...).
                settings::set_transcript_compact_mode(!before.transcript_compact_mode, cx);
                settings::update(SavePolicy::Immediate, cx, |s| {
                    s.notifications_enabled = !before.notifications_enabled;
                    s.sidebar_show_branch = !before.sidebar_show_branch;
                    s.escape_stops_active_agent = !before.escape_stops_active_agent;
                });
                // Navigating away saves the shell's own working copy.
                shell.remember_settings_section(SettingsSection::Notifications, cx);
                // The shell's own writes still land.
                shell.settings.sidebar_width += 10.0;
                shell.schedule_save(cx);
            })
            .unwrap();
        let after = cx.update(|cx| settings::current(cx));
        assert_eq!(
            after.transcript_compact_mode,
            !before.transcript_compact_mode
        );
        assert_eq!(after.notifications_enabled, !before.notifications_enabled);
        assert_eq!(after.sidebar_show_branch, !before.sidebar_show_branch);
        assert_eq!(
            after.escape_stops_active_agent,
            !before.escape_stops_active_agent
        );
        assert_eq!(
            after.settings_section,
            SettingsSection::Notifications.canonical()
        );
        assert_eq!(after.sidebar_width, before.sidebar_width + 10.0);
    }

    #[gpui::test]
    fn workspace_slash_commands_open_existing_zuse_surfaces(cx: &mut TestAppContext) {
        use crate::composer::WorkspaceCommand;
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        for command in [
            WorkspaceCommand::Settings,
            WorkspaceCommand::New,
            WorkspaceCommand::Resume,
            WorkspaceCommand::Model,
        ] {
            window
                .update(cx, |shell, window, cx| {
                    if matches!(command, WorkspaceCommand::Model) {
                        shell.close_command_palette(window, cx);
                    }
                    shell.pending_workspace_command = Some(command);
                    cx.notify();
                })
                .unwrap();
            // Render through the window's arena, as production does. Calling
            // Shell::render directly allocates modal children in the fallback
            // arena, which does not belong to this test App's lifecycle.
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            window
                .update(cx, |shell, _, cx| {
                    match command {
                        WorkspaceCommand::Settings => {
                            assert!(matches!(shell.route, Route::Settings(_)));
                        }
                        WorkspaceCommand::New => {
                            assert!(matches!(shell.route, Route::Chat));
                            assert!(shell.state.read(cx).selected_chat.is_none());
                        }
                        WorkspaceCommand::Resume => assert!(shell.command_palette.is_some()),
                        WorkspaceCommand::Model => {
                            assert!(shell.composer.read(cx).pickers().read(cx).is_open());
                        }
                        _ => unreachable!(),
                    }
                    assert!(shell.pending_workspace_command.is_none());
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn shell_saves_keep_motion_settings_chosen_in_appearance(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                // The Appearance page writes these straight to the store,
                // outside the shell's working copy.
                settings::update(SavePolicy::Immediate, cx, |settings| {
                    settings.reduce_motion = crate::motion::ReduceMotion::On;
                    settings.pause_animations_in_background = true;
                });
                // Any shell-owned change (sidebar width, panels) republishes
                // the working copy; it must not revert the motion choices.
                shell.schedule_save(cx);
                let saved = settings::current(cx);
                assert_eq!(saved.reduce_motion, crate::motion::ReduceMotion::On);
                assert!(saved.pause_animations_in_background);
            })
            .unwrap();
    }

    #[gpui::test]
    fn shell_saves_keep_every_setting_chosen_outside_the_shell(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                // Voice, transcript and diff controls save straight to the
                // store. None of them is known to the Shell.
                settings::update(SavePolicy::Immediate, cx, |settings| {
                    settings.dictation_enabled = true;
                    settings.dictation_input = Some("coreaudio:usb".into());
                    settings.codex_voice = Some("ember".into());
                    settings.transcript_compact_mode = true;
                    settings.code_fences_fit_content = true;
                    settings.diff_split = true;
                    settings.diff_wrap = true;
                });
                // Leaving Settings and resizing both republish the Shell's copy.
                shell.settings.settings_section = SettingsSection::Appearance;
                shell.settings.sidebar_width = 300.0;
                shell.schedule_save(cx);
                let saved = settings::current(cx);
                assert!(saved.dictation_enabled);
                assert_eq!(saved.dictation_input.as_deref(), Some("coreaudio:usb"));
                assert_eq!(saved.codex_voice.as_deref(), Some("ember"));
                assert!(saved.transcript_compact_mode);
                assert!(saved.code_fences_fit_content);
                assert!(saved.diff_split);
                assert!(saved.diff_wrap);
                assert_eq!(saved.settings_section, SettingsSection::Appearance);
                assert_eq!(saved.sidebar_width, 300.0);
                // An outside change to a field the Shell also holds still wins
                // once the Shell has nothing newer of its own.
                settings::update(SavePolicy::Immediate, cx, |settings| {
                    settings.dictation_enabled = false;
                    settings.codex_voice = None;
                    settings.sidebar_width = 320.0;
                });
                shell.schedule_save(cx);
                let saved = settings::current(cx);
                assert!(!saved.dictation_enabled);
                assert_eq!(saved.codex_voice, None);
                assert_eq!(saved.sidebar_width, SIDEBAR_MAX);
                assert_eq!(shell.settings.sidebar_width, SIDEBAR_MAX);
            })
            .unwrap();
    }

    #[gpui::test]
    fn sidebar_settings_trigger_toggles_the_settings_route(cx: &mut TestAppContext) {
        // Just the footer row: the full shell's settings modal would cover it.
        struct FooterHost(Entity<Shell>);
        impl Render for FooterHost {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.0.update(cx, |shell, cx| {
                    div()
                        .w(px(256.0))
                        .child(shell.render_sidebar_footer(&Theme::default(), cx))
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            FooterHost(cx.new(|cx| {
                let state = cx.new(|_| AppState::new());
                Shell::new(
                    state,
                    EngineBootConfig {
                        target: None,
                        data_dir: dir.path().into(),
                        ipc_port: 0,
                        edge_url: "http://127.0.0.1:1".into(),
                        edge_token: None,
                        org_id: None,
                        workos_client_id: None,
                        default_harness: zuse_protocol::HarnessId::Mock,
                    },
                    cx,
                )
            }))
        });
        let shell = host.read_with(cx, |host, _| host.0.clone());
        for expect_open in [true, false] {
            cx.update(|window, cx| window.draw(cx).clear());
            assert!(cx.debug_bounds("sidebar-account").is_some());
            let trigger = cx.debug_bounds("settings-trigger").unwrap().center();
            cx.simulate_click(trigger, gpui::Modifiers::default());
            shell.read_with(cx, |shell, _| {
                if expect_open {
                    assert!(matches!(
                        shell.route,
                        Route::Settings(SettingsSection::General)
                    ));
                } else {
                    assert!(matches!(shell.route, Route::Chat));
                }
            });
        }
    }

    #[gpui::test]
    fn sidebar_footer_leaves_pull_requests_to_the_nav(cx: &mut TestAppContext) {
        // Chat and Settings both render this row; host it alone so the full
        // shell's pages don't cover it.
        struct FooterRow(Entity<Shell>);
        impl Render for FooterRow {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.0.update(cx, |shell, cx| {
                    div()
                        .w(px(256.0))
                        .child(shell.render_sidebar_footer_row(&Theme::default(), cx))
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            FooterRow(cx.new(|cx| {
                let state = cx.new(|_| AppState::new());
                Shell::new(
                    state,
                    EngineBootConfig {
                        target: None,
                        data_dir: dir.path().into(),
                        ipc_port: 0,
                        edge_url: "http://127.0.0.1:1".into(),
                        edge_token: None,
                        org_id: None,
                        workos_client_id: None,
                        default_harness: zuse_protocol::HarnessId::Mock,
                    },
                    cx,
                )
            }))
        });
        let shell = host.read_with(cx, |host, _| host.0.clone());
        shell.update(cx, |shell, cx| {
            shell.open_settings(SettingsSection::General, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("sidebar-footer-row").is_some());
        assert!(cx.debug_bounds("settings-trigger").is_some());
        assert!(
            cx.debug_bounds("open-pull-requests").is_none(),
            "Pull requests moved to the sidebar nav"
        );
    }

    /// A ready Shell with no engine, for the Pull requests route.
    fn pull_request_shell(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Shell>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let data_dir = dir.path().to_path_buf();
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.connection = zuse_protocol::view::ConnectionStatus::Ready;
                state
            });
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir,
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        shell.update(cx, |shell, _| shell.debug_gate = Some(GatePhase::Ready));
        (shell, cx, dir)
    }

    fn open_pull_request(cx: &mut gpui::VisualTestContext, number: u32) {
        cx.update(|window, cx| {
            window.dispatch_action(
                Box::new(crate::pull_request_detail::OpenPullRequest(
                    format!("https://github.com/a/b/pull/{number}"),
                    None,
                )),
                cx,
            );
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn new_thread_canvas_has_no_overview_tab_strip(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = pull_request_shell(cx);
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        shell.update(cx, |shell, cx| {
            shell.reduced_motion = true;
            shell.open_pull_requests(cx);
            shell.open_new_session(None, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(shell.read_with(cx, |shell, cx| !shell.has_conversation(cx)));
        assert!(cx.debug_bounds("overview-threads").is_none());
        assert!(cx.debug_bounds("overview-pull-requests").is_none());
        assert!(cx.debug_bounds("overview-add").is_none());
    }

    #[gpui::test]
    fn pull_requests_share_workspace_sidebar_and_place_navigation_before_toggle(
        cx: &mut TestAppContext,
    ) {
        let (shell, cx, _dir) = pull_request_shell(cx);
        shell.update(cx, |shell, cx| {
            shell.reduced_motion = true;
            shell.open_pull_requests(cx);
        });
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("workspace-app-logo").is_some());
        assert!(shell.read_with(cx, |shell, _| shell.workspace_gutter() > 0.));
        assert!(cx.debug_bounds("overview-pull-requests").is_none());
        let navigation = cx.debug_bounds("titlebar-navigation").unwrap();
        let toggle = cx.debug_bounds("workspace-hide-sidebar").unwrap();
        assert!(navigation.right() <= toggle.left());

        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear());
        let navigation = cx.debug_bounds("titlebar-navigation").unwrap();
        let toggle = cx.debug_bounds("toggle-sidebar").unwrap();
        assert!(toggle.right() <= navigation.left());
    }

    #[gpui::test]
    fn pull_request_pane_follows_history_shortcuts_and_its_tab_menu(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = pull_request_shell(cx);
        cx.update(|_, cx| motion::set_reduced_motion(cx, true));
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        shell.update(cx, |shell, cx| shell.open_pull_requests(cx));
        cx.run_until_parked();
        for number in 1..=3 {
            open_pull_request(cx, number);
        }
        let redraw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear());
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear());
        };
        let pane = |cx: &mut gpui::VisualTestContext| {
            shell.read_with(cx, |shell, _| {
                (
                    shell.pull_request_pane_open(),
                    shell.pull_request_pane.active_index(),
                    shell.pull_request_pane.tabs.len(),
                )
            })
        };
        let back = |cx: &mut gpui::VisualTestContext, forward: bool| {
            shell.update(cx, |shell, cx| {
                if forward {
                    shell.navigate_forward(cx)
                } else {
                    shell.navigate_back(cx)
                }
            });
            redraw(cx);
        };

        // Back steps through the pull requests viewed, then to the board
        // alone with the tabs kept; Forward reopens the pane.
        back(cx, false);
        assert_eq!(pane(cx), (true, Some(1), 3));
        back(cx, false);
        assert_eq!(pane(cx), (true, Some(0), 3));
        back(cx, false);
        assert_eq!(pane(cx).0, false);
        assert_eq!(pane(cx).2, 3);
        back(cx, true);
        assert_eq!(pane(cx), (true, Some(0), 3));

        // Picking a tab is a step Back can undo.
        redraw(cx);
        let tab = cx.debug_bounds("pull-request-tab-2").unwrap();
        cx.simulate_click(tab.center(), gpui::Modifiers::default());
        assert_eq!(pane(cx).1, Some(2));
        back(cx, false);
        assert_eq!(pane(cx).1, Some(0));

        // The right sidebar shortcut hides and shows the pane.
        for open in [false, true] {
            cx.update(|window, cx| window.dispatch_action(Box::new(ToggleChanges), cx));
            redraw(cx);
            assert_eq!(pane(cx).0, open);
        }

        // Next tab cycles the pane's tabs while it has focus.
        cx.update(|window, cx| {
            let focus = shell.read(cx).navigation_focus.right.clone();
            window.focus(&focus, cx);
            window.dispatch_action(Box::new(NextSession), cx);
        });
        redraw(cx);
        assert_eq!(pane(cx).1, Some(1));

        // ⌘W closes the active tab first.
        shell.update_in(cx, |shell, window, cx| {
            assert!(shell.close_active_surface(window, cx))
        });
        assert_eq!(pane(cx).2, 2);

        // The tab menu closes the others.
        redraw(cx);
        let tab = cx.debug_bounds("pull-request-tab-0").unwrap();
        cx.simulate_mouse_down(
            tab.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            tab.center(),
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        redraw(cx);
        let others = cx
            .debug_bounds("pull-request-tab-menu-Close other tabs")
            .unwrap();
        cx.simulate_click(others.center(), gpui::Modifiers::default());
        redraw(cx);
        let urls: Vec<_> = shell.read_with(cx, |shell, _| {
            shell
                .pull_request_pane
                .tabs
                .iter()
                .map(|tab| tab.url.clone())
                .collect()
        });
        assert_eq!(urls, ["https://github.com/a/b/pull/1"]);
    }

    #[gpui::test]
    fn pull_request_tab_cycling_keeps_focus_after_leaving_content(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = pull_request_shell(cx);
        cx.update(|_, cx| motion::set_reduced_motion(cx, true));
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        shell.update(cx, |shell, cx| shell.open_pull_requests(cx));
        cx.run_until_parked();
        open_pull_request(cx, 1);
        open_pull_request(cx, 2);
        cx.update(|window, cx| window.draw(cx).clear());

        // Focus a descendant that is unmounted when its PR tab is hidden,
        // rather than the pane's stable focus handle used by the other test.
        let content = cx.debug_bounds("pr-detail-scroll").unwrap();
        cx.simulate_click(
            gpui::point(content.left() + px(24.0), content.top() + px(100.0)),
            gpui::Modifiers::default(),
        );
        cx.update(|window, cx| {
            let shell = shell.read(cx);
            assert!(!shell.right_pane_open(cx), "the chat pane is closed");
            assert!(shell.navigation_focus.in_right(window, cx));
            assert!(!shell.navigation_focus.right.is_focused(window));
        });

        for (key, expected) in [("ctrl-tab", 0), ("ctrl-tab", 1), ("ctrl-shift-tab", 0)] {
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear());
            cx.update(|window, cx| {
                let shell = shell.read(cx);
                assert!(matches!(shell.route, Route::PullRequests));
                assert_eq!(shell.pull_request_pane.active_index(), Some(expected));
                assert!(
                    shell.navigation_focus.in_right(window, cx),
                    "focus after {key}"
                );
            });
        }

        // History switches pages without explicit tab activation, so focus
        // recovery must also recognize the PR pane independently of the chat.
        let content = cx.debug_bounds("pr-detail-scroll").unwrap();
        cx.simulate_click(
            gpui::point(content.left() + px(24.0), content.top() + px(100.0)),
            gpui::Modifiers::default(),
        );
        shell.update(cx, |shell, cx| shell.navigate_back(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        cx.update(|window, cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.pull_request_pane.active_index(), Some(1));
            assert!(shell.navigation_focus.in_right(window, cx));
        });
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert!(matches!(shell.route, Route::PullRequests));
            assert_eq!(shell.pull_request_pane.active_index(), Some(0));
        });
    }

    #[gpui::test]
    fn pull_request_tabs_reorder_by_dragging(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = pull_request_shell(cx);
        cx.update(|_, cx| motion::set_reduced_motion(cx, true));
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        shell.update(cx, |shell, cx| shell.open_pull_requests(cx));
        cx.run_until_parked();
        open_pull_request(cx, 1);
        open_pull_request(cx, 2);
        cx.update(|window, cx| window.draw(cx).clear());
        let from = cx.debug_bounds("pull-request-tab-0").unwrap().center();
        let to = cx.debug_bounds("pull-request-tab-1").unwrap().center();
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        // The first move starts the drag; the next one reports its slot.
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.update(|_, cx| assert!(cx.has_active_drag(), "tab drag never started"));
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        shell.read_with(cx, |shell, _| {
            let urls: Vec<_> = shell
                .pull_request_pane
                .tabs
                .iter()
                .map(|tab| tab.url.as_str())
                .collect();
            assert_eq!(
                urls,
                [
                    "https://github.com/a/b/pull/2",
                    "https://github.com/a/b/pull/1"
                ]
            );
            // The active pull request stays active in its new slot.
            assert_eq!(shell.pull_request_pane.active_index(), Some(0));
        });
    }

    #[gpui::test]
    fn pull_request_route_uses_full_window_without_titlebar_overlap(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.connection = zuse_protocol::view::ConnectionStatus::Ready;
                state
            });
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        shell.update(cx, |shell, cx| {
            shell.debug_gate = Some(GatePhase::Ready);
            shell.active_chat = "chat".into();
            shell
                .panels
                .update("chat", |panel| panel.changes_open = true);
            assert!(shell.right_pane_open(cx));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("right-pane").is_some(),
            "the chat shows its side pane"
        );
        shell.update(cx, |shell, cx| {
            shell.open_pull_requests(cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("right-pane").is_none());
        let board = cx.debug_bounds("pull-requests-column").unwrap();
        assert!(
            board.size.width > px(300.0),
            "the board keeps the room the pane would take"
        );

        // Opening pull requests adds tabs to a pane beside the board.
        let open = |cx: &mut gpui::VisualTestContext, number: u32| {
            cx.update(|window, cx| {
                window.dispatch_action(
                    Box::new(crate::pull_request_detail::OpenPullRequest(
                        format!("https://github.com/a/b/pull/{number}"),
                        None,
                    )),
                    cx,
                );
            });
            cx.run_until_parked();
        };
        cx.update(|_, cx| motion::set_reduced_motion(cx, true));
        cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
        open(cx, 1);
        open(cx, 2);
        open(cx, 1);
        shell.read_with(cx, |shell, _| {
            let tabs: Vec<_> = shell
                .pull_request_pane
                .tabs
                .iter()
                .map(|tab| tab.url.as_str())
                .collect();
            assert_eq!(
                tabs,
                [
                    "https://github.com/a/b/pull/1",
                    "https://github.com/a/b/pull/2"
                ],
                "reopening a pull request reuses its tab"
            );
            assert_eq!(shell.pull_request_pane.active_index(), Some(0));
            assert!(shell.pull_request_pane_open());
        });
        cx.update(|window, cx| window.draw(cx).clear());
        let pane = cx.debug_bounds("pull-request-pane").unwrap();
        let board = cx.debug_bounds("pull-requests-column").unwrap();
        assert!(
            board.right() <= pane.left(),
            "the board stays beside the pane"
        );
        assert!(pane.right() <= px(1400.0));
        // Expand covers the board; collapsing gives it back.
        let click = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::default());
            cx.update(|window, cx| window.draw(cx).clear());
        };
        click(cx, "expand-pull-request-pane");
        let covering = cx.debug_bounds("pull-request-pane").unwrap();
        assert!(covering.size.width > pane.size.width);
        assert!(covering.left() <= board.left() + px(zuse_viewport::workspace_style::GUTTER));
        click(cx, "expand-pull-request-pane");
        assert_eq!(cx.debug_bounds("pull-request-pane").unwrap(), pane);
        // The toggle hides the pane and keeps its tabs.
        click(cx, "toggle-pull-request-pane");
        assert!(cx.debug_bounds("pull-request-pane").is_none());
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.pull_request_pane.tabs.len(), 2)
        });
        click(cx, "toggle-pull-request-pane");
        assert_eq!(cx.debug_bounds("pull-request-pane").unwrap(), pane);
        // Closing the last tab collapses the pane; the board fills back in.
        for _ in 0..2 {
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(crate::pull_request_detail::ClosePullRequest), cx);
            });
            cx.run_until_parked();
        }
        shell.read_with(cx, |shell, _| {
            assert!(shell.pull_request_pane.tabs.is_empty());
            assert!(!shell.pull_request_pane_open());
        });
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("pull-request-pane").is_none());

        // The pane's tabs and the persistent controls share the titlebar even
        // with the sidebar collapsed and the macOS traffic lights hidden.
        cx.update(|window, cx| {
            motion::set_reduced_motion(cx, true);
            window.dispatch_action(
                Box::new(crate::pull_request_detail::OpenPullRequest(
                    "https://github.com/a/b/pull/1".into(),
                    None,
                )),
                cx,
            );
        });
        for width in [600.0, 1400.0] {
            cx.simulate_resize(gpui::size(px(width), px(800.0)));
            for collapsed in [true, false] {
                shell.update(cx, |shell, cx| {
                    shell.settings.sidebar_collapsed = collapsed;
                    shell.sidebar_tween = None;
                    cx.notify();
                });
                for fullscreen in [false, true] {
                    cx.update(|window, cx| {
                        if window.is_fullscreen() != fullscreen {
                            window.toggle_fullscreen();
                        }
                        window.draw(cx).clear();
                    });
                    cx.run_until_parked();
                    let tab = cx.debug_bounds("pull-request-tab-0").unwrap();
                    // Covering the board, the tabs sit just past the cluster
                    // like the chat pane's in takeover. The titlebar `+` only
                    // shows while the sidebar (and its New chat row) is hidden.
                    if let Some(new_session) = cx.debug_bounds("titlebar-new-session") {
                        assert!(
                            tab.left() >= new_session.right(),
                            "PR tab overlaps New session: width={width}, collapsed={collapsed}, fullscreen={fullscreen}, tab={tab:?}, new_session={new_session:?}"
                        );
                    } else {
                        assert!(
                            !collapsed,
                            "collapsed sidebar must keep the titlebar New session"
                        );
                    }
                    let toggle = cx.debug_bounds("toggle-pull-request-pane").unwrap();
                    assert!(toggle.left() >= tab.right() && toggle.right() <= px(width));
                    // The pull request's own header is the pane's toolbar,
                    // below the titlebar band.
                    let title = cx.debug_bounds("pr-toolbar-title").unwrap();
                    assert!(title.top() >= px(Theme::TITLEBAR_HEIGHT));
                    let more = cx.debug_bounds("pr-more").unwrap();
                    assert!(more.right() <= px(width));
                }
            }
        }
    }

    #[gpui::test]
    fn pull_request_handoff_stages_its_prompt_in_a_new_session(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                let space = |id: &str, device: &str| zuse_protocol::Space {
                    id: id.into(),
                    device_id: device.into(),
                    path: format!("/{id}"),
                    name: None,
                    git_detected: true,
                    git_checked_at: None,
                    checkout_id: None,
                    repository_id: None,
                    remote_repository: None,
                    created_at: Utc::now(),
                };
                shell.state.update(cx, |state, cx| {
                    state.local_device_id = Some("local".into());
                    state.apply_spaces(vec![
                        space("unrelated", "local"),
                        space("pr-project", "remote"),
                    ]);
                    state.select_space(Some("unrelated".into()), cx);
                });
                shell.settings.space_filter = Some("unrelated".into());
                shell.composer.update(cx, |composer, cx| {
                    composer
                        .input
                        .update(cx, |input, cx| input.set_text("half-written reply", cx))
                });
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(
                    Box::new(crate::pull_request_detail::StartPullRequestSession {
                        prompt: "Review pull request #7".into(),
                        context: None,
                        project_id: "pr-project".into(),
                        device: "remote".into(),
                        repository: "a/b".into(),
                        own_chat: None,
                    }),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                assert!(
                    shell.state.read(cx).selected_chat.is_none(),
                    "a new session opens"
                );
                assert_eq!(
                    shell.state.read(cx).selected_space.as_deref(),
                    Some("pr-project")
                );
                assert_eq!(
                    shell.state.read(cx).effective_device_id().as_deref(),
                    Some("remote")
                );
                let composer = shell.composer.read(cx);
                assert_eq!(
                    composer.input.read(cx).text(),
                    "half-written reply\n\nReview pull request #7",
                    "a draft already on the canvas stays above the prompt"
                );
                assert!(composer.focus_pending);
            })
            .unwrap();
    }

    #[gpui::test]
    fn pull_request_context_appends_to_the_draft_without_sending(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                let space = |id: &str, device: &str| zuse_protocol::Space {
                    id: id.into(),
                    device_id: device.into(),
                    path: format!("/{id}"),
                    name: None,
                    git_detected: true,
                    git_checked_at: None,
                    checkout_id: None,
                    repository_id: None,
                    remote_repository: None,
                    created_at: Utc::now(),
                };
                shell.state.update(cx, |state, cx| {
                    state.local_device_id = Some("local".into());
                    state.apply_spaces(vec![
                        space("unrelated", "local"),
                        space("pr-project", "remote"),
                    ]);
                    state.select_space(Some("unrelated".into()), cx);
                });
                shell.settings.space_filter = Some("unrelated".into());
                shell.composer.update(cx, |composer, cx| {
                    composer
                        .input
                        .update(cx, |input, cx| input.set_text("half-written reply", cx))
                });
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(
                    Box::new(crate::pull_request_detail::AddPullRequestContext(
                        "Review feedback snapshot".into(),
                    )),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, cx| {
                assert!(
                    shell.state.read(cx).selected_chat.is_none(),
                    "a new session opens"
                );
                assert_eq!(
                    shell.state.read(cx).selected_space.as_deref(),
                    Some("unrelated")
                );
                assert!(
                    shell.state.read(cx).chats.is_empty(),
                    "adding context never creates or sends a chat"
                );
                let composer = shell.composer.read(cx);
                assert_eq!(
                    composer.input.read(cx).text(),
                    "half-written reply\n\nReview feedback snapshot",
                    "a draft already on the canvas stays above the prompt"
                );
                assert!(composer.focus_pending);
            })
            .unwrap();
    }

    #[gpui::test]
    fn opening_terminals_focuses_the_terminal_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let mut drawer_window = None;
        for embedded in [false, false, true] {
            let panel = window
                .update(cx, |shell, window, cx| {
                    shell.open_chat("terminal-session".into(), cx);
                    shell.active_chat = "terminal-session".into();
                    let panel = if embedded {
                        shell.add_terminal_surface(cx);
                        shell.right_terminal.clone().unwrap()
                    } else {
                        shell.toggle_terminal(window, cx);
                        shell.terminal.clone().unwrap()
                    };
                    assert!(!shell.composer.read(cx).focus_pending);
                    panel
                })
                .unwrap();
            let terminal_window = if !embedded && drawer_window.is_some() {
                drawer_window.unwrap()
            } else {
                let handle = cx.update(|cx| {
                    cx.open_window(gpui::WindowOptions::default(), |_, _| panel.clone())
                        .unwrap()
                });
                if !embedded {
                    drawer_window = Some(handle);
                }
                handle
            };
            cx.update_window(terminal_window.into(), |_, window, cx| {
                window.draw(cx).clear();
                assert!(
                    panel.read(cx).focus_handle().is_focused(window),
                    "embedded: {embedded}"
                );
                // Ordinary redraws must not steal focus from another control.
                let other_input = cx.focus_handle();
                window.focus(&other_input, cx);
                window.draw(cx).clear();
                assert!(other_input.is_focused(window));
            })
            .unwrap();
            if !embedded {
                window
                    .update(cx, |shell, window, cx| {
                        shell.toggle_terminal(window, cx);
                        assert!(shell.composer.focus_handle(cx).is_focused(window));
                    })
                    .unwrap();
            }
        }
    }

    #[gpui::test]
    fn session_navigation_focuses_composer_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        // Render the real composer in its own window to exercise mounting
        // without the shell's boot/connection gate hiding the destination.
        let composer_window = cx.update(|cx| {
            let composer = window.read(cx).unwrap().composer.clone();
            cx.open_window(gpui::WindowOptions::default(), |_, _| composer)
                .unwrap()
        });
        for destination in ["initial", "chat", "chat", "new", "new", "back", "settings"] {
            window
                .update(cx, |shell, _, cx| match destination {
                    "chat" => shell.open_chat("existing-session".into(), cx),
                    "new" => shell.open_new_session(None, cx),
                    "back" => shell.apply_nav(NavEntry::Chat("existing-session".into()), cx),
                    "settings" => {
                        shell.open_settings(SettingsSection::Devices, cx);
                        shell.close_settings(cx);
                    }
                    _ => {}
                })
                .unwrap();
            cx.update_window(composer_window.into(), |composer, window, cx| {
                window.draw(cx).clear();
                // Closing a floating settings dialog restores its origin in
                // the shell; it must not request focus in the composer.
                assert_eq!(
                    composer
                        .downcast::<Composer>()
                        .unwrap()
                        .focus_handle(cx)
                        .is_focused(window),
                    destination != "settings",
                    "{destination}"
                );
                // Subsequent renders after a click-away must not reclaim it.
                window.blur();
                window.draw(cx).clear();
                assert!(window.focused(cx).is_none(), "{destination}");
            })
            .unwrap();
        }
    }

    #[gpui::test]
    fn projectless_new_session_restores_opt_out_and_clears_sidebar_filter(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        crate::settings::composer::ComposerDefaults {
            device: Some("remote".into()),
            no_project: true,
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, _| {
                    state.apply_spaces(vec![zuse_protocol::Space {
                        id: "repo".into(),
                        device_id: "local".into(),
                        path: "/repo".into(),
                        name: None,
                        git_detected: false,
                        git_checked_at: None,
                        checkout_id: None,
                        repository_id: None,
                        remote_repository: None,
                        created_at: Utc::now(),
                    }]);
                    // Boot opened an existing project session after loading defaults.
                    state.selected_chat = Some("existing-project-chat".into());
                    state.no_project = false;
                });
                shell.settings.space_filter = None;
                shell.open_new_session(None, cx);
                assert!(shell.state.read(cx).no_project);
                assert!(shell.state.read(cx).selected_space.is_none());
                assert_eq!(
                    shell.state.read(cx).effective_device_id().as_deref(),
                    Some("remote")
                );

                // An explicit filter may select its project, but opting out again
                // must remove that filter before the first send can hide the chat.
                shell.set_space_filter(Some("repo".into()), cx);
                shell
                    .state
                    .update(cx, |state, cx| state.select_space(None, cx));
                shell.on_state_changed(&shell.state.clone(), cx);
                assert!(shell.settings.space_filter.is_none());
                assert!(shell.state.read(cx).no_project);
            })
            .unwrap();
    }

    #[gpui::test]
    fn new_session_reopens_where_it_was_left_after_visiting_a_chat(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let space = |id: &str, device: &str| zuse_protocol::Space {
            id: id.into(),
            device_id: device.into(),
            path: format!("/{id}"),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            repository_id: None,
            remote_repository: None,
            created_at: Utc::now(),
        };
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, cx| {
                    state.apply_spaces(vec![space("mine", "local"), space("other", "remote")]);
                    state.apply_chats(vec![zuse_protocol::Chat {
                        id: "elsewhere".into(),
                        device_id: "remote".into(),
                        title: None,
                        archived: false,
                        cwd: None,
                        branch: None,
                        checkout_id: None,
                        source_context: None,
                        config: None,
                        last_message_preview: None,
                        last_message_at: None,
                        created_at: Utc::now(),
                        harness_session_id: None,
                        harness_session_cwd: None,
                        parent_chat_id: None,
                        thread_id: None,
                        space_id: Some("other".into()),
                        last_seen_at: None,
                        room_gen: None,
                    }]);
                    state.select_space(Some("mine".into()), cx);
                });
                shell.settings.space_filter = None;
                shell.open_chat("elsewhere".into(), cx);
                assert_eq!(
                    shell.state.read(cx).selected_space.as_deref(),
                    Some("other")
                );
                shell.open_new_session(None, cx);
                let state = shell.state.read(cx);
                assert!(state.selected_chat.is_none());
                assert_eq!(state.selected_space.as_deref(), Some("mine"));
                assert_eq!(state.effective_device_id().as_deref(), Some("local"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn new_session_in_project_homes_the_canvas_on_that_project(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let space = |id: &str| zuse_protocol::Space {
            id: id.into(),
            device_id: "local".into(),
            path: format!("/{id}"),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            repository_id: None,
            remote_repository: None,
            created_at: Utc::now(),
        };
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, _| {
                    state.apply_spaces(vec![space("one"), space("two")]);
                });
                shell.settings.space_filter = None;
                shell.open_new_session(Some("two".into()), cx);
                let state = shell.state.read(cx);
                assert!(state.selected_chat.is_none());
                assert_eq!(state.selected_space.as_deref(), Some("two"));

                // The explicit project wins over a standing filter.
                shell.settings.space_filter = Some("one".into());
                shell.open_new_session(Some("two".into()), cx);
                assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("two"));
            })
            .unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[gpui::test]
    fn transcript_links_open_new_tabs_and_reject_stale_sessions(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let weak = window
            .update(cx, |shell, window, cx| {
                use crate::markdown::render::{
                    LinkAction, LinkActivation, LinkOutcome, LinkTarget,
                };
                shell.active_chat = "first-session".into();
                shell.state.update(cx, |state, _| {
                    state.selected_chat = Some("first-session".into())
                });
                let mut activation = LinkActivation {
                    target: LinkTarget::new("Docs", "https://example.com/docs"),
                    action: LinkAction::Primary,
                    source_session: Some("first-session".into()),
                };
                assert!(!shell.right_pane_open(cx));
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                assert!(shell.right_pane_open(cx));
                let first = shell.browser_seq;
                assert_eq!(
                    shell.browsers[&first].read(cx).page.url.as_deref(),
                    Some("https://example.com/docs")
                );
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(first)
                );
                shell.activate_session_link(&activation, window, cx);
                assert_eq!(shell.browsers.len(), 2);
                settings::update(settings::SavePolicy::Immediate, cx, |settings| {
                    settings.open_web_links_in_zuse = false;
                });
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::External("https://example.com/docs".into())
                );
                assert_eq!(shell.browsers.len(), 2);
                activation.action = LinkAction::Internal;
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal,
                    "the explicit internal action ignores the default preference"
                );
                assert_eq!(shell.browsers.len(), 3);
                activation.action = LinkAction::External;
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::External("https://example.com/docs".into())
                );
                assert_eq!(shell.browsers.len(), 3);
                activation.source_session = Some("other-session".into());
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Rejected
                );
                activation.source_session = Some("first-session".into());
                shell.state.update(cx, |state, _| {
                    state.selected_chat = Some("switch-in-progress".into())
                });
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Rejected
                );
                assert_eq!(shell.browsers.len(), 3);
                shell.state.update(cx, |state, _| {
                    state.selected_chat = Some("first-session".into())
                });
                shell.add_subagent_surface(
                    "first-session".into(),
                    "child-doc".into(),
                    "Child".into(),
                    true,
                    cx,
                );
                let child = shell
                    .subagent_tabs
                    .values()
                    .next()
                    .unwrap()
                    .transcript
                    .clone();
                let ui = child
                    .update(cx, |transcript, cx| transcript.link_ui(cx))
                    .unwrap();
                assert_eq!(ui.source_session.as_deref(), Some("first-session"));
                activation.action = LinkAction::Internal;
                activation.source_session = ui.source_session;
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                assert_eq!(shell.browsers.len(), 4);
                let weak = shell.browsers[&first].downgrade();
                shell.close_right_surface(RightSurface::Browser(first), window, cx);
                weak
            })
            .unwrap();
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }

    /// Encoded destinations open the decoded path, and an absolute
    /// destination no root owns opens through the linking chat's own file
    /// context with its absolute path intact.
    #[cfg(unix)]
    #[gpui::test]
    fn encoded_and_outside_file_links_open_in_the_linking_chats_context(cx: &mut TestAppContext) {
        use crate::markdown::render::{LinkAction, LinkActivation, LinkOutcome, LinkTarget};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("2026-09-26/Some Folder")).unwrap();
        std::fs::write(
            root.join("2026-09-26/Some Folder/it's here.txt"),
            "encoded\n",
        )
        .unwrap();
        let outside = dir.path().join("outside/INFORME.md");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "# Informe\n").unwrap();
        cx.update(|cx| {
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, window, cx| {
                let mut owner = super::tests::chat_with_path(Some(&root.to_string_lossy()), None);
                owner.id = "owner".into();
                owner.device_id = "local".into();
                shell.active_chat = "owner".into();
                shell.state.update(cx, |state, _| {
                    state.local_device_id = Some("local".into());
                    state.apply_chats(vec![owner]);
                    state.selected_chat = Some("owner".into());
                });

                let mut activation = LinkActivation {
                    target: LinkTarget::new("enc", "2026-09-26/Some%20Folder/it%27s%20here.txt"),
                    action: LinkAction::Primary,
                    source_session: Some("owner".into()),
                };
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                let id = shell.file_surface_seq;
                assert_eq!(
                    shell.file_surface_paths.get(&id).map(String::as_str),
                    Some("2026-09-26/Some Folder/it's here.txt")
                );

                // The absolute spelling of the same file resolves in-root.
                let absolute = format!(
                    "{}/2026-09-26/Some%20Folder/it%27s%20here.txt",
                    root.display()
                );
                activation.target = LinkTarget::new("abs", &absolute);
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                let id = shell.file_surface_seq;
                assert_eq!(
                    shell.file_surface_paths.get(&id).map(String::as_str),
                    Some("2026-09-26/Some Folder/it's here.txt")
                );
                assert_eq!(shell.file_surfaces[&id].read(cx).chat_id(), "owner");

                // Outside every known root the absolute path still opens,
                // through the linking chat.
                let outside_target = outside.to_string_lossy().into_owned();
                activation.target = LinkTarget::new("outside", &outside_target);
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                let id = shell.file_surface_seq;
                assert_eq!(
                    shell.file_surface_paths.get(&id).map(String::as_str),
                    Some(outside_target.as_str())
                );
                assert_eq!(shell.file_surfaces[&id].read(cx).chat_id(), "owner");

                // A path under another project root on this device keeps its
                // absolute spelling: its root-relative tail would name a
                // different file under the linking chat's checkout.
                let project = dir.path().join("other-project");
                shell.state.update(cx, |state, _| {
                    state.apply_spaces(vec![zuse_protocol::Space {
                        id: "other".into(),
                        device_id: "local".into(),
                        path: project.to_string_lossy().into_owned(),
                        name: None,
                        git_detected: false,
                        git_checked_at: None,
                        checkout_id: None,
                        repository_id: None,
                        remote_repository: None,
                        created_at: Utc::now(),
                    }]);
                });
                let project_target = project.join("notes.md").to_string_lossy().into_owned();
                activation.target = LinkTarget::new("project", &project_target);
                assert_eq!(
                    shell.activate_session_link(&activation, window, cx),
                    LinkOutcome::Internal
                );
                let id = shell.file_surface_seq;
                assert_eq!(
                    shell.file_surface_paths.get(&id).map(String::as_str),
                    Some(project_target.as_str())
                );
                assert_eq!(shell.file_surfaces[&id].read(cx).chat_id(), "owner");
            })
            .unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[gpui::test]
    fn markdown_preview_events_open_browser_from_tree_and_file_tabs(cx: &mut TestAppContext) {
        use crate::markdown::render::{LinkAction, LinkActivation, LinkTarget};
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(settings::UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let surfaces = window
            .update(cx, |shell, window, cx| {
                shell.active_chat = "owner".into();
                shell
                    .state
                    .update(cx, |state, _| state.selected_chat = Some("owner".into()));
                shell.add_files_surface(window, cx);
                shell.add_file_surface("README.md".into(), window, cx);
                [
                    shell.files[&shell.panel_key(cx)].clone(),
                    shell.file_surfaces[&shell.file_surface_seq].clone(),
                ]
            })
            .unwrap();
        let mut last_external_url = None;
        for (index, surface) in surfaces.iter().enumerate() {
            let mut activation = LinkActivation {
                target: LinkTarget::new("Docs", &format!("https://example.com/preview/{index}")),
                action: LinkAction::Primary,
                source_session: Some("owner".into()),
            };
            surface.update(cx, |_, cx| {
                cx.emit(FilesEvent::OpenWebLink(activation.clone()))
            });
            cx.run_until_parked();
            assert_eq!(cx.opened_url(), last_external_url);
            window
                .update(cx, |shell, _, cx| {
                    assert!(shell.right_pane_open(cx));
                    assert_eq!(shell.browsers.len(), index + 1);
                    assert_eq!(
                        shell.resolved_right_active(cx),
                        RightSurface::Browser(shell.browser_seq)
                    );
                    assert_eq!(
                        shell.browsers[&shell.browser_seq]
                            .read(cx)
                            .page
                            .url
                            .as_deref(),
                        Some(activation.target.original.as_str())
                    );
                })
                .unwrap();
            activation.action = LinkAction::External;
            surface.update(cx, |_, cx| {
                cx.emit(FilesEvent::OpenWebLink(activation.clone()))
            });
            cx.run_until_parked();
            assert_eq!(
                cx.opened_url().as_deref(),
                Some(activation.target.original.as_str())
            );
            last_external_url = Some(activation.target.original.clone());
            activation.target = LinkTarget::new("Stale", "https://example.com/stale");
            activation.source_session = Some("stale-owner".into());
            for action in [LinkAction::Internal, LinkAction::External] {
                activation.action = action;
                surface.update(cx, |_, cx| {
                    cx.emit(FilesEvent::OpenWebLink(activation.clone()))
                });
                cx.run_until_parked();
                assert_eq!(cx.opened_url(), last_external_url);
            }
            window
                .update(cx, |shell, _, _| {
                    assert_eq!(shell.browsers.len(), index + 1)
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn browser_profile_changes_clear_tabs_and_reselect_the_stable_identity(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, window, cx| {
                shell.active_chat = "session".into();
                shell.state.update(cx, |state, _| {
                    state.workspace_scope = Some(zuse_protocol::WorkspaceScope::Local);
                    state.local_device_id = Some("device-a".into());
                });
                // Selecting the identity happens before navigation, without requiring a render.
                shell.add_browser_surface(None, window, cx);
                let original = shell.browser_profile.clone().unwrap();
                let original_cache = shell.pull_request_cache.clone();
                shell.pull_requests_page =
                    Some(cx.new(|cx| PullRequestsPage::new(shell.state.clone(), cx)));
                shell.active_chat = "another-chat".into();
                shell.add_browser_surface(None, window, cx);
                assert_eq!(shell.browsers.len(), 2);
                assert_eq!(shell.browser_profile.as_ref(), Some(&original));
                assert!(std::rc::Rc::ptr_eq(
                    &shell.pull_request_cache,
                    &original_cache
                ));
                assert!(shell.pull_requests_page.is_some());
                shell.state.update(cx, |state, _| {
                    state.local_device_id = Some("device-b".into())
                });
                shell.sync_browser_profile(cx);
                assert!(shell.browsers.is_empty());
                assert!(shell.browser_subs.is_empty());
                assert_ne!(shell.browser_profile.as_ref(), Some(&original));
                assert!(!std::rc::Rc::ptr_eq(
                    &shell.pull_request_cache,
                    &original_cache
                ));
                assert!(shell.pull_requests_page.is_none());
                shell.state.update(cx, |state, _| {
                    state.local_device_id = Some("device-a".into())
                });
                shell.sync_browser_profile(cx);
                assert_eq!(shell.browser_profile.as_ref(), Some(&original));
                shell.add_browser_surface(None, window, cx);
                let restored_cache = shell.pull_request_cache.clone();
                shell.pull_requests_page =
                    Some(cx.new(|cx| PullRequestsPage::new(shell.state.clone(), cx)));
                shell
                    .state
                    .update(cx, |state, _| state.workspace_scope = None);
                shell.sync_browser_profile(cx);
                assert!(shell.browser_profile.is_none());
                assert!(shell.browsers.is_empty());
                assert!(!std::rc::Rc::ptr_eq(
                    &shell.pull_request_cache,
                    &restored_cache
                ));
                assert!(shell.pull_requests_page.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn browser_tabs_keep_session_ownership_and_release_on_close(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        let weak = window
            .update(cx, |shell, window, cx| {
                // A fresh-session canvas never accumulates ownerless tabs.
                shell.add_browser_surface(None, window, cx);
                assert!(shell.browsers.is_empty());
                shell.active_chat = "first-session".into();
                shell.add_browser_surface(None, window, cx);
                let first = shell.browser_seq;
                let weak = shell.browsers[&first].downgrade();
                shell.add_browser_surface(None, window, cx);
                let second = shell.browser_seq;
                assert_ne!(first, second);
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(second)
                );
                shell.active_chat = "second-session".into();
                assert!(shell.right_surface_rows(cx).is_empty());
                shell.add_browser_surface(None, window, cx);
                let other = shell.browser_seq;
                shell.active_chat = "first-session".into();
                assert_eq!(shell.right_surface_rows(cx).len(), 2);
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(second)
                );
                // Closing a background tab preserves the selected address input.
                let focus = window.focused(cx);
                shell.close_right_surface(RightSurface::Browser(first), window, cx);
                assert_eq!(window.focused(cx), focus);
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(second)
                );
                shell.close_right_surface(RightSurface::Browser(second), window, cx);
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Picker);
                shell.active_chat = "second-session".into();
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(other)
                );
                shell.close_right_surface(RightSurface::Browser(other), window, cx);
                assert!(shell.browsers.is_empty());
                assert!(shell.browser_subs.is_empty());
                weak
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            weak.upgrade().is_none(),
            "closed browser retained by callbacks"
        );
    }

    #[gpui::test]
    fn cmd_w_closes_the_active_right_pane_surface_before_the_window(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, window, cx| {
                shell.active_chat = "session".into();
                shell.toggle_right_pane(cx);
                assert!(shell.right_pane_open(cx));

                shell.add_browser_surface(None, window, cx);
                let first = shell.browser_seq;
                shell.add_browser_surface(None, window, cx);
                let second = shell.browser_seq;
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(second)
                );

                shell.add_browser_surface(None, window, cx);
                let third = shell.browser_seq;
                let middle = RightSurface::Browser(second);
                assert_eq!(
                    shell.tabs_to_close(middle, TabCloseAction::Left, cx),
                    vec![RightSurface::Browser(first)]
                );
                assert_eq!(
                    shell.tabs_to_close(middle, TabCloseAction::Right, cx),
                    vec![RightSurface::Browser(third)]
                );
                assert_eq!(
                    shell.tabs_to_close(middle, TabCloseAction::Others, cx),
                    vec![RightSurface::Browser(first), RightSurface::Browser(third)]
                );
                assert_eq!(
                    shell.tabs_to_close(middle, TabCloseAction::This, cx),
                    vec![middle]
                );
                assert!(
                    shell
                        .tabs_to_close(RightSurface::Browser(first), TabCloseAction::Left, cx)
                        .is_empty()
                );
                assert!(
                    shell
                        .tabs_to_close(RightSurface::Browser(third), TabCloseAction::Right, cx)
                        .is_empty()
                );
                for tab in shell.tabs_to_close(middle, TabCloseAction::Right, cx) {
                    shell.close_right_surface(tab, window, cx);
                }
                shell.set_right_active(middle, cx);

                // ⌘W closes the active tab, not the window.
                assert!(shell.close_active_surface(window, cx));
                assert_eq!(
                    shell.resolved_right_active(cx),
                    RightSurface::Browser(first)
                );

                // …and again for the last tab, which also collapses the
                // surface host (nothing left to show).
                assert!(shell.close_active_surface(window, cx));
                assert_eq!(shell.resolved_right_active(cx), RightSurface::Picker);
                assert!(!shell.right_pane_open(cx));

                // A closed pane falls through to the window-close rung
                // instead of being consumed.
                assert!(!shell.close_active_surface(window, cx));

                // So does an open pane with nothing left to close.
                shell.toggle_right_pane(cx);
                assert!(shell.right_pane_open(cx));
                assert!(!shell.close_active_surface(window, cx));
            })
            .unwrap();
    }

    #[gpui::test]
    fn lifecycle_actions_keep_pending_and_failed_file_saves_alive(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        for failed in [false, true] {
            window
                .update(cx, |shell, window, cx| {
                    window.activate_window();
                    let state = shell.state.clone();
                    let files = cx.new(|cx| {
                        let mut files = FilesSurface::new(
                            state,
                            "test".into(),
                            false,
                            1000,
                            13.0,
                            false,
                            false,
                            cx,
                        );
                        files.seed_pending_exit_test_document(failed);
                        files
                    });
                    shell.file_surfaces.insert(0, files);
                    shell
                        .file_surface_keys
                        .insert(("test".into(), "test".into(), "test.rs".into()), 0);
                })
                .unwrap();
            cx.update(|cx| cx.dispatch_action(&crate::app_menus::Quit));
            cx.run_until_parked();
            window
                .update(cx, |shell, _, cx| {
                    assert!(matches!(shell.pending_exit, Some(PendingExit::Quit)));
                    assert!(!shell.all_file_edits_flushed(cx));
                    shell.cancel_file_close(RightSurface::File(0), cx);
                    assert!(shell.pending_exit.is_none());
                })
                .unwrap();
            cx.update(|cx| cx.dispatch_action(&crate::app_menus::CloseWindow));
            cx.run_until_parked();
            window
                .update(cx, |shell, _, cx| {
                    assert!(matches!(shell.pending_exit, Some(PendingExit::CloseWindow)));
                    shell.quit_for_runtime_change(cx);
                    assert!(matches!(
                        shell.pending_exit,
                        Some(PendingExit::RuntimeChange)
                    ));
                    assert!(shell.runtime_change_task.is_none());
                    shell.apply_staged_update(PathBuf::from("must-not-install"), cx);
                    assert!(matches!(
                        shell.pending_exit,
                        Some(PendingExit::InstallUpdate(_))
                    ));
                    shell.cancel_file_close(RightSurface::File(0), cx);
                    assert!(shell.pending_exit.is_none());
                })
                .unwrap();
        }
    }
}

/// Native browser regression fixture hooks are excluded from shipped builds.
#[cfg(feature = "browser-fixture")]
impl Shell {
    pub fn fixture_shortcut_visual(
        &mut self,
        kind: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcuts_panel_open = false;
        self.transcript.update(cx, |t, cx| t.close_find(cx));
        match kind {
            "find" | "find-empty" => {
                self.transcript.update(cx, |t, cx| {
                    t.open_find(window, cx);
                    t.set_find_query(
                        if kind == "find" {
                            "repo"
                        } else {
                            "unmatched-query"
                        },
                        cx,
                    );
                });
            }
            "shortcuts" => self.shortcuts_panel_open = true,
            _ => self
                .composer
                .update(cx, |c, cx| c.fixture_editing_visual(kind, window, cx)),
        }
        cx.notify();
    }

    pub fn fixture_focus_mounted(&self, window: &Window, cx: &App) -> bool {
        self.shortcut_focus.contains_focused(window, cx)
    }
    pub fn fixture_browser_annotation_comments(&self, cx: &App) -> Vec<String> {
        let composer = self.composer.read(cx);
        self.state
            .read(cx)
            .context_items(&composer.current_key)
            .iter()
            .filter_map(|item| {
                item.browser_annotation
                    .as_ref()
                    .map(|annotation| annotation.comment.clone())
            })
            .collect()
    }
    pub fn fixture_active_browser(
        &self,
        cx: &App,
    ) -> Option<(u64, Entity<crate::browser::BrowserSurface>)> {
        let RightSurface::Browser(id) = self.resolved_right_active(cx) else {
            return None;
        };
        self.browsers.get(&id).cloned().map(|browser| (id, browser))
    }
    pub fn fixture_open_browser(
        &mut self,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (u64, Entity<crate::browser::BrowserSurface>) {
        self.set_surfaces_open(true, cx);
        // Hosted Macs can expose only a 1024px desktop. Use the app's
        // normal collapsed-sidebar layout to keep both conversation and
        // preview readable in that real window.
        if f32::from(window.viewport_size().width) < 1200.0 {
            self.settings.sidebar_collapsed = true;
        }
        self.add_browser_surface(url, window, cx);
        (self.browser_seq, self.browsers[&self.browser_seq].clone())
    }
    pub fn fixture_select_browser(&mut self, id: u64, cx: &mut Context<Self>) {
        self.set_right_active(RightSurface::Browser(id), cx);
    }
    pub fn fixture_close_browser(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.close_right_surface(RightSurface::Browser(id), window, cx);
    }
    pub fn fixture_browser_menu(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.right_plus.open(());
            cx.notify();
        } else {
            self.close_right_plus(cx);
        }
    }
    pub fn fixture_browser_menu_mounted(&self) -> bool {
        self.right_plus.get().is_some()
    }
    pub fn fixture_expand_browser(&mut self, cx: &mut Context<Self>) {
        self.toggle_right_pane_expand(cx);
    }
    pub fn fixture_blur_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.route = Route::Settings(SettingsSection::Devices);
        window.blur();
        cx.notify();
    }
    pub fn fixture_toggle_sidebar(&mut self, right: bool, cx: &mut Context<Self>) {
        if right {
            // The fixtures drive the surface host only; the explorer stays put.
            self.set_surfaces_open(!self.right_pane_open(cx), cx);
        } else {
            self.toggle_sidebar(cx);
        }
    }
    pub fn fixture_resize_browser(&mut self, width: f32, cx: &mut Context<Self>) {
        self.settings.right_pane_width = width;
        cx.notify();
    }
    /// `main` closes Settings; `settings/<slug>` opens that page.
    /// Opens the composer's model picker (review route `main/model-picker`).
    pub fn fixture_open_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pickers = self.composer.read(cx).pickers().clone();
        pickers.update(cx, |pickers, cx| {
            pickers.fixture_seed_catalog(cx);
            pickers.open_model_menu(window, cx);
        });
    }
    /// Opens a composer menu (review routes `<page>/access-menu`,
    /// `<page>/device-menu`, `<page>/project-menu`) or Cmd+K
    /// (`<page>/palette-menu`); `new/…` starts on the new-chat canvas.
    pub fn fixture_open_menu(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.fixture_reset_panels(cx);
        if route.starts_with("new/") {
            self.open_new_session(None, cx);
        }
        let composer = self.composer.clone();
        let pickers = composer.read(cx).pickers().clone();
        if route.ends_with("access-auto-menu") {
            // "Approve for me" selected: the chip and the menu's check. Wait
            // out the new-chat canvas resetting the draft picks.
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                composer.update(cx, |composer, cx| {
                    composer.fixture_set_permission(zuse_protocol::SandboxLevel::Auto, cx);
                    composer.fixture_open_permissions(cx);
                });
            })
            .detach();
        } else if route.ends_with("access-menu") {
            composer.update(cx, |composer, cx| composer.fixture_open_permissions(cx));
        } else if route.ends_with("device-menu") {
            pickers.update(cx, |p, cx| {
                p.fixture_open(crate::pickers::PickerKind::Device, window, cx)
            });
        } else if route.ends_with("project-menu") {
            pickers.update(cx, |p, cx| {
                p.fixture_open(crate::pickers::PickerKind::Space, window, cx)
            });
        } else if route.ends_with("palette-menu") {
            self.toggle_command_palette(window, cx);
        } else if route.ends_with("sidebar-view-menu") {
            self.open_sidebar_view_menu(window, cx);
        } else if route.ends_with("branch-menu") {
            self.fixture_open_branch_menu(cx);
        } else if route.ends_with("tabs-history-menu") {
            self.conversation_history.open(());
            cx.notify();
        } else if route.ends_with("plus-menu") {
            // The right pane's `+` menu with the Files tab open and a pull
            // request on the chat's branch.
            self.fixture_open_panel("panel/files", window, cx);
            self.fixture_seed_pull_request(cx);
            self.right_plus.open(());
            cx.notify();
        } else if route.ends_with("open-in-menu") {
            self.fixture_open_panel("panel/files", window, cx);
            self.fixture_open_in_menu(cx);
        } else if route.ends_with("outline-menu") {
            // The outline needs a wide conversation column.
            self.set_surfaces_open(false, cx);
            self.fixture_seed_transcript(true, cx);
            self.transcript
                .update(cx, |transcript, cx| transcript.fixture_open_outline(cx));
        }
    }

    /// Close what an earlier capture route left open: the pane's `+` menu,
    /// the Open in menu, and a main-area tab over the conversation.
    fn fixture_reset_panels(&mut self, cx: &mut Context<Self>) {
        self.right_plus = popover::Popup::default();
        self.open_in.menu = popover::Popup::default();
        self.branch_menu.menu = popover::Popup::default();
        self.activate_main_tab(None, cx);
        cx.notify();
    }

    /// Give the selected chat a branch with an open pull request.
    fn fixture_seed_pull_request(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let selected = state.selected_chat.clone();
            let Some(chat) = state
                .chats
                .iter_mut()
                .find(|chat| Some(&chat.id) == selected.as_ref())
            else {
                return;
            };
            chat.source_context = Some(zuse_protocol::ConversationSourceContext {
                checkout_id: "fixture-checkout".into(),
                repo_root: "/tmp/fieldnotes".into(),
                cwd: "/tmp/fieldnotes".into(),
                branch: "fieldnotes/workspace".into(),
                head_sha: None,
                observed_at: Utc::now(),
            });
            #[cfg(feature = "project-palette-fixture")]
            {
                let chat = chat.clone();
                state.fixture_sidebar_change_request(zuse_protocol::CheckoutChangeRequestStatus {
                    error: None,
                    checkout_id: "fixture-checkout".into(),
                    device_id: chat.device_id.clone(),
                    cwd: "/tmp/fieldnotes".into(),
                    branch: "fieldnotes/workspace".into(),
                    updated_at: Utc::now(),
                    change_request: Some(zuse_protocol::ChangeRequestSummary {
                        provider: "github".into(),
                        number: 418,
                        title: "Build the Fieldnotes workspace".into(),
                        url: "https://github.com/example/fieldnotes/pull/418".into(),
                        state: zuse_protocol::ChangeRequestState::Open,
                        base_ref: "main".into(),
                        head_ref: "fieldnotes/workspace".into(),
                        is_draft: false,
                        review_decision: Default::default(),
                    }),
                });
            }
            cx.notify();
        });
    }
    /// Right-pane and main-area tab states for review captures:
    /// `panel/launcher` (the empty pane's surface list), `panel/files` (the
    /// explorer over a synthetic tree) and `main/review` (the review tab in
    /// the main area, beside a side chat's tab).
    pub fn fixture_open_panel(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.fixture_reset_panels(cx);
        self.state.update(cx, |state, cx| {
            if let Some(space) = state.spaces.iter_mut().find(|space| space.id == "project") {
                space.git_detected = true;
            }
            let selected = state.selected_chat.clone();
            if let Some(chat) = state
                .chats
                .iter_mut()
                .find(|chat| Some(&chat.id) == selected.as_ref())
            {
                chat.checkout_id = Some("fixture-checkout".into());
                chat.cwd = Some("/tmp/fieldnotes".into());
            }
            cx.notify();
        });
        match route {
            "panel/launcher" => self.set_surfaces_open(true, cx),
            "panel/git" => {
                self.open_review_tab(window, cx);
                let frame = serde_json::json!({"refreshing": false, "snapshot": {
                    "chatId": "fixture-git", "checkoutId": "fixture-checkout", "cwd": "/tmp/fieldnotes",
                    "branch": "alex/connection-handoff", "headSha": "abc1234",
                    "upstream": "origin/alex/connection-handoff", "ahead": 2, "behind": 0,
                    "complete": true, "revision": "r1",
                    "files": [
                        {"path": "apps/desktop/src/handoff.rs", "oldPath": null, "index": "modified", "worktree": "unchanged"},
                        {"path": "apps/desktop/src/session.rs", "oldPath": null, "index": "modified", "worktree": "modified"},
                        {"path": "crates/engine/src/connection.rs", "oldPath": null, "index": "unchanged", "worktree": "modified"},
                        {"path": "crates/engine/src/retry.rs", "oldPath": null, "index": "unchanged", "worktree": "untracked"},
                        {"path": "README.md", "oldPath": null, "index": "unchanged", "worktree": "modified"},
                    ],
                }});
                if let Some(id) = self.active_main_diff(cx)
                    && let Some(changes) = self.diffs.get(&id).cloned()
                {
                    changes.update(cx, |changes, cx| {
                        changes.fixture_seed_diff(FIXTURE_REVIEW_PATCH, cx);
                        changes.fixture_seed_git(frame, cx);
                    });
                }
            }
            "panel/files" => {
                self.add_files_surface(window, cx);
                if let Some(files) = self.files.get(&self.panel_key(cx)).cloned() {
                    files.update(cx, |files, cx| {
                        files.fixture_seed_tree(
                            &[
                                (
                                    "",
                                    &[
                                        ".github/",
                                        "apps/",
                                        "crates/",
                                        "docs/",
                                        ".gitignore",
                                        "Cargo.toml",
                                        "README.md",
                                        "package.json",
                                        "tsconfig.json",
                                    ],
                                ),
                                ("apps", &["desktop/", "web/"]),
                                (
                                    "apps/desktop",
                                    &[
                                        "src/",
                                        "electron-builder.yml",
                                        "index.html",
                                        "main.ts",
                                        "vite.config.ts",
                                        "styles.css",
                                    ],
                                ),
                                ("apps/desktop/src", &["App.tsx", "main.rs", "logo.svg"]),
                            ],
                            cx,
                        )
                    });
                }
            }
            "main/file" => {
                // A file opened from the tree, as a main-area tab beside the
                // conversation; the tree stays in the right pane.
                self.fixture_open_panel("panel/files", window, cx);
                self.add_file_surface("apps/desktop/src/main.rs".into(), window, cx);
                if let Some(id) = self.active_main_file(cx)
                    && let Some(file) = self.file_surfaces.get(&id).cloned()
                {
                    file.update(cx, |file, cx| {
                        file.fixture_seed_document(FIXTURE_FILE_TEXT, cx)
                    });
                }
            }
            "main/review" => {
                self.state.update(cx, |state, cx| {
                    let Some(mut side) = state.selected_chat_row().cloned() else {
                        return;
                    };
                    side.parent_chat_id = Some(side.id.clone());
                    side.id = "fixture-side-chat".into();
                    side.title = Some("Explore caching options".into());
                    if !state.chats.iter().any(|chat| chat.id == side.id) {
                        state.chats.push(side);
                    }
                    cx.notify();
                });
                // The review needs the room: fold the pane and the tree.
                self.close_files_panel(window, cx);
                self.set_surfaces_open(false, cx);
                self.open_review_tab(window, cx);
                if let Some(id) = self.active_main_diff(cx)
                    && let Some(changes) = self.diffs.get(&id).cloned()
                {
                    changes.update(cx, |changes, cx| {
                        changes.fixture_seed_diff(FIXTURE_REVIEW_PATCH, cx)
                    });
                }
            }
            _ => {}
        }
    }
    /// Collapse the Projects view groups of these space ids, so a review
    /// capture reaches the projects below them.
    pub fn fixture_collapse_sidebar_groups(&mut self, space_ids: &[&str], cx: &mut Context<Self>) {
        let keys: Vec<String> = {
            let state = self.state.read(cx);
            space_ids
                .iter()
                .filter_map(|id| state.space_row(id))
                .map(zuse_protocol::view::project_key)
                .collect()
        };
        for key in keys {
            let key = self.sidebar_group_collapse_key(&key);
            self.sidebar_collapsed_groups.insert(key);
        }
        cx.notify();
    }
    /// `pr-<variant>` opens a synthetic pull request beside the chat:
    /// `summary`, `summary-checks`, `code`, `code-open`, or `activity`.
    pub fn fixture_open_pull_request(
        &mut self,
        variant: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let url = crate::pull_request_detail::fixture_seed_cache(&self.pull_request_cache);
        self.open_chat_pull_request(url.clone(), None, window, cx);
        let page = self
            .chat_pull_requests
            .values()
            .find(|tab| tab.url == url)
            .map(|tab| tab.page.clone());
        if let Some(page) = page {
            page.update(cx, |page, cx| page.fixture_review(variant, cx));
        }
    }
    pub fn fixture_review_route(&mut self, route: &str, cx: &mut Context<Self>) {
        // `<route>:<variant>` seeds fixture data (plugins, providers, the ACP
        // dialog) for pages that otherwise wait on an engine.
        let (route, variant) = route.split_once(':').unwrap_or((route, ""));
        if route == "plugins" {
            self.close_settings(cx);
            settings::plugins::fixture_seed(self.state.clone(), variant, cx);
            self.open_plugins_page(cx);
            return;
        }
        match route
            .strip_prefix("settings/")
            .and_then(SettingsSection::from_slug)
        {
            Some(section) => self.open_settings(section, cx),
            None => self.close_settings(cx),
        }
        match route {
            "settings/plugins" => settings::plugins::fixture_seed(self.state.clone(), variant, cx),
            "settings/providers" => {
                let state = self.state.clone();
                let page = cx.new(|cx| HarnessesPage::new(state, cx));
                page.update(cx, |page, cx| page.fixture_seed(variant, cx));
                self.harnesses_page = Some(page);
            }
            // The new-session view with its context tray above the composer.
            "new-thread" => self.open_new_session(None, cx),
            _ => {}
        }
        // `main:usage` adds queued follow-ups and context/plan usage to the
        // worked conversation; `:usage-open` also pins the usage card open.
        if variant.starts_with("usage") {
            self.fixture_seed_transcript(false, cx);
            self.composer.update(cx, |composer, cx| {
                composer.fixture_seed_usage(variant == "usage-open", cx)
            });
        }
        // `main:tray-<kind>` seeds the composer's attached tray over the
        // worked conversation (offline, errors, sign-in, annotations,
        // attachments, plan, queue, or `stack` for several at once).
        if let Some(kind) = variant.strip_prefix("tray-") {
            if route != "new-thread" {
                self.fixture_seed_transcript(false, cx);
            }
            self.composer
                .update(cx, |composer, cx| composer.fixture_seed_trays(kind, cx));
            // `tray-queue` also shows the jump-to-latest pill above the tray.
            if kind == "queue" {
                self.transcript
                    .update(cx, |transcript, cx| transcript.fixture_show_jump_button(cx));
            }
        }
        // `prcard-flow-<stage>` keeps one turn across routes, so its card
        // moves from stage to stage instead of a new card each time.
        if let Some(stage) = variant.strip_prefix("prcard-flow-") {
            self.fixture_seed_pull_request_card(stage, true, cx);
        } else if let Some(stage) = variant.strip_prefix("prcard-") {
            self.fixture_seed_pull_request_card(stage, false, cx);
        }
        // `<route>:draft` types a prompt so the send key is live.
        if variant == "draft" {
            self.composer.update(cx, |composer, cx| {
                composer.fixture_set_draft("Tidy the onboarding copy and add a test", cx)
            });
        }
        // `main:transcript` seeds a worked conversation (tool calls, thinking,
        // subagents, a hovered reply footer); `:transcript-card` also opens
        // the session card over it, `:transcript-fork` the reply's Fork
        // menu; `:transcript-outline` adds a second prompt so the outline
        // control shows.
        if variant.starts_with("transcript") {
            self.fixture_seed_transcript(variant == "transcript-outline", cx);
            if variant.starts_with("transcript-card") {
                self.session_info_overlay = true;
            }
            // `:transcript-card-git` runs the chat in a worktree of a Git
            // project whose branch has a pull request, so the card lists
            // the checkout, branch and pull request rows.
            if variant == "transcript-card-git" || variant.starts_with("transcript-card-pr") {
                self.state.update(cx, |state, cx| {
                    if let Some(space) = state.spaces.iter_mut().find(|s| s.id == "project") {
                        space.git_detected = true;
                    }
                    let source = state.chats.iter().find(|c| c.id == "chat-0").cloned();
                    if let Some((source, chat)) =
                        source.zip(state.chats.iter_mut().find(|c| c.id == "browser-fixture"))
                    {
                        chat.branch = source.branch;
                        chat.source_context = source.source_context;
                        chat.checkout_id = source.checkout_id;
                        chat.cwd = Some("/tmp/worktrees/fieldnotes/quiet-owl".into());
                    }
                    cx.notify();
                });
            }
            // `:transcript-working` runs a turn, so the live status row shows.
            if variant == "transcript-working" {
                self.state.update(cx, |state, cx| {
                    state.apply_sessions(vec![zuse_protocol::Session {
                        last_completed_turn: None,
                        running_subagents: 1,
                        awaiting_input: None,
                        background_tasks: Vec::new(),
                        chat_id: "browser-fixture".into(),
                        device_id: "local".into(),
                        status: zuse_protocol::SessionStatus::Working,
                        started_at: Some(Utc::now() - chrono::Duration::seconds(42)),
                        updated_at: Utc::now(),
                    }]);
                    cx.notify();
                });
            }
            // `:transcript-card-git` also runs background work, with the
            // card's Background work section unfolded.
            if variant == "transcript-card-git" {
                self.session_info_folded.insert("background");
                let now = Utc::now();
                let task = |id: &str,
                            description: &str,
                            command: &str,
                            started: i64,
                            ended: Option<(i64, zuse_protocol::BackgroundTaskOutcome)>,
                            stoppable| {
                    zuse_protocol::BackgroundTask {
                        id: id.into(),
                        kind: zuse_protocol::BackgroundTaskKind::Shell,
                        description: description.into(),
                        command: Some(command.into()),
                        started_at: now - chrono::Duration::seconds(started),
                        ended_at: ended.map(|(at, _)| now - chrono::Duration::seconds(at)),
                        outcome: ended.map(|(_, outcome)| outcome),
                        stoppable,
                    }
                };
                self.state.update(cx, |state, cx| {
                    state.apply_sessions(vec![zuse_protocol::Session {
                        last_completed_turn: None,
                        running_subagents: 0,
                        awaiting_input: None,
                        background_tasks: vec![
                            task(
                                "bg-1",
                                "Start the dev server",
                                "pnpm dev --port 3000",
                                312,
                                None,
                                true,
                            ),
                            task(
                                "bg-2",
                                "cargo watch -x test",
                                "cargo watch -x test",
                                95,
                                None,
                                true,
                            ),
                            task(
                                "bg-3",
                                "npm run build",
                                "npm run build",
                                400,
                                Some((340, zuse_protocol::BackgroundTaskOutcome::Completed)),
                                false,
                            ),
                            task(
                                "bg-4",
                                "tail -f logs/server.log",
                                "tail -f logs/server.log",
                                600,
                                Some((420, zuse_protocol::BackgroundTaskOutcome::Stopped)),
                                false,
                            ),
                        ],
                        chat_id: "browser-fixture".into(),
                        device_id: "local".into(),
                        status: zuse_protocol::SessionStatus::Idle,
                        started_at: Some(now),
                        updated_at: now,
                    }]);
                    cx.notify();
                });
            }
            // `:transcript-card-pr…` opens the card's pull request menu
            // (`-repair` / `-merge` / `-status` its group).
            if let Some(rest) = variant.strip_prefix("transcript-card-pr") {
                let submenu = match rest {
                    "-repair" => Some(summary_pull_request::PrSubmenu::Repair),
                    "-merge" => Some(summary_pull_request::PrSubmenu::Merge),
                    "-status" => Some(summary_pull_request::PrSubmenu::Status),
                    _ => None,
                };
                self.fixture_seed_summary_pull_request(
                    "https://github.com/opcodehq/zuse/pull/412",
                    rest != "-row",
                    submenu,
                );
            }
            // `:transcript-fork` opens the Fork menu on the hovered reply.
            if variant == "transcript-fork" {
                self.transcript
                    .update(cx, |transcript, cx| transcript.fixture_fork_menu("a1", cx));
            }
            cx.notify();
        }
        // `main:tabs…` seeds a thread with three conversations, one closed
        // into the strip's history (`-hover` points at an inactive tab).
        if variant.starts_with("tabs") {
            self.fixture_seed_thread_conversations(variant == "tabs-hover", cx);
        }
        // `main:chips` seeds a conversation whose messages carry web links
        // and a draft holding every chip kind; `:chips-mention` also opens
        // the `@` popover.
        if variant.starts_with("chips") {
            self.fixture_seed_link_transcript(cx);
            self.composer.update(cx, |composer, cx| {
                composer.fixture_seed_chips(variant == "chips-mention", cx)
            });
        }
    }

    /// `main:prcard-<stage>`: a conversation whose turn opened a pull
    /// request, with its card seeded at `creating`, `conflicts`, `checks`,
    /// `ready` or `merged` (the branch, its commits beside main's and the
    /// composer's chips).
    fn fixture_seed_pull_request_card(&mut self, stage: &str, flow: bool, cx: &mut Context<Self>) {
        use zuse_protocol::branch_graph::{BranchGraph, ConflictHunk, MergeConflict, MergePreview};
        use zuse_protocol::{
            ChangeRequestCheck, GitHistoryCommit, GitHistoryRef, GitHistoryRefKind,
        };
        if stage == "none" {
            self.fixture_seed_create_pull_request(cx);
            return;
        }
        let url = "https://github.com/example/fieldnotes/pull/418";
        let now = Utc::now();
        let sha = |short: &str| format!("{short}{}", "0".repeat(40 - short.len()));
        let commit = |short: &str,
                      parents: &[&str],
                      subject: &str,
                      author: &str,
                      minutes: i64,
                      refs: &[(GitHistoryRefKind, &str)]| {
            GitHistoryCommit {
                sha: sha(short),
                parent_shas: parents.iter().map(|p| sha(p)).collect(),
                subject: subject.into(),
                author_name: author.into(),
                author_email: format!("{}@example.test", author.to_lowercase().replace(' ', ".")),
                authored_at: (now - chrono::Duration::minutes(minutes)).to_rfc3339(),
                refs: refs
                    .iter()
                    .map(|(kind, label)| GitHistoryRef {
                        kind: *kind,
                        label: (*label).into(),
                    })
                    .collect(),
            }
        };
        let merged = matches!(stage, "checks" | "ready" | "merged");
        let mut commits = Vec::new();
        if merged {
            commits.push(commit(
                "7e0c3b",
                &["d29f50", "5be27d"],
                "Merge main into feat/session-refresh",
                "You",
                0,
                &[(GitHistoryRefKind::Branch, "feat/session-refresh")],
            ));
        }
        commits.extend([
            commit(
                "d29f50",
                &["c70a92"],
                "Add session refresh tests",
                "You",
                1,
                if merged {
                    &[]
                } else {
                    &[(GitHistoryRefKind::Branch, "feat/session-refresh")]
                },
            ),
            commit(
                "5be27d",
                &["4d1e07"],
                "Read sessions with the current key",
                "Lena Park",
                6,
                &[(GitHistoryRefKind::Remote, "main")],
            ),
            commit(
                "4d1e07",
                &["a3f9c1", "e83c51"],
                "Merge feat/checkout-analytics",
                "Sam Ortiz",
                12,
                &[],
            ),
            commit(
                "c70a92",
                &["1f8e3b"],
                "Refresh sessions near expiry",
                "You",
                25,
                &[],
            ),
            commit(
                "e83c51",
                &["62b0e4"],
                "Add analytics consent banner",
                "Sam Ortiz",
                32,
                &[(GitHistoryRefKind::Branch, "feat/checkout-analytics")],
            ),
            commit(
                "a3f9c1",
                &["9c2d4a"],
                "Rotate session keys",
                "Lena Park",
                18,
                &[],
            ),
            commit(
                "1f8e3b",
                &["9c2d4a"],
                "Read session expiry from the token",
                "You",
                40,
                &[],
            ),
            commit(
                "62b0e4",
                &["9c2d4a"],
                "Track checkout funnel events",
                "Sam Ortiz",
                120,
                &[],
            ),
            commit("9c2d4a", &[], "Bump next to 16.3", "Sam Ortiz", 180, &[]),
        ]);
        let mut head_shas: Vec<String> = ["d29f50", "c70a92", "1f8e3b"]
            .iter()
            .map(|s| sha(s))
            .collect();
        if merged {
            head_shas.insert(0, sha("7e0c3b"));
        }
        let conflicts = vec![
            MergeConflict {
                path: "src/middleware.ts".into(),
                base_author: Some("Lena Park".into()),
                hunks: vec![ConflictHunk {
                    line: 18,
                    head: "if (nearExpiry(session)) await refresh(session)".into(),
                    base: "const session = await readSession(req, keys)".into(),
                }],
                note: None,
            },
            MergeConflict {
                path: "src/auth.ts".into(),
                base_author: Some("Lena Park".into()),
                hunks: vec![
                    ConflictHunk {
                        line: 42,
                        head: "return refreshIfNear(token, 5 * MINUTE)".into(),
                        base: "return verify(token, keys.current)".into(),
                    },
                    ConflictHunk {
                        line: 57,
                        head: "const ttl = session.expiresAt - Date.now()".into(),
                        base: "const ttl = keys.current.ttl".into(),
                    },
                ],
                note: None,
            },
        ];
        let graph = BranchGraph {
            base_ref: "origin/main".into(),
            head_branch: "feat/session-refresh".into(),
            head_sha: Some(commits[0].sha.clone()),
            fork_sha: Some(sha("9c2d4a")),
            ahead: head_shas.len(),
            behind: if merged { 0 } else { 4 },
            commits,
            head_shas,
            self_email: Some("you@example.test".into()),
            uncommitted: if stage == "creating" { 3 } else { 0 },
            merge: Some(if stage == "conflicts" {
                MergePreview {
                    clean: false,
                    conflicts,
                    more_files: 0,
                }
            } else {
                MergePreview {
                    clean: true,
                    ..Default::default()
                }
            }),
        };
        let check = |name: &str, conclusion: &str, status: &str| ChangeRequestCheck {
            name: name.into(),
            conclusion: conclusion.into(),
            status: status.into(),
            ..Default::default()
        };
        let detail = (stage != "creating").then(|| zuse_protocol::ChangeRequestDetail {
            title: "Refresh sessions in the auth middleware".into(),
            url: url.into(),
            number: 418,
            base_ref_name: "main".into(),
            head_ref_name: "feat/session-refresh".into(),
            state: if stage == "merged" { "MERGED" } else { "OPEN" }.into(),
            mergeable: if stage == "conflicts" {
                "CONFLICTING"
            } else {
                "MERGEABLE"
            }
            .into(),
            additions: 482,
            deletions: 37,
            status_check_rollup: if stage == "failed" {
                vec![
                    check("Tests", "FAILURE", "COMPLETED"),
                    check("Lint", "SUCCESS", "COMPLETED"),
                    check("Types", "SUCCESS", "COMPLETED"),
                    check("Preview", "SUCCESS", "COMPLETED"),
                ]
            } else if stage == "checks" {
                vec![
                    check("Tests", "", "IN_PROGRESS"),
                    check("Lint", "SUCCESS", "COMPLETED"),
                    check("Types", "SUCCESS", "COMPLETED"),
                    check("Preview", "", "QUEUED"),
                ]
            } else {
                ["Tests", "Lint", "Types", "Preview"]
                    .iter()
                    .map(|name| check(name, "SUCCESS", "COMPLETED"))
                    .collect()
            },
            ..Default::default()
        });
        let key = if stage == "creating" { "" } else { url };
        cx.default_global::<crate::pull_request_card::PullRequestCardFixtures>()
            .0
            .insert(key.into(), (detail, graph));

        let millis = now.timestamp_millis();
        let tool = |id: &str, command: &str, output: Option<&str>| {
            let mut part = serde_json::json!({
                "kind": "tool", "id": id,
                "call": {"kind": "exec", "command": command},
                "resolved": output.is_some(),
            });
            if let Some(output) = output {
                part["output"] = output.into();
            }
            part
        };
        let creating = stage == "creating";
        let reply = match stage {
            "conflicts" => {
                "PR #418 is open, but `main` moved on while we worked: Lena's session key rotation changed the same lines in `auth.ts` and `middleware.ts`. I can merge `main` in and resolve them."
            }
            "creating" => "",
            _ => {
                "PR #418 is open against `main`. I merged `main` in and resolved the conflicts with Lena's key rotation; the checks are below."
            }
        };
        let mut parts = vec![
            tool(
                "c1",
                "git add -A && git commit -m 'Add session refresh tests'",
                Some("[feat/session-refresh d29f50] Add session refresh tests"),
            ),
            tool(
                "c2",
                "git push -u origin feat/session-refresh",
                Some(
                    "branch 'feat/session-refresh' set up to track 'origin/feat/session-refresh'.",
                ),
            ),
            tool(
                "c3",
                "gh pr create --fill --base main",
                (!creating).then_some(url),
            ),
        ];
        if !reply.is_empty() {
            parts.push(serde_json::json!({"kind": "text", "id": "t1", "text": reply}));
        }
        let entries = vec![
            serde_json::from_value::<zuse_doc::SessionMessageEntry>(serde_json::json!({
                "id": if flow { "u-flow".to_owned() } else { format!("u-{stage}") }, "role": "user", "createdAt": millis - 60_000, "deviceId": "local",
                "status": "complete",
                "parts": [{"kind": "text", "id": "t", "text": "Create PR"}],
            }))
            .unwrap(),
            serde_json::from_value::<zuse_doc::SessionMessageEntry>(serde_json::json!({
                "id": if flow { "a-flow".to_owned() } else { format!("a-{stage}") }, "role": "assistant", "createdAt": millis - 55_000, "deviceId": "local",
                "status": if creating { "streaming" } else { "complete" },
                "durationMs": 5_000, "parts": parts,
            }))
            .unwrap(),
        ];
        self.fixture_seed_pull_request(cx);
        self.state.update(cx, |state, cx| {
            state.apply_transcript(entries);
            cx.notify();
        });
        self.transcript.update(cx, |transcript, cx| {
            transcript.fixture_reload_pull_request_cards(cx)
        });
    }

    /// A Git chat with changes and no pull request: the composer offers
    /// Create PR.
    fn fixture_seed_create_pull_request(&mut self, cx: &mut Context<Self>) {
        self.fixture_seed_transcript(false, cx);
        self.state.update(cx, |state, cx| {
            let selected = state.selected_chat.clone();
            if let Some(chat) = state
                .chats
                .iter_mut()
                .find(|chat| Some(&chat.id) == selected.as_ref())
            {
                chat.source_context = Some(zuse_protocol::ConversationSourceContext {
                    checkout_id: "fixture-checkout".into(),
                    repo_root: "/tmp/fieldnotes".into(),
                    cwd: "/tmp/fieldnotes".into(),
                    branch: "feat/session-refresh".into(),
                    head_sha: None,
                    observed_at: Utc::now(),
                });
            }
            cx.notify();
        });
    }

    fn fixture_seed_link_transcript(&mut self, cx: &mut Context<Self>) {
        let now = Utc::now().timestamp_millis();
        let entry = |id: &str, role: &str, text: &str, ago_ms: i64| {
            serde_json::from_value::<zuse_doc::SessionMessageEntry>(serde_json::json!({
                "id": id, "role": role, "createdAt": now - ago_ms,
                "parts": [{"kind":"text","id":"t","text":text}],
                "deviceId": "local", "status": "complete", "durationMs": 12_000,
            }))
            .unwrap()
        };
        let entries = vec![
            entry(
                "u1",
                "user",
                "Can you review https://github.com/opcodehq/zuse/pull/482 and compare it with https://linear.app/zuse/issue/ZUS-118/login-redirect and https://example.com/docs/auth?",
                300_000,
            ),
            entry(
                "a1",
                "assistant",
                "I read https://github.com/opcodehq/zuse/pull/482. It keeps the token in the redirect, which the [OAuth guide](https://www.oauth.com/oauth2-servers/redirect-uris/) warns against. The design is in https://www.figma.com/file/Ab12/Login-Flow and the GPUI docs are at https://docs.rs/gpui.",
                240_000,
            ),
        ];
        self.state.update(cx, |state, cx| {
            state.apply_transcript(entries);
            cx.notify();
        });
    }

    fn fixture_seed_transcript(&mut self, follow_up: bool, cx: &mut Context<Self>) {
        let now = Utc::now().timestamp_millis();
        let tool = |id: &str, call: serde_json::Value, extra: serde_json::Value| {
            let mut part = serde_json::json!({"kind":"tool","id":id,"call":call,"resolved":true});
            part.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            part
        };
        let entry = |id: &str, role: &str, parts: Vec<serde_json::Value>, ago_ms: i64| {
            serde_json::from_value::<zuse_doc::SessionMessageEntry>(serde_json::json!({
                "id": id, "role": role, "parts": parts, "createdAt": now - ago_ms,
                "deviceId": "local", "status": "complete", "durationMs": 42_000,
            }))
            .unwrap()
        };
        let none = serde_json::json!({});
        let entries = vec![
            entry(
                "u1",
                "user",
                vec![
                    serde_json::json!({"kind":"text","id":"t","text":"Give me an overview of this repo and fix the login redirect."}),
                ],
                600_000,
            ),
            entry(
                "a1",
                "assistant",
                vec![
                    serde_json::json!({"kind":"reasoning","id":"r1","text":"Let me look at the layout of the workspace first. Then the auth crate."}),
                    tool(
                        "x1",
                        serde_json::json!({"kind":"exec","command":"ls && cat package.json | head -60 && ls apps packages"}),
                        none.clone(),
                    ),
                    tool(
                        "x2",
                        serde_json::json!({"kind":"readFile","path":"crates/auth/src/redirect.rs"}),
                        none.clone(),
                    ),
                    tool(
                        "x3",
                        serde_json::json!({"kind":"search","pattern":"fn redirect_target","path":"crates"}),
                        none.clone(),
                    ),
                    serde_json::json!({"kind":"reasoning","id":"r2","text":"The redirect keeps the original query string. It should drop the token."}),
                    tool(
                        "x4",
                        serde_json::json!({"kind":"editFile","path":"crates/auth/src/redirect.rs"}),
                        serde_json::json!({"diffStats":[{"path":"crates/auth/src/redirect.rs","additions":12,"deletions":3}]}),
                    ),
                    tool(
                        "x5",
                        serde_json::json!({"kind":"exec","command":"cargo test -p auth redirect"}),
                        serde_json::json!({"isError":true,"output":"1 failed"}),
                    ),
                    tool(
                        "x6",
                        serde_json::json!({"kind":"webSearch","query":"oauth redirect uri token leak"}),
                        none.clone(),
                    ),
                    tool(
                        "x7",
                        serde_json::json!({"kind":"mcp","server":"github","tool":"list_pull_requests"}),
                        none.clone(),
                    ),
                    serde_json::json!({"kind":"text","id":"t1","text":"Here's the overview. The redirect now drops the token before returning to the app."}),
                ],
                540_000,
            ),
            entry(
                "a2",
                "assistant",
                vec![
                    serde_json::json!({"kind":"reasoning","id":"r3","text":"Session expiry and cookie flags are independent. Two parallel audits will be faster than reading both paths myself."}),
                    tool(
                        "s1",
                        serde_json::json!({"kind":"unknown","name":"Agent: audit session expiry"}),
                        serde_json::json!({"subagentRef":"chat--sub--s1","subagentStatus":"running","subagentTail":"Reading crates/auth/src/session.rs"}),
                    ),
                    tool(
                        "s2",
                        serde_json::json!({"kind":"unknown","name":"Agent: check cookie flags"}),
                        serde_json::json!({"subagentRef":"chat--sub--s2","subagentStatus":"done"}),
                    ),
                    serde_json::json!({"kind":"text","id":"t2","text":"Two audits are under way."}),
                ],
                240_000,
            ),
        ];
        let mut entries = entries;
        if follow_up {
            entries.push(entry(
                "u2",
                "user",
                vec![
                    serde_json::json!({"kind":"text","id":"t","text":"Now add a regression test."}),
                ],
                60_000,
            ));
        }
        self.state.update(cx, |state, cx| {
            state.apply_transcript(entries);
            cx.notify();
        });
        self.transcript.update(cx, |transcript, cx| {
            transcript.fixture_hover_entry("a1", cx);
        });
    }
}

#[cfg(test)]
mod right_tab_mouse_regressions {
    use super::*;
    use gpui::{AppContext, TestAppContext, VisualTestContext};

    // Render the production strip and use its real Shell callbacks, without
    // starting an engine or rendering the rest of the desktop application.
    struct TabHost {
        shell: Entity<Shell>,
        _data_dir: tempfile::TempDir,
    }

    impl Render for TabHost {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.shell.update(cx, |shell, cx| {
                let tabs = shell.render_right_tab_strip(cx);
                shell.titlebar_drag_region(
                    "right-tab-test-titlebar",
                    div().w(px(400.)).h(px(40.)).child(tabs),
                    cx,
                )
            })
        }
    }

    fn setup(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let (host, cx) = cx.add_window_view(|_, cx| {
            let shell = cx.new(|cx| {
                let state = cx.new(|_| AppState::new());
                let mut shell = Shell::new(
                    state,
                    EngineBootConfig {
                        target: None,
                        data_dir: dir.path().into(),
                        ipc_port: 0,
                        edge_url: "http://127.0.0.1:1".into(),
                        edge_token: None,
                        org_id: None,
                        workos_client_id: None,
                        default_harness: zuse_protocol::HarnessId::Mock,
                    },
                    cx,
                );
                shell.active_chat = "parent".into();
                for id in ["first", "second"] {
                    shell.add_subagent_surface("parent".into(), id.into(), id.into(), false, cx);
                }
                shell
            });
            TabHost {
                shell,
                _data_dir: dir,
            }
        });
        let shell = host.read_with(cx, |host, _| host.shell.clone());
        cx.update(|window, cx| window.draw(cx).clear());
        (shell, cx)
    }

    #[gpui::test]
    fn update_banner_fires_during_continuous_state_changes(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        shell.update(cx, |shell, cx| {
            shell.settings.notifications_enabled = false;
            shell.state.update(cx, |state, _| {
                state.harness_updates = vec![
                    serde_json::from_value(serde_json::json!({
                        "harness": "codex", "phase": "available", "latestVersion": "2.0.0",
                        "policy": "notify", "source": "unknown", "canApply": true
                    }))
                    .unwrap(),
                    serde_json::from_value(serde_json::json!({
                        "harness": "hermes", "phase": "available",
                        "policy": "notify", "source": "unknown", "canApply": true
                    }))
                    .unwrap(),
                ];
            });
        });
        for _ in 0..12 {
            shell.update(cx, |shell, cx| {
                shell.on_state_changed(&shell.state.clone(), cx)
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
        }
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.harness_update_seen.len(), 2);
            assert!(shell.harness_update_banner_task.is_none());
        });
        // Rechecks and repeated versionless availability remain deduplicated.
        for phase in [
            zuse_protocol::HarnessUpdatePhase::Checking,
            zuse_protocol::HarnessUpdatePhase::Available,
        ] {
            shell.update(cx, |shell, cx| {
                shell
                    .state
                    .update(cx, |state, _| state.harness_updates[1].phase = phase);
                shell.on_state_changed(&shell.state.clone(), cx);
                assert!(shell.harness_update_banner_task.is_none());
            });
        }
        // A confirmed current state ends the availability episode, permitting
        // another commit-only update to notify even if the version is unchanged.
        shell.update(cx, |shell, cx| {
            shell.state.update(cx, |state, _| {
                state.harness_updates[1].phase = zuse_protocol::HarnessUpdatePhase::Current
            });
            shell.on_state_changed(&shell.state.clone(), cx);
            assert_eq!(shell.harness_update_seen.len(), 1);
            shell.state.update(cx, |state, _| {
                state.harness_updates[1].phase = zuse_protocol::HarnessUpdatePhase::Available
            });
            shell.on_state_changed(&shell.state.clone(), cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.harness_update_seen.len(), 2)
        });
    }

    #[gpui::test]
    fn subagent_close_press_does_not_start_parent_drag(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        let start = cx.debug_bounds("right-surface-close-0").unwrap().center();
        let end = start + gpui::point(px(6.), px(0.));
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.update(|_, cx| assert!(!cx.has_active_drag(), "close press started a tab drag"));
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.subagent_tabs.contains_key(&1));
            assert!(shell.subagent_tabs.contains_key(&2));
            assert_eq!(shell.resolved_right_active(cx), RightSurface::Subagent(2));
        });
    }

    #[gpui::test]
    fn right_tab_close_sits_at_the_trailing_edge(cx: &mut TestAppContext) {
        let (_shell, cx) = setup(cx);
        let tab = cx.debug_bounds("right-surface-tab-0").unwrap();
        let close = cx.debug_bounds("right-surface-close-0").unwrap();
        assert!(
            close.left() > tab.center().x,
            "close is not after the title"
        );
        // The chip's `px-1.5` trailing padding.
        assert_eq!(close.right(), tab.right() - px(6.));
    }

    #[gpui::test]
    fn tab_strip_scrolls_over_chips_inside_titlebar(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        shell.update(cx, |shell, cx| {
            for id in ["third", "fourth", "fifth", "sixth"] {
                shell.add_subagent_surface("parent".into(), id.into(), id.into(), false, cx);
            }
        });
        cx.update(|window, cx| window.draw(cx).clear());
        let start = cx.debug_bounds("right-surface-tab-0").unwrap().center();
        cx.update(|window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position: start,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(-100.), px(0.))),
                    modifiers: gpui::Modifiers::default(),
                    touch_phase: gpui::TouchPhase::Moved,
                }),
                cx,
            );
        });
        shell.read_with(cx, |shell, _| {
            assert!(
                shell.right_tab_scroll.offset().x < px(0.),
                "tab strip did not scroll"
            );
        });
    }

    #[gpui::test]
    fn subagent_tab_body_still_selects_drags_and_middle_closes(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        let start = cx.debug_bounds("right-surface-tab-0").unwrap().center();
        cx.simulate_click(start, gpui::Modifiers::default());
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.resolved_right_active(cx), RightSurface::Subagent(1));
        });
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(
            start + gpui::point(px(8.), px(0.)),
            Some(MouseButton::Left),
            gpui::Modifiers::default(),
        );
        cx.update(|_, cx| assert!(cx.has_active_drag(), "tab drag did not start"));
        cx.simulate_mouse_up(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Middle, gpui::Modifiers::default());
        cx.simulate_mouse_up(start, MouseButton::Middle, gpui::Modifiers::default());
        shell.read_with(cx, |shell, _| {
            assert!(!shell.subagent_tabs.contains_key(&1));
            assert!(shell.subagent_tabs.contains_key(&2));
        });
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    fn subagent_tab_click_jitter_selects_without_starting_a_drag(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        let start = cx.debug_bounds("right-surface-tab-0").unwrap().center();
        // Sub-threshold pointer jitter (the Windows drag rectangle is 4px,
        // matching the system SM_CXDRAG default): a jittery click must stay
        // a click — no drag ghost, tab still activates on release.
        let end = start + gpui::point(px(3.), px(0.));

        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.update(|_, cx| {
            assert!(
                !cx.has_active_drag(),
                "ordinary Windows click jitter started a tab drag"
            )
        });
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());

        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.resolved_right_active(cx), RightSurface::Subagent(1));
        });
    }

    /// The user-visible contract: dragging a surface tab onto another slot
    /// reorders the strip. On Windows the chip's `on_drag` used to be gated
    /// off entirely (`click_activation_drag_enabled`), so the drag could
    /// never start; on every platform the drop could not land on a chip
    /// (BlockMouse carve-out). This test is the regression lock for both.
    #[gpui::test]
    fn surface_tab_drag_reorders_the_strip(cx: &mut TestAppContext) {
        let (shell, cx) = setup(cx);
        let from = cx.debug_bounds("right-surface-tab-0").unwrap().center();
        let to = cx.debug_bounds("right-surface-tab-1").unwrap().center();

        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        // First move crosses the threshold and promotes the press into a
        // drag (bubble phase). The DragMoveEvent dispatch that computes the
        // drop slot only fires on the NEXT move (capture phase, after the
        // drag is already active) — a real pointer always produces both.
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.update(|_, cx| {
            assert!(cx.has_active_drag(), "surface tab drag never started");
        });
        cx.simulate_mouse_move(to, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());

        shell.read_with(cx, |shell, cx| {
            let key = shell.panel_key(cx);
            let tabs = shell.right_tabs.get(&key).expect("panel has surface tabs");
            assert_eq!(
                tabs,
                &vec![RightSurface::Subagent(2), RightSurface::Subagent(1)],
                "tab drag did not reorder the strip"
            );
        });
    }
}

#[cfg(test)]
mod shortcut_focus_regressions {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    struct ShortcutHost {
        root: FocusHandle,
        unfocused: FocusHandle,
        editor: FocusHandle,
        show_editor: bool,
        jumps: usize,
    }

    impl Render for ShortcutHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let root = self.root.clone();
            let unfocused = self.unfocused.clone();
            let preferred = self.editor.clone();
            window.defer(cx, move |window, cx| {
                restore_mounted_focus(&root, &preferred, &unfocused, window, cx);
            });
            div()
                .size_full()
                .track_focus(&self.root)
                .child(div().track_focus(&self.unfocused))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        // Exercise mouse focus handoffs, hiding a focused pane,
                        // and clicking a control that explicitly clears focus.
                        this.show_editor = event.position.x < px(100.0);
                        if event.position.x < px(200.0) {
                            window.focus(&this.editor, cx);
                        } else {
                            window.blur();
                        }
                        cx.notify();
                    }),
                )
                .on_action(cx.listener(|this, _: &JumpSession, _, _| this.jumps += 1))
                .when(self.show_editor, |el| {
                    el.child(div().track_focus(&self.editor))
                })
        }
    }

    #[gpui::test]
    fn explicit_blur_does_not_refocus_a_mounted_input(cx: &mut TestAppContext) {
        let host = cx.add_window(|_, cx| ShortcutHost {
            root: cx.focus_handle(),
            unfocused: cx.focus_handle(),
            editor: cx.focus_handle(),
            show_editor: true,
            jumps: 0,
        });
        cx.run_until_parked();
        cx.update_window(host.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        host.update(cx, |host, window, cx| {
            window.focus(&host.editor, cx);
            window.blur();
            restore_mounted_focus(&host.root, &host.editor, &host.unfocused, window, cx);
            assert!(host.unfocused.is_focused(window));
            // Subsequent renders must keep the neutral shortcut focus too.
            restore_mounted_focus(&host.root, &host.editor, &host.unfocused, window, cx);
            assert!(host.unfocused.is_focused(window));
        })
        .unwrap();
    }

    #[gpui::test]
    fn shortcuts_recover_from_retained_editor_focus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                &platform_combo("mod-2"),
                JumpSession(1),
                None,
            )]);
        });
        let host = cx.add_window(|_, cx| ShortcutHost {
            root: cx.focus_handle(),
            unfocused: cx.focus_handle(),
            editor: cx.focus_handle(),
            show_editor: true,
            jumps: 0,
        });
        for show_editor in [true, false, true, false] {
            host.update(cx, |host, window, cx| {
                host.show_editor = show_editor;
                // Keep the editor handle alive and focused even when hidden.
                window.focus(&host.editor, cx);
                cx.notify();
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(host.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            host.update(cx, |host, window, cx| {
                restore_mounted_focus(&host.root, &host.editor, &host.unfocused, window, cx);
                assert!(host.root.contains_focused(window, cx));
                assert_eq!(host.editor.is_focused(window), show_editor);
            })
            .unwrap();
            cx.simulate_keystrokes(host.into(), &platform_combo("mod-2"));
        }
        host.update(cx, |host, window, cx| {
            assert_eq!(host.jumps, 4);
            window.blur();
            restore_mounted_focus(&host.root, &host.editor, &host.unfocused, window, cx);
            assert!(host.unfocused.is_focused(window));
        })
        .unwrap();
    }

    #[gpui::test]
    fn shortcuts_work_after_mouse_focus_changes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                &platform_combo("mod-2"),
                JumpSession(1),
                None,
            )]);
        });
        let host = cx.add_window(|_, cx| ShortcutHost {
            root: cx.focus_handle(),
            unfocused: cx.focus_handle(),
            editor: cx.focus_handle(),
            show_editor: true,
            jumps: 0,
        });
        for (index, x) in [50.0, 150.0, 250.0, 50.0, 150.0, 250.0]
            .into_iter()
            .enumerate()
        {
            cx.update_window(host.into(), |_, window, cx| {
                window.draw(cx).clear();
                window.dispatch_event(
                    gpui::PlatformInput::MouseDown(MouseDownEvent {
                        position: gpui::point(px(x), px(20.0)),
                        button: MouseButton::Left,
                        modifiers: gpui::Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
            })
            .unwrap();
            // Dispatch immediately after the mouse event; no manual recovery.
            cx.simulate_keystrokes(host.into(), &platform_combo("mod-2"));
            host.update(cx, |host, window, cx| {
                assert_eq!(
                    host.jumps,
                    index + 1,
                    "shortcut failed after mouse click at {x}"
                );
                assert!(host.root.contains_focused(window, cx));
                assert_eq!(host.editor.is_focused(window), x < 100.0);
            })
            .unwrap();
        }
    }
}

/// Native visual QA uses the production shell with isolated fixture data.
#[cfg(feature = "appshots-fixture")]
impl Shell {
    pub fn fixture_appshots_settings(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.open_settings(SettingsSection::Appshots, cx);
        } else {
            self.close_settings(cx);
        }
    }
    pub fn fixture_appshots_composer(&self) -> Entity<Composer> {
        self.composer.clone()
    }
    pub fn fixture_appshots_sidebar(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.settings.sidebar_collapsed = collapsed;
        cx.notify();
    }
    pub fn fixture_appshots_transcript_start(&self, cx: &mut Context<Self>) {
        self.transcript
            .update(cx, |t, cx| t.fixture_appshots_start(cx));
    }
    pub fn fixture_transcript(&self) -> Entity<crate::transcript::Transcript> {
        self.transcript.clone()
    }
}

#[cfg(test)]
mod settings_modal_regressions {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    struct FocusHost {
        background: FocusHandle,
        start: FocusHandle,
        first: FocusHandle,
        last: FocusHandle,
        end: FocusHandle,
    }

    impl Render for FocusHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .child(
                    div()
                        .id("background")
                        .track_focus(&self.background)
                        .tab_index(0),
                )
                .child(
                    div()
                        .id("modal")
                        .track_focus(&self.start)
                        .tab_group()
                        .tab_stop(false)
                        .child(div().id("first").track_focus(&self.first).tab_index(0))
                        .child(div().id("last").track_focus(&self.last).tab_index(0))
                        .child(
                            div()
                                .id("end")
                                .track_focus(&self.end)
                                .tab_index(0)
                                .tab_stop(false),
                        ),
                )
        }
    }

    #[gpui::test]
    fn settings_tab_wraps_in_both_directions_without_entering_workspace(cx: &mut TestAppContext) {
        let host = cx.add_window(|_, cx| FocusHost {
            background: cx.focus_handle().tab_stop(true),
            start: cx.focus_handle(),
            first: cx.focus_handle().tab_stop(true),
            last: cx.focus_handle().tab_stop(true),
            end: cx.focus_handle(),
        });
        cx.update_window(host.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        host.update(cx, |host, window, cx| {
            window.focus(&host.start, cx);
            assert!(!host.first.is_focused(window));
            assert!(!host.last.is_focused(window));
            move_settings_focus(&host.start, &host.end, false, window, cx);
            assert!(host.first.is_focused(window));
            for reverse in [false, false, true, true, true, false] {
                let was_first = host.first.is_focused(window);
                move_settings_focus(&host.start, &host.end, reverse, window, cx);
                assert!(host.start.contains_focused(window, cx));
                assert_eq!(host.last.is_focused(window), was_first);
                assert!(!host.background.is_focused(window));
            }
        })
        .unwrap();
    }

    #[gpui::test]
    fn settings_visits_preserve_conversation_and_navigation_history(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            settings::init(settings::UiSettings::default(), dir.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            )
        });
        window
            .update(cx, |shell, _, cx| {
                let history = shell.nav.current().clone();
                let selected = shell.state.read(cx).selected_chat.clone();
                for section in SettingsSection::ALL {
                    shell.open_settings(section, cx);
                    assert_eq!(shell.route, Route::Settings(section.canonical()));
                    assert_eq!(shell.nav.current().clone(), history);
                    assert_eq!(shell.state.read(cx).selected_chat, selected);
                    shell.toggle_settings(cx);
                    assert_eq!(shell.route, Route::Chat);
                    assert!(!shell.settings_focus_pending);
                    shell.toggle_settings(cx);
                    assert_eq!(shell.route, Route::Settings(section.reopenable()));
                }
                shell.open_settings(SettingsSection::Agents, cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Harnesses));
                shell.close_settings(cx);
                assert_eq!(shell.route, Route::Chat);
                assert_eq!(shell.nav.current().clone(), history);
                assert!(shell.settings_restore_pending);
                shell.open_pull_requests(cx);
                shell.open_settings(SettingsSection::General, cx);
                shell.close_settings(cx);
                assert_eq!(shell.route, Route::PullRequests);
                assert_eq!(shell.nav.current(), &NavEntry::PullRequests);
            })
            .unwrap();
    }

    fn init_settings_test(
        saved: settings::UiSettings,
        dir: &std::path::Path,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            settings::init(saved, dir, cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
        });
    }

    fn test_shell(dir: &std::path::Path, cx: &mut Context<Shell>) -> Shell {
        let state = cx.new(|_| AppState::new());
        Shell::new(
            state,
            EngineBootConfig {
                target: None,
                data_dir: dir.into(),
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zuse_protocol::HarnessId::Mock,
            },
            cx,
        )
    }

    #[test]
    fn remembered_sections_reopen_only_where_the_nav_can_show_them() {
        assert_eq!(
            SettingsSection::Shortcuts.reopenable(),
            SettingsSection::Shortcuts
        );
        // The legacy Accounts alias lands on the page that absorbed it.
        assert_eq!(
            SettingsSection::Agents.reopenable(),
            SettingsSection::Harnesses
        );
        // Appshots is hidden off-desktop, so it cannot be reopened there.
        assert_eq!(
            SettingsSection::Appshots.reopenable(),
            if crate::appshots::is_desktop() {
                SettingsSection::Appshots
            } else {
                SettingsSection::General
            }
        );
        for section in SettingsSection::ALL {
            assert!(section.reopenable().visible_in_nav(), "{section:?}");
            assert_eq!(SettingsSection::from_slug(section.slug()), Some(section));
        }
    }

    #[test]
    fn open_route_names_a_section_or_reopens_the_remembered_one() {
        let remembered = SettingsSection::Notifications;
        assert_eq!(
            settings_open_route("settings", remembered),
            Some(SettingsSection::Notifications)
        );
        assert_eq!(
            settings_open_route("settings", SettingsSection::Agents),
            Some(SettingsSection::Harnesses)
        );
        for (route, section) in [
            ("settings/devices", SettingsSection::Devices),
            ("settings/providers", SettingsSection::Harnesses),
            ("settings/agents", SettingsSection::Harnesses),
            ("settings/harnesses", SettingsSection::Harnesses),
            ("settings/general", SettingsSection::General),
            ("settings/voice", SettingsSection::Voice),
            ("settings/conversations", SettingsSection::General),
            ("settings/files", SettingsSection::Files),
            ("settings/appshots", SettingsSection::Appshots),
            ("settings/archived", SettingsSection::Archived),
            ("settings/cloud", SettingsSection::Cloud),
            (
                "settings/cloud-repositories",
                SettingsSection::CloudRepositories,
            ),
            ("settings/cloud-machine", SettingsSection::CloudMachine),
            ("settings/usage", SettingsSection::Usage),
            ("settings/billing", SettingsSection::Billing),
        ] {
            assert_eq!(
                settings_open_route(route, remembered),
                Some(section),
                "{route}"
            );
        }
        assert_eq!(settings_open_route("settings/invoices", remembered), None);
        assert_eq!(settings_open_route("new", remembered), None);
    }

    /// Session navigation shortcuts are not swallowed by Settings: each one
    /// leaves the page and lands on its target, as it would from chat.
    #[gpui::test]
    fn navigation_shortcuts_work_from_settings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = settings::UiSettings::default();
        saved
            .keymap
            .set(ShortcutId::JumpSession(0), "mod-alt-1".into());
        let keymap = saved.keymap.clone();
        init_settings_test(saved, dir.path(), cx);
        cx.update(|cx| apply_keymap(cx, &keymap, ComposerSendBehavior::default()));
        let window = cx.add_window(|_, cx| {
            let mut shell = test_shell(dir.path(), cx);
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                state.chats = ["older", "newer"]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, id)| {
                        serde_json::from_value(serde_json::json!({
                            "id": id, "title": id, "deviceId": "local", "archived": false,
                            "createdAt": Utc::now() - chrono::Duration::minutes(10 - ix as i64),
                        }))
                        .unwrap()
                    })
                    .collect();
                state.selected_chat = Some("older".into());
            });
            shell
        });
        let mut press = |combo: &str| {
            window
                .update(cx, |shell, _, cx| {
                    shell.open_settings(SettingsSection::Shortcuts, cx)
                })
                .unwrap();
            cx.run_until_parked();
            cx.simulate_keystrokes(window.into(), &platform_combo(combo));
            window
                .read_with(cx, |shell, cx| {
                    (shell.route, shell.state.read(cx).selected_chat.clone())
                })
                .unwrap()
        };
        assert_eq!(
            press(keymap.get(ShortcutId::JumpSession(0))),
            (Route::Chat, Some("newer".into()))
        );
        assert_eq!(
            press(&keymap.next_session),
            (Route::Chat, Some("older".into()))
        );
        assert_eq!(
            press(&keymap.prev_session),
            (Route::Chat, Some("newer".into()))
        );
        assert_eq!(press(&keymap.new_session), (Route::Chat, None));
    }

    #[gpui::test]
    fn settings_reopen_where_they_were_left(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        init_settings_test(settings::UiSettings::default(), dir.path(), cx);
        let window = cx.add_window(|_, cx| test_shell(dir.path(), cx));
        window
            .update(cx, |shell, _, cx| {
                // Nothing remembered yet: General.
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::General));
                // Switching sections inside Settings is remembered.
                shell.open_settings(SettingsSection::Shortcuts, cx);
                assert_eq!(shell.settings.settings_section, SettingsSection::Shortcuts);
                assert_eq!(
                    settings::current(cx).settings_section,
                    SettingsSection::Shortcuts
                );
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Chat);
                // ⌘, / the footer gear reopen it…
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Shortcuts));
                shell.close_settings(cx);
                // …as do the palette and `/settings`.
                shell.open_last_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Shortcuts));
                shell.close_settings(cx);

                // A link naming a section wins, and becomes the one remembered.
                shell.open_settings(SettingsSection::Appearance, cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Appearance));
                shell.close_settings(cx);
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Appearance));
                shell.close_settings(cx);

                // Legacy aliases are remembered as the page they resolve to.
                shell.open_settings(SettingsSection::Agents, cx);
                assert_eq!(shell.settings.settings_section, SettingsSection::Harnesses);
                shell.close_settings(cx);
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::Harnesses));
                settings::flush(cx);
            })
            .unwrap();
        // It survives a restart.
        assert_eq!(
            settings::UiSettings::load(dir.path()).settings_section,
            SettingsSection::Harnesses
        );
    }

    #[gpui::test]
    fn unknown_or_hidden_remembered_sections_reopen_general(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings::UiSettings::path(dir.path()),
            r#"{"sidebarWidth": 300, "settingsSection": "invoices"}"#,
        )
        .unwrap();
        let saved = settings::UiSettings::load(dir.path());
        assert_eq!(saved.sidebar_width, 300.0);
        init_settings_test(saved, dir.path(), cx);
        let window = cx.add_window(|_, cx| test_shell(dir.path(), cx));
        window
            .update(cx, |shell, _, cx| {
                shell.toggle_settings(cx);
                assert_eq!(shell.route, Route::Settings(SettingsSection::General));
                shell.close_settings(cx);
                // A remembered section this build hides falls back as well.
                shell.settings.settings_section = SettingsSection::Appshots;
                shell.toggle_settings(cx);
                assert_eq!(
                    shell.route,
                    Route::Settings(SettingsSection::Appshots.reopenable())
                );
            })
            .unwrap();
    }

    /// The full shell draws its sidebar as a cached view after its own
    /// render, so the lifted row must be built and drawn within one frame.
    /// Carried out of the sidebar it follows the pointer, and dropped on a
    /// conversation it becomes a chat chip in that composer.
    #[gpui::test]
    fn a_dragged_session_leaves_the_sidebar_and_drops_as_a_chat_mention(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        init_settings_test(settings::UiSettings::default(), dir.path(), cx);
        let (shell, cx) = cx.add_window_view(|_, cx| test_shell(dir.path(), cx));
        cx.simulate_resize(gpui::size(px(1200.0), px(800.0)));
        shell.update(cx, |shell, cx| {
            shell.debug_gate = Some(GatePhase::Ready);
            shell.splash = SplashPhase::Gone;
            shell.reduced_motion = true;
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                state.chats = ["older", "newer"]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, id)| {
                        serde_json::from_value(serde_json::json!({
                            "id": id, "title": format!("Chat {id}"), "deviceId": "local",
                            "archived": false,
                            "createdAt": Utc::now() - chrono::Duration::minutes(10 - ix as i64),
                        }))
                        .unwrap()
                    })
                    .collect();
                state.selected_chat = Some("newer".into());
            });
            shell.active_chat = "newer".into();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        let row = cx.debug_bounds("chat-older").expect("sidebar row");
        let from = row.center();
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        // Several frames of dragging, inside the sidebar and then out of it.
        for step in 1..=6 {
            let x = from.x + px(4.0 + 120.0 * step as f32);
            cx.simulate_mouse_move(
                gpui::point(x, from.y + px(40.0)),
                Some(MouseButton::Left),
                gpui::Modifiers::default(),
            );
            cx.update(|window, cx| window.draw(cx).clear());
        }
        // The sidebar is a cached view, whose contents tests can't query by
        // selector; the lifted row's own window position is the check.
        let sidebar_width = shell.read_with(cx, |shell, _| shell.settings.sidebar_width);
        shell.read_with(cx, |shell, _| {
            let transfer = shell
                .sidebar_session_transfer
                .as_ref()
                .expect("still dragging");
            let left = transfer.slide_x.to;
            let expected = f32::from(transfer.pointer.x - transfer.cursor_offset.x);
            assert!(
                left > sidebar_width,
                "outside the sidebar the row follows the pointer"
            );
            assert!((left - expected).abs() < 0.5);
        });
        let drop = gpui::point(from.x + px(724.0), from.y + px(40.0));
        cx.simulate_mouse_up(drop, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        shell.read_with(cx, |shell, cx| {
            let text = shell.composer.read(cx).input.read(cx).text().to_owned();
            assert!(
                text.contains("[Chat older](zuse-chat:older)"),
                "the drop mentions the chat: {text:?}"
            );
            assert!(shell.sidebar_session_transfer.is_none());
        });
    }

    #[gpui::test]
    fn voice_stage_opens_over_settings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        init_settings_test(settings::UiSettings::default(), dir.path(), cx);
        let (shell, cx) = cx.add_window_view(|_, cx| test_shell(dir.path(), cx));
        shell.update(cx, |shell, cx| {
            shell.debug_gate = Some(GatePhase::Ready);
            shell.reduced_motion = true;
            shell.open_settings(SettingsSection::Voice, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("voice-stage").is_none());

        shell.update(cx, |shell, cx| {
            shell.voice.update(cx, |voice, _| {
                voice.phase = zuse_protocol::voice::VoicePhase::Active
            });
            shell.set_voice_stage_open(true, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear());
        let stage = cx
            .debug_bounds("voice-stage")
            .expect("stage renders in Settings");
        shell.read_with(cx, |shell, cx| {
            assert!(matches!(shell.route, Route::Settings(_)));
            assert!(shell.voice.read(cx).stage_open);
            // The settings sidebar stays usable beside the stage.
            assert_eq!(f32::from(stage.origin.x), shell.settings.sidebar_width);
        });
    }

    #[gpui::test]
    fn escape_closes_what_is_open_inside_settings_before_settings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        init_settings_test(settings::UiSettings::default(), dir.path(), cx);
        let (shell, cx) = cx.add_window_view(|_, cx| test_shell(dir.path(), cx));
        shell.update(cx, |shell, cx| {
            // No engine in tests: render the workspace, not the boot gate.
            shell.debug_gate = Some(GatePhase::Ready);
            shell.open_settings(SettingsSection::Devices, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());

        // A dialog that does not hold focus (the rename input was never
        // clicked) still closes before Settings does.
        let devices = shell.read_with(cx, |shell, _| shell.devices_page.clone().unwrap());
        devices.update(cx, |page, cx| {
            page.open_rename("device-1".into(), "Studio".into(), cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        cx.simulate_keystrokes("escape");
        assert!(!devices.update(cx, |page, cx| page.dismiss_on_escape(cx)));
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.route, Route::Settings(SettingsSection::Devices));
        });

        // So does the account menu opened from the settings footer.
        shell.update(cx, |shell, cx| {
            shell.user_menu.open(());
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        cx.simulate_keystrokes("escape");
        shell.read_with(cx, |shell, _| {
            assert!(!shell.user_menu.is_open());
            assert_eq!(shell.route, Route::Settings(SettingsSection::Devices));
        });
        // The exit animation's reap runs on wall-clock time; stand in for it.
        shell.update(cx, |shell, cx| {
            shell.user_menu = popover::Popup::default();
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear());

        // With nothing left open, Escape leaves Settings.
        cx.simulate_keystrokes("escape");
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.route, Route::Chat);
            assert_eq!(shell.settings.settings_section, SettingsSection::Devices);
        });
    }
}

/// Native visual QA of the pull request board and detail with isolated data.
#[cfg(feature = "pull-request-fixture")]
impl Shell {
    pub fn fixture_pull_request_detail(
        &self,
    ) -> Option<Entity<crate::pull_request_detail::PullRequestDetailPage>> {
        self.active_pull_request_page()
    }

    /// Size the pull request pane by hand, without the open animation.
    pub fn fixture_pull_request_pane_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.settings.pull_request_pane_width = width;
        self.pull_request_pane.tween = None;
        cx.notify();
    }

    pub fn fixture_toggle_pull_request_pane_expand(&mut self, cx: &mut Context<Self>) {
        self.toggle_pull_request_pane_expand(cx);
        self.pull_request_pane.tween = None;
    }

    pub fn fixture_pull_request_settings(&mut self, open: bool, cx: &mut Context<Self>) {
        if open {
            self.open_settings(SettingsSection::General, cx);
        } else {
            self.close_settings(cx);
        }
    }
}

#[cfg(feature = "browser-fixture")]
const FIXTURE_FILE_TEXT: &str = r#"use std::path::PathBuf;

/// Where the desktop app keeps its window state between launches.
fn state_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fieldnotes")
}

fn main() {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).expect("state directory");
    println!("Fieldnotes desktop starting in {}", dir.display());
    fieldnotes::app::run(dir);
}
"#;

#[cfg(feature = "browser-fixture")]
const FIXTURE_REVIEW_PATCH: &str = "diff --git a/src/cache.rs b/src/cache.rs
index 1111111..2222222 100644
--- a/src/cache.rs
+++ b/src/cache.rs
@@ -1,9 +1,14 @@
 use std::collections::HashMap;
+use std::time::{Duration, Instant};
 
 pub struct Cache {
-    entries: HashMap<String, String>,
+    entries: HashMap<String, (String, Instant)>,
+    ttl: Duration,
 }
 
 impl Cache {
-    pub fn get(&self, key: &str) -> Option<&String> {
-        self.entries.get(key)
+    pub fn get(&self, key: &str) -> Option<&str> {
+        let (value, stored) = self.entries.get(key)?;
+        (stored.elapsed() < self.ttl).then_some(value.as_str())
     }
+}
diff --git a/README.md b/README.md
index 3333333..4444444 100644
--- a/README.md
+++ b/README.md
@@ -3,4 +3,5 @@
 Fieldnotes keeps project notes beside the code.
 
-Run `cargo run` to start.
+Run `cargo run` to start. Cached pages expire after
+the configured time to live.
";

#[cfg(feature = "simulator-fixture")]
impl Shell {
    pub fn fixture_simulator(
        &mut self,
        state: zuse_protocol::simulators::State,
        frame: Option<zuse_protocol::simulators::Frame>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_surfaces_open(true, cx);
        self.add_simulator_surface(window, cx);
        if let RightSurface::Simulator(id) = self.resolved_right_active(cx) {
            self.simulators
                .get(&id)
                .unwrap()
                .update(cx, |pane, cx| pane.fixture_load(state, frame, cx));
        }
    }
}
