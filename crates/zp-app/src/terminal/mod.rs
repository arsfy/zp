//! An integrated terminal, like VS Code's panel: the user's shell in a PTY, emulated by
//! `alacritty_terminal` and drawn as a grid of monospace cells.
//!
//! The PTY is read on its own thread, which updates the shared [`Term`] and wakes the
//! view. Special keys become escape sequences ([`keys`]); text (and IME composition)
//! arrives through the platform input handler.

mod colors;
pub mod keys;
mod links;
mod process;
mod search;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, ColorExt as _, Context, DispatchPhase,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, Hsla, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, ScrollWheelEvent, StrikethroughStyle, Subscription,
    Task, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, font, point,
    prelude::*, px, rgb_to_hsla, size,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged};

use self::colors::{Palette, to_hsla};
use crate::context_menu::ContextMenu;
use crate::theme::ActiveTheme;

/// Lines kept above the screen.
const SCROLLBACK: usize = 10_000;
/// Line height relative to the font size.
const LINE_HEIGHT: f32 = 1.3;
/// Largest text a program may put on the clipboard through OSC 52.
const OSC52_MAX_BYTES: usize = 1 << 20;

/// Whether a program's OSC 52 copy reaches the system clipboard: only the clipboard
/// target (the primary selection would overwrite it too) and only reasonable sizes.
fn accepts_clipboard_store(target: ClipboardType, text: &str) -> bool {
    matches!(target, ClipboardType::Clipboard) && text.len() <= OSC52_MAX_BYTES
}

/// Forwards terminal events to the view.
#[derive(Clone)]
struct Listener(futures::channel::mpsc::UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

/// A terminal grid size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// What the last frame measured, for mouse and IME positions.
#[derive(Clone, Copy, Default)]
struct Metrics {
    origin: Point<Pixels>,
    cell: gpui::Size<Pixels>,
    grid: Option<GridSize>,
    cursor: Option<Bounds<Pixels>>,
}

pub enum TerminalEvent {
    /// The shell exited.
    Exited,
    /// The program set the title.
    TitleChanged,
    ContextMenu(Entity<ContextMenu>, Point<Pixels>),
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    focus_handle: FocusHandle,
    metrics: Rc<RefCell<Metrics>>,
    title: Option<String>,
    exited: Option<String>,
    /// IME composition in progress.
    marked: Option<String>,
    /// Where a mouse selection started (until the pointer moves).
    selection_anchor: Option<(GridPoint, Side, SelectionType)>,
    selecting: bool,
    /// Fractional lines of scrolling not applied yet.
    scroll_remainder: f32,
    link_detector: links::Detector,
    hovered_link: Option<links::Link>,
    pending_link: Option<links::Link>,
    reported_button: Option<MouseButton>,
    last_mouse_report: Option<(GridPoint, u8)>,
    reported_focus: bool,
    search_input: Entity<EditableTextState>,
    search: Option<search::Search>,
    _subscriptions: Vec<Subscription>,
    /// Finds the program in the foreground.
    process: process::Watch,
    /// Its name (`zsh`, `vim`, `cargo`…), once known.
    process_name: Option<String>,
    _events: Task<()>,
    _poll: Task<()>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// A started shell, before it gets a view.
pub struct Shell {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    received: futures::channel::mpsc::UnboundedReceiver<TermEvent>,
    process: process::Watch,
}

impl Shell {
    /// Starts the user's shell in `cwd`.
    pub fn spawn(cwd: &Path) -> anyhow::Result<Self> {
        let (events, received) = futures::channel::mpsc::unbounded();
        let listener = Listener(events);
        let grid = GridSize {
            columns: 80,
            lines: 24,
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            // Programs may copy, but never read the clipboard back.
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &grid, listener.clone())));
        let env = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ("TERM_PROGRAM".to_owned(), "zp".to_owned()),
        ]);
        let options = tty::Options {
            working_directory: Some(cwd.to_path_buf()),
            env,
            ..tty::Options::default()
        };
        let pty = tty::new(&options, window_size(grid, size(px(8.), px(16.))), 0)?;
        let process = process::Watch::new(&pty);
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Self {
            term,
            sender,
            received,
            process,
        })
    }
}

impl TerminalView {
    /// A view of `shell`.
    pub fn new(shell: Shell, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Shell {
            term,
            sender,
            mut received,
            process,
        } = shell;
        // The program in the foreground can change without output (e.g. `sleep`).
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(700))
                    .await;
                if this.update(cx, |this, cx| this.poll_process(cx)).is_err() {
                    break;
                }
            }
        });
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = received.next().await {
                let mut batch = vec![event];
                while let Ok(event) = received.try_recv() {
                    batch.push(event);
                }
                let updated = this.update(cx, |this, cx| {
                    let changed = batch.iter().any(|event| matches!(event, TermEvent::Wakeup));
                    for event in batch {
                        this.handle_event(event, cx);
                    }
                    if changed {
                        this.refresh_search(false, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let focus_handle = cx.focus_handle();
        let search_input = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let subscriptions = vec![
            cx.subscribe(&search_input, |this, _, _: &TextChanged, cx| {
                this.refresh_search(true, cx);
            }),
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                this.report_focus(window, cx)
            }),
            cx.on_blur(&focus_handle, window, |this, window, cx| {
                this.report_focus(window, cx)
            }),
            cx.observe_window_activation(window, |this, window, cx| this.report_focus(window, cx)),
        ];
        Self {
            term,
            sender,
            focus_handle,
            metrics: Rc::default(),
            title: None,
            exited: None,
            marked: None,
            selection_anchor: None,
            selecting: false,
            scroll_remainder: 0.0,
            link_detector: links::Detector::default(),
            hovered_link: None,
            pending_link: None,
            reported_button: None,
            last_mouse_report: None,
            reported_focus: false,
            search_input,
            search: None,
            _subscriptions: subscriptions,
            process,
            process_name: None,
            _events: task,
            _poll: poll,
        }
    }

    /// The name of the program in the foreground (the shell when idle).
    pub fn process_name(&self) -> Option<&str> {
        self.process_name.as_deref()
    }

    /// Looks up the foreground program when it changed.
    fn poll_process(&mut self, cx: &mut Context<Self>) {
        let Some(lookup) = self.process.changed() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let name = cx.background_executor().spawn(lookup).await;
            let _ = this.update(cx, |this, cx| {
                if name.is_some() && this.process_name != name {
                    this.process_name = name;
                    cx.emit(TerminalEvent::TitleChanged);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The title the program set, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[cfg(feature = "visual-tests")]
    pub(crate) fn cell_position(&self, line: usize, column: usize) -> Point<Pixels> {
        let metrics = *self.metrics.borrow();
        metrics.origin
            + point(
                metrics.cell.width * (column as f32 + 0.5),
                metrics.cell.height * (line as f32 + 0.5),
            )
    }

    /// The visible text (tests).
    #[cfg(test)]
    pub fn screen_text(&self) -> String {
        let term = self.term.lock();
        let grid = term.grid();
        (0..grid.screen_lines())
            .map(|line| {
                let row = &grid[Line(line as i32)];
                (0..grid.columns())
                    .map(|column| row[Column(column)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Sends `bytes` to the program.
    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            let _ = self.sender.send(Msg::Input(bytes));
        }
    }

    fn write_str(&self, text: &str) {
        self.write(text.as_bytes().to_vec());
    }

    fn handle_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup => {
                self.poll_process(cx);
                self.hovered_link = None;
                cx.notify()
            }
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => cx.notify(),
            TermEvent::Title(title) => {
                self.title = Some(title);
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            TermEvent::ResetTitle => {
                self.title = None;
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            TermEvent::ClipboardStore(target, text) => {
                if accepts_clipboard_store(target, &text) {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
            }
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                self.write_str(&format(&text));
            }
            TermEvent::ColorRequest(index, format) => {
                let theme = cx.theme().clone();
                let rgb = {
                    let term = self.term.lock();
                    let palette = Palette::new(&theme, term.colors());
                    match index {
                        256 => palette.named(NamedColor::Foreground),
                        257 => palette.named(NamedColor::Background),
                        258 => palette.named(NamedColor::Cursor),
                        index => palette.resolve(Color::Indexed(index.min(255) as u8)),
                    }
                };
                self.write_str(&format(rgb));
            }
            TermEvent::PtyWrite(text) => self.write_str(&text),
            TermEvent::TextAreaSizeRequest(format) => {
                let metrics = *self.metrics.borrow();
                if let Some(grid) = metrics.grid {
                    self.write_str(&format(window_size(grid, metrics.cell)));
                }
            }
            TermEvent::Bell => {}
            TermEvent::ChildExit(status) => {
                self.exited = Some(match status.code() {
                    Some(code) => format!("exited with code {code}"),
                    None => "exited".to_owned(),
                });
                cx.emit(TerminalEvent::Exited);
                cx.notify();
            }
            TermEvent::Exit => {
                if self.exited.is_none() {
                    self.exited = Some("exited".to_owned());
                    cx.emit(TerminalEvent::Exited);
                }
                cx.notify();
            }
        }
    }

    /// After typing: show the bottom and drop the selection.
    fn after_input(&mut self, cx: &mut Context<Self>) {
        let mut term = self.term.lock();
        term.scroll_display(Scroll::Bottom);
        term.selection = None;
        drop(term);
        self.hovered_link = None;
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let shortcut = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control && modifiers.shift
        };
        if shortcut {
            let handled = match keystroke.key.as_str() {
                "c" => {
                    self.copy(cx);
                    true
                }
                "v" => {
                    self.paste(cx);
                    true
                }
                "k" => {
                    self.clear(cx);
                    true
                }
                "a" => {
                    self.select_all(cx);
                    true
                }
                "f" => {
                    self.open_search(window, cx);
                    true
                }
                "g" if self.search.is_some() => {
                    self.next_match(modifiers.shift, cx);
                    true
                }
                "up" | "home" => {
                    self.scroll_display(Scroll::Top, cx);
                    true
                }
                "down" | "end" => {
                    self.scroll_display(Scroll::Bottom, cx);
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                return;
            }
            if modifiers.platform {
                return;
            }
        }
        if modifiers.shift && !modifiers.control && !modifiers.alt {
            let scroll = match keystroke.key.as_str() {
                "pageup" => Some(Scroll::PageUp),
                "pagedown" => Some(Scroll::PageDown),
                "home" => Some(Scroll::Top),
                "end" => Some(Scroll::Bottom),
                "insert" => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => None,
            };
            if let Some(scroll) = scroll {
                self.scroll_display(scroll, cx);
                cx.stop_propagation();
                return;
            }
        }
        if keystroke.key == "escape" && self.search.is_some() {
            self.close_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if keystroke.key == "f3" && self.search.is_some() {
            self.next_match(modifiers.shift, cx);
            cx.stop_propagation();
            return;
        }
        let mode = *self.term.lock().mode();
        if let Some(sequence) = keys::to_esc(keystroke, mode.contains(TermMode::APP_CURSOR)) {
            self.write_str(&sequence);
            self.after_input(cx);
            cx.stop_propagation();
        }
    }

    /// Copies the selection; `false` without one.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.term.lock().selection_to_string() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let mut term = self.term.lock();
        let mut selection = Selection::new(
            SelectionType::Simple,
            GridPoint::new(term.topmost_line(), Column(0)),
            Side::Left,
        );
        selection.update(
            GridPoint::new(term.bottommost_line(), term.last_column()),
            Side::Right,
        );
        term.selection = Some(selection);
        drop(term);
        cx.notify();
    }

    fn scroll_display(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        self.term.lock().scroll_display(scroll);
        self.hovered_link = None;
        self.last_mouse_report = None;
        cx.notify();
    }

    fn report_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        if focused != self.reported_focus {
            if self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
                self.write_str(if focused { "\x1b[I" } else { "\x1b[O" });
            }
            self.reported_focus = focused;
        }
        if !focused {
            self.selecting = false;
            self.selection_anchor = None;
            self.pending_link = None;
        }
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        self.write_str(&keys::paste(&text, bracketed));
        self.after_input(cx);
    }

    /// Clears the screen and scrollback (like ⌘K), then asks the shell to redraw.
    fn clear(&mut self, cx: &mut Context<Self>) {
        {
            let mut term = self.term.lock();
            term.grid_mut().clear_history();
        }
        // Form feed: shells clear the screen and redraw the prompt.
        self.write_str("\x0c");
        self.after_input(cx);
    }

    /// The grid cell under window position `position`.
    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let metrics = *self.metrics.borrow();
        let grid = metrics.grid?;
        let offset = position - metrics.origin;
        let column_f = (f32::from(offset.x) / f32::from(metrics.cell.width)).max(0.0);
        let line = (f32::from(offset.y) / f32::from(metrics.cell.height))
            .floor()
            .clamp(0.0, (grid.lines - 1) as f32) as i32;
        let column = (column_f.floor() as usize).min(grid.columns - 1);
        let side = if column_f.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let display_offset = self.term.lock().grid().display_offset() as i32;
        Some((
            GridPoint::new(Line(line - display_offset), Column(column)),
            side,
        ))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.focus_handle.focus(window, cx);
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if link_modifier(event.modifiers)
            && event.button == MouseButton::Left
            && self.inside_grid(event.position)
        {
            self.pending_link = self.link_detector.at(&self.term.lock(), point);
            if self.pending_link.is_some() {
                return;
            }
        }
        let mode = *self.term.lock().mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            if let Some(code) = mouse_button_code(event.button) {
                self.reported_button = Some(event.button);
                self.send_mouse(code, point, false, event.modifiers);
            }
            return;
        }
        if event.button == MouseButton::Right {
            self.open_context_menu(event.position, point, window, cx);
            return;
        }
        if event.button == MouseButton::Middle {
            self.paste(cx);
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let kind = if event.modifiers.alt {
            SelectionType::Block
        } else {
            match event.click_count {
                2 => SelectionType::Semantic,
                3.. => SelectionType::Lines,
                _ => SelectionType::Simple,
            }
        };
        let mut term = self.term.lock();
        if event.modifiers.shift
            && !event.modifiers.alt
            && let Some(selection) = term.selection.as_mut()
        {
            selection.update(point, side);
        } else if matches!(kind, SelectionType::Simple | SelectionType::Block) {
            term.selection = None;
            self.selection_anchor = Some((point, side, kind));
        } else {
            term.selection = Some(Selection::new(kind, point, side));
            self.selection_anchor = None;
        }
        self.selecting = true;
        drop(term);
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let previous_hover = self.hovered_link.clone();
        self.update_hover(event.position, event.modifiers);
        let hover_changed = self.hovered_link != previous_hover;
        if self
            .pending_link
            .as_ref()
            .is_some_and(|link| self.hovered_link.as_ref() != Some(link))
        {
            self.pending_link = None;
        }
        if self.pending_link.is_some() {
            if hover_changed {
                cx.notify();
            }
            return;
        }
        let mode = *self.term.lock().mode();
        if !self.selecting
            && self.inside_grid(event.position)
            && !event.modifiers.shift
            && !link_modifier(event.modifiers)
        {
            let report = mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && self.reported_button.is_some());
            if report && let Some((point, _)) = self.grid_point(event.position) {
                let code = self
                    .reported_button
                    .and_then(mouse_button_code)
                    .unwrap_or(3)
                    + 32;
                if self.last_mouse_report != Some((point, code)) {
                    self.send_mouse(code, point, false, event.modifiers);
                }
            }
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            if hover_changed {
                cx.notify();
            }
            return;
        }
        let metrics = *self.metrics.borrow();
        if let Some(grid) = metrics.grid {
            let delta = if event.position.y < metrics.origin.y {
                1
            } else if event.position.y > metrics.origin.y + metrics.cell.height * grid.lines as f32
            {
                -1
            } else {
                0
            };
            if delta != 0 {
                self.term.lock().scroll_display(Scroll::Delta(delta));
            }
        }
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        let mut term = self.term.lock();
        if let Some((anchor, anchor_side, kind)) = self.selection_anchor.take() {
            term.selection = Some(Selection::new(kind, anchor, anchor_side));
        }
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
        }
        drop(term);
        cx.notify();
    }

    fn mouse_up(&mut self, event: &gpui::MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.reported_button == Some(event.button) {
            if let Some((point, _)) = self.grid_point(event.position)
                && let Some(code) = mouse_button_code(event.button)
            {
                self.send_mouse(code, point, true, event.modifiers);
            }
            self.reported_button = None;
        }
        if event.button != MouseButton::Left {
            return;
        }
        if let Some(link) = self.pending_link.take()
            && link_modifier(event.modifiers)
            && self.inside_grid(event.position)
            && let Some((point, _)) = self.grid_point(event.position)
            && self.link_detector.at(&self.term.lock(), point).as_ref() == Some(&link)
        {
            cx.open_url(&link.url);
        }
        self.selecting = false;
        self.selection_anchor = None;
        cx.notify();
    }

    fn inside_grid(&self, position: Point<Pixels>) -> bool {
        let metrics = *self.metrics.borrow();
        metrics.grid.is_some_and(|grid| {
            Bounds::new(
                metrics.origin,
                size(
                    metrics.cell.width * grid.columns as f32,
                    metrics.cell.height * grid.lines as f32,
                ),
            )
            .contains(&position)
        })
    }

    fn update_hover(&mut self, position: Point<Pixels>, modifiers: Modifiers) {
        self.hovered_link =
            if link_modifier(modifiers) && !self.selecting && self.inside_grid(position) {
                self.grid_point(position)
                    .and_then(|(point, _)| self.link_detector.at(&self.term.lock(), point))
            } else {
                None
            };
    }

    fn send_mouse(&mut self, code: u8, point: GridPoint, release: bool, modifiers: Modifiers) {
        let term = self.term.lock();
        let line = (point.line.0 + term.grid().display_offset() as i32).max(0) as usize;
        if let Some(bytes) =
            keys::mouse_report(code, point.column.0, line, release, modifiers, *term.mode())
        {
            self.write(bytes);
        }
        self.last_mouse_report = if release { None } else { Some((point, code)) };
    }

    fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        point: GridPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = self.link_detector.at(&self.term.lock(), point);
        let selected = self.term.lock().selection_to_string().is_some();
        let this = cx.entity().downgrade();
        let menu = ContextMenu::new(window, cx, |mut menu| {
            if let Some(link) = link {
                let url = link.url.clone();
                menu = menu.item("Open Link", move |_, cx| cx.open_url(&url));
                menu = menu
                    .item("Copy Link", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(link.url.clone()))
                    })
                    .separator();
            }
            if selected {
                let view = this.clone();
                menu = menu.item("Copy", move |_, cx| {
                    let _ = view.update(cx, |view, cx| {
                        view.copy(cx);
                    });
                });
            }
            let paste = this.clone();
            let select = this.clone();
            let find = this.clone();
            menu.item("Paste", move |window, cx| {
                let _ = paste.update(cx, |view, cx| {
                    view.focus_handle.focus(window, cx);
                    view.paste(cx);
                });
            })
            .item("Select All", move |window, cx| {
                let _ = select.update(cx, |view, cx| {
                    view.focus_handle.focus(window, cx);
                    view.select_all(cx);
                });
            })
            .item("Find", move |window, cx| {
                let _ = find.update(cx, |view, cx| view.open_search(window, cx));
            })
            .separator()
            .item("Clear Terminal", move |window, cx| {
                let _ = this.update(cx, |view, cx| {
                    view.focus_handle.focus(window, cx);
                    view.clear(cx);
                });
            })
        });
        cx.emit(TerminalEvent::ContextMenu(menu, position));
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.metrics.borrow().cell.height;
        if line_height <= px(0.) {
            return;
        }
        let delta = f32::from(event.delta.pixel_delta(line_height).y) / f32::from(line_height);
        let lines = delta + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        let whole = whole as i32;
        if whole == 0 {
            return;
        }
        let mode = *self.term.lock().mode();
        let whole = whole.clamp(-1000, 1000);
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            if let Some((point, _)) = self.grid_point(event.position) {
                for _ in 0..whole.unsigned_abs() {
                    self.send_mouse(
                        if whole > 0 { 64 } else { 65 },
                        point,
                        false,
                        event.modifiers,
                    );
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !event.modifiers.shift
        {
            // Full-screen programs (less, vim) scroll with the arrow keys.
            let key = match (whole > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => "\x1bOA",
                (true, false) => "\x1b[A",
                (false, true) => "\x1bOB",
                (false, false) => "\x1b[B",
            };
            self.write_str(&key.repeat(whole.unsigned_abs() as usize));
        } else {
            self.scroll_display(Scroll::Delta(whole), cx);
        }
        cx.notify();
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let term = self.term.clone();
        let sender = self.sender.clone();
        let metrics = self.metrics.clone();
        let (paint_term, paint_metrics) = (self.term.clone(), self.metrics.clone());
        let theme = cx.theme().clone();
        let focus = self.focus_handle.clone();
        let view = cx.entity();
        let resize_view = cx.entity().downgrade();
        let marked = self.marked.clone();
        let hovered = self.hovered_link.clone();
        let matches = self
            .search
            .as_ref()
            .map(|search| search.matches.clone())
            .unwrap_or_default();
        let active_match = self
            .search
            .as_ref()
            .and_then(|search| search.active().cloned());
        let font_family = theme.metrics.mono_font.clone();
        let font_size = theme.metrics.font_size - px(1.);
        canvas(
            move |bounds, window, cx| {
                let base = font(font_family);
                let text_system = window.text_system();
                let font_id = text_system.resolve_font(&base);
                let cell_width = text_system
                    .advance(font_id, font_size, 'm')
                    .map_or(font_size * 0.6, |advance| advance.width);
                let cell = size(cell_width, (font_size * LINE_HEIGHT).round());
                let grid = GridSize {
                    columns: ((bounds.size.width / cell.width).floor() as usize).max(2),
                    lines: ((bounds.size.height / cell.height).floor() as usize).max(1),
                };
                let mut stored = metrics.borrow_mut();
                if stored.grid != Some(grid) {
                    term.lock().resize(grid);
                    let _ = sender.send(Msg::Resize(window_size(grid, cell)));
                    let view = resize_view.clone();
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |view, cx| view.refresh_search(false, cx));
                    });
                }
                stored.grid = Some(grid);
                stored.cell = cell;
                stored.origin = bounds.origin;
                base
            },
            move |bounds, base, window, cx| {
                let (term, metrics) = (paint_term, paint_metrics);
                window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                let focused = focus.is_focused(window);
                let cell = metrics.borrow().cell;
                let term = term.lock();
                let content = term.renderable_content();
                let palette = Palette::new(&theme, content.colors);
                let default_background = palette.background();
                let selection_background = to_hsla(Rgb {
                    r: 0x26,
                    g: 0x4f,
                    b: 0x78,
                });
                let display_offset = content.display_offset as i32;
                let origin = bounds.origin;
                let at = |line: usize, column: usize| {
                    point(
                        origin.x + cell.width * column as f32,
                        origin.y + cell.height * line as f32,
                    )
                };

                let mut backgrounds: Vec<(usize, usize, usize, Hsla)> = Vec::new();
                let mut segments: Vec<Segment> = Vec::new();
                for indexed in content.display_iter {
                    let flags = indexed.cell.flags;
                    if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                        continue;
                    }
                    let line = (indexed.point.line.0 + display_offset) as usize;
                    let column = indexed.point.column.0;
                    let width = if flags.contains(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    };
                    let mut fg_color = indexed.cell.fg;
                    if flags.contains(Flags::BOLD) {
                        fg_color = palette.brighten(fg_color);
                    }
                    let mut fg = palette.resolve(fg_color);
                    let mut bg = palette.resolve(indexed.cell.bg);
                    if flags.contains(Flags::DIM) {
                        fg = Rgb {
                            r: (f32::from(fg.r) * 0.66) as u8,
                            g: (f32::from(fg.g) * 0.66) as u8,
                            b: (f32::from(fg.b) * 0.66) as u8,
                        };
                    }
                    if flags.contains(Flags::INVERSE) {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    let selected = content
                        .selection
                        .is_some_and(|range| range.contains(indexed.point));
                    let background = if selected {
                        Some(selection_background)
                    } else if active_match
                        .as_ref()
                        .is_some_and(|range| range.contains(&indexed.point))
                    {
                        Some(rgb_to_hsla(theme.colors.accent).opacity(0.45))
                    } else if matches
                        .get(matches.partition_point(|range| *range.end() < indexed.point))
                        .is_some_and(|range| range.contains(&indexed.point))
                    {
                        Some(rgb_to_hsla(theme.colors.warning).opacity(0.25))
                    } else {
                        (bg != default_background).then(|| to_hsla(bg))
                    };
                    if let Some(color) = background {
                        match backgrounds.last_mut() {
                            Some((l, start, len, c))
                                if *l == line && *start + *len == column && *c == color =>
                            {
                                *len += width;
                            }
                            _ => backgrounds.push((line, column, width, color)),
                        }
                    }
                    if flags.contains(Flags::HIDDEN) {
                        continue;
                    }
                    let mut text = String::new();
                    text.push(indexed.cell.c);
                    if let Some(extra) = indexed.cell.zerowidth() {
                        text.extend(extra);
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    let style = Style {
                        color: if hovered
                            .as_ref()
                            .is_some_and(|link| link.range.contains(&indexed.point))
                        {
                            rgb_to_hsla(theme.colors.accent)
                        } else {
                            to_hsla(fg)
                        },
                        bold: flags.contains(Flags::BOLD),
                        italic: flags.contains(Flags::ITALIC),
                        underline: flags.intersects(Flags::ALL_UNDERLINES)
                            || hovered
                                .as_ref()
                                .is_some_and(|link| link.range.contains(&indexed.point)),
                        strike: flags.contains(Flags::STRIKEOUT),
                    };
                    match segments.last_mut() {
                        Some(segment)
                            if width == 1
                                && !segment.wide
                                && segment.line == line
                                && segment.end == column
                                && segment.style == style =>
                        {
                            segment.text.push_str(&text);
                            segment.end += 1;
                        }
                        _ => segments.push(Segment {
                            line,
                            start: column,
                            end: column + width,
                            wide: width == 2,
                            text,
                            style,
                        }),
                    }
                }

                for (line, column, len, color) in backgrounds {
                    window.paint_quad(quad_fill(
                        Bounds::new(at(line, column), size(cell.width * len as f32, cell.height)),
                        color,
                    ));
                }
                let font_size = cell.height / LINE_HEIGHT;
                for segment in &segments {
                    paint_text(
                        &segment.text,
                        &segment.style,
                        &base,
                        font_size,
                        at(segment.line, segment.start),
                        cell.height,
                        window,
                        cx,
                    );
                }

                // The cursor (only at the bottom of the scrollback).
                let cursor = content.cursor;
                let line = cursor.point.line.0 + display_offset;
                let visible = cursor.shape != CursorShape::Hidden
                    && line >= 0
                    && (line as usize) < term.screen_lines();
                if visible {
                    let line = line as usize;
                    let column = cursor.point.column.0;
                    let wide = term.grid()[cursor.point].flags.contains(Flags::WIDE_CHAR);
                    let width = cell.width * if wide { 2.0 } else { 1.0 };
                    let bounds = Bounds::new(at(line, column), size(width, cell.height));
                    metrics.borrow_mut().cursor = Some(bounds);
                    let color = to_hsla(palette.named(NamedColor::Cursor));
                    if let Some(marked) = marked.as_deref() {
                        let style = Style {
                            color: to_hsla(palette.named(NamedColor::Foreground)),
                            underline: true,
                            ..Style::default()
                        };
                        window.paint_quad(quad_fill(
                            Bounds::new(
                                bounds.origin,
                                size(
                                    cell.width * marked.chars().count() as f32 * 2.0,
                                    cell.height,
                                ),
                            ),
                            to_hsla(default_background),
                        ));
                        paint_text(
                            marked,
                            &style,
                            &base,
                            font_size,
                            bounds.origin,
                            cell.height,
                            window,
                            cx,
                        );
                    } else if !focused {
                        window.paint_quad(
                            gpui::PaintQuad::new(bounds)
                                .border_widths(1f32)
                                .border_color(color)
                                .border_style(gpui::BorderStyle::Solid),
                        );
                    } else {
                        match cursor.shape {
                            CursorShape::Beam => window.paint_quad(quad_fill(
                                Bounds::new(bounds.origin, size(px(2.), cell.height)),
                                color,
                            )),
                            CursorShape::Underline => window.paint_quad(quad_fill(
                                Bounds::new(
                                    point(bounds.origin.x, bounds.bottom() - px(2.)),
                                    size(width, px(2.)),
                                ),
                                color,
                            )),
                            _ => {
                                window.paint_quad(quad_fill(bounds, color));
                                // The character under a block cursor, in the background color.
                                let c = term.grid()[cursor.point].c;
                                if !c.is_whitespace() {
                                    let style = Style {
                                        color: to_hsla(default_background),
                                        ..Style::default()
                                    };
                                    paint_text(
                                        &c.to_string(),
                                        &style,
                                        &base,
                                        font_size,
                                        bounds.origin,
                                        cell.height,
                                        window,
                                        cx,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    metrics.borrow_mut().cursor = None;
                }
            },
        )
        .size_full()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Style {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn link_modifier(modifiers: Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

fn mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

/// Cells of one line with the same style, painted as one run.
struct Segment {
    line: usize,
    start: usize,
    end: usize,
    wide: bool,
    text: String,
    style: Style,
}

#[allow(clippy::too_many_arguments)]
fn paint_text(
    text: &str,
    style: &Style,
    base: &Font,
    font_size: Pixels,
    origin: Point<Pixels>,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let mut font = base.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    let run = TextRun {
        len: text.len(),
        font,
        color: style.color,
        background_color: None,
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(style.color),
            wavy: false,
        }),
        strikethrough: style.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.color),
        }),
        letter_spacing: None,
    };
    let shaped = window
        .text_system()
        .shape_line(text.to_owned().into(), font_size, &[run]);
    let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
}

fn quad_fill(bounds: Bounds<Pixels>, color: Hsla) -> gpui::PaintQuad {
    gpui::PaintQuad::new(bounds).background(color)
}

fn window_size(grid: GridSize, cell: gpui::Size<Pixels>) -> WindowSize {
    WindowSize {
        num_lines: grid.lines as u16,
        num_cols: grid.columns as u16,
        cell_width: f32::from(cell.width) as u16,
        cell_height: f32::from(cell.height) as u16,
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        self.update_hover(window.mouse_position(), window.modifiers());
        let movement = cx.entity().downgrade();
        let release = cx.entity().downgrade();
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(
                cx.listener(|this, _: &crate::actions::TerminalFindNext, _, cx| {
                    this.next_match(false, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::TerminalFindPrevious, _, cx| {
                    this.next_match(true, cx)
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::actions::TerminalCloseSearch, window, cx| {
                    this.close_search(window, cx)
                },
            ))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .children(self.render_search(cx))
            .child(
                div()
                    .id("terminal-grid")
                    .debug_selector(|| "terminal-grid".into())
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
                    .on_mouse_move_all(move |event, phase, _, window, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ =
                                movement.update(cx, |view, cx| view.mouse_move(event, window, cx));
                        }
                    })
                    .on_mouse_up_all(move |event, phase, _, window, cx| {
                        if phase == DispatchPhase::Bubble {
                            let _ = release.update(cx, |view, cx| view.mouse_up(event, window, cx));
                        }
                    })
                    .on_modifiers_changed(cx.listener(|_, _, _, cx| cx.notify()))
                    .on_scroll_wheel(cx.listener(Self::scroll))
                    .cursor_text()
                    .when(self.hovered_link.is_some(), |this| this.cursor_pointer())
                    .when_some(self.hovered_link.as_ref(), |this, link| {
                        this.tooltip(crate::ui::tooltip(link.url.clone()))
                    })
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px_3()
                    .py_1()
                    .child(self.render_grid(cx)),
            )
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|marked| 0..marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.write_str(text);
        self.after_input(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.metrics.borrow().cursor
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
