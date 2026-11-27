# Create a KarryCode extension

Open `/creator` in an Agent chat and ask for a package. Drafts live under `.karrycode/extensions/<id>/`. `extension_author` accepts `guide`, `validate`, `test` and `install`. KarryCode shows the source and requested file access before executable tests, then shows the diff, test results and installation scope before installation. `/extensions` lists installed revisions, drafts, and verification errors.

A complete translation-check example uses these two files.

```toml
# .karrycode/extensions/translations/extension.toml
format = 1
id = "translations"
version = "1.0.0"

[[tools]]
name = "check"
description = "Find which locales are missing the requested key"
engine = "rhai"
entry = "check.rhai"
schema = '{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}'
reads = ["locales/en.json", "locales/ru.json"]
timeout = 5
headless = true

[[tools.tests]]
input = '{"key":"sign_in"}'
expected = '{"text":"Missing: locales/ru.json\n"}'
[tools.tests.files]
"locales/en.json" = '{"sign_in":"Sign in"}'
"locales/ru.json" = '{"quit":"Выход"}'
```

```rust
// .karrycode/extensions/translations/check.rhai
let missing = "";
for path in files.keys() {
    let locale = parse_json(files[path]);
    if !locale.contains(arguments.key) {
        missing += path + "\n";
    }
}
#{ text: if missing == "" { "All declared locales contain the key" } else { "Missing: " + missing } }
```

The tool is registered as `ext_translations_check`. `reads` lists individual project text files. `writes` lists individual paths that a tool may propose to create or replace. The output is `{"text":"summary","changes":[{"path":"notes.txt","content":"new contents"}]}`. KarryCode applies proposed writes through the ordinary preview, permission and conflict checks. `--print` can use declared headless tools with an empty write list. Tests use synthetic files from the manifest.

Use `python`, `powershell` or `bash` as `engine` for scripts that accept `{"arguments":{},"files":{}}` as UTF-8 JSON on stdin and return JSON on stdout. Use stderr for diagnostics. Windows requires AppContainer and a compatible installed interpreter. Linux requires Bubblewrap and an interpreter available under the system paths mounted in the sandbox. PowerShell uses `pwsh`. Scripts receive only listed files, without profile credentials. Rhai has no project filesystem, network or host API and runs in a short-lived worker.

On Windows, Bash requires a native Bash executable on `PATH`. Python can stage its executable, DLLs and standard library for AppContainer. Extension scripts should use the standard library. Creator reports an interpreter or isolation failure during the test.

## Skills and appearance

Add `skills/<name>/SKILL.md` and optional companion files to the package. The installed skill appears as `<package>/<name>` in `/skills`. Mention the registered tool name and a practical example in the skill body.

Set `appearance = "console.toml"` above any `[[tools]]` in the manifest. The appearance file can contain the following.

```toml
name = "Project verification"
locale = "locale.toml"  # Optional. See /theme → Interface strings for IDs.

[palette]
brand = "#A7C7BE"
border = "#547770"

[layout]
header = "compact"  # full, compact, hidden
border = "square"   # rounded, square, ascii
compact_status = true
compact_spacing = true
input_lines = 4
max_width = 116
input_inset = 2
logo = ["  ██  ", " ████ "]

[[panels]]
id = "verify"
title = "Project verification"
[[panels.tabs]]
title = "Checks"
[[panels.tabs.widgets]]
kind = "plan"
[[panels.tabs.widgets]]
kind = "last_check"
[[panels.tabs.widgets]]
kind = "button"
label = "Check sign-in translation"
tool = "ext_translations_check"
arguments = { key = "sign_in" }
```

Available widgets are `text`, `list`, `table`, `plan`, `last_check`, `progress` and `button`. A button can also open a supported built-in command such as `/tasks`. `/panels translations/verify` opens the example. `/theme` previews an installed appearance before applying it.

Locale files have a `name` and a `[messages]` table keyed by the ID shown in `/theme`. Keep format placeholders and their order. Missing entries use English. Start with `karrycode --safe-ui` to recover the built-in layout and language.
