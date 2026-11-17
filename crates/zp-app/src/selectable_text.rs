//! Selection shared by the independently rendered text fragments in a conversation.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, ColorExt as _, CursorStyle, DispatchPhase, Element, ElementId, FocusHandle,
    GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior, InlineBoxRequest, InlineLayout,
    InlineLayoutRequest, InlineTextMetrics, InspectorElementId, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseUpEvent, PaintQuad, Pixels, Point, Rgba, ShapedTextLayout, SharedString,
    Style, TextLayout, TextLayoutOptions, TextSelectionKind, VerticalAlign, Window, px,
    rgb_to_hsla, size,
};

use crate::theme::ActiveTheme;

pub type TextId = (usize, usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Endpoint {
    id: TextId,
    offset: usize,
}

struct Fragment {
    text: SharedString,
    separator: &'static str,
}

#[derive(Clone, Default)]
struct Geometry(Rc<RefCell<Option<TextGeometry>>>);

struct TextGeometry {
    inline: Arc<InlineLayout>,
    shaped: Arc<ShapedTextLayout>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    spans: Vec<TextSpan>,
}

struct TextSpan {
    range: Range<usize>,
    line: usize,
    x_range: Range<Pixels>,
}

impl TextGeometry {
    fn spans(inline: &InlineLayout, ranges: &[Range<usize>]) -> Vec<TextSpan> {
        let mut spans = Vec::new();
        for (source_run, range) in ranges.iter().enumerate() {
            for (line_index, line) in inline.layout.visual_lines.iter().enumerate() {
                let fragments = &inline.layout.paint_fragments[line.paint_fragment_range.clone()];
                let mut fragments = fragments
                    .iter()
                    .filter(|fragment| fragment.source_run == source_run);
                let Some(first) = fragments.next() else {
                    continue;
                };
                let mut x_range = first.x_range.clone();
                for fragment in fragments {
                    x_range.start = x_range.start.min(fragment.x_range.start);
                    x_range.end = x_range.end.max(fragment.x_range.end);
                }
                x_range.start += inline.lines[line_index].origin.x;
                x_range.end += inline.lines[line_index].origin.x;
                let range =
                    range.start.max(line.text_range.start)..range.end.min(line.text_range.end);
                if !range.is_empty() {
                    spans.push(TextSpan {
                        range,
                        line: line_index,
                        x_range,
                    });
                }
            }
        }
        spans
    }

    // Text interaction uses uniform rows, while inline padding changes painted row heights.
    fn interaction_point(&self, mut point: Point<Pixels>) -> Point<Pixels> {
        let lines = &self.inline.lines;
        if let Some((index, line)) = lines
            .iter()
            .enumerate()
            .find(|(_, line)| point.y < line.origin.y + line.size.height)
            .or_else(|| lines.last().map(|line| (lines.len() - 1, line)))
        {
            point.y =
                self.line_height * (index as f32 + (point.y - line.origin.y) / line.size.height);
        }
        point
    }

    #[cfg(any(test, feature = "visual-tests"))]
    fn painted_point(&self, mut point: Point<Pixels>) -> Point<Pixels> {
        let row = (point.y / self.line_height).max(0.);
        if let Some(line) = self.inline.lines.get(row as usize) {
            point.y = line.origin.y + line.size.height * row.fract();
        }
        point
    }

    fn selection_bounds(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut regions = Vec::new();
        for span in &self.spans {
            let range = range.start.max(span.range.start)..range.end.min(span.range.end);
            if range.is_empty() {
                continue;
            }
            for mut bounds in self.shaped.selection_bounds(range, self.line_height) {
                if (bounds.origin.y / self.line_height).max(0.) as usize == span.line {
                    let line = &self.inline.lines[span.line];
                    // Source ranges can include synthetic padding boxes at their boundaries.
                    let left = bounds.left().max(span.x_range.start);
                    let right = bounds.right().min(span.x_range.end);
                    if right <= left {
                        continue;
                    }
                    bounds.origin.x = left;
                    bounds.size.width = right - left;
                    bounds.origin.y = line.origin.y;
                    bounds.size.height = line.size.height;
                    regions.push(bounds);
                }
            }
        }
        regions
    }

    fn code_background_bounds(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        let mut regions: Vec<_> = self
            .selection_bounds(range)
            .into_iter()
            .map(|mut bounds| {
                let left = (bounds.left() - px(4.)).max(Pixels::ZERO);
                let right = (bounds.right() + px(4.)).min(self.inline.size.width);
                bounds.origin.x = left;
                bounds.size.width = right - left;
                bounds
            })
            .collect();
        regions.sort_by_key(|bounds| (bounds.origin.y, bounds.origin.x));
        let mut merged: Vec<Bounds<Pixels>> = Vec::new();
        for bounds in regions {
            if let Some(previous) = merged.last_mut()
                && previous.origin.y == bounds.origin.y
                && previous.size.height == bounds.size.height
                && previous.right() >= bounds.left()
            {
                previous.size.width = previous.right().max(bounds.right()) - previous.left();
            } else {
                merged.push(bounds);
            }
        }
        merged
    }
}

impl Geometry {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().as_ref().unwrap().bounds
    }

    #[cfg(any(test, feature = "visual-tests"))]
    fn line_height(&self) -> Pixels {
        self.0.borrow().as_ref().unwrap().line_height
    }

    fn index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let geometry = self.0.borrow();
        let geometry = geometry.as_ref().unwrap();
        geometry.shaped.closest_byte_index_for_pixel_point(
            geometry.interaction_point(position - geometry.bounds.origin),
            geometry.line_height,
        )
    }

    #[cfg(any(test, feature = "visual-tests"))]
    fn position_for_index(&self, offset: usize) -> Option<Point<Pixels>> {
        let geometry = self.0.borrow();
        let geometry = geometry.as_ref()?;
        Some(
            geometry.bounds.origin
                + geometry.painted_point(
                    geometry
                        .shaped
                        .visual_position_for_byte_index(offset, geometry.line_height)?,
                ),
        )
    }

    fn selection_at(&self, position: Point<Pixels>, kind: TextSelectionKind) -> Range<usize> {
        let geometry = self.0.borrow();
        let geometry = geometry.as_ref().unwrap();
        geometry.shaped.selection_from_pixel_point(
            geometry.interaction_point(position - geometry.bounds.origin),
            geometry.line_height,
            kind,
        )
    }
}

#[derive(Default)]
struct SelectionState {
    fragments: BTreeMap<TextId, Fragment>,
    regions: BTreeMap<TextId, Geometry>,
    anchor: Option<Endpoint>,
    head: Option<Endpoint>,
    dragging: bool,
}

impl SelectionState {
    fn endpoints(&self) -> Option<(Endpoint, Endpoint)> {
        let (anchor, head) = (self.anchor?, self.head?);
        Some((anchor.min(head), anchor.max(head)))
    }

    fn range(&self, id: TextId) -> Option<Range<usize>> {
        let (start, end) = self.endpoints()?;
        if id < start.id || id > end.id {
            return None;
        }
        let text = &self.fragments.get(&id)?.text;
        let start = if id == start.id { start.offset } else { 0 };
        let end = if id == end.id { end.offset } else { text.len() };
        let start = text.floor_char_boundary(start.min(text.len()));
        let end = text.floor_char_boundary(end.min(text.len()));
        (start < end).then_some(start..end)
    }

    fn selected_text(&self) -> Option<String> {
        let mut text = String::new();
        for (&id, fragment) in &self.fragments {
            if let Some(range) = self.range(id) {
                if !text.is_empty() {
                    text.push_str(fragment.separator);
                }
                text.push_str(&fragment.text[range]);
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

#[derive(Clone)]
pub struct TextSelection {
    state: Rc<RefCell<SelectionState>>,
    focus: FocusHandle,
}

impl TextSelection {
    pub fn new(focus: FocusHandle) -> Self {
        Self {
            state: Rc::default(),
            focus,
        }
    }

    pub fn clear(&self) {
        *self.state.borrow_mut() = SelectionState::default();
    }

    pub fn begin_frame(&self) {
        self.state.borrow_mut().regions.clear();
    }

    pub fn begin_document(&self, row: usize, block: usize) {
        self.state
            .borrow_mut()
            .fragments
            .retain(|id, _| (id.0, id.1) != (row, block));
    }

    pub fn selected_text(&self) -> Option<String> {
        self.state.borrow().selected_text()
    }

    pub fn select_all(&self) {
        let mut state = self.state.borrow_mut();
        let row = state.anchor.map(|anchor| anchor.id.0);
        let mut fragments = state
            .fragments
            .iter()
            .filter(|(id, _)| row.is_none_or(|row| id.0 == row));
        let Some((&first, first_fragment)) = fragments.next() else {
            return;
        };
        let (&last, fragment) = fragments.next_back().unwrap_or((&first, first_fragment));
        let end = fragment.text.len();
        state.anchor = Some(Endpoint {
            id: first,
            offset: 0,
        });
        state.head = Some(Endpoint {
            id: last,
            offset: end,
        });
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    pub fn extend_at(&self, position: Point<Pixels>, window: &mut Window) {
        let mut state = self.state.borrow_mut();
        if !state.dragging {
            return;
        }
        let nearest = state.regions.iter().min_by(|(_, left), (_, right)| {
            let distance = |layout: &Geometry| {
                let bounds = layout.bounds();
                let dx = (bounds.left() - position.x)
                    .max(position.x - bounds.right())
                    .max(Pixels::ZERO);
                let dy = (bounds.top() - position.y)
                    .max(position.y - bounds.bottom())
                    .max(Pixels::ZERO);
                let (dx, dy) = (f32::from(dx), f32::from(dy));
                dx * dx + dy * dy
            };
            distance(left).total_cmp(&distance(right))
        });
        if let Some((&id, layout)) = nearest {
            let offset = layout
                .index_for_position(position)
                .unwrap_or_else(|index| index);
            let head = Endpoint { id, offset };
            if state.head != Some(head) {
                state.head = Some(head);
                window.refresh();
            }
        }
    }

    pub fn end_drag(&self) {
        self.state.borrow_mut().dragging = false;
    }

    #[cfg(any(test, feature = "visual-tests"))]
    pub(crate) fn point_for_text(&self, text: &str, offset: usize) -> Option<Point<Pixels>> {
        let state = self.state.borrow();
        let (id, _) = state
            .fragments
            .iter()
            .find(|(_, fragment)| fragment.text.as_ref() == text)?;
        let layout = state.regions.get(id)?;
        let mut position = layout.position_for_index(offset)?;
        position.y += layout.line_height() / 2.;
        Some(position)
    }

    #[cfg(feature = "visual-tests")]
    pub(crate) fn assert_inline_geometry(&self, text: &str, ranges: &[Range<usize>]) {
        let state = self.state.borrow();
        let (id, _) = state
            .fragments
            .iter()
            .find(|(_, fragment)| fragment.text.as_ref() == text)
            .unwrap();
        let geometry = state.regions[id].0.borrow();
        let geometry = geometry.as_ref().unwrap();
        let native = geometry
            .inline
            .layout
            .platform_layout
            .inline_geometry_for_ranges(ranges);
        assert!(
            geometry.inline.lines.len() >= 3,
            "fixture must wrap across multiple rows"
        );
        for (range, native) in ranges.iter().zip(native) {
            let painted = geometry.selection_bounds(range.clone());
            assert_eq!(painted.len(), native.len());
            for (painted, native) in painted.iter().zip(native) {
                assert_eq!(painted.origin.y, native.bounds.origin.y);
                assert_eq!(painted.size.height, native.bounds.size.height);
                let interaction = geometry.interaction_point(native.bounds.center());
                assert_eq!(
                    (interaction.y / geometry.line_height) as usize,
                    native.visual_line_index
                );
            }
            let source_run = ranges
                .iter()
                .position(|candidate| candidate == range)
                .unwrap()
                * 2
                + 1;
            for (line_index, line) in geometry.inline.layout.visual_lines.iter().enumerate() {
                for fragment in
                    &geometry.inline.layout.paint_fragments[line.paint_fragment_range.clone()]
                {
                    if fragment.source_run == source_run {
                        let bounds = painted
                            .iter()
                            .find(|bounds| {
                                bounds.origin.y == geometry.inline.lines[line_index].origin.y
                            })
                            .unwrap();
                        assert_eq!(
                            bounds.origin.x,
                            geometry.inline.lines[line_index].origin.x + fragment.x_range.start,
                            "glyph and selection x disagree for {}",
                            &text[range.clone()]
                        );
                        assert_eq!(
                            bounds.right(),
                            geometry.inline.lines[line_index].origin.x + fragment.x_range.end,
                            "glyph and selection width disagree for {}",
                            &text[range.clone()]
                        );
                    }
                }
            }
            for bounds in geometry.code_background_bounds(range.clone()) {
                assert!(bounds.left() >= Pixels::ZERO);
                assert!(bounds.right() <= geometry.inline.size.width);
            }
            for (relative, _) in text[range.clone()].char_indices().skip(1) {
                let offset = range.start + relative;
                let caret = geometry
                    .shaped
                    .visual_position_for_byte_index(offset, geometry.line_height)
                    .unwrap();
                let painted = geometry.painted_point(caret);
                assert_eq!(painted.x, caret.x);
                let index = geometry.shaped.closest_byte_index_for_pixel_point(
                    geometry.interaction_point(painted),
                    geometry.line_height,
                );
                assert_eq!(index.unwrap_or_else(|index| index), offset);
            }
        }
    }
}

pub struct SelectableText {
    element_id: ElementId,
    layout: Geometry,
    text: SharedString,
    selection: TextSelection,
    id: TextId,
    separator: &'static str,
    links: Vec<(Range<usize>, String)>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code: Vec<Range<usize>>,
    code_style: Option<(SharedString, Rgba)>,
}

impl SelectableText {
    pub fn new(
        element_id: impl Into<ElementId>,
        id: TextId,
        content: SharedString,
        selection: TextSelection,
        separator: &'static str,
    ) -> Self {
        Self {
            text: content,
            element_id: element_id.into(),
            layout: Geometry::default(),
            selection,
            id,
            separator,
            links: Vec::new(),
            highlights: Vec::new(),
            code: Vec::new(),
            code_style: None,
        }
    }

    pub fn links(mut self, links: Vec<(Range<usize>, String)>) -> Self {
        self.links = links;
        self
    }

    pub fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        self.highlights = highlights;
        self
    }

    pub fn inline_code(
        mut self,
        ranges: Vec<Range<usize>>,
        font: SharedString,
        background: Rgba,
    ) -> Self {
        self.code = ranges;
        self.code_style = Some((font, background));
        self
    }
}

impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.element_id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<gpui::accesskit::Role> {
        Some(gpui::accesskit::Role::Label)
    }

    fn write_a11y_info(&self, node: &mut gpui::accesskit::Node) {
        node.set_value(self.text.to_string());
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        self.selection.state.borrow_mut().fragments.insert(
            self.id,
            Fragment {
                text: self.text.clone(),
                separator: self.separator,
            },
        );
        let base = window.text_style();
        let font_size = base.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            base.line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let font_id = window.text_system().resolve_font(&base.font());
        let metrics = InlineTextMetrics {
            ascent: window.text_system().ascent(font_id, font_size),
            descent: window.text_system().descent(font_id, font_size),
            x_height: window.text_system().x_height(font_id, font_size),
        };
        let mut boundaries = vec![0, self.text.len()];
        for range in self
            .highlights
            .iter()
            .map(|(range, _)| range)
            .chain(self.code.iter())
        {
            boundaries.extend([range.start, range.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let runs: Vec<_> = boundaries
            .windows(2)
            .map(|range| {
                let mut style = base.clone();
                for (highlight_range, highlight) in &self.highlights {
                    if highlight_range.contains(&range[0]) {
                        style = style.highlight(*highlight);
                    }
                }
                if self.code.iter().any(|code| code.contains(&range[0]))
                    && let Some((font, _)) = &self.code_style
                {
                    style.font_family = font.clone();
                }
                style.to_run(range[1] - range[0])
            })
            .collect();
        // Zero-text boxes reserve real padding without adding characters to copied text.
        let boxes: Vec<_> = self
            .code
            .iter()
            .flat_map(|range| [range.start, range.end])
            .enumerate()
            .map(|(id, index)| InlineBoxRequest {
                id: id as u64,
                index,
                size: size(px(4.), line_height + px(2.)),
                vertical_align: VerticalAlign::Middle,
            })
            .collect();
        let text = self.text.clone();
        let run_ranges: Vec<_> = boundaries
            .windows(2)
            .map(|range| range[0]..range[1])
            .collect();
        let geometry = self.layout.clone();
        let layout_id =
            window.request_measured_layout(Style::default(), move |known, available, window, _| {
                let wrap_width =
                    TextLayout::evaluate_wrap_width(&base.white_space, known, available);
                let inline = window.text_system().layout_inline(InlineLayoutRequest {
                    text: &text,
                    runs: &runs,
                    text_styles: &[],
                    boxes: &boxes,
                    font_size,
                    line_height,
                    text_metrics: metrics,
                    options: TextLayoutOptions {
                        wrap_width,
                        text_align: base.text_align,
                        ..Default::default()
                    },
                    bidi_scopes: &[],
                });
                let result = inline.size;
                let spans = TextGeometry::spans(&inline, &run_ranges);
                *geometry.0.borrow_mut() = Some(TextGeometry {
                    shaped: Arc::new(ShapedTextLayout {
                        layout: inline.layout.clone(),
                        wrap_width,
                    }),
                    inline,
                    bounds: Bounds::default(),
                    line_height,
                    spans,
                });
                result
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) -> Hitbox {
        self.layout.0.borrow_mut().as_mut().unwrap().bounds = bounds;
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.selection
            .state
            .borrow_mut()
            .regions
            .insert(self.id, self.layout.clone());
        hitbox
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((_, background)) = self.code_style {
            let geometry = self.layout.0.borrow();
            let geometry = geometry.as_ref().unwrap();
            for range in &self.code {
                for mut region in geometry.code_background_bounds(range.clone()) {
                    region.origin += geometry.bounds.origin;
                    window.paint_quad(
                        PaintQuad::new(region)
                            .background(background)
                            .corner_radii(px(4.)),
                    );
                }
            }
        }
        if self.selection.is_focused(window)
            && let Some(range) = self.selection.state.borrow().range(self.id)
        {
            let color = rgb_to_hsla(cx.theme().colors.accent).opacity(0.3);
            let geometry = self.layout.0.borrow();
            let geometry = geometry.as_ref().unwrap();
            for mut region in geometry.selection_bounds(range) {
                region.origin += geometry.bounds.origin;
                window.paint_quad(PaintQuad::new(region).background(color));
            }
        }
        {
            let geometry = self.layout.0.borrow();
            let geometry = geometry.as_ref().unwrap();
            if let Err(error) = geometry.inline.paint(geometry.bounds.origin, window, cx) {
                tracing::error!("failed to paint conversation text: {error}");
            }
        }
        let index = self.layout.index_for_position(window.mouse_position()).ok();
        let over_link =
            index.is_some_and(|index| self.links.iter().any(|(range, _)| range.contains(&index)));
        window.set_cursor_style(
            if over_link {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            },
            hitbox,
        );

        let selection = self.selection.clone();
        let layout = self.layout.clone();
        let hitbox_down = hitbox.clone();
        let id = self.id;
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox_down.is_hovered(window)
            {
                return;
            }
            let offset = layout
                .index_for_position(event.position)
                .unwrap_or_else(|index| index);
            let mut state = selection.state.borrow_mut();
            let endpoint = Endpoint { id, offset };
            if event.modifiers.shift && state.anchor.is_some() {
                state.head = Some(endpoint);
            } else {
                let range = if event.click_count >= 2 {
                    layout.selection_at(
                        event.position,
                        if event.click_count == 2 {
                            TextSelectionKind::Word
                        } else {
                            TextSelectionKind::HardLine
                        },
                    )
                } else {
                    offset..offset
                };
                state.anchor = Some(Endpoint {
                    id,
                    offset: range.start,
                });
                state.head = Some(Endpoint {
                    id,
                    offset: range.end,
                });
            }
            state.dragging = event.click_count == 1;
            drop(state);
            selection.focus.focus(window, cx);
            window.refresh();
            cx.stop_propagation();
        });

        let selection = self.selection.clone();
        let layout = self.layout.clone();
        let hitbox_up = hitbox.clone();
        let links = self.links.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble
                && event.button == MouseButton::Left
                && hitbox_up.is_hovered(window)
            {
                let state = selection.state.borrow();
                if state.anchor == state.head
                    && state.anchor.is_some_and(|anchor| anchor.id == id)
                    && let Ok(index) = layout.index_for_position(event.position)
                    && let Some((_, url)) = links.iter().find(|(range, _)| range.contains(&index))
                {
                    cx.open_url(url);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_copies_unicode_and_separators_in_both_directions() {
        let mut state = SelectionState::default();
        for (id, text, separator) in [
            ((0, 0, 0), "hello ", "\n\n"),
            ((0, 0, 1), "中文", ""),
            ((0, 0, 2), " world", ""),
            ((0, 0, 3), "fn main() {}\n", "\n\n"),
        ] {
            state.fragments.insert(
                id,
                Fragment {
                    text: text.into(),
                    separator,
                },
            );
        }
        let start = Endpoint {
            id: (0, 0, 0),
            offset: 3,
        };
        let end = Endpoint {
            id: (0, 0, 3),
            offset: 12,
        };
        for (anchor, head) in [(start, end), (end, start)] {
            state.anchor = Some(anchor);
            state.head = Some(head);
            assert_eq!(
                state.selected_text().as_deref(),
                Some("lo 中文 world\n\nfn main() {}")
            );
        }
    }
}
