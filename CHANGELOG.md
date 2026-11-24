# Changelog

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
