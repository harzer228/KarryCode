# Security

KarryCode runs as a regular user. Review action prompts before sharing desktop content or changing files. On Windows, DPAPI ties the local profile key to your account. Keep the encrypted profile and the key needed to unlock it together when backing up a Windows installation.

`--print` uses Auto approval rules. Actions that need a prompt are declined, while sandboxed commands and reversible project edits may run under the usual rules. Configured user MCP servers can start during initialization. Project MCP entries still require explicit approval.

Context compaction sends older conversation excerpts and shortened tool results to the active model provider. A saved chat stores the resulting summary and retained messages in its encrypted profile.

Project model files can select an existing connection and set model limits after review. The profile model file stores endpoints, model IDs and limits. Keys stay encrypted and are scoped to the connection ID, URL and protocol.

Creator tests executable drafts after reviewing their code and declared file access. An approval records the exact package files and their project or profile scope. External scripts need platform isolation and get a staged copy of the declared files. Rhai receives JSON and declared file contents through its worker. Tool output can propose changes through the existing review and conflict checks. [Extension format and runtime requirements](docs/extensions.md)

Report sensitive issues through GitHub private vulnerability reporting when enabled. Include the version, operating system and a synthetic reproduction. Remove API keys, profile snapshots and private paths from logs and screenshots.
