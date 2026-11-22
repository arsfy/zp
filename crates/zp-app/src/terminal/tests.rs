use alacritty_terminal::event::VoidListener;
use alacritty_terminal::vte::ansi::Processor;
use std::time::{Duration, Instant};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};

use super::*;
use crate::tests::project_dir;
use crate::tests_conversation::{open, setup_models};

const LINK: &str = "https://example.com/zp-terminal";

/// Lets the shell run until its screen satisfies `done`. Waits in real time, and long:
/// shells start slowly on CI (PowerShell especially).
fn wait_for_screen(
    cx: &mut TestAppContext,
    terminal: &Entity<TerminalView>,
    done: impl Fn(&str) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        cx.run_until_parked();
        let text = terminal.read_with(cx, |view, _| view.screen_text());
        if done(&text) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out, screen:\n{text}");
        cx.executor().advance_clock(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The screen row showing `LINK` on its own.
fn link_row(view: &TerminalView) -> usize {
    view.screen_text()
        .lines()
        .position(|line| line == LINK)
        .unwrap()
}

fn grid(text: &str, columns: usize, lines: usize) -> Term<VoidListener> {
    let mut term = Term::new(
        Config::default(),
        &GridSize { columns, lines },
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, text.as_bytes());
    term
}

#[test]
fn urls_handle_wraps_wide_characters_and_punctuation() {
    let term = grid(
        "\u{4f60}\u{597d} (https://example.com/a_(b)?x=1&y=2).",
        16,
        6,
    );
    let mut detector = links::Detector::default();
    let link = detector
        .at(&term, GridPoint::new(Line(1), Column(0)))
        .unwrap();
    assert_eq!(link.url, "https://example.com/a_(b)?x=1&y=2");
    assert_eq!(link.range.start(), &GridPoint::new(Line(0), Column(6)));
    assert!(
        detector
            .at(&term, GridPoint::new(Line(0), Column(0)))
            .is_none()
    );
    assert!(
        detector
            .at(
                &term,
                link.range
                    .end()
                    .add(&term, alacritty_terminal::index::Boundary::Grid, 1)
            )
            .is_none()
    );

    let term = grid(
        "http://[::1]:3000/ https://localhost:8080/ www.example.com.",
        80,
        4,
    );
    assert_eq!(
        detector
            .at(&term, GridPoint::new(Line(0), Column(8)))
            .unwrap()
            .url,
        "http://[::1]:3000/"
    );
    assert_eq!(
        detector
            .at(&term, GridPoint::new(Line(0), Column(45)))
            .unwrap()
            .url,
        "https://www.example.com"
    );
}

#[test]
fn osc8_links_use_the_target_and_reject_command_schemes() {
    let term = grid(
        "\x1b]8;;https://example.com/docs\x1b\\Read docs\x1b]8;;\x1b\\ \x1b]8;;javascript:alert(1)\x1b\\bad\x1b]8;;\x1b\\",
        40,
        4,
    );
    let mut detector = links::Detector::default();
    let link = detector
        .at(&term, GridPoint::new(Line(0), Column(5)))
        .unwrap();
    assert_eq!(link.url, "https://example.com/docs");
    assert_eq!(
        link.range,
        GridPoint::new(Line(0), Column(0))..=GridPoint::new(Line(0), Column(8))
    );
    assert!(
        detector
            .at(&term, GridPoint::new(Line(0), Column(11)))
            .is_none()
    );
}

#[test]
fn osc52_copies_only_the_clipboard_target_within_the_size_limit() {
    assert!(accepts_clipboard_store(ClipboardType::Clipboard, "copied"));
    assert!(!accepts_clipboard_store(ClipboardType::Selection, "copied"));
    let limit = "x".repeat(OSC52_MAX_BYTES);
    assert!(accepts_clipboard_store(ClipboardType::Clipboard, &limit));
    assert!(!accepts_clipboard_store(
        ClipboardType::Clipboard,
        &format!("{limit}x")
    ));
}

#[test]
fn search_is_literal_unicode_aware_and_covers_history() {
    let term = grid(
        "first Match\r\nsecond match\r\nthird MATCH\r\n\u{4f60}\u{597d} cargo.test\r\nlast\r\n",
        20,
        3,
    );
    assert!(term.history_size() > 0);
    let mut search = search::Search::default();
    search.refresh(&term, "match", true);
    assert_eq!(search.matches.len(), 3);
    assert!(search.matches[0].start().line.0 < 0);
    let original = search.current;
    search.advance(true);
    search.advance(false);
    assert_eq!(search.current, original);
    search.refresh(&term, "cargo.test", true);
    assert_eq!(search.matches.len(), 1);
    assert_eq!(search.active().unwrap().start().column, Column(5));
    search.refresh(&term, "cargo.*", true);
    assert!(search.matches.is_empty());
    search.refresh(&term, "", true);
    assert!(search.matches.is_empty());

    let wrapped = grid("prefix cargo.test suffix", 10, 4);
    search.refresh(&wrapped, "cargo.test", true);
    assert_eq!(search.matches.len(), 1);
    assert_ne!(search.matches[0].start().line, search.matches[0].end().line);
}

#[gpui::test]
fn terminal_links_search_selection_and_context_menu(cx: &mut TestAppContext) {
    let faux = zp_ai::FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    cx.dispatch_action(window.into(), crate::actions::ToggleTerminal);
    cx.run_until_parked();
    let terminal = window
        .update(cx, |project, _, _| project.terminal().unwrap())
        .unwrap();
    // Show a link and two search hits on their own lines; the shell keeps quiet while
    // `sleep` runs. The command builds them so its echo doesn't contain them. Windows
    // runs PowerShell (through ConPTY, which redraws rather than passing output through,
    // so the test finds the lines instead of clearing the screen), and is only fed once
    // its prompt is up.
    let command: &[u8] = if cfg!(windows) {
        wait_for_screen(cx, &terminal, |text| {
            text.lines().any(|line| line.starts_with("PS "))
        });
        b"Write-Host ('https://example.com/zp-{0}' -f 'terminal'); Write-Host ('find-{0} find-{0}' -f 'marker'); Start-Sleep 30\r"
    } else {
        b"printf 'https://example.com/zp-%s\\nfind-%s find-%s\\n' terminal marker marker; sleep 30\r"
    };
    terminal.update(cx, |view, _| view.write(command.to_vec()));
    wait_for_screen(cx, &terminal, |text| {
        let mut lines = text.lines().skip_while(|line| *line != LINK);
        lines.next().is_some() && lines.next() == Some("find-marker find-marker")
    });
    cx.run_until_parked();
    let point = terminal.read_with(cx, |view, _| {
        let metrics = *view.metrics.borrow();
        metrics.origin
            + point(
                metrics.cell.width * 8.,
                metrics.cell.height * (link_row(view) as f32 + 0.5),
            )
    });
    let modifier = if cfg!(target_os = "macos") {
        Modifiers {
            platform: true,
            ..Modifiers::none()
        }
    } else {
        Modifiers {
            control: true,
            ..Modifiers::none()
        }
    };
    {
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(point, Modifiers::none());
    }
    assert!(cx.opened_url().is_none());
    {
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.simulate_click(point, modifier);
    }
    assert_eq!(cx.opened_url().as_deref(), Some(LINK));

    let expected = terminal.read_with(cx, |view, _| {
        let mut search = search::Search::default();
        search.refresh(&view.term.lock(), "find-marker", true);
        search.matches.len()
    });
    assert!(expected >= 2);
    cx.simulate_keystrokes(
        window.into(),
        if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-shift-f"
        },
    );
    cx.simulate_input(window.into(), "find-marker");
    cx.run_until_parked();
    assert_eq!(
        terminal.read_with(cx, |view, _| view.search.as_ref().unwrap().matches.len()),
        expected
    );
    let before = terminal.read_with(cx, |view, _| view.search.as_ref().unwrap().current);
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    assert_ne!(
        terminal.read_with(cx, |view, _| view.search.as_ref().unwrap().current),
        before
    );
    cx.simulate_keystrokes(window.into(), "escape");
    cx.run_until_parked();
    assert!(terminal.read_with(cx, |view, _| view.search.is_none()));
    assert!(
        window
            .update(cx, |_, window, cx| terminal
                .read(cx)
                .focus_handle
                .is_focused(window))
            .unwrap()
    );

    // Select all, then copy. ConPTY repaints the screen a moment after a resize or focus
    // change (here: the search bar closing), and its line clears drop a selection that
    // covers them; retry until a copy gets through.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.simulate_keystrokes(
            window.into(),
            if cfg!(target_os = "macos") {
                "cmd-a cmd-c"
            } else {
                "ctrl-shift-a ctrl-shift-c"
            },
        );
        let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        if copied.as_deref().is_some_and(|text| text.contains(LINK)) {
            break;
        }
        let selected = terminal.read_with(cx, |view, _| view.term.lock().selection.is_some());
        assert!(
            Instant::now() < deadline,
            "select all and copy failed (selection kept: {selected}, clipboard: {copied:?})"
        );
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
    }
    terminal.update(cx, |view, cx| view.scroll_display(Scroll::Bottom, cx));
    cx.run_until_parked();
    let point = terminal.read_with(cx, |view, _| {
        let metrics = *view.metrics.borrow();
        metrics.origin
            + gpui::point(
                metrics.cell.width * 8.,
                metrics.cell.height * (link_row(view) as f32 + 0.5),
            )
    });
    terminal.update(cx, |view, _| {
        let target = view.grid_point(point).unwrap().0;
        assert!(view.link_detector.at(&view.term.lock(), target).is_some());
        view.update_hover(point, modifier);
        assert_eq!(
            view.hovered_link.as_ref().map(|link| link.url.as_str()),
            Some(LINK)
        );
    });
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |view, cx| {
                let mut parser: Processor = Processor::new();
                parser.advance(&mut *view.term.lock(), b"\x1b[?1000h\x1b[?1006h");
                view.mouse_down(
                    &MouseDownEvent {
                        position: point,
                        button: MouseButton::Left,
                        modifiers: Modifiers {
                            shift: true,
                            alt: true,
                            ..Modifiers::none()
                        },
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                assert!(view.selecting);
                assert!(view.reported_button.is_none());
                let end = point + gpui::point(view.metrics.borrow().cell.width * 5., px(0.));
                view.mouse_move(
                    &MouseMoveEvent {
                        position: end,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers {
                            shift: true,
                            alt: true,
                            ..Modifiers::none()
                        },
                    },
                    window,
                    cx,
                );
                assert_eq!(
                    view.term.lock().selection.as_ref().unwrap().ty,
                    SelectionType::Block
                );
                view.mouse_up(
                    &gpui::MouseUpEvent {
                        position: end,
                        button: MouseButton::Left,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
                assert!(!view.selecting);
                parser.advance(&mut *view.term.lock(), b"\x1b[?1000l\x1b[?1006l");
            });
        })
        .unwrap();
    window
        .update(cx, |_, window, cx| {
            terminal.update(cx, |view, cx| {
                view.mouse_down(
                    &MouseDownEvent {
                        position: point,
                        button: MouseButton::Right,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
            })
        })
        .unwrap();
    cx.run_until_parked();
    let menu = window
        .update(cx, |project, _, _| project.context_menu().unwrap())
        .unwrap();
    assert!(
        menu.read_with(cx, |menu, _| menu.labels())
            .contains(&"Copy Link".to_owned())
    );
}
