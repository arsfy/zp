## v0.1.0-rc.2 18 Nov 2026

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

## v0.1.0-rc.1 18 Nov 2026

- First version
