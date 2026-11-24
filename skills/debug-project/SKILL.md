---
name: debug-project
description: "Diagnose a failing build, test, command or runtime behavior in the current project. Use when the user pastes an error, a stack trace, a failing test name, or says the project does not start, builds or behaves incorrectly. Finds the cause from files and evidence before proposing a fix."
---

# Debug a KarryCode project

## Rules

- Evidence first. A hypothesis is not a cause until a file, a log or a command
  result supports it.
- In «diagnose» mode you can read, list and search, but you cannot edit or run
  commands. Ask the user to switch to `/code` for a fix.
- Never claim that a command ran or a test passed without its output.

## Workflow

1. Restate the symptom in one sentence: what was expected, what happened, and
   how to reproduce it.
2. Collect evidence with the narrowest tool:
   - `action: read` for the file named in the traceback;
   - `action: search` for the exact error string, test name or symbol;
   - `action: list` to confirm the project layout and the entry point.
3. Build two or three competing explanations and look for the observation that
   separates them. Prefer the explanation that also predicts a second symptom.
4. Name the root cause and the smallest safe fix. If the fix touches more than
   one file, split it into separate `action: edit` proposals and explain the
   order.
5. Propose verification: `cargo-check`, `cargo-test`, `npm-test`, `npm-build`
   or `python-test` through `action: run`. The user confirms the command.
6. If the evidence is insufficient, say exactly which file, log or command
   output is missing. Do not guess.

## Output format

```
Симптом: ...
Причина: ... (file:line или вывод команды)
Почему это причина: ...
Исправление: ...
Проверка: ...
```

If nothing is found, list what was checked and the next concrete probe.

Use inspect_project to identify the relevant package. For a running web app, inspect the hidden browser snapshot and console before claiming a cause.
