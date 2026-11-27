# Changelog

## 1.3-beta

- Retry failed interactive startup in a temporary recovery chat. Use `--safe` manually to skip project config, extensions, MCP and tools.
- Keep diagnostics available in a plain console when the profile or terminal cannot open.
- Split the native agent into focused modules for commands, tool actions, files and provider requests.

- Keep a structured work plan through `/compact` and `/sessions`, review it through `/code`, and follow progress in `/tasks`.
- Add profile and project TOML model configuration in `/config` with encrypted, endpoint-bound credentials and per-chat selection.
- Add `read_many`, conflict-checked `apply_patch` and paged `read_tool_result`.
- Create and test local tools and skills with `/creator`. Approve exact revisions, then manage or restore them through `/extensions`.
- Add appearance packs, interface dictionaries, data-only panels and the `--safe-ui` recovery option.
- Run one task with `-p` or `--print`, including tasks read from stdin. The reply goes to stdout, progress goes to stderr, and the chat remains temporary.
- Compact older messages with `/compact` or automatically near the model's input limit. Keep recent messages and complete tool rounds.
- Give each model action its own named tool and argument schema. Continue accepting replies written for the legacy `project` tool.
- Open an explicit project folder even if the terminal's previous working directory was removed.
- Restore blocking mode on accepted preview sockets so Windows reads honor the configured timeouts.

## 1.2-beta

- Refresh the chat header, startup notice and command suggestions immediately when switching languages.
- Set a per-connection parallel request limit in `/setting`. The effective limit also accounts for the subagent setting and any cap observed from the provider.
- Configure a provider and default model with the first-launch wizard. The wizard supports keyless local endpoints and manual model setup.
- Build and verify Windows MSVC, Linux GNU and Linux musl assets automatically on release tags, with per-file SHA-256 checksums.
- Update the application icon, terminal mark and repository artwork.
- Document Windows memory and CPU usage with a reproducible comparison against Aider, Claude Code and OpenCode.

## 1.1-beta

- Continue long tool runs past the previous 24-step limit and clear the resume hint when work starts.
- Approve input on the built-in local preview in Auto mode. Search project text with regular expressions without launching a shell.
- Open chat links with a single click. Drag over the text to select and copy.

## 0.1.0-beta.1

- Standalone Rust agent with the `karrycode` command and one-command installers for Windows and Linux.
- Separate local encrypted profile with Windows DPAPI and Linux Secret Service or password unlock.
- Provider and model selection, workspace switching within a conversation, tools and Windows desktop control.
