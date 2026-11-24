---
name: skill-writer
description: "Create or rewrite a KarryCode skill in the SKILL.md format, including a ready-made ChatGPT prompt for drafting one. Use when the user wants a new skill, asks how to teach the agent a workflow, or wants a skill turned into a reusable instruction file."
---

# Writing a KarryCode skill

A skill is a folder with `SKILL.md`. KarryCode reads the `name` and
`description` from the front matter and shows them in `/skills`; the model loads
the body on demand with `action: skill` and the `name` field. Keep the body as a
procedure, not as documentation.

## Where skills live

| Location | Scope |
| --- | --- |
| `<project>/.karrycode/skills/<name>/SKILL.md` | This project |
| `<USB bundle>/skills/<name>/SKILL.md` | Travels with the KarryCode bundle |
| `%USERPROFILE%\.karrycode\skills\<name>\SKILL.md` | All of the user's projects |
| `%USERPROFILE%\.agents\skills`, `.claude\skills`, `.codex\skills` | Shared skills available as fallbacks |

The first folder that provides a name wins, so a project skill can override a
built-in one. Built-ins (`code-review`, `debug-project`, `skill-writer`) are
compiled into the binary and always available.

## Front matter

```markdown
---
name: my-skill
description: "One or two sentences: when to use this skill and what it produces. Start with a verb; include the phrases a user would say."
---
```

Rules for `description`:

- 1–2 sentences, under 400 characters;
- says **when** to use it, not what the skill contains;
- includes two or three trigger phrases in the user's language;
- no secrets and no file paths that only exist on one machine.

## Body

1. `## When to use` — triggers and non-goals.
2. `## Workflow` — numbered steps, each naming a concrete KarryCode action.
3. `## Output format` — the exact shape of the answer.
4. Limits and safety rules: what the skill must never do.

KarryCode actions available to a skill: `read`, `list`, `search`, `edit`,
`run` (confirmed), `plan`, `skill`, and `mcp`. A skill must not promise
anything outside that list. `run` tasks are limited to `cargo-test`,
`cargo-check`, `npm-test`, `npm-build`, `python-test`.

Keep the body under 6 KiB. Long checklists make the model skim.

## Ready ChatGPT prompt

Copy the block below into ChatGPT and replace the text in `[...]`. It returns a
complete `SKILL.md`; save it to one of the folders above and run `/skills` in
KarryCode.

```
Help me write a reusable KarryCode skill as a SKILL.md file.

Task: [what the skill should do and when to use it]
Project: [language, framework, tests and constraints]
Output: [for example, findings with severity and file locations]
Restrictions: [for example, require permission before edits]

Return one SKILL.md in the user's language, with YAML front matter containing
a kebab-case name and a short description. Include usage, workflow, output
format and limitations. Name only tools that KarryCode provides, respect
confirmation rules, and keep the body under 6 KiB. Ask at most two questions
if needed. Omit secrets and machine-specific paths.
```

## Check before saving

- `name` is unique and in kebab-case;
- the description contains the words a user would actually say;
- every step names an action that exists;
- the output format is concrete enough to grade;
- the file is valid UTF-8 without a BOM.

Then open `/skills` in KarryCode: the skill must appear with its description and
source. Ask the agent to use it and confirm that it loads the body instead of
answering from memory.

When asked to save the result, use create_file or apply_changes and show the preview. Reference companion files within the skill folder.
