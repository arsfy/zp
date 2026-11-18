## v0.1.0-rc.3 19 Nov 2026

#### Conversation

- Code blocks in Markdown are syntax-highlighted, with a light or dark theme to match the app. Long blocks are highlighted in the background and very large ones stay plain.
- Tool cards and thinking blocks that open only while in progress stay open for at least 1.5 seconds, so quick commands and short thoughts no longer flash open and shut.
- Thinking blocks no longer show leading or trailing blank lines, and an expanded block with no text shows only its header.
- The transcript keeps its scroll position when rows are remeasured while a response is streaming.
- Long API errors (compact JSON, request IDs) wrap inside the response card instead of pushing the **Retry** button out of view.
- The tasks card stays above the composer after all tasks are done, until you close it with its × button.

#### Sessions

- A new session appears in the sidebar as soon as its first message is saved, instead of after the run finishes.
- Deleting a session asks first: **Hide Session** removes it from this project's sidebar and keeps the file, while **Move to Trash** also moves the session file to Trash, which can reduce local usage totals. A busy session cannot be moved to Trash.
- The sidebar says "No sessions yet" when a project has none, and "No matching sessions" only while searching.

#### Packaging and updates

- Linux builds are also published for ARM64 (`aarch64` tarball and AppImage, `arm64` Debian package).
- Release notes on GitHub end with a Downloads section linking each package.
- Clicking the update button in the title bar opens the update dialog reliably; previously the click could do nothing.

## v0.1.0-rc.2 18 Nov 2026 `0d59eaa`

#### Providers and models

- Gateways that resell other vendors' models (such as Command Code) now support thinking levels: models their list does not describe borrow capabilities, thinking levels and limits from the same model in the built-in catalogs or on models.dev.
- Models are sent to the protocol they are served on when the provider's model list says so, e.g. Claude models of a gateway go to `/messages` instead of failing on `/chat/completions`.
- Command Code: no `developer` role (rejected by Qwen models), `max_tokens` instead of `max_completion_tokens`, and `minimal` reasoning is sent as `low`.
- Provider errors show the actual cause: when a proxy only says something like `openai_error` or "Provider returned error", the response body (or OpenRouter's upstream error) is shown with it. A JSON error answered to a streaming request is reported instead of "stream ended without finish_reason".
- The context size no longer reads as nearly empty when a provider reports zero prompt tokens, so automatic compaction still triggers.
- The custom provider form has a **Test** button that lists the endpoint's models with the entered settings, without saving.

#### Permissions

- The auto-review model has its own thinking level (Settings → Permissions, or `permissions.reviewThinking`), off by default.

#### Interface

- Tool call headers line up the tool name, arguments and status on one baseline; this was most visible on Windows.
- Disclosure arrows, picker checkmarks and dropdown arrows are drawn as icons instead of text symbols that looked different on each platform.
- Settings no longer announce every successful change at the top of the page; only errors and shortcut conflicts are shown. Copying the version info shows a check on the button instead.
- The custom provider dialog keeps its buttons in view and scrolls as one area.

#### Packaging and updates

- Fixed the Windows installer build (Inno Setup received a split argument), and the installer icon is now committed instead of generated during the build.
- Linux archives include both the MIT and Apache-2.0 licenses.
- macOS releases can be built without Apple notarization. Such ad-hoc signed copies only accept updates that are intact ZP bundles, and the update prompt explains that macOS will ask again for keychain and folder access. The README explains how to open them.
- `zp-cli models` prints context sizes such as `1.05M` instead of `1.048576M`.

## v0.1.0-rc.1 18 Nov 2026 `cdd2a57`

- First version
