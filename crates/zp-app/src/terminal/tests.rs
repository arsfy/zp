use alacritty_terminal::event::VoidListener;
use alacritty_terminal::vte::ansi::Processor;
use gpui::{Modifiers, TestAppContext, VisualTestContext};

use super::*;
use crate::tests::project_dir;
use crate::tests_conversation::{open, setup_models, wait_until};

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
    terminal.update(cx, |view, _| view.write(b"printf '\\033[2J\\033[Hhttps://example.com/zp-terminal\\nfind-marker find-marker\\n'; sleep 30\r".to_vec()));
    wait_until(cx, |cx| {
        terminal.read_with(cx, |view, _| {
            view.screen_text()
                .starts_with("https://example.com/zp-terminal")
        })
    });
    cx.run_until_parked();
    let point = terminal.read_with(cx, |view, _| {
        let metrics = *view.metrics.borrow();
        metrics.origin + point(metrics.cell.width * 8., metrics.cell.height / 2.)
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
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/zp-terminal")
    );

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

    cx.simulate_keystrokes(
        window.into(),
        if cfg!(target_os = "macos") {
            "cmd-a cmd-c"
        } else {
            "ctrl-shift-a ctrl-shift-c"
        },
    );
    let copied = cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap());
    assert!(copied.contains("https://example.com/zp-terminal"));
    terminal.update(cx, |view, cx| view.scroll_display(Scroll::Bottom, cx));
    cx.run_until_parked();
    let point = terminal.read_with(cx, |view, _| {
        let metrics = *view.metrics.borrow();
        let line = view
            .screen_text()
            .lines()
            .position(|line| line.starts_with("https://example.com/zp-terminal"))
            .unwrap();
        metrics.origin
            + gpui::point(
                metrics.cell.width * 8.,
                metrics.cell.height * (line as f32 + 0.5),
            )
    });
    terminal.update(cx, |view, _| {
        let target = view.grid_point(point).unwrap().0;
        assert!(view.link_detector.at(&view.term.lock(), target).is_some());
        view.update_hover(point, modifier);
        assert_eq!(
            view.hovered_link.as_ref().map(|link| link.url.as_str()),
            Some("https://example.com/zp-terminal")
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
