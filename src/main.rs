use gpui::{
    App, Bounds, Context, KeyBinding, Menu, MenuItem, Window, WindowBounds, WindowOptions,
    actions, div, prelude::*, px, rgb, size,
};

actions!(zp, [Quit]);

struct Counter {
    count: usize,
}

impl Render for Counter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(div().text_2xl().child("Hello, GPUI-CE!"))
            .child(format!("Count: {}", self.count))
            .child(
                div()
                    .id("increment")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .cursor_pointer()
                    .hover_style(|style| style.bg(rgb(0xb4befe)))
                    .child("Click me")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.count += 1;
                        cx.notify();
                    })),
            )
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.activate(true);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![Menu {
            name: "zp".into(),
            items: vec![MenuItem::action("Quit", Quit)],
            disabled: false,
        }]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(600.), px(400.)), cx);
        cx.open_window(
            WindowOptions::new().window_bounds(Some(WindowBounds::Windowed(bounds))),
            |_, cx| cx.new(|_| Counter { count: 0 }),
        )
        .expect("failed to open window");
    });
}
