# Windows CLI resource benchmark

Measured on 24 November 2026. The machine ran Windows 11 22H2, build 22621.5409, on an Intel Core i3-10105F with 4 cores, 8 logical CPUs and 15.9 GiB of usable RAM.

## Setup

- The tested KarryCode build includes the 1.2-beta changes and reports 1.1-beta because it was compiled before the version bump. It was built with Rust 1.100.0 and the release profile. The executable is 7.28 MiB. Its SHA-256 is `29f3dbe3977aed16e703cfdcaf9caf17be232e4734c7b8d1f228adfe94ed137e`.
- The comparison used Claude Code 2.1.328, Aider 0.86.2 and OpenCode 2.2.5. OpenCode ran in standalone mode with its private server. Each CLI opened an isolated profile and an empty project in the same 120×34 Windows pseudo-terminal. KarryCode used its usual encrypted history.
- Each CLI received the same plain-text prompt. A loopback API sent the same 6,418-byte answer in 101 chunks, one per nominal 50 ms, through the API protocol that each client supports. Auxiliary requests such as session titles received a short response. The loopback server provided the response text without model inference.
- The CLIs ran sequentially with three valid runs each. Each run used a fresh project folder. The harness sampled the CLI and its descendants with a 100 ms pause between readings. It measured roughly two seconds of idle time after the prompt appeared, the response stream, and the full turn through two seconds after the final line became visible. Browser, shell and project build tools were inactive.

## Reading the measurements

- **Idle RAM** is the median resident working set over the idle sample window. **Streaming RAM** is the peak between the start of the fixture stream and its final text chunk. **Full-turn peak** includes the period immediately after the answer, when a CLI can save its session.
- The sampler adds working sets across the CLI's process tree. Shared pages can be counted in multiple processes. It measures the CLI tree separately from the pseudo-terminal host and test server. [Windows working set](https://learn.microsoft.com/en-us/windows/win32/procthread/process-working-set)
- **Streaming CPU** is the change in combined user and kernel CPU time in the streaming sample window, divided by elapsed wall time and the number of logical CPUs. On this machine, 100% denotes all 8 logical CPUs. A zero means the process timer did not register an increment in that sample window.
- **Time to prompt** is measured from spawning the process until the interactive prompt is visible. The profiles were already configured.

KarryCode's full-turn memory peak occurs when it seals the conversation snapshot with age and scrypt work factor 18. [Snapshot encryption in 1.2-beta](https://github.com/harzer228/KarryCode/blob/v1.2-beta/src/profile.rs#L356)

## Individual runs

| CLI | Run | Time to prompt, s | Idle, MiB | Streaming peak, MiB | Full-turn peak, MiB | After turn, MiB | Streaming CPU |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| KarryCode 1.1-beta | 1 | 1.190 | 12.25 | 14.13 | 270.17 | 14.15 | 0.150% |
| KarryCode 1.1-beta | 2 | 1.422 | 12.29 | 14.12 | 270.21 | 14.30 | 0.075% |
| KarryCode 1.1-beta | 3 | 1.296 | 12.30 | 14.09 | 270.19 | 14.31 | 0.000% |
| Aider 0.86.2 | 1 | 10.886 | 200.23 | 248.21 | 248.36 | 248.36 | 0.472% |
| Aider 0.86.2 | 2 | 7.349 | 200.31 | 248.22 | 248.40 | 248.40 | 0.179% |
| Aider 0.86.2 | 3 | 7.104 | 200.12 | 247.69 | 247.85 | 247.85 | 0.387% |
| Claude Code 2.1.328 | 1 | 2.328 | 216.91 | 294.36 | 294.36 | 223.44 | 0.071% |
| Claude Code 2.1.328 | 2 | 1.084 | 235.92 | 283.35 | 283.35 | 228.48 | 0.341% |
| Claude Code 2.1.328 | 3 | 1.033 | 233.81 | 284.25 | 284.25 | 228.10 | 0.248% |
| OpenCode 2.2.5 | 1 | 2.625 | 416.35 | 529.02 | 529.02 | 492.88 | 0.565% |
| OpenCode 2.2.5 | 2 | 2.418 | 414.68 | 522.96 | 522.96 | 483.24 | 0.316% |
| OpenCode 2.2.5 | 3 | 2.584 | 414.41 | 509.76 | 509.76 | 480.16 | 0.326% |

## Reproduce

Use Python 3 on Windows and install the sampler dependencies in a virtual environment.

```powershell
python -m pip install psutil==7.2.2 pyte==0.8.2 pywinpty==3.1.2
cargo build --release --locked -j 1
python benchmarks/windows_cli.py --karrycode target/release/karrycode.exe --claude (Get-Command claude).Source --aider (Get-Command aider).Source --opencode (Get-Command opencode).Source --runs 3 --out windows-results.json
```

Pass executable paths for the CLIs you have installed. The script binds its fixture to loopback and uses temporary profiles and fixture credentials. [Sampler and fixture](windows_cli.py)

## Plans, TOML configuration and Creator

On 27 November 2026, the same fixture ran three times on an optimized Windows build with the new features and no installed extensions. The executable was 12.45 MiB. Its SHA-256 was `7e1a9f157d2e487b49d640c98709962bfdee91212c13074eb809d81ab7a60dec`. This snapshot was built before the Windows interpreter staging refinements.

| Metric | Three runs |
| --- | ---: |
| Time to prompt | 1.58–2.25 s |
| Idle RAM | 12.07–12.21 MiB |
| Peak RAM while streaming | 14.01–14.21 MiB |
| Full-turn peak RAM, including profile save | 269.96–270.06 MiB |
| Streaming CPU, share of all logical CPUs | 0.18–0.26% |

A separate Rhai fixture took 113 ms from approval to the next request carrying the tool result. Its process tree rose from 13.17 MiB to 37.05 MiB before the conversation save. The fixture passed a small JSON object to a fresh worker and returned one string. Running scripts with more data or external interpreters has a different cost.

In Ubuntu WSL, a separate smoke test sent JSON and one staged text file through the KarryCode extension runner. Bash returned its result in 48 ms and Python in 46 ms with Bubblewrap. On this Windows host, AppContainer rejected the installed Python and PowerShell. A native Windows Bash executable was unavailable. These launches yielded diagnostic results.
