use super::*;
use gpui::{TestAppContext, VisualTestContext};

#[test]
fn provider_brand_colors_are_consistent_across_pickers() {
    use zuse_protocol::HarnessId;
    for harness in [
        HarnessId::Codex,
        HarnessId::ClaudeCode,
        HarnessId::Gemini,
        HarnessId::Kiro,
        HarnessId::Zuse,
    ] {
        assert!(
            crate::pickers::harness_brand_icon(harness).1.is_some(),
            "{harness:?}"
        );
    }
    let codex = crate::pickers::harness_brand_icon(HarnessId::Codex)
        .1
        .unwrap();
    let purple: gpui::Hsla = gpui::rgb(0x8b5cf6).into();
    assert_eq!((codex.h, codex.s, codex.l), (purple.h, purple.s, purple.l));
    assert!(
        crate::pickers::harness_brand_icon(HarnessId::Cursor)
            .1
            .is_none()
    );
}

struct SidebarHeadings;

impl Render for SidebarHeadings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div().size_full().flex().flex_col().children([
            spaces::sidebar_section_heading(theme, "Zappers", "test-zappers-label").child(
                spaces::sidebar_header_button("test-zappers-plus", icons::PLUS, theme)
                    .debug_selector(|| "test-zappers-plus".into()),
            ),
            spaces::sidebar_section_heading(theme, "Threads", "test-threads-label").child(
                spaces::sidebar_header_button("test-threads-plus", icons::PLUS, theme)
                    .debug_selector(|| "test-threads-plus".into()),
            ),
        ])
    }
}

#[gpui::test]
fn sidebar_section_labels_and_buttons_share_geometry(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
    });
    let (_, cx) = cx.add_window_view(|_, _| SidebarHeadings);
    cx.update(|window, cx| window.draw(cx).clear());
    let zappers = cx.debug_bounds("test-zappers-label").unwrap();
    let threads = cx.debug_bounds("test-threads-label").unwrap();
    let zappers_plus = cx.debug_bounds("test-zappers-plus").unwrap();
    let threads_plus = cx.debug_bounds("test-threads-plus").unwrap();
    assert_eq!(zappers.left(), threads.left());
    assert_eq!(zappers.size, threads.size);
    assert_eq!(zappers_plus.left(), threads_plus.left());
    assert_eq!(zappers_plus.size, threads_plus.size);
}

#[test]
fn branch_errors_explain_worktree_conflicts_and_preserve_other_details() {
    let message = format_branch_error(
        "main",
        "git: fatal: 'main' is already used by worktree at '/tmp/main worktree'",
    );
    assert!(message.contains("Branch already in use"));
    assert!(message.contains("/tmp/main worktree"));
    assert!(message.contains("Switch that worktree"));
    assert!(
        format_branch_error("feature", "local changes would be overwritten")
            .contains("Commit or stash")
    );
    let conflict = format_branch_error(
        "feature",
        "git: error: Your local changes would be overwritten by checkout:\n app/page.tsx\nAborting",
    );
    assert_eq!(
        conflict,
        "Uncommitted changes\nCommit or stash your changes before switching branches."
    );
    assert!(format_branch_error("feature", "permission denied").contains("permission denied"));
}

#[gpui::test]
fn branch_label_uses_current_checkout_not_run_start_context(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    shell.update(cx, |shell, cx| {
        shell.branch_menu.loaded_for = Some("local:/repo".into());
        shell.branch_menu.current_branch = Some("main".into());
        assert_eq!(shell.branch_target(cx).unwrap().branch, "main");
        shell.branch_menu.loaded_for = Some("local:/another".into());
        assert_eq!(
            shell.branch_target(cx).unwrap().branch,
            "feature/a-long-branch-name-to-test-truncation"
        );
    });
}

struct TabHost {
    shell: Entity<Shell>,
    _directory: tempfile::TempDir,
}

impl Render for TabHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.shell.update(cx, |shell, cx| {
            let theme = Theme::of(cx).clone();
            let session_info = shell
                .session_info_visible()
                .then(|| shell.render_session_info(1.0, 600.0, cx))
                .flatten();
            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(Theme::TITLEBAR_HEIGHT))
                        .child(shell.render_title_bar(cx)),
                )
                .child(shell.render_sidebar_nav(&theme, cx))
                .child(div().flex_1())
                .child(shell.composer.clone())
                .children(session_info)
        })
    }
}

#[gpui::test]
fn sidebar_opens_plugins_and_pull_requests_without_overview_tabs(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    let plugins = cx.debug_bounds("sidebar-plugins-row").unwrap().center();
    cx.simulate_click(plugins, gpui::Modifiers::default());
    assert!(shell.read_with(cx, |shell, _| matches!(shell.route, Route::Plugins)));
    cx.update(|window, cx| window.draw(cx).clear());
    let requests = cx
        .debug_bounds("sidebar-pull-requests-row")
        .unwrap()
        .center();
    cx.simulate_click(requests, gpui::Modifiers::default());
    assert!(shell.read_with(cx, |shell, _| matches!(shell.route, Route::PullRequests)));
}

#[gpui::test]
fn conversations_and_add_button_precede_other_tabs(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    let (chat_id, diff_id) = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let chat_id = shell.state.read(cx).selected_chat.clone().unwrap();
            shell.active_chat = chat_id.clone();
            shell.open_review_tab(window, cx);
            (chat_id, shell.active_main_diff(cx).unwrap())
        })
    });
    cx.update(|window, cx| window.draw(cx).clear());
    let chat = cx
        .debug_bounds(Box::leak(
            format!("thread-conversation-{chat_id}").into_boxed_str(),
        ))
        .unwrap();
    let add = cx.debug_bounds("add-thread-conversation").unwrap();
    let diff = cx
        .debug_bounds(Box::leak(
            format!("main-tab-diff-{diff_id}").into_boxed_str(),
        ))
        .unwrap();
    assert!(chat.right() <= add.left());
    assert!(add.right() <= diff.left());
    assert!(chat.top() >= px(0.) && chat.bottom() <= px(Theme::TITLEBAR_HEIGHT));
    assert_eq!(chat.size.height, px(28.));
}

#[gpui::test]
fn sibling_chats_keep_thread_review_tabs(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            let mut sibling = shell.state.read(cx).chats[0].clone();
            sibling.id = "second-chat".into();
            sibling.thread_id = Some("chat".into());
            shell.state.update(cx, |state, _| state.chats.push(sibling));
            shell.active_chat = "chat".into();
            shell.open_review_tab(window, cx);
            let review = shell.active_main_diff(cx).unwrap();
            shell.open_chat("second-chat".into(), cx);
            shell.active_chat = "second-chat".into();
            assert_eq!(shell.main_tab_rows(cx).len(), 1);
            assert_eq!(shell.active_main_tab(cx), None);
            shell.activate_main_tab(Some(main_tabs::MainTab::Diff(review)), cx);
            assert_eq!(shell.active_main_diff(cx), Some(review));
        })
    });
}

#[gpui::test]
fn delayed_new_chat_does_not_interrupt_other_navigation(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    shell.update(cx, |shell, cx| {
        let mut created = shell.state.read(cx).chats[0].clone();
        created.id = "created-chat".into();
        created.thread_id = Some("chat".into());
        shell.open_plugins_page(cx);
        shell.finish_thread_conversation("chat", Ok(created), cx);
        assert!(matches!(shell.route, Route::Plugins));
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("chat"));
        assert!(
            shell
                .state
                .read(cx)
                .chats
                .iter()
                .any(|chat| chat.id == "created-chat")
        );
        shell.open_chat("chat".into(), cx);
        let mut next = shell.state.read(cx).chats[0].clone();
        next.id = "next-chat".into();
        next.thread_id = Some("chat".into());
        shell.finish_thread_conversation("chat", Ok(next), cx);
        assert_eq!(
            shell.state.read(cx).selected_chat.as_deref(),
            Some("next-chat")
        );
    });
}

fn setup(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
    let directory = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_base::init(cx);
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
    let (host, cx) = cx.add_window_view(|_, cx| {
        let shell = cx.new(|cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                let mut chat =
                    crate::shell::tests::chat_with_path(Some("/repo"), Some(("/repo", "/repo")));
                chat.device_id = "local".into();
                chat.source_context.as_mut().unwrap().branch =
                    "feature/a-long-branch-name-to-test-truncation".into();
                state.local_device_id = Some("local".into());
                state.no_project = true;
                state.selected_chat = Some(chat.id.clone());
                state.chats.push(chat);
                state
                    .checkout_diff_stats
                    .push(crate::state::CheckoutDiffStat {
                        checkout_id: "checkout".into(),
                        device_id: "local".into(),
                        cwd: "/repo".into(),
                        additions: 2,
                        deletions: 5,
                        files: 2,
                    });
                state
            });
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    target: None,
                    data_dir: directory.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zuse_protocol::HarnessId::Mock,
                },
                cx,
            );
            shell.reduced_motion = true;
            shell
                .composer
                .update(cx, |composer, _| composer.focus_pending = false);
            shell
        });
        TabHost {
            shell,
            _directory: directory,
        }
    });
    let shell = host.read_with(cx, |host, _| host.shell.clone());
    cx.update(|window, cx| window.draw(cx).clear());
    (shell, cx)
}

#[gpui::test]
fn branch_picker_opens_left_of_the_session_card(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    for width in [600.0, 900.0] {
        cx.simulate_resize(gpui::size(px(width), px(700.0)));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.session_info_overlay = true;
                shell
                    .composer
                    .update(cx, |composer, cx| composer.set_available_width(width, cx));
            });
            window.refresh();
            window.draw(cx).clear();
        });
        let branch = cx.debug_bounds("session-info-branch").expect("branch row");
        let card = cx.debug_bounds("session-info").expect("session card");
        let input = shell.read_with(cx, |shell, cx| {
            shell
                .composer
                .read(cx)
                .surface_bounds()
                .get()
                .expect("composer surface")
        });
        assert!(branch.left() >= card.left());
        assert!(branch.right() <= card.right());
        assert!(branch.bottom() <= input.top());
        assert!(cx.debug_bounds("top-bar-branch").is_none());
        if !shell.read_with(cx, |shell, _| shell.branch_menu.menu.is_open()) {
            cx.simulate_click(branch.center(), gpui::Modifiers::default());
        }
        cx.update(|window, cx| window.draw(cx).clear());
        assert!(
            shell.read_with(cx, |shell, _| shell.session_info_visible()),
            "session dismissed on branch press"
        );
        assert!(
            shell.read_with(cx, |shell, _| shell.branch_menu.menu.is_open()),
            "branch picker did not open"
        );
        let menu = cx.debug_bounds("branch-menu").expect("branch menu");
        assert!(menu.right() <= card.left(), "menu {menu:?}, card {card:?}");
        assert!(menu.left() >= px(0.0));
        assert!(shell.read_with(cx, |shell, _| shell.session_info_visible()));
    }
    let branch = cx.debug_bounds("session-info-branch").unwrap();
    cx.simulate_click(branch.center(), gpui::Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear());
    assert!(cx.debug_bounds("branch-menu").is_none());
    assert!(shell.read_with(cx, |shell, _| shell.session_info_visible()));
}

#[gpui::test]
fn attachment_menu_stays_above_its_button_when_the_prompt_grows(cx: &mut TestAppContext) {
    let (shell, cx) = setup(cx);
    cx.update(|_, cx| crate::motion::set_reduced_motion(cx, true));
    for text in ["", "First line\nSecond line\nThird line\nFourth line"] {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.composer.update(cx, |composer, cx| {
                    composer
                        .input
                        .update(cx, |input, cx| input.set_text(text, cx));
                    cx.notify();
                });
            });
            window.draw(cx).clear();
        });
        let button = cx.debug_bounds("composer-attach").expect("attach button");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear());
        let menu = cx
            .debug_bounds("composer-add-menu")
            .expect("attachment menu");
        assert!((menu.left() - button.left()).abs() <= px(1.0));
        assert!(menu.bottom() <= button.top());
        assert!(
            button.top() - menu.bottom() <= px(12.0),
            "menu {menu:?}, button {button:?}"
        );
        assert!(
            menu.size.height <= px(90.0),
            "single-line menu rows: {menu:?}"
        );
        assert!(menu.size.width <= px(168.0), "compact menu: {menu:?}");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear());
    }
}
