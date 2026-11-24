# Contributing to KarryCode CLI

Use the pinned Rust toolchain. Run `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` from the root.

Keep English and Russian interface strings paired. Use synthetic profiles for tests and terminal recordings. Describe the behavior change and include the platform, verification steps, and a screenshot for visible changes. Open an issue when a proposal changes a public command or data format.

KarryCode started from the MIT-licensed CarryCode Agent at commit [e25b490](https://github.com/harzer228/CarryCode/commit/e25b490703a4964e16d8624819da3f5ff5b5d6a2).
