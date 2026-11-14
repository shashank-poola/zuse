//! Provider browsing and variable-height model rows over the existing run controller.
use super::*;
use std::{cell::RefCell, sync::Arc};
use zuse_viewport::model_picker_style as ui;

fn picker_brand(harness: HarnessId, fallback: gpui::Hsla) -> (&'static str, gpui::Hsla) {
    let (path, tint) = harness_brand_icon(harness);
    (path, tint.unwrap_or(fallback))
}

#[derive(Clone)]
enum Item {
    Label(String),
    Header {
        provider: HarnessId,
        name: SharedString,
        count: usize,
        current: bool,
    },
    Row {
        row: ModelRowData,
        index: usize,
        provider: bool,
        bottom: f32,
    },
}
struct Cache {
    key: ModelRowsKey,
    signature: String,
    items: Arc<Vec<Item>>,
    height: f32,
}
pub(super) struct MenuState {
    list: gpui::ListState,
    cache: RefCell<Option<Cache>>,
}
impl Default for MenuState {
    fn default() -> Self {
        Self {
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(256.)),
            cache: RefCell::new(None),
        }
    }
}

impl Pickers {
    pub(super) fn settings_menu_height(&self, _cx: &App) -> f32 {
        ui::MAX_HEIGHT
    }

    /// A provider's discovery failure belongs to that provider's tab. The
    /// All view keeps listing every healthy provider instead of leading with
    /// one agent's raw error (e.g. an agent that was never signed in).
    fn scoped_model_refresh_error(&self, cx: &App) -> Option<String> {
        let scope = self.model_scope?;
        self.rail_descriptors(cx)
            .iter()
            .any(|p| p.id == scope)
            .then(|| self.model_refresh_errors.get(&scope))
            .flatten()
            .map(|error| super::model_catalog_notice(error))
    }

    fn reference_key(&self, cx: &App) -> ModelRowsKey {
        ModelRowsKey {
            query: self.search.read(cx).text().trim().to_lowercase(),
            rail: self.model_rail,
            scope: self.model_scope,
            effective: self.effective_harness(cx),
            locked: self.harness_locked(cx),
            catalog_rev: self.catalog_rev,
            selected: self.effective_model_id(cx).map(str::to_owned),
        }
    }
    pub(super) fn reference_model_rows(&self, cx: &App) -> Vec<ModelRowData> {
        let query = self.search.read(cx).text().trim().to_lowercase();
        let current = self.effective_harness(cx);
        let mut providers = self.rail_descriptors(cx);
        if self.model_scope.is_none() && query.is_empty() {
            providers.sort_by_key(|p| Some(p.id) != current);
        }
        let mut rows = vec![];
        for provider in providers {
            if self.model_scope.is_some_and(|scope| scope != provider.id) {
                continue;
            }
            if let Some(models) = self
                .model_catalog(cx)
                .get(&provider.id)
                .and_then(Loadable::ready)
            {
                for model in models {
                    if self.model_rail == ModelRail::Favorites
                        && !self.defaults.is_favorite(provider.id, &model.id)
                    {
                        continue;
                    }
                    if query.is_empty()
                        || model.id.to_lowercase().contains(&query)
                        || model.label.to_lowercase().contains(&query)
                    {
                        rows.push(ModelRowData {
                            harness: provider.id,
                            harness_name: provider.name.clone().into(),
                            model: model.clone(),
                            selected_only: false,
                            ambiguous: false,
                        });
                    }
                }
                if Some(provider.id) == current {
                    if let Some(id) = self
                        .effective_model_id(cx)
                        .filter(|id| !models.iter().any(|m| m.id == *id))
                    {
                        if self.model_rail == ModelRail::Favorites
                            && !self.defaults.is_favorite(provider.id, id)
                        {
                            continue;
                        }
                        let label = self.selected_model_label(cx).unwrap_or_else(|| id.into());
                        if query.is_empty()
                            || id.to_lowercase().contains(&query)
                            || label.to_lowercase().contains(&query)
                        {
                            rows.push(ModelRowData {
                                harness: provider.id,
                                harness_name: provider.name.into(),
                                model: Model {
                                    id: id.into(),
                                    label,
                                    description: None,
                                    reasoning_levels: vec![],
                                    options: vec![],
                                },
                                selected_only: true,
                                ambiguous: false,
                            });
                        }
                    }
                }
            }
        }
        // Catalogs can contain repeated entries for the same model ID. Keep
        // separate IDs, but make same-named choices distinguishable below.
        let mut seen = std::collections::HashSet::new();
        rows.retain(|row| seen.insert((row.harness, row.model.id.clone())));
        mark_ambiguous(&mut rows);
        rows
    }
    fn reference_items(&self, cx: &App) -> (Arc<Vec<Item>>, f32) {
        let key = self.reference_key(cx);
        if let Some(cache) = self
            .model_menu
            .cache
            .borrow()
            .as_ref()
            .filter(|c| c.key == key)
        {
            return (cache.items.clone(), cache.height);
        }
        let rows = self.model_rows(cx);
        let favorites = self.model_rail == ModelRail::Favorites;
        let grouped = self.model_scope.is_none() && key.query.is_empty() && !favorites;
        let provider = self.model_scope.is_none() && !grouped && !favorites;
        let signature = serde_json::json!([
            key.query,
            self.model_scope,
            rows.iter()
                .map(|r| (&r.harness, &r.model.id, &r.model.label, &r.harness_name))
                .collect::<Vec<_>>()
        ])
        .to_string();
        let mut items = vec![];
        let mut height = 10.;
        if !rows.is_empty() {
            let heading = self
                .model_scope
                .and_then(|scope| {
                    self.rail_descriptors(cx)
                        .into_iter()
                        .find(|p| p.id == scope)
                        .map(|p| p.name)
                })
                .unwrap_or_else(|| if favorites { "Favorites" } else { "Models" }.to_string());
            items.push(Item::Label(heading));
            height += ui::LABEL_HEIGHT;
            for (index, row) in rows.iter().enumerate() {
                if grouped && (index == 0 || rows[index - 1].harness != row.harness) {
                    let count = rows[index..]
                        .iter()
                        .take_while(|r| r.harness == row.harness)
                        .count();
                    items.push(Item::Header {
                        provider: row.harness,
                        name: row.harness_name.clone(),
                        count,
                        current: Some(row.harness) == key.effective,
                    });
                    height += ui::GROUP_HEADER_HEIGHT;
                }
                let last =
                    index + 1 == rows.len() || grouped && rows[index + 1].harness != row.harness;
                let bottom = if grouped && last {
                    8.
                } else if last {
                    0.
                } else {
                    2.
                };
                items.push(Item::Row {
                    row: row.clone(),
                    index,
                    provider: provider || row.ambiguous,
                    bottom,
                });
                height += if provider || row.ambiguous {
                    ui::PROVIDER_ROW_HEIGHT
                } else {
                    ui::ROW_HEIGHT
                };
                height += bottom;
            }
        } else {
            height += 64.;
        }
        let items = Arc::new(items);
        if self
            .model_menu
            .cache
            .borrow()
            .as_ref()
            .is_none_or(|c| c.signature != signature)
        {
            self.model_menu.list.reset(items.len());
        }
        *self.model_menu.cache.borrow_mut() = Some(Cache {
            key,
            signature,
            items: items.clone(),
            height,
        });
        (items, height)
    }
    pub(super) fn scroll_reference_row(&self, row: usize, cx: &App) {
        let (items, _) = self.reference_items(cx);
        if let Some(index) = items
            .iter()
            .position(|i| matches!(i,Item::Row{index,..}if *index==row))
        {
            self.model_menu.list.scroll_to_reveal_item(index);
        }
    }
    pub(super) fn reset_reference_scroll(&self) {
        self.model_menu.list.scroll_to(gpui::ListOffset::default());
    }

    fn reference_item(&mut self, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        match item {
            Item::Label(label) => ui::section(label.clone()).into_any_element(),
            Item::Header {
                provider,
                name,
                count,
                current,
            } => {
                let (path, tint) = picker_brand(*provider, Theme::of(cx).text);
                ui::provider_heading(
                    name.clone(),
                    crate::icons::brand_icon(path, 14., tint),
                    *count,
                    *current,
                )
                .into_any_element()
            }
            Item::Row {
                row,
                index,
                provider,
                bottom,
            } => {
                let theme = Theme::of(cx).for_popup();
                let (path, tint) = picker_brand(row.harness, theme.text_muted);
                let selected = self.effective_harness(cx) == Some(row.harness)
                    && self
                        .selected_model(cx)
                        .is_some_and(|m| m.id == row.model.id);
                let favorite = self.defaults.is_favorite(row.harness, &row.model.id);
                let harness = row.harness;
                let click_model = row.model.id.clone();
                let keyboard_model = click_model.clone();
                let index = *index;
                let star = ui::favorite_button(("model-favorite", index), favorite)
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_model_favorite(harness, &click_model, cx);
                    }))
                    .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                        if matches!(e.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.toggle_model_favorite(harness, &keyboard_model, cx);
                        }
                    }));
                div()
                    .pb(px(*bottom))
                    .child(
                        ui::model_row(
                            ("model-row", index),
                            row.model.label.clone(),
                            if row.ambiguous {
                                Some(SharedString::from(row.model.id.clone()))
                            } else {
                                provider.then(|| row.harness_name.clone())
                            },
                            crate::icons::brand_icon(path, 16., tint),
                            selected,
                            self.compact_keyboard && self.active == index,
                            (index < 9).then_some(index + 1),
                            star,
                        )
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                cx.stop_propagation();
                                this.activate_model_index(index, cx);
                            }
                        }))
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered {
                                this.active = index;
                                this.compact_keyboard = false;
                                cx.notify();
                            }
                        }))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.activate_model_index(index, cx)),
                        ),
                    )
                    .into_any_element()
            }
        }
    }
    fn scope_model_menu(&mut self, scope: Option<HarnessId>, cx: &mut Context<Self>) {
        self.model_scope = scope;
        self.model_rail = if scope.is_none() {
            ModelRail::Favorites
        } else {
            ModelRail::Harness
        };
        if let Some(provider) = scope {
            self.ensure_models(provider, false, cx);
        } else {
            self.prefetch_models(false, cx);
        }
        self.active = 0;
        self.refresh_compact_placeholder(cx);
        cx.notify();
    }
    pub(super) fn render_reference_menu(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.compact_model_list = true;
        if self.model_scope.is_none() && self.model_rail != ModelRail::Favorites {
            self.model_scope = self
                .effective_harness(cx)
                .or_else(|| self.rail_descriptors(cx).first().map(|p| p.id));
        }
        let theme = Theme::of(cx).for_popup();
        let (items, _) = self.reference_items(cx);
        let empty = items.is_empty();
        let error = if self.cloud_models_active(cx) {
            self.cloud.model_error.clone()
        } else {
            None
        }
        .or_else(|| match self.harness_catalog(cx) {
            Loadable::Error(message) => Some(message.clone()),
            _ => (!self.cloud_models_active(cx))
                .then(|| self.scoped_model_refresh_error(cx))
                .flatten(),
        });
        let loading = matches!(self.harness_catalog(cx), Loadable::Loading | Loadable::Idle)
            || self.rail_descriptors(cx).iter().any(|p| {
                self.model_scope.is_none_or(|scope| scope == p.id)
                    && matches!(
                        self.model_catalog(cx).get(&p.id),
                        None | Some(Loadable::Loading | Loadable::Idle)
                    )
            });
        let providers = self.rail_descriptors(cx);
        self.compact_motion.frame_height = ui::MAX_HEIGHT.min(self.menu_geometry().height);
        let mut rail = div()
            .id("model-provider-rail")
            .overflow_y_scroll()
            .w(px(44.))
            .flex_none()
            .p(px(5.))
            .border_r_1()
            .border_color(theme.border.opacity(0.5))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.))
            .child(
                ui::provider_tab(
                    "model-tab-favorites",
                    "Favorite models",
                    self.model_rail == ModelRail::Favorites,
                    false,
                    Some(crate::icons::brand_icon(
                        crate::icons::STAR,
                        16.,
                        theme.text_muted,
                    )),
                )
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if matches!(e.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.scope_model_menu(None, cx);
                    }
                }))
                .on_click(cx.listener(|this, _, _, cx| this.scope_model_menu(None, cx))),
            );
        for (index, p) in providers.iter().enumerate() {
            let provider = p.id;
            let (path, tint) = picker_brand(provider, theme.text_muted);
            rail = rail.child(
                ui::provider_tab(
                    ("harness-tab", index),
                    format!("{} models", p.name),
                    self.model_rail != ModelRail::Favorites && self.model_scope == Some(provider),
                    false,
                    Some(crate::icons::brand_icon(path, 16., tint)),
                )
                .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                    if matches!(e.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.scope_model_menu(Some(provider), cx);
                    }
                }))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.scope_model_menu(Some(provider), cx)),
                ),
            );
        }
        let list = gpui::list(
            self.model_menu.list.clone(),
            cx.processor(move |this, index, _, cx| this.reference_item(&items[index], cx)),
        )
        .size_full();
        let search = div()
            .flex_none()
            .h(px(ui::SEARCH_HEIGHT))
            .px(px(14.))
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .flex()
            .items_center()
            .child(div().flex_1().min_w_0().child(self.search.clone()));
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .when_some(error, |body, message| {
                body.child(self.retry_row(
                    "model-menu-retry",
                    &message,
                    PickerKind::HarnessModel,
                    &theme,
                    cx,
                ))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(ui::LIST_INSET))
                    .pb(px(10.))
                    .when(!empty, |v| v.child(list))
                    .when(empty, |v| {
                        v.child(
                            div()
                                .px(px(12.))
                                .py(px(24.))
                                .text_size(px(12.))
                                .text_color(theme.text_muted)
                                .child(if loading {
                                    crate::localization::text(
                                        "providers-model_picker_checking_available_agents",
                                        cx,
                                    )
                                } else if self.model_rail == ModelRail::Favorites
                                    && self.search.read(cx).text().trim().is_empty()
                                {
                                    "No favorites yet. Star a model to add it here.".to_string()
                                } else {
                                    "No models match your search.".to_string()
                                }),
                        )
                    }),
            );
        let content = div()
            .size_full()
            .flex()
            .flex_col()
            .child(search)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_stretch()
                    .child(rail)
                    .child(body),
            )
            .into_any_element();
        self.popover_frame_flush(ui::WIDTH, content, cx)
    }
}
