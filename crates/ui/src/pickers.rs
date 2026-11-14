//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle), HarnessModelPicker (harness rail + model list, harness
//! locked once the chat exists), TraitsPicker (reasoning ladder + advertised
//! model options; trigger shows the non-default summary "High · 1M · Fast").
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering, folder-browser navigation, traits summary) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use crate::localization::message as tr;
use zuse_i18n::messages as msg;
mod cloud;
pub(crate) use cloud::CloudCanvasChoices;
mod cloud_models;
mod compact;
mod creation_sources;
mod import_chat;
mod model_menu;
mod settings_models;
mod workspace;
pub(crate) use creation_sources::CreationIssueTitle;

use crate::roll_text::{roll_text, rolling};
use std::collections::{HashMap, HashSet};

thread_local! {
    /// Labels of every model a catalog has shown in this app, by id. A picker
    /// whose own catalog hasn't arrived yet (a cloud chat just opened) names
    /// its model as the catalog will, instead of showing the raw id first.
    /// Pickers live on the UI thread, so one table per thread is one per app
    /// (and keeps tests, which each run on their own thread, independent).
    static KNOWN_MODEL_LABELS: std::cell::RefCell<HashMap<String, String>> =
        Default::default();
}

pub(crate) fn remember_model_labels<'a>(models: impl IntoIterator<Item = &'a Model>) {
    KNOWN_MODEL_LABELS.with_borrow_mut(|known| {
        for model in models {
            if !model.label.trim().is_empty() {
                known.insert(model.id.clone(), model.label.clone());
            }
        }
    });
}

fn known_model_label(id: &str) -> Option<String> {
    KNOWN_MODEL_LABELS.with_borrow(|known| known.get(id).cloned())
}
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use zuse_engine::registry::{HarnessDescriptor, TitleSettings};
use zuse_protocol::{
    ChatConfig, FolderListing, HarnessId, Model, ReasoningLevel, RepoRef, SandboxLevel, Space,
};
use zuse_rpc::methods;

/// Display cap for the ref list (paging 100 at a time with a status
/// footer is the alternative; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// Dev/testing knob: `ZUSE_SLOW_CATALOG_MS=<ms>` delays every harness and
/// model catalog result app-side — the chip/tab/list loading states are
/// sub-second against a warm local daemon and unstageable otherwise
/// (headless-rig captures; same family as `ZUSE_OPEN_PICKER`).
fn slow_catalog_delay() -> Option<std::time::Duration> {
    std::env::var("ZUSE_SLOW_CATALOG_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
}

// ---------------------------------------------------------------------------
// Catalog invalidation (Settings → Providers toggles)
// ---------------------------------------------------------------------------

/// Marker global: [`bump_harness_catalog`] pokes it whenever a Settings →
/// Agents toggle changes some device's enabled set, and every [`Pickers`]
/// observes it to force-refresh its cached harness catalog — without this the
/// composer served the boot-time list until restart (user report).
#[derive(Default)]
pub struct HarnessCatalogChanged;

impl gpui::Global for HarnessCatalogChanged {}

/// Notify all composers that some device's harness catalog changed. The
/// global carries no data — `default_global` pushes the observer effect, and
/// the observers re-fetch from the engine (the source of truth).
pub fn bump_harness_catalog(cx: &mut App) {
    cx.default_global::<HarnessCatalogChanged>();
}

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the git
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub sandbox: Option<SandboxLevel>,
    /// Plan mode for the new chat, separate from its access level.
    pub plan: bool,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs.
    pub checkout: CheckoutKind,
}

/// Where a new session runs (`local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (zuse mints a `zuse/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete harness,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub sandbox: SandboxLevel,
    pub plan: bool,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: self.sandbox,
            plan: self.plan,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The harness's default model: the first catalog row (both curated catalogs
/// lead with the flagship — zuse's `pickDefaultModel` Opus preference maps to
/// the same row here).
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// An explicit selection never silently becomes a different model after refresh.
fn selected_catalog_model<'a>(models: &'a [Model], selected: Option<&str>) -> Option<&'a Model> {
    match selected {
        Some(id) => models.iter().find(|model| model.id == id),
        None => default_model(models),
    }
}

/// A model's default reasoning: X-High when the ladder offers it (zuse
/// `DEFAULT_REASONING = "xhigh"`), else High, else the ladder's first entry.
/// `None` only for ladder-less models (e.g. Haiku's thinking toggle instead).
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it, else fall to the model's default (never a stale
/// or foreign level — zuse use-run-config.ts's derived-model discipline).
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// Keep only the picks `model` still offers. Remembered picks outlive the
/// model they were made on, and harnesses apply some options blindly (Claude
/// appends `[1m]` to any model id when `contextWindow` is "1m").
pub fn offered_options(
    model: &Model,
    mut selections: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    selections.retain(|id, choice| {
        model.options.iter().any(|option| {
            option.id == *id
                && choice
                    .as_str()
                    .is_some_and(|choice| option.choices.iter().any(|c| c.id == choice))
        })
    });
    selections
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Windows browser paths are parsed by shape even on a POSIX client: the
/// device being browsed can be remote. A UNC root includes BOTH server and
/// share; extended paths keep their prefix and use only backslash separators.
struct WindowsBrowserPath<'a> {
    root: &'a str,
    rest: &'a str,
    extended: bool,
}

impl WindowsBrowserPath<'_> {
    fn is_separator(&self, ch: char) -> bool {
        ch == '\\' || (!self.extended && ch == '/')
    }

    fn root_path(&self) -> String {
        let root = if self.extended {
            self.root.to_owned()
        } else {
            self.root.replace('/', "\\")
        };
        format!("{root}\\")
    }

    fn normalized_rest(&self) -> String {
        let rest = self.rest.trim_matches(|ch| self.is_separator(ch));
        if self.extended {
            rest.to_owned()
        } else {
            rest.replace('/', "\\")
        }
    }
}

fn windows_browser_path(path: &str) -> Option<WindowsBrowserPath<'_>> {
    let extended = path.starts_with(r"\\?\");
    let start = if extended { 4 } else { 0 };
    let tail = &path[start..];
    let bytes = tail.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes
            .get(2)
            .is_none_or(|b| *b == b'\\' || (!extended && *b == b'/'))
    {
        return Some(WindowsBrowserPath {
            root: &path[..start + 2],
            rest: &path[start + 2..],
            extended,
        });
    }
    let unc_start = if extended {
        tail.get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("UNC\\"))?;
        8
    } else if path.starts_with(r"\\") && !path.starts_with(r"\\.\") {
        2
    } else {
        return None;
    };
    let is_separator = |ch| ch == '\\' || (!extended && ch == '/');
    let server_end = unc_start + path[unc_start..].find(is_separator)?;
    if server_end == unc_start {
        return None;
    }
    let share_start = server_end + 1;
    let share_end = path[share_start..]
        .find(is_separator)
        .map_or(path.len(), |at| share_start + at);
    if share_end == share_start {
        return None;
    }
    Some(WindowsBrowserPath {
        root: &path[..share_end],
        rest: &path[share_end..],
        extended,
    })
}

fn is_windows_path(path: &str) -> bool {
    windows_browser_path(path).is_some()
}

/// Lexical identity for browser rows, crumbs and existing spaces. Preserve
/// POSIX spelling and directory casing (Windows directories can be case
/// sensitive). Prefix aliases are equivalent only for ordinary Windows names.
pub fn folder_path_key(path: &str) -> String {
    let Some(windows) = windows_browser_path(path) else {
        return path.to_owned();
    };
    let rest = windows.normalized_rest();
    let ordinary_name = |name: &str| {
        let stem = name.split('.').next().unwrap_or_default();
        let reserved = matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL"
        ) || stem.get(..3).is_some_and(|prefix| {
            (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
                && matches!(
                    &stem[3..],
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
        });
        !name.ends_with(['.', ' '])
            && !name.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
            && !name.chars().any(char::is_control)
            && !reserved
    };
    let unc_root = windows.extended
        && windows
            .root
            .get(4..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\"));
    let ordinary_names = rest.split('\\').all(ordinary_name)
        && (!unc_root || windows.root[8..].split('\\').all(ordinary_name));
    let root = if windows.extended && ordinary_names {
        let tail = &windows.root[4..];
        if tail
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\"))
        {
            format!(r"\\{}", &tail[4..])
        } else {
            tail.to_owned()
        }
    } else if windows.extended {
        windows.root.to_owned()
    } else {
        windows.root.replace('/', "\\")
    };
    // Drive letters never distinguish case (`c:` is `C:`). Folder names keep
    // theirs: a directory can be made case-sensitive (`fsutil
    // setCaseSensitiveInfo`), so `Repo` and `repo` may be different folders.
    let root = uppercase_drive_letter(root);
    if rest.is_empty() {
        format!("{root}\\")
    } else {
        format!("{root}\\{rest}")
    }
}

/// `c:` → `C:`, also behind an extended `\\?\` prefix; other roots unchanged.
fn uppercase_drive_letter(mut root: String) -> String {
    let drive = root.strip_prefix(r"\\?\").unwrap_or(&root);
    let bytes = drive.as_bytes();
    if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_lowercase() {
        let at = root.len() - 2;
        root[at..at + 1].make_ascii_uppercase();
    }
    root
}

pub fn same_folder_path(left: &str, right: &str) -> bool {
    folder_path_key(left) == folder_path_key(right)
}

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    if let Some(windows) = windows_browser_path(path) {
        let rest = windows.normalized_rest();
        if rest.is_empty() {
            return None;
        }
        let parent = rest.rfind('\\').map_or("", |at| &rest[..at]);
        return Some(format!("{}{parent}", windows.root_path()));
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with(['/', '\\']) {
        format!("{base}{name}")
    } else if windows_browser_path(base).is_some() {
        format!("{base}\\{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Byte length of `name`'s prefix matching `query`, compared char-for-char
/// case-insensitively; `None` when `query` isn't a prefix of `name`. The
/// length indexes into `name` (not `query`) so the completion suffix keeps
/// the folder's real casing: `("Documents", "doc") → Some(3)` → `"uments"`.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder `names` (slash-descend):
/// exact match first — case-SENSITIVE before case-insensitive, so `GitHub/`
/// picks a `GitHub` sibling over `github` — then a unique case-insensitive
/// prefix. Ambiguity resolves to `None`: the slash stays in the query.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

/// Whether a palette query is path-shaped (absolute, home-relative or
/// drive-rooted) rather than a folder name to filter by.
pub fn is_typed_path(query: &str) -> bool {
    query.starts_with(['/', '~']) || is_windows_path(query)
}

/// Interpret a palette query as a typed path jump: absolute (`/disk2/projects`),
/// drive-rooted (`D:\projects`) or home-relative (`~`, `~/github`). Returns the
/// absolute path to browse, trailing separator trimmed. `home` is the device's
/// resolved home — `None` until the first listing lands, when `~` can't expand
/// yet. A query like `~foo` is a folder name, not a path.
pub fn typed_path_target(query: &str, home: Option<&str>) -> Option<String> {
    let query = query.trim();
    if let Some(windows) = windows_browser_path(query) {
        let rest = windows.normalized_rest();
        return Some(if rest.is_empty() {
            windows.root_path()
        } else {
            format!("{}{rest}", windows.root_path())
        });
    }
    if let Some(rest) = query.strip_prefix('~') {
        if is_windows_path(home?) {
            let home = typed_path_target(home?, None)?;
            if rest.is_empty() {
                return Some(home);
            }
            let rest = rest
                .strip_prefix(['/', '\\'])?
                .trim_end_matches(['/', '\\']);
            return Some(if rest.is_empty() {
                home
            } else {
                child_path(&home, &rest.replace('/', "\\"))
            });
        }
        let home = home?.trim_end_matches('/');
        if rest.is_empty() {
            return Some(home.to_string());
        }
        let rest = rest.strip_prefix('/')?.trim_end_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if query.starts_with('/') {
        let trimmed = query.trim_end_matches('/');
        return Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
    }
    None
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    if let Some(windows) = windows_browser_path(path) {
        let root = windows.root_path();
        let mut out = vec![(root.clone(), root.clone())];
        let mut acc = root;
        for segment in windows
            .rest
            .split(|ch| windows.is_separator(ch))
            .filter(|s| !s.is_empty())
        {
            acc = child_path(&acc, segment);
            out.push((segment.to_owned(), acc.clone()));
        }
        return out;
    }
    let root = "/".to_string();
    let mut out = vec![(root.clone(), root)];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&zuse_protocol::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Sentinel for "no keyboard-highlighted row" (`active`): matches no index,
/// and `usize::MAX as isize == -1` — `menu_step` treats it like `None`, so
/// the first Down lands on row 0.
const NO_ACTIVE_ROW: usize = usize::MAX;

/// One project-picker row: every space of a project
/// ([`zuse_protocol::view::project_key`]), named for its representative.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectRow {
    key: String,
    name: String,
}

/// One device-picker row: a device, and the picked project's checkout on it.
#[derive(Clone, Debug, PartialEq)]
struct DeviceRow {
    device_id: String,
    name: String,
    /// The project's space on this device; `None` when no project is picked.
    space: Option<Space>,
    /// The checkout's path, when the device holds several of the project.
    detail: Option<String>,
}

/// What names the selected model, shared by the composer chip and the
/// compact panel so the two never disagree about it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelName {
    /// The catalog row's label, else the remembered pick's, else the id.
    Named(SharedString),
    /// Nothing names it yet, but the harness or model catalog that would
    /// is still on its way.
    Loading,
    /// Nothing offers a model: no agents at all, or a provider whose
    /// catalog came back empty or failed with no pick remembered.
    None { no_agents: bool },
}

/// Which pane the harness/model picker's icon rail is showing (a harness
/// or favorites). `Harness` means
/// "the effective harness's list" — the rail has no browse-without-commit
/// state; clicking a brand icon picks that harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ModelRail {
    All,
    Favorites,
    #[default]
    Harness,
}

/// Cache key for the flattened model-row list: any input that changes the
/// list's CONTENT (not its highlight/selection, which render per-row).
#[derive(Clone, PartialEq, Eq)]
struct ModelRowsKey {
    query: String,
    rail: ModelRail,
    scope: Option<HarnessId>,
    effective: Option<HarnessId>,
    locked: bool,
    catalog_rev: u64,
    selected: Option<String>,
}

/// One row of the model list: the model plus the harness it belongs to —
/// search results and the favorites view mix harnesses, and every row's
/// subline names its harness.
#[derive(Debug, Clone)]
struct ModelRowData {
    harness: HarnessId,
    harness_name: SharedString,
    model: Model,
    selected_only: bool,
    /// Another listed row of the same harness shares this label, so the
    /// row shows its description to tell them apart.
    ambiguous: bool,
}

/// Flag rows whose label another listed row of the same harness shares.
fn mark_ambiguous(rows: &mut [ModelRowData]) {
    let mut labels: HashMap<(HarnessId, String), usize> = HashMap::new();
    for row in rows.iter() {
        *labels
            .entry((row.harness, row.model.label.clone()))
            .or_default() += 1;
    }
    for row in rows.iter_mut() {
        row.ambiguous = labels[&(row.harness, row.model.label.clone())] > 1;
    }
}

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the new-session context tray (Current
    /// checkout/worktree | New worktree).
    Checkout,
    /// The combined agent/model/traits popover: harness tabs across the top,
    /// the tab's model list beneath the search, and the pinned traits tray
    /// (reasoning ladder + model options) at the bottom — one trigger, one
    /// card (the separate Traits popover folded in here).
    HarnessModel,
    /// New-session canvas only: which project the session mints into. A pick
    /// re-keys everything project-derived (refs, harness/model catalogs) via
    /// the state observer.
    Space,
    /// New-session canvas only: the device project-less sessions run on (a
    /// project pick implies its own host and overrides this).
    Device,
}

#[derive(Default)]
pub(crate) struct ReturnComposerFocus {
    pub pointer: Option<gpui::Point<gpui::Pixels>>,
}

impl gpui::EventEmitter<ReturnComposerFocus> for Pickers {}

/// A model picked while bound to thread naming (see [`Pickers::new_for_titles`]);
/// the owner persists it with `SetTitleSettings`.
pub struct TitleModelPicked(pub TitleSettings);

impl gpui::EventEmitter<TitleModelPicked> for Pickers {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelSetting {
    Reasoning,
    Option(String),
}

/// Where a model setting row lives: the tray under the model list (the
/// selected model's settings) or the hover card of a model configured in
/// place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingScope {
    Tray,
    Card,
}

/// A model whose options pick what it runs (Devin Fusion: `lead` and
/// `sidekick`, plus effort and fast mode) is configured from a card that
/// opens when its row is hovered.
fn configured_in_place(model: &Model) -> bool {
    model.options.iter().any(|o| o.id == "lead")
}

/// The card reads like Devin's own Fusion panel: Lead, Effort, Sidekick,
/// then switches.
fn card_order(mut groups: Vec<SettingGroup>) -> Vec<SettingGroup> {
    if let Some(at) = groups
        .iter()
        .position(|g| g.id == ModelSetting::Option("lead".into()))
    {
        let lead = groups.remove(at);
        groups.insert(0, lead);
    }
    for group in &mut groups {
        if group.id == ModelSetting::Reasoning {
            group.label = "Effort".into();
        }
    }
    groups
}

const CONFIG_CARD_WIDTH: f32 = 252.0;

/// Boolean speed settings and binary context sizes use switches.
/// Enabled defaults retain their actual on position.
fn is_toggle(group: &SettingGroup) -> bool {
    let ModelSetting::Option(id) = &group.id else {
        return false;
    };
    zuse_protocol::presentation::model_option_switch_values(
        id,
        group.choices.iter().map(|choice| choice.value.as_str()),
    )
    .is_some()
}

fn toggle_group_is_on(group: &SettingGroup) -> bool {
    let ModelSetting::Option(id) = &group.id else {
        return false;
    };
    zuse_protocol::presentation::model_option_switch_values(
        id,
        group.choices.iter().map(|choice| choice.value.as_str()),
    )
    .is_some_and(|(on, _)| {
        group
            .choices
            .iter()
            .any(|choice| choice.selected && choice.value == on)
    })
}

#[derive(Clone)]
struct SettingChoice {
    label: String,
    reasoning: Option<ReasoningLevel>,
    value: String,
    selected: bool,
    default: bool,
}

#[derive(Clone)]
struct SettingGroup {
    id: ModelSetting,
    label: String,
    choices: Vec<SettingChoice>,
}

pub struct Pickers {
    cloud: cloud::Cloud,
    state: Entity<AppState>,
    config: DraftConfig,
    source_picker: Entity<zuse_viewport::creation_sources::SourcePicker>,
    import_picker: Entity<zuse_viewport::import_chat::ImportMenu>,
    import_tasks: HashMap<u64, Task<()>>,
    source_tasks: HashMap<u64, Task<()>>,
    /// Thread-naming binding (Settings → General): the model picker reads and
    /// emits the local device's title settings instead of a composer draft.
    /// `config.harness` then only tracks the tab being browsed.
    title: Option<TitleSettings>,
    settings_model: settings_models::State,
    /// Sticky last-used picks (zuse `zuse.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    _settings_observe: Subscription,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<String>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<String>,
    device_owner: Option<String>,
    target_generation: u64,
    menu_geometry: HashMap<PickerKind, popover::MenuGeometry>,
    model_trigger_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    // Keep the open menu anchored while its selected label and options change.
    /// The model chip glides to its content's width as the model, effort
    /// or fast tier changes; `chip_resizing` clips instead of truncating
    /// while it moves.
    chip_width: Option<compact::ScalarTransition>,
    chip_resizing: bool,
    open_model_height: f32,
    compact_model_list: bool,
    /// The model list's provider tab strip: its sideways scroll, and the
    /// tab last brought into view as the list scrolled.
    compact_strip_viewed: Option<usize>,
    effort_dragging: bool,
    compact_motion: compact::CompactMotion,
    compact_keyboard: bool,
    effort_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    open: popover::Popup<PickerKind>,
    /// The harness/model picker's rail selection (favorites vs the effective
    /// harness's list). Re-primed on every open.
    model_rail: ModelRail,
    model_scope: Option<HarnessId>,
    model_menu: model_menu::MenuState,
    effort_menu_open: bool,
    effort_trigger_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    setting_menu: Option<ModelSetting>,
    setting_active: usize,
    setting_on_left: bool,
    setting_hover: popover::HoverIntent<ModelSetting>,
    setting_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    setting_scroll: gpui::ScrollHandle,
    setting_scope: SettingScope,
    /// The model row whose settings card is open (see [`configured_in_place`]).
    config_row: Option<usize>,
    config_hover: popover::HoverIntent<usize>,
    config_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    config_on_left: bool,
    /// Where a card choice list just closed after a pick: the pointer is
    /// still over it, and that must not read as leaving the card.
    config_grace: Option<gpui::Bounds<gpui::Pixels>>,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    model_refresh_errors: HashMap<HarnessId, String>,
    /// The loaded harness list belongs to the previous device: reload it
    /// once, keeping its rows on screen meanwhile (see the state observer).
    harnesses_stale: bool,
    /// The stale harness list's reload is in flight (one request).
    harnesses_revalidating: bool,
    /// Loaded model slots still holding the previous device's catalog: shown
    /// while the new device's reload runs, never used to resolve a send, and
    /// cleared only when the new device's answer lands (a failure replaces
    /// the rows rather than keeping the wrong device's).
    stale_models: HashSet<HarnessId>,
    /// Stale slots whose reload is in flight (one request per slot).
    revalidating: HashSet<HarnessId>,
    refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    refs_space: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    workspace_keyboard: bool,
    workspace_ready: bool,
    /// Models-list scroll — keyboard nav keeps the highlighted row in view.
    /// A `UniformListScrollHandle`: the model list virtualizes (7k-model
    /// catalogs must scroll smoothly), and this is its handle; the plain
    /// base handle inside serves the floating scrollbar's metrics.
    model_scroll: gpui::UniformListScrollHandle,
    /// Flattened rows the list/keyboard/⌘N all walk, cached per
    /// [`ModelRowsKey`]: a 7k-model catalog rebuilt+ranked on every
    /// keystroke, arrow press AND render was the picker's open/scroll lag.
    model_rows_cache: std::cell::RefCell<Option<(ModelRowsKey, std::sync::Arc<Vec<ModelRowData>>)>>,
    /// Bumped on every catalog/favorites mutation; invalidates the cache.
    catalog_rev: u64,
    /// Hover/drag state of the floating menu scrollbar. One instance serves
    /// every picker list like `menu_scroll` does — the popups are mutually
    /// exclusive, so only one list mounts at a time.
    menu_bar: popover::MenuScrollbarState,
    /// Scroll handle shared by the plain-div picker lists (branch, project,
    /// device) — the popups are mutually exclusive, so only one mounts at a
    /// time and a fresh open resets the offset.
    menu_scroll: gpui::ScrollHandle,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    /// One-shot mute for the next Edited event's highlight reset — armed by
    /// [`Self::toggle`]'s programmatic clear (see the subscription).
    search_reset_muted: bool,
    focus: FocusHandle,
    /// `ZUSE_OPEN_PICKER` boot: keep claiming focus until it sticks, so
    /// keyboard nav drives the data-side-opened popover (headless rigs have
    /// no synthetic pointer, but synthetic keys do arrive).
    boot_focus_pending: bool,
    /// Reclaim focus while mounting: shell recovery can replace an immediate
    /// focus request while the menu is still absent from the dispatch tree.
    focus_on_mount: bool,
    load_task: Option<Task<()>>,
    /// Own slot: the refs load runs concurrently with the eager
    /// harness/model loads — sharing `load_task` would abort one mid-flight.
    refs_task: Option<Task<()>>,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// Last mid-session switch failure (shown in the ref popover).
    switch_error: Option<String>,
    _search_events: Subscription,
    _state_observe: Subscription,
    _catalog_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self::build(state, None, false, cx)
    }

    /// The model picker bound to thread naming: only title-capable agents,
    /// no traits tray, and picks surface as [`TitleModelPicked`].
    pub fn new_for_titles(
        state: Entity<AppState>,
        settings: TitleSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(state, Some(settings), false, cx)
    }

    pub fn set_title_settings(&mut self, settings: TitleSettings, cx: &mut Context<Self>) {
        self.config.harness = None;
        self.title = Some(settings);
        self.catalog_rev += 1;
        cx.notify();
    }

    fn build(
        state: Entity<AppState>,
        title: Option<TitleSettings>,
        default_settings: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let source_picker = Self::new_creation_sources(cx);
        let import_picker = Self::new_import_menu(cx);
        let search = cx.new(|cx| {
            ComposerInput::with_context("Search…", "PaletteSearch", cx)
                .with_accessibility_role(gpui::Role::SearchInput)
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                // Typing in a filter resets the highlight to the top of the
                // fresh results. `set_text` emits Edited on programmatic
                // clears too, and this subscription runs AFTER `toggle`
                // returns — an unmuted reset clobbers the just-anchored
                // selected row back to 0, leaving the top row wearing a
                // second highlight next to the selection (user report;
                // `toggle` arms the mute right before its clear).
                if !std::mem::take(&mut this.search_reset_muted) {
                    if matches!(
                        this.open_kind(),
                        Some(PickerKind::Branch | PickerKind::Space | PickerKind::Device)
                    ) {
                        this.active = 0;
                    }
                    if this.open_kind() == Some(PickerKind::HarnessModel) {
                        this.setting_menu = None;
                        this.setting_bounds = None;
                        this.active = 0;
                        this.model_scroll_base().set_offset(gpui::Point::default());
                    }
                }
                cx.notify();
            }
            ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                this.on_search_submit(cx)
            }
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::PastedText { .. }
            | ComposerInputEvent::OpenAttachment(_)
            | ComposerInputEvent::OpenFile(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ListDelete
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.sandbox = None;
                this.config.plan = false;
                this.switch_error = None;
            }
            // A space switch invalidates the branch draft + cache — the folder
            // (and possibly the device) changed under them.
            let space = state.read(cx).selected_space.clone();
            let device = state.read(cx).effective_device_id();
            let device_changed = device != this.device_owner;
            let workspace_ready = state
                .read(cx)
                .selected_space_row()
                .is_some_and(|s| s.git_detected);
            if space != this.space_owner
                || device_changed
                || workspace_ready != this.workspace_ready
            {
                this.space_owner = space;
                this.device_owner = device;
                this.workspace_ready = workspace_ready;
                this.setting_menu = None;
                this.setting_bounds = None;
                this.refs_task = None;
                this.config.branch = None;
                this.config.checkout = Self::remembered_checkout(&state, &this.defaults, cx);
                this.refs = Loadable::Idle;
                this.refs_space = None;
            }
            let source_target = this.creation_source_target(cx);
            this.source_picker
                .update(cx, |picker, cx| picker.set_target(source_target, cx));
            // Catalogs are per-DEVICE (fetched from the space's host), so a
            // project switch on the same device keeps them. A device switch
            // revalidates against the new host with the loaded rows still on
            // screen, so the model chip never blanks; in-flight loads for
            // the old host are discarded by the generation bump and restart.
            if device_changed {
                this.target_generation = this.target_generation.wrapping_add(1);
                this.load_task = None;
                this.harnesses_revalidating = false;
                if matches!(this.harnesses, Loadable::Ready(_)) {
                    this.harnesses_stale = true;
                } else {
                    this.harnesses = Loadable::Idle;
                }
                this.models
                    .retain(|_, slot| matches!(slot, Loadable::Ready(_)));
                this.stale_models = this.models.keys().copied().collect();
                this.revalidating.clear();
                this.model_refresh_errors.clear();
                this.catalog_rev += 1;
            }
            cx.notify();
        });
        // A Settings → Providers toggle changed some device's enabled set:
        // force-refresh the cached catalog so the rail/chips follow without a
        // restart (stale rows stay visible while the reload runs).
        let catalog_observe = cx.observe_global::<HarnessCatalogChanged>(|this: &mut Self, cx| {
            this.ensure_harnesses(true, cx);
            cx.notify();
        });
        // Dev/testing knob: `ZUSE_OPEN_PICKER=model|traits|repo|branch` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let boot_open = match std::env::var("ZUSE_OPEN_PICKER").ok().as_deref() {
            _ if title.is_some() || default_settings => None,
            Some("model") => Some(PickerKind::HarnessModel),
            Some("traits") => Some(PickerKind::HarnessModel),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            Some("device") => Some(PickerKind::Device),
            _ => None,
        };
        let mut open = popover::Popup::default();
        if let Some(kind) = boot_open {
            open.open(kind);
        }
        // Sticky last-used picks: loaded synchronously so the very first frame
        // shows the remembered harness/model/reasoning, never a placeholder.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        // Restore explicit opt-outs as well as project picks before the first frame.
        if title.is_none() && !default_settings {
            state.update(cx, |s, _| s.restore_composer_target(&defaults));
        }
        let settings_observe =
            cx.observe_global::<crate::settings::SettingsStore>(|this: &mut Self, cx| {
                this.catalog_rev += 1;
                cx.notify();
            });
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        let device_owner = state.read(cx).effective_device_id();
        let checkout = Self::remembered_checkout(&state, &defaults, cx);
        let workspace_ready = state
            .read(cx)
            .selected_space_row()
            .is_some_and(|s| s.git_detected);
        Self {
            cloud: Default::default(),
            state,
            space_owner,
            device_owner,
            target_generation: 0,
            menu_geometry: HashMap::new(),
            model_trigger_bounds: None,
            chip_width: None,
            chip_resizing: false,
            open_model_height: model_menu_height(0),
            compact_model_list: false,
            compact_strip_viewed: None,
            effort_dragging: false,
            compact_motion: compact::CompactMotion::default(),
            compact_keyboard: false,
            effort_bounds: None,
            config: DraftConfig {
                checkout,
                ..Default::default()
            },
            source_picker,
            import_picker,
            import_tasks: Default::default(),
            source_tasks: Default::default(),
            title,
            settings_model: settings_models::State::new(default_settings, cx),
            defaults,
            _settings_observe: settings_observe,
            data_dir,
            draft_owner,
            open,
            model_rail: ModelRail::default(),
            model_scope: None,
            model_menu: Default::default(),
            effort_menu_open: false,
            effort_trigger_bounds: None,
            setting_menu: None,
            setting_active: 0,
            setting_on_left: false,
            setting_hover: popover::HoverIntent::default(),
            setting_bounds: None,
            setting_scroll: gpui::ScrollHandle::new(),
            setting_scope: SettingScope::Tray,
            config_row: None,
            config_hover: popover::HoverIntent::default(),
            config_bounds: None,
            config_on_left: false,
            config_grace: None,
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            model_refresh_errors: HashMap::new(),
            harnesses_stale: false,
            harnesses_revalidating: false,
            stale_models: HashSet::new(),
            revalidating: HashSet::new(),
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            workspace_keyboard: false,
            workspace_ready,
            model_scroll: gpui::UniformListScrollHandle::new(),
            model_rows_cache: std::cell::RefCell::new(None),
            catalog_rev: 0,
            menu_bar: popover::MenuScrollbarState::default(),
            menu_scroll: gpui::ScrollHandle::new(),
            search,
            search_reset_muted: false,
            focus: cx.focus_handle(),
            boot_focus_pending: boot_open.is_some(),
            focus_on_mount: false,
            load_task: None,
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            _search_events: search_events,
            _state_observe: state_observe,
            _catalog_observe: catalog_observe,
        }
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    fn save_defaults(&self) {
        // The title picker never writes run picks into the composer's memory.
        if self.is_settings_model() {
            return;
        }
        self.persist_defaults();
    }

    /// Re-read the shared memory: the composer and the thread-naming picker
    /// each hold a copy, and both write favorites.
    pub(crate) fn reload_defaults(&mut self) {
        if let Some(dir) = self.data_dir.as_deref() {
            self.defaults = ComposerDefaults::load(dir);
            self.catalog_rev += 1;
        }
    }

    fn persist_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    /// Whether the current agent offers plan mode at all.
    pub fn plan_mode_available(&self, cx: &App) -> bool {
        !self.cloud_launch_frozen()
            && !self.state.read(cx).selected_chat_is_zot_coordinator()
            && !matches!(
                self.resolved(cx).harness,
                Some(HarnessId::Pi | HarnessId::Muse)
            )
    }

    /// Flip plan mode for the draft or the selected chat; the access level is
    /// left as it is. `false` when the chat's saved choices could not be
    /// staged (the caller explains why).
    pub fn toggle_plan_mode(&mut self, cx: &mut Context<Self>) -> bool {
        let resolved = self.resolved(cx);
        // Turning plan mode off is always allowed, even after switching to an
        // agent that does not offer it.
        if self.cloud_launch_frozen() || (!resolved.plan && !self.plan_mode_available(cx)) {
            return true;
        }
        let plan = !resolved.plan;
        if self.state.read(cx).selected_chat.is_none() {
            self.config.plan = plan;
            cx.notify();
            return true;
        }
        let Some(config) = resolved.chat_config() else {
            return false;
        };
        self.state.update(cx, |state, cx| {
            state.stage_chat_configuration(zuse_protocol::ChatConfigPatch::plan(plan), config, cx)
        })
    }

    pub fn set_draft_sandbox(&mut self, sandbox: SandboxLevel, cx: &mut Context<Self>) {
        if self.cloud_launch_frozen() {
            return;
        }
        if self.state.read(cx).selected_chat.is_none() {
            self.config.sandbox = Some(sandbox);
            cx.notify();
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Keep a first-send snapshot stable while it is being submitted.
    fn harness_locked(&self, cx: &App) -> bool {
        if self.cloud.task_binding.is_some() {
            return false;
        }
        let state = self.state.read(cx);
        !self.is_settings_model()
            && state.selected_chat.is_some()
            && !state.side_chat_harness_editable()
            && !state.conversation_provider_editable()
    }

    /// The harnesses this picker offers: runnable ones for the composer,
    /// narrowed to title-capable agents when bound to thread naming.
    fn offered(&self, list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
        let mut offered = offered_harnesses(list);
        if self.title.is_some() {
            offered.retain(|d| zuse_providers::supports_titles(d.id) && d.id != HarnessId::Mock);
        }
        offered
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The selected target device when it differs from the connected
    /// engine's own — harness/model catalogs come from the device that RUNS
    /// the agents (the CLIs live there; the viewer may have neither claude
    /// nor codex installed — user report: "can't load codex models/traits
    /// anywhere" from a Mac without codex).
    fn space_target(&self, cx: &App) -> Option<String> {
        // Thread naming is this device's setting; so are its catalogs.
        if self.is_settings_model() {
            return None;
        }
        let state = self.state.read(cx);
        let device = state.effective_device_id()?;
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    /// Effective harness: picked, or the chat's config, or the first listed.
    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(choice) = self
            .cloud
            .task_binding
            .as_ref()
            .and_then(|binding| binding.choice.as_ref())
        {
            return Some(choice.harness);
        }
        if self.cloud_models_active(cx) {
            return self.cloud.catalog.as_ref().and_then(|catalog| {
                catalog
                    .providers
                    .iter()
                    .find(|row| Some(row.id) == self.config.harness)
                    .or_else(|| {
                        catalog.providers.iter().find(|row| {
                            crate::settings::default_model(cx)
                                .is_some_and(|model| row.id == model.provider)
                        })
                    })
                    .or_else(|| {
                        catalog
                            .providers
                            .iter()
                            .find(|row| Some(row.id) == self.defaults.harness)
                    })
                    .or_else(|| catalog.providers.first())
                    .map(|row| row.id)
            });
        }
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if self.settings_model.defaults {
            return crate::settings::default_model(cx)
                .map(|selection| selection.provider)
                .or_else(|| {
                    self.harnesses
                        .ready()
                        .and_then(|list| self.offered(list).first().map(|row| row.id))
                });
        }
        if let Some(title) = &self.title {
            // Browsing starts on the saved agent, else the first title-capable one.
            return title.harness.or_else(|| {
                self.harnesses
                    .ready()
                    .and_then(|list| self.offered(list).first().map(|d| d.id))
            });
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        // New-chat canvas: the remembered last-used harness (sticky defaults),
        // when the loaded catalog still offers it (the device may have
        // disabled it in Settings → Providers since).
        // A list still from the previous device reads as not loaded: it may
        // offer harnesses this device lacks, and what the chip shows must be
        // what a send would carry.
        let fresh = self.harnesses.ready().filter(|_| !self.harnesses_stale);
        if self.state.read(cx).selected_chat.is_none()
            && let Some(preferred) = crate::settings::default_model(cx)
        {
            if fresh.is_none_or(|list| {
                offered_harnesses(list)
                    .iter()
                    .any(|descriptor| descriptor.id == preferred.provider)
            }) {
                return Some(preferred.provider);
            }
        }
        if let Some(harness) = self.defaults.harness {
            let offered = match fresh {
                Some(list) => offered_harnesses(list).iter().any(|d| d.id == harness),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(harness);
            }
        }
        // Fall back to the first OFFERED harness: the registry lists the mock
        // harness first, and resolving chips against it would boot the
        // new-chat canvas onto "Mock" instead of Claude Code + its default
        // model (it stays available under `ZUSE_HARNESS=mock`).
        fresh.and_then(|list| offered_harnesses(list).first().map(|d| d.id))
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(choice) = self
            .cloud
            .task_binding
            .as_ref()
            .and_then(|binding| binding.choice.as_ref())
        {
            return choice.model.as_deref();
        }

        if self.cloud_models_active(cx) {
            let provider = self
                .cloud
                .catalog
                .as_ref()?
                .providers
                .iter()
                .find(|row| Some(row.id) == self.effective_harness(cx))?;
            return self
                .config
                .model
                .as_deref()
                .and_then(|model| provider.resolve_model(model))
                .or_else(|| {
                    crate::settings::default_model(cx)
                        .filter(|model| model.provider == provider.id)
                        .and_then(|model| provider.resolve_model(&model.model))
                })
                .or_else(|| {
                    self.defaults
                        .model_for(provider.id)
                        .and_then(|model| provider.resolve_model(&model.id))
                })
                .map(|model| model.id.as_str())
                .or(Some(provider.default_model.as_str()));
        }
        if self.settings_model.defaults {
            return crate::settings::default_model(cx)
                .filter(|selection| Some(selection.provider) == self.effective_harness(cx))
                .map(|selection| selection.model.as_str());
        }
        if let Some(title) = &self.title {
            // Only the saved agent's tab shows a selected row.
            return title.model.as_deref().filter(|_| {
                title.harness.is_some() && title.harness == self.effective_harness(cx)
            });
        }
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        if let Some(preferred) =
            crate::settings::default_model(cx).filter(|preferred| preferred.provider == harness)
        {
            return Some(&preferred.model);
        }
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// Effective reasoning — always concrete once the model is known: the
    /// draft pick / chat config / remembered default, clamped to the selected
    /// model's ladder, falling back to the model's default level.
    /// The reasoning level picked, configured or remembered — before any
    /// clamping to the selected model's ladder.
    fn explicit_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        if let Some(choice) = self
            .cloud
            .task_binding
            .as_ref()
            .and_then(|binding| binding.choice.as_ref())
        {
            return choice.reasoning;
        }
        self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: the level last used with this model, else the
                // last-used level overall.
                None => self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.reasoning_for(h, self.effective_model_id(cx)))
                    .or(self.defaults.reasoning),
            }
        })
    }

    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        if !self.model_configuration_available(cx) {
            return None;
        }
        let explicit = self.explicit_reasoning(cx);
        // The old device's ladder must not clamp the chip's level either:
        // a send in this window carries the unclamped value.
        let stale = !self.cloud_models_active(cx)
            && self
                .effective_harness(cx)
                .is_some_and(|h| self.stale_models.contains(&h));
        if stale || self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    /// Only an implicit selection follows the harness default. An explicit ID
    /// absent from the live catalog keeps its identity and remembered chip label.
    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.model_catalog(cx).get(&harness)?.ready()?;
        let selected = self.effective_model_id(cx);
        // An unset title model means "cheapest", not the catalog default.
        if self.title.is_some() && selected.is_none() {
            return None;
        }
        selected_catalog_model(models, selected)
    }

    fn model_name(&self, cx: &App) -> ModelName {
        if self.no_agents_available(cx) && self.effective_harness(cx).is_none() {
            return ModelName::None { no_agents: true };
        }
        if let Some(label) = self.selected_model_label(cx) {
            return ModelName::Named(label.into());
        }
        // A list still from the previous device is loading for this one.
        let catalog_loading = (!self.cloud_models_active(cx) && self.harnesses_stale)
            || matches!(self.harness_catalog(cx), Loadable::Idle | Loadable::Loading);
        let models_loading = self.effective_harness(cx).is_some_and(|harness| {
            !matches!(
                self.model_catalog(cx).get(&harness),
                Some(Loadable::Ready(_)) | Some(Loadable::Error(_))
            )
        });
        if catalog_loading || models_loading {
            ModelName::Loading
        } else {
            ModelName::None { no_agents: false }
        }
    }

    fn selected_model_label(&self, cx: &App) -> Option<String> {
        self.selected_model(cx)
            .map(|model| model.label.clone())
            .or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| {
                                crate::settings::default_model(cx)
                                    .filter(|model| {
                                        Some(model.provider) == self.effective_harness(cx)
                                            && model.model == id
                                    })
                                    .map(|model| model.label.clone())
                            })
                            .or_else(|| self.defaults.label_for(id).map(str::to_owned))
                            .or_else(|| known_model_label(id))
                            .unwrap_or_else(|| id.to_owned()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            })
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the remembered picks for the model the
    /// new-chat canvas resolves to (same id [`Self::resolved`] sends).
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        if let Some(choice) = self
            .cloud
            .task_binding
            .as_ref()
            .and_then(|binding| binding.choice.as_ref())
        {
            return choice.model_options.clone();
        }

        if !self.model_configuration_available(cx) {
            return Default::default();
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat
                .config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default();
        }
        let Some(harness) = self.effective_harness(cx) else {
            return Default::default();
        };
        match self.selected_model(cx) {
            Some(model) => offered_options(
                model,
                self.defaults
                    .model_options_for(harness, &model.id)
                    .cloned()
                    .unwrap_or_default(),
            ),
            // Catalog not loaded (or failed): the picks were validated for
            // this exact model when made, so they are safe to send as-is.
            None => self
                .effective_model_id(cx)
                .and_then(|id| self.defaults.model_options_for(harness, id))
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// The catalog is loaded and offers nothing runnable — the no-agents
    /// state (every enabled harness is missing its CLI, or nothing is
    /// enabled). False while the catalog is still loading or failed
    /// (nothing to conclude yet; offline sends must not be blocked on it).
    pub fn no_agents_available(&self, cx: &App) -> bool {
        // The previous device's list can't speak for this one.
        self.harness_catalog(cx)
            .ready()
            .filter(|_| self.cloud_models_active(cx) || !self.harnesses_stale)
            .is_some_and(|list| self.offered(list).is_empty())
    }

    pub(crate) fn steering_capability(&self, cx: &App) -> Option<bool> {
        if !self.cloud_models_active(cx) && self.harnesses_stale {
            return None;
        }
        self.harness_catalog(cx)
            .ready()
            .and_then(|list| {
                let selected = self.effective_harness(cx)?;
                list.iter().find(|h| h.id == selected)
            })
            .map(|h| {
                h.supports_steering && h.steering_mode == zuse_protocol::SteeringMode::StepBoundary
            })
    }

    pub(crate) fn steers_mid_turn(&self, cx: &App) -> bool {
        self.steering_capability(cx).unwrap_or(false)
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        if let Some(choice) = self.cloud_task_choice() {
            return choice;
        }
        let (sandbox, plan) = if self.state.read(cx).selected_chat.is_none() {
            (
                self.config
                    .sandbox
                    .unwrap_or_else(|| crate::settings::default_permission_mode(cx).sandbox()),
                self.config.plan,
            )
        } else {
            let chat = self.state.read(cx).selected_chat_row();
            (
                crate::settings::default_permission_mode(cx).for_chat(chat),
                chat.and_then(|chat| chat.config.as_ref())
                    .is_some_and(|config| config.plan),
            )
        };
        let harness = self.effective_harness(cx);
        // A catalog still from the previous device must not pick for this
        // one (its default model, its effort ladder): send only what was
        // chosen or remembered, as when no catalog has loaded.
        if !self.cloud_models_active(cx) && harness.is_some_and(|h| self.stale_models.contains(&h))
        {
            return ResolvedRunConfig {
                harness,
                sandbox,
                plan,
                model: self.effective_model_id(cx).map(str::to_string),
                reasoning: self.explicit_reasoning(cx),
                model_options: self.explicit_options(cx),
            };
        }
        ResolvedRunConfig {
            harness,
            sandbox,
            plan,
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    // ---- open/close ----

    /// The picker that's open AND interactive — `None` while one animates out.
    fn open_kind(&self) -> Option<PickerKind> {
        self.open.as_open().copied()
    }

    /// Whether any picker popover is open (shell-side: session-nav shortcuts
    /// go quiet underneath an open popover instead of yanking the session out
    /// from under it).
    pub fn is_open(&self) -> bool {
        self.open.as_open().is_some()
    }

    /// The picker to render: open or mid-exit.
    fn mounted_kind(&self) -> Option<PickerKind> {
        self.open.get().copied()
    }

    /// Begin the exit animation (shared by every close path).
    fn animate_close(&mut self, cx: &mut Context<Self>) {
        if self.is_open() {
            if self.is_settings_model() {
                self.return_settings_focus(cx);
            } else {
                cx.emit(ReturnComposerFocus::default());
            }
        }
        self.dismiss(cx);
    }

    /// Outside clicks and navigation keep focus at the clicked destination.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.focus_on_mount = false;
        self.effort_dragging = false;
        self.cancel_setting_hover();
        self.setting_menu = None;
        self.setting_bounds = None;
        self.close_config();
        self.menu_bar = popover::MenuScrollbarState::default();
        if self.open.begin_close() {
            popover::reap_popup(cx, |pickers: &mut Self| &mut pickers.open);
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.animate_close(cx);
        cx.notify();
    }

    /// Capture knob (`ZUSE_OPEN_DIALOG=model`): open the combined
    /// harness/model menu programmatically.
    /// A jump-slot press while the model menu is open. The shell's session
    /// bindings (Mod+1…9) win the dispatch race — gpui runs a matched
    /// binding before any key handler — so the shell forwards the slot here
    /// instead of going quiet and eating the very chips the rows advertise
    /// (macOS field report: "cmd shortcuts do nothing in the model
    /// selector"). Returns whether the menu was open and the slot consumed.
    pub fn jump_model_slot(&mut self, slot: usize, cx: &mut Context<Self>) -> bool {
        if self.open_kind() != Some(PickerKind::HarnessModel) {
            return false;
        }
        self.activate_model_index(slot, cx);
        cx.notify();
        true
    }

    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_kind() != Some(PickerKind::HarnessModel) {
            self.toggle(PickerKind::HarnessModel, window, cx);
        }
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.cloud_launch_frozen() {
            return;
        }
        if kind == PickerKind::HarnessModel && self.settings_model.pending.is_some() {
            return;
        }
        if kind == PickerKind::HarnessModel && self.settings_model.defaults {
            self.config.harness = None;
            self.config.model = None;
        }
        // A press that found this picker open closes it — the card's
        // `on_mouse_down_out` already began the close on that same press,
        // so by click time the popup reads as closed and a plain toggle
        // would reopen it. A press while a DIFFERENT picker is open doesn't
        // count (see note_trigger_press_matching): that click switches.
        let pressed_open = self.open.take_press_was_open();
        if self.open_kind() == Some(kind) || pressed_open {
            self.animate_close(cx);
            if pressed_open {
                cx.emit(ReturnComposerFocus::default());
            }
            cx.notify();
            return;
        }
        if kind == PickerKind::HarnessModel {
            self.effort_menu_open = false;
            self.model_scope = self
                .effective_harness(cx)
                .or_else(|| self.rail_descriptors(cx).first().map(|p| p.id));
            self.open_model_height = model_menu_height(self.tray_group_count(cx));
            self.compact_model_list = true;
            self.compact_strip_viewed = None;
            self.effort_dragging = false;
            self.compact_keyboard = false;
            self.compact_motion = compact::CompactMotion::default();
        }
        if matches!(kind, PickerKind::Device | PickerKind::Space) {
            self.cloud.refresh();
        }
        self.open.open(kind);
        self.focus_on_mount = true;
        // The plain-div menus (branch / project / device) share one scroll
        // handle; a fresh open starts at the top. The model list resets its
        // own virtualized handle below. Sync the rail baselines so the jump
        // back to the top isn't read as scrolling.
        if kind != PickerKind::HarnessModel {
            popover::reset_menu_scroll(&self.menu_scroll, &mut self.menu_bar);
        }
        // Clearing stale text emits Edited AFTER this function returns —
        // mute that one event so its reset can't clobber the highlight
        // anchored below (the no-op clear is also skipped for the same
        // reason).
        self.search_reset_muted = !self.search.read(cx).text().is_empty();
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        // Prime the model picker's rail BEFORE anchoring the highlight (the
        // visible rows depend on it): favorites when accessible stars exist,
        // otherwise the effective harness. Locked chats keep their own harness.
        if kind == PickerKind::HarnessModel {
            // Favorites may have been starred from the other picker.
            self.reload_defaults();
        }
        if kind == PickerKind::HarnessModel && self.title.is_some() {
            // Each open browses from the saved agent's tab.
            self.config.harness = None;
            self.model_rail = ModelRail::Harness;
        } else if kind == PickerKind::HarnessModel {
            let locked = self.harness_locked(cx);
            let effective = self.effective_harness(cx);
            let has_accessible_favorites = self.defaults.favorites.iter().any(|favorite| {
                !locked || (self.compact_model_picker(cx) && Some(favorite.harness) == effective)
            });
            self.model_rail = if has_accessible_favorites {
                ModelRail::Favorites
            } else {
                ModelRail::Harness
            };
            if self.compact_model_picker(cx) && self.model_rail == ModelRail::Favorites {
                self.model_scope = None;
                self.prefetch_models(false, cx);
            }
        }
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report).
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::NewWorktree => 0,
                CheckoutKind::Local => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::HarnessModel => self.selected_model_index(cx),
            PickerKind::Space => self.selected_space_menu_index(cx),
            PickerKind::Device => self.selected_device_index(cx),
        };
        if kind == PickerKind::Checkout {
            self.workspace_keyboard = false;
        }
        if kind == PickerKind::HarnessModel {
            // scroll_to_item below may land anywhere; the first note of the
            // settled position is the fresh baseline, not scroll motion.
            popover::reset_menu_scroll(&self.model_scroll_base(), &mut self.menu_bar);
            if self.compact_model_picker(cx) {
                self.reset_reference_scroll();
            } else {
                self.scroll_to_model_row(self.active, gpui::ScrollStrategy::Nearest, cx);
            }
        }
        // Model shortcuts start on the menu; its search field takes focus
        // when clicked. Ref and project pickers start in their filter input.
        match kind {
            PickerKind::Branch => {
                self.switch_error = None; // stale mid-session failures don't linger
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search projects…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Device => {
                window.focus(&self.focus, cx);
            }
            PickerKind::HarnessModel => {
                let placeholder = if self.compact_model_picker(cx) {
                    self.compact_search_placeholder(cx)
                } else {
                    "Search models…".into()
                };
                self.search.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, cx);
                });
                window.focus(&self.focus, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            PickerKind::HarnessModel => {
                // Force: the enabled set moves under us (Settings → Providers,
                // possibly from another viewer) — every open revalidates,
                // keeping current rows visible until the fresh catalog lands.
                self.ensure_harnesses(true, cx);
                // Model discovery can recover after a slow/plugin-heavy ACP
                // cold start. Revalidate on every open instead of pinning a
                // timeout/fallback result until the application restarts.
                self.prefetch_models(true, cx);
            }
            // Reopening retries stale Cloud metadata without clearing visible rows.
            // A failed earlier read must not stay latched until app restart.
            PickerKind::Space | PickerKind::Device => {
                self.cloud.refresh();
                self.ensure_cloud(cx);
            }
        }
        cx.notify();
    }

    // ---- loads ----

    fn ensure_harnesses(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.cloud_models_active(cx) {
            if force {
                self.cloud.refresh();
            }
            self.ensure_cloud(cx);
            return;
        }
        // Non-forced (the render loop's eager kick) only loads from Idle: an
        // Error that could re-trigger a load would flip back to Loading
        // before the retry row ever painted (and spam the engine); Retry
        // resets to Idle. FORCED refreshes (a Settings → Providers toggle, a
        // picker open) reload through Ready/Error too — the enabled set just
        // changed under the cache, which otherwise served the boot-time
        // catalog until restart (user report). Stale-while-revalidate: loaded
        // rows stay on screen while the fresh catalog lands.
        let reload = match self.harnesses {
            Loadable::Idle => true,
            Loadable::Loading => false,
            Loadable::Ready(_) | Loadable::Error(_) => {
                force || (self.harnesses_stale && !self.harnesses_revalidating)
            }
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if self.harnesses_stale {
            self.harnesses_revalidating = true;
        }
        let target = self.space_target(cx);
        let generation = self.target_generation;
        if !matches!(self.harnesses, Loadable::Ready(_)) {
            self.harnesses = Loadable::Loading;
            self.catalog_rev += 1;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(target) = &target {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::Value::Object(params))
                .await;
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                if pickers.target_generation != generation {
                    return;
                }
                pickers.harnesses_stale = false;
                pickers.harnesses_revalidating = false;
                pickers.catalog_rev += 1;
                pickers.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.prefetch_models(false, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Resolve the active agent first. Other agents are discovered when the
    /// picker is opened, so startup doesn't compete with chat history on 3G.
    fn prefetch_models(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.cloud_models_active(cx) {
            self.ensure_cloud(cx);
            return;
        }
        let mut targets: Vec<HarnessId> = if self.open_kind() == Some(PickerKind::HarnessModel) {
            self.harnesses
                .ready()
                .map(|list| self.offered(list).iter().map(|d| d.id).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(effective) = self.effective_harness(cx)
            && !targets.contains(&effective)
        {
            targets.insert(0, effective);
        }
        for harness in targets {
            self.ensure_models(harness, force, cx);
        }
    }

    fn ensure_models(&mut self, harness: HarnessId, force: bool, cx: &mut Context<Self>) {
        if self.cloud_models_active(cx) {
            self.ensure_cloud(cx);
            return;
        }
        // Normal prefetches load absent/Idle slots once. Picker-open refreshes
        // also retry Ready/Error slots, while an in-flight load is always
        // reused. Ready rows stay visible until the replacement lands.
        let reload = match self.models.get(&harness) {
            None | Some(Loadable::Idle) => true,
            Some(Loadable::Loading) => false,
            Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => {
                force
                    || (self.stale_models.contains(&harness)
                        && !self.revalidating.contains(&harness))
            }
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if self.stale_models.contains(&harness) {
            self.revalidating.insert(harness);
        }
        let target = self.space_target(cx);
        let generation = self.target_generation;
        if !matches!(self.models.get(&harness), Some(Loadable::Ready(_))) {
            self.models.insert(harness, Loadable::Loading);
            self.catalog_rev += 1;
        }
        cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "harness": harness, "force": force });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            // A plugin-heavy OpenCode cold start can fail once while caches,
            // MCP servers, or plugin runtimes are still warming. Keep this
            // single Loading slot alive for two retries so recovery requires
            // no picker close/reopen and cannot launch duplicate probes.
            let mut attempt = 1_u64;
            let result = loop {
                let result = engine
                    .client()
                    .call(methods::LIST_MODELS, params.clone())
                    .await;
                if result.is_ok() || harness != HarnessId::Opencode || attempt >= 3 {
                    break result;
                }
                if let Err(error) = &result {
                    tracing::warn!(
                        %error,
                        attempt,
                        "OpenCode model discovery failed; retrying automatically"
                    );
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(attempt * 2))
                    .await;
                attempt += 1;
            };
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                if pickers.target_generation != generation {
                    return;
                }
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        // Display hygiene for catalogs from older engines
                        // (`default` alias rows, orphan `[1m]` variants,
                        // version-less alias labels).
                        Ok(models) => Loadable::Ready(normalize_model_rows(harness, models)),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.land_model_reload(harness, loaded, cx);
            })
            .ok();
        })
        .detach();
    }

    /// A model load for the current device landed. A slot that still held
    /// the previous device's rows is current again either way — on failure
    /// the error replaces those rows instead of being retained under them.
    fn land_model_reload(
        &mut self,
        harness: HarnessId,
        loaded: Loadable<Vec<Model>>,
        cx: &mut Context<Self>,
    ) {
        self.revalidating.remove(&harness);
        if self.stale_models.remove(&harness) && matches!(loaded, Loadable::Error(_)) {
            self.models.remove(&harness);
        }
        self.apply_model_catalog(harness, loaded, cx);
    }

    fn apply_model_catalog(
        &mut self,
        harness: HarnessId,
        loaded: Loadable<Vec<Model>>,
        cx: &mut Context<Self>,
    ) {
        if let Loadable::Error(error) = &loaded {
            self.model_refresh_errors.insert(harness, error.clone());
        } else {
            self.model_refresh_errors.remove(&harness);
        }
        if let Loadable::Error(error) = &loaded
            && matches!(self.models.get(&harness), Some(Loadable::Ready(_)))
        {
            tracing::warn!(%error, ?harness, "Model refresh failed; retaining visible rows");
            cx.notify();
            return;
        }
        if let Loadable::Ready(models) = &loaded {
            remember_model_labels(models.iter());
            let fresh = self
                .defaults
                .remember_labels(models.iter().map(|m| (m.id.as_str(), m.label.as_str())));
            if fresh {
                self.save_defaults();
            }
        }
        self.models.insert(harness, loaded);
        self.catalog_rev += 1;
        // A list that landed while its popover is open re-anchors the
        // keyboard highlight onto the selected row (it sat at 0 while
        // loading).
        if self.open_kind() == Some(PickerKind::HarnessModel)
            && self.effective_harness(cx) == Some(harness)
        {
            self.active = self.selected_model_index(cx);
        }
        self.refresh_compact_placeholder(cx);
        cx.notify();
    }

    /// ListRefs for the selected SPACE's folder — targeted at the space's
    /// device (relay-forwarded when remote), keyed/invalidated by space id.
    /// Rows carry checkout state (`current`, `worktreePath`) so the picker can
    /// tag refs and the checkout-kind selector can offer worktree reuse.
    fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return; // a load is already in flight
        }
        // Non-forced (the footer's eager kick, re-run every render) only loads
        // from Idle: an Error must WAIT for an explicit retry/reopen (force),
        // or re-render would flip Error back to Loading before the retry row
        // ever paints — an eternal skeleton plus an RPC storm (user report:
        // "the ref dropdown never loads anything").
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        // Stale-while-revalidate: a forced refresh of an already-loaded space
        // keeps the current rows on screen while the reload runs — a send that
        // just minted a worktree (or a terminal-side branch) appears on the
        // popover's next open without the list ever flashing to a skeleton.
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                // Rows landed under an open, un-searched popover: re-home the
                // nav highlight to the selected row.
                if pickers.open_kind() == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Refs are fixed at creation: an existing session can never move
        // (wing's rule — the footer renders read-only labels there, so this
        // is a belt-and-braces guard).
        if self.state.read(cx).selected_chat_row().is_some() {
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree").
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (a full ref switch — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.animate_close(cx);
        cx.notify();
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder
    /// (relay-forwarded for remote spaces). Success records the pick and
    /// refreshes tags; failure keeps the popover open with git's message.
    fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.config.branch = Some(ref_name);
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current plain ref picked:
            // drop the pick (we don't checkout the main folder) — the current
            // branch takes over.
            self.config.branch = None;
        }
        self.config.checkout = kind;
        if let Some(space) = self.state.read(cx).selected_space_row() {
            self.defaults.workspace_by_project.insert(
                zuse_protocol::view::project_key(space),
                if kind == CheckoutKind::NewWorktree {
                    zuse_protocol::presentation::WorkspaceMode::Worktree
                } else {
                    zuse_protocol::presentation::WorkspaceMode::Local
                },
            );
            self.save_defaults();
        }
        self.animate_close(cx);
        cx.notify();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.cloud_launch_frozen() {
            return;
        }
        if self.harness_locked(cx) {
            return;
        }
        if self.is_settings_model() {
            self.config.harness = Some(harness);
            self.config.model = None;
            self.config.reasoning = None;
            self.ensure_models(harness, false, cx);
            self.active = self.selected_model_index(cx);
            cx.notify();
            return;
        }
        if self.state.read(cx).selected_chat.is_some()
            && self.effective_harness(cx) != Some(harness)
        {
            // Each conversation owns its provider and model settings.
            // Clear the old provider's traits before applying its replacement.
            let model = self.defaults.model_for(harness).map(|m| m.id.clone());
            let reasoning = self.defaults.reasoning_for(harness, model.as_deref());
            let options = model
                .as_deref()
                .and_then(|model| self.defaults.model_options_for(harness, model))
                .cloned()
                .unwrap_or_default();
            self.update_chat_config(cx, move |config| {
                config.harness = harness;
                config.model = model;
                config.reasoning = reasoning;
                config.model_options = options;
            });
        }
        if self.config.harness != Some(harness) {
            // The remembered model for this harness takes over via the
            // defaults fallback; a foreign pick must not linger.
            self.config.model = None;
            self.config.reasoning = None;
        }
        self.config.harness = Some(harness);
        if self.title.is_none() {
            self.defaults.harness = Some(harness);
            self.save_defaults();
        }
        self.model_scroll_base().set_offset(gpui::Point::default());
        self.ensure_models(harness, false, cx);
        // Re-anchor the keyboard highlight onto the new harness's selected row.
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        if self.cloud_launch_frozen() {
            return;
        }
        self.setting_menu = None;
        if self.settings_model.defaults {
            self.choose_default_model(model_id, cx);
            return;
        }
        if self.title.is_some() {
            let settings = TitleSettings {
                harness: self.effective_harness(cx),
                model: (model_id != "default" && !model_id.trim().is_empty()).then_some(model_id),
            };
            self.title = Some(settings.clone());
            self.catalog_rev += 1;
            cx.emit(TitleModelPicked(settings));
            self.close(cx);
            return;
        }
        // The card stays open on a pick (user request): model and traits
        // share one popover now, and adjusting the tray right after choosing
        // a model is the expected flow. Esc, click-out, or the chip close it.
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            self.update_chat_config(cx, move |config| config.model = Some(model_id));
        } else {
            // New chat: draft pick + sticky last-used memory for this harness.
            // Effort follows the model: its own remembered level, if any.
            self.config.model = Some(model_id.clone());
            self.config.reasoning = None;
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .model_catalog(cx)
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        if self.cloud_launch_frozen() {
            return;
        }
        // Always a concrete selection (no toggle-back-to-default).
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
            if let Some(harness) = self.effective_harness(cx) {
                let model = self.effective_model_id(cx).map(str::to_owned);
                self.defaults
                    .remember_reasoning(harness, model.as_deref(), level);
            } else {
                self.defaults.reasoning = Some(level);
            }
            self.save_defaults();
        }
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.cloud_launch_frozen() {
            return;
        }
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if let Some(harness) = self.effective_harness(cx)
            && let Some(model) = self
                .selected_model(cx)
                .filter(|m| m.options.iter().any(|o| o.id == option_id))
                .map(|m| m.id.clone())
        {
            // New chat: the pick is the sticky memory for the catalog model
            // that offered it — never stored unvalidated.
            let options = self.defaults.model_options_mut(harness, &model);
            if default {
                options.remove(&option_id);
            } else {
                options.insert(option_id, serde_json::Value::String(choice_id));
            }
            self.save_defaults();
        }
        cx.notify();
    }

    /// Stage model, reasoning and option fields with the shared coordinator.
    /// Reasoning and options are clamped to the chosen model's catalog.
    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        if self.cloud.task_binding.is_some() {
            if let Some(mut choice) = self
                .cloud
                .task_binding
                .as_ref()
                .and_then(|binding| binding.choice.clone())
                .or_else(|| self.resolved(cx).chat_config())
            {
                change(&mut choice);
                self.set_cloud_task_choice(choice, cx);
            }
            return;
        }
        if self.state.read(cx).selected_chat.is_none() {
            return;
        }
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return; // harness unknown (catalog + chat row both missing) — nothing safe to write
        };
        // Preserve fields the pickers don't own.
        if let Some(existing) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            config.sandbox = existing.sandbox;
            config.plan = existing.plan;
        }
        change(&mut config);
        // Reasoning must stay concrete for whatever model the row now names —
        // same ladder resolution as [`Self::trait_ladder`] (model levels, else
        // the harness's advertised ladder).
        if let Some(models) = self.models.get(&config.harness).and_then(|l| l.ready()) {
            let mut ladder = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if ladder.is_empty()
                && let Some(descriptor) = self
                    .harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == config.harness))
            {
                ladder = descriptor.reasoning_levels.clone();
            }
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
            // Options likewise: a model switch must not carry picks the new
            // model doesn't offer (e.g. a 1M context onto Haiku).
            if let Some(model) = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
            {
                config.model_options =
                    offered_options(model, std::mem::take(&mut config.model_options));
            }
        }
        self.state.update(cx, |state, cx| {
            state.stage_chat_configuration(
                zuse_protocol::ChatConfigPatch::model(config.clone()),
                config,
                cx,
            );
        });
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the harness's advertised ladder) — shared by render and keyboard nav.
    fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(cx)
            .and_then(|h| {
                self.harness_catalog(cx)
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    /// The harness descriptors the picker rail offers, with the committed
    /// harness force-included even when it's outside the offered set (a
    /// dev session's mock harness, or one disabled after the chat existed).
    /// Existing chats only offer their own harness.
    fn rail_descriptors(&self, cx: &App) -> Vec<HarnessDescriptor> {
        let Some(list) = self.harness_catalog(cx).ready() else {
            return Vec::new();
        };
        let mut descriptors = self.offered(list);
        if let Some(effective) = self.effective_harness(cx)
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list.iter().find(|d| d.id == effective && d.installed)
        {
            descriptors.insert(0, descriptor.clone());
        }
        if self.harness_locked(cx) {
            let effective = self.effective_harness(cx);
            descriptors.retain(|d| Some(d.id) == effective);
        }
        descriptors
    }

    /// The model rows the picker currently shows, flat and in render order —
    /// keyboard nav, ⌘N jumps, Enter and the render walk THE SAME list.
    ///
    /// A live search spans every ready harness (the sidebar hides and
    /// the query ignores it); otherwise the rail selection decides —
    /// favorites across harnesses, or the effective harness's list with its
    /// starred rows floated to the top. A locked chat
    /// restricts every view to its own harness.
    /// Cached [`Self::visible_model_rows`]: selection/highlight changes and
    /// re-renders share one flattened list until an input actually changes.
    fn model_rows(&self, cx: &App) -> std::sync::Arc<Vec<ModelRowData>> {
        let key = ModelRowsKey {
            query: self.search.read(cx).text().trim().to_string(),
            rail: self.model_rail,
            scope: self.model_scope,
            effective: self.effective_harness(cx),
            locked: self.harness_locked(cx),
            catalog_rev: self.catalog_rev,
            selected: self.effective_model_id(cx).map(str::to_owned),
        };
        if let Some((cached_key, rows)) = self.model_rows_cache.borrow().as_ref()
            && *cached_key == key
        {
            return rows.clone();
        }
        let rows = std::sync::Arc::new(self.visible_model_rows(cx));
        *self.model_rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn visible_model_rows(&self, cx: &App) -> Vec<ModelRowData> {
        if self.compact_model_picker(cx) {
            return self.reference_model_rows(cx);
        }
        let effective = self.effective_harness(cx);
        let descriptors = self.rail_descriptors(cx);
        // Favorite lookups are per-row; the Vec scan made the flatten
        // O(models × favorites).
        let favorites: std::collections::HashSet<(HarnessId, &str)> = self
            .defaults
            .favorites
            .iter()
            .map(|f| (f.harness, f.model.as_str()))
            .collect();
        let query = self.search.read(cx).text().trim().to_string();
        let mut rows = scoped_model_rows(
            &query,
            self.model_rail,
            effective,
            &descriptors,
            |harness| {
                self.model_catalog(cx)
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .map(|models| models.as_slice())
            },
            |harness, model| favorites.contains(&(harness, model)),
        );
        // This row belongs only to the current selection. It is never merged
        // into the fresh catalog or made available as a new choice elsewhere.
        if self.model_rail == ModelRail::Harness
            && let Some(harness) = effective
            && let Some(id) = self.effective_model_id(cx)
            && self
                .model_catalog(cx)
                .get(&harness)
                .and_then(Loadable::ready)
                .is_some_and(|models| !models.iter().any(|m| m.id == id))
            && let Some(descriptor) = descriptors.iter().find(|d| d.id == harness)
        {
            let label = self
                .selected_model_label(cx)
                .unwrap_or_else(|| id.to_owned());
            let query = query.to_lowercase();
            if query.is_empty()
                || id.to_lowercase().contains(&query)
                || label.to_lowercase().contains(&query)
            {
                rows.insert(
                    0,
                    ModelRowData {
                        harness,
                        harness_name: descriptor.name.clone().into(),
                        selected_only: true,
                        ambiguous: false,
                        model: Model {
                            id: id.into(),
                            label,
                            description: Some(
                                "Selected in this chat; absent from the current model list".into(),
                            ),
                            reasoning_levels: vec![],
                            options: vec![],
                        },
                    },
                );
            }
        }
        mark_ambiguous(&mut rows);
        rows
    }

    /// The row the keyboard-nav highlight starts on: the resolved selected
    /// model's index in the VISIBLE rows (the favorites/search views may not
    /// contain it — then 0), 0 while the list is loading.
    fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self
            .effective_model_id(cx)
            .or_else(|| self.selected_model(cx).map(|m| m.id.as_str()));
        let effective = self.effective_harness(cx);
        self.model_rows(cx)
            .iter()
            .position(|row| {
                Some(row.harness) == effective && selected == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    /// The picker's visible row count (keyboard nav bounds).
    fn model_rows_len(&self, cx: &App) -> usize {
        self.model_rows(cx).len()
    }

    /// Enter on the harness/model popover: pick the highlighted model.
    fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        if self.setting_menu.is_some() {
            self.activate_setting_choice(cx);
        } else if self.compact_model_picker(cx) && self.compact_model_list {
            self.activate_model_index(self.active, cx);
        } else if let Some(index) = self.active.checked_sub(self.model_rows_len(cx)) {
            if let Some(group) = self.setting_groups(cx).get(index).cloned() {
                if is_toggle(&group) {
                    self.toggle_tray_option(group, cx);
                } else {
                    self.open_setting(SettingScope::Tray, group.id.clone(), cx);
                }
            }
        } else {
            self.activate_model_index(self.active, cx);
        }
    }

    /// Pick the visible row at `ix` — a foreign-harness row (favorites /
    /// search) switches the harness first, exactly like clicking its rail
    /// icon and then the model.
    fn activate_model_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.is_settings_model() && (!self.is_open() || self.settings_model.pending.is_some()) {
            return;
        }
        let Some(row) = self.model_rows(cx).get(ix).cloned() else {
            return;
        };
        if row.selected_only {
            return;
        }
        if self.effective_harness(cx) != Some(row.harness) {
            if self.harness_locked(cx) {
                return;
            }
            self.pick_harness(row.harness, cx);
        }
        self.pick_model(row.model.id, cx);
        if self.compact_model_picker(cx) {
            self.compact_model_list = true;
            self.focus_on_mount = false;
            self.close(cx);
        }
    }

    /// Star/unstar a model and persist it with the sticky defaults.
    fn toggle_model_favorite(&mut self, harness: HarnessId, model: &str, cx: &mut Context<Self>) {
        self.reload_defaults();
        let keyboard_row = (self.compact_model_picker(cx) && self.compact_keyboard)
            .then(|| self.model_rows(cx).get(self.active).cloned())
            .flatten();
        self.defaults.toggle_favorite(harness, model);
        self.persist_defaults();
        self.catalog_rev += 1;
        // Pointer interactions retain the selected-row highlight after sorting.
        self.active = self.selected_model_index(cx);
        // Keyboard focus follows identity, independently of selection styling.
        if let Some(row) = keyboard_row {
            self.active = self
                .model_rows(cx)
                .iter()
                .position(|candidate| {
                    candidate.harness == row.harness && candidate.model.id == row.model.id
                })
                .unwrap_or(self.active);
            self.scroll_to_model_row(self.active, gpui::ScrollStrategy::Nearest, cx);
        }
        cx.notify();
    }

    fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// the session's branch on an existing chat, the draft pick on a new one,
    /// else the current branch. Capped to the displayed window.
    fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self, cx: &App) -> CheckoutPlan {
        if self.creation_source_target(cx).is_some() {
            if let Some(zuse_protocol::creation_source::Resolution::Checkout { outcome, .. }) =
                self.source_picker.read(cx).selection()
            {
                return CheckoutPlan::ReuseWorktree {
                    path: outcome.worktree.path.clone(),
                    branch: outcome.worktree.branch.clone(),
                };
            }
        }
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Workspace mode is independent of the selected base branch.
    fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "Worktree",
            CheckoutKind::Local => "Local",
        }
    }

    // ---- the project picker (new-session canvas) ----

    /// The picker's project rows: one per project across every device —
    /// clones and worktrees sharing a repository identity are one row, named
    /// for their representative. The device chip then picks which checkout.
    fn project_rows(&self, cx: &App) -> Vec<ProjectRow> {
        let state = self.state.read(cx);
        let mut rows: Vec<ProjectRow> = Vec::new();
        for space in &state.spaces {
            let key = zuse_protocol::view::project_key(space);
            if rows.iter().any(|row| row.key == key) {
                continue;
            }
            rows.push(ProjectRow {
                key,
                name: state.representative_space(space).display_name().to_string(),
            });
        }
        rows.sort_by_key(|row| (row.name.to_lowercase(), row.key.clone()));
        rows
    }

    /// [`Self::project_rows`] matching the search query, ranked
    /// (`popover::filter_indices`).
    fn filtered_project_rows(&self, cx: &App) -> Vec<ProjectRow> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.project_rows(cx);
        let names: Vec<String> = rows.iter().map(|row| row.name.clone()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    /// Current project row on an unsearched open, or the final opt-out row.
    /// An implicit empty selection has no highlight until the user navigates.
    fn selected_space_menu_index(&self, cx: &App) -> usize {
        let state = self.state.read(cx);
        if let Some(project) = &state.cloud_project {
            let Some(id) = project["projectId"].as_str() else {
                return NO_ACTIVE_ROW;
            };
            return self
                .filtered_cloud_projects(cx)
                .iter()
                .position(|row| row["projectId"].as_str() == Some(id))
                .map(|index| self.filtered_project_rows(cx).len() + index + 1)
                .unwrap_or(NO_ACTIVE_ROW);
        }
        if state.no_project {
            0
        } else {
            // The missing-selection sentinel is not a project row ordinal.
            self.selected_project_index(cx)
                .checked_add(1)
                .unwrap_or(NO_ACTIVE_ROW)
        }
    }

    fn selected_project_index(&self, cx: &App) -> usize {
        if self.state.read(cx).no_project {
            return self.project_rows(cx).len();
        }
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(zuse_protocol::view::project_key);
        selected
            .and_then(|key| self.project_rows(cx).iter().position(|row| row.key == key))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// Pick a project, keeping the device when it has a checkout of it:
    /// the current checkout, else one on the current device, else this
    /// device's, else the first.
    fn pick_project(&mut self, key: String, cx: &mut Context<Self>) {
        let space_id = {
            let state = self.state.read(cx);
            let Some(member) = state
                .spaces
                .iter()
                .find(|s| zuse_protocol::view::project_key(s) == key)
            else {
                return;
            };
            let members = state.project_members(member);
            let selected = state.selected_space_row().map(|s| s.id.clone());
            let device = state.effective_device_id();
            members
                .iter()
                .find(|s| selected.as_deref() == Some(s.id.as_str()))
                .or_else(|| {
                    members
                        .iter()
                        .find(|s| device.as_deref() == Some(s.device_id.as_str()))
                })
                .or(members.first())
                .map(|s| s.id.clone())
        };
        if let Some(space_id) = space_id {
            self.pick_space(space_id, cx);
        }
    }

    /// Re-home the canvas onto another project. The state observer does the
    /// heavy lifting: branch draft, ref cache, and the per-device
    /// harness/model catalogs all invalidate on the project change.
    fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.leave_cloud(cx);
        self.state.update(cx, |s, cx| {
            s.auto_selected = true;
            s.select_space(Some(space_id), cx);
        });
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_no_project(&mut self, cx: &mut Context<Self>) {
        self.leave_cloud(cx);
        self.state.update(cx, |s, cx| {
            // A late opening chats frame must not auto-open an old session
            // after the user has explicitly chosen the new-session target.
            s.auto_selected = true;
            s.select_space(None, cx);
        });
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.leave_cloud(cx);
        self.state
            .update(cx, |s, cx| s.select_device(device_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the device/project picks — the "last selected" defaults the
    /// next boot's canvas restores.
    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.device = state
                .selected_device
                .clone()
                .or_else(|| state.local_device_id.clone());
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir {
            if let Err(err) = self.defaults.save(dir) {
                tracing::warn!(error = %err, "composer-defaults save failed");
            }
        }
    }

    /// The device picker's rows. With a project picked: its checkouts, one
    /// row per device (this device first), with the path when a device holds
    /// several. Without one: every device — project-less sessions run in its
    /// home.
    fn device_rows(&self, cx: &App) -> Vec<DeviceRow> {
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let device_name = |id: &str| {
            state
                .device_name(id)
                .unwrap_or("Unknown device")
                .to_string()
        };
        if let Some(space) = state.selected_space_row() {
            let members = state.project_members(space);
            return members
                .iter()
                .map(|member| {
                    let shared = members
                        .iter()
                        .filter(|other| other.device_id == member.device_id)
                        .count()
                        > 1;
                    DeviceRow {
                        device_id: member.device_id.clone(),
                        name: device_name(&member.device_id),
                        space: Some((*member).clone()),
                        detail: shared.then(|| member.path.clone()),
                    }
                })
                .collect();
        }
        let mut devices: Vec<&zuse_protocol::Device> = state.devices.iter().collect();
        devices.sort_by_key(|d| {
            (
                local.as_deref() != Some(d.id.as_str()),
                d.name.to_lowercase(),
                d.id.clone(),
            )
        });
        devices
            .into_iter()
            .map(|device| DeviceRow {
                device_id: device.id.clone(),
                name: device.name.clone(),
                space: None,
                detail: None,
            })
            .collect()
    }

    /// [`Self::device_rows`] filtered by the search box (same ranked
    /// substring match as the project rows).
    fn filtered_device_rows(&self, cx: &App) -> Vec<DeviceRow> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.device_rows(cx);
        let names: Vec<String> = rows
            .iter()
            .map(|row| match &row.detail {
                Some(detail) => format!("{} {detail}", row.name),
                None => row.name.clone(),
            })
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    fn selected_device_index(&self, cx: &App) -> usize {
        let (selected, effective) = {
            let state = self.state.read(cx);
            (
                state.selected_space_row().map(|s| s.id.clone()),
                state.effective_device_id(),
            )
        };
        self.device_rows(cx)
            .iter()
            .position(|row| match &row.space {
                Some(space) => selected.as_deref() == Some(space.id.as_str()),
                None => effective.as_deref() == Some(row.device_id.as_str()),
            })
            .unwrap_or(0)
    }

    /// A device row picks the project's checkout there, or — without a
    /// project — the device itself.
    fn pick_device_row(&mut self, row: DeviceRow, cx: &mut Context<Self>) {
        match row.space {
            Some(space) => self.pick_space(space.id, cx),
            None => self.pick_device(row.device_id, cx),
        }
    }

    /// Project-scoped computers, followed by the prepared Cloud destination.
    fn render_device_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let rows = self.filtered_device_rows(cx);
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let selected_space = state.selected_space_row().map(|s| s.id.clone());
        let effective = state.effective_device_id();
        let cloud = state.cloud_project.is_some();
        let mut body = div().flex().flex_col();
        for (i, row) in rows.into_iter().enumerate() {
            let selected = !cloud
                && match &row.space {
                    Some(space) => selected_space.as_deref() == Some(&space.id),
                    None => effective.as_deref() == Some(&row.device_id),
                };
            let online = state.device_online(&row.device_id, chrono::Utc::now());
            let label = if local.as_deref() == Some(&row.device_id) {
                "This computer".to_owned()
            } else {
                row.name.clone()
            };
            body = body.child(
                zuse_viewport::creation_style::option(
                    ("device-row", i),
                    label,
                    "monitor",
                    selected,
                )
                .when(self.active == i, |v| v.bg(theme.element_hover))
                .when(!online, |v| {
                    v.child(zuse_viewport::creation_style::meta("Offline"))
                })
                .child(zuse_viewport::creation_style::selection(selected))
                .on_click(cx.listener(move |this, _, _, cx| this.pick_device_row(row.clone(), cx))),
            );
        }
        body.child(self.cloud_rows(cx)).into_any_element()
    }

    /// The project popover: search + one row per project across devices
    /// (check on the current pick), then "New project" and the opt-out rows.
    /// No per-row `@ device` tag — the device chip next door picks the host.
    fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let cloud_rows = self.cloud_project_rows(cx);
        let rows = self.filtered_project_rows(cx);
        let has_cloud = !self.filtered_cloud_projects(cx).is_empty();
        let state = self.state.read(cx);
        let selected = state
            .cloud_project
            .is_none()
            .then(|| {
                state
                    .selected_space_row()
                    .map(zuse_protocol::view::project_key)
            })
            .flatten();
        let no_project = state.no_project && state.cloud_project.is_none();
        let mut choices = div()
            .id("creation-native-projects")
            .max_h(px(240.))
            .gap(px(1.))
            .overflow_y_scroll()
            .track_scroll(&self.menu_scroll)
            .flex()
            .flex_col()
            .child(
                zuse_viewport::creation_style::option("project-none", "No project", "", no_project)
                    .h(px(32.))
                    .when(self.active == 0, |v| v.bg(theme.element_hover))
                    .child(zuse_viewport::creation_style::selection(no_project))
                    .on_click(cx.listener(|this, _, _, cx| this.pick_no_project(cx))),
            );
        if rows.is_empty() && !has_cloud {
            choices = choices.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(if self.search.read(cx).text().is_empty() {
                        crate::localization::text("chat-chat_landing_no_projects_yet", cx)
                    } else {
                        ("No projects match.").to_string()
                    }),
            );
        }
        for (i, row) in rows.into_iter().enumerate() {
            let selected = selected.as_deref() == Some(row.key.as_str());
            choices = choices.child(
                zuse_viewport::creation_style::option(
                    ("space-row", i),
                    row.name,
                    "folder",
                    selected,
                )
                .h(px(32.))
                .when(self.active == i + 1, |v| v.bg(theme.element_hover))
                .child(zuse_viewport::creation_style::selection(selected))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.pick_project(row.key.clone(), cx)),
                ),
            );
        }
        choices = choices.child(cloud_rows);
        div()
            .flex()
            .flex_col()
            .child(
                div().pb(px(6.)).child(
                    div()
                        .h(px(32.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .rounded(px(7.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.bg)
                        .child(
                            crate::icons::icon(crate::icons::MAGNIFER)
                                .size(px(14.))
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().child(self.search.clone())),
                ),
            )
            .child(choices)
            .child(zuse_viewport::creation_style::separator())
            .child(
                zuse_viewport::creation_style::option(
                    "project-new",
                    "Add new project…",
                    "plus",
                    false,
                )
                .h(px(32.))
                .role(gpui::Role::Button)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dismiss(cx);
                    window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
                })),
            )
            .into_any_element()
    }
    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open_kind() == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
        if self.open_kind() == Some(PickerKind::Space) {
            let rows = self.filtered_project_rows(cx);
            if self.active == 0 {
                self.pick_no_project(cx);
            } else if let Some(row) = rows.get(self.active - 1) {
                self.pick_project(row.key.clone(), cx);
            } else {
                self.select_cloud_index(self.active - 1 - rows.len(), cx);
            }
        }
        if self.open_kind() == Some(PickerKind::Device) {
            let rows = self.filtered_device_rows(cx);
            if let Some(row) = rows.get(self.active).cloned() {
                self.pick_device_row(row, cx);
            } else if self.active == rows.len() {
                self.select_cloud_destination(cx);
            }
        }
        // Palette-search Enter submits the highlighted model or setting.
        if self.open_kind() == Some(PickerKind::HarnessModel) {
            self.activate_model_row(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &Window, cx: &mut Context<Self>) {
        self.cancel_setting_hover();
        // The frame stays mounted (and possibly focused) through the exit
        // animation — keys must not drive a dying popover.
        if !self.open.is_open() {
            return;
        }
        if self.open_kind() == Some(PickerKind::Checkout) {
            match event.keystroke.key.as_str() {
                "home" | "end" => {
                    self.workspace_keyboard = true;
                    self.active = usize::from(event.keystroke.key == "end");
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                "up" | "down" => self.workspace_keyboard = true,
                _ => {}
            }
        }
        if self.open_kind() == Some(PickerKind::HarnessModel) && self.compact_model_picker(cx) {
            // Escape only steps back a page or closes a submenu: it should
            // not light focus rings on the controls it returns to. The next
            // navigation key brings them back.
            self.compact_keyboard = event.keystroke.key != "escape";
            if self.compact_model_list
                && event.keystroke.modifiers.platform
                && event.keystroke.modifiers.shift
                && event.keystroke.key.eq_ignore_ascii_case("f")
            {
                if let Some(row) = self.model_rows(cx).get(self.active).cloned() {
                    self.toggle_model_favorite(row.harness, &row.model.id, cx);
                }
                cx.stop_propagation();
                return;
            }
        }
        if self.setting_menu.is_some() {
            match event.keystroke.key.as_str() {
                "escape" | "left" => {
                    self.setting_menu = None;
                    self.setting_bounds = None;
                }
                "up" | "down" => {
                    let count = self
                        .scope_groups(self.setting_scope, cx)
                        .into_iter()
                        .find(|g| Some(&g.id) == self.setting_menu.as_ref())
                        .map(|g| g.choices.len())
                        .unwrap_or(0);
                    self.setting_active = popover::menu_step(
                        Some(self.setting_active),
                        count,
                        if event.keystroke.key == "up" { -1 } else { 1 },
                    )
                    .unwrap_or(0);
                    self.setting_scroll.scroll_to_item(self.setting_active);
                }
                "enter" => self.activate_setting_choice(cx),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.open_kind() == Some(PickerKind::HarnessModel) && self.compact_model_picker(cx) {
            if !self.compact_model_list {
                self.compact_panel_key(event, cx);
                return;
            }
            if event.keystroke.key == "escape" {
                self.close(cx);
                cx.stop_propagation();
                return;
            }
        }
        if event.keystroke.key == "right"
            && !self.compact_model_picker(cx)
            && self.open_kind() == Some(PickerKind::HarnessModel)
            && self.active >= self.model_rows_len(cx)
        {
            self.activate_model_row(cx);
            cx.stop_propagation();
            return;
        }
        // ⌘1…⌘9 jump-picks the Nth visible model row (the chips on the rows
        // advertise these).
        if self.open_kind() == Some(PickerKind::HarnessModel)
            && event.keystroke.modifiers.platform
            && let Ok(n) = event.keystroke.key.parse::<usize>()
            && (1..=9).contains(&n)
        {
            self.activate_model_index(n - 1, cx);
            cx.notify();
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );

        match key {
            MenuKey::Escape => {
                self.animate_close(cx);
                cx.notify();
                cx.stop_propagation();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open_kind() {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    // Continue from model rows into the pinned settings triggers.
                    Some(PickerKind::HarnessModel) => {
                        self.model_rows_len(cx)
                            + if self.compact_model_picker(cx) {
                                0
                            } else {
                                self.setting_groups(cx).len()
                            }
                    }
                    Some(PickerKind::Space) => {
                        self.filtered_project_rows(cx).len()
                            + self.filtered_cloud_projects(cx).len()
                            + 1
                    }
                    Some(PickerKind::Device) => {
                        self.filtered_device_rows(cx).len()
                            + usize::from(self.cloud_destination(cx).is_some())
                    }
                    None => 0,
                };
                let current = (self.active != NO_ACTIVE_ROW).then_some(self.active);
                self.active = popover::menu_step(current, count, delta).unwrap_or(0);
                // Keep the highlighted MODEL row in view (the rows are the
                // scroll container's direct children, so indices map 1:1);
                // the traits chips below live in the pinned tray and never
                // need scrolling into view.
                if self.open_kind() == Some(PickerKind::HarnessModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.scroll_to_model_row(self.active, gpui::ScrollStrategy::Nearest, cx);
                }
                cx.notify();
                cx.stop_propagation();
            }
            MenuKey::Enter | MenuKey::ModEnter => {
                if self.open_kind() == Some(PickerKind::HarnessModel) {
                    self.activate_model_row(cx);
                } else if self.open_kind() == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::NewWorktree
                    } else {
                        CheckoutKind::Local
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn menu_geometry(&self) -> popover::MenuGeometry {
        self.mounted_kind()
            .and_then(|kind| self.menu_geometry.get(&kind).copied())
            .unwrap_or(popover::MenuGeometry {
                height: 320.0,
                below: false,
            })
    }

    fn measure_trigger(&self, kind: PickerKind, cx: &Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let compact = kind == PickerKind::HarnessModel && self.compact_model_picker(cx);
        let this_setting = self.is_settings_model();
        let settings_height = this_setting.then(|| self.settings_menu_height(cx));
        // New-thread model and workspace menus open down, matching the
        // centered composer's layout. In-thread menus retain adaptive placement.
        let below = (self.title.is_some() || self.state.read(cx).selected_chat.is_none())
            && matches!(kind, PickerKind::Branch | PickerKind::Space);
        gpui::canvas(
            move |bounds, window, cx| {
                let mut geometry = popover::menu_geometry(
                    f32::from(bounds.top()),
                    f32::from(bounds.bottom()),
                    f32::from(window.viewport_size().height),
                );
                let space_below = (f32::from(window.viewport_size().height - bounds.bottom())
                    - 14.0)
                    .clamp(0.0, 640.0);
                // Prefer below on the new-chat canvas only when it can fit
                // useful content. Tall drafts/attachments can put the trigger
                // near the window bottom; retain adaptive placement there.
                let prefer_below = below && space_below >= 180.0;
                if prefer_below {
                    geometry = popover::MenuGeometry {
                        height: space_below,
                        below: true,
                    };
                }
                // The compact card opens down on the new-chat canvas like the
                // standard menu, and up from the docked composer.
                if compact && !prefer_below && !this_setting {
                    geometry = popover::MenuGeometry {
                        height: (f32::from(bounds.top()) - 14.0).clamp(0.0, 640.0),
                        below: false,
                    };
                }
                if let Some(required) = settings_height {
                    let above = (f32::from(bounds.top()) - 14.).max(0.);
                    let below =
                        (f32::from(window.viewport_size().height - bounds.bottom()) - 14.).max(0.);
                    let prefer_below = below >= required || below > above;
                    geometry = popover::MenuGeometry {
                        height: (if prefer_below { below } else { above }).min(640.),
                        below: prefer_below,
                    };
                }
                entity
                    .update(cx, |this, cx| {
                        if kind == PickerKind::HarnessModel {
                            this.model_trigger_bounds = Some(bounds);
                        }
                        if this.menu_geometry.get(&kind) != Some(&geometry) {
                            this.menu_geometry.insert(kind, geometry);
                            cx.notify();
                        }
                    })
                    .ok();
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0()
    }

    // ---- render ----

    /// The model chip's content width (padding, brand slot, label, effort
    /// and fast mark), eased toward on every change. The chip is anchored at
    /// its trailing edge beside Send, as is its popover, so only its leading
    /// edge moves and an open card stays put while the selection updates.
    #[allow(clippy::too_many_arguments)]
    fn model_chip_width(
        &mut self,
        window: &mut Window,
        label: &SharedString,
        suffix: Option<&SharedString>,
        label_loading: bool,
        fast: bool,
        cx: &App,
    ) -> f32 {
        const GAP: f32 = 6.0;
        let size = crate::typography::text_xs().to_pixels(window.rem_size());
        let mut font = window.text_style().font();
        font.weight = gpui::FontWeight::MEDIUM;
        let measure = |text: &SharedString| -> f32 {
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            f32::from(
                window
                    .text_system()
                    .shape_line(text.clone(), size, &[run], None)
                    .width,
            )
            .ceil()
        };
        let label_width = if label_loading { 56.0 } else { measure(label) };
        // Pill: 10px sides, 12px brand icon, label, 12px chevron.
        let mut width = 2.0 * 10.0 + 12.0 + GAP + label_width + GAP + 12.0;
        if let Some(suffix) = suffix {
            width += GAP + measure(suffix);
        }
        if fast {
            width += GAP + 13.0;
        }
        let target = width.min(248.0);
        let (value, moving) = compact::ScalarTransition::sample(
            &mut self.chip_width,
            target,
            std::time::Instant::now(),
            cx.reduce_motion(),
        );
        self.chip_resizing = moving;
        if moving {
            window.request_animation_frame();
        }
        value
    }

    #[allow(clippy::too_many_arguments)]
    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        // The chip never collapses while identity resolves (user report):
        // `icon_loading` swaps the brand slot for the pixel-glyph loader
        // (harness unknown), `label_loading` swaps the text for a ghost bar
        // (model unknown).
        icon_loading: bool,
        label_loading: bool,
        suffix: Option<(SharedString, Option<gpui::Hsla>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let base = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Space => "picker-space",
            PickerKind::Device => "picker-device",
        };
        // Hover state is keyed globally: two composers on screen (main +
        // side chat) must not light each other's chips, so the key carries
        // this picker's entity.
        let id: SharedString = format!("{base}-{}", cx.entity_id()).into();
        let open = self.open_kind() == Some(kind);
        let resizing = kind == PickerKind::HarnessModel && self.chip_resizing;
        // The composer's model trigger is a filled pill (`h-7 rounded-full
        // bg-muted/65 px-2.5 text-[11px]`, 12px brand icon, trailing caret);
        // the destination triggers stay quiet ghost chips.
        let model = kind == PickerKind::HarnessModel;
        let pill_rest = theme.muted_fill().opacity(0.65);
        let pill_hover = theme.muted_fill();
        // Ghost pill (zuse composer/styles.tsx `pill`): `h-8 rounded-lg px-2.5
        // gap-1.5 text-[12px] font-medium text-muted-foreground`, icons size-4,
        // hover/open wash — no border, no caret; the actions row stays quiet.
        div()
            .relative()
            .child(self.measure_trigger(kind, cx))
            .id(id.clone())
            .h(px(if model { 28.0 } else { 32.0 }))
            .max_w(px(248.0))
            // Shrinkable under row pressure — four footer chips share one
            // line; without min_w_0 they overflowed and painted overlapped.
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            // The model trigger sits beside the right-hand composer actions.
            // Its own padding participates in that visible gap; footer and
            // destination triggers keep their wider independent hit areas.
            .px(px(10.0))
            .when(model, |el| el.rounded_full())
            .when(!model, |el| el.rounded(px(8.0)))
            .text_size(crate::typography::text_xs())
            .font_weight(gpui::FontWeight::MEDIUM)
            // zuse composer/styles.tsx `pill`: `transition-colors` — the wash
            // and text brighten fade over 150ms.
            .text_color(motion::hover_blend(
                &id,
                if model || set {
                    theme.text
                } else {
                    theme.text_muted
                },
                theme.text_strong,
            ))
            .bg(match (model, open) {
                (true, true) => pill_hover,
                (true, false) => motion::hover_blend(&id, pill_rest, pill_hover),
                (false, true) => theme.element_hover,
                (false, false) => {
                    motion::hover_blend(&id, gpui::transparent_black(), theme.element_hover)
                }
            })
            .on_hover(motion::hover_listener(id.clone()))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when(icon_loading, |el| {
                el.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                    "picker-chip-loader",
                    2.0,
                    theme.loader_glyph(),
                    cx.entity_id(),
                    cx,
                )))
            })
            .when_some(
                (!icon_loading).then_some(chip_icon).flatten(),
                |el, (path, tint)| {
                    el.child(crate::icons::brand_icon(
                        path,
                        if model { 12.0 } else { 16.0 },
                        tint.unwrap_or(theme.text_muted),
                    ))
                },
            )
            .when(label_loading, |el| {
                // Nothing is named on screen: the label rolls in from empty.
                rolling(format!("{id}-label"), SharedString::default(), true);
                el.child(popover::skeleton_bar(56.0, cx.entity_id(), cx))
            })
            .when(!label_loading, |el| {
                // A changing label rolls (Scritto-style); at rest it is the
                // plain truncating label, or the clip-don't-ellipsize variant
                // while the model chip's width glides.
                let rolling = rolling(format!("{id}-label"), label.clone(), cx.reduce_motion());
                el.child(match rolling {
                    Some(rolling) => rolling,
                    None if resizing => {
                        resizing_chip_text(format!("{id}-label").into(), 1.0, None, label)
                    }
                    None => div().min_w_0().truncate().child(label).into_any_element(),
                })
            })
            // The effort half of the combined model+effort chip (and the space
            // chip's "@ device" tag): muted, no icon — one button, two tones.
            // `tint` overrides the muted tone (the offline warning). Under row
            // pressure the suffix yields FIRST (large shrink factor) so the
            // model name — the run's identity — truncates last.
            .when_some(suffix, |el, (suffix, tint)| {
                let color = tint.unwrap_or(theme.text_muted.opacity(0.7));
                let rolling = rolling(format!("{id}-suffix"), suffix.clone(), cx.reduce_motion());
                el.child(if let Some(rolling) = rolling {
                    div()
                        .flex()
                        .flex_shrink(1000.0)
                        .min_w_0()
                        .text_color(color)
                        .child(rolling)
                        .into_any_element()
                } else if resizing {
                    resizing_chip_text(format!("{id}-suffix").into(), 1000.0, Some(color), suffix)
                } else {
                    div()
                        .flex_shrink(1000.0)
                        .min_w_0()
                        .text_color(color)
                        .truncate()
                        .child(suffix)
                        .into_any_element()
                })
            })
    }

    /// A context-tray trigger (the new-session destination and checkout
    /// chips on the tab behind the composer): `h-7 px-2.5 text-[12px]`, a
    /// 14px icon and a faint chevron, foreground text so the tray reads as
    /// the chat's context rather than as footnotes.
    fn tray_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        // Per-picker hover key, like the trigger chips.
        let id: SharedString = format!("{id}-{}", cx.entity_id()).into();
        div()
            .relative()
            .child(self.measure_trigger(kind, cx))
            .id(id.clone())
            .border_1()
            .border_color(gpui::transparent_black())
            .h(px(28.0))
            .max_w(px(224.0))
            .min_w(px(60.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded_full()
            .text_size(px(11.))
            .line_height(px(14.85))
            .font_weight(gpui::FontWeight::NORMAL)
            .text_color(motion::hover_blend(&id, theme.text, theme.text_strong))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(&id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id.clone()))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(if icon_path == crate::icons::DITHER_CLOUD {
                        16.
                    } else {
                        14.
                    }))
                    .flex_none()
                    .text_color(theme.text),
            )
            .child(roll_text(format!("{id}-label"), label, cx.reduce_motion()))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.6)),
            )
    }
    /// New-session destination controls: the project, then the device it
    /// runs on, as a chip-only cluster floating above the composer's trailing
    /// edge.
    pub fn render_new_thread_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = (self.open.closing_since(), self.menu_geometry().below);
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            Some(PickerKind::Device) => {
                let content = self.render_device_popover(cx);
                Some((PickerKind::Device, self.popover_frame(280.0, content, cx)))
            }
            _ => None,
        };
        let (mut device_label, mut project_label, mut offline) = {
            let state = self.state.read(cx);
            let device_id = state.effective_device_id();
            let device_label: SharedString = device_id
                .as_deref()
                .and_then(|id| state.device_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| "This device".to_string())
                .into();
            let offline = device_id
                .as_deref()
                .is_some_and(|id| !state.device_online(id, chrono::Utc::now()));
            let project_label: SharedString = state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No project".to_string())
                .into();
            (device_label, project_label, offline)
        };
        if self.state.read(cx).effective_device_id() == self.state.read(cx).local_device_id {
            device_label = "This computer".into();
        }
        if let Some(project) = &self.state.read(cx).cloud_project {
            device_label = SharedString::from("Cloud sandbox · boxd");
            project_label = SharedString::from(
                project["displayName"]
                    .as_str()
                    .unwrap_or("Repository")
                    .to_owned(),
            );
            offline = false;
        }
        let device_chip = self
            .tray_chip(
                PickerKind::Device,
                "picker-device",
                if self.state.read(cx).cloud_project.is_some() {
                    crate::icons::DITHER_CLOUD
                } else {
                    crate::icons::DITHER_MONITOR
                },
                device_label,
                &theme,
                cx,
            )
            .when(offline, |el| el.text_color(theme.warning.opacity(0.8)));
        let project_chip = self.tray_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::DITHER_FOLDER,
            project_label,
            &theme,
            cx,
        );
        div()
            .min_w_0()
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(attach_creation_overlay(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .child(attach_creation_overlay(
                device_chip,
                &mut overlay,
                PickerKind::Device,
                "device-popover",
                closing,
            ))
            .children(self.render_creation_workspace(cx))
            .children(self.render_import_chat(cx))
            .child(div().flex_1())
            .children(self.render_new_thread_git_selectors(cx))
            .into_any_element()
    }

    /// Prepare a source checkout or issue context before the first send.
    pub fn render_new_thread_git_selectors(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.cloud_launch_frozen() {
            return None;
        }
        let git = self
            .state
            .read(cx)
            .selected_space_row()
            .is_some_and(|space| space.git_detected);
        if !git {
            return None;
        }
        self.ensure_refs(false, cx);
        let target = self.creation_source_target(cx)?;
        self.source_picker
            .update(cx, |picker, cx| picker.set_target(Some(target), cx));
        Some(self.source_picker.clone().into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        popover::popover_card(&theme)
            .w(px(width))
            .when(self.mounted_kind() == Some(PickerKind::Checkout), |v| {
                v.p(px(8.)).border_0().rounded(px(10.)).gap(px(0.))
            })
            .when(
                matches!(
                    self.mounted_kind(),
                    Some(PickerKind::Device | PickerKind::Space)
                ),
                |v| v.p(px(7.)).gap(px(0.)).rounded(px(10.)).text_size(px(12.)),
            )
            // zuse caps its tallest picker at min(640px, 75vh).
            .max_h(px(self.menu_geometry().height))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.is_open() && !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .on_mouse_down_out(
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.setting_menu.is_some()
                        && this
                            .setting_bounds
                            .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    let workspace = this.mounted_kind() == Some(PickerKind::Checkout);
                    let previous = window.focused(cx);
                    this.dismiss(cx);
                    if workspace {
                        let entity = cx.entity().downgrade();
                        window.defer(cx, move |window, cx| {
                            if window.focused(cx) == previous {
                                let _ = entity
                                    .update(cx, |_, cx| cx.emit(ReturnComposerFocus::default()));
                            }
                        });
                    } else if this.focus.contains_focused(window, cx) {
                        window.blur();
                    }
                }),
            )
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }
    /// [`Self::popover_frame`] without the p-1 inset — the harness/model
    /// picker's rail + list panes bleed to the card edge (zuse
    /// harness-model-picker.tsx `className="w-80 p-0"`).
    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .h(px(self.menu_geometry().height.min(self.open_model_height)))
            .when(self.compact_model_picker(cx), |el| {
                el.h(px(self.compact_motion.frame_height))
            })
            .when(self.is_settings_model(), |el| {
                el.on_action(
                    cx.listener(|this, jump: &crate::shell::JumpSession, _, cx| {
                        if this.jump_model_slot(jump.0, cx) {
                            cx.stop_propagation();
                        }
                    }),
                )
            })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.is_open() && !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .on_mouse_down_out(
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.setting_menu.is_some()
                        && this
                            .setting_bounds
                            .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        return;
                    }
                    let previous = window.focused(cx);
                    this.dismiss(cx);
                    cx.emit(ReturnComposerFocus {
                        pointer: Some(event.position),
                    });
                    let entity = cx.entity().downgrade();
                    let menu_focus = this.focus.clone();
                    let settings_trigger = this
                        .is_settings_model()
                        .then(|| this.settings_model.trigger.clone());
                    window.defer(cx, move |window, cx| {
                        let empty = window.focused(cx).is_none();
                        let unchanged = window.focused(cx) == previous;
                        let retained = menu_focus.contains_focused(window, cx);
                        // The search field and focusable rows can hand focus to
                        // each other as the menu closes. Keep a clicked control's
                        // focus, and return any focus left in the menu to the composer.
                        if empty || unchanged || retained {
                            if let Some(trigger) = settings_trigger {
                                window.focus(&trigger, cx);
                            } else {
                                let _ = entity
                                    .update(cx, |_, cx| cx.emit(ReturnComposerFocus::default()));
                            }
                        }
                    });
                }),
            )
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel => {
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.model_refresh_errors.clear();
                            this.catalog_rev += 1;
                            this.ensure_harnesses(false, cx);
                        }
                        // Projects/devices load nothing; no retry surface exists.
                        PickerKind::Space | PickerKind::Device => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The virtualized list's plain scroll handle (bounds/offset for the
    /// floating scrollbar; `UniformList` tracks it internally).
    fn model_scroll_base(&self) -> gpui::ScrollHandle {
        self.model_scroll.0.borrow().base_handle.clone()
    }

    /// The scroll handle of whichever picker menu is mounted. The popups are
    /// mutually exclusive: the model list owns its virtualized handle, the
    /// plain-div menus (branch / project / device) share `menu_scroll`.
    /// Keys on the MOUNTED menu, not `open_kind` — popovers keep rendering
    /// through the exit animation, and the rail must keep measuring the
    /// closing menu's own handle, not `menu_scroll`'s idle geometry.
    fn active_menu_scroll(&self) -> gpui::ScrollHandle {
        if self.mounted_kind() == Some(PickerKind::HarnessModel)
            && (self.model_rail != ModelRail::All || self.compact_model_list)
        {
            self.model_scroll_base()
        } else {
            self.menu_scroll.clone()
        }
    }

    /// The list-hover half of the rail treatment; the strip's own hover,
    /// press, drag, and mouse-up listeners come from [`popover::rail`].
    fn on_menu_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu_bar.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    /// Model picker with a provider rail beside a scoped search.
    /// Existing chats show only their own harness tab and models.
    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let height = self.menu_geometry().height.min(self.open_model_height);
        let (list_height, tray_height) = model_menu_budgets(height, self.tray_group_count(cx));

        let theme = Theme::of(cx).for_popup();

        // Catalog-level loading/error take over the whole card — the tabs ARE
        // the catalog, so there is nothing stable to draw above the skeleton.
        match &self.harnesses {
            Loadable::Loading | Loadable::Idle => {
                return div()
                    .h(px(list_height))
                    .p(px(8.0))
                    .child(popover::skeleton_menu_rows(
                        "harness-skeleton",
                        &theme,
                        5,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Error(message) => {
                let message = message.clone();
                return div()
                    .h(px(list_height))
                    .p(px(8.0))
                    .child(self.retry_row(
                        "harness-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Ready(_) => {}
        }

        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        let all_view = self.model_rail == ModelRail::All;
        let descriptors = self.rail_descriptors(cx);
        // No-agents empty state: the catalog loaded but offers nothing
        // runnable (every enabled harness is missing its CLI, or nothing is
        // enabled) and there's no committed chat harness to force-include —
        // guidance instead of an empty tab row.
        if descriptors.is_empty() {
            return div()
                .p(px(16.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::TERMINAL)
                        .size(px(20.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::text_sm())
                        .text_color(theme.text)
                        .child(SharedString::from("No agents available")),
                )
                .child(
                    div()
                        .text_size(crate::typography::text_xs())
                        .text_color(theme.text_muted)
                        .text_center()
                        .child(SharedString::from(
                            "Enable an installed agent in Settings → Providers, \
                             or install an agent CLI.",
                        )),
                )
                .into_any_element();
        }
        let rows = self.model_rows(cx);

        // Provider rail stays beside the searchable model list.
        let mut tabs = div()
            .flex_none()
            .w(px(44.0))
            .p(px(6.0))
            .border_r_1()
            .border_color(theme.border.opacity(0.5))
            .bg(theme.muted_fill().opacity(0.2))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0));
        tabs = tabs.child(
            div()
                .id("model-tab-favorites")
                .relative()
                .w(px(32.0))
                .h(px(32.0))
                .flex_none()
                .rounded(px(MODEL_ROW_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(all_view, |el| model_selected(el, &theme))
                .when(!all_view, |el| {
                    el.hover(|s| s.bg(theme.muted_fill().opacity(0.6)))
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.setting_menu = None;
                    this.model_rail = ModelRail::All;
                    // Anchor on the selected row when it's starred, else
                    // the top — never a stray second highlight.
                    this.active = this.selected_model_index(cx);
                    this.model_scroll_base().set_offset(gpui::Point::default());
                    this.scroll_to_model_row(this.active, gpui::ScrollStrategy::Nearest, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if all_view {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(tr(cx, msg::settings::pokedex_pane_all)),
                ),
        );
        for (ix, descriptor) in descriptors.iter().enumerate() {
            let harness = descriptor.id;
            let is_viewed = !all_view && effective == Some(harness);
            let live = descriptor.installed && descriptor.enabled != Some(false);
            let (icon_path, tint) = harness_brand_icon(harness);
            tabs = tabs.child(
                div()
                    .id(("harness-tab", ix))
                    .relative()
                    .w(px(32.0))
                    .h(px(32.0))
                    .flex_none()
                    .rounded(px(MODEL_ROW_RADIUS))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(is_viewed, |el| model_selected(el, &theme))
                    .when(!is_viewed, |el| {
                        el.hover(|s| s.bg(theme.muted_fill().opacity(0.6)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.setting_menu = None;
                        this.model_rail = ModelRail::Harness;
                        this.pick_harness(harness, cx);
                        cx.notify();
                    }))
                    .child(crate::icons::brand_icon(
                        icon_path,
                        14.0,
                        tint.unwrap_or(if is_viewed {
                            theme.text
                        } else {
                            theme.text_muted
                        }),
                    ))
                    // Live: the provider's CLI is installed and enabled.
                    .when(live, |el| {
                        el.child(
                            div()
                                .absolute()
                                .right(px(4.0))
                                .bottom(px(4.0))
                                .size(px(6.0))
                                .rounded_full()
                                .bg(theme.accent),
                        )
                    }),
            );
        }

        // ── search row: icon + borderless input over a full-bleed hairline.
        //    The placeholder names the scope — the query never leaves the
        //    viewed tab (user request; the old global search hid the rail).
        let search_row = div()
            .flex_none()
            .p(px(10.0))
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .child(
                div()
                    .h(px(32.0))
                    .px(px(10.0))
                    .rounded(px(MODEL_ROW_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.bg)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        crate::icons::icon(crate::icons::MAGNIFER)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(crate::typography::text_xs())
                            .child(self.search.clone()),
                    ),
            );

        // ── model rows: a VIRTUALIZED uniform list — only the visible slice
        //    renders, so a 7k-model catalog scrolls as smoothly as seven
        //    (field report: the un-virtualized stack was the picker's lag).
        //    Keyboard nav scrolls via the UniformListScrollHandle.
        let effective_models = effective.and_then(|h| self.model_catalog(cx).get(&h));
        // The compact list interleaves its section label and provider
        // headings with the rows (`compact::CompactItem`).
        let items = self
            .compact_model_picker(cx)
            .then(|| std::sync::Arc::new(self.compact_items(cx)));
        let model_list: Option<AnyElement> = if !rows.is_empty() {
            let entity = cx.entity();
            let row_data = rows.clone();
            let count = items.as_ref().map_or(rows.len(), |items| items.len());
            Some(
                gpui::uniform_list("model-menu-scroll", count, move |range, _window, app| {
                    entity.update(app, |this, cx| {
                        range
                            .filter_map(|ix| {
                                let row = match items.as_ref().map(|items| &items[ix]) {
                                    None => ix,
                                    Some(compact::CompactItem::Row(row)) => *row,
                                    Some(item) => {
                                        return Some(this.render_compact_item(ix, item, cx));
                                    }
                                };
                                row_data
                                    .get(row)
                                    .map(|data| this.render_model_row(row, data, cx))
                            })
                            .collect::<Vec<AnyElement>>()
                    })
                })
                .size_full()
                .px(px(popover::CARD_INSET))
                .track_scroll(&model_scroll)
                .into_any_element(),
            )
        } else {
            None
        };
        let list_children: Vec<AnyElement> = if !rows.is_empty() {
            Vec::new()
        } else if searching {
            vec![empty_list_note(&theme, "No models found")]
        } else if all_view {
            vec![empty_list_note(
                &theme,
                "No models available for this agent.",
            )]
        } else {
            match effective_models {
                Some(Loadable::Error(message)) => {
                    let message = model_catalog_notice(message);
                    vec![self.retry_row(
                        "model-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    )]
                }
                _ => vec![popover::skeleton_menu_rows(
                    "model-skeleton",
                    &theme,
                    5,
                    cx.entity_id(),
                    cx,
                )],
            }
        };

        let refresh_error = effective
            .and_then(|harness| self.model_refresh_errors.get(&harness))
            .filter(|_| !rows.is_empty())
            .cloned()
            .map(|message| {
                self.retry_row(
                    "model-refresh-retry",
                    &model_catalog_notice(&message),
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                )
            });

        let model_scrollbar = popover::rail(self, "model-scrollbar", &theme, cx);
        let list_host = div()
            .id("model-list-scroll-host")
            .relative()
            .flex_none()
            .h(px(list_height))
            .py(px(popover::CARD_INSET))
            .on_hover(cx.listener(Self::on_menu_list_hover))
            .child(match model_list {
                Some(list) => {
                    popover::faded_menu_list(&self.model_scroll_base(), list).into_any_element()
                }
                // Empty/loading/error notes: a plain static stack.
                None => div()
                    .id("model-menu-scroll")
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(popover::CARD_INSET))
                    .children(list_children)
                    .into_any_element(),
            })
            // Absolute child: the hit rail and thumb float above the
            // scroll content without consuming any list width.
            .children(model_scrollbar);

        // ── traits tray: the reasoning ladder + model options PINNED under
        //    the list (the separate Traits popover folded in here — user
        //    request). Hidden entirely when the selected model has neither.
        // The effort slider carries reasoning; with nothing else to set, no
        // empty tray band sits above it.
        let has_tray = self.title.is_none() && self.tray_group_count(cx) > 0;
        let tray: Option<AnyElement> = has_tray.then(|| {
            let sections = self.render_traits_sections(cx);
            div()
                .id("model-traits-tray")
                .flex_none()
                .border_t_1()
                .border_color(crate::theme::hairline(0.08))
                // Long option stacks scroll inside the tray rather than
                // growing the card past the viewport.
                .h(px(tray_height))
                .overflow_y_scroll()
                .px(px(popover::CARD_INSET))
                .child(sections)
                .into_any_element()
        });

        let effort_slider = self.render_effort_slider(cx);
        div()
            .flex()
            .flex_row()
            .min_w_0()
            .child(tabs)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(search_row)
                    .children(refresh_error)
                    .child(list_host)
                    .children(tray)
                    .children(effort_slider),
            )
            .into_any_element()
    }

    /// One model row for the virtualized list. `ix` is the row's GLOBAL index
    /// (⌘N chips, hover-cursor, and activation all key on it). The 2px
    /// inter-row gap is baked into each item's bottom padding so every item
    /// is the same height (uniform_list measures the first).
    fn render_model_row(
        &mut self,
        ix: usize,
        row: &ModelRowData,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.compact_model_picker(cx) {
            return self.render_compact_model_row(ix, row, cx);
        }
        let theme = Theme::of(cx).for_popup();
        let effective = self.effective_harness(cx);
        let is_selected = Some(row.harness) == effective
            && self
                .effective_model_id(cx)
                .or_else(|| self.selected_model(cx).map(|m| m.id.as_str()))
                == Some(row.model.id.as_str());
        let is_active = ix == self.active;
        let is_fav = self.defaults.is_favorite(row.harness, &row.model.id);
        let (icon_path, tint) = harness_brand_icon(row.harness);
        let label: SharedString = row.model.label.clone().into();
        let harness_name = row.harness_name.clone();
        let harness = row.harness;
        let star_model = row.model.id.clone();
        // Rows show only the model name. The description appears only to
        // tell identically-named rows apart (field report: "GLM-5.2" exists
        // under 64 connected opencode providers, whose driver ships the
        // provider name in `description`).
        let attribution: Option<SharedString> = row
            .model
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| {
                row.ambiguous && !d.is_empty() && !d.eq_ignore_ascii_case(harness_name.as_ref())
            })
            .map(|d| SharedString::from(d.to_owned()));
        let compact = self.model_rail == ModelRail::Harness;
        let mut el = div()
            .id(("model-row", ix))
            .min_h(px(32.0))
            .px(px(8.0))
            .py(px(if compact { 4.0 } else { 6.0 }))
            .rounded(px(MODEL_ROW_RADIUS))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer();
        // ONE moving highlight (combobox-style): hovering moves the
        // keyboard cursor instead of painting its own wash, so hover + arrow
        // cursor can never wear two washes at once. Selection is the
        // distinct stronger treatment (wash + ring).
        if is_selected {
            el = model_selected(el, &theme);
        } else if is_active {
            el = el.bg(theme.muted_fill().opacity(0.6));
        }
        el = el.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.active != ix {
                this.active = ix;
                cx.notify();
            }
        }));
        // Compact single-line rows on a harness tab (user request): every
        // row there shares the tab's harness, so the identity subline is
        // dead weight — attribution rides inline instead (opencode ships
        // identically-named models under 64 providers; it must stay
        // visible). The favorites tab mixes harnesses and keeps the
        // two-line layout with the brand subline.
        let body: AnyElement = if compact {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .max_w_full()
                        .truncate()
                        .text_size(crate::typography::text_xs())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_strong)
                        .child(label),
                )
                .when_some(attribution, |el, attribution| {
                    el.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::text_xs())
                            .text_color(theme.text_muted)
                            .child(attribution),
                    )
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(crate::typography::text_xs())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_strong)
                        .child(label),
                )
                .child(
                    // Harness identity subline, plus
                    // the model's own attribution when it carries one.
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(crate::icons::brand_icon(
                            icon_path,
                            11.0,
                            tint.unwrap_or(theme.text_muted),
                        ))
                        .child(
                            div()
                                .flex_none()
                                .text_size(crate::typography::text_xs())
                                .text_color(theme.text_muted)
                                .child(harness_name),
                        )
                        .when_some(attribution, |el, attribution| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::text_xs())
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from("·")),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::text_xs())
                                    .text_color(theme.text_muted)
                                    .child(attribution),
                            )
                        }),
                )
                .into_any_element()
        };
        el = el
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_model_index(ix, cx);
            }))
            .child(body);
        if is_selected {
            el = el.child(model_check(&theme));
        }
        if ix < 9 {
            el = el.child(popover::kbd_hint(&theme, &format!("⌘{}", ix + 1)));
        }
        // A model configured in place opens its settings card on hover.
        // The title picker has no settings, so no card either.
        let configurable =
            self.title.is_none() && configured_in_place(&row.model) && !row.selected_only;
        let card_open = configurable && self.config_row == Some(ix);
        if configurable {
            let entity = cx.entity().downgrade();
            let exit_entity = cx.entity().downgrade();
            el = el.relative().child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        let left = bounds.right() + px(CONFIG_CARD_WIDTH + 12.0)
                            > window.viewport_size().width;
                        let _ = entity.update(cx, |this, cx| {
                            if this.config_on_left != left {
                                this.config_on_left = left;
                                cx.notify();
                            }
                        });
                    },
                    move |trigger, _, window, _| {
                        if !card_open {
                            return;
                        }
                        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Bubble {
                                return;
                            }
                            let _ = exit_entity.update(cx, |this, cx| {
                                if this.config_row != Some(ix) {
                                    return;
                                }
                                let pointer = event.position;
                                // An open card choice list — including the cone
                                // from its trigger row toward it — is inside the card.
                                let in_choices = this.setting_scope == SettingScope::Card
                                    && this.setting_menu.is_some()
                                    && this.setting_hover.contains_pointer(
                                        gpui::Bounds::default(),
                                        this.setting_bounds,
                                        pointer,
                                        this.setting_on_left,
                                    );
                                if this.config_bounds.is_some_and(|b| b.contains(&pointer)) {
                                    this.config_grace = None;
                                }
                                let in_grace =
                                    this.config_grace.is_some_and(|b| b.contains(&pointer));
                                if in_choices
                                    || in_grace
                                    || this.config_hover.contains_pointer(
                                        trigger,
                                        this.config_bounds,
                                        pointer,
                                        this.config_on_left,
                                    )
                                {
                                    return;
                                }
                                this.close_config();
                                cx.notify();
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            );
            if card_open {
                let card = self.render_config_card(cx);
                el = el.child(popover::nested_menu(
                    format!("model-config-{ix}"),
                    card,
                    self.config_on_left,
                ));
            }
        }
        el = el.child(
            div()
                .id(("model-star", ix))
                .flex_none()
                .w(px(22.0))
                .h(px(22.0))
                .rounded(px(popover::MENU_ITEM_RADIUS))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_model_favorite(harness, &star_model, cx);
                }))
                .child(
                    crate::icons::icon(if is_fav {
                        crate::icons::STAR_BOLD
                    } else {
                        crate::icons::STAR
                    })
                    .size(px(13.0))
                    .text_color(if is_fav {
                        theme.warning
                    } else {
                        theme.text_muted
                    }),
                ),
        );
        if !configurable {
            return div().pb(px(2.0)).child(el).into_any_element();
        }
        // Hover intent on a stable wrapper, as for the setting triggers.
        div()
            .id(("model-config-hover", ix))
            .pb(px(2.0))
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                if *hovered {
                    let pointer = window.mouse_position();
                    let action = this.config_hover.enter(
                        this.config_row.as_ref(),
                        &ix,
                        pointer,
                        this.config_bounds,
                        this.config_on_left,
                    );
                    this.apply_config_hover(action, ix, pointer, cx);
                } else {
                    this.config_hover.leave(&ix);
                }
            }))
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    let action = this.config_hover.moved(
                        this.config_row.as_ref(),
                        &ix,
                        event.position,
                        this.config_bounds,
                        this.config_on_left,
                    );
                    this.apply_config_hover(action, ix, event.position, cx);
                }),
            )
            .child(el)
            .into_any_element()
    }

    /// The hover card of a model configured in place: its setting triggers
    /// (each with nested choices) and its switches.
    fn render_config_card(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let Some(row) = self.card_row(cx) else {
            return div().into_any_element();
        };
        let settings = self.render_setting_rows(SettingScope::Card, cx);
        let toggles: Vec<AnyElement> = self
            .card_groups(cx)
            .into_iter()
            .filter(is_toggle)
            .enumerate()
            .map(|(i, group)| {
                let on = toggle_group_is_on(&group);
                let label = SharedString::from(group.label.clone());
                popover::menu_row_nav(&theme, false, false, format!("card-toggle-{i}"))
                    .id(("card-toggle", i))
                    .h(px(30.0))
                    .py(px(0.0))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_card_option(group.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .child(div().flex_1().child(label))
                    .child(crate::settings::widgets::toggle_switch(
                        &theme,
                        on,
                        format!("model-card-{i}"),
                    ))
                    .into_any_element()
            })
            .collect();
        let entity = cx.entity().downgrade();
        popover::popover_card(&theme)
            .w(px(CONFIG_CARD_WIDTH))
            .relative()
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .text_size(crate::typography::text_sm())
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_strong)
                    .child(SharedString::from(row.model.label.clone())),
            )
            .child(settings)
            .when(!toggles.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .pb(px(popover::CARD_INSET))
                        .children(toggles),
                )
            })
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        let _ = entity.update(cx, |this, _| this.config_bounds = Some(bounds));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    /// The tray's settings. A model configured in place keeps them in its
    /// hover card only — except in the compact picker, whose panel is the
    /// one place the selected model's settings live.
    fn setting_groups(&self, cx: &App) -> Vec<SettingGroup> {
        // Titles always run at minimal reasoning with no model options.
        if self.is_settings_model() || !self.model_configuration_available(cx) {
            return Vec::new();
        }
        if self.selected_model(cx).is_some_and(configured_in_place) {
            return if self.compact_model_picker(cx) || self.effort_menu_open {
                card_order(self.live_groups(cx))
            } else {
                Vec::new()
            };
        }
        self.live_groups(cx)
    }

    /// The selected model's settings with the current picks.
    fn live_groups(&self, cx: &App) -> Vec<SettingGroup> {
        build_setting_groups(
            self.trait_ladder(cx),
            self.effective_reasoning(cx),
            self.selected_model(cx),
            &self.explicit_options(cx),
        )
    }

    /// Settings the pinned tray lists: the effort slider owns reasoning (and
    /// an effort-shaped option), so those never take a tray row.
    fn tray_group_count(&self, cx: &App) -> usize {
        let hidden = self.compact_hidden_options(cx);
        self.setting_groups(cx)
            .iter()
            .filter(|group| Self::compact_option_visible(&group.id, &hidden))
            .count()
    }

    fn scope_groups(&self, scope: SettingScope, cx: &App) -> Vec<SettingGroup> {
        match scope {
            SettingScope::Tray => self.setting_groups(cx),
            SettingScope::Card => self.card_groups(cx),
        }
    }

    /// The row whose settings card is open, while it still lists a model
    /// configured in place.
    fn card_row(&self, cx: &App) -> Option<ModelRowData> {
        self.config_row
            .and_then(|ix| self.model_rows(cx).get(ix).cloned())
            .filter(|row| {
                self.title.is_none() && !row.selected_only && configured_in_place(&row.model)
            })
    }

    fn row_is_selected(&self, row: &ModelRowData, cx: &App) -> bool {
        Some(row.harness) == self.effective_harness(cx)
            && self
                .effective_model_id(cx)
                .or_else(|| self.selected_model(cx).map(|m| m.id.as_str()))
                == Some(row.model.id.as_str())
    }

    /// The card's settings: the live ones when its model is the selected
    /// one, otherwise what picking it would start from (the remembered
    /// options, the current effort clamped to its ladder).
    fn card_groups(&self, cx: &App) -> Vec<SettingGroup> {
        let Some(row) = self.card_row(cx) else {
            return Vec::new();
        };
        if self.row_is_selected(&row, cx) {
            return card_order(self.live_groups(cx));
        }
        let levels = if row.model.reasoning_levels.is_empty() {
            self.harnesses
                .ready()
                .and_then(|list| list.iter().find(|d| d.id == row.harness))
                .map(|d| d.reasoning_levels.clone())
                .unwrap_or_default()
        } else {
            row.model.reasoning_levels.clone()
        };
        let reasoning = clamp_reasoning(self.effective_reasoning(cx), &levels);
        let selections = offered_options(
            &row.model,
            self.defaults
                .model_options_for(row.harness, &row.model.id)
                .cloned()
                .unwrap_or_default(),
        );
        card_order(build_setting_groups(
            levels,
            reasoning,
            Some(&row.model),
            &selections,
        ))
    }

    /// A choice made in the card selects its model first.
    fn select_card_model(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.config_row
            && let Some(row) = self.card_row(cx)
            && !self.row_is_selected(&row, cx)
        {
            self.activate_model_index(ix, cx);
        }
    }

    fn open_config(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.config_hover.cancel();
        self.config_hover.reset();
        if self.setting_scope == SettingScope::Card {
            self.setting_menu = None;
            self.setting_bounds = None;
        }
        self.config_row = Some(ix);
        self.config_bounds = None;
        cx.notify();
    }

    fn close_config(&mut self) {
        self.config_hover.cancel();
        self.config_hover.reset();
        self.config_row = None;
        self.config_bounds = None;
        self.config_grace = None;
        if self.setting_scope == SettingScope::Card {
            self.setting_menu = None;
            self.setting_bounds = None;
        }
    }

    fn apply_config_hover(
        &mut self,
        action: popover::HoverAction,
        ix: usize,
        pointer: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        match action {
            popover::HoverAction::None => {}
            popover::HoverAction::Open => {
                self.open_config(ix, cx);
                self.config_hover.record_origin(pointer);
            }
            popover::HoverAction::Defer => {
                let source = self.config_row;
                self.config_hover.defer(cx, move |this, cx| {
                    if this.is_open()
                        && this.config_row == source
                        && this.config_hover.pending() == Some(&ix)
                    {
                        this.open_config(ix, cx);
                        this.config_hover.record_origin(pointer);
                    }
                });
            }
        }
    }

    fn cancel_setting_hover(&mut self) {
        self.setting_hover.cancel();
    }

    fn hover_setting(
        &mut self,
        scope: SettingScope,
        id: ModelSetting,
        index: usize,
        pointer: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .setting_menu
            .as_ref()
            .filter(|_| self.setting_scope == scope);
        let action = self.setting_hover.enter(
            current,
            &id,
            pointer,
            self.setting_bounds,
            self.setting_on_left,
        );
        self.apply_setting_hover(scope, action, id, index, pointer, cx);
    }

    fn apply_setting_hover(
        &mut self,
        scope: SettingScope,
        action: popover::HoverAction,
        id: ModelSetting,
        index: usize,
        pointer: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        match action {
            popover::HoverAction::None => {}
            popover::HoverAction::Open => {
                if scope == SettingScope::Tray {
                    self.active = index;
                }
                self.open_setting(scope, id, cx);
                self.setting_hover.record_origin(pointer);
            }
            popover::HoverAction::Defer => {
                let source = self.setting_menu.clone();
                self.setting_hover.defer(cx, move |this, cx| {
                    if this.is_open()
                        && this.setting_menu == source
                        && this.setting_hover.pending() == Some(&id)
                    {
                        if scope == SettingScope::Tray {
                            this.active = index;
                        }
                        this.open_setting(scope, id, cx);
                        this.setting_hover.record_origin(pointer);
                    }
                });
            }
        }
    }

    fn open_setting(&mut self, scope: SettingScope, id: ModelSetting, cx: &mut Context<Self>) {
        self.cancel_setting_hover();
        self.setting_hover.reset();
        self.setting_active = self
            .scope_groups(scope, cx)
            .iter()
            .find(|g| g.id == id)
            .and_then(|g| g.choices.iter().position(|c| c.selected))
            .unwrap_or(0);
        self.setting_scope = scope;
        self.setting_menu = Some(id);
        self.setting_bounds = None;
        self.setting_scroll = gpui::ScrollHandle::new();
        self.setting_scroll.scroll_to_item(self.setting_active);
        cx.notify();
    }

    fn activate_setting_choice(&mut self, cx: &mut Context<Self>) {
        let scope = self.setting_scope;
        let Some(group) = self
            .scope_groups(scope, cx)
            .into_iter()
            .find(|g| Some(&g.id) == self.setting_menu.as_ref())
        else {
            return;
        };
        let Some(choice) = group.choices.get(self.setting_active) else {
            return;
        };
        if scope == SettingScope::Card {
            // Keep the card: the pointer rests where this list was.
            self.config_grace = self.setting_bounds;
            self.select_card_model(cx);
        }
        match group.id {
            ModelSetting::Reasoning => {
                if let Some(level) = choice.reasoning {
                    self.pick_reasoning(level, cx);
                }
            }
            ModelSetting::Option(id) => {
                self.pick_option(id, choice.value.clone(), choice.default, cx)
            }
        }
        self.setting_menu = None;
        self.setting_bounds = None;
        cx.notify();
    }

    /// Flip a card switch (Fast Mode): selects the card's model first.
    fn toggle_card_option(&mut self, group: SettingGroup, cx: &mut Context<Self>) {
        let ModelSetting::Option(id) = group.id else {
            return;
        };
        let Some(next) = group.choices.into_iter().find(|c| !c.selected) else {
            return;
        };
        self.select_card_model(cx);
        self.pick_option(id, next.value, next.default, cx);
    }

    fn toggle_tray_option(&mut self, group: SettingGroup, cx: &mut Context<Self>) {
        let ModelSetting::Option(id) = group.id else {
            return;
        };
        let Some(next) = group.choices.into_iter().find(|choice| !choice.selected) else {
            return;
        };
        self.cancel_setting_hover();
        self.setting_menu = None;
        self.setting_bounds = None;
        self.pick_option(id, next.value, next.default, cx);
        cx.notify();
    }

    /// Each model setting gets a compact trigger and its own nested choices.
    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.render_setting_rows(SettingScope::Tray, cx)
    }

    /// Setting triggers for the tray or a hover card. Card rows stay out of
    /// the keyboard cursor, and its switches render separately.
    fn render_setting_rows(&mut self, scope: SettingScope, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let tray = scope == SettingScope::Tray;
        let base_index = if tray {
            self.model_rows_len(cx)
        } else {
            NO_ACTIVE_ROW / 2
        };
        let (row_id, hover_id, choice_id, key) = if tray {
            (
                "model-setting",
                "model-setting-hover",
                "setting-choice",
                "model",
            )
        } else {
            (
                "card-setting",
                "card-setting-hover",
                "card-setting-choice",
                "card",
            )
        };
        let mut rows = Vec::new();
        let groups = self.scope_groups(scope, cx).into_iter().filter(|g| {
            (tray || !is_toggle(g))
                && (!tray || Self::compact_option_visible(&g.id, &self.compact_hidden_options(cx)))
        });
        for (ix, group) in groups.enumerate() {
            let mode_icon = match &group.id {
                ModelSetting::Option(id) if id.to_ascii_lowercase().contains("context") => {
                    crate::icons::EXPAND_ARROWS
                }
                _ => crate::icons::TUNING,
            };
            if tray
                && self.compact_model_picker(cx)
                && !Self::compact_option_visible(&group.id, &self.compact_hidden_options(cx))
            {
                continue;
            }
            if tray && is_toggle(&group) {
                let on = toggle_group_is_on(&group);
                let label = SharedString::from(group.label.clone());
                let value = group
                    .choices
                    .iter()
                    .find(|choice| choice.selected)
                    .map(|choice| choice.label.clone())
                    .unwrap_or_default();
                let key_group = group.clone();
                rows.push(
                    popover::menu_row_nav(
                        &theme,
                        false,
                        self.active == base_index + ix,
                        format!("model-toggle-{ix}"),
                    )
                    .id(("model-toggle", ix))
                    .role(gpui::Role::Switch)
                    .aria_label(label.clone())
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .tab_index(0)
                    .h(px(30.))
                    .py(px(0.))
                    .gap(px(8.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = base_index + ix;
                        this.toggle_tray_option(group.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                        if !event.is_held
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.toggle_tray_option(key_group.clone(), cx);
                            cx.stop_propagation();
                        }
                    }))
                    .child(
                        crate::icons::icon(mode_icon)
                            .size(px(14.))
                            .text_color(theme.text_muted),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_muted)
                            .child(SharedString::from(value)),
                    )
                    .child(crate::settings::widgets::toggle_switch(
                        &theme,
                        on,
                        format!("model-tray-{ix}"),
                    ))
                    .into_any_element(),
                );
                continue;
            }
            let open = self.setting_scope == scope && self.setting_menu.as_ref() == Some(&group.id);
            let value = group
                .choices
                .iter()
                .find(|c| c.selected)
                .map(|c| c.label.clone())
                .unwrap_or_default();
            let id = group.id.clone();
            let click_id = id.clone();
            let outside_id = id.clone();
            let move_id = id.clone();
            let exit_id = id.clone();
            let exit_entity = cx.entity().downgrade();
            let entity = cx.entity().downgrade();
            let mut row = popover::menu_row(
                &theme,
                open || (tray && self.active == base_index + ix),
                format!("{key}-setting-{ix}"),
            )
            .id((row_id, ix))
            .relative()
            .h(px(if self.compact_model_picker(cx) {
                26.0
            } else {
                30.0
            }))
            .py(px(0.0))
            .when(tray, |row| {
                row.h(px(30.))
                    .gap(px(8.))
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .child(
                        crate::icons::icon(mode_icon)
                            .size(px(14.))
                            .text_color(theme.text_muted),
                    )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if tray {
                    this.active = base_index + ix;
                }
                if this.effort_menu_open && this.setting_menu.as_ref() != Some(&click_id) {
                    this.open_setting(scope, click_id.clone(), cx);
                    cx.stop_propagation();
                    return;
                }
                this.cancel_setting_hover();
                this.setting_menu = None;
                this.setting_bounds = None;
                window.focus(&this.focus, cx);
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_down_out(
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    // The trigger dismisses its own child. Elsewhere in the parent,
                    // close this child during capture and let that control receive
                    // the same click. Clicks inside the floating child stay local.
                    if this.setting_scope == scope
                        && this.setting_menu.as_ref() == Some(&outside_id)
                        && !this
                            .setting_bounds
                            .is_some_and(|bounds| bounds.contains(&event.position))
                    {
                        this.cancel_setting_hover();
                        this.setting_menu = None;
                        this.setting_bounds = None;
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        let left = bounds.right() + px(244.0) > window.viewport_size().width;
                        let _ = entity.update(cx, |this, cx| {
                            if this.setting_on_left != left {
                                this.setting_on_left = left;
                                cx.notify();
                            }
                        });
                    },
                    move |trigger, _, window, _| {
                        if !open {
                            return;
                        }
                        window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                            if phase != gpui::DispatchPhase::Bubble {
                                return;
                            }
                            let _ = exit_entity.update(cx, |this, cx| {
                                if this.setting_scope != scope
                                    || this.setting_menu.as_ref() != Some(&exit_id)
                                {
                                    return;
                                }
                                if this.setting_hover.contains_pointer(
                                    trigger,
                                    this.setting_bounds,
                                    event.position,
                                    this.setting_on_left,
                                ) {
                                    return;
                                }
                                this.cancel_setting_hover();
                                this.setting_menu = None;
                                this.setting_bounds = None;
                                this.setting_hover.reset();
                                // Do not leave a keyboard-style selection on the
                                // trigger after pointer navigation dismisses it.
                                if tray {
                                    this.active = 0;
                                }
                                cx.notify();
                            });
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(group.label.clone())),
            )
            .child(
                div()
                    .max_w(px(100.0))
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(value)),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                    .size(px(12.0))
                    .text_color(theme.text_muted),
            );
            if open {
                let entity = cx.entity().downgrade();
                let menu = popover::popover_card(&theme)
                    .w(px(232.0))
                    .relative()
                    .child(
                        div()
                            .id("model-setting-choices")
                            .max_h(px(240.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.setting_scroll)
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .children(group.choices.into_iter().enumerate().map(
                                |(choice_ix, choice)| {
                                    popover::menu_row(
                                        &theme,
                                        choice_ix == self.setting_active,
                                        format!("{key}-setting-choice-{ix}-{choice_ix}"),
                                    )
                                    .id((choice_id, choice_ix))
                                    .h(px(30.0))
                                    .py(px(0.0))
                                    .flex_none()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.setting_active = choice_ix;
                                        this.activate_setting_choice(cx);
                                        cx.stop_propagation();
                                    }))
                                    .child(SharedString::from(choice.label))
                                    .child(div().flex_1())
                                    .when(choice.default, |el| el.child(default_badge(&theme)))
                                    .when(
                                        choice.selected,
                                        |el| {
                                            el.child(
                                                crate::icons::icon(crate::icons::CHECK)
                                                    .size(px(14.0))
                                                    .text_color(theme.text),
                                            )
                                        },
                                    )
                                },
                            )),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let _ =
                                    entity.update(cx, |this, _| this.setting_bounds = Some(bounds));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    );
                row = row.child(popover::nested_menu(
                    format!("{key}-setting-menu-{ix}"),
                    menu.into_any_element(),
                    self.setting_on_left,
                ));
            }
            // Keep hover ownership on a stable wrapper: menu_row already
            // owns its hover animation, and changing the open row's styling
            // must not reopen a child just dismissed by clicking its trigger.
            rows.push(
                div()
                    .id((hover_id, ix))
                    .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                        if *hovered {
                            this.hover_setting(
                                scope,
                                id.clone(),
                                base_index + ix,
                                window.mouse_position(),
                                cx,
                            );
                        } else {
                            this.setting_hover.leave(&id);
                        }
                    }))
                    .on_mouse_move(
                        cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                            let current = this
                                .setting_menu
                                .as_ref()
                                .filter(|_| this.setting_scope == scope);
                            let action = this.setting_hover.moved(
                                current,
                                &move_id,
                                event.position,
                                this.setting_bounds,
                                this.setting_on_left,
                            );
                            this.apply_setting_hover(
                                scope,
                                action,
                                move_id.clone(),
                                base_index + ix,
                                event.position,
                                cx,
                            );
                        }),
                    )
                    .child(row)
                    .into_any_element(),
            );
        }
        // The compact panel's own inset already closes the card below the
        // last row; a second one left the bottom twice as deep as the sides.
        let compact = tray && self.compact_model_picker(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pt(px(popover::CARD_INSET))
            .when(!compact, |el| el.pb(px(popover::CARD_INSET)))
            .children(rows)
            .into_any_element()
    }
}

/// Readable interaction state for the isolated compact-picker fixture.
#[cfg(feature = "compact-picker-fixture")]
pub struct CompactPickerFixtureState {
    pub model_list: bool,
    pub explicit_reasoning: Option<ReasoningLevel>,
    pub reasoning: Option<ReasoningLevel>,
    pub fast: bool,
    pub focused_model: Option<String>,
    pub focused_model_favorite: bool,
}

/// Synthetic catalog and assertions for the isolated compact-picker fixture.
/// None of this is linked into production builds.
#[cfg(feature = "compact-picker-fixture")]
impl Pickers {
    pub fn fixture_compact_catalog(&mut self, cx: &mut Context<Self>) {
        self.config.harness = Some(HarnessId::Codex);
        self.config.model = Some("gpt-5.4".into());
        self.config.reasoning = None;
        self.defaults
            .model_options_mut(HarnessId::Codex, "gpt-5.4")
            .clear();
        self.harnesses = Loadable::Ready(
            serde_json::from_value(serde_json::json!([
                {
                    "id":"codex",
                    "name":"Codex",
                    "installed":true,
                    "enabled":true,
                    "supportsSteering":true,
                    "steeringMode":"step-boundary",
                    "reasoningLevels":[]
                }
            ]))
            .unwrap(),
        );
        self.models.insert(
            HarnessId::Codex,
            Loadable::Ready(
                serde_json::from_value(serde_json::json!([
                    {
                        "id":"gpt-5.4",
                        "label":"GPT-5.4",
                        "description":"Best for complex coding and reasoning",
                        "reasoningLevels":["low","medium","high","xhigh"],
                        "options":[
                            {
                                "id":"serviceTier",
                                "label":"Service tier",
                                "defaultChoice":"standard",
                                "choices":[
                                    {"id":"standard","label":"Standard"},
                                    {"id":"fast","label":"Fast"}
                                ]
                            },
                            {
                                "id":"contextWindow",
                                "label":"Context window",
                                "defaultChoice":"standard",
                                "choices":[
                                    {"id":"standard","label":"Standard"},
                                    {"id":"1m","label":"1M tokens"}
                                ]
                            }
                        ]
                    },
                    {
                        "id":"gpt-5.3-codex",
                        "label":"GPT-5.3 Codex",
                        "description":"Optimized for agentic coding"
                    },
                    {
                        "id":"gpt-5.2",
                        "label":"GPT-5.2",
                        "description":"General purpose reasoning"
                    },
                    {
                        "id":"gpt-5.1-codex-mini",
                        "label":"GPT-5.1 Codex Mini",
                        "description":"Fast, efficient coding"
                    }
                ]))
                .unwrap(),
            ),
        );
        self.compact_model_list = false;
        self.compact_keyboard = false;
        self.active = 0;
        self.catalog_rev += 1;
        cx.notify();
    }

    pub fn fixture_compact_state(&self, cx: &App) -> CompactPickerFixtureState {
        let focused = self
            .compact_model_list
            .then(|| self.model_rows(cx).get(self.active).cloned())
            .flatten();
        CompactPickerFixtureState {
            model_list: self.compact_model_list,
            explicit_reasoning: self.config.reasoning,
            reasoning: self.effective_reasoning(cx),
            fast: self
                .compact_fast_choice(cx)
                .is_some_and(|(_, _, _, fast)| fast),
            focused_model_favorite: focused
                .as_ref()
                .is_some_and(|row| self.defaults.is_favorite(row.harness, &row.model.id)),
            focused_model: focused.map(|row| row.model.id),
        }
    }
}

/// The floating-scrollbar treatment for every picker list
/// ([`popover::rail`] folds the note/hide/metrics/render + pointer listeners
/// into one call): one shared rail state, fed by whichever handle
/// [`Pickers::active_menu_scroll`] resolves for the mounted menu.
impl popover::ScrollRailHost for Pickers {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        &mut self.menu_bar
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        Some(self.active_menu_scroll())
    }
}

/// The "Default" marker beside a section's default choice: a ghost badge —
/// bare muted text, no border or fill (user request).
/// Reasoning plus each offered option, with the current picks marked.
fn build_setting_groups(
    levels: Vec<ReasoningLevel>,
    reasoning: Option<ReasoningLevel>,
    model: Option<&Model>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Vec<SettingGroup> {
    let mut groups = Vec::new();
    if !levels.is_empty() {
        let default = default_reasoning(&levels);
        groups.push(SettingGroup {
            id: ModelSetting::Reasoning,
            label: "Reasoning".into(),
            choices: levels
                .into_iter()
                .map(|level| SettingChoice {
                    label: reasoning_label(level).into(),
                    value: String::new(),
                    reasoning: Some(level),
                    selected: reasoning == Some(level),
                    default: default == Some(level),
                })
                .collect(),
        });
    }
    for option in model.map(|m| m.options.as_slice()).unwrap_or_default() {
        if option.choices.is_empty() {
            continue;
        }
        let selected = selections
            .get(&option.id)
            .and_then(|v| v.as_str())
            .unwrap_or(&option.default_choice);
        groups.push(SettingGroup {
            id: ModelSetting::Option(option.id.clone()),
            label: option.label.clone(),
            choices: option
                .choices
                .iter()
                .map(|choice| SettingChoice {
                    label: choice.label.clone(),
                    value: choice.id.clone(),
                    reasoning: None,
                    selected: selected == choice.id,
                    default: option.default_choice == choice.id,
                })
                .collect(),
        });
    }
    groups
}

fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::text_xs())
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.for_popup().text_muted)
        .child(SharedString::from("Default"))
}

/// Brand mark + optional tint for a harness (the Claude mark keeps its brand
/// orange even on the monochrome surface; the mock harness scripts
/// Claude-flavoured runs, so it wears the Claude mark).
/// Flatten the picker's visible rows for one tab. The QUERY NEVER LEAVES THE
/// VIEWED TAB (user request; the old global search spanned every harness and
/// hid the rail): on a harness tab it ranks that harness's models only, on
/// the favorites tab it ranks the starred set. Without a query, a harness
/// tab lists its catalog stars-first and the favorites tab lists every star.
fn scoped_model_rows<'a>(
    query: &str,
    rail: ModelRail,
    effective: Option<HarnessId>,
    descriptors: &[HarnessDescriptor],
    models_for: impl Fn(HarnessId) -> Option<&'a [Model]>,
    is_favorite: impl Fn(HarnessId, &str) -> bool,
) -> Vec<ModelRowData> {
    let row = |descriptor: &HarnessDescriptor, model: &Model| ModelRowData {
        harness: descriptor.id,
        harness_name: SharedString::from(descriptor.name.clone()),
        model: model.clone(),
        selected_only: false,
        ambiguous: false,
    };
    let in_scope = |descriptor: &HarnessDescriptor, model: &Model| match rail {
        ModelRail::All => true,
        ModelRail::Favorites => is_favorite(descriptor.id, &model.id),
        ModelRail::Harness => Some(descriptor.id) == effective,
    };
    if !query.is_empty() {
        // Rank: label prefix < label substring < description hit; stars,
        // then input order, break ties (a field ladder plus a favorite
        // boost, collapsed to our ranks). The description stays
        // in the haystack — opencode's provider attribution ("anthropic")
        // must find its models even inside one tab.
        let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
        let mut input_ix = 0usize;
        for descriptor in descriptors {
            let Some(models) = models_for(descriptor.id) else {
                continue;
            };
            for model in models {
                if !in_scope(descriptor, model) {
                    continue;
                }
                let by_label = popover::match_rank(query, &model.label);
                let by_description = popover::match_rank(
                    query,
                    &format!(
                        "{} {}",
                        model.description.as_deref().unwrap_or(""),
                        model.label
                    ),
                )
                .map(|rank| rank + 2);
                if let Some(rank) = by_label.into_iter().chain(by_description).min() {
                    let starred = !is_favorite(descriptor.id, &model.id);
                    ranked.push((rank, starred as usize, input_ix, row(descriptor, model)));
                }
                input_ix += 1;
            }
        }
        ranked.sort_by_key(|(rank, unstarred, ix, _)| {
            if rail == ModelRail::All {
                (*unstarred, *rank, *ix)
            } else {
                (*rank, *unstarred, *ix)
            }
        });
        return ranked.into_iter().map(|(_, _, _, row)| row).collect();
    }
    match rail {
        ModelRail::All | ModelRail::Favorites => {
            let mut rows = Vec::new();
            for descriptor in descriptors {
                let Some(models) = models_for(descriptor.id) else {
                    continue;
                };
                for model in models {
                    if rail == ModelRail::All || is_favorite(descriptor.id, &model.id) {
                        rows.push(row(descriptor, model));
                    }
                }
            }
            if rail == ModelRail::All {
                rows.sort_by_key(|row| !is_favorite(row.harness, &row.model.id));
            }
            rows
        }
        ModelRail::Harness => {
            let Some(descriptor) = descriptors.iter().find(|d| Some(d.id) == effective) else {
                return Vec::new();
            };
            let Some(models) = models_for(descriptor.id) else {
                return Vec::new();
            };
            let (starred, rest): (Vec<&Model>, Vec<&Model>) = models
                .iter()
                .partition(|m| is_favorite(descriptor.id, &m.id));
            starred
                .into_iter()
                .chain(rest)
                .map(|model| row(descriptor, model))
                .collect()
        }
    }
}

/// Centered muted note filling an empty model list ("No models found").
fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(crate::typography::text_xs())
        .text_color(theme.for_popup().text_muted)
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}

/// Display-side model-list hygiene, mirroring the engine's discovery-side
/// fold (`models_from_session`) for catalogs served by OLDER engines (the
/// space's device may run any version): the `default` alias row drops when a
/// real row exists, an orphan `<model>[1m]` variant presents as its base id
/// with the Context Window trait pinned to 1M, and Claude rows adopt the
/// curated catalog's labels so the version number always shows ("Opus 5.5",
/// not the wire's terse "Opus" alias — user request). Idempotent over
/// already-clean lists. The send path recomposes the advertised id from the
/// base + trait (`pick_model_value`), so a folded pick still runs.
pub(crate) fn normalize_model_rows(harness: HarnessId, models: Vec<Model>) -> Vec<Model> {
    fn strip_1m(id: &str) -> Option<&str> {
        id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
    }
    fn norm(id: &str) -> String {
        id.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let catalog = match harness {
        HarnessId::ClaudeCode => zuse_providers::claude::catalog::static_models(),
        _ => Vec::new(),
    };
    // Curated label for an id: exact normalized match, else — for bare
    // alphabetic aliases like `opus` — the first (flagship-ordered) family
    // row. Versioned foreign ids never fuzzy-match.
    let curated_label = |id: &str| -> Option<String> {
        let id_norm = norm(id);
        if let Some(row) = catalog.iter().find(|m| norm(&m.id) == id_norm) {
            return Some(row.label.clone());
        }
        (!id_norm.is_empty() && id_norm.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| catalog.iter().find(|m| norm(&m.id).contains(&id_norm)))
            .flatten()
            .map(|m| m.label.clone())
    };
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let has_real = ids.iter().any(|id| !id.eq_ignore_ascii_case("default"));
    models
        .into_iter()
        .filter_map(|mut model| {
            if has_real && model.id.eq_ignore_ascii_case("default") {
                return None;
            }
            if let Some(base) = strip_1m(&model.id.clone()) {
                if ids.iter().any(|other| other == base) {
                    // The bare base is listed too — the engine already gave
                    // it the Context Window trait; the variant row is noise.
                    return None;
                }
                model.id = base.to_string();
                // "Opus (1M context)" → "Opus".
                if let Some(at) = model.label.rfind(" (")
                    && model.label.ends_with(')')
                {
                    model.label.truncate(at);
                    while model.label.ends_with(' ') {
                        model.label.pop();
                    }
                }
                if !model.options.iter().any(|o| o.id == "contextWindow") {
                    model.options.push(zuse_protocol::ModelOption {
                        id: "contextWindow".into(),
                        label: "Context Window".into(),
                        choices: vec![
                            zuse_protocol::ModelOptionChoice {
                                id: "200k".into(),
                                label: "200K".into(),
                            },
                            zuse_protocol::ModelOptionChoice {
                                id: "1m".into(),
                                label: "1M".into(),
                            },
                        ],
                        default_choice: "1m".into(),
                    });
                }
            }
            if let Some(label) = curated_label(&model.id) {
                model.label = label;
            }
            Some(model)
        })
        .collect()
}

/// The agent's product name, as status lines and update notices say it.
pub(crate) fn harness_display_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Pi => "Pi",
        HarnessId::Muse => "Muse Code",
        HarnessId::Opencode => "OpenCode",
        HarnessId::Antigravity => "Antigravity",
        HarnessId::Gemini => "Gemini CLI",
        HarnessId::Kiro => "Kiro CLI",
        HarnessId::Zuse => "Zuse",
        HarnessId::Mock => "Mock",
        HarnessId::CustomAcp(_) => "ACP agent",
    }
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::Zuse => (crate::icons::ZUSE_LOGO, Some(gpui::rgb(0x89ca21).into())),
        HarnessId::Gemini => (crate::icons::GEMINI_MARK, Some(gpui::rgb(0x4285f4).into())),
        HarnessId::Kiro => (crate::icons::KIRO_MARK, Some(gpui::rgb(0x9b6df3).into())),
        HarnessId::CustomAcp(id) if id.as_str() == "acp-gemini" => {
            (crate::icons::GEMINI_MARK, Some(gpui::rgb(0x4285f4).into()))
        }
        HarnessId::CustomAcp(_) => (crate::icons::TERMINAL, None),
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::CODEX_MARK, Some(gpui::rgb(0x8b5cf6).into())),
        HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
        // Cognition's mark (the Devin product icon), monochrome.
        HarnessId::Devin => (crate::icons::DEVIN_MARK, None),
        // Monochrome mark, tinted by the surface like OpenAI's.
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        // Nous Research's mark (the Hermes product icon), monochrome.
        HarnessId::Hermes => (crate::icons::HERMES_MARK, None),
        HarnessId::Pi => (crate::icons::PI_MARK, None),
        // The Meta symbol Muse Code ships under, monochrome.
        HarnessId::Muse => (crate::icons::MUSE_MARK, None),
        // The pixel-"o" from opencode's wordmark (their favicon), monochrome.
        HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
        HarnessId::Antigravity => (crate::icons::ANTIGRAVITY_MARK, None),
    }
}

/// `ZUSE_HARNESS=mock` (the e2e/dev rig) opts the mock harness into the UI;
/// production launches never set it, so the mock never surfaces there.
fn mock_harness_enabled() -> bool {
    std::env::var("ZUSE_HARNESS").ok().as_deref().map(str::trim) == Some("mock")
}

/// Production pickers AND chip resolution hide the mock harness — the
/// registry always lists it, but it must never surface in real UI (neither in
/// the picker rail nor as the eager default the chips resolve against).
/// `ZUSE_HARNESS=mock` shows it; otherwise it only remains when it's
/// literally all there is (a dev build with no real harness registered).
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// What the composer actually offers: [`visible_harnesses`] narrowed to the
/// catalog device's enabled set AND installed CLIs (Settings → Providers is
/// per-device state, so a space on another device follows THAT device's
/// toggles; a default-enabled agent whose CLI is missing would only
/// manufacture NotInstalled errors at send, and one that needs a login it
/// doesn't have would only fail on auth). The dev-rig mock opt-in
/// survives the filter. There is NO fallback: a catalog where nothing is
/// both enabled and installed offers nothing, and the composer surfaces the
/// no-agents empty state + blocks new sends — resurrecting descriptors that
/// can only fail with NotInstalled is the #128 bug.
pub fn offered_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    offered_harnesses_impl(list, mock_harness_enabled())
}

fn offered_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, allow_mock)
        .into_iter()
        .filter(|d| {
            d.installed
                // An agent that only runs signed in, without a login (or a
                // key/proxy standing in for one), would only fail at send.
                && d.signed_in != Some(false)
                && (zuse_engine::registry::descriptor_enabled(d)
                    || (allow_mock && d.id == HarnessId::Mock))
        })
        .collect()
}

// Tabs and search consume 80px, with 2px reserved for the card border.
// Keep one model row where possible; long option stacks scroll in the tray.
/// The model card's fixed chrome: the bordered search header (10px padding
/// around a 32px field, plus its hairline), the effort slider band and the
/// card border.
const MODEL_MENU_CHROME: f32 = 120.0;

/// Model picker rows, rail items and the search field share one radius.
const MODEL_ROW_RADIUS: f32 = 7.0;

/// The selected model row / provider treatment: a lime-tinted wash and ring.
fn model_selected<E: Styled>(el: E, theme: &Theme) -> E {
    el.bg(theme.accent.opacity(0.12))
        .shadow(vec![gpui::BoxShadow {
            color: theme.accent.opacity(0.2),
            offset: gpui::point(px(0.0), px(0.0)),
            blur_radius: px(0.0),
            spread_radius: px(1.0),
            inset: true,
        }])
}

/// The lime check marking the selected model row.
fn model_check(theme: &Theme) -> gpui::Svg {
    crate::icons::icon(crate::icons::CHECK)
        .size(px(14.0))
        .flex_none()
        .text_color(theme.accent)
}

fn model_menu_height(setting_count: usize) -> f32 {
    let tray = if setting_count == 0 {
        0.0
    } else {
        (setting_count as f32 * 32.0 + 7.0).min(236.0)
    };
    MODEL_MENU_CHROME + 216.0 + tray
}

fn model_menu_budgets(height: f32, setting_count: usize) -> (f32, f32) {
    let body = (height - MODEL_MENU_CHROME).max(0.0);
    let desired_tray = if setting_count == 0 {
        0.0
    } else {
        (setting_count as f32 * 32.0 + 7.0).min(236.0)
    };
    let tray = desired_tray.min((body - 30.0).max(0.0));
    // The list absorbs changes in tray height, keeping the card's top edge
    // and search field stationary while choosing models and options.
    (body - tray, tray)
}

/// Chip text while the chip's width animates: clipped rather than
/// ellipsized (an ellipsis would flicker in and out each frame), with the
/// clip edge faded so a half-shown glyph never ends in a hard cut. The ramp
/// is as wide as the hidden text, so it is zero exactly at the clip edge.
fn resizing_chip_text(
    id: SharedString,
    shrink: f32,
    color: Option<gpui::Hsla>,
    text: SharedString,
) -> AnyElement {
    let overflow = gpui::ScrollHandle::new();
    crate::edge_fade::edge_faded(
        16.0,
        false,
        false,
        div()
            .id(id)
            .flex_shrink(shrink)
            .min_w_0()
            .overflow_hidden()
            .track_scroll(&overflow)
            .flex()
            .when_some(color, |el, color| el.text_color(color))
            .child(div().flex_none().whitespace_nowrap().child(text)),
    )
    .fade_right(true)
    .fade_scroll_x(&overflow)
    .into_any_element()
}

/// Fast mode's `(on, off)` choices, whatever form a harness gives it: a
/// `fastMode`/`fast_mode` on/off toggle (Claude), Cursor's `fast` true/false,
/// or a tier/speed option offering `fast` (Codex, Devin). Off is the default
/// when fast isn't, else the other choice — Cursor runs some models fast by
/// default. Every model then gets the same fast-mode UI.
fn fast_mode_values(option: &zuse_protocol::ModelOption) -> Option<(&str, &str)> {
    // A switch cannot represent a third tier; keep the full option menu.
    if option.choices.len() != 2 {
        return None;
    }
    let has = |id: &str| option.choices.iter().any(|choice| choice.id == id);
    let on = if matches!(option.id.as_str(), "fastMode" | "fast_mode") && has("on") {
        "on"
    } else if option.id == "fast" && has("true") {
        "true"
    } else if has("fast") {
        "fast"
    } else {
        return None;
    };
    let off = if option.default_choice != on && has(&option.default_choice) {
        option.default_choice.as_str()
    } else {
        option
            .choices
            .iter()
            .map(|choice| choice.id.as_str())
            .find(|id| *id != on)?
    };
    Some((on, off))
}

/// Attach the model menu to the trigger's trailing edge.
fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: (Option<std::time::Instant>, bool),
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.relative().child(if closing.1 {
            popover::anchored_menu_below_end(id, element, closing.0)
        } else {
            popover::anchored_menu_above_end(id, element, closing.0)
        });
    }
    chip
}

fn attach_creation_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: (Option<std::time::Instant>, bool),
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.relative().child(if closing.1 {
            popover::anchored_menu_below(id, element, closing.0)
        } else {
            popover::anchored_menu_above(id, element, closing.0)
        });
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // A ZUSE_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open_kind() {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        let focus_on_mount = std::mem::take(&mut self.focus_on_mount) && self.is_open();
        if focus_on_mount {
            // The frame exists even when loading/error/empty content omits
            // the input. Claim it during mount, after any pre-frame shell
            // recovery has handled the old dispatch tree.
            window.focus(&self.focus, cx);
        }
        if self.is_open() {
            let search = self.search.focus_handle(cx);
            let frame = self.focus.clone();
            let autofocus_search = self.open_kind() != Some(PickerKind::HarnessModel);
            window.defer(cx, move |window, cx| {
                let has_search = frame.contains(&search, window);
                if autofocus_search && focus_on_mount && frame.is_focused(window) && has_search {
                    // Transfer to the filter only once it is actually mounted.
                    window.focus(&search, cx);
                } else if search.is_focused(window) && !has_search {
                    // Loading, empty, and error states can omit the search box.
                    // Keep Escape/arrow keys on the mounted menu in those states.
                    window.focus(&frame, cx);
                }
            });
        }

        // Resolve the selected model before opening a picker. Unselected
        // agents wait until the picker is actually needed.
        self.ensure_harnesses(false, cx);
        self.prefetch_models(false, cx);
        // A popover opened data-side (ZUSE_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open_kind(),
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (zuse `modelText`); the
        // harness reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        // No-agents state: nothing runnable resolved (and the catalog is
        // loaded, so that's a conclusion, not a loading gap) — the chip says
        // so instead of wearing a brand mark for an agent that can't run.
        let no_agents = self.no_agents_available(cx) && self.effective_harness(cx).is_none();
        self.settings_model.window = Some(window.window_handle());
        let model_name = self.model_name(cx);
        let model_label: SharedString = if let Some(pending) = self.settings_model.pending {
            pending.into()
        } else if self.settings_model.defaults {
            crate::settings::default_model(cx)
                .map(|selection| selection.label.clone())
                .unwrap_or_else(|| "Agent default".into())
                .into()
        } else if let Some(title) = &self.title {
            // The saved choice, not the tab being browsed.
            match (title.harness, title.model.as_deref()) {
                (None, _) => "Session agent".into(),
                (Some(_), None) => "Agent default".into(),
                (Some(harness), Some(id)) => self
                    .models
                    .get(&harness)
                    .and_then(Loadable::ready)
                    .and_then(|models| models.iter().find(|m| m.id == id))
                    .map(|m| m.label.clone())
                    .or_else(|| {
                        crate::settings::default_model(cx)
                            .filter(|model| {
                                Some(model.provider) == self.effective_harness(cx)
                                    && model.model == id
                            })
                            .map(|model| model.label.clone())
                    })
                    .or_else(|| self.defaults.label_for(id).map(str::to_owned))
                    .unwrap_or_else(|| id.to_owned())
                    .into(),
            }
        } else {
            match &model_name {
                ModelName::Named(label) => label.clone(),
                ModelName::None { no_agents: true } => "No agents available".into(),
                ModelName::Loading | ModelName::None { .. } => SharedString::default(),
            }
        };
        // A list still from the previous device is loading for this one.
        let catalog_loading = (!self.cloud_models_active(cx) && self.harnesses_stale)
            || matches!(self.harness_catalog(cx), Loadable::Idle | Loadable::Loading);
        // Harness unknown while the catalog resolves: the pixel-glyph loader
        // instead of guessing a brand mark.
        let chip_icon_loading = self.title.is_none()
            && self.effective_harness(cx).is_none()
            && !no_agents
            && catalog_loading;
        // Harness known but nothing names the model yet (fresh install, no
        // remembered pick): a ghost label instead of a bare icon.
        let chip_label_loading = self.title.is_none() && model_name == ModelName::Loading;
        let chip_harness = match &self.title {
            Some(title) => title.harness,
            None => self.effective_harness(cx),
        };
        let harness_icon: (&'static str, Option<gpui::Hsla>) = match chip_harness {
            Some(harness) => harness_brand_icon(harness),
            None if self.title.is_some() => (crate::icons::CHAT_ROUND_LINE, Some(theme.text_muted)),
            None if no_agents => (crate::icons::TERMINAL, Some(theme.text_muted)),
            None => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        // The chip names the model and its effort only; the other options
        // (context, fast mode, ...) live in the popover. The effort always
        // reads in the muted second tone, default or not (user request).
        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the new-session context tray (see
        // `render_new_thread_git_selectors`), not here.
        let closing = (self.open.closing_since(), self.menu_geometry().below);
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            // Context-tray pickers — their popovers mount up there.
            Some(PickerKind::Branch)
            | Some(PickerKind::Checkout)
            | Some(PickerKind::Space)
            | Some(PickerKind::Device) => None,
            Some(PickerKind::HarnessModel) => {
                let menu = if self.compact_model_picker(cx) {
                    self.render_compact_menu(window, cx)
                } else {
                    let content = self.render_harness_model_popover(cx);
                    self.popover_frame_flush(380.0, content, cx)
                };
                Some((PickerKind::HarnessModel, menu))
            }
            None => None,
        };

        // The composer places this model chip beside microphone and Send:
        // brand icon + model name, then the effort as the chip's muted second
        // tone — the ladder's level, else an effort option's choice (Cursor).
        // None for the title picker (titles always run at minimal reasoning).
        let fast = !self.is_settings_model()
            && self.selected_model(cx).is_some_and(|model| {
                model.options.iter().any(|option| {
                    fast_mode_values(option).is_some_and(|(on, _)| {
                        self.resolved(cx)
                            .model_options
                            .get(&option.id)
                            .and_then(|value| value.as_str())
                            .unwrap_or(&option.default_choice)
                            == on
                    })
                })
            });
        let chip_width =
            self.model_chip_width(window, &model_label, None, chip_label_loading, fast, cx);
        let model_chip = self
            .trigger_chip(
                PickerKind::HarnessModel,
                model_label,
                true,
                Some(harness_icon),
                chip_icon_loading,
                chip_label_loading,
                None,
                &theme,
                cx,
            )
            .when(fast, |chip| {
                chip.child(motion::fast_tier(
                    "composer-fast-tier",
                    div().flex_none().child(
                        crate::icons::icon(crate::icons::FAST_TIER_BOLD)
                            .size(px(13.0))
                            .text_color(theme.accent),
                    ),
                ))
            })
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .ml_auto()
                    .text_color(theme.text_muted),
            );
        let model_chip = model_chip
            .w(px(chip_width))
            .when(self.is_settings_model(), |view| {
                view.w(px(256.))
                    .max_w(window.viewport_size().width * 0.4)
                    .h(px(28.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(zuse_viewport::palette::tone(
                        gpui::hsla(128. / 360., 0.05, 0.17, 1.),
                        gpui::hsla(112. / 360., 0.12, 0.82, 1.),
                    ))
                    .bg(zuse_viewport::settings_style::card_bg())
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_size(px(12.))
                    .hover(|view| view.bg(theme.settings_frame_bg()))
                    .track_focus(&self.settings_model.trigger)
                    .aria_label(if self.settings_model.defaults {
                        crate::localization::text("onboarding-defaults_default_model", cx)
                    } else {
                        ("Thread naming model").to_string()
                    })
                    .when(self.settings_model.pending.is_some(), |view| {
                        view.opacity(0.6)
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if this.settings_model.trigger.is_focused(window)
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            this.toggle(PickerKind::HarnessModel, window, cx);
                            cx.stop_propagation();
                        }
                    }))
            });
        // Both presentations align to the chip's trailing edge beside Send.
        let model_chip = attach_overlay_end(
            model_chip,
            &mut overlay,
            PickerKind::HarnessModel,
            "model-popover",
            closing,
        );
        let effort = self.compact_effort(cx);
        let has_options = self.model_configuration_available(cx)
            && (self.live_groups(cx).iter().any(|group| {
                Self::compact_option_visible(&group.id, &self.compact_hidden_options(cx))
            }) || self.compact_fast_choice(cx).is_some());
        let effort_control = if !self.is_settings_model() && (effort.is_some() || has_options) {
            {
                let label = effort
                    .as_ref()
                    .and_then(|e| e.labels.get(e.selected))
                    .cloned()
                    .unwrap_or_else(|| "Options".into());
                let mut button = div()
                    .id("reasoning-trigger")
                    .relative()
                    .role(gpui::Role::Button)
                    .aria_label("Reasoning")
                    .tab_index(0)
                    .cursor_pointer()
                    .px(px(8.))
                    .h(px(28.))
                    .flex_none()
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(crate::typography::text_xs())
                    .text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.muted_fill().opacity(0.6)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.effort_menu_open = !this.effort_menu_open;
                            if this.effort_menu_open {
                                this.dismiss(cx);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        } else if event.keystroke.key == "escape" && this.effort_menu_open {
                            this.effort_menu_open = false;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.effort_menu_open = !this.effort_menu_open;
                        if this.effort_menu_open {
                            this.dismiss(cx);
                        }
                        cx.notify();
                    }))
                    .child(label)
                    .child(
                        div()
                            .id("reasoning-chevron")
                            .size(px(14.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                                    .size(px(14.))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child({
                        let entity = cx.entity().downgrade();
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let _ = entity.update(cx, |this, _| {
                                    this.effort_trigger_bounds = Some(bounds)
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0()
                    });
                if self.effort_menu_open {
                    let popup_theme = Theme::of(cx).for_popup();
                    let labels = effort
                        .as_ref()
                        .map(|e| e.labels.clone())
                        .unwrap_or_default();
                    let selected = effort.as_ref().map(|e| e.selected);
                    let sections =
                        (self.tray_group_count(cx) > 0).then(|| self.render_traits_sections(cx));
                    let fast_choice = self.compact_fast_choice(cx);
                    let menu = popover::popover_card(&popup_theme)
                        .w(px(if has_options { 208. } else { 170. }))
                        .on_mouse_down_out(cx.listener(
                            |this, event: &gpui::MouseDownEvent, _, cx| {
                                if !this
                                    .setting_bounds
                                    .is_some_and(|bounds| bounds.contains(&event.position))
                                    && !this
                                        .effort_trigger_bounds
                                        .is_some_and(|bounds| bounds.contains(&event.position))
                                {
                                    this.effort_menu_open = false;
                                    cx.notify();
                                }
                            },
                        ))
                        .when(!labels.is_empty(), |menu| {
                            menu.child(
                                div()
                                    .px(px(8.))
                                    .py(px(4.))
                                    .text_size(px(11.))
                                    .text_color(popup_theme.text_muted)
                                    .child("Reasoning"),
                            )
                        })
                        .children(labels.into_iter().enumerate().map(|(index, label)| {
                            popover::menu_row(
                                &popup_theme,
                                false,
                                format!("reasoning-choice-{index}"),
                            )
                            .id(("reasoning-choice", index))
                            .role(gpui::Role::Button)
                            .tab_index(0)
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    if let Some(effort) = this.compact_effort(cx) {
                                        this.pick_compact_effort(effort, index, cx);
                                    }
                                    this.effort_menu_open = false;
                                    cx.stop_propagation();
                                    cx.notify();
                                } else if event.keystroke.key == "escape" {
                                    this.effort_menu_open = false;
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }))
                            .h(px(28.))
                            .py(px(0.))
                            .text_size(px(12.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(effort) = this.compact_effort(cx) {
                                    this.pick_compact_effort(effort, index, cx);
                                }
                                this.effort_menu_open = false;
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(label)
                            .child(div().flex_1())
                            .when(Some(index) == selected, |row| {
                                row.child(
                                    crate::icons::icon(crate::icons::CHECK)
                                        .size(px(13.))
                                        .text_color(popup_theme.text),
                                )
                            })
                        }))
                        .when(has_options, |menu| {
                            menu.child(
                                div()
                                    .mt(px(5.))
                                    .pt(px(6.))
                                    .border_t_1()
                                    .border_color(popup_theme.border.opacity(0.5))
                                    .px(px(8.))
                                    .text_size(px(11.))
                                    .text_color(popup_theme.text_muted)
                                    .child("Modes"),
                            )
                        })
                        .children(sections)
                        .when_some(fast_choice, |menu, (option, choice, default, on)| {
                            let key_option = option.clone();
                            let key_choice = choice.clone();
                            menu.child(
                                popover::menu_row(&popup_theme, false, "fast-mode")
                                    .id("reasoning-fast-mode")
                                    .role(gpui::Role::Switch)
                                    .aria_label("Fast mode")
                                    .tab_index(0)
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if !event.is_held
                                                && matches!(
                                                    event.keystroke.key.as_str(),
                                                    "enter" | "space"
                                                )
                                            {
                                                this.pick_option(
                                                    key_option.clone(),
                                                    key_choice.clone(),
                                                    default,
                                                    cx,
                                                );
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .aria_toggled(if on {
                                        gpui::Toggled::True
                                    } else {
                                        gpui::Toggled::False
                                    })
                                    .h(px(30.))
                                    .py(px(0.))
                                    .gap(px(8.))
                                    .text_size(px(12.))
                                    .line_height(px(16.))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pick_option(
                                            option.clone(),
                                            choice.clone(),
                                            default,
                                            cx,
                                        );
                                        cx.stop_propagation();
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::FAST_TIER)
                                            .size(px(14.))
                                            .text_color(popup_theme.text_muted),
                                    )
                                    .child(div().flex_1().child("Fast"))
                                    .child(crate::settings::widgets::toggle_switch(
                                        &popup_theme,
                                        on,
                                        "reasoning-fast",
                                    )),
                            )
                        });
                    button = button.child(popover::anchored_menu_above_end(
                        "reasoning-menu",
                        menu.into_any_element(),
                        None,
                    ));
                }
                Some(button.into_any_element())
            }
        } else {
            None
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            // Shrinkable under row pressure, like the footer chips: the chip's
            // own `min_w_0().truncate()` label/suffix only engage when this
            // cluster is allowed to give up width — `flex_none` here let the
            // labels paint over the attach/send buttons at narrow widths
            // instead of truncating (user report).
            .min_w_0()
            .gap(px(4.0))
            .child(model_chip)
            .children(effort_control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuse_protocol::{FolderEntry, Model, ModelOption, ModelOptionChoice};

    struct ModelShortcutHost {
        focus_sub: Option<gpui::Subscription>,
        root: FocusHandle,
        editor: FocusHandle,
        neutral: FocusHandle,
        pickers: Entity<Pickers>,
    }

    impl Render for ModelShortcutHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if self.focus_sub.is_none() {
                self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Self, window, cx| {
                    let root = this.root.clone();
                    let editor = this.editor.clone();
                    let neutral = this.neutral.clone();
                    window.on_next_frame(move |window, cx| {
                        crate::shell::restore_mounted_focus(&root, &editor, &neutral, window, cx);
                    });
                    cx.notify();
                }));
            }
            let root = self.root.clone();
            let editor = self.editor.clone();
            let neutral = self.neutral.clone();
            window.defer(cx, move |window, cx| {
                crate::shell::restore_mounted_focus(&root, &editor, &neutral, window, cx);
            });
            div()
                .size_full()
                .track_focus(&self.root)
                .on_action(
                    cx.listener(|this, _: &crate::shell::OpenModelPicker, window, cx| {
                        this.pickers
                            .update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
                        cx.notify();
                    }),
                )
                .child(div().track_focus(&self.editor))
                .child(div().track_focus(&self.neutral))
                .child(self.pickers.clone())
        }
    }

    #[gpui::test]
    fn saved_model_survives_a_fresh_catalog_without_becoming_a_new_choice(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        for had_disk_catalog in [false, true] {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.chats.push(serde_json::from_value(serde_json::json!({
                    "id":"saved-chat", "deviceId":"device", "archived":false, "createdAt":"2026-09-01T00:00:00Z",
                    "config":{"harness":"codex", "model":"saved-model", "reasoning":"high", "sandbox":"workspace-write", "modelOptions":{"serviceTier":"fast"}}
                })).unwrap());
                state.selected_chat = Some("saved-chat".into()); state
            });
            let pickers = cx.new(|cx| Pickers::new(state, cx));
            pickers.update(cx, |pickers, cx| {
                pickers.defaults = ComposerDefaults::default();
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
                if had_disk_catalog {
                    pickers.apply_model_catalog(
                        HarnessId::Codex,
                        Loadable::Ready(vec![bare_model("saved-model", "Remembered model")]),
                        cx,
                    );
                }
                pickers.apply_model_catalog(
                    HarnessId::Codex,
                    Loadable::Ready(vec![bare_model("fresh-default", "Fresh default")]),
                    cx,
                );
                assert_eq!(pickers.resolved(cx).model.as_deref(), Some("saved-model"));
                assert_eq!(pickers.resolved(cx).reasoning, Some(ReasoningLevel::High));
                assert_eq!(pickers.resolved(cx).model_options["serviceTier"], "fast");
                assert_eq!(
                    pickers.selected_model_label(cx).as_deref(),
                    Some(if had_disk_catalog {
                        "Remembered model"
                    } else {
                        "saved-model"
                    })
                );
                let rows = pickers.model_rows(cx);
                assert_eq!(rows[0].model.id, "saved-model");
                assert!(rows[0].selected_only);
                assert_eq!(pickers.selected_model_index(cx), 0);
                assert_eq!(pickers.models[&HarnessId::Codex].ready().unwrap().len(), 1);
                pickers.activate_model_index(0, cx);
                assert_eq!(pickers.resolved(cx).model.as_deref(), Some("saved-model"));
                // Changing selection also invalidates the row cache; the
                // unlisted ID does not remain available as a new choice.
                pickers.config.model = Some("fresh-default".into());
                let rows = pickers.model_rows(cx);
                assert_eq!(rows.len(), 1);
                assert!(!rows[0].selected_only);
                assert_eq!(
                    pickers.selected_model_label(cx).as_deref(),
                    Some("Fresh default")
                );
            });
        }
    }

    #[test]
    fn explicit_missing_model_does_not_resolve_to_the_catalog_default() {
        let rows = vec![bare_model("first", "First")];
        assert_eq!(selected_catalog_model(&rows, None).unwrap().id, "first");
        assert!(selected_catalog_model(&rows, Some("saved")).is_none());
    }

    fn side_chat_picker(unsaved: bool, cx: &mut App) -> (Entity<AppState>, Entity<Pickers>) {
        let parent = cx.new(|_| AppState::new());
        let chat = serde_json::from_value(serde_json::json!({
            "id": "side", "parentChatId": "main", "deviceId": "local",
            "cwd": "/tmp/main", "branch": "main", "archived": false,
            "createdAt": chrono::Utc::now(),
            "config": {
                "harness": "claude-code", "model": "parent-model", "reasoning": "high",
                "modelOptions": { "context": "1m" }, "sandbox": "auto", "plan": true
            }
        }))
        .unwrap();
        let state = cx.new(|cx| AppState::side_chat_state(&parent, chat, unsaved, cx));
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, _| {
            pickers.harnesses = Loadable::Ready(vec![
                descriptor(HarnessId::ClaudeCode, "Claude Code"),
                descriptor(HarnessId::Codex, "Codex"),
            ]);
            pickers.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Ready(vec![bare_model("parent-model", "Parent model")]),
            );
        });
        (state, pickers)
    }

    #[gpui::test]
    fn unsaved_side_chat_can_pick_another_harness_before_catalog_load(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let (state, pickers) = side_chat_picker(true, cx);
            pickers.update(cx, |pickers, cx| {
                assert!(!pickers.harness_locked(cx));
                assert_eq!(pickers.rail_descriptors(cx).len(), 2);
                pickers.pick_harness(HarnessId::Codex, cx);
                let resolved = pickers.resolved(cx);
                assert_eq!(resolved.harness, Some(HarnessId::Codex));
                assert_eq!(resolved.model, None);
                assert_eq!(resolved.reasoning, None);
                assert!(resolved.model_options.is_empty());
                let create = state.read(cx).unsaved_side_chat_create("side").unwrap();
                assert_eq!(create["config"]["harness"], "codex");
                assert_eq!(create["config"]["model"], serde_json::Value::Null);
                assert_eq!(create["config"]["sandbox"], "auto");
                assert_eq!(create["config"]["plan"], true);
                assert_eq!(create["parentChatId"], "main");
                assert_eq!(create["cwd"], "/tmp/main");
                assert_eq!(create["branch"], "main");

                pickers.apply_model_catalog(
                    HarnessId::Codex,
                    Loadable::Ready(vec![bare_model("codex-model", "Codex model")]),
                    cx,
                );
                assert_eq!(pickers.resolved(cx).model.as_deref(), Some("codex-model"));
            });
        });
    }

    #[gpui::test]
    fn side_chat_choices_remain_editable_after_creation(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (out, mut requests) = tokio::sync::mpsc::channel::<String>(256);
        let (_replies, inbound) = tokio::sync::mpsc::channel::<String>(256);
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let (state, pickers) = side_chat_picker(true, cx);
            state.update(cx, |state, _| {
                state.set_test_engine(EngineHandle::from_test_client(zuse_rpc::RpcClient::new(
                    out, inbound,
                )));
            });
            pickers.update(cx, |pickers, cx| {
                let mut model = bare_model("codex-model", "Codex model");
                model.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::Medium];
                pickers
                    .models
                    .insert(HarnessId::Codex, Loadable::Ready(vec![model]));
                pickers
                    .defaults
                    .toggle_favorite(HarnessId::Codex, "codex-model");
                pickers.model_rail = ModelRail::Favorites;
                let rows = pickers.model_rows(cx);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].harness, HarnessId::Codex);
                pickers.activate_model_index(0, cx);
                pickers.pick_reasoning(ReasoningLevel::Low, cx);
                let create = state.read(cx).unsaved_side_chat_create("side").unwrap();
                assert_eq!(create["config"]["harness"], "codex");
                assert_eq!(create["config"]["model"], "codex-model");
                assert_eq!(create["config"]["reasoning"], "low");
                assert_eq!(create["config"]["modelOptions"], serde_json::json!({}));
                assert!(!state.read(cx).configuration_saving());
            });
        });
        cx.run_until_parked();
        assert!(
            requests.try_recv().is_err(),
            "draft choices must not send RPCs"
        );

        cx.update(|cx| {
            // Persisted conversations can change provider, including forks.
            let (_, fork) = side_chat_picker(false, cx);
            fork.update(cx, |pickers, cx| {
                assert!(!pickers.harness_locked(cx));
                assert_eq!(pickers.rail_descriptors(cx).len(), 2);
            });
            let (state, pickers) = side_chat_picker(true, cx);
            state.update(cx, |state, _| {
                state.begin_pending_send("side", "first", chrono::Utc::now());
            });
            pickers.update(cx, |pickers, cx| {
                assert!(
                    pickers.harness_locked(cx),
                    "first send fixes the harness before the RPC finishes"
                );
                pickers.pick_harness(HarnessId::Codex, cx);
                assert_eq!(pickers.resolved(cx).harness, Some(HarnessId::ClaudeCode));
            });
            state.update(cx, |state, _| state.end_pending_send("side", "first"));
            assert!(
                !pickers.read(cx).harness_locked(cx),
                "failed creation remains editable"
            );
            state.update(cx, |state, cx| state.side_chat_saved("side", cx));
            pickers.update(cx, |pickers, cx| {
                assert!(!pickers.harness_locked(cx));
                assert_eq!(pickers.rail_descriptors(cx).len(), 2);
            });
        });
    }

    #[gpui::test]
    fn missing_selected_harness_is_not_reintroduced_into_the_rail(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let state = cx.new(|_| AppState::new());
            let pickers = cx.new(|cx| Pickers::new(state, cx));
            pickers.update(cx, |pickers, cx| {
                pickers.config.harness = Some(HarnessId::Codex);
                let mut missing = descriptor(HarnessId::Codex, "Codex");
                missing.installed = false;
                pickers.harnesses = Loadable::Ready(vec![missing]);
                assert!(pickers.rail_descriptors(cx).is_empty());
            });
        });
    }

    #[gpui::test]
    fn refresh_failure_keeps_rows_and_catalog_change_reanchors_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let mut pickers = Pickers::new(state, cx);
            pickers.config.harness = Some(HarnessId::ClaudeCode);
            pickers.config.model = Some("chosen".into());
            pickers.harnesses =
                Loadable::Ready(vec![descriptor(HarnessId::ClaudeCode, "Claude Code")]);
            pickers
        });
        handle
            .update(cx, |pickers, window, cx| {
                let rows = vec![bare_model("chosen", "Chosen"), bare_model("other", "Other")];
                pickers.apply_model_catalog(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(rows.clone()),
                    cx,
                );
                pickers.open_model_menu(window, cx);
                assert_eq!(pickers.active, 0);
                pickers.apply_model_catalog(
                    HarnessId::ClaudeCode,
                    Loadable::Error("refresh failed".into()),
                    cx,
                );
                assert_eq!(
                    pickers.models[&HarnessId::ClaudeCode],
                    Loadable::Ready(rows.clone())
                );
                assert_eq!(
                    pickers
                        .model_refresh_errors
                        .get(&HarnessId::ClaudeCode)
                        .map(String::as_str),
                    Some("refresh failed")
                );
                pickers.apply_model_catalog(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(rows.into_iter().rev().collect()),
                    cx,
                );
                assert_eq!(pickers.active, 1);
                assert!(
                    !pickers
                        .model_refresh_errors
                        .contains_key(&HarnessId::ClaudeCode)
                );
                pickers.apply_model_catalog(
                    HarnessId::Codex,
                    Loadable::Error("cold failure".into()),
                    cx,
                );
                assert!(matches!(
                    pickers.models[&HarnessId::Codex],
                    Loadable::Error(_)
                ));
            })
            .unwrap();
    }

    #[gpui::test]
    fn model_shortcut_focuses_mounted_picker_and_routes_navigation(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            crate::composer::init(cx, Default::default());
            cx.bind_keys([gpui::KeyBinding::new(
                "cmd-/",
                crate::shell::OpenModelPicker,
                None,
            )]);
        });
        let handle = cx.add_window(|_, cx| ModelShortcutHost {
            focus_sub: None,
            root: cx.focus_handle(),
            editor: cx.focus_handle(),
            neutral: cx.focus_handle(),
            pickers: cx.new(|cx| {
                let state = cx.new(|_| AppState::new());
                let mut pickers = Pickers::new(state, cx);
                pickers.config.harness = Some(HarnessId::ClaudeCode);
                pickers.config.model = Some("first".into());
                pickers.harnesses =
                    Loadable::Ready(vec![descriptor(HarnessId::ClaudeCode, "Claude Code")]);
                pickers.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![
                        bare_model("first", "First"),
                        bare_model("second", "Second"),
                    ]),
                );
                pickers
            }),
        });
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .update(cx, |host, window, cx| window.focus(&host.editor, cx))
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "cmd-/");
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .update(cx, |host, window, cx| {
                host.pickers.read_with(cx, |pickers, cx| {
                    assert!(pickers.is_open());
                    assert!(
                        pickers.focus.contains_focused(window, cx),
                        "shortcut must transfer focus into the mounted picker"
                    );
                    assert!(pickers.focus.is_focused(window));
                    assert!(
                        !pickers.search.read(cx).focus_handle(cx).is_focused(window),
                        "opening the model picker must leave search unfocused"
                    );
                });
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "down");
        handle
            .read_with(cx, |host, cx| assert_eq!(host.pickers.read(cx).active, 1))
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "up");
        handle
            .read_with(cx, |host, cx| assert_eq!(host.pickers.read(cx).active, 0))
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "s");
        handle
            .read_with(cx, |host, cx| {
                assert!(host.pickers.read(cx).search.read(cx).text().is_empty());
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "cmd-2");
        handle
            .read_with(cx, |host, cx| {
                assert_eq!(host.pickers.read(cx).effective_model_id(cx), Some("second"));
            })
            .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.simulate_keystrokes(handle.into(), "cmd-/");
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .update(cx, |host, window, cx| {
                let search = host
                    .pickers
                    .read_with(cx, |pickers, cx| pickers.search.read(cx).focus_handle(cx));
                window.focus(&search, cx);
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "s");
        handle
            .read_with(cx, |host, cx| {
                assert_eq!(host.pickers.read(cx).search.read(cx).text(), "s");
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "escape");
        handle
            .read_with(cx, |host, cx| assert!(!host.pickers.read(cx).is_open()))
            .unwrap();

        // Catalog loading/error/empty cards omit the input but still need
        // immediate keyboard focus on their mounted frame for Escape.
        for catalog in [
            Loadable::Loading,
            Loadable::Error("offline".into()),
            Loadable::Ready(vec![]),
        ] {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            handle
                .update(cx, |host, window, cx| {
                    host.pickers.update(cx, |pickers, cx| {
                        pickers.config.harness = None;
                        pickers.defaults.harness = None;
                        pickers.harnesses = catalog;
                        cx.notify();
                    });
                    window.focus(&host.editor, cx);
                })
                .unwrap();
            cx.simulate_keystrokes(handle.into(), "cmd-/");
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            handle
                .update(cx, |host, window, cx| {
                    assert!(host.pickers.read(cx).focus.contains_focused(window, cx));
                })
                .unwrap();
            cx.simulate_keystrokes(handle.into(), "escape");
            handle
                .read_with(cx, |host, cx| assert!(!host.pickers.read(cx).is_open()))
                .unwrap();
        }
    }

    #[test]
    fn model_menu_height_tracks_both_sides_and_long_settings() {
        for (top, bottom, viewport, below) in [
            (80.0, 112.0, 500.0, true),
            (430.0, 462.0, 500.0, false),
            (190.0, 222.0, 360.0, false),
        ] {
            let geometry = popover::menu_geometry(top, bottom, viewport);
            assert_eq!(geometry.below, below);
            for settings in [0, 1, 3, 20] {
                let height = geometry.height.min(model_menu_height(settings));
                let (list, tray) = model_menu_budgets(height, settings);
                assert!(list + tray + 82.0 <= geometry.height);
                assert!((0.0..=216.0).contains(&list));
                assert!((0.0..=236.0).contains(&tray));
                if settings == 0 {
                    assert_eq!(tray, 0.0);
                }
            }
        }
        assert!(model_menu_budgets(180.0, 2).0 < model_menu_budgets(400.0, 2).0);
    }

    #[gpui::test]
    fn model_menu_prefers_above_when_it_has_room(cx: &mut gpui::TestAppContext) {
        struct Fixture(Entity<Pickers>, bool);
        impl Render for Fixture {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let top = if self.1 {
                    window.viewport_size().height - px(50.0)
                } else {
                    px(300.0)
                };
                div().size_full().relative().child(
                    div()
                        .absolute()
                        .top(top)
                        .left(px(200.0))
                        .child(self.0.clone()),
                )
            }
        }
        cx.update(|cx| cx.set_global(Theme::dark()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Fixture(cx.new(|cx| Pickers::new(state, cx)), false)
        });
        for (new_thread, near_bottom) in
            [(true, false), (true, true), (false, false), (false, true)]
        {
            handle
                .update(cx, |fixture, _, cx| {
                    fixture.1 = near_bottom;
                    fixture.0.update(cx, |picker, cx| {
                        picker.state.update(cx, |state, cx| {
                            state.selected_chat = (!new_thread).then(|| "thread".into());
                            cx.notify();
                        });
                        cx.notify();
                    });
                    cx.notify();
                })
                .unwrap();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            handle
                .read_with(cx, |fixture, cx| {
                    let picker = fixture.0.read(cx);
                    let geometry = picker.menu_geometry[&PickerKind::HarnessModel];
                    assert!(!geometry.below);
                    assert!(geometry.height > 180.0);
                    assert!(
                        model_menu_budgets(geometry.height.min(model_menu_height(2)), 2).0 >= 30.0
                    );
                })
                .unwrap();
        }
    }

    #[test]
    fn every_fast_mode_encoding_gets_the_same_controls() {
        let option = |id: &str, choices: &[&str], default: &str| ModelOption {
            id: id.into(),
            label: id.into(),
            choices: choices
                .iter()
                .map(|c| ModelOptionChoice {
                    id: (*c).into(),
                    label: (*c).into(),
                })
                .collect(),
            default_choice: default.into(),
        };
        // Codex tier, Claude toggle, ACP speed and the snake_case toggle.
        assert_eq!(
            fast_mode_values(&option("serviceTier", &["default", "fast"], "default")),
            Some(("fast", "default"))
        );
        assert_eq!(
            fast_mode_values(&option("fastMode", &["off", "on"], "off")),
            Some(("on", "off"))
        );
        assert_eq!(
            fast_mode_values(&option("speed", &["standard", "fast"], "standard")),
            Some(("fast", "standard"))
        );
        assert_eq!(
            fast_mode_values(&option("fast_mode", &["off", "on"], "off")),
            Some(("on", "off"))
        );
        // Cursor's true/false switch, off by default (Opus) and on by
        // default (Grok, Composer): both still toggle between the two.
        assert_eq!(
            fast_mode_values(&option("fast", &["false", "true"], "false")),
            Some(("true", "false"))
        );
        assert_eq!(
            fast_mode_values(&option("fast", &["false", "true"], "true")),
            Some(("true", "false"))
        );
        assert_eq!(
            fast_mode_values(&option("serviceTier", &["default", "fast"], "fast")),
            Some(("fast", "default"))
        );
        // Not fast mode: other toggles, including Cursor's true/false ones.
        assert_eq!(
            fast_mode_values(&option("thinking", &["off", "on"], "off")),
            None
        );
        assert_eq!(
            fast_mode_values(&option("thinking", &["false", "true"], "true")),
            None
        );
        assert_eq!(
            fast_mode_values(&option("contextWindow", &["200k", "1m"], "200k")),
            None
        );
    }

    #[gpui::test]
    fn open_model_trigger_follows_the_selection_while_open(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            // Settle the width glide in one frame.
            motion::set_reduced_motion(cx, true);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let mut pickers = Pickers::new(state, cx);
            pickers.config.harness = Some(HarnessId::Codex);
            pickers.config.model = Some("short".into());
            pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
            let mut long = bare_model("long", "A longer model name");
            long.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
            long.options.push(ModelOption {
                id: "serviceTier".into(),
                label: "Service tier".into(),
                default_choice: "default".into(),
                choices: vec![
                    ModelOptionChoice {
                        id: "default".into(),
                        label: "Standard".into(),
                    },
                    ModelOptionChoice {
                        id: "fast".into(),
                        label: "Fast".into(),
                    },
                ],
            });
            pickers.models.insert(
                HarnessId::Codex,
                Loadable::Ready(vec![bare_model("short", "Short"), long]),
            );
            pickers
        });
        let draw = |cx: &mut gpui::TestAppContext| {
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            handle
                .read_with(cx, |pickers, _| {
                    pickers.model_trigger_bounds.unwrap().size.width
                })
                .unwrap()
        };
        let short = draw(cx);
        handle
            .update(cx, |pickers, window, cx| {
                pickers.open_model_menu(window, cx)
            })
            .unwrap();
        let mut widths = Vec::new();
        for (model, fast) in [("long", false), ("long", true), ("short", false)] {
            handle
                .update(cx, |pickers, _, cx| {
                    pickers.config.model = Some(model.into());
                    pickers
                        .defaults
                        .model_options_mut(HarnessId::Codex, model)
                        .insert(
                            "serviceTier".into(),
                            if fast { "fast" } else { "default" }.into(),
                        );
                    cx.notify();
                })
                .unwrap();
            widths.push(draw(cx));
        }
        // The open chip names the live selection instead of keeping the
        // width it opened with: longer for the long model, longer again
        // with its effort and fast mark, and back once the short one returns.
        assert!(widths[0] > short, "{widths:?} vs {short:?}");
        assert!(widths[1] > widths[0], "{widths:?}");
        assert_eq!(widths[2], short);
    }

    #[gpui::test]
    fn open_model_card_keeps_height_when_options_change(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        struct Fixture {
            pickers: Entity<Pickers>,
            bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
        }
        impl Render for Fixture {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let measured = self.bounds.clone();
                let menu = self.pickers.update(cx, |pickers, cx| {
                    let content = pickers.render_harness_model_popover(cx);
                    pickers.popover_frame_flush(304.0, content, cx)
                });
                div().size_full().relative().child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w(px(304.0))
                        .child(menu)
                        .child(
                            gpui::canvas(move |bounds, _, _| measured.set(bounds), |_, _, _, _| {})
                                .absolute()
                                .inset_0(),
                        ),
                )
            }
        }
        cx.update(|cx| cx.set_global(Theme::dark()));
        let bounds = Rc::new(Cell::new(gpui::Bounds::default()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let pickers = cx.new(|cx| {
                let mut pickers = Pickers::new(state, cx);
                pickers.config.harness = Some(HarnessId::Codex);
                pickers.config.model = Some("model".into());
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
                pickers.open_model_height = model_menu_height(2);
                pickers.menu_geometry.insert(
                    PickerKind::HarnessModel,
                    popover::MenuGeometry {
                        height: 500.0,
                        below: false,
                    },
                );
                pickers.open.open(PickerKind::HarnessModel);
                pickers
            });
            Fixture {
                pickers,
                bounds: bounds.clone(),
            }
        });
        for available in [500.0, 200.0, 500.0] {
            for settings in [0, 1, 3, 0] {
                handle
                    .update(cx, |fixture, _, cx| {
                        fixture.pickers.update(cx, |pickers, cx| {
                            let mut model = bare_model("model", "Model");
                            model.options = (0..settings)
                                .map(|i| ModelOption {
                                    id: format!("option-{i}"),
                                    label: format!("Option {i}"),
                                    default_choice: "on".into(),
                                    choices: vec![
                                        ModelOptionChoice {
                                            id: "on".into(),
                                            label: "On".into(),
                                        },
                                        ModelOptionChoice {
                                            id: "off".into(),
                                            label: "Off".into(),
                                        },
                                    ],
                                })
                                .collect();
                            pickers
                                .models
                                .insert(HarnessId::Codex, Loadable::Ready(vec![model]));
                            pickers
                                .menu_geometry
                                .get_mut(&PickerKind::HarnessModel)
                                .unwrap()
                                .height = available;
                            cx.notify();
                        });
                        cx.notify();
                    })
                    .unwrap();
                cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                    .unwrap();
                assert!(
                    (f32::from(bounds.get().size.height) - available.min(model_menu_height(2)))
                        .abs()
                        < 0.1,
                    "settings={settings}, available={available}, bounds={:?}",
                    bounds.get()
                );
            }
        }
    }

    #[gpui::test]
    fn picker_completion_and_dismissal_have_distinct_focus_behavior(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;
        cx.update(|cx| cx.set_global(Theme::dark()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let returned = Rc::new(Cell::new(0));
        let observed = returned.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(
                &handle.entity(cx).unwrap(),
                move |_, _: &ReturnComposerFocus, _| observed.set(observed.get() + 1),
            )
        });
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open.open(PickerKind::Checkout);
                pickers.pick_checkout(CheckoutKind::Local, cx);
            })
            .unwrap();
        assert_eq!(returned.get(), 1);
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open.open(PickerKind::HarnessModel);
                pickers.pick_model("test-model".into(), cx);
                assert!(
                    pickers.is_open(),
                    "model options remain available after a selection"
                );
                pickers.dismiss(cx);
            })
            .unwrap();
        assert_eq!(
            returned.get(),
            1,
            "outside clicks must not request composer focus"
        );
        handle
            .update(cx, |pickers, window, cx| {
                pickers.open.open(PickerKind::Checkout);
                pickers.open.note_trigger_press();
                pickers.dismiss(cx); // Capture closes before the trigger's click.
                pickers.toggle(PickerKind::Checkout, window, cx);
            })
            .unwrap();
        assert_eq!(
            returned.get(),
            2,
            "closing via the trigger returns to the composer"
        );
    }

    #[gpui::test]
    fn project_selector_opens_without_a_local_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        for (cloud, no_project, expected) in [
            (false, false, NO_ACTIVE_ROW),
            (true, false, 1),
            (false, true, 0),
        ] {
            let handle = cx.add_window(move |_, cx| {
                let state = cx.new(move |_| {
                    let mut state = AppState::new();
                    state.no_project = no_project;
                    state.cloud_project = cloud.then(|| {
                        serde_json::json!({
                            "projectId":"cloud-only", "displayName":"Cloud repository"
                        })
                    });
                    state
                });
                let mut pickers = Pickers::new(state, cx);
                pickers
                    .cloud
                    .set_project_rows_for_test(vec![serde_json::json!({
                        "projectId":"cloud-only", "displayName":"Cloud repository"
                    })]);
                pickers
            });
            handle
                .update(cx, |pickers, window, cx| {
                    pickers.toggle(PickerKind::Space, window, cx);
                    assert_eq!(pickers.open_kind(), Some(PickerKind::Space));
                    assert_eq!(
                        pickers.active, expected,
                        "cloud={cloud}, no_project={no_project}"
                    );
                })
                .unwrap();
            cx.run_until_parked();
            handle
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    fn projectless_picker_clears_checkout_and_supports_keyboard_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![Space {
                id: "repo".into(),
                device_id: "local".into(),
                remote_repository: None,
                path: "/repo".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                repository_id: None,
                created_at: chrono::Utc::now(),
            }]);
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, cx| {
            pickers.config.branch = Some("old-branch".into());
            pickers.config.checkout = CheckoutKind::NewWorktree;
            pickers.open.open(PickerKind::Space);
            pickers.active = 0; // The explicit No project row precedes project choices.
            pickers.on_search_submit(cx);
        });
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            assert!(pickers.config.branch.is_none());
            assert_eq!(pickers.config.checkout, CheckoutKind::default());
            assert!(pickers.defaults.no_project);
            assert!(pickers.state.read(cx).auto_selected);
            assert!(pickers.defaults.project.is_none());
            assert_eq!(pickers.selected_project_index(cx), 1);
        });
        state.update(cx, |state, cx| state.select_device("remote".into(), cx));
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            assert_eq!(pickers.space_target(cx).as_deref(), Some("remote"));
            // Projects list across devices; the opt-out row stays picked.
            assert_eq!(pickers.selected_project_index(cx), 1);
            // Only the device switch re-targets the per-device catalogs; the
            // same-device projectless pick kept them.
            assert_eq!(pickers.target_generation, 1);
        });
    }

    fn git_space(id: &str, device: &str) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            remote_repository: None,
            path: format!("/{device}/{id}"),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            repository_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn repo_ref(name: &str, current: bool, worktree: Option<&str>) -> RepoRef {
        RepoRef {
            name: name.into(),
            current,
            worktree_path: worktree.map(str::to_string),
        }
    }

    #[gpui::test]
    fn project_switch_on_one_device_keeps_the_model_catalog(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![git_space("a", "local"), git_space("b", "local")]);
            state.selected_space = Some("a".into());
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults = ComposerDefaults::default();
            pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
            pickers.apply_model_catalog(
                HarnessId::Codex,
                Loadable::Ready(vec![bare_model("gpt", "GPT")]),
                cx,
            );
            assert_eq!(pickers.model_name(cx), ModelName::Named("GPT".into()));
        });
        state.update(cx, |state, cx| state.select_space(Some("b".into()), cx));
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            // Same host: the catalog stands, the chip never blanks.
            assert_eq!(pickers.model_name(cx), ModelName::Named("GPT".into()));
            assert!(matches!(pickers.harnesses, Loadable::Ready(_)));
            assert!(!pickers.harnesses_stale && pickers.stale_models.is_empty());
        });
    }

    #[gpui::test]
    fn device_switch_revalidates_the_catalog_without_blanking(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![git_space("a", "local"), git_space("b", "remote")]);
            state.selected_space = Some("a".into());
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults = ComposerDefaults::default();
            // A picked harness keeps the chip naming its model throughout.
            pickers.config.harness = Some(HarnessId::Codex);
            pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
            pickers.apply_model_catalog(
                HarnessId::Codex,
                Loadable::Ready(vec![bare_model("gpt", "GPT")]),
                cx,
            );
            pickers
                .models
                .insert(HarnessId::ClaudeCode, Loadable::Loading);
        });
        state.update(cx, |state, cx| state.select_space(Some("b".into()), cx));
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            // Old rows stay on screen, marked for one reload from the new host;
            // a load in flight for the old host restarts instead.
            assert_eq!(pickers.model_name(cx), ModelName::Named("GPT".into()));
            assert!(pickers.harnesses_stale);
            assert_eq!(pickers.stale_models, HashSet::from([HarnessId::Codex]));
            assert!(!pickers.models.contains_key(&HarnessId::ClaudeCode));
            // The old device's catalog never picks for the new one: with no
            // explicit or remembered model, a send leaves the choice to the
            // new device instead of carrying the old catalog's default.
            assert_eq!(pickers.resolved(cx).model, None);
            // A failed reload replaces the old device's rows — it is not
            // retained like a same-device refresh failure.
            pickers.land_model_reload(
                HarnessId::Codex,
                Loadable::Error("device offline".into()),
                cx,
            );
            assert!(pickers.stale_models.is_empty());
            assert!(matches!(
                pickers.models.get(&HarnessId::Codex),
                Some(Loadable::Error(_))
            ));
            assert_ne!(pickers.model_name(cx), ModelName::Named("GPT".into()));
            // With nothing picked or remembered, the old device's harness list
            // doesn't choose either — for the chip or a send.
            pickers.config.harness = None;
            assert!(pickers.harnesses_stale);
            assert_eq!(pickers.effective_harness(cx), None);
            assert_eq!(pickers.resolved(cx).harness, None);
            // ...and the chip reads as loading, never blank.
            assert_eq!(pickers.model_name(cx), ModelName::Loading);
        });
    }

    #[gpui::test]
    fn same_device_refresh_failure_keeps_the_loaded_rows(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![git_space("a", "local")]);
            state.selected_space = Some("a".into());
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults = ComposerDefaults::default();
            pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
            pickers.apply_model_catalog(
                HarnessId::Codex,
                Loadable::Ready(vec![bare_model("gpt", "GPT")]),
                cx,
            );
            pickers.land_model_reload(HarnessId::Codex, Loadable::Error("flaky".into()), cx);
            assert_eq!(pickers.model_name(cx), ModelName::Named("GPT".into()));
            assert!(matches!(
                pickers.models.get(&HarnessId::Codex),
                Some(Loadable::Ready(_))
            ));
        });
    }

    #[gpui::test]
    fn project_switch_clears_ref_actions_until_current_refs_land(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![git_space("a", "local"), git_space("b", "local")]);
            state.selected_space = Some("a".into());
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, _cx| {
            pickers.config.checkout = CheckoutKind::Local;
            pickers.defaults.workspace_by_project.insert(
                "b".into(),
                zuse_protocol::presentation::WorkspaceMode::Local,
            );
            pickers.refs = Loadable::Ready(vec![
                repo_ref("main", false, None),
                repo_ref("feature", true, Some("/wt/feature")),
            ]);
            pickers.refs_space = Some("a".into());
            assert_eq!(pickers.effective_ref_name().as_deref(), Some("feature"));
            assert_eq!(pickers.checkout_label(), "Local");
        });
        state.update(cx, |state, cx| state.select_space(Some("b".into()), cx));
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            // Ref actions must wait for the new project's own refs to load.
            assert!(matches!(pickers.refs, Loadable::Idle));
            assert_eq!(pickers.effective_ref_name(), None);
            assert_eq!(pickers.checkout_label(), "Local");
            // Nothing that acts on refs sees the previous project's rows.
            assert!(pickers.filtered_ref_rows(cx).is_empty());
            assert_eq!(
                pickers.checkout_plan(cx),
                CheckoutPlan::CurrentCheckout { branch: None }
            );
            pickers.refs = Loadable::Ready(vec![repo_ref("trunk", true, None)]);
            assert_eq!(pickers.effective_ref_name().as_deref(), Some("trunk"));
            assert_eq!(pickers.checkout_label(), "Local");
        });
    }

    #[gpui::test]
    fn project_picker_lists_repositories_then_their_devices(cx: &mut gpui::TestAppContext) {
        let space = |id: &str, device: &str, name: &str, repository: Option<&str>, minutes| Space {
            id: id.into(),
            device_id: device.into(),
            remote_repository: None,
            path: format!("/{device}/{id}"),
            name: Some(name.into()),
            git_detected: repository.is_some(),
            git_checked_at: None,
            checkout_id: None,
            repository_id: repository.map(str::to_string),
            created_at: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH
                + chrono::TimeDelta::minutes(minutes),
        };
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("mac".into());
            state.devices = ["mac", "vps"]
                .into_iter()
                .map(|id| {
                    serde_json::from_value(serde_json::json!({
                        "id": id, "name": id.to_uppercase(), "platform": "macos", "lastSeenAt": null
                    }))
                    .unwrap()
                })
                .collect();
            state.apply_spaces(vec![
                space("notes", "mac", "notes", None, 0),
                space("server", "vps", "zuse", Some("github.com/o/zuse"), 1),
                space("laptop", "mac", "zuse-laptop", Some("github.com/o/zuse"), 2),
                space("server-wt", "vps", "zuse-wt", Some("github.com/o/zuse"), 3),
            ]);
            state.selected_space = Some("notes".into());
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, cx| {
            // One row per repository, named for its oldest checkout, listed
            // alphabetically.
            let rows: Vec<(String, String)> = pickers
                .project_rows(cx)
                .into_iter()
                .map(|row| (row.name, row.key))
                .collect();
            assert_eq!(
                rows,
                [
                    ("notes".into(), "notes".into()),
                    ("zuse".into(), "repo:github.com/o/zuse".into())
                ]
            );
            // Picking it keeps the canvas on this device's checkout.
            pickers.pick_project("repo:github.com/o/zuse".into(), cx);
            assert_eq!(
                pickers.state.read(cx).selected_space.as_deref(),
                Some("laptop")
            );
            // "zuse" is the second row once rows are alphabetical.
            assert_eq!(pickers.selected_project_index(cx), 1);
            // The device picker offers only the project's checkouts; a device
            // holding two is told apart by path.
            let devices: Vec<(String, Option<String>)> = pickers
                .device_rows(cx)
                .into_iter()
                .map(|row| (row.device_id, row.detail))
                .collect();
            assert_eq!(
                devices,
                [
                    ("mac".into(), None),
                    ("vps".into(), Some("/vps/server".into())),
                    ("vps".into(), Some("/vps/server-wt".into())),
                ]
            );
            let server = pickers.device_rows(cx).remove(1);
            pickers.pick_device_row(server, cx);
            assert_eq!(
                pickers.state.read(cx).selected_space.as_deref(),
                Some("server")
            );
            assert_eq!(pickers.selected_device_index(cx), 1);
            // Without a project, every device is offered.
            pickers.pick_no_project(cx);
            let devices: Vec<(String, bool)> = pickers
                .device_rows(cx)
                .into_iter()
                .map(|row| (row.device_id, row.space.is_some()))
                .collect();
            assert_eq!(devices, [("mac".into(), false), ("vps".into(), false)]);
        });
        // A device switch keeps the project when the device has a checkout.
        state.update(cx, |state, cx| {
            state.select_space(Some("laptop".into()), cx);
            state.select_device("vps".into(), cx);
            assert_eq!(state.selected_space.as_deref(), Some("server"));
        });
    }

    fn bare_model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }
    }

    #[gpui::test]
    fn nested_model_settings_navigate_and_preserve_independent_choices(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            crate::composer::init(cx, Default::default());
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |pickers, window, cx| {
                let mut model = bare_model("opus", "Opus");
                model.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
                model.options = ["contextWindow", "serviceTier"]
                    .map(|id| ModelOption {
                        id: id.into(),
                        label: id.into(),
                        default_choice: "standard".into(),
                        choices: ["standard", "extended"]
                            .map(|id| ModelOptionChoice {
                                id: id.into(),
                                label: id.into(),
                            })
                            .into(),
                    })
                    .into();
                pickers.config.harness = Some(HarnessId::ClaudeCode);
                pickers.harnesses =
                    Loadable::Ready(vec![descriptor(HarnessId::ClaudeCode, "Claude Code")]);
                pickers.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![model, bare_model("haiku", "Haiku")]),
                );
                pickers.pick_model("opus".into(), cx);
                pickers.open.open(PickerKind::HarnessModel);
                window.focus(&pickers.focus, cx);
                assert_eq!(pickers.setting_groups(cx).len(), 3);
                let key = |key: &str| KeyDownEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                pickers.active = pickers.model_rows_len(cx);
                pickers.on_key_down(&key("right"), window, cx);
                assert_eq!(pickers.setting_menu, Some(ModelSetting::Reasoning));
                pickers.on_key_down(&key("up"), window, cx);
                pickers.on_key_down(&key("enter"), window, cx);
                assert_eq!(pickers.effective_reasoning(cx), Some(ReasoningLevel::Low));
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.is_open());
                for id in ["contextWindow", "serviceTier"] {
                    pickers.open_setting(SettingScope::Tray, ModelSetting::Option(id.into()), cx);
                    assert_eq!(pickers.setting_active, 0);
                    pickers.on_key_down(&key("down"), window, cx);
                    pickers.on_key_down(&key("enter"), window, cx);
                    assert_eq!(pickers.resolved(cx).model_options[id], "extended");
                }
                // Returning to one setting keeps its choice, and restoring its
                // default does not reset a sibling option or close the picker.
                pickers.open_setting(
                    SettingScope::Tray,
                    ModelSetting::Option("contextWindow".into()),
                    cx,
                );
                assert_eq!(pickers.setting_active, 1);
                pickers.on_key_down(&key("up"), window, cx);
                pickers.on_key_down(&key("enter"), window, cx);
                assert!(
                    !pickers
                        .resolved(cx)
                        .model_options
                        .contains_key("contextWindow")
                );
                assert_eq!(
                    pickers.resolved(cx).model_options["serviceTier"],
                    "extended"
                );
                pickers.open_setting(SettingScope::Tray, ModelSetting::Reasoning, cx);
                pickers.on_key_down(&key("escape"), window, cx);
                assert!(pickers.is_open());
                assert!(pickers.setting_menu.is_none());
                pickers.open_setting(SettingScope::Tray, ModelSetting::Reasoning, cx);
                pickers.pick_model("haiku".into(), cx);
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.setting_groups(cx).is_empty());
            })
            .unwrap();
        for setting in [
            ModelSetting::Reasoning,
            ModelSetting::Option("contextWindow".into()),
            ModelSetting::Option("serviceTier".into()),
        ] {
            handle
                .update(cx, |pickers, _, cx| {
                    pickers.pick_model("opus".into(), cx);
                    pickers.open_setting(SettingScope::Tray, setting, cx);
                })
                .unwrap();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
        }
        handle
            .update(cx, |pickers, window, cx| {
                pickers.setting_menu = None;
                pickers.active = 0;
                window.focus(&pickers.search.read(cx).focus_handle(cx), cx);
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "down down right");
        handle
            .read_with(cx, |pickers, _| {
                assert_eq!(pickers.setting_menu, Some(ModelSetting::Reasoning))
            })
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "down enter");
        handle
            .read_with(cx, |pickers, cx| {
                assert_eq!(pickers.effective_reasoning(cx), Some(ReasoningLevel::High));
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.is_open());
            })
            .unwrap();
    }

    /// Devin Fusion: one row configured in place. Its hover card lists Lead,
    /// Effort, Sidekick and a Fast Mode switch; any choice made there selects
    /// Fusion and applies to it.
    #[gpui::test]
    fn fusion_card_configures_and_selects_the_model(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            crate::composer::init(cx, Default::default());
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let option = |id: &str, label: &str, choices: &[(&str, &str)]| ModelOption {
            id: id.into(),
            label: label.into(),
            default_choice: choices[0].0.into(),
            choices: choices
                .iter()
                .map(|(id, label)| ModelOptionChoice {
                    id: (*id).into(),
                    label: (*label).into(),
                })
                .collect(),
        };
        handle
            .update(cx, |pickers, window, cx| {
                let mut fusion = bare_model("fusion", "Fusion");
                fusion.reasoning_levels = vec![
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ];
                fusion.options = vec![
                    option(
                        "lead",
                        "Lead",
                        &[
                            ("claude-fable-5-1", "Claude Fable 5.1"),
                            ("gpt-6-sol", "GPT-6 Sol"),
                        ],
                    ),
                    option(
                        "sidekick",
                        "Sidekick",
                        &[
                            ("swe-2-medium", "SWE-2 Medium"),
                            ("swe-2-high", "SWE-2 High"),
                        ],
                    ),
                    option(
                        "speed",
                        "Fast Mode",
                        &[("standard", "Standard"), ("fast", "Fast")],
                    ),
                ];
                pickers.config.harness = Some(HarnessId::Devin);
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Devin, "Devin")]);
                pickers.models.insert(
                    HarnessId::Devin,
                    Loadable::Ready(vec![
                        bare_model("adaptive", "Adaptive"),
                        fusion,
                        bare_model("swe-2-high", "SWE-2"),
                    ]),
                );
                pickers.pick_model("adaptive".into(), cx);
                pickers.open.open(PickerKind::HarnessModel);
                window.focus(&pickers.focus, cx);
                // Only Fusion is configured in place.
                let rows = pickers.model_rows(cx);
                let configured: Vec<_> =
                    rows.iter().map(|r| configured_in_place(&r.model)).collect();
                assert_eq!(configured, [false, true, false]);

                pickers.open_config(1, cx);
                let labels: Vec<_> = pickers
                    .card_groups(cx)
                    .iter()
                    .map(|g| g.label.clone())
                    .collect();
                assert_eq!(labels, ["Lead", "Effort", "Sidekick", "Fast Mode"]);
                // Nothing is picked just by looking.
                assert_eq!(pickers.resolved(cx).model.as_deref(), Some("adaptive"));

                pickers.open_setting(SettingScope::Card, ModelSetting::Option("lead".into()), cx);
                pickers.setting_active = 1;
                pickers.activate_setting_choice(cx);
                assert_eq!(pickers.resolved(cx).model.as_deref(), Some("fusion"));
                assert_eq!(pickers.resolved(cx).model_options["lead"], "gpt-6-sol");

                pickers.open_setting(SettingScope::Card, ModelSetting::Reasoning, cx);
                pickers.setting_active = 3;
                pickers.activate_setting_choice(cx);
                assert_eq!(pickers.effective_reasoning(cx), Some(ReasoningLevel::XHigh));

                let fast = pickers
                    .card_groups(cx)
                    .into_iter()
                    .find(|g| g.label == "Fast Mode")
                    .unwrap();
                assert!(is_toggle(&fast));
                pickers.toggle_card_option(fast, cx);
                let resolved = pickers.resolved(cx);
                assert_eq!(resolved.model_options["speed"], "fast");
                assert_eq!(resolved.model_options["lead"], "gpt-6-sol");

                // A card flyout is not the tray's: the tray row with the same
                // setting stays closed while the card's is open.
                pickers.open_setting(
                    SettingScope::Card,
                    ModelSetting::Option("sidekick".into()),
                    cx,
                );
                assert_eq!(pickers.setting_scope, SettingScope::Card);
                pickers.close_config();
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.config_row.is_none());
            })
            .unwrap();
        // Both the card and its choice list render.
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open_config(1, cx);
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open_setting(SettingScope::Card, ModelSetting::Option("lead".into()), cx);
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .read_with(cx, |pickers, _| {
                assert_eq!(pickers.config_row, Some(1));
                assert_eq!(
                    pickers.setting_menu,
                    Some(ModelSetting::Option("lead".into()))
                );
            })
            .unwrap();
    }

    /// Pointer paths through the Fusion card: the cone toward a card row's
    /// choice list keeps both open, a pick keeps the card, and the selected
    /// Fusion's settings live only in its card (not the tray).
    #[gpui::test]
    fn fusion_card_mouse_paths(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        struct CardFixture {
            pickers: Entity<Pickers>,
            bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
        }
        impl Render for CardFixture {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let measured = self.bounds.clone();
                let menu = self.pickers.update(cx, |pickers, cx| {
                    let content = pickers.render_harness_model_popover(cx);
                    pickers.popover_frame_flush(304.0, content, cx)
                });
                div().size_full().relative().child(
                    div()
                        .absolute()
                        .top(px(60.0))
                        .left(px(32.0))
                        .w(px(304.0))
                        .child(menu)
                        .child(
                            gpui::canvas(move |bounds, _, _| measured.set(bounds), |_, _, _, _| {})
                                .absolute()
                                .inset_0(),
                        ),
                )
            }
        }
        cx.update(|cx| cx.set_global(Theme::dark()));
        let measured = Rc::new(Cell::new(gpui::Bounds::default()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let pickers = cx.new(|cx| Pickers::new(state, cx));
            pickers.update(cx, |pickers, cx| {
                let option = |id: &str, label: &str, choices: &[(&str, &str)]| ModelOption {
                    id: id.into(),
                    label: label.into(),
                    default_choice: choices[0].0.into(),
                    choices: choices
                        .iter()
                        .map(|(id, label)| ModelOptionChoice {
                            id: (*id).into(),
                            label: (*label).into(),
                        })
                        .collect(),
                };
                let mut fusion = bare_model("fusion", "Fusion");
                fusion.reasoning_levels = vec![ReasoningLevel::Medium, ReasoningLevel::High];
                fusion.options = vec![
                    option(
                        "lead",
                        "Lead",
                        &[("a", "Lead A"), ("b", "Lead B"), ("c", "Lead C")],
                    ),
                    option("sidekick", "Sidekick", &[("x", "Side X"), ("y", "Side Y")]),
                    option(
                        "speed",
                        "Fast Mode",
                        &[("standard", "Standard"), ("fast", "Fast")],
                    ),
                ];
                pickers.config.harness = Some(HarnessId::Devin);
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Devin, "Devin")]);
                pickers.models.insert(
                    HarnessId::Devin,
                    Loadable::Ready(vec![bare_model("adaptive", "Adaptive"), fusion]),
                );
                pickers.open.open(PickerKind::HarnessModel);
                pickers.pick_model("adaptive".into(), cx);
            });
            CardFixture {
                pickers,
                bounds: measured.clone(),
            }
        });
        let pickers = handle
            .read_with(cx, |fixture, _| fixture.pickers.clone())
            .unwrap();
        let hover = |window: &mut Window, cx: &mut App, position| {
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position,
                    ..Default::default()
                }),
                cx,
            );
            window.draw(cx).clear();
            window.draw(cx).clear();
        };
        let click = |window: &mut Window, cx: &mut App, position| {
            for input in [
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
            ] {
                window.dispatch_event(input, cx);
            }
            window.draw(cx).clear();
            window.draw(cx).clear();
        };
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        let parent = measured.get();
        // Find the Fusion row by hovering down the list.
        let mut row = None;
        for step in 0..60 {
            let at = gpui::point(
                parent.left() + px(60.0),
                parent.top() + px(4.0 * step as f32),
            );
            cx.update_window(handle.into(), |_, window, cx| hover(window, cx, at))
                .unwrap();
            if pickers.read_with(cx, |p, _| p.config_row) == Some(1) {
                row = Some(at);
                break;
            }
        }
        row.expect("hovering the Fusion row opens its card");
        let card = pickers
            .read_with(cx, |p, _| p.config_bounds)
            .expect("card measured");
        assert!(card.left() > parent.right(), "card opens beside the menu");
        // Find the Lead trigger inside the card.
        let mut lead = None;
        for step in 0..40 {
            let at = gpui::point(card.left() + px(40.0), card.top() + px(3.0 * step as f32));
            cx.update_window(handle.into(), |_, window, cx| hover(window, cx, at))
                .unwrap();
            if pickers.read_with(cx, |p, _| p.setting_menu.clone())
                == Some(ModelSetting::Option("lead".into()))
            {
                lead = Some(at);
                break;
            }
        }
        let lead = lead.expect("hovering Lead opens its choices");
        let list = pickers
            .read_with(cx, |p, _| p.setting_bounds)
            .expect("choices measured");
        assert!(list.left() > card.right());
        // Diagonal path from the Lead row to the bottom of its list: through
        // the gap and below the Lead row — inside the cone, not the card.
        let target = gpui::point(list.left() + px(12.0), list.bottom() - px(10.0));
        for t in 1..=10 {
            let f = t as f32 / 10.0;
            let at = gpui::point(
                lead.x + (target.x - lead.x) * f,
                lead.y + (target.y - lead.y) * f,
            );
            cx.update_window(handle.into(), |_, window, cx| hover(window, cx, at))
                .unwrap();
            pickers.read_with(cx, |p, _| {
                assert_eq!(p.config_row, Some(1), "card closed at step {t}");
                assert_eq!(
                    p.setting_menu,
                    Some(ModelSetting::Option("lead".into())),
                    "choices closed at step {t}"
                );
            });
        }
        // Pick the last choice: Fusion is selected, the list closes, the
        // card stays — also while the pointer rests where the list was.
        cx.update_window(handle.into(), |_, window, cx| {
            click(window, cx, target);
            hover(window, cx, target + gpui::point(px(3.0), px(-2.0)));
        })
        .unwrap();
        pickers.read_with(cx, |p, cx| {
            assert_eq!(p.resolved(cx).model.as_deref(), Some("fusion"));
            assert_eq!(p.resolved(cx).model_options["lead"], "c");
            assert!(p.setting_menu.is_none());
            assert_eq!(p.config_row, Some(1), "a pick keeps the card open");
            // Selected Fusion keeps its settings out of the tray.
            assert!(p.setting_groups(cx).is_empty());
            assert!(!p.card_groups(cx).is_empty());
        });
        // Back into the card, then far away: the card closes.
        cx.update_window(handle.into(), |_, window, cx| {
            hover(window, cx, card.center());
            hover(window, cx, gpui::point(px(4.0), px(4.0)));
        })
        .unwrap();
        pickers.read_with(cx, |p, _| assert!(p.config_row.is_none()));
    }

    #[gpui::test]
    fn nested_model_menu_mouse_paths_work_on_both_sides(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        struct MouseFixture {
            pickers: Entity<Pickers>,
            on_left: bool,
            bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
        }
        impl Render for MouseFixture {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let measured = self.bounds.clone();
                let menu = self.pickers.update(cx, |pickers, cx| {
                    let content = pickers.render_harness_model_popover(cx);
                    pickers.popover_frame_flush(304.0, content, cx)
                });
                div().size_full().relative().child(
                    div()
                        .absolute()
                        .top(px(100.0))
                        .w(px(304.0))
                        .when(self.on_left, |el| el.right(px(32.0)))
                        .when(!self.on_left, |el| el.left(px(32.0)))
                        .child(menu)
                        .child(
                            gpui::canvas(move |bounds, _, _| measured.set(bounds), |_, _, _, _| {})
                                .absolute()
                                .inset_0(),
                        ),
                )
            }
        }
        cx.update(|cx| cx.set_global(Theme::dark()));
        for on_left in [false, true] {
            let measured = Rc::new(Cell::new(gpui::Bounds::default()));
            let handle = cx.add_window(|_, cx| {
                let state = cx.new(|_| AppState::new());
                let pickers = cx.new(|cx| Pickers::new(state, cx));
                pickers.update(cx, |pickers, cx| {
                    let mut model = bare_model("test", "Test model");
                    model.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
                    // Effort has its own control, so the tray lists only
                    // options; three choices keep each a nested menu rather
                    // than a switch.
                    for (id, label, choices) in [
                        ("contextWindow", "Context window", ["200k", "500k", "1m"]),
                        ("tier", "Tier", ["low", "standard", "high"]),
                    ] {
                        model.options.push(ModelOption {
                            id: id.into(),
                            label: label.into(),
                            choices: choices
                                .map(|choice| ModelOptionChoice {
                                    id: choice.into(),
                                    label: choice.into(),
                                })
                                .into(),
                            default_choice: choices[0].into(),
                        });
                    }
                    pickers.config.harness = Some(HarnessId::ClaudeCode);
                    pickers.harnesses =
                        Loadable::Ready(vec![descriptor(HarnessId::ClaudeCode, "Claude Code")]);
                    pickers
                        .models
                        .insert(HarnessId::ClaudeCode, Loadable::Ready(vec![model]));
                    pickers.open.open(PickerKind::HarnessModel);
                    pickers.pick_model("test".into(), cx);
                });
                MouseFixture {
                    pickers,
                    on_left,
                    bounds: measured.clone(),
                }
            });
            let pickers = handle
                .read_with(cx, |fixture, _| fixture.pickers.clone())
                .unwrap();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            let parent = measured.get();
            let click = |window: &mut Window, cx: &mut App, position| {
                window.dispatch_event(
                    gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position,
                        ..Default::default()
                    }),
                    cx,
                );
                window.dispatch_event(
                    gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        click_count: 1,
                        ..Default::default()
                    }),
                    cx,
                );
                window.dispatch_event(
                    gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        click_count: 1,
                        ..Default::default()
                    }),
                    cx,
                );
            };
            let hover = |window: &mut Window, cx: &mut App, position| {
                window.dispatch_event(
                    gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position,
                        ..Default::default()
                    }),
                    cx,
                );
                window.draw(cx).clear();
                window.draw(cx).clear();
            };
            // The tray's first trigger: probe down the card for the first row
            // that opens a menu, then rest inside it. Effort and the tray sit
            // at the bottom, so this stays correct as their heights change.
            let mut probe = f32::from(parent.top());
            let first_row = loop {
                assert!(probe < f32::from(parent.bottom()), "no tray trigger found");
                let at = gpui::point(parent.center().x, px(probe));
                cx.update_window(handle.into(), |_, window, cx| hover(window, cx, at))
                    .unwrap();
                if pickers.read_with(cx, |pickers, _| pickers.setting_menu.is_some()) {
                    break probe;
                }
                probe += 2.0;
            };
            let trigger = gpui::point(parent.center().x, px(first_row + 8.0));
            // The provider rail sits inside the card's left edge, so the tray
            // row (and the child anchored to it) starts past the card edge.
            // Probe in from the open side for the row's actual edge.
            let mut edge = if on_left {
                f32::from(parent.left()) + 2.0
            } else {
                f32::from(parent.right()) - 2.0
            };
            loop {
                assert!(
                    edge > f32::from(parent.left()) && edge < f32::from(parent.right()),
                    "no tray row edge found"
                );
                let at = gpui::point(px(edge), trigger.y);
                cx.update_window(handle.into(), |_, window, cx| {
                    hover(window, cx, gpui::point(px(8.0), px(8.0)));
                    hover(window, cx, at)
                })
                .unwrap();
                if pickers.read_with(cx, |pickers, _| pickers.setting_menu.is_some()) {
                    break;
                }
                edge += if on_left { 2.0 } else { -2.0 };
            }
            let row_edge = px(edge);
            cx.update_window(handle.into(), |_, window, cx| {
                hover(window, cx, gpui::point(px(8.0), px(8.0)));
                hover(window, cx, trigger);
                window.draw(cx).clear();
                window.draw(cx).clear();
            })
            .unwrap();
            let submenu = pickers.read_with(cx, |pickers, _| {
                assert_eq!(
                    pickers.setting_menu,
                    Some(ModelSetting::Option("contextWindow".into()))
                );
                assert_eq!(pickers.setting_on_left, on_left);
                pickers
                    .setting_bounds
                    .expect("submenu measured after opening")
            });
            if on_left {
                assert!(submenu.right() < row_edge);
            } else {
                assert!(submenu.left() > row_edge);
            }
            // Clicking the hovered trigger dismisses its child and keeps it closed.
            cx.update_window(handle.into(), |_, window, cx| {
                click(window, cx, trigger);
                window.draw(cx).clear();
            })
            .unwrap();
            pickers.read_with(cx, |pickers, _| {
                assert!(
                    pickers.setting_menu.is_none(),
                    "trigger click dismisses the hovered submenu"
                );
                assert!(pickers.is_open());
            });
            cx.update_window(handle.into(), |_, window, cx| {
                // Moving inside the same hovered row must not reopen it.
                hover(window, cx, trigger + gpui::point(px(2.0), px(0.0)));
                assert!(pickers.read(cx).setting_menu.is_none());
                hover(window, cx, gpui::point(px(8.0), px(8.0)));
                hover(window, cx, trigger);
            })
            .unwrap();
            // Cross the neighboring trigger diagonally toward the open child.
            // It must not steal the menu while the pointer is inside the cone.
            let diagonal = gpui::point(
                if on_left {
                    row_edge + px(8.0)
                } else {
                    row_edge - px(8.0)
                },
                trigger.y + px(32.0),
            );
            cx.update_window(handle.into(), |_, window, cx| {
                hover(window, cx, diagonal);
                assert_eq!(
                    pickers.read(cx).setting_menu,
                    Some(ModelSetting::Option("contextWindow".into()))
                );
                assert!(pickers.read(cx).setting_hover.pending().is_some());
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                hover(
                    window,
                    cx,
                    diagonal + gpui::point(px(if on_left { -3.0 } else { 3.0 }), px(0.0)),
                );
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| {
                assert_eq!(
                    pickers.read(cx).setting_menu,
                    Some(ModelSetting::Option("contextWindow".into())),
                    "forward progress renews grace"
                );
                hover(window, cx, submenu.center());
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            cx.run_until_parked();
            pickers.read_with(cx, |pickers, _| {
                assert_eq!(
                    pickers.setting_menu,
                    Some(ModelSetting::Option("contextWindow".into()))
                );
                assert!(pickers.setting_hover.pending().is_none());
            });
            // Resting on the sibling expresses intent to switch after grace.
            cx.update_window(handle.into(), |_, window, cx| {
                hover(window, cx, trigger);
                hover(window, cx, diagonal);
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(350));
            cx.run_until_parked();
            pickers.read_with(cx, |pickers, _| {
                assert_eq!(
                    pickers.setting_menu,
                    Some(ModelSetting::Option("tier".into()))
                );
            });
            // Moving away from the cone switches immediately. A click during
            // the grace period dismisses instead, with no delayed reopening.
            cx.update_window(handle.into(), |_, window, cx| {
                hover(window, cx, trigger);
                hover(window, cx, diagonal);
                hover(window, cx, trigger + gpui::point(px(0.0), px(32.0)));
                assert_eq!(
                    pickers.read(cx).setting_menu,
                    Some(ModelSetting::Option("tier".into()))
                );
                hover(window, cx, trigger);
                hover(window, cx, diagonal);
                click(window, cx, diagonal);
                window.draw(cx).clear();
            })
            .unwrap();
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(400));
            cx.run_until_parked();
            pickers.read_with(cx, |pickers, _| {
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.setting_hover.pending().is_none());
                assert!(pickers.is_open());
            });
            cx.update_window(handle.into(), |_, window, cx| hover(window, cx, trigger))
                .unwrap();
            let gap_x = if on_left {
                (submenu.right() + row_edge) / 2.0
            } else {
                (row_edge + submenu.left()) / 2.0
            };
            // Leaving the safe corridor closes the child and clears its
            // trigger selection, while preserving the parent picker.
            for outside in [
                gpui::point(gap_x, submenu.bottom() + px(20.0)),
                gpui::point(parent.center().x, parent.top() + px(60.0)),
                gpui::point(px(8.0), px(8.0)),
            ] {
                cx.update_window(handle.into(), |_, window, cx| {
                    hover(window, cx, outside);
                    assert!(pickers.read(cx).setting_menu.is_none());
                    assert_eq!(pickers.read(cx).active, 0);
                    assert!(pickers.read(cx).is_open());
                    hover(window, cx, trigger);
                    assert_eq!(
                        pickers.read(cx).setting_menu,
                        Some(ModelSetting::Option("contextWindow".into()))
                    );
                })
                .unwrap();
            }
            // Moving through the gap into the child remains safe.
            let target = gpui::point(
                submenu.left() + px(30.0),
                submenu.bottom() - px(popover::CARD_INSET + 30.0 + popover::MENU_GAP + 15.0),
            );
            for position in [gpui::point(gap_x, trigger.y + px(16.0)), target] {
                cx.update_window(handle.into(), |_, window, cx| {
                    window.dispatch_event(
                        gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                            position,
                            ..Default::default()
                        }),
                        cx,
                    );
                    window.draw(cx).clear();
                })
                .unwrap();
                cx.executor().advance_clock(Duration::from_secs(2));
                cx.run_until_parked();
                pickers.read_with(cx, |pickers, _| {
                    assert!(pickers.is_open());
                    assert_eq!(
                        pickers.setting_menu,
                        Some(ModelSetting::Option("contextWindow".into()))
                    );
                });
            }
            cx.update_window(handle.into(), |_, window, cx| click(window, cx, target))
                .unwrap();
            pickers.read_with(cx, |pickers, cx| {
                assert_eq!(
                    pickers.explicit_options(cx).get("contextWindow"),
                    Some(&serde_json::json!("500k"))
                );
                assert!(pickers.setting_menu.is_none());
                assert!(pickers.is_open());
            });
            // Hover switches directly between sibling triggers without clicking.
            cx.update_window(handle.into(), |_, window, cx| {
                window.draw(cx).clear();
                hover(window, cx, trigger);
                hover(window, cx, trigger + gpui::point(px(0.0), px(32.0)));
                window.draw(cx).clear();
                window.draw(cx).clear();
            })
            .unwrap();
            pickers.read_with(cx, |pickers, _| {
                assert_eq!(
                    pickers.setting_menu,
                    Some(ModelSetting::Option("tier".into()))
                );
                assert!(pickers.is_open());
            });
            // Clicking the parent's search closes only the child and focuses
            // the input on that same click, rather than swallowing the press.
            cx.update_window(handle.into(), |_, window, cx| {
                // The search field sits 10px inside the card's top edge and is
                // 32px tall.
                click(
                    window,
                    cx,
                    gpui::point(parent.center().x, parent.top() + px(26.0)),
                );
                pickers.read_with(cx, |pickers, cx| {
                    assert!(pickers.setting_menu.is_none());
                    assert!(pickers.is_open());
                    assert!(pickers.search.read(cx).focus_handle(cx).is_focused(window));
                });
                window.draw(cx).clear();
                hover(window, cx, trigger);
                window.draw(cx).clear();
                window.draw(cx).clear();
            })
            .unwrap();
            pickers.read_with(cx, |pickers, _| assert!(pickers.setting_menu.is_some()));
            cx.update_window(handle.into(), |_, window, cx| {
                click(window, cx, gpui::point(px(8.0), px(8.0)))
            })
            .unwrap();
            pickers.read_with(cx, |pickers, _| assert!(!pickers.is_open()));
        }
    }

    #[gpui::test]
    fn remembered_options_stay_valid_for_the_sent_model_without_a_catalog(
        cx: &mut gpui::TestAppContext,
    ) {
        let one_m = serde_json::Value::String("1m".into());
        let mut opus = bare_model("opus", "Opus");
        opus.options.push(ModelOption {
            id: "contextWindow".into(),
            label: "Context window".into(),
            choices: ["200k", "1m"]
                .map(|id| ModelOptionChoice {
                    id: id.into(),
                    label: id.into(),
                })
                .into(),
            default_choice: "200k".into(),
        });
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults.harness = Some(HarnessId::ClaudeCode);
            pickers.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Ready(vec![opus.clone(), bare_model("haiku", "Haiku")]),
            );
            pickers.pick_model("opus".into(), cx);
            pickers.pick_option("contextWindow".into(), "1m".into(), false, cx);
            let resolved = pickers.resolved(cx);
            assert_eq!(resolved.model.as_deref(), Some("opus"));
            assert_eq!(resolved.model_options.get("contextWindow"), Some(&one_m));

            pickers.pick_model("haiku".into(), cx);
            assert!(pickers.resolved(cx).model_options.is_empty());

            // Restart (draft cleared) and send before the catalog is usable.
            for catalog in [
                Loadable::Idle,
                Loadable::Loading,
                Loadable::Error("down".into()),
            ] {
                pickers.config = DraftConfig::default();
                pickers.models.insert(HarnessId::ClaudeCode, catalog);
                let resolved = pickers.resolved(cx);
                assert_eq!(resolved.model.as_deref(), Some("haiku"));
                assert!(
                    resolved.model_options.is_empty(),
                    "Opus's 1M pick must not ride along with Haiku"
                );
            }

            // The Opus memory itself survives for when Opus is picked again.
            pickers.pick_model("opus".into(), cx);
            let resolved = pickers.resolved(cx);
            assert_eq!(resolved.model.as_deref(), Some("opus"));
            assert_eq!(resolved.model_options.get("contextWindow"), Some(&one_m));
        });
    }

    fn descriptor(id: HarnessId, name: &str) -> HarnessDescriptor {
        HarnessDescriptor {
            id,
            name: name.into(),
            installed: true,
            can_install: false,
            enabled: Some(true),
            reasoning_levels: Vec::new(),
            steering_mode: zuse_protocol::SteeringMode::StepBoundary,
            supports_steering: false,
            signed_in: None,
        }
    }
    #[gpui::test]
    fn native_model_discovery_defers_other_agents_until_picker_open(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (out, _requests) = tokio::sync::mpsc::channel::<String>(16);
        let (_replies, inbound) = tokio::sync::mpsc::channel::<String>(16);
        let state = cx.new(|_| AppState::new());
        state.update(cx, |state, _| {
            state.set_test_engine(EngineHandle::from_test_client(zuse_rpc::RpcClient::new(
                out, inbound,
            )))
        });
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.harnesses = Loadable::Ready(vec![
                descriptor(HarnessId::Codex, "Codex"),
                descriptor(HarnessId::ClaudeCode, "Claude"),
            ]);
            pickers.config.harness = Some(HarnessId::Codex);
            pickers.models.clear();
            pickers.prefetch_models(false, cx);
            assert!(matches!(
                pickers.models.get(&HarnessId::Codex),
                Some(Loadable::Loading)
            ));
            assert!(!pickers.models.contains_key(&HarnessId::ClaudeCode));
            pickers.open.open(PickerKind::HarnessModel);
            pickers.prefetch_models(false, cx);
            assert!(matches!(
                pickers.models.get(&HarnessId::ClaudeCode),
                Some(Loadable::Loading)
            ));
        });
    }

    #[gpui::test]
    fn model_name_never_reads_select_model_while_anything_can_still_name_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let named = |label: &str| ModelName::Named(label.into());
        handle
            .update(cx, |picker, _, cx| {
                // Harness catalog still loading, a pick remembered: named.
                picker.config.harness = Some(HarnessId::Codex);
                picker.defaults.remember_model(
                    HarnessId::Codex,
                    "gpt-6-luna".into(),
                    "GPT-6-Luna".into(),
                );
                assert_eq!(picker.model_name(cx), named("GPT-6-Luna"));
                // Model catalog loading, pick remembered: still named.
                picker.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
                picker.models.insert(HarnessId::Codex, Loadable::Loading);
                assert_eq!(picker.model_name(cx), named("GPT-6-Luna"));
                // A draft id the catalog never learned keeps its id.
                picker.config.model = Some("gpt-7".into());
                assert_eq!(picker.model_name(cx), named("gpt-7"));
                // Nothing picked or remembered while loading: loading, not
                // "Select model".
                picker.config.model = None;
                picker.defaults.model_by_harness.clear();
                assert_eq!(picker.model_name(cx), ModelName::Loading);
                picker.harnesses = Loadable::Loading;
                assert_eq!(picker.model_name(cx), ModelName::Loading);
                // Catalog in, nothing remembered: its default model.
                picker.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
                picker.models.insert(
                    HarnessId::Codex,
                    Loadable::Ready(vec![bare_model("gpt-a", "A"), bare_model("gpt-b", "B")]),
                );
                assert_eq!(picker.model_name(cx), named("A"));
                // Only a provider with nothing to offer is unnamed.
                picker
                    .models
                    .insert(HarnessId::Codex, Loadable::Ready(Vec::new()));
                assert_eq!(picker.model_name(cx), ModelName::None { no_agents: false });
                picker
                    .models
                    .insert(HarnessId::Codex, Loadable::Error("offline".into()));
                assert_eq!(picker.model_name(cx), ModelName::None { no_agents: false });
                // ...and a remembered pick names it even then.
                picker.defaults.remember_model(
                    HarnessId::Codex,
                    "gpt-6-luna".into(),
                    "GPT-6-Luna".into(),
                );
                assert_eq!(picker.model_name(cx), named("GPT-6-Luna"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn model_menu_browsing_keeps_selection_and_activation_closes(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let mut picker = Pickers::new(state, cx);
            picker.config.harness = Some(HarnessId::Codex);
            picker.harnesses = Loadable::Ready(vec![
                descriptor(HarnessId::ClaudeCode, "Claude"),
                descriptor(HarnessId::Codex, "Codex"),
            ]);
            picker.models.insert(
                HarnessId::Codex,
                Loadable::Ready(vec![
                    bare_model("balanced", "Balanced"),
                    bare_model("deep", "Deep"),
                ]),
            );
            let mut model = bare_model("opus", "Opus");
            model.description = Some("Only in the description".into());
            picker
                .models
                .insert(HarnessId::ClaudeCode, Loadable::Ready(vec![model]));
            picker
        });
        handle
            .update(cx, |picker, window, cx| {
                picker.open_model_menu(window, cx);
                assert_eq!(picker.model_rows(cx)[0].harness, HarnessId::Codex);
                picker.model_scope = Some(HarnessId::ClaudeCode);
                assert_eq!(picker.model_rows_len(cx), 1);
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::Codex));
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("description", cx));
                assert_eq!(picker.model_rows_len(cx), 0);
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("  OPUS  ", cx));
                assert_eq!(picker.model_rows_len(cx), 1);
                picker.activate_model_index(0, cx);
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::ClaudeCode));
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("opus"));
                assert!(!picker.is_open());
                picker.open_model_menu(window, cx);
                assert!(picker.model_scope.is_none());
                assert!(picker.search.read(cx).text().is_empty());
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        cx.simulate_keystrokes(handle.into(), "escape");
        handle
            .read_with(cx, |picker, _| assert!(!picker.is_open()))
            .unwrap();
    }

    #[gpui::test]
    fn compact_panel_lists_settings_of_models_configured_in_place(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let option = |id: &str, label: &str, choices: &[(&str, &str)]| ModelOption {
            id: id.into(),
            label: label.into(),
            default_choice: choices[0].0.into(),
            choices: choices
                .iter()
                .map(|(id, label)| ModelOptionChoice {
                    id: (*id).into(),
                    label: (*label).into(),
                })
                .collect(),
        };
        handle
            .update(cx, |pickers, _, cx| {
                let mut fusion = bare_model("fusion", "Fusion");
                fusion.reasoning_levels = vec![ReasoningLevel::Medium, ReasoningLevel::High];
                fusion.options = vec![
                    option(
                        "sidekick",
                        "Sidekick",
                        &[("swe-2", "SWE-2"), ("swe-3", "SWE-3")],
                    ),
                    option("lead", "Lead", &[("fable", "Fable"), ("sol", "Sol")]),
                ];
                pickers.config.harness = Some(HarnessId::Devin);
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Devin, "Devin")]);
                pickers.models.insert(
                    HarnessId::Devin,
                    Loadable::Ready(vec![bare_model("adaptive", "Adaptive"), fusion]),
                );
                pickers.pick_model("fusion".into(), cx);
                // No hover card in the compact panel: Lead leads, as in the card.
                let hidden = pickers.compact_hidden_options(cx);
                let visible: Vec<_> = pickers
                    .setting_groups(cx)
                    .into_iter()
                    .filter(|g| Pickers::compact_option_visible(&g.id, &hidden))
                    .map(|g| g.id)
                    .collect();
                assert_eq!(
                    visible,
                    [
                        ModelSetting::Option("lead".into()),
                        ModelSetting::Option("sidekick".into())
                    ]
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_fast_switch_preserves_enabled_provider_defaults(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |pickers, window, cx| {
                let mut grok = bare_model("grok-4.6", "Grok 4.6");
                grok.options = vec![ModelOption {
                    id: "fast".into(),
                    label: "Fast".into(),
                    default_choice: "true".into(),
                    choices: [("false", "false"), ("true", "Fast\u{200b}")]
                        .iter()
                        .map(|(id, label)| ModelOptionChoice {
                            id: (*id).into(),
                            label: (*label).into(),
                        })
                        .collect(),
                }];
                pickers.config.harness = Some(HarnessId::Cursor);
                pickers.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Cursor, "Cursor")]);
                pickers
                    .models
                    .insert(HarnessId::Cursor, Loadable::Ready(vec![grok]));
                pickers.pick_model("grok-4.6".into(), cx);
                // On by default: the button starts lit and turning it off is
                // an explicit pick; turning it back on returns to the default.
                let (option, next, default, fast) = pickers.compact_fast_choice(cx).unwrap();
                assert_eq!((next.as_str(), default, fast), ("false", false, true));
                pickers.pick_option(option, next, default, cx);
                let (option, next, default, fast) = pickers.compact_fast_choice(cx).unwrap();
                assert_eq!((next.as_str(), default, fast), ("true", true, false));
                pickers.pick_option(option, next, default, cx);
                assert!(pickers.compact_fast_choice(cx).unwrap().3);
                assert!(!pickers.explicit_options(cx).contains_key("fast"));
                // F on the open panel flips it the same way.
                pickers.open.open(PickerKind::HarnessModel);
                let key = |key: &str| KeyDownEvent {
                    keystroke: gpui::Keystroke::parse(key).unwrap(),
                    is_held: false,
                    prefer_character_input: false,
                };
                pickers.on_key_down(&key("f"), window, cx);
                assert!(!pickers.compact_fast_choice(cx).unwrap().3);
                pickers.on_key_down(&key("cmd-f"), window, cx);
                assert!(!pickers.compact_fast_choice(cx).unwrap().3);
                pickers.on_key_down(&key("f"), window, cx);
                assert!(pickers.compact_fast_choice(cx).unwrap().3);
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_list_hides_loading_and_failed_harnesses(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let mut picker = Pickers::new(state, cx);
            picker.config.harness = Some(HarnessId::Codex);
            picker.harnesses = Loadable::Ready(vec![
                descriptor(HarnessId::Codex, "Codex"),
                descriptor(HarnessId::ClaudeCode, "Claude"),
                descriptor(HarnessId::Grok, "Grok"),
            ]);
            picker.models.insert(
                HarnessId::Codex,
                Loadable::Ready(vec![bare_model("model", "Model")]),
            );
            picker.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Error("Catalog timed out".into()),
            );
            picker.models.insert(HarnessId::Grok, Loadable::Loading);
            picker
        });
        handle
            .update(cx, |picker, window, cx| {
                picker.open_model_menu(window, cx);
                picker.show_compact_models(cx);
                assert_eq!(picker.model_rows_len(cx), 1);
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        // Only the ready model is navigable: no status rows follow it.
        cx.simulate_keystrokes(handle.into(), "down");
        handle
            .read_with(cx, |picker, _| assert_eq!(picker.active, 0))
            .unwrap();
        handle
            .update(cx, |picker, _, cx| {
                picker.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![bare_model("claude", "Claude model")]),
                );
                picker.catalog_rev += 1;
                // A provider's models become directly selectable when its
                // catalog finishes loading; no provider-page detour is needed.
                assert_eq!(picker.model_rows_len(cx), 2);
                assert_eq!(picker.model_rows(cx)[1].harness, HarnessId::ClaudeCode);
                picker.activate_model_index(1, cx);
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::ClaudeCode));
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("claude"));
                picker.show_compact_models(cx);
                assert_eq!(picker.model_rows_len(cx), 2);
                assert_eq!(
                    picker.model_rows(cx)[picker.active].harness,
                    HarnessId::ClaudeCode
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_slider_drives_cursor_effort_option(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |picker, _, cx| {
                // Cursor's shape: no reasoning ladder, effort as an option.
                let mut opus = bare_model("claude-opus-5", "Claude Opus 5");
                opus.options = vec![ModelOption {
                    id: "effort".into(),
                    label: "Effort".into(),
                    default_choice: "high".into(),
                    choices: [("low", "Low"), ("medium", "Medium"), ("high", "High")]
                        .iter()
                        .map(|(id, label)| ModelOptionChoice {
                            id: (*id).into(),
                            label: (*label).into(),
                        })
                        .collect(),
                }];
                picker.config.harness = Some(HarnessId::Cursor);
                picker.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Cursor, "Cursor")]);
                picker
                    .models
                    .insert(HarnessId::Cursor, Loadable::Ready(vec![opus]));
                picker.pick_model("claude-opus-5".into(), cx);
                let effort = picker.compact_effort(cx).unwrap();
                assert_eq!(effort.labels, ["Low", "Medium", "High"]);
                assert_eq!(effort.selected, 2);
                // The slider owns it: no Effort row in the panel.
                assert_eq!(picker.compact_hidden_options(cx), ["effort"]);
                picker.pick_compact_effort(effort, 0, cx);
                assert_eq!(picker.resolved(cx).model_options["effort"], "low");
                assert_eq!(picker.compact_effort(cx).unwrap().selected, 0);
                // Back to the default clears the explicit pick.
                let effort = picker.compact_effort(cx).unwrap();
                picker.pick_compact_effort(effort, 2, cx);
                assert!(!picker.explicit_options(cx).contains_key("effort"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn switching_provider_restores_its_last_model_and_effort(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let laddered = |id: &str, label: &str| {
            let mut model = bare_model(id, label);
            model.reasoning_levels = vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ];
            model
        };
        handle
            .update(cx, |picker, _, cx| {
                picker.harnesses = Loadable::Ready(vec![
                    descriptor(HarnessId::Codex, "Codex"),
                    descriptor(HarnessId::ClaudeCode, "Claude"),
                ]);
                picker.models.insert(
                    HarnessId::Codex,
                    Loadable::Ready(vec![laddered("gpt-a", "A"), laddered("gpt-b", "B")]),
                );
                picker.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![laddered("opus", "Opus")]),
                );
                picker.pick_harness(HarnessId::Codex, cx);
                picker.pick_model("gpt-b".into(), cx);
                picker.pick_reasoning(ReasoningLevel::Low, cx);
                picker.pick_compact_provider(HarnessId::ClaudeCode, cx);
                picker.pick_reasoning(ReasoningLevel::High, cx);
                // Back to Codex: its last model, at that model's last effort.
                picker.pick_compact_provider(HarnessId::Codex, cx);
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("gpt-b"));
                assert_eq!(picker.effective_reasoning(cx), Some(ReasoningLevel::Low));
                // A model with no effort of its own takes the last one used
                // anywhere (High, on Claude).
                picker.pick_model("gpt-a".into(), cx);
                assert_eq!(picker.effective_reasoning(cx), Some(ReasoningLevel::High));
                picker.pick_compact_provider(HarnessId::ClaudeCode, cx);
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("opus"));
                assert_eq!(picker.effective_reasoning(cx), Some(ReasoningLevel::High));
                // The list's tabs are its provider groups, the current provider
                // (Claude) first; a starred model keeps its place in its
                // provider's run but forms its own group.
                picker.show_compact_models(cx);
                assert_eq!(
                    picker.compact_groups(cx),
                    vec![
                        (Some(HarnessId::ClaudeCode), 0),
                        (Some(HarnessId::Codex), 1)
                    ]
                );
                picker.toggle_model_favorite(HarnessId::Codex, "gpt-a", cx);
                assert_eq!(
                    picker.compact_groups(cx),
                    vec![
                        (Some(HarnessId::ClaudeCode), 0),
                        (None, 1),
                        (Some(HarnessId::Codex), 2),
                    ]
                );
                let rows = picker.model_rows(cx);
                assert_eq!(
                    (rows[0].harness, rows[0].model.id.as_str()),
                    (HarnessId::ClaudeCode, "opus")
                );
                // The starred model stays in its provider's run, flagged by
                // its own group.
                assert_eq!(
                    (rows[1].harness, rows[1].model.id.as_str()),
                    (HarnessId::Codex, "gpt-a")
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_picker_shares_selection_reset_and_thread_restrictions(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            let mut picker = Pickers::new(state, cx);
            picker.config.harness = Some(HarnessId::Codex);
            picker.config.model = Some("codex-model".into());
            picker.harnesses = Loadable::Ready(vec![
                descriptor(HarnessId::Codex, "Codex"),
                descriptor(HarnessId::ClaudeCode, "Claude"),
            ]);
            let mut model = bare_model("codex-model", "Codex model");
            model.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
            picker
                .models
                .insert(HarnessId::Codex, Loadable::Ready(vec![model]));
            picker.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Ready(vec![bare_model("claude-model", "Claude model")]),
            );
            picker
        });
        handle
            .update(cx, |picker, window, cx| {
                picker.open_model_menu(window, cx);
                assert!(picker.compact_model_list);
                // Models from every offered provider are directly selectable.
                picker.show_compact_models(cx);
                assert_eq!(picker.model_rows_len(cx), 2);
                assert_eq!(picker.model_rows(cx)[0].harness, HarnessId::Codex);
                assert_eq!(picker.model_rows(cx)[1].harness, HarnessId::ClaudeCode);
                // Searching must also find another provider's model.
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("Claude", cx));
                assert_eq!(picker.model_rows_len(cx), 1);
                assert_eq!(picker.model_rows(cx)[0].harness, HarnessId::ClaudeCode);
                picker.activate_model_index(0, cx);
                assert!(!picker.is_open());
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::ClaudeCode));
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("claude-model"));
                picker.pick_harness(HarnessId::Codex, cx);
                // Clicking a foreign-provider row works without a search too.
                picker.show_compact_models(cx);
                picker.activate_model_index(1, cx);
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::ClaudeCode));
                picker.pick_harness(HarnessId::Codex, cx);
                // Switching provider (Tab on the panel) lands back on the
                // panel at that provider's model.
                picker.pick_compact_provider(HarnessId::ClaudeCode, cx);
                assert!(!picker.compact_model_list);
                assert_eq!(picker.resolved(cx).harness, Some(HarnessId::ClaudeCode));
                assert_eq!(picker.resolved(cx).model.as_deref(), Some("claude-model"));
                picker.pick_harness(HarnessId::Codex, cx);
                picker.pick_model("codex-model".into(), cx);
                picker.pick_reasoning(ReasoningLevel::Low, cx);
                assert_eq!(picker.config.model.as_deref(), Some("codex-model"));
                picker.state.update(cx, |state, cx| {
                    state.selected_chat = Some("thread".into());
                    cx.notify();
                });
                picker.show_compact_models(cx);
                assert_eq!(picker.model_rows_len(cx), 1);
                assert_eq!(picker.rail_descriptors(cx)[0].id, HarnessId::Codex);
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("Claude", cx));
                assert_eq!(picker.model_rows_len(cx), 0);
                picker.search.update(cx, |input, cx| input.set_text("", cx));
                // Until the selected chat row arrives, keep its last known provider.
                assert_eq!(picker.compact_groups(cx), vec![(Some(HarnessId::Codex), 0)]);
                picker.compact_model_list = false;
                picker.focus_on_mount = true;
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
        handle
            .read_with(cx, |picker, _| assert!(picker.effort_bounds.is_some()))
            .unwrap();
        handle
            .update(cx, |picker, _, cx| {
                picker.show_compact_models(cx);
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("no-matching-model", cx));
                picker.active = 0;
                picker.activate_model_row(cx);
                assert!(
                    picker.setting_menu.is_none(),
                    "Empty search must not activate hidden settings"
                );
                picker.search.update(cx, |input, cx| input.set_text("", cx));
            })
            .unwrap();
        cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
            .unwrap();
    }

    #[gpui::test]
    fn reasoning_menu_sorts_levels_and_retains_advertised_modes(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle.update(cx, |picker, _, cx| {
            picker.harnesses = Loadable::Ready(vec![descriptor(HarnessId::Codex, "Codex")]);
            picker.config.harness = Some(HarnessId::Codex);
            picker.config.model = Some("gpt-test".into());
            let mut model = bare_model("gpt-test", "GPT Test");
            model.reasoning_levels = vec![ReasoningLevel::XHigh, ReasoningLevel::High, ReasoningLevel::Low, ReasoningLevel::Medium];
            model.options = serde_json::from_value(serde_json::json!([
                {"id":"serviceTier","label":"Service tier","defaultChoice":"default","choices":[{"id":"default","label":"Standard"},{"id":"fast","label":"Fast"}]},
                {"id":"contextWindow","label":"Context window","defaultChoice":"200k","choices":[{"id":"200k","label":"200K"},{"id":"1m","label":"1M"}]},
                {"id":"other","label":"Other","defaultChoice":"a","choices":[{"id":"a","label":"A"},{"id":"b","label":"B"},{"id":"c","label":"C"}]}
            ])).unwrap();
            picker.models.insert(HarnessId::Codex, Loadable::Ready(vec![model]));
            picker.effort_menu_open = true;
            let effort = picker.compact_effort(cx).unwrap();
            assert_eq!(effort.labels.iter().map(|v| v.as_ref()).collect::<Vec<&str>>(), ["Low", "Medium", "High", "X-High"]);
            picker.pick_compact_effort(effort, 0, cx);
            assert_eq!(picker.effective_reasoning(cx), Some(ReasoningLevel::Low));
            assert!(picker.compact_fast_choice(cx).is_some());
            let groups = picker.setting_groups(cx);
            assert!(groups.iter().any(|g| g.id == ModelSetting::Option("contextWindow".into())));
            assert!(groups.iter().any(|g| g.id == ModelSetting::Option("other".into())));
            assert!(picker.toggle_fast_mode(cx));
            assert_eq!(picker.resolved(cx).model_options["serviceTier"], "fast");
            picker.pick_option("contextWindow".into(), "1m".into(), false, cx);
            assert_eq!(picker.resolved(cx).model_options["contextWindow"], "1m");
            assert_eq!(picker.resolved(cx).model_options["serviceTier"], "fast");
        }).unwrap();
    }

    #[gpui::test]
    fn compact_picker_opens_favorites_or_current_provider(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |picker, window, cx| {
                picker.harnesses = Loadable::Ready(vec![
                    descriptor(HarnessId::Codex, "Codex"),
                    descriptor(HarnessId::ClaudeCode, "Claude"),
                ]);
                picker.models.insert(
                    HarnessId::Codex,
                    Loadable::Ready(vec![
                        bare_model("gpt-a", "GPT A"),
                        bare_model("gpt-b", "GPT B"),
                    ]),
                );
                picker.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![bare_model("opus", "Opus")]),
                );
                picker.config.harness = Some(HarnessId::Codex);
                picker.config.model = Some("gpt-a".into());

                picker.open_model_menu(window, cx);
                assert_eq!(picker.model_rail, ModelRail::Harness);
                assert_eq!(picker.model_scope, Some(HarnessId::Codex));
                assert_eq!(picker.model_rows(cx).len(), 2);
                picker.close(cx);

                picker.toggle_model_favorite(HarnessId::ClaudeCode, "opus", cx);
                picker.open_model_menu(window, cx);
                assert_eq!(picker.model_rail, ModelRail::Favorites);
                assert_eq!(picker.model_scope, None);
                let rows = picker.model_rows(cx);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].model.id, "opus");
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_picker_deduplicates_ids_and_marks_same_named_models(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |picker, _, cx| {
                picker.harnesses =
                    Loadable::Ready(vec![descriptor(HarnessId::ClaudeCode, "Claude")]);
                picker.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![
                        bare_model("claude-sonnet-5", "Sonnet 5"),
                        bare_model("claude-sonnet-5", "Sonnet 5"),
                        bare_model("sonnet", "Sonnet 5"),
                        bare_model("opus", "Opus"),
                    ]),
                );
                picker.config.harness = Some(HarnessId::ClaudeCode);
                picker.model_scope = Some(HarnessId::ClaudeCode);
                picker.model_rail = ModelRail::Harness;
                let rows = picker.model_rows(cx);
                assert_eq!(
                    rows.iter()
                        .map(|row| row.model.id.as_str())
                        .collect::<Vec<_>>(),
                    ["claude-sonnet-5", "sonnet", "opus"]
                );
                assert!(rows[0].ambiguous);
                assert!(rows[1].ambiguous);
                assert!(!rows[2].ambiguous);
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_favorites_filter_and_shortcuts_follow_visible_rows(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            let mut settings = crate::settings::UiSettings::default();
            settings.compact_model_picker = true;
            crate::settings::init(settings, dir.path(), cx);
        });
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        handle
            .update(cx, |picker, _, cx| {
                picker.harnesses = Loadable::Ready(vec![
                    descriptor(HarnessId::Codex, "Codex"),
                    descriptor(HarnessId::ClaudeCode, "Claude"),
                ]);
                picker.models.insert(
                    HarnessId::Codex,
                    Loadable::Ready(vec![
                        bare_model("gpt-a", "GPT A"),
                        bare_model("gpt-b", "GPT B"),
                    ]),
                );
                picker.models.insert(
                    HarnessId::ClaudeCode,
                    Loadable::Ready(vec![bare_model("opus", "Opus")]),
                );
                picker.config.harness = Some(HarnessId::Codex);
                picker.model_scope = Some(HarnessId::Codex);
                picker.model_rail = ModelRail::Harness;
                picker.toggle_model_favorite(HarnessId::Codex, "gpt-b", cx);
                picker.toggle_model_favorite(HarnessId::ClaudeCode, "opus", cx);
                assert_eq!(picker.model_rows(cx).len(), 2);
                picker.model_scope = None;
                picker.model_rail = ModelRail::Favorites;
                let ids: Vec<_> = picker
                    .model_rows(cx)
                    .iter()
                    .map(|row| row.model.id.clone())
                    .collect();
                assert_eq!(ids, ["gpt-b", "opus"]);
                picker
                    .search
                    .update(cx, |input, cx| input.set_text("opus", cx));
                assert_eq!(picker.model_rows(cx)[0].model.id, "opus");
                assert_eq!(picker.model_rows(cx).len(), 1);
                picker.search.update(cx, |input, cx| input.set_text("", cx));
                picker.open.open(PickerKind::HarnessModel);
                assert!(picker.jump_model_slot(1, cx));
                assert_eq!(picker.effective_harness(cx), Some(HarnessId::ClaudeCode));
                assert_eq!(picker.effective_model_id(cx), Some("opus"));
                picker.toggle_model_favorite(HarnessId::Codex, "gpt-b", cx);
                assert_eq!(picker.model_rows(cx).len(), 1);
                assert_eq!(picker.model_rows(cx)[0].model.id, "opus");
            })
            .unwrap();
    }

    #[test]
    fn compact_all_models_keeps_favorites_first_in_catalog_and_search() {
        let descriptors = vec![
            descriptor(HarnessId::ClaudeCode, "Claude"),
            descriptor(HarnessId::Codex, "Codex"),
        ];
        let claude = vec![
            bare_model("plain-a", "Model A"),
            bare_model("star-a", "My Model A"),
        ];
        let codex = vec![
            bare_model("plain-b", "Model B"),
            bare_model("star-b", "My Model B"),
        ];
        for query in ["", "model"] {
            let rows = scoped_model_rows(
                query,
                ModelRail::All,
                Some(HarnessId::Codex),
                &descriptors,
                |harness| match harness {
                    HarnessId::ClaudeCode => Some(claude.as_slice()),
                    HarnessId::Codex => Some(codex.as_slice()),
                    _ => None,
                },
                |_, id| id.starts_with("star"),
            );
            assert_eq!(
                rows.iter()
                    .map(|row| row.model.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["star-a", "star-b", "plain-a", "plain-b"]
            );
        }
    }

    #[test]
    fn tab_search_never_leaves_the_viewed_harness() {
        let descriptors = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
            descriptor(HarnessId::Codex, "Codex"),
        ];
        let claude = vec![bare_model("fable-5", "Fable 5")];
        let codex = vec![bare_model("gpt-fable", "Fable (Codex)")];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            match harness {
                HarnessId::ClaudeCode => Some(claude.as_slice()),
                HarnessId::Codex => Some(codex.as_slice()),
                _ => None,
            }
        };
        // Both catalogs match "fable", but the viewed tab is Claude — the
        // Codex hit must not appear.
        let rows = scoped_model_rows(
            "fable",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            |_, _| false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, HarnessId::ClaudeCode);
        assert_eq!(rows[0].model.id, "fable-5");
    }

    #[test]
    fn favorites_tab_search_ranks_only_starred_rows() {
        let descriptors = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
            descriptor(HarnessId::Codex, "Codex"),
        ];
        let claude = vec![bare_model("fable-5", "Fable 5")];
        let codex = vec![bare_model("gpt-fable", "Fable (Codex)")];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            match harness {
                HarnessId::ClaudeCode => Some(claude.as_slice()),
                HarnessId::Codex => Some(codex.as_slice()),
                _ => None,
            }
        };
        let starred =
            |harness: HarnessId, model: &str| harness == HarnessId::Codex && model == "gpt-fable";
        let rows = scoped_model_rows(
            "fable",
            ModelRail::Favorites,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, HarnessId::Codex);

        // Empty query on the favorites tab: the starred set, nothing else.
        let rows = scoped_model_rows(
            "",
            ModelRail::Favorites,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.id, "gpt-fable");
    }

    #[test]
    fn harness_tab_lists_stars_first_and_description_still_matches() {
        let descriptors = vec![descriptor(HarnessId::Opencode, "opencode")];
        let mut provider_a = bare_model("glm-5.2-a", "GLM-5.2");
        provider_a.description = Some("Anthropic".into());
        let mut provider_b = bare_model("glm-5.2-b", "GLM-5.2");
        provider_b.description = Some("Baseten".into());
        let models = vec![provider_a, provider_b];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::Opencode).then_some(models.as_slice())
        };
        let starred = |harness: HarnessId, model: &str| {
            harness == HarnessId::Opencode && model == "glm-5.2-b"
        };
        // No query: catalog order with the star floated to the top.
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::Opencode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows[0].model.id, "glm-5.2-b");
        assert_eq!(rows[1].model.id, "glm-5.2-a");
        // Provider attribution stays searchable inside the tab.
        let rows = scoped_model_rows(
            "baseten",
            ModelRail::Harness,
            Some(HarnessId::Opencode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.id, "glm-5.2-b");
    }

    /// Rows show descriptions only to tell same-named models apart.
    #[test]
    fn only_same_named_rows_are_ambiguous() {
        let descriptors = vec![
            descriptor(HarnessId::Opencode, "opencode"),
            descriptor(HarnessId::Devin, "Devin"),
        ];
        let mut glm_a = bare_model("glm-a", "GLM-5.2");
        glm_a.description = Some("Anthropic".into());
        let mut glm_b = bare_model("glm-b", "GLM-5.2");
        glm_b.description = Some("Baseten".into());
        let opencode = vec![glm_a, glm_b, bare_model("kimi", "Kimi")];
        let devin = vec![bare_model("glm", "GLM-5.2")];
        let mut rows = scoped_model_rows(
            "",
            ModelRail::Favorites,
            None,
            &descriptors,
            |harness| match harness {
                HarnessId::Opencode => Some(opencode.as_slice()),
                HarnessId::Devin => Some(devin.as_slice()),
                _ => None,
            },
            |_, _| true,
        );
        mark_ambiguous(&mut rows);
        let flags: Vec<_> = rows
            .iter()
            .map(|r| (r.model.id.as_str(), r.ambiguous))
            .collect();
        assert!(flags.contains(&("glm-a", true)));
        assert!(flags.contains(&("glm-b", true)));
        assert!(flags.contains(&("kimi", false)));
        assert!(
            flags.contains(&("glm", false)),
            "other harness: not a clash"
        );
    }

    #[test]
    fn normalize_drops_default_alias_and_folds_orphan_1m_rows() {
        // The shape an OLDER engine serves: a `default` alias row plus
        // 1M-pinned variants with no bare base. A non-claude harness keeps
        // wire labels (no curated catalog to borrow from).
        let models = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("titan[1m]", "Titan (1M context)"),
                bare_model("gpt-x-9[1m]", "GPT X-9"),
                bare_model("nano", "Nano"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["titan", "gpt-x-9", "nano"]
        );
        assert_eq!(models[0].label, "Titan");
        assert_eq!(models[1].label, "GPT X-9");
        // Folded rows pin the Context Window trait to 1M.
        assert!(
            models[0]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        assert!(models[2].options.is_empty());

        // A `default`-only list survives (nothing real to prefer).
        let only_default =
            normalize_model_rows(HarnessId::Codex, vec![bare_model("default", "Default")]);
        assert_eq!(only_default.len(), 1);

        // A base-plus-variant pair (already folded by a NEWER engine — the
        // variant never reaches us; belt-and-braces if it does): variant
        // drops, base is untouched.
        let paired = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("titan-5", "Titan 5"),
                bare_model("titan-5[1m]", "Titan 5 (1M)"),
            ],
        );
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].id, "titan-5");

        // Idempotent over a clean list.
        let clean = vec![bare_model("titan-5", "Titan 5")];
        assert_eq!(normalize_model_rows(HarnessId::Codex, clean.clone()), clean);
    }

    #[test]
    fn normalize_gives_claude_rows_their_versioned_catalog_labels() {
        // The real prod shape: alias values with terse names. Claude rows
        // adopt the curated labels so the version number always shows
        // (user request), exact ids included; foreign ids pass through.
        let models = normalize_model_rows(
            HarnessId::ClaudeCode,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("opus[1m]", "Opus (1M context)"),
                bare_model("claude-fable-5[1m]", "Fable"),
                bare_model("sonnet", "Sonnet"),
                bare_model("haiku", "Haiku"),
                bare_model("claude-nova-1", "Nova 1"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec!["Opus 5.5", "Fable 5", "Sonnet 5", "Haiku 4.5", "Nova 1"]
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "claude-fable-5", "sonnet", "haiku", "claude-nova-1"]
        );
    }

    #[test]
    fn remembered_picks_keep_only_offered_options() {
        let mut model = bare_model("opus", "Opus");
        model.options = vec![ModelOption {
            id: "context".into(),
            label: "Context window".into(),
            choices: vec![
                ModelOptionChoice {
                    id: "standard".into(),
                    label: "Standard".into(),
                },
                ModelOptionChoice {
                    id: "1m".into(),
                    label: "1M".into(),
                },
            ],
            default_choice: "standard".into(),
        }];
        let mut remembered = serde_json::Map::new();
        remembered.insert("context".into(), serde_json::Value::String("1m".into()));
        remembered.insert("fastMode".into(), serde_json::Value::String("on".into()));
        let mut stale = remembered.clone();
        stale.insert("context".into(), serde_json::Value::String("2m".into()));
        let mut want = serde_json::Map::new();
        want.insert("context".into(), serde_json::Value::String("1m".into()));
        assert_eq!(offered_options(&model, remembered), want);
        assert!(offered_options(&model, stale).is_empty());
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn windows_folder_paths_and_breadcrumbs() {
        assert_eq!(
            parent_path(r"D:\Random\zuse"),
            Some(r"D:\Random".to_string())
        );
        assert_eq!(parent_path(r"D:\Random"), Some(r"D:\".to_string()));
        assert_eq!(parent_path(r"D:\Random\"), Some(r"D:\".to_string()));
        assert_eq!(parent_path(r"D:\"), None);
        assert_eq!(parent_path("D:"), None);
        assert_eq!(child_path(r"D:\", "Random"), r"D:\Random");
        assert_eq!(child_path(r"D:\Random", "zuse"), r"D:\Random\zuse");
        let crumbs = breadcrumbs(r"D:\Random\zuse");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, [r"D:\", "Random", "zuse"]);
        assert_eq!(crumbs[0].1, r"D:\");
        assert_eq!(crumbs[1].1, r"D:\Random");
        assert_eq!(breadcrumbs(r"D:\").len(), 1);
        assert!(!is_windows_path("/D:/x"));
        assert!(!is_windows_path("ab:/x"));
    }

    #[test]
    fn extended_and_unc_folder_navigation_preserves_roots() {
        for root in [r"\\?\D:\", r"\\server\share\", r"\\?\UNC\server\share\"] {
            let folder = format!("{root}projects\\zuse");
            let parent = format!("{root}projects");
            assert_eq!(parent_path(&folder), Some(parent.clone()));
            assert_eq!(parent_path(&parent), Some(root.to_owned()));
            assert_eq!(parent_path(root), None);
            assert_eq!(child_path(&parent, "zuse"), folder);
            let crumbs = breadcrumbs(&folder);
            assert_eq!(
                crumbs,
                vec![
                    (root.to_owned(), root.to_owned()),
                    ("projects".into(), parent),
                    ("zuse".into(), folder.clone()),
                ]
            );
            assert!(is_typed_path(&folder));
            assert_eq!(
                typed_path_target(&format!("{folder}\\"), None),
                Some(folder)
            );
            assert_eq!(typed_path_target(root, None), Some(root.to_owned()));
        }
        // The server alone isn't a browsable share, and device namespaces
        // must not be mistaken for a server/share pair.
        assert!(!is_windows_path(r"\\server"));
        assert!(!is_windows_path(r"\\server\\repo"));
        assert!(!is_windows_path(r"\\.\C:\repo"));
        assert!(!is_windows_path(r"\\?\Volume{123}\repo"));
    }

    #[test]
    fn windows_folder_identity_handles_prefixes_without_changing_posix_names() {
        assert!(same_folder_path(
            r"C:\Users\Ana\repo",
            r"\\?\C:\Users\Ana\repo"
        ));
        assert!(same_folder_path(
            r"C:/Users/Ana/repo/",
            r"\\?\C:\Users\Ana\repo"
        ));
        assert!(same_folder_path(
            r"\\server\share\repo",
            r"\\?\UNC\server\share\repo"
        ));
        assert!(!same_folder_path(
            r"\\server\share\repo",
            r"\\server\other\repo"
        ));
        assert!(!same_folder_path(r"C:\repo", r"D:\repo"));
        assert!(!same_folder_path(r"C:\Repo", r"C:\repo"));
        // Only the drive letter is case-insensitive.
        assert!(same_folder_path(r"c:\Users\Ana", r"C:\Users\Ana"));
        assert!(same_folder_path(r"c:/Users/Ana/", r"\\?\C:\Users\Ana"));
        assert!(same_folder_path(r"\\?\c:\repo.", r"\\?\C:\repo."));
        assert!(same_folder_path(r"d:\", r"D:\"));
        assert!(!same_folder_path(r"c:\users\ana", r"C:\Users\Ana"));
        // Extended paths can refer to names with different Win32 semantics.
        for name in ["repo.", "repo ", "..", "NUL.txt", "COM¹"] {
            assert!(!same_folder_path(
                &format!(r"C:\{name}"),
                &format!(r"\\?\C:\{name}")
            ));
        }
        for path in [
            "/home/ana/repo",
            "/Users/Ana/repo",
            r"/tmp/name\with\backslashes",
            "//server/share",
        ] {
            assert_eq!(folder_path_key(path), path);
        }
        assert!(!same_folder_path("/home/Ana", "/home/ana"));
    }

    #[test]
    fn home_relative_jumps_use_the_remote_windows_separator() {
        for home in [
            r"C:\Users\Ana",
            r"\\?\C:\Users\Ana",
            r"\\server\share\Ana",
            r"\\?\UNC\server\share\Ana",
        ] {
            assert_eq!(
                typed_path_target("~/code/zuse/", Some(home)),
                Some(format!("{home}\\code\\zuse"))
            );
            assert_eq!(
                typed_path_target(r"~\code\zuse\", Some(home)),
                Some(format!("{home}\\code\\zuse"))
            );
        }
        assert_eq!(
            typed_path_target("~/code", Some("/home/ana")),
            Some("/home/ana/code".into())
        );
        assert_eq!(
            typed_path_target("~/code", Some("/Users/Ana")),
            Some("/Users/Ana/code".into())
        );
        assert_eq!(typed_path_target(r"~\code", Some("/home/ana")), None);
    }

    #[test]
    fn completion_prefix_lengths() {
        // Case-insensitive; the length indexes into the NAME's bytes.
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("zuse", "zuse"), Some(4));
        assert_eq!(completion_prefix_len("zuse", ""), Some(0));
        assert_eq!(completion_prefix_len("zuse", "dev"), None);
        // Longer than the name → not a prefix.
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        // Multibyte names slice on a char boundary.
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        // Exact casing beats the earlier case-insensitive sibling…
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        // …but with no exact-cased hit, case-insensitive exact still lands.
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
        // Unique prefix descends; an ambiguous one keeps the slash honest.
        assert_eq!(segment_target(&names, "work"), Some(2));
        assert_eq!(segment_target(&names, "g"), None);
        assert_eq!(segment_target(&names, "x"), None);
    }

    #[test]
    fn typed_path_target_expands_absolute_and_home_paths() {
        let home = Some("/home/wing");
        assert_eq!(typed_path_target("/disk2/", home), Some("/disk2".into()));
        assert_eq!(
            typed_path_target("/disk2/projects", home),
            Some("/disk2/projects".into())
        );
        assert_eq!(typed_path_target("/", home), Some("/".into()));
        assert_eq!(typed_path_target("~", home), Some("/home/wing".into()));
        assert_eq!(typed_path_target("~/", home), Some("/home/wing".into()));
        assert_eq!(
            typed_path_target("~/github/", home),
            Some("/home/wing/github".into())
        );
        // `~x` is a folder name; relative queries are searches, not paths.
        assert_eq!(typed_path_target("~x", home), None);
        assert_eq!(typed_path_target("src", home), None);
        // `~` can't expand before the device's home is known.
        assert_eq!(typed_path_target("~/github", None), None);
        assert_eq!(typed_path_target("/disk2", None), Some("/disk2".into()));
    }

    #[test]
    fn typed_path_target_accepts_windows_drive_paths() {
        let home = Some(r"C:\Users\wing");
        assert_eq!(typed_path_target(r"D:\", home), Some(r"D:\".into()));
        assert_eq!(typed_path_target("D:", home), Some(r"D:\".into()));
        assert_eq!(typed_path_target("D:/", home), Some(r"D:\".into()));
        assert_eq!(
            typed_path_target(r"D:\Random\zuse\", home),
            Some(r"D:\Random\zuse".into())
        );
        // Forward slashes normalise so the crumb trail can match the path.
        assert_eq!(
            typed_path_target("D:/Random/zuse", None),
            Some(r"D:\Random\zuse".into())
        );
        assert!(is_typed_path(r"D:\x"));
        assert!(is_typed_path("/x") && is_typed_path("~"));
        assert!(!is_typed_path("src") && !is_typed_path("ab:/x"));
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "zuse".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "zuse");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::ClaudeCode);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::ClaudeCode);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
        assert!(!config.plan);
        resolved.plan = true;
        assert!(resolved.chat_config().unwrap().plan);
        resolved.sandbox = SandboxLevel::DangerFullAccess;
        assert_eq!(
            resolved.chat_config().unwrap().sandbox,
            SandboxLevel::DangerFullAccess
        );
    }

    #[test]
    fn default_model_is_first_catalog_row() {
        let models = vec![
            Model {
                id: "flagship".into(),
                label: "Flagship".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
        ];
        assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn default_reasoning_prefers_high_then_medium() {
        use ReasoningLevel::*;
        // Recommended default is High (user-corrected), even on full ladders.
        assert_eq!(
            default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            Some(High)
        );
        assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
        // No High: Medium.
        assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
        // Neither offered: first entry.
        assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
        // Ladder-less model (Haiku): no reasoning at all.
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
        use ReasoningLevel::*;
        let ladder = [Low, Medium, High, Max];
        // A pick the ladder offers survives.
        assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
        // A remembered level the new model doesn't offer heals to its default.
        assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
        // No pick at all resolves to the concrete default too.
        assert_eq!(clamp_reasoning(None, &ladder), Some(High));
        assert_eq!(clamp_reasoning(Some(High), &[]), None);
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zuse_protocol::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            can_install: false,
            enabled: None,
            signed_in: None,
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
        ];
        // Env-independent core: mock hidden in production…
        let visible = visible_harnesses_impl(&mixed, false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::ClaudeCode);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
        // …and opted back in by ZUSE_HARNESS=mock (the e2e rig).
        assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
        assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
    }

    #[test]
    fn offered_harnesses_leave_out_agents_that_need_a_missing_login() {
        let descriptor = |id: HarnessId, signed_in: Option<bool>| HarnessDescriptor {
            id,
            name: format!("{id:?}"),
            supports_steering: true,
            steering_mode: zuse_protocol::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            can_install: false,
            enabled: Some(true),
            signed_in,
        };
        let catalog = vec![
            descriptor(HarnessId::ClaudeCode, Some(true)),
            descriptor(HarnessId::Codex, Some(false)),
            descriptor(HarnessId::Opencode, None),
        ];
        let offered: Vec<_> = offered_harnesses_impl(&catalog, false)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(offered, [HarnessId::ClaudeCode, HarnessId::Opencode]);
    }

    #[test]
    fn offered_harnesses_follow_the_catalog_enabled_flags() {
        let descriptor = |id: HarnessId, name: &str, enabled: Option<bool>| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zuse_protocol::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            can_install: false,
            enabled,
            signed_in: None,
        };
        let catalog = |claude: Option<bool>, codex: Option<bool>, grok: Option<bool>| {
            vec![
                descriptor(HarnessId::Mock, "Mock", Some(false)),
                descriptor(HarnessId::ClaudeCode, "Claude Code", claude),
                descriptor(HarnessId::Codex, "Codex", codex),
                descriptor(HarnessId::Grok, "Grok", grok),
            ]
        };
        // A catalog from an engine predating the flag (all None) follows its
        // installed probes, so every detected real harness is offered.
        let offered = offered_harnesses_impl(&catalog(None, None, None), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Grok]
        );
        // The device's flags win: Grok on, Codex off; catalog order holds.
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), Some(true)), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Grok]
        );
        // The dev-rig mock opt-in survives the enabled filter (and Grok's
        // unknown flag still resolves through its installed probe).
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), None), true);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Mock, HarnessId::ClaudeCode, HarnessId::Grok]
        );
        // Nothing enabled offers nothing — the composer renders the
        // no-agents empty state instead of resurrecting disabled agents.
        let offered =
            offered_harnesses_impl(&catalog(Some(false), Some(false), Some(false)), false);
        assert!(offered.is_empty());
        // So does a legacy catalog whose installed probes all failed: never
        // resurface unrunnable agents just to avoid an empty picker.
        let mut missing = catalog(None, None, None);
        missing.iter_mut().for_each(|d| d.installed = false);
        assert!(offered_harnesses_impl(&missing, false).is_empty());
    }

    #[test]
    fn offered_harnesses_require_an_installed_cli() {
        let descriptor =
            |id: HarnessId, name: &str, enabled: Option<bool>, installed: bool| HarnessDescriptor {
                id,
                name: name.into(),
                supports_steering: true,
                steering_mode: zuse_protocol::SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed,
                can_install: false,
                enabled,
                signed_in: None,
            };
        // Enabled-but-missing-CLI agents stay out of the rail; an installed
        // enabled one rides along. A live engine no longer stamps that
        // combination (enablement follows detection), but a catalog from an
        // older engine still can — the filter is the cross-version defense.
        let catalog = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(true), false),
            descriptor(HarnessId::Grok, "Grok", Some(true), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Grok]
        );
        // Nothing enabled AND installed: an empty offered set — the fresh
        // machine where the default-enabled Claude/Codex have no CLIs (#128).
        // No fallback: offering them again would only manufacture
        // NotInstalled errors at send; the composer shows the no-agents
        // state and blocks new sends instead.
        let catalog = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(false), false),
            descriptor(HarnessId::Grok, "Grok", Some(false), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert!(offered.is_empty());
    }
}

/// Catalog for the isolated native screenshot fixture; never used by the app.
#[cfg(feature = "project-palette-fixture")]
impl Pickers {
    pub(crate) fn fixture_model_catalog(&mut self, cx: &mut Context<Self>) {
        if matches!(self.models.get(&HarnessId::Codex), Some(Loadable::Ready(_))) {
            return;
        }
        self.config.harness = Some(HarnessId::Codex);
        self.config.model = Some("gpt-5.4".into());
        self.harnesses = Loadable::Ready(serde_json::from_value(serde_json::json!([
            {"id":"codex","name":"Codex","supportsSteering":true,"steeringMode":"step-boundary","reasoningLevels":[]}
        ])).unwrap());
        self.models.insert(HarnessId::Codex, Loadable::Ready(serde_json::from_value(serde_json::json!([
            {"id":"gpt-5.4","label":"GPT-5.4","description":"For complex coding and reasoning", "reasoningLevels":["low","medium","high","xhigh"], "options":[
                {"id":"context-window","label":"Context window","defaultChoice":"standard","choices":[{"id":"standard","label":"Standard"},{"id":"1m","label":"1M tokens"}]},
                {"id":"service-tier","label":"Service tier","defaultChoice":"auto","choices":[{"id":"auto","label":"Standard"},{"id":"fast","label":"Fast"}]}
            ]},
            {"id":"gpt-5.3-codex","label":"GPT-5.3 Codex","description":"Optimized for agentic coding"},
            {"id":"gpt-5.2","label":"GPT-5.2","description":"General purpose reasoning"},
            {"id":"gpt-5.1-codex-mini","label":"GPT-5.1 Codex Mini","description":"Fast, efficient coding"}
        ])).unwrap()));
        self.catalog_rev += 1;
        cx.notify();
    }
}

#[cfg(feature = "browser-fixture")]
impl Pickers {
    /// Review fixture: open one picker as its trigger would.
    pub fn fixture_open(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_kind() != Some(kind) {
            self.toggle(kind, window, cx);
        }
    }

    /// Review fixture: a ready provider catalog, so the model picker renders
    /// real rows without an engine behind it.
    pub fn fixture_seed_catalog(&mut self, cx: &mut Context<Self>) {
        let descriptor = |id, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            installed: true,
            can_install: false,
            enabled: Some(true),
            reasoning_levels: Vec::new(),
            steering_mode: zuse_protocol::SteeringMode::StepBoundary,
            supports_steering: false,
            signed_in: None,
        };
        self.harnesses = Loadable::Ready(vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
            descriptor(HarnessId::Codex, "Codex"),
        ]);
        let model = |id: &str, label: &str| Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ],
            options: Vec::new(),
        };
        // The selected model runs fast by default (the Advanced footer
        // toggle, on).
        let fast = |mut model: Model| {
            model.options = serde_json::from_value(serde_json::json!([
                {"id":"serviceTier","label":"Service tier","defaultChoice":"fast","choices":[
                    {"id":"standard","label":"Standard"},{"id":"fast","label":"Fast"}]}
            ]))
            .unwrap();
            model
        };
        self.apply_model_catalog(
            HarnessId::ClaudeCode,
            Loadable::Ready(vec![
                model("claude-opus-4-1", "Claude Opus 4.1"),
                fast(model("claude-sonnet-4-6", "claude-sonnet-4-6")),
                model("claude-sonnet-4-5", "Claude Sonnet 4.5"),
                model("claude-haiku-4-5", "Claude Haiku 4.5"),
            ]),
            cx,
        );
        self.apply_model_catalog(
            HarnessId::Codex,
            Loadable::Ready(vec![model("gpt-5-codex", "GPT-5 Codex")]),
            cx,
        );
    }
}

fn model_catalog_notice(error: &str) -> String {
    let error = error.to_ascii_lowercase();
    if error.contains("auth_required")
        || error.contains("api key")
        || error.contains("unauthorized")
    {
        "This agent needs sign-in. Open Settings → Providers, then retry.".into()
    } else {
        "Could not load this agent’s models. Retry or check Settings → Providers.".into()
    }
}
