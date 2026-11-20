## v0.1.0 20 Nov 2026

#### Subagents

- The agent can hand read-only research to subagents that work in parallel and report back a conclusion. Turn them on in **Settings → Subagents** (off by default). A subagent can only read the project (`read`, `grep`, `find`, `ls`), optionally limited to the paths the agent names, and sends what it reads to its model.
- **Follow main model** uses the model and thinking level of the current run. **Task profiles** let the agent choose among profiles you set up, each with a purpose, a model (or the main model) and a thinking level; one is the default.
- Tasks start on their own by default. Choose **Ask each time** to confirm each one; the approval card shows the model and its endpoint, what the subagent may read and how much context it gets. A trusted project can turn subagents off, require asking, limit the profiles and tighten the limits in the `subagents` section of `.zp/settings.json`.
- Limits: 4 parallel tasks and 4 tasks per run by default. Model requests per task, a timeout and output tokens per request can be set and have no limit by default; time spent waiting for a free slot does not count toward the timeout.
- A subagent's card stays open while it works, whatever the tool display settings, and shows its latest tool calls and the end of what it is thinking or writing, with a **Cancel** button. When done it shows the time, model requests, tool calls, tokens and cost, the files and searches it used, and its conclusion as Markdown (long conclusions start shortened). A conclusion cut off at the output limit is marked as such; a cancelled task is not shown as a failure.
- Subagent tokens and cost count toward the conversation's totals once a task finishes; they do not count toward its context size.
- `zp-cli run` offers the subagent tool when subagents are enabled, and warns when `--tools subagent` names it while it is unavailable.

#### Permissions and tools

- When **Auto-review** hands a call to you, the approval card explains in plain words what it would do (what it runs, which files or services it touches), next to the review model's reason. Both are written in the language of your request. `zp-cli` prints the explanation too.
- A call the review model is deciding on shows **reviewing…** instead of **running…**; it only runs once allowed.
- The tool calls of one response are checked and run independently: a call waiting for review or approval no longer holds back the others, which run as soon as they are allowed. You are still asked about one call at a time, and approving a command for the session also covers the waiting calls it applies to.
- A command that leaves background processes holding its output returns after 100 ms instead of 250 ms.

#### Context and prompt caching

- Automatic compaction runs inside the agent's run, before each model request, so a long run of tool calls is compacted between steps instead of overflowing. While it summarizes, the composer shows **Compacting the conversation…**.
- When a request does not fit (an overflow error, or a response stopped at the length limit with nothing written and the window full), ZP compacts once and retries; the failed attempt is dropped from the conversation. Rate-limit errors that mention tokens are no longer mistaken for an overflow.
- Summary requests are sized to fit the context window: their output gets a share of the reserve, and the oldest history is cut when even that does not fit. Summaries, reviews and titles do not write to the prompt cache.
- The context size includes the system prompt and tool definitions, and usage measured before a compaction or with another model no longer counts, so the context percentage is right after compacting or switching models.
- **Settings → General → Prompt cache retention**: **Default**, **Extended** (1 hour on Anthropic, 24 hours on earlier OpenAI models, 12 hours on GPT-5.6 and later; cache writes cost more) or **Off** (no cache writes, so every request pays the full input price). `zp-cli` uses the same setting. Custom providers can set `promptCacheKeyMode` and `supportsLongCacheRetention` in `settings.json`.
- The conversation's totals include summary requests and abandoned attempts. The cache percentage in the status bar is that of the latest request; `zp-cli` prints cache reads and writes separately.

#### Conversation and sessions

- Typing `@` completes files at the caret, also in the middle of a message. `@` alone lists the project's top folder; picking a folder inserts it and lists what it contains. Paths with spaces are quoted (`@"docs/my notes.md"`) and attached when sent.
- Thinking blocks are expanded by default. A display you chose in **Settings → Appearance** is kept.
- Hiding a session from the sidebar also closes its tab. If its agent is still running, ZP asks first; keeping it running leaves the session listed.
- In the model picker, **↑** / **↓** move through the list (wrapping around) and **Enter** picks the highlighted model. The current model is highlighted when the picker opens; typing a filter moves the highlight to the first match.
- Models of a gateway or custom provider whose model list carries no prices use the same model's list price (from models.dev or a built-in catalog), so the status bar cost is no longer stuck at $0.0000. Requests already recorded keep their cost of 0.

## v0.1.0-rc.4 19 Nov 2026 `34800d6`

#### Conversation

- Thinking blocks, tool cards, the context summary and the tasks card slide open and shut instead of jumping to their new height, and their chevrons turn as they do. This includes blocks that open while running and close when done. The model, thinking and permission menus and the session tree fade in when opened.
- Messages show when they were sent, and responses when they finished and how long the run took (time spent waiting for approval is not counted). Times from another day include the date. For sessions saved by earlier versions the duration is estimated from the time of the user's message.
- The changes summary under an answer lists the changed files with their added and removed lines: the first three, with **Show N more files** for the rest. Clicking a file opens the turn's diff at that file.
- Tabs in command output, code blocks and messages are drawn as spaces up to the next tab stop instead of as boxes, so tab-separated output lines up in columns. Copying a code block keeps the tabs.

#### Projects

- **Open Folder** in a project window switches that window to the new folder instead of opening another one; a folder that is already open is brought forward instead. If agents are running or terminals are open, ZP asks first, since switching stops them.

#### Permissions

- In **Auto-approve edits** and **Auto-review**, `rm` of files inside the project runs without asking, like other file changes. This applies only when every path is a literal inside the project (no globs or variables, symlinks resolved), and never for the project folder itself. Removals outside the project still need approval. On Windows this requires the bash shell.

#### Performance

- The Git history draws only the commits in view, and the Git view is no longer redrawn while other parts of the window animate. Long histories had made animations elsewhere stutter.

## v0.1.0-rc.3 19 Nov 2026 `7882ba0`

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
