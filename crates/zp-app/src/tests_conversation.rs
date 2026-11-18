//! Conversations, tabs and session management on GPUI's test platform with scripted
//! providers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{Entity, TestAppContext, WindowHandle, point, px};
use serde_json::json;
use tempfile::TempDir;
use zp_agent::AgentMessage;
use zp_ai::{AssistantContent, AssistantMessage, FauxProvider, FauxResponse, Models, TextContent};
use zp_session::{Session, SessionManager};

use crate::actions::{
    ActivateTab, CloseTab, ConfirmRename, NewChat, NextTab, PreviousTab, SendMessage,
};
use crate::conversation::ConversationView;
use crate::conversation::transcript::{Row, ToolStatus};
use crate::project::{ProjectWindow, RenameTarget};
use crate::store::{AppModels, AppStore, FileTrash};
use crate::tests::{project_dir, saved_state, setup};
use crate::workspace;

/// App with `providers` as the only models (the first one is the default).
pub(crate) fn setup_models(
    cx: &mut TestAppContext,
    providers: &[&FauxProvider],
    titles: bool,
) -> TempDir {
    cx.executor().allow_parking();
    let data = setup(cx, None);
    let providers: Vec<FauxProvider> = providers.iter().map(|p| (*p).clone()).collect();
    cx.update(|cx| {
        gpui_tokio::init(cx);
        let mut models = Models::new();
        for provider in providers {
            models.set_provider(Arc::new(provider));
        }
        AppModels::set(models, titles, cx);
        cx.set_global(FileTrash(|path| Ok(std::fs::remove_file(path)?)));
    });
    // Tools run without approvals unless a test asks for them.
    set_permission_mode(&data, zp_core::permissions::PermissionMode::FullAuto);
    data
}

/// Sets the default permission mode in the global settings.
pub(crate) fn set_permission_mode(data: &TempDir, mode: zp_core::permissions::PermissionMode) {
    let mut settings = zp_core::settings::Settings::default();
    settings.permissions.mode = Some(mode);
    settings
        .save(&zp_core::paths::settings_file(data.path()))
        .unwrap();
}

/// Lets Tokio-side work progress until `done` holds (or fails after a few seconds).
pub(crate) fn wait_until(
    cx: &mut TestAppContext,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    for _ in 0..600 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        cx.executor().advance_clock(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out");
}

pub(crate) fn open(cx: &mut TestAppContext, project: &Path) -> WindowHandle<ProjectWindow> {
    let window = cx
        .update(|cx| workspace::open_project(project, None, cx))
        .unwrap();
    cx.run_until_parked();
    window
}

pub(crate) fn active(
    window: WindowHandle<ProjectWindow>,
    cx: &mut TestAppContext,
) -> Entity<ConversationView> {
    window
        .update(cx, |view, _, _| view.active_conversation().unwrap().clone())
        .unwrap()
}

pub(crate) fn tab_count(window: WindowHandle<ProjectWindow>, cx: &mut TestAppContext) -> usize {
    window.update(cx, |view, _, _| view.tabs().len()).unwrap()
}

/// Types `text` into the active tab and sends it.
pub(crate) fn send(window: WindowHandle<ProjectWindow>, text: &str, cx: &mut TestAppContext) {
    let conversation = active(window, cx);
    conversation.update(cx, |view, cx| view.set_draft(text, cx));
    cx.dispatch_action(window.into(), SendMessage);
    cx.run_until_parked();
}

pub(crate) fn idle(conversation: &Entity<ConversationView>, rows: usize, cx: &mut TestAppContext) {
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| {
            !view.is_running() && view.transcript().len() == rows
        })
    });
}

fn assistant(text: &str) -> AgentMessage {
    let mut message = AssistantMessage::empty("faux", "faux", "faux-1");
    message
        .content
        .push(AssistantContent::Text(TextContent { text: text.into() }));
    message.into()
}

/// Writes a two-message session for `project` into the data directory.
fn write_session(data: &TempDir, project: &Path, first: &str) -> PathBuf {
    let manager = SessionManager::new(zp_core::paths::sessions_dir(data.path()));
    let mut session = manager.create(project);
    session.append_message(AgentMessage::user(first)).unwrap();
    session
        .append_message(assistant(&format!("re: {first}")))
        .unwrap();
    std::thread::sleep(Duration::from_millis(3));
    session.file().unwrap().to_path_buf()
}

#[gpui::test]
fn prompt_history_recalls_sent_text_and_restores_drafts(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("one"));
    faux.push(FauxResponse::text("two"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    cx.simulate_keystrokes(window.into(), "up down");
    assert_eq!(conversation.read_with(cx, |view, cx| view.draft(cx)), "");

    send(window, "first prompt", cx);
    idle(&conversation, 2, cx);
    send(window, "second prompt\nwith two lines", cx);
    idle(&conversation, 4, cx);
    conversation.update(cx, |view, cx| view.set_draft("unfinished draft", cx));
    cx.run_until_parked();
    for (key, expected) in [
        ("up", "second prompt\nwith two lines"),
        ("up", "first prompt"),
        ("up", "first prompt"),
        ("down", "second prompt\nwith two lines"),
        ("down", "unfinished draft"),
        ("down", "unfinished draft"),
    ] {
        cx.simulate_keystrokes(window.into(), key);
        cx.run_until_parked();
        assert_eq!(
            conversation.read_with(cx, |view, cx| view.draft(cx)),
            expected
        );
    }

    cx.simulate_keystrokes(window.into(), "up");
    cx.simulate_input(window.into(), " edited");
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "down");
    assert_eq!(
        conversation.read_with(cx, |view, cx| view.draft(cx)),
        "second prompt\nwith two lines edited"
    );
    assert_eq!(faux.requests().len(), 2, "recalling does not send a prompt");
}

#[gpui::test]
fn prompt_history_preserves_multiline_navigation_and_completion(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("done"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "sent prompt", cx);
    idle(&conversation, 2, cx);

    conversation.update(cx, |view, cx| view.set_draft("first\nsecond", cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "up");
    assert_eq!(
        conversation.read_with(cx, |view, cx| view.draft(cx)),
        "first\nsecond"
    );

    conversation.update(cx, |view, cx| view.set_draft("/", cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "down");
    assert_eq!(
        conversation.read_with(cx, |view, _| view.completion().unwrap().selected),
        1
    );
    cx.simulate_keystrokes(window.into(), "up");
    assert_eq!(
        conversation.read_with(cx, |view, _| view.completion().unwrap().selected),
        0
    );
    assert_eq!(conversation.read_with(cx, |view, cx| view.draft(cx)), "/");
}

#[gpui::test]
fn prompt_history_restores_saved_messages_and_is_per_tab(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let file = write_session(&data, &project, "saved prompt");
    let window = open(cx, &project);
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&file, false, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let conversation = active(window, cx);
    cx.simulate_keystrokes(window.into(), "up");
    assert_eq!(
        conversation.read_with(cx, |view, cx| view.draft(cx)),
        "saved prompt"
    );
    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    let new_conversation = active(window, cx);
    cx.simulate_keystrokes(window.into(), "up");
    assert_eq!(
        new_conversation.read_with(cx, |view, cx| view.draft(cx)),
        ""
    );
    assert_eq!(
        conversation.read_with(cx, |view, cx| view.draft(cx)),
        "saved prompt"
    );
}

#[gpui::test]
fn sends_runs_tools_saves_and_restores_tabs(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "echo from-tool"}),
    ));
    faux.push(FauxResponse::text("All **done**."));
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    send(window, "run it", cx);
    let conversation = active(window, cx);
    idle(&conversation, 3, cx);

    conversation.read_with(cx, |view, _| {
        let rows = view.transcript().rows();
        assert!(matches!(&rows[0], Row::User { text, .. } if text == "run it"));
        let call = view
            .transcript()
            .assistant(1)
            .unwrap()
            .tool_calls()
            .next()
            .unwrap()
            .clone();
        let tool = view.transcript().tool(&call.id).unwrap();
        assert_eq!(
            (tool.status, tool.output.as_str()),
            (ToolStatus::Done, "from-tool")
        );
        assert_eq!(view.title(), "run it");
    });
    let file = conversation
        .read_with(cx, |view, _| view.session_file())
        .unwrap();
    let state = saved_state(cx, &data);
    assert_eq!(
        state.project(&project).tabs[0].session.as_deref(),
        Some(file.as_path())
    );

    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let reopened = open(cx, &project);
    let restored = active(reopened, cx).read_with(cx, |view, _| view.transcript().len());
    assert_eq!(restored, 3);
}

#[gpui::test]
fn three_tabs_run_at_the_same_time(cx: &mut TestAppContext) {
    let providers: Vec<FauxProvider> = ["faux-a", "faux-b", "faux-c"]
        .into_iter()
        .map(|id| {
            let faux = FauxProvider::new(id).with_chunk_delay(Duration::from_millis(40));
            faux.push(FauxResponse::tool_call(
                "bash",
                json!({"command": format!("echo from-{id}")}),
            ));
            faux.push(FauxResponse::text(format!("{id} finished")));
            faux
        })
        .collect();
    let data = setup_models(cx, &providers.iter().collect::<Vec<_>>(), false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);

    for (index, provider) in providers.iter().enumerate() {
        if index > 0 {
            cx.dispatch_action(window.into(), NewChat);
            cx.run_until_parked();
        }
        let conversation = active(window, cx);
        let model = provider.model();
        window
            .update(cx, |_, window, cx| {
                conversation.update(cx, |view, cx| view.select_model(model, window, cx))
            })
            .unwrap();
        send(window, &format!("task {index}"), cx);
    }
    let conversations: Vec<_> = window
        .update(cx, |view, _, _| {
            view.tabs().iter().map(|t| t.conversation.clone()).collect()
        })
        .unwrap();
    assert_eq!(conversations.len(), 3);
    let running = conversations
        .iter()
        .filter(|c| c.read_with(cx, |view, _| view.is_running()))
        .count();
    assert_eq!(running, 3, "all three tabs work at once");

    for conversation in &conversations {
        idle(conversation, 3, cx);
    }
    for (conversation, provider) in conversations.iter().zip(&providers) {
        conversation.read_with(cx, |view, _| {
            let call = view
                .transcript()
                .assistant(1)
                .unwrap()
                .tool_calls()
                .next()
                .unwrap()
                .clone();
            let tool = view.transcript().tool(&call.id).unwrap();
            assert_eq!(tool.output, format!("from-{}", provider.model().provider));
            assert_eq!(
                view.transcript().assistant(2).unwrap().text(),
                format!("{} finished", provider.model().provider)
            );
        });
        assert_eq!(
            provider.requests().len(),
            2,
            "each tab used its own provider"
        );
    }
    // The background tabs finished while hidden; the active one did not.
    let unread: Vec<bool> = window
        .update(cx, |view, _, _| {
            view.tabs().iter().map(|t| t.unread).collect()
        })
        .unwrap();
    assert_eq!(unread, [true, true, false]);
    cx.dispatch_action(window.into(), ActivateTab(0));
    cx.run_until_parked();
    let unread: Vec<bool> = window
        .update(cx, |view, _, _| {
            view.tabs().iter().map(|t| t.unread).collect()
        })
        .unwrap();
    assert_eq!(unread, [false, true, false]);
    assert_eq!(saved_state(cx, &data).project(&project).tabs.len(), 3);
}

#[gpui::test]
fn tab_navigation_ordering_pinning_and_closing(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let sessions: Vec<PathBuf> = (0..3)
        .map(|i| write_session(&data, &project, &format!("s{i}")))
        .collect();
    let window = open(cx, &project);
    window
        .update(cx, |view, window, cx| {
            for path in &sessions {
                view.open_session_file(path, false, window, cx);
            }
        })
        .unwrap();
    // The initial empty chat plus three sessions.
    assert_eq!(tab_count(window, cx), 4);
    let titles = |cx: &mut TestAppContext| -> Vec<String> {
        window
            .update(cx, |view, _, cx| {
                view.tabs()
                    .iter()
                    .map(|t| t.conversation.read(cx).title())
                    .collect()
            })
            .unwrap()
    };
    let active_index =
        |cx: &mut TestAppContext| window.update(cx, |view, _, _| view.active_index()).unwrap();
    assert_eq!(titles(cx), ["New Chat", "s0", "s1", "s2"]);
    assert_eq!(active_index(cx), 3);

    cx.dispatch_action(window.into(), ActivateTab(0));
    cx.run_until_parked();
    assert_eq!(active_index(cx), 0);
    cx.dispatch_action(window.into(), PreviousTab);
    cx.run_until_parked();
    assert_eq!(active_index(cx), 3, "wraps around");
    cx.dispatch_action(window.into(), NextTab);
    cx.dispatch_action(window.into(), ActivateTab(usize::MAX));
    cx.run_until_parked();
    assert_eq!(active_index(cx), 3);

    // Drag s2 to the front; pin s1, which moves before the others.
    window
        .update(cx, |view, _, cx| view.move_tab(3, 0, cx))
        .unwrap();
    assert_eq!(titles(cx), ["s2", "New Chat", "s0", "s1"]);
    assert_eq!(active_index(cx), 0, "the active tab moved along");
    window
        .update(cx, |view, _, cx| view.toggle_pin(3, cx))
        .unwrap();
    assert_eq!(titles(cx), ["s1", "s2", "New Chat", "s0"]);

    // Tabs, pins and the active tab survive reopening the project.
    window
        .update(cx, |view, window, cx| view.activate_tab(3, window, cx))
        .unwrap();
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    let restored = window
        .update(cx, |view, _, cx| {
            view.tabs()
                .iter()
                .map(|t| (t.conversation.read(cx).title(), t.pinned))
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(
        restored,
        [
            ("s1".to_owned(), true),
            ("s2".to_owned(), false),
            ("New Chat".to_owned(), false),
            ("s0".to_owned(), false)
        ]
    );
    assert_eq!(
        window.update(cx, |view, _, _| view.active_index()).unwrap(),
        3
    );

    // "Close Others" keeps pinned tabs; closing the last real tab leaves a new chat, and
    // closing that empty chat closes the window.
    window
        .update(cx, |view, window, cx| view.close_other_tabs(1, window, cx))
        .unwrap();
    assert_eq!(titles_of(window, cx), ["s1", "s2"]);
    cx.dispatch_action(window.into(), CloseTab);
    cx.run_until_parked();
    assert_eq!(titles_of(window, cx), ["s1"]);
    window
        .update(cx, |view, _, cx| view.toggle_pin(0, cx))
        .unwrap();
    window
        .update(cx, |view, window, cx| view.close_tab(0, window, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(titles_of(window, cx), ["New Chat"]);
    cx.dispatch_action(window.into(), CloseTab);
    cx.run_until_parked();
    assert!(
        cx.update(|cx| workspace::Workspaces::project_window(&project, cx))
            .is_none()
    );
}

fn titles_of(window: WindowHandle<ProjectWindow>, cx: &mut TestAppContext) -> Vec<String> {
    window
        .update(cx, |view, _, cx| {
            view.tabs()
                .iter()
                .map(|t| t.conversation.read(cx).title())
                .collect()
        })
        .unwrap()
}

#[gpui::test]
fn closing_a_running_tab_asks_first(cx: &mut TestAppContext) {
    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(200));
    faux.push(FauxResponse::text("a slow answer that takes a while"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    send(window, "go", cx);
    let conversation = active(window, cx);
    assert!(conversation.read_with(cx, |view, _| view.is_running()));

    cx.dispatch_action(window.into(), CloseTab);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(tab_count(window, cx), 2);

    cx.dispatch_action(window.into(), CloseTab);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Stop and Close");
    cx.run_until_parked();
    assert_eq!(tab_count(window, cx), 1);
}

#[gpui::test]
fn sidebar_preview_open_search_and_menu(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let first = write_session(&data, &project, "parser rewrite");
    let second = write_session(&data, &project, "fix flaky test");
    let window = open(cx, &project);
    let listed = window
        .update(cx, |view, _, cx| view.visible_sessions(cx).len())
        .unwrap();
    assert_eq!(listed, 2);

    // A single click previews; the next preview replaces it; a double click keeps it.
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&first, true, window, cx)
        })
        .unwrap();
    assert_eq!(titles_of(window, cx), ["New Chat", "parser rewrite"]);
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&second, true, window, cx)
        })
        .unwrap();
    assert_eq!(titles_of(window, cx), ["New Chat", "fix flaky test"]);
    let preview = window
        .update(cx, |view, _, _| view.tabs()[1].preview)
        .unwrap();
    assert!(preview);
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&second, false, window, cx)
        })
        .unwrap();
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&first, true, window, cx)
        })
        .unwrap();
    assert_eq!(
        titles_of(window, cx),
        ["New Chat", "fix flaky test", "parser rewrite"]
    );
    // Opening an open session switches to it.
    window
        .update(cx, |view, window, cx| {
            view.open_session_file(&second, false, window, cx)
        })
        .unwrap();
    assert_eq!(
        window.update(cx, |view, _, _| view.active_index()).unwrap(),
        1
    );

    // Search matches titles and message text.
    let search = |query: &str, cx: &mut TestAppContext| -> Vec<String> {
        window
            .update(cx, |view, _, cx| {
                view.set_session_search(query, cx);
                view.visible_sessions(cx)
                    .iter()
                    .map(|s| s.title().to_owned())
                    .collect()
            })
            .unwrap()
    };
    assert_eq!(search("re: parser", cx), ["parser rewrite"]);
    assert_eq!(search("FLAKY", cx), ["fix flaky test"]);
    assert!(search("nothing", cx).is_empty());
    search("", cx);

    // Context menu: rename a session that is open in a tab.
    let info = window
        .update(cx, |view, _, cx| {
            view.visible_sessions(cx)
                .into_iter()
                .find(|s| s.path == first)
                .unwrap()
        })
        .unwrap();
    window
        .update(cx, |view, window, cx| {
            view.session_menu(&info, point(px(10.), px(10.)), window, cx)
        })
        .unwrap();
    let menu = window
        .update(cx, |view, _, _| view.context_menu().unwrap())
        .unwrap();
    let labels = menu.read_with(cx, |menu, _| menu.labels());
    assert_eq!(labels[..3], ["Open in New Tab", "Rename…", "Duplicate"]);
    assert_eq!(labels.last().map(String::as_str), Some("Delete"));
    let rename = menu.read_with(cx, |menu, _| menu.handler("Rename…"));
    let any_window: gpui::AnyWindowHandle = window.into();
    any_window
        .update(cx, |_, window, cx| rename(window, cx))
        .unwrap();
    let (target, input) = window
        .update(cx, |view, _, _| view.rename_input().unwrap())
        .unwrap();
    assert_eq!(target, RenameTarget::Session(first.clone()));
    input.update(cx, |input, cx| input.emplace("Parser v2", cx));
    cx.dispatch_action(window.into(), ConfirmRename);
    cx.run_until_parked();
    assert_eq!(Session::open(&first).unwrap().name(), Some("Parser v2"));
    assert!(titles_of(window, cx).contains(&"Parser v2".to_owned()));

    // Duplicate opens a fork; Delete closes its tab and removes the file.
    window
        .update(cx, |view, window, cx| {
            view.duplicate_session(&first, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(tab_count(window, cx), 4);
    let fork = active(window, cx)
        .read_with(cx, |view, _| view.session_file())
        .unwrap();
    assert_ne!(fork, first);
    assert_eq!(Session::open(&fork).unwrap().context().messages.len(), 2);
    window
        .update(cx, |view, window, cx| {
            view.delete_session(&second, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert!(!second.exists());
    assert_eq!(tab_count(window, cx), 3);
    let listed = window
        .update(cx, |view, _, cx| view.visible_sessions(cx).len())
        .unwrap();
    assert_eq!(listed, 2, "the deleted one is gone, the fork is listed");
}

#[gpui::test]
fn message_actions_and_session_tree(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    for answer in ["a1", "a2", "a2 edited"] {
        faux.push(FauxResponse::text(answer));
    }
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "u1", cx);
    idle(&conversation, 2, cx);
    send(window, "u2", cx);
    idle(&conversation, 4, cx);

    // Edit & Resend the second question in place: the conversation stays until the
    // edit is sent, which starts a sibling branch.
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.edit_and_resend(2, window, cx))
        })
        .unwrap();
    let rows = conversation.read_with(cx, |view, _| view.transcript().len());
    assert_eq!(rows, 4);
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| {
                let input = view.editing().unwrap().input.clone();
                input.update(cx, |input, cx| input.emplace("u2 edited", cx));
                view.submit_edit(window, cx)
            })
        })
        .unwrap();
    idle(&conversation, 4, cx);
    let texts = |cx: &mut TestAppContext| -> Vec<String> {
        conversation.read_with(cx, |view, _| {
            (0..view.transcript().len())
                .map(|row| match &view.transcript().rows()[row] {
                    Row::User { text, .. } => text.clone(),
                    _ => view.transcript().assistant(row).unwrap().text(),
                })
                .collect()
        })
    };
    assert_eq!(texts(cx), ["u1", "a1", "u2 edited", "a2 edited"]);

    // The tree shows the fork after a1; picking the old branch switches back.
    let rows = conversation.read_with(cx, |view, _| view.tree_rows());
    let shape: Vec<(usize, &str, bool)> = rows
        .iter()
        .map(|r| (r.depth, r.label.as_str(), r.active))
        .collect();
    assert_eq!(
        shape,
        [
            (0, "u1", true),
            (0, "a1", true),
            (1, "u2 edited", true),
            (1, "a2 edited", true),
            (1, "u2", false),
            (1, "a2", false),
        ]
    );
    let old = rows.iter().find(|r| r.label == "u2").unwrap().id.clone();
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.switch_branch(&old, window, cx))
        })
        .unwrap();
    assert_eq!(texts(cx), ["u1", "a1", "u2", "a2"]);

    // Branch from a1, then fork a1 into a new tab.
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.branch_from(1, window, cx))
        })
        .unwrap();
    assert_eq!(texts(cx), ["u1", "a1"]);
    conversation.update(cx, |view, cx| view.fork_from(1, cx));
    cx.run_until_parked();
    assert_eq!(tab_count(window, cx), 2);
    let fork = active(window, cx);
    assert_ne!(fork, conversation);
    assert_eq!(fork.read_with(cx, |view, _| view.transcript().len()), 2);
}

#[gpui::test]
fn names_new_sessions_with_a_model(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("Sure."));
    faux.push(FauxResponse::text("\"Refactor the CLI.\""));
    let _data = setup_models(cx, &[&faux], true);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "please refactor the command line interface", cx);
    idle(&conversation, 2, cx);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| view.title() == "Refactor the CLI")
    });
    // Only once per session (this third request finds no scripted answer and fails).
    send(window, "thanks", cx);
    idle(&conversation, 4, cx);
    assert_eq!(faux.requests().len(), 3);
}

#[gpui::test]
fn conversations_open_without_any_api_key(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let _data = setup(cx, None);
    cx.update(|cx| {
        gpui_tokio::init(cx);
        let models = Models::builtin().with_env(Arc::new(|_| None));
        AppModels::set(models, false, cx);
    });
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);

    // Without a usable provider the conversation opens with no model at all, and the
    // session records none.
    let (model, choices) = conversation.read_with(cx, |view, cx| {
        (view.status().model, view.model_choices_for_test(cx).len())
    });
    assert_eq!((model, choices), (None, 0));

    // Sending keeps the draft and asks for a model; no request is made.
    conversation.update(cx, |view, cx| view.set_draft("hello", cx));
    cx.dispatch_action(window.into(), SendMessage);
    cx.run_until_parked();
    let (needs_model, needs_key, draft, running) = conversation.read_with(cx, |view, cx| {
        (
            view.needs_model(),
            view.needs_key(cx),
            view.draft(cx),
            view.is_running(),
        )
    });
    assert!(needs_model);
    assert_eq!((needs_key, draft.as_str(), running), (None, "hello", false));

    // Once a key is stored, its models can be picked and the notice goes away.
    cx.update(|cx| {
        cx.global::<AppModels>()
            .credentials
            .set("anthropic", "sk-test")
            .unwrap()
    });
    let choice = conversation.read_with(cx, |view, cx| {
        view.model_choices_for_test(cx)
            .into_iter()
            .next()
            .expect("anthropic models")
            .model
    });
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.select_model(choice.clone(), window, cx))
        })
        .unwrap();
    let (model, needs_model) =
        conversation.read_with(cx, |view, _| (view.status().model, view.needs_model()));
    assert_eq!(model.map(|m| m.qualified_id()), Some(choice.qualified_id()));
    assert!(!needs_model);
}

#[gpui::test]
fn new_conversations_remember_the_last_model(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("back to the original model"));
    let other = FauxProvider::new("other");
    let data = setup_models(cx, &[&faux, &other], false);
    let fallback = faux.model().qualified_id();
    let recent = other.model().qualified_id();
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let original = active(window, cx);

    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    let conversation = active(window, cx);
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.select_model(other.model(), window, cx))
        })
        .unwrap();
    assert_eq!(
        saved_state(cx, &data).last_model.as_deref(),
        Some(recent.as_str())
    );
    cx.update(|cx| AppStore::init(Some(zp_core::paths::state_file(data.path())), cx));

    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    assert_eq!(
        active(window, cx).read_with(cx, |view, _| view.model().unwrap().qualified_id()),
        recent
    );
    assert_eq!(
        original.read_with(cx, |view, _| view.model().unwrap().qualified_id()),
        fallback
    );

    // Merely revisiting an old conversation keeps the preference; sending uses its model.
    cx.dispatch_action(window.into(), ActivateTab(0));
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| AppStore::state(cx).last_model.clone()),
        Some(recent.clone())
    );
    send(window, "continue with the original model", cx);
    idle(&original, 2, cx);
    assert_eq!(
        saved_state(cx, &data).last_model.as_deref(),
        Some(fallback.as_str())
    );

    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    let conversation = active(window, cx);
    assert_eq!(
        conversation.read_with(cx, |view, _| view.model().unwrap().qualified_id()),
        fallback
    );
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| view.select_model(other.model(), window, cx))
        })
        .unwrap();
    cx.update(|cx| cx.global::<AppModels>().models.remove_provider("other"));
    cx.dispatch_action(window.into(), NewChat);
    cx.run_until_parked();
    assert_eq!(
        active(window, cx).read_with(cx, |view, _| view.model().unwrap().qualified_id()),
        fallback
    );
}

#[gpui::test]
fn measures_the_output_speed(cx: &mut TestAppContext) {
    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(200));
    faux.push(FauxResponse::text(
        "a fairly long answer streamed in several chunks ".repeat(6),
    ));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    assert!(conversation.read_with(cx, |v, _| v.status().tokens_per_second.is_none()));

    send(window, "hello", cx);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| {
            let status = view.status();
            view.is_running() && status.usage.output > 5 && status.tokens_per_second.is_some()
        })
    });
    let live = conversation.read_with(cx, |view, _| view.status());
    assert!(live.output_estimated);
    assert!(live.tokens_per_second.is_some_and(|speed| speed > 0.));
    idle(&conversation, 2, cx);
    let final_status = conversation.read_with(cx, |view, _| view.status());
    assert!(!final_status.output_estimated);
    assert_eq!(
        final_status.usage.output, 5,
        "final provider count replaces the estimate"
    );
    let speed = final_status.tokens_per_second;
    assert!(speed.is_some_and(|speed| speed > 0.0), "{speed:?}");
}

#[gpui::test]
fn ime_composition_has_bounds_before_the_next_frame(cx: &mut TestAppContext) {
    use gpui::{Bounds, EntityInputHandler as _};

    let faux = FauxProvider::default();
    setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    conversation.update(cx, |view, cx| view.set_draft("hello", cx));
    cx.run_until_parked();
    let composer = conversation.read_with(cx, |view, _| view.composer_input());
    let marked_bounds = |cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                composer.update(cx, |input, cx| {
                    let marked = input.marked_text_range(window, cx)?;
                    input.bounds_for_range(marked, Bounds::default(), window, cx)
                })
            })
            .unwrap()
    };

    // The IME inserts marked text and asks where it is before anything is laid out
    // again; without an answer macOS shows its window at the screen's corner.
    let stale = window
        .update(cx, |_, window, cx| {
            composer.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                let marked = input.marked_text_range(window, cx).unwrap();
                input.bounds_for_range(marked, Bounds::default(), window, cx)
            })
        })
        .unwrap()
        .expect("bounds right after the edit");
    cx.run_until_parked();
    let fresh = marked_bounds(cx).expect("bounds after layout");
    assert!(stale.origin.x > px(0.), "after `hello`: {stale:?}");
    assert!(
        (stale.origin.x - fresh.origin.x).abs() < px(1.),
        "{stale:?} vs {fresh:?}"
    );
    assert!(
        (stale.origin.y - fresh.origin.y).abs() < px(1.),
        "{stale:?} vs {fresh:?}"
    );
}

#[gpui::test]
fn scrolling_up_while_a_reply_streams_stays_put(cx: &mut TestAppContext) {
    use gpui::{Modifiers, ScrollDelta, ScrollWheelEvent, TouchPhase, VisualTestContext};

    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(300));
    let long: String = (1..=300)
        .map(|n| format!("Line {n} of a long answer.\n\n"))
        .collect();
    faux.push(FauxResponse::text(long));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    send(window, "write a lot", cx);
    let conversation = active(window, cx);
    let streamed = |cx: &mut TestAppContext| {
        conversation.read_with(cx, |view, _| {
            let transcript = view.transcript();
            transcript
                .len()
                .checked_sub(1)
                .and_then(|row| transcript.assistant(row))
                .map_or(0, |message| message.text().len())
        })
    };
    // The first part of the reply is on screen and the view follows it.
    wait_until(cx, |cx| streamed(cx) > 0);
    let first_part = streamed(cx);
    assert!(conversation.read_with(cx, |view, _| view.transcript_scroll().1));

    // Scroll up into the reply.
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_event(ScrollWheelEvent {
        position: point(px(600.), px(300.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(400.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    visual.run_until_parked();
    let (scrolled, following) = conversation.read_with(&visual, |view, _| view.transcript_scroll());
    assert!(!following, "scrolling up stops following the output");

    // More of the reply arrives: the view stays where the user put it.
    wait_until(cx, |cx| streamed(cx) > first_part);
    let (now, following) = conversation.read_with(cx, |view, _| view.transcript_scroll());
    assert!(!following);
    assert_eq!(
        (now.item_ix, now.offset_in_item),
        (scrolled.item_ix, scrolled.offset_in_item),
        "streamed output moved the scroll position"
    );
}
