//! Tool behavior against real temp directories.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use zp_agent::{AgentTool, AgentToolResult, ToolContext, validate_arguments};
use zp_ai::UserContent;
use zp_tools::{FileChange, coding_tools, create_tool};

fn tool(name: &str, cwd: &Path) -> Arc<dyn AgentTool> {
    create_tool(name, cwd).unwrap()
}

/// Validates like the agent loop does, then executes.
async fn run(tool: &dyn AgentTool, args: Value) -> anyhow::Result<AgentToolResult> {
    run_with(
        tool,
        args,
        ToolContext::detached("call", CancellationToken::new()),
    )
    .await
}

async fn run_with(
    tool: &dyn AgentTool,
    args: Value,
    ctx: ToolContext,
) -> anyhow::Result<AgentToolResult> {
    let args = validate_arguments(&tool.parameters(), tool.prepare_arguments(args))
        .map_err(|e| anyhow::anyhow!(e))?;
    tool.execute(args, ctx).await
}

fn text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(UserContent::as_text)
        .collect()
}

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn every_tool_has_a_valid_declaration() {
    let dir = tempfile::tempdir().unwrap();
    struct Nobody;
    impl zp_tools::Asker for Nobody {
        fn ask(
            &self,
            _: &str,
            _: zp_tools::AskRequest,
        ) -> futures::future::BoxFuture<'static, Option<Vec<zp_tools::Answer>>> {
            Box::pin(async { None })
        }
    }
    let mut tools = coding_tools(dir.path());
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "read", "bash", "edit", "write", "grep", "find", "ls", "todo"
        ]
    );
    tools.push(std::sync::Arc::new(zp_tools::AskTool::new(
        std::sync::Arc::new(Nobody),
    )));
    for tool in &tools {
        let schema = tool.parameters();
        assert_eq!(schema["type"], "object", "{}", tool.name());
        assert!(tool.prompt_snippet().is_some(), "{}", tool.name());
        assert!(!tool.description().is_empty());
    }
}

// ----- read ------------------------------------------------------------------------------

#[tokio::test]
async fn read_text_paging_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let lines: Vec<String> = (1..=2500).map(|i| format!("line {i}")).collect();
    write(dir.path(), "big.txt", &lines.join("\n"));
    write(dir.path(), "small.txt", "a\nb\nc\n");
    write(dir.path(), "empty.txt", "");
    let read = tool("read", dir.path());

    let result = run(&*read, json!({"path": "small.txt"})).await.unwrap();
    assert_eq!(text(&result), "a\nb\nc\n");

    let result = run(
        &*read,
        json!({"path": "small.txt", "offset": 2, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "b\n\n[2 more lines in file. Use offset=3 to continue.]"
    );

    let result = run(&*read, json!({"path": "big.txt"})).await.unwrap();
    let output = text(&result);
    assert!(output.starts_with("line 1\n"));
    assert!(output.ends_with("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
    assert!(
        result.details.unwrap()["truncation"]["truncated"]
            .as_bool()
            .unwrap()
    );

    let result = run(&*read, json!({"path": "big.txt", "offset": "2400"}))
        .await
        .unwrap();
    assert!(text(&result).starts_with("line 2400\n"));

    assert_eq!(
        text(&run(&*read, json!({"path": "empty.txt"})).await.unwrap()),
        "(empty file)"
    );
    let error = run(&*read, json!({"path": "small.txt", "offset": 99}))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Offset 99 is beyond end of file (4 lines total)"
    );
    let error = run(&*read, json!({"path": "missing.txt"}))
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("Could not read missing.txt"));
}

#[tokio::test]
async fn read_images_and_binary_files() {
    let dir = tempfile::tempdir().unwrap();
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    std::fs::write(dir.path().join("pic.png"), png).unwrap();
    std::fs::write(dir.path().join("blob.bin"), b"abc\0def").unwrap();
    let read = tool("read", dir.path());

    let result = run(&*read, json!({"path": "pic.png"})).await.unwrap();
    assert_eq!(result.content.len(), 2);
    let UserContent::Image(image) = &result.content[1] else {
        panic!("expected an image")
    };
    assert_eq!(image.mime_type, "image/png");

    let error = run(&*read, json!({"path": "blob.bin"})).await.unwrap_err();
    assert!(error.to_string().contains("binary file"));
}

// ----- write / edit ----------------------------------------------------------------------

#[tokio::test]
async fn write_creates_and_overwrites_with_file_changes() {
    let dir = tempfile::tempdir().unwrap();
    let write_tool = tool("write", dir.path());

    let result = run(
        &*write_tool,
        json!({"path": "nested/dir/a.txt", "content": "one"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "Created nested/dir/a.txt (3 bytes)");
    let change = FileChange::from_details(result.details.as_ref().unwrap()).unwrap();
    assert!(change.is_creation());
    assert!(change.path.ends_with("nested/dir/a.txt"));

    let result = run(
        &*write_tool,
        json!({"path": "nested/dir/a.txt", "content": "two"}),
    )
    .await
    .unwrap();
    let change = FileChange::from_details(result.details.as_ref().unwrap()).unwrap();
    assert_eq!(
        (change.before.as_deref(), change.after.as_str()),
        (Some("one"), "two")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested/dir/a.txt")).unwrap(),
        "two"
    );
}

#[tokio::test]
async fn edit_accepts_common_argument_shapes() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/lib.rs", "fn a() {}\r\nfn b() {}\r\n");
    let edit = tool("edit", dir.path());

    let result = run(
        &*edit,
        json!({"path": "src/lib.rs", "edits": [{"oldText": "fn a() {}", "newText": "fn a() {\n    1\n}"}]}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "Successfully replaced 1 block(s) in src/lib.rs."
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        "fn a() {\r\n    1\r\n}\r\nfn b() {}\r\n"
    );
    let change = FileChange::from_details(result.details.as_ref().unwrap()).unwrap();
    assert_eq!(change.before.as_deref(), Some("fn a() {}\r\nfn b() {}\r\n"));

    // Legacy top-level fields, and edits as a JSON string.
    run(
        &*edit,
        json!({"path": "src/lib.rs", "oldText": "fn b", "newText": "fn c"}),
    )
    .await
    .unwrap();
    run(
        &*edit,
        json!({"path": "src/lib.rs", "edits": "[{\"oldText\": \"fn c\", \"newText\": \"fn d\"}]"}),
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(dir.path().join("src/lib.rs"))
            .unwrap()
            .contains("fn d() {}")
    );

    let error = run(
        &*edit,
        json!({"path": "src/lib.rs", "edits": [{"oldText": "zzz", "newText": ""}]}),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("Could not find the exact text in src/lib.rs.")
    );
    let error = run(
        &*edit,
        json!({"path": "nope.rs", "edits": [{"oldText": "a", "newText": "b"}]}),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("Could not edit file: nope.rs")
    );
}

#[tokio::test]
async fn parallel_edits_to_one_file_do_not_lose_updates() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (0..20).map(|i| format!("v{i}=0\n")).collect();
    write(dir.path(), "f.txt", &content);
    let edit = tool("edit", dir.path());
    let calls = (0..20).map(|i| {
        let edit = edit.clone();
        async move {
            run(&*edit, json!({"path": "f.txt", "edits": [{"oldText": format!("v{i}=0"), "newText": format!("v{i}=1")}]}))
                .await
                .unwrap()
        }
    });
    futures::future::join_all(calls).await;
    let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(after.matches("=1").count(), 20, "{after}");
}

// ----- bash ------------------------------------------------------------------------------

#[tokio::test]
async fn bash_output_exit_codes_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "marker.txt", "here");
    let bash = tool("bash", dir.path());

    let result = run(
        &*bash,
        json!({"command": "echo out; echo err 1>&2; cat marker.txt"}),
    )
    .await
    .unwrap();
    assert!(!result.is_error);
    let output = text(&result);
    for part in ["out", "err", "here"] {
        assert!(output.contains(part), "{output}");
    }
    assert_eq!(result.details.unwrap()["exitCode"], 0);

    let result = run(&*bash, json!({"command": "echo failing; exit 3"}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(text(&result), "failing\n\nCommand exited with code 3");

    let result = run(&*bash, json!({"command": "true"})).await.unwrap();
    assert_eq!(text(&result), "(no output)");
}

#[tokio::test]
async fn bash_timeout_cancel_and_updates() {
    let dir = tempfile::tempdir().unwrap();
    let bash = tool("bash", dir.path());

    let started = Instant::now();
    let error = run(
        &*bash,
        json!({"command": "echo before; sleep 30", "timeout": 2}),
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(
        error.to_string(),
        "before\n\nCommand timed out after 2 seconds"
    );

    let cancel = CancellationToken::new();
    let ctx = ToolContext::detached("call", cancel.clone());
    let canceller = cancel.clone();
    // Cancel once the script says all ticks are out (a fixed delay races slow machines).
    let marker = dir.path().join("ticked");
    tokio::spawn(async move {
        let waiting = Instant::now();
        while !marker.exists() && waiting.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        canceller.cancel();
    });
    let started = Instant::now();
    let error = run_with(
        &*bash,
        json!({"command": "for i in 1 2 3; do echo tick $i; sleep 0.1; done; touch ticked; sleep 30"}),
        ctx,
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(20));
    let message = error.to_string();
    assert!(
        message.contains("tick 3") && message.ends_with("Command aborted"),
        "{message}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_does_not_wait_for_background_processes() {
    let dir = tempfile::tempdir().unwrap();
    let bash = tool("bash", dir.path());
    let started = Instant::now();
    let result = run(&*bash, json!({"command": "sleep 5 & echo started"}))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(text(&result), "started");
}

#[tokio::test]
async fn bash_truncates_long_output_and_keeps_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let bash = tool("bash", dir.path());
    let result = run(
        &*bash,
        json!({"command": "for i in $(seq 1 3000); do echo line $i; done"}),
    )
    .await
    .unwrap();
    let output = text(&result);
    assert!(output.contains("line 3000"));
    assert!(!output.contains("line 1000\n"));
    assert!(
        output.contains("[Showing lines 1001-3000 of 3000. Full output: "),
        "{}",
        &output[output.len() - 200..]
    );
    let path = result.details.unwrap()["fullOutputPath"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("line 1\n")
    );
    std::fs::remove_file(path).unwrap();
}

// ----- grep / find / ls ------------------------------------------------------------------

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "target/\n*.log\n");
    write(
        dir.path(),
        "src/main.rs",
        "fn main() {\n    let todo = 1;\n    println!(\"TODO\");\n}\n",
    );
    write(
        dir.path(),
        "src/util/mod.rs",
        "// todo: tidy\npub fn helper() {}\n",
    );
    write(dir.path(), "README.md", "# Demo\nTODO list\n");
    write(dir.path(), ".hidden/config.toml", "todo = true\n");
    write(dir.path(), "target/debug/out.rs", "todo in build output\n");
    write(dir.path(), "debug.log", "todo in log\n");
    dir
}

#[tokio::test]
async fn grep_respects_gitignore_globs_and_options() {
    let dir = project();
    let grep = tool("grep", dir.path());

    let result = run(&*grep, json!({"pattern": "todo"})).await.unwrap();
    assert_eq!(
        text(&result),
        ".hidden/config.toml:1: todo = true\nsrc/main.rs:2:     let todo = 1;\nsrc/util/mod.rs:1: // todo: tidy"
    );

    let result = run(
        &*grep,
        json!({"pattern": "todo", "ignoreCase": true, "glob": "*.rs"}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "src/main.rs:2:     let todo = 1;\nsrc/main.rs:3:     println!(\"TODO\");\nsrc/util/mod.rs:1: // todo: tidy"
    );

    let result = run(
        &*grep,
        json!({"pattern": "let todo", "context": 1, "path": "src/main.rs"}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        "main.rs-1- fn main() {\nmain.rs:2:     let todo = 1;\nmain.rs-3-     println!(\"TODO\");"
    );

    let result = run(&*grep, json!({"pattern": "(", "literal": true}))
        .await
        .unwrap();
    assert!(text(&result).contains("src/main.rs:1: fn main() {"));

    let result = run(
        &*grep,
        json!({"pattern": "todo", "ignoreCase": true, "limit": 2}),
    )
    .await
    .unwrap();
    assert!(
        text(&result)
            .ends_with("[2 matches limit reached. Use limit=4 for more, or refine pattern]")
    );

    assert_eq!(
        text(
            &run(&*grep, json!({"pattern": "nothing-here"}))
                .await
                .unwrap()
        ),
        "No matches found"
    );
    assert!(
        run(&*grep, json!({"pattern": "("}))
            .await
            .unwrap_err()
            .to_string()
            .starts_with("Invalid regex")
    );
}

#[tokio::test]
async fn find_matches_names_and_paths() {
    let dir = project();
    let find = tool("find", dir.path());

    let result = run(&*find, json!({"pattern": "*.rs"})).await.unwrap();
    assert_eq!(text(&result), "src/main.rs\nsrc/util/mod.rs");

    let result = run(&*find, json!({"pattern": "src/**/*.rs"}))
        .await
        .unwrap();
    assert_eq!(text(&result), "src/main.rs\nsrc/util/mod.rs");

    let result = run(&*find, json!({"pattern": "util"})).await.unwrap();
    assert_eq!(text(&result), "src/util/");

    let result = run(&*find, json!({"pattern": "readme*"})).await.unwrap();
    assert_eq!(text(&result), "README.md", "smart case");

    let result = run(&*find, json!({"pattern": "*", "limit": 2}))
        .await
        .unwrap();
    assert!(text(&result).contains("[2 results limit reached"));

    assert_eq!(
        text(&run(&*find, json!({"pattern": "*.py"})).await.unwrap()),
        "No files found matching pattern"
    );
}

#[tokio::test]
async fn ls_lists_sorted_with_directory_markers() {
    let dir = project();
    let ls = tool("ls", dir.path());
    let result = run(&*ls, json!({})).await.unwrap();
    assert_eq!(
        text(&result),
        ".gitignore\n.hidden/\ndebug.log\nREADME.md\nsrc/\ntarget/"
    );
    let result = run(&*ls, json!({"path": "src", "limit": 1})).await.unwrap();
    assert_eq!(
        text(&result),
        "main.rs\n\n[1 entries limit reached. Use limit=2 for more]"
    );
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    assert_eq!(
        text(&run(&*ls, json!({"path": "empty"})).await.unwrap()),
        "(empty directory)"
    );
    assert!(
        run(&*ls, json!({"path": "README.md"}))
            .await
            .unwrap_err()
            .to_string()
            .starts_with("Not a directory")
    );
}
