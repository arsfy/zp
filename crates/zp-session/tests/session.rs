//! Session storage scenarios on real files.

use std::path::Path;

use zp_agent::{AgentMessage, CustomMessage};
use zp_ai::{AssistantMessage, Message, TextContent, ThinkingLevel};
use zp_session::{
    COMPACTION_SUMMARY_TYPE, EntryData, ModelRef, Session, SessionManager, encode_cwd,
};

fn assistant(text: &str, model: &str) -> AgentMessage {
    let mut message = AssistantMessage::empty("anthropic-messages", "anthropic", model);
    message
        .content
        .push(zp_ai::AssistantContent::Text(TextContent {
            text: text.into(),
        }));
    message.into()
}

fn texts(messages: &[AgentMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            AgentMessage::Llm(Message::User(user)) => user
                .content
                .iter()
                .filter_map(zp_ai::UserContent::as_text)
                .collect(),
            AgentMessage::Llm(Message::Assistant(a)) => a.text(),
            AgentMessage::Llm(Message::ToolResult(_)) => "<tool>".into(),
            AgentMessage::Custom(custom) => format!(
                "[{}] {}",
                custom.custom_type,
                custom
                    .content
                    .iter()
                    .filter_map(zp_ai::UserContent::as_text)
                    .collect::<String>()
            ),
        })
        .collect()
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

#[test]
fn writes_lazily_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(root.path());
    let cwd = Path::new("/work/app");
    let mut session = manager.create(cwd);
    let file = session.file().unwrap().to_path_buf();
    assert!(file.starts_with(root.path().join("--work-app--")));
    assert!(
        file.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!("_{}.jsonl", session.id()))
    );

    session
        .append_model_change("anthropic", "claude-a")
        .unwrap();
    session
        .append_thinking_level_change(ThinkingLevel::High)
        .unwrap();
    assert!(!file.exists(), "no file before the conversation starts");
    assert!(manager.list(cwd).unwrap().is_empty());

    session.append_message(AgentMessage::user("hello")).unwrap();
    assert!(session.is_persisted());
    assert_eq!(line_count(&file), 4);
    session
        .append_message(assistant("hi!", "claude-a"))
        .unwrap();
    assert_eq!(line_count(&file), 5);

    let reopened = Session::open(&file).unwrap();
    assert_eq!(reopened.id(), session.id());
    assert_eq!(reopened.entries(), session.entries());
    assert_eq!(reopened.leaf_id(), session.leaf_id());
    let context = reopened.context();
    assert_eq!(texts(&context.messages), ["hello", "hi!"]);
    assert_eq!(context.thinking_level, Some(ThinkingLevel::High));
    assert_eq!(
        context.model,
        Some(ModelRef {
            provider: "anthropic".into(),
            model_id: "claude-a".into()
        })
    );

    let in_memory = {
        let mut session = Session::in_memory(cwd);
        session.append_message(AgentMessage::user("x")).unwrap();
        session
    };
    assert!(in_memory.file().is_none() && !in_memory.is_persisted());
}

#[test]
fn branches_and_tree() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(root.path());
    let mut session = manager.create(Path::new("/p"));
    let u1 = session.append_message(AgentMessage::user("u1")).unwrap();
    let a1 = session.append_message(assistant("a1", "m")).unwrap();
    session.append_message(AgentMessage::user("u2")).unwrap();
    let a2 = session.append_message(assistant("a2", "m")).unwrap();

    session.branch(&a1).unwrap();
    session.append_message(AgentMessage::user("u3")).unwrap();
    session.append_message(assistant("a3", "m")).unwrap();

    assert_eq!(texts(&session.context().messages), ["u1", "a1", "u3", "a3"]);
    assert_eq!(
        texts(&session.context_at(Some(&a2)).messages),
        ["u1", "a1", "u2", "a2"]
    );
    assert_eq!(session.children(&a1).len(), 2);

    let tree = session.tree();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].entry.id, u1);
    assert_eq!(tree[0].children[0].children.len(), 2, "a1 has two branches");

    // The file is the same tree; the leaf after reopening is the last appended entry.
    let reopened = Session::open(session.file().unwrap()).unwrap();
    assert_eq!(
        texts(&reopened.context().messages),
        ["u1", "a1", "u3", "a3"]
    );

    // Editing the first message: a new root.
    session.reset_leaf();
    session
        .append_message(AgentMessage::user("u1 edited"))
        .unwrap();
    assert_eq!(texts(&session.context().messages), ["u1 edited"]);
    assert_eq!(session.tree().len(), 2);
    assert!(session.branch("missing").is_err());
}

#[test]
fn compaction_replaces_older_history_with_a_summary() {
    let mut session = Session::in_memory(Path::new("/p"));
    session.append_message(AgentMessage::user("u1")).unwrap();
    session.append_message(assistant("a1", "m")).unwrap();
    let u2 = session.append_message(AgentMessage::user("u2")).unwrap();
    session.append_message(assistant("a2", "m")).unwrap();
    session
        .append_compaction("did u1/a1".into(), &u2, 1234, None)
        .unwrap();
    session.append_message(AgentMessage::user("u3")).unwrap();

    let messages = session.context().messages;
    assert_eq!(
        texts(&messages),
        [
            format!("[{COMPACTION_SUMMARY_TYPE}] did u1/a1"),
            "u2".into(),
            "a2".into(),
            "u3".into()
        ]
    );
    let AgentMessage::Custom(summary) = &messages[0] else {
        panic!()
    };
    assert_eq!(summary.details.as_ref().unwrap()["tokensBefore"], 1234);
    assert!(
        session
            .append_compaction("x".into(), "nope", 0, None)
            .is_err()
    );
}

#[test]
fn labels_names_and_custom_messages() {
    let mut session = Session::in_memory(Path::new("/p"));
    let u1 = session.append_message(AgentMessage::user("u1")).unwrap();
    session
        .append_message(CustomMessage::text("note", "remember").into())
        .unwrap();
    session.append_label(&u1, Some("start".into())).unwrap();
    assert_eq!(session.label(&u1), Some("start"));
    session.append_label(&u1, None).unwrap();
    assert_eq!(session.label(&u1), None);
    assert!(session.name().is_none());
    session
        .append_session_info(Some("Fix tests".into()))
        .unwrap();
    assert_eq!(session.name(), Some("Fix tests"));
    // Labels and metadata are not part of the transcript.
    assert_eq!(
        texts(&session.context().messages),
        ["u1", "[note] remember"]
    );
}

#[test]
fn tolerates_damaged_files() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(root.path());
    let mut session = manager.create(Path::new("/p"));
    session.append_message(AgentMessage::user("u1")).unwrap();
    let file = session.file().unwrap().to_path_buf();

    // A torn final write and a garbage line.
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("not json\n{\"id\":\"zz\",\"parentId\":null,\"timest");
    std::fs::write(&file, text).unwrap();

    let mut reopened = Session::open(&file).unwrap();
    assert_eq!(reopened.entries().len(), 1);
    reopened.append_message(assistant("a1", "m")).unwrap();
    let again = Session::open(&file).unwrap();
    assert_eq!(texts(&again.context().messages), ["u1", "a1"]);

    std::fs::write(root.path().join("other.jsonl"), "{}\n").unwrap();
    assert!(Session::open(&root.path().join("other.jsonl")).is_err());
}

#[test]
fn manager_lists_finds_and_forks() {
    let root = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(root.path());
    let cwd = Path::new("/work/app");

    let mut first = manager.create(cwd);
    first
        .append_message(AgentMessage::user("first task"))
        .unwrap();
    let a1 = first.append_message(assistant("done", "m")).unwrap();
    first.append_message(AgentMessage::user("more")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut second = manager.create(cwd);
    second
        .append_message(AgentMessage::user("second task"))
        .unwrap();
    second.append_session_info(Some("Named".into())).unwrap();
    let mut other = manager.create(Path::new("/elsewhere"));
    other
        .append_message(AgentMessage::user("unrelated"))
        .unwrap();

    let listed = manager.list(cwd).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second.id(), "most recent first");
    assert_eq!(listed[0].title(), "Named");
    assert_eq!(listed[1].title(), "first task");
    assert_eq!(listed[1].message_count, 3);
    assert_eq!(manager.list_all().unwrap().len(), 3);

    let found = manager.find(cwd, &first.id()[..13]).unwrap().unwrap();
    assert_eq!(found.path, first.file().unwrap());
    assert!(manager.find(cwd, "ffffffff-nope").unwrap().is_none());
    assert_eq!(manager.continue_recent(cwd).unwrap().id(), second.id());

    let fork = manager.fork(&first, &a1).unwrap();
    assert!(fork.is_persisted());
    assert_ne!(fork.id(), first.id());
    assert_eq!(texts(&fork.context().messages), ["first task", "done"]);
    assert_eq!(
        fork.header().parent_session.as_deref(),
        first
            .file()
            .map(|p| p.to_string_lossy().into_owned())
            .as_deref()
    );
    assert_eq!(manager.list(cwd).unwrap().len(), 3);
    let reopened = Session::open(fork.file().unwrap()).unwrap();
    assert!(matches!(
        reopened.entries()[1].data,
        EntryData::Message { .. }
    ));

    assert_eq!(encode_cwd(Path::new("/work/my-app")), "--work-my-app--");
    assert_eq!(encode_cwd(Path::new("C:\\Users\\me")), "--C--Users-me--");
}
