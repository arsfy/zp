//! A small independently rendered working indicator and active task clock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, Context, IntoElement, Render, Window, div, prelude::*, px,
};

use crate::theme::ActiveTheme;

#[derive(Default)]
struct ActiveClock {
    elapsed: Duration,
    resumed: Option<Instant>,
}

impl ActiveClock {
    fn elapsed(&self, now: Instant) -> Duration {
        self.elapsed
            + self
                .resumed
                .map_or(Duration::ZERO, |at| now.duration_since(at))
    }

    fn pause(&mut self, now: Instant) {
        self.elapsed = self.elapsed(now);
        self.resumed = None;
    }

    fn resume(&mut self, now: Instant) {
        self.resumed.get_or_insert(now);
    }
}

#[derive(Default)]
pub(super) struct RunProgress {
    clock: Arc<Mutex<ActiveClock>>,
    seconds: u64,
    paused: bool,
}

impl RunProgress {
    pub fn start(&mut self, cx: &mut Context<Self>) {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) = ActiveClock::default();
        self.set_paused(false, cx);
    }

    pub fn set_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let mut clock = self.clock.lock().unwrap_or_else(PoisonError::into_inner);
        if paused {
            clock.pause(now);
        } else {
            clock.resume(now);
        }
        self.paused = paused;
        self.seconds = clock.elapsed(now).as_secs();
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let seconds = self.elapsed(cx).as_secs();
        if seconds != self.seconds {
            self.seconds = seconds;
            cx.notify();
        }
    }

    pub fn elapsed(&self, cx: &gpui::App) -> Duration {
        self.clock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed(cx.background_executor().now())
    }

    pub fn elapsed_sampler(&self, cx: &gpui::App) -> impl Fn() -> u64 + use<> {
        let clock = self.clock.clone();
        let executor = cx.background_executor().clone();
        move || {
            clock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .elapsed(executor.now())
                .as_secs()
        }
    }
}

pub(super) fn format_elapsed(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

impl Render for RunProgress {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors.text_muted;
        let elapsed = format_elapsed(self.seconds);
        div()
            .id("run-progress")
            .h(px(38.))
            .w(px(160.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(20.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .children((0..3usize).map(|index| {
                        div().size(px(6.)).rounded_full().bg(color).with_animation(
                            ("loading-dot", index),
                            Animation::new(Duration::from_millis(1200)).repeat_synced(),
                            move |dot, t| {
                                let phase = (t - index as f32 / 3.0).rem_euclid(1.0);
                                let wave = (phase * std::f32::consts::TAU).cos() * 0.5 + 0.5;
                                dot.opacity(0.25 + 0.75 * wave)
                            },
                        )
                    })),
            )
            .child(
                div()
                    .id("run-elapsed")
                    .h(px(18.))
                    .text_size(px(11.))
                    .line_height(px(18.))
                    .text_color(color)
                    .child(if self.paused {
                        format!("{elapsed} (paused)")
                    } else {
                        elapsed
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_blocked_time_and_resumes_without_resetting() {
        let start = Instant::now();
        let mut clock = ActiveClock::default();
        clock.resume(start);
        clock.resume(start + Duration::from_secs(2));
        clock.pause(start + Duration::from_secs(3));
        clock.pause(start + Duration::from_secs(7));
        assert_eq!(
            clock.elapsed(start + Duration::from_secs(60)),
            Duration::from_secs(3)
        );
        clock.resume(start + Duration::from_secs(60));
        assert_eq!(
            clock.elapsed(start + Duration::from_secs(62)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn formats_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(60), "1m 0s");
        assert_eq!(format_elapsed(83), "1m 23s");
        assert_eq!(format_elapsed(125), "2m 5s");
        assert_eq!(format_elapsed(3601), "1h 0m 1s");
    }
}
