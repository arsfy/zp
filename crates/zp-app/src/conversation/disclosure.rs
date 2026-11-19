//! Animated opening and closing of collapsible blocks (thinking, tool cards, summaries,
//! the task list): their content slides open or shut and fades, and the disclosure
//! chevron turns, instead of the block jumping to its new height.
//!
//! A block's open state is derived anew on every draw (the user's choice, settings, a
//! running tool...), so a change between two draws starts a transition, whatever caused
//! it. A block seen for the first time is drawn in its state without one.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, IntoElement, Pixels, SharedString, div, prelude::*, px};

/// Time to open or close a block fully; a reversed transition takes the share left.
pub(crate) const DURATION: Duration = Duration::from_millis(180);

/// The open state of the conversation's collapsible blocks, by key.
#[derive(Default)]
pub(crate) struct Disclosures {
    blocks: RefCell<HashMap<SharedString, Block>>,
    /// A transition was drawn mid-way since the view last asked: draw another frame.
    animating: Cell<bool>,
}

struct Block {
    open: bool,
    /// How far open the block was (0 to 1) when its transition started, and when.
    from: f32,
    since: Option<Instant>,
    /// Natural height of the content, measured each time it is drawn.
    height: Rc<Cell<Pixels>>,
}

impl Block {
    /// How far open the block is at `now` (0 to 1, linear in time); ends a finished
    /// transition.
    fn position(&mut self, now: Instant) -> f32 {
        let target = if self.open { 1. } else { 0. };
        let Some(since) = self.since else {
            return target;
        };
        let total = DURATION.mul_f32((target - self.from).abs());
        let elapsed = now.saturating_duration_since(since);
        if total.is_zero() || elapsed >= total {
            self.since = None;
            return target;
        }
        let t = elapsed.as_secs_f32() / total.as_secs_f32();
        self.from + (target - self.from) * t
    }
}

/// How far a block is open as drawn now.
#[derive(Clone)]
pub(crate) struct Openness {
    key: SharedString,
    /// 0 (closed) to 1 (open), eased.
    pub(crate) value: f32,
    /// Whether a transition is under way.
    pub(crate) moving: bool,
    height: Rc<Cell<Pixels>>,
}

impl Disclosures {
    /// The openness of block `key`, which should now be `open`. A change since the last
    /// call starts a transition from where the block is (unless the app reduces motion).
    pub(crate) fn openness(&self, key: impl Into<SharedString>, open: bool, cx: &App) -> Openness {
        let now = cx.background_executor().now();
        let key = key.into();
        let mut blocks = self.blocks.borrow_mut();
        let block = blocks.entry(key.clone()).or_insert_with(|| Block {
            open,
            from: if open { 1. } else { 0. },
            since: None,
            height: Rc::default(),
        });
        if block.open != open {
            let from = block.position(now);
            block.open = open;
            block.from = from;
            block.since = (!cx.reduce_motion()).then_some(now);
        }
        let position = block.position(now);
        let moving = block.since.is_some();
        if moving {
            self.animating.set(true);
        }
        Openness {
            key,
            value: smoothstep(position),
            moving,
            height: block.height.clone(),
        }
    }

    /// Whether a transition was drawn since the last call: the caller requests a frame.
    pub(crate) fn take_animating(&self) -> bool {
        self.animating.replace(false)
    }

    /// Forgets every block (e.g. when the transcript is rebuilt and rows change meaning).
    pub(crate) fn clear(&self) {
        self.blocks.borrow_mut().clear();
    }
}

/// Eases both ways alike, so a transition reversed mid-way continues smoothly.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// The collapsible content of a block, as far open as `openness`: nothing when closed,
/// the content as is when open, else clipped to that share of its height and faded.
/// `content` is built only when something is shown; `None` shows nothing.
pub(crate) fn collapsible(
    openness: &Openness,
    content: impl FnOnce() -> Option<AnyElement>,
) -> Option<AnyElement> {
    if openness.value <= 0. && !openness.moving {
        return None;
    }
    let content = content()?;
    let height = openness.height.clone();
    let measured = openness.height.clone();
    let key = openness.key.clone();
    let wrapper = div()
        .debug_selector(move || format!("disclosure-{key}"))
        .flex()
        .flex_col()
        .on_children_prepainted(move |bounds, _, _| {
            if let Some(bounds) = bounds.first() {
                measured.set(bounds.size.height);
            }
        })
        .child(div().flex_shrink_0().child(content));
    Some(if openness.moving {
        wrapper
            .overflow_hidden()
            .h(px(f32::from(height.get()) * openness.value))
            .opacity(openness.value)
            .into_any_element()
    } else {
        wrapper.into_any_element()
    })
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn changes_animate_and_reversals_continue_from_the_current_position(cx: &mut TestAppContext) {
        let disclosures = Disclosures::default();
        let value = |open, cx: &mut TestAppContext| {
            cx.update(|cx| disclosures.openness("block", open, cx).value)
        };
        let near = |actual: f32, expected: f32| {
            assert!((actual - expected).abs() < 1e-3, "{actual} != {expected}");
        };
        assert_eq!(value(false, cx), 0., "first seen: drawn as it is");
        assert!(!disclosures.take_animating());

        assert_eq!(value(true, cx), 0., "a change starts where the block is");
        assert!(disclosures.take_animating());
        cx.executor().advance_clock(DURATION / 2);
        near(value(true, cx), 0.5);

        // Closing half-way takes half the time.
        near(value(false, cx), 0.5);
        cx.executor().advance_clock(DURATION / 4);
        near(value(false, cx), smoothstep(0.25));
        cx.executor().advance_clock(DURATION / 4);
        assert_eq!(value(false, cx), 0.);
        assert!(disclosures.take_animating());
        assert_eq!(value(false, cx), 0.);
        assert!(!disclosures.take_animating(), "settled");
    }

    #[gpui::test]
    fn reduced_motion_switches_at_once(cx: &mut TestAppContext) {
        let disclosures = Disclosures::default();
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            disclosures.openness("block", false, cx);
            let openness = disclosures.openness("block", true, cx);
            assert_eq!(openness.value, 1.);
            assert!(!openness.moving);
        });
        assert!(!disclosures.take_animating());
    }
}
