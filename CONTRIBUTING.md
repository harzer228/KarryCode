# Contributing to KarryCode CLI

Use the pinned Rust toolchain. Run `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` from the root.

Run `cargo build --locked` to build the current checkout. To smoke-test a local change, run `cargo run --locked -- --help` and check the relevant feature from a temporary project.

For `--print` changes, test with a configured synthetic profile and a loopback provider. Capture stdout, stderr, and the exit code separately. For compaction and tool changes, verify the request format and check that retained tool calls still match their results.

Keep English and Russian interface strings paired. Use synthetic profiles for tests and terminal recordings. Describe the behavior change and include the platform, verification steps, and a screenshot for visible changes. Open an issue when a proposal changes a public command or data format.

Update both READMEs and the Unreleased changelog when a change affects how people run or configure KarryCode.

KarryCode started from the MIT-licensed CarryCode Agent at commit [e25b490](https://github.com/harzer228/CarryCode/commit/e25b490703a4964e16d8624819da3f5ff5b5d6a2).
