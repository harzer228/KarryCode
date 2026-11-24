---
name: code-review
description: "Review a project or a diff for bugs, security problems and regressions. Use when the user asks to review code, check a change, audit a file or a pull request, or wants a second opinion before committing. Reports findings with severity and a concrete fix."
---

# Code review for KarryCode

## When to use

Use this skill when the user says «проверь код», «сделай ревью», «что тут может
сломаться», «review this change», or before committing a risky edit. Do not use
it for ordinary explanation of code that the user did not ask to review.

## Workflow

1. Establish the scope: a file, a module, a diff, or the whole project. If the
   user did not name a scope, ask one short question instead of reviewing
   everything.
2. Read the relevant files with `action: read`. Use `action: search` for a
   literal symbol or error text. Respect the 64 KiB per file limit and the 50
   match search limit; read in slices when a file is large.
3. Look for, in this order:
   - correctness: wrong conditions, off-by-one, unchecked errors, races;
   - data and safety: path traversal, command injection, secrets in logs,
     untrusted input reaching a shell or a file path;
   - regressions: changed behavior that existing tests do not cover;
   - resource limits: unbounded reads, allocations, retries, timeouts;
   - readability only when it hides a real defect.
4. Check the tests that cover the changed behavior. If the project has a test
   task, offer `action: run` with `cargo-test` or `npm-test`; the user must
   confirm it.
5. Report findings, not a rewrite.

## Output format

```
Findings: N (blocker / high / medium / low)

[severity] file:line — short title
  Why: one or two sentences with a concrete failure scenario.
  Fix: the specific change to make.
```

Write the report in the user's language. Order findings by severity. If there are no findings, say so and name the
checks you performed. Never invent a line number: quote the code you saw.

## Edits

Do not edit during a review unless the user asks to fix. When they do, propose
exactly one `action: edit` per file with `path`, `reason`, `before`, `after`;
the user reviews every replacement. Do not combine several files into one edit
object.

Use git_status and git_diff to establish scope and read the surrounding lines. Use a reviewed change group if the user asks to apply fixes.
