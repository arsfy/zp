# gpui_ce_elements (patched)

A copy of `crates/gpui_elements` from [gpui-ce](https://github.com/gpui-ce/gpui-ce) at rev
`f388375d03e63d75811fa1cca3a563203b920d98` (Apache-2.0, see `LICENSE.md`). The workspace
uses it in place of the git crate through `[patch]`.

## Local change

`EditableTextState::bounds_for_range` (`src/editable_text/state.rs`) returned `None` while the
text layout was older than the text. That happens right after an IME inserts marked text and
asks where the composition is, before the next frame lays the text out again. macOS then puts
the candidate window at the bottom-left corner of the screen.

The patched version falls back to the previous layout: the composition starts where the
caret already was, so positions before the edit are still right.

When upgrading gpui-ce, check whether upstream fixed this; if so, delete this directory and
the `[patch]` entry.
