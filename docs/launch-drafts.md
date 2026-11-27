# KarryCode announcement drafts

## Reddit — r/rust

Use the weekly “What’s Everyone Working on this Week?” thread for an introduction. For a standalone post, first have a Rust-specific technical result or question to discuss. [r/rust moderation guidance](https://www.reddit.com/r/rust/comments/1wkmzun/no_more_code_dumps/)

**Weekly comment draft**

> I’m building KarryCode, a Rust terminal coding agent that connects to a model endpoint you choose. The piece I’d like feedback on is streaming model output into the terminal while keeping keyboard input and tool approvals responsive. The same client can talk to a hosted API or a local OpenAI-compatible server, which makes it possible to exercise the UI against a deterministic test server. If you’ve built a streaming Rust TUI, what do you use to test redraw latency and cancellation? [Source and demo](https://github.com/harzer228/KarryCode)

For performance figures, use the Windows benchmark and name the tested build, OS, CPU and scenario. If you discuss `--print` or context compaction, identify the development build. [Windows benchmark](../benchmarks/windows.md)

## Reddit — r/LocalLLaMA

Use this as a brief for a post in your own words after running the setup with a real local model. The moderators discourage posting generated copy. [r/LocalLLaMA rule update](https://www.reddit.com/r/LocalLLaMA/comments/1su3ao4/rlocalllama_rule_updates/)

- **Title ingredients** — your local server, model, and a concrete task you finished with KarryCode.
- **Reproduction** — hardware and OS, model and quantization, server version, endpoint such as `http://127.0.0.1:11434/v1`, exact prompt and project.
- **Observations** — what the model did, what needed manual intervention, and how the tools behaved. Include timing or hardware figures only when you measured them.
- **Discussion** — ask which server or model would best test the remaining compatibility gap. Link the repo and visible demo after the observations.

## Hacker News — Show HN

Use this structure to write your own title and text. HN asks authors to write submissions and comments themselves. [HN guidelines](https://news.ycombinator.com/newsguidelines.html) · [Show HN](https://news.ycombinator.com/showhn.html)

- **Title pattern** — Show HN: KarryCode – [describe what someone can try in one line].
- **Link** — the repo with working install instructions, a release, and an immediate browser demo.
- **First comment** — explain why you built it, show one trade-off in the Rust agent loop or provider integration, and describe how to try it with an existing local model endpoint.
- **Close** — name one limitation and one technical question you want readers to help investigate.

## Хабр — техническая статья

**Рабочий заголовок** — «Как устроен KarryCode: агент на Rust, свой API и предпросмотр из терминала».

1. **Сценарий.** Открыть чужой проект, переключить папку командой `/cd`, дать модели небольшое задание и проверить результат.
2. **Архитектура.** Показать путь от TUI или `--print` до API, аргументы отдельного инструмента и проверку разрешений. Привести пример сжатия длинного диалога и указать, какие данные уходят провайдеру.
3. **Локальная модель.** Поднять Ollama отдельно, выбрать совместимый endpoint, указать ID модели. Приложить точные версии, запрос и фактический результат.
4. **Данные и разрешения.** Показать, что хранится в зашифрованном профиле, что отправляется модели и когда запрашивается доступ к рабочему столу Windows.
5. **Производительность.** Измерить размер бинарника и время до первого интерактивного кадра. Указать команду замеров, ОС, железо, число запусков, медиану и p95. Отдельно привести задержку ответа модели и параметры сервера.
6. **Ограничения.** Указать поддерживаемые ОС, зависимость браузерных инструментов от Chromium, область работы статического предпросмотра и известные несовместимости провайдеров.
7. **Воспроизведение.** Ссылки на релиз, контрольные суммы, тестовый проект и записанный прогон.
