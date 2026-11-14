//! Cloud tasks share project headings with local checkouts. These are real
//! account task rows, never synthetic local chats or guest connections.
use super::*;
use serde_json::Value;

pub(super) struct CloudSidebarGroup {
    pub group: Option<(String, String)>,
    pub project: Value,
    pub rows: Vec<Value>,
}
impl Shell {
    pub(super) fn has_offline_cloud_navigation(&self, cx: &App) -> bool {
        let state = self.state.read(cx);
        matches!(state.connection, ConnectionStatus::Failed(_))
            && state.engine().is_some()
            && !state.cloud_sidebar_projects().is_empty()
    }
    /// Account navigation stays available while the local execution lane fails.
    /// This never changes its connection state or installs a local composer.
    pub(super) fn render_offline_cloud_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let groups = self.cloud_sidebar_groups(cx);
        let mut elements = Vec::new();
        for group in groups {
            let project = group.project;
            let title = group
                .group
                .as_ref()
                .map(|(_, name)| name.as_str())
                .or_else(|| project["displayName"].as_str())
                .unwrap_or("Cloud project")
                .to_owned();
            let id = project["projectId"].as_str().unwrap_or_default().to_owned();
            let header = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .py(px(8.))
                .child(
                    icon(icons::DITHER_CLOUD)
                        .size(px(14.))
                        .text_color(theme.text_muted),
                )
                .child(div().flex_1().text_size(px(12.)).child(title))
                .child(
                    div()
                        .id(SharedString::from(format!("offline-cloud-new-{id}")))
                        .cursor_pointer()
                        .px(px(5.))
                        .py(px(3.))
                        .rounded_md()
                        .text_size(px(11.))
                        .child("New thread")
                        .hover(|v| v.bg(theme.element_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_new_cloud_session(project.clone(), cx)
                        })),
                );
            let mut rows = Vec::new();
            for row in group.rows {
                if let Some((_, _, row)) = self.render_cloud_sidebar_row(row, &theme, cx) {
                    rows.push(row)
                }
            }
            elements.push(div().child(header).children(rows).into_any_element());
        }
        div()
            .id("offline-cloud-sidebar")
            .w(px(self.settings.sidebar_width.max(SIDEBAR_MIN)))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px(px(16.))
                    .py(px(16.))
                    .text_size(px(14.))
                    .child("Cloud conversations"),
            )
            .child(
                div()
                    .id("offline-cloud-projects")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(6.))
                    .children(elements),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child("Local computer offline · Cloud access is separate"),
            )
            .into_any_element()
    }
    pub(super) fn cloud_sidebar_groups(&self, cx: &App) -> Vec<CloudSidebarGroup> {
        let state = self.state.read(cx);
        let workspaces = state.cloud_sidebar_workspaces();
        let filter_key = self
            .settings
            .space_filter
            .as_deref()
            .and_then(|id| state.space_row(id))
            .map(zuse_protocol::view::project_key);
        state
            .cloud_sidebar_projects()
            .into_iter()
            .filter_map(|project| {
                let identity =
                    zuse_protocol::presentation::cloud_project_group(&project, &state.spaces)?;
                if filter_key.as_ref().is_some_and(|key| key != &identity.key) {
                    return None;
                }
                let group = match self.settings.sidebar_organization {
                    SidebarOrganization::ByProject => Some((identity.key, identity.name)),
                    SidebarOrganization::ByDevice => {
                        Some(("cloud-computers".into(), "Cloud computers".into()))
                    }
                    SidebarOrganization::InOneList => None,
                };
                let mut rows: Vec<_> = workspaces
                    .iter()
                    .filter(|row| row["projectId"].as_str() == Some(identity.project_id.as_str()))
                    .cloned()
                    .collect();
                rows.sort_by(|a, b| {
                    b["updatedAt"]
                        .as_i64()
                        .unwrap_or_default()
                        .cmp(&a["updatedAt"].as_i64().unwrap_or_default())
                });
                Some(CloudSidebarGroup {
                    group,
                    project,
                    rows,
                })
            })
            .collect()
    }
    /// The conversation target for a cloud task row, when it can open.
    fn cloud_row_target(&self, row: &Value, cx: &App) -> Option<crate::state::CloudTarget> {
        let workspace = row["workspaceId"].as_str().filter(|id| !id.is_empty())?;
        let session = row["initialSessionId"]
            .as_str()
            .filter(|id| !id.is_empty())?;
        let state = self.state.read(cx);
        Some(crate::state::CloudTarget {
            controller: state.engine()?.controller(),
            subject: state.cloud_user_id()?.to_owned(),
            organization: state.cloud_organization_id().map(str::to_owned),
            workspace: workspace.to_owned(),
            initial_session: session.to_owned(),
        })
    }
    /// Open the cloud task behind a `cloud:<workspace>` sidebar row.
    pub(super) fn open_cloud_sidebar_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(workspace) = id.strip_prefix(super::CLOUD_ROW_PREFIX) else {
            return;
        };
        let row = self
            .state
            .read(cx)
            .cloud_sidebar_workspaces()
            .into_iter()
            .find(|row| row["workspaceId"].as_str() == Some(workspace));
        let Some(row) = row else { return };
        if let Some(target) = self.cloud_row_target(&row, cx) {
            self.open_cloud_conversation(target, row, serde_json::json!({}), cx);
        }
    }
    /// A cloud task drawn exactly like a local chat row: the project's
    /// avatar, its title and age, and the same activity corner. Only the
    /// click target differs.
    pub(super) fn render_cloud_sidebar_row(
        &self,
        row: Value,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<(String, f32, AnyElement)> {
        self.cloud_row_target(&row, cx)?;
        let workspace = row["workspaceId"].as_str()?.to_owned();
        let state = self.state.read(cx);
        let selected = self
            .cloud_conversation
            .as_ref()
            .is_some_and(|view| view.read(cx).target.workspace == workspace)
            || state
                .engine()
                .and_then(|engine| engine.cloud_workspace())
                .as_deref()
                == Some(workspace.as_str());
        let title = transcript::single_line(
            row["title"]
                .as_str()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("New session"),
        );
        let now = Utc::now();
        let updated = row["updatedAt"]
            .as_i64()
            .or_else(|| row["createdAt"].as_i64())
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or(now);
        let time_ago = zuse_protocol::view::format_time_ago(updated, now);
        // Starting and resuming read as activity, like a local run; an idle
        // or sleeping computer shows its age, like any idle chat.
        let status = match row["state"].as_str() {
            Some("ready" | "paused" | "pausing" | "archived") => zuse_protocol::ChatIndicator::Idle,
            Some("failed" | "error" | "blocked") => zuse_protocol::ChatIndicator::Errored,
            _ => zuse_protocol::ChatIndicator::Working,
        };
        let project_group = self.settings.sidebar_organization == SidebarOrganization::ByProject;
        let height = sidebar_row_height(
            self.sidebar_compact_rows(),
            self.sidebar_project_labels(),
            project_group,
        );
        let id = format!("{}{workspace}", super::CLOUD_ROW_PREFIX);
        let project = self.cloud_row_project_name(&row, cx);
        let element = self.render_chat_row(
            id.clone(),
            title.into(),
            time_ago.into(),
            project.into(),
            None,
            None,
            None,
            status,
            selected,
            false,
            false,
            None,
            None,
            self.settings.sidebar_show_project_icon && !project_group,
            project_group,
            None,
            theme,
            cx,
        );
        Some((id, height, element))
    }
    fn cloud_row_project_name(&self, row: &Value, cx: &App) -> String {
        let state = self.state.read(cx);
        let project_id = row["projectId"].as_str().unwrap_or_default();
        state
            .cloud_sidebar_projects()
            .into_iter()
            .find(|project| project["projectId"].as_str() == Some(project_id))
            .and_then(|project| {
                zuse_protocol::presentation::cloud_project_group(&project, &state.spaces)
                    .map(|group| group.name)
            })
            .unwrap_or_else(|| "Cloud".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use serde_json::json;

    struct Pending;
    #[async_trait::async_trait]
    impl zuse_rpc::RpcService for Pending {
        async fn handle(
            &self,
            _: &str,
            _: Value,
        ) -> Result<zuse_rpc::RpcReply, zuse_rpc::RpcError> {
            Ok(zuse_rpc::RpcReply::Stream(Box::pin(
                futures::stream::pending(),
            )))
        }
    }
    struct ProjectList {
        shell: Entity<Shell>,
        _directory: tempfile::TempDir,
    }
    impl Render for ProjectList {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.shell.update(cx, |shell, cx| {
                let rows = shell.render_active_rows(&Theme::of(cx).clone(), cx);
                div()
                    .w(px(320.))
                    .flex()
                    .flex_col()
                    .children(rows.rows.into_iter().map(|(_, _, row)| row))
            })
        }
    }
    #[gpui::test]
    fn typed_cloud_workspaces_paint_under_project_and_pencil_opens_current_canvas(
        cx: &mut TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let directory = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            gpui_tokio::init_from_handle(cx, runtime.handle().clone());
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            settings::init(UiSettings::default(), directory.path(), cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
        });
        let controller = crate::state::EngineHandle::from_test_client(zuse_rpc::memory_client(
            std::sync::Arc::new(Pending),
        ));
        let (host,cx)=cx.add_window_view(|_,cx| {
            let shell=cx.new(|cx| {
                let state=cx.new(|_| {
                    let mut state=AppState::new();
                    state.apply_cloud_account(json!({"user":{"id":"owner"},"orgId":null}));
                    let rows=(1..=2).map(|n| {
                        // Exactly the controller LIST decode/re-encode path that
                        // previously discarded projectId before sidebar grouping.
                        let row:zuse_model::cloud::Workspace=serde_json::from_value(json!({
                            "workspaceId":format!("task-{n}"),"projectId":"project","providerId":"boxd",
                            "chatId":format!("session-{n}"),"initialSessionId":format!("session-{n}"),
                            "title":format!("Saved cloud chat {n}"),"branch":format!("task/branch-{n}"),
                            "state":"paused","desiredState":"paused","statusCode":"paused","revision":1,
                            "rpcProtocol":"zuse-rpc-v1","createdAt":n,"updatedAt":n
                        })).unwrap();serde_json::to_value(row).unwrap()
                    }).collect::<Vec<_>>();
                    assert!(state.apply_cloud_sidebar_catalog(
                        &json!({"accountId":"owner","organizationId":null,"ownerId":"owner","projects":[{"projectId":"project","displayName":"Cloud repo","repositoryIdentity":"github.com/owner/repo","canCreate":true}]}),
                        &json!({"accountId":"owner","organizationId":null,"workspaces":rows})));
                    state
                });
                state.update(cx,|state,_|state.set_test_engine(controller));
                let mut shell=Shell::new(state,EngineBootConfig {
                    target:None,data_dir:directory.path().into(),ipc_port:0,edge_url:"http://127.0.0.1:1".into(),
                    edge_token:None,org_id:None,workos_client_id:None,default_harness:zuse_protocol::HarnessId::Mock,
                },cx);
                shell.reduced_motion=true;
                shell.settings.sidebar_organization=SidebarOrganization::ByProject;
                shell.sidebar_collapsed_groups.clear();
                let project=shell.state.read(cx).cloud_sidebar_projects()[0].clone();
                shell.open_new_cloud_session(project,cx);
                let groups=shell.cloud_sidebar_groups(cx);
                assert_eq!(groups.len(),1);assert_eq!(groups[0].rows.len(),2);
                assert_eq!(groups[0].rows[0]["workspaceId"],"task-2");
                shell
            });
            ProjectList {shell,_directory:directory}
        });
        let shell = host.read_with(cx, |host, _| host.shell.clone());
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(cx.debug_bounds("chat-cloud:task-1").is_some());
        assert!(cx.debug_bounds("chat-cloud:task-2").is_some());
        let pencil = cx
            .debug_bounds("sidebar-group-new-chat-project:cloud-project:project")
            .expect("cloud project must have the same pencil action as a local project");
        let composer = shell.read_with(cx, |shell, _| shell.composer.entity_id());
        cx.simulate_mouse_move(pencil.center(), None, gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear());
        cx.simulate_click(pencil.center(), gpui::Modifiers::default());
        shell.read_with(cx, |shell, cx| {
            assert!(matches!(shell.route, Route::Chat));
            assert!(shell.cloud_conversation.is_none());
            assert_eq!(
                shell.composer.entity_id(),
                composer,
                "project pencil must reuse the current composer"
            );
            assert_eq!(
                shell.state.read(cx).cloud_project.as_ref().unwrap()["projectId"],
                "project"
            );
        });
    }
}

impl Shell {
    pub(super) fn render_cloud_sidebar_status(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.state.read(cx);
        if state.cloud_user_id().is_none() {
            return None;
        }
        // Background refreshes stay silent: the rows are already on screen
        // and update in place. Only a failure the user can act on shows.
        let loading = false;
        let label = state.cloud_sidebar_error()?.to_owned();
        Some(
            div()
                .id("cloud-sidebar-sync-status")
                .debug_selector(|| "cloud-sidebar-sync-status".into())
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .py(px(5.))
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .when(loading, |row| {
                    row.child(crate::loaders::mini_mono_spinner(
                        "cloud-sidebar-sync-spinner",
                        2.,
                        theme.text_muted,
                        cx.entity_id(),
                        cx,
                    ))
                })
                .child(div().flex_1().min_w_0().child(label))
                .child(
                    div()
                        .id("cloud-sidebar-retry")
                        .debug_selector(|| "cloud-sidebar-retry".into())
                        .cursor_pointer()
                        .child("Retry")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state
                                .update(cx, |state, cx| state.retry_cloud_sidebar(cx))
                        })),
                )
                .into_any_element(),
        )
    }
}
