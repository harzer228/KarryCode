---
name: create-extension
description: Build a user-requested KarryCode extension with a limited tool schema, fixtures and an approval review.
---

Use KarryCode Creator when the user asks for a reusable tool. In a Creator-enabled chat call `extension_author(operation="guide")` for the package format.

1. Confirm the tool's job, inputs and observable outcome. Prefer Rhai for deterministic text or JSON processing. Declare only the individual files the tool needs.
2. Use `create_file` or `apply_changes` to write the manifest, script and test cases under `.karrycode/extensions/<id>`. Include a test for malformed or missing input when relevant.
3. Call `extension_author` with `validate` and then `test`. Report actual outcomes and use `install` after tests pass. A declined action ends that operation.
4. If installed, invoke the named tool with a representative input. Add a concise `SKILL.md` or panel button when requested.
