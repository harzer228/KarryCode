"""Sample Windows CLI process trees against a fixed local streaming response."""

import argparse
import contextlib
import json
import os
import platform
import re
import statistics
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from queue import Empty, Queue
from urllib.parse import urlsplit

import psutil
import pyte
from winpty import PtyProcess


PROMPT = "Summarize the benchmark fixture using plain text. Do not use tools or change files."
END = "BENCHMARK COMPLETE"
MODEL = "fixture"
LINES = [
    f"Benchmark line {i:03}: The terminal renders this fixed text reply.\n"
    for i in range(100)
]
REPLY = "".join(LINES) + END
SEND_INTERVAL = 0.05
SAMPLE_INTERVAL = 0.1
MIB = 2**20


class Fixture(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def respond(self, payload):
        data = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if urlsplit(self.path).path == "/v1/models":
            self.respond({"data": [{"id": MODEL, "context_length": 32768, "max_output_tokens": 4096}]})
        else:
            self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        path = urlsplit(self.path).path
        if path.endswith("/count_tokens"):
            self.respond({"input_tokens": 600})
            return
        if path not in ("/v1/messages", "/v1/chat/completions"):
            self.send_error(404)
            return
        anthropic = path.endswith("/messages")
        messages = body.get("messages", [])
        content = json.dumps(messages[-1]["content"]) if messages else ""
        main = PROMPT in content and "Reply with only the title." not in content
        chunks = [*LINES, END] if main else ["Benchmark fixture"]
        model = body["model"]
        stream_record = {
            "path": path, "streaming": bool(body.get("stream")),
            "metadata": [(m["role"], str(m["content"])[:200]) for m in messages],
            "max_tokens": body.get("max_tokens"),
        }
        if main:
            self.server.streams.append(stream_record)
        if not body.get("stream"):
            text = "".join(chunks)
            if anthropic:
                self.respond({
                    "id": "msg_fixture", "model": model, "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": text}], "stop_reason": "end_turn",
                    "usage": {"input_tokens": 600, "output_tokens": 20},
                })
            else:
                self.respond(self.openai(model, {
                    "message": {"role": "assistant", "content": text}, "finish_reason": "stop",
                }, "chat.completion"))
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        try:
            if anthropic:
                self.event("message_start", message={
                    "id": "msg_fixture", "model": model, "type": "message", "role": "assistant",
                    "content": [], "stop_reason": None, "stop_sequence": None,
                    "usage": {"input_tokens": 600, "output_tokens": 0},
                })
                self.event("content_block_start", index=0, content_block={"type": "text", "text": ""})
            else:
                self.chunk(model, {"role": "assistant"})
            stream_record["stream_start"] = time.perf_counter()
            for text in chunks:
                if main:
                    time.sleep(SEND_INTERVAL)
                if text == END:
                    stream_record["stream_end"] = time.perf_counter()
                if anthropic:
                    self.event("content_block_delta", index=0, delta={"type": "text_delta", "text": text})
                else:
                    self.chunk(model, {"content": text})
            if anthropic:
                self.event("content_block_stop", index=0)
                self.event("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None},
                           usage={"output_tokens": 20})
                self.event("message_stop")
            else:
                self.chunk(model, {}, "stop")
                self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    @staticmethod
    def openai(model, choice, kind):
        return {
            "id": "chatcmpl-fixture", "object": kind, "created": 1700000000, "model": model,
            "choices": [{"index": 0, **choice}],
        }

    def event(self, name, **fields):
        self.wfile.write(f"event: {name}\ndata: {json.dumps({'type': name, **fields})}\n\n".encode())
        self.wfile.flush()

    def chunk(self, model, delta, finish=None):
        event = self.openai(model, {"delta": delta, "finish_reason": finish}, "chat.completion.chunk")
        self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
        self.wfile.flush()


class Terminal:
    def __init__(self, command, cwd, env):
        self.started = time.perf_counter()
        self.child = PtyProcess.spawn(command, cwd=str(cwd), env=env, dimensions=(34, 120))
        self.proc = psutil.Process(self.child.pid)
        self.screen = pyte.Screen(120, 34)
        self.parser = pyte.Stream(self.screen)
        self.output = Queue()
        self.cpu = {}
        threading.Thread(target=self.collect, daemon=True).start()

    def collect(self):
        while self.child.isalive():
            try:
                self.output.put(self.child.read(65536))
            except (EOFError, OSError):
                break

    def display(self):
        while True:
            try:
                self.parser.feed(self.output.get_nowait())
            except Empty:
                return "\n".join(self.screen.display)

    def wait(self, text, seconds=20):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            frame = self.display()
            if re.search(text, frame, re.I):
                return
            if not self.child.isalive() and self.output.empty():
                break
            time.sleep(0.05)
        raise RuntimeError(f"Expected {text!r}\n{self.display()}")

    def type(self, value):
        self.child.write(value)

    def sample(self):
        rss = 0
        for proc in [self.proc, *self.proc.children(recursive=True)]:
            with contextlib.suppress(psutil.NoSuchProcess, psutil.AccessDenied):
                with proc.oneshot():
                    resident = proc.memory_info().rss
                    times = proc.cpu_times()
                rss += resident
                self.cpu[proc.pid] = times.user + times.system
        return rss / MIB

    def close(self):
        children = []
        with contextlib.suppress(psutil.NoSuchProcess):
            children = self.proc.children(recursive=True)
        if self.child.isalive():
            self.child.terminate(force=True)
        for child in children:
            with contextlib.suppress(psutil.NoSuchProcess, psutil.AccessDenied):
                child.terminate()


def isolated_environment(directory, server):
    allowed = {
        "SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "USERNAME", "USERDOMAIN",
        "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "PROGRAMFILES", "PROGRAMFILES(X86)",
        "PROGRAMDATA", "SYSTEMDRIVE", "COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)",
    }
    env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
    for key in (
        "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
        "XDG_CACHE_HOME", "XDG_STATE_HOME", "TEMP", "TMP",
    ):
        path = directory / key.lower()
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    env.update(TERM="xterm-256color", COLORTERM="truecolor", NO_PROXY="localhost,127.0.0.1")
    env.update({name: server for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")})
    return env


def write_json(path, data):
    path.write_text(json.dumps(data), encoding="utf-8")


def prepare(name, directory, endpoint, project):
    env = isolated_environment(directory, endpoint)
    workspace = directory / project
    workspace.mkdir(exist_ok=True)
    if name == "KarryCode":
        return workspace, env, [], r"Ready\. /help"
    if name == "Claude Code":
        config = directory / "claude"
        config.mkdir(exist_ok=True)
        env.update(CLAUDE_CONFIG_DIR=str(config), ANTHROPIC_BASE_URL=endpoint,
                   ANTHROPIC_AUTH_TOKEN="fixture-key", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1",
                   DISABLE_AUTOUPDATER="1", CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS="1")
        write_json(config / ".claude.json", {
            "hasCompletedOnboarding": True, "theme": "dark", "lastOnboardingVersion": "2.1.328",
            "projects": {
                workspace.as_posix(): {"hasTrustDialogAccepted": True},
                str(workspace): {"hasTrustDialogAccepted": True},
            },
        })
        return workspace, env, ["--model", "claude-sonnet-4-6", "--strict-mcp-config",
                                "--mcp-config", '{"mcpServers":{}}'], r"(?m)^ *❯[\s ]*$"
    if name == "Aider":
        env.update(OPENAI_API_BASE=f"{endpoint}/v1", OPENAI_API_KEY="fixture-key",
                   LITELLM_LOCAL_MODEL_COST_MAP="True")
        flags = ["--model", f"openai/{MODEL}", "--no-check-update", "--analytics-disable",
                 "--no-git", "--no-show-model-warnings", "--no-show-release-notes"]
        return workspace, env, flags, r"\bAider v.*[\s\S]+\n\s*>\s*"
    env.update(OPENCODE_DISABLE_AUTOUPDATE="1", OPENCODE_DISABLE_MODELS_FETCH="1",
               OPENCODE_CONFIG=str(directory / "opencode.json"), BENCH_API_KEY="fixture-key")
    write_json(directory / "opencode.json", {
        "update": "disable", "model": f"bench/{MODEL}",
        "providers": {
            "bench": {
                "env": ["BENCH_API_KEY"], "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": f"{endpoint}/v1"},
                "models": {MODEL: {"name": MODEL, "limit": {"context": 32768, "output": 4096}}},
            }
        },
    })
    return workspace, env, ["--standalone"], r"Ask anything|tab agents|/models"


def seed_karrycode(exe, workspace, env, endpoint):
    terminal = Terminal([exe], workspace, env)
    try:
        terminal.wait("First launch")
        terminal.type("\x1b[B")
        terminal.type("\x1b[B")
        terminal.type("\r")
        terminal.wait("2/3")
        terminal.type(f"{endpoint}/v1\tfixture-key\r")
        terminal.wait("3/3")
        terminal.type("\r")
        terminal.wait("Model catalog")
        terminal.type("\r")
        terminal.wait("New model")
        terminal.type("\r")
        terminal.wait("Save your setup")
        terminal.type("\r")
        terminal.wait(r"Ready\. /help")
        terminal.type("/exit\r")
        deadline = time.monotonic() + 5
        while terminal.child.isalive() and time.monotonic() < deadline:
            time.sleep(.05)
        assert not terminal.child.isalive()
    finally:
        terminal.close()


def measure(command, workspace, env, ready, server):
    terminal = Terminal(command, workspace, env)
    try:
        terminal.wait(ready)
        ready_seconds = time.perf_counter() - terminal.started
        idle = []
        for _ in range(20):
            terminal.display()
            idle.append(terminal.sample())
            time.sleep(SAMPLE_INTERVAL)

        streams = len(server.streams)
        terminal.type(PROMPT)
        time.sleep(.25)
        terminal.sample()
        cpu_start = sum(terminal.cpu.values())
        start = time.perf_counter()
        terminal.type("\r")
        active = []
        settled_at = None
        while time.perf_counter() - start < 25:
            frame = terminal.display()
            at = time.perf_counter()
            resident = terminal.sample()
            active.append((at, resident, sum(terminal.cpu.values()) - cpu_start))
            stream_finished = len(server.streams) > streams and "stream_end" in server.streams[streams]
            if settled_at is None and stream_finished and END in frame:
                settled_at = at + 2
            if settled_at is not None and at >= settled_at:
                break
            time.sleep(SAMPLE_INTERVAL)
        else:
            raise RuntimeError(f"No complete benchmark response\n{frame}")
        assert len(server.streams) == streams + 1, server.streams[streams:]
        stream_record = server.streams[streams]
        assert stream_record["streaming"]
        stream_samples = [sample for sample in active
                          if stream_record["stream_start"] <= sample[0] < stream_record["stream_end"]]
        if len(stream_samples) < 12:
            raise RuntimeError(f"Only {len(stream_samples)} readings during the response stream")
        wall = time.perf_counter() - start
        cpu = sum(terminal.cpu.values()) - cpu_start
        stream_wall = stream_samples[-1][0] - stream_samples[0][0]
        stream_cpu = stream_samples[-1][2] - stream_samples[0][2]
        return {
            "ready_s": round(ready_seconds, 3), "idle_mib": round(statistics.median(idle), 2),
            "stream_peak_mib": round(max(s[1] for s in stream_samples), 2),
            "stream_cpu_percent": round(100 * stream_cpu / stream_wall / psutil.cpu_count(), 3),
            "active_peak_mib": round(max(s[1] for s in active), 2),
            "settled_mib": round(statistics.median(s[1] for s in active[-5:]), 2),
            "active_s": round(wall, 3),
            "cpu_core_s": round(cpu, 4),
            "whole_cpu_percent": round(100 * cpu / wall / psutil.cpu_count(logical=True), 3),
        }
    finally:
        terminal.close()


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--karrycode", type=Path)
    cli.add_argument("--claude", type=Path)
    cli.add_argument("--aider", type=Path)
    cli.add_argument("--opencode", type=Path)
    cli.add_argument("--runs", type=int, default=3)
    cli.add_argument("--out", type=Path, required=True)
    args = cli.parse_args()
    executables = {
        name: str(path.resolve(strict=True))
        for name, path in zip(("KarryCode", "Claude Code", "Aider", "OpenCode"),
                              (args.karrycode, args.claude, args.aider, args.opencode))
        if path is not None
    }
    if not executables or args.runs < 1:
        cli.error("provide at least one executable and one run")
    with ThreadingHTTPServer(("127.0.0.1", 0), Fixture) as server:
        server.streams = []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        endpoint = f"http://127.0.0.1:{server.server_port}"
        result = {
            "windows": platform.version(), "logical_cpus": psutil.cpu_count(),
            "stream_reply_bytes": len(REPLY.encode()), "stream_chunks": len(LINES) + 1,
            "sampling_ms": 100, "stream_delay_ms": 50, "runs": {}, "errors": {},
        }
        with tempfile.TemporaryDirectory(prefix="karrycode-benchmark-", ignore_cleanup_errors=True) as temp:
            for name, exe in executables.items():
                directory = Path(temp) / name
                directory.mkdir()
                if name == "KarryCode":
                    workspace, env, _, _ = prepare(name, directory, endpoint, "setup")
                    seed_karrycode(exe, workspace, env, endpoint)
                result["runs"][name] = []
            names = list(executables)
            for index in range(args.runs):
                for name in names[index % len(names):] + names[:index % len(names)]:
                    directory = Path(temp) / name
                    workspace, env, flags, ready = prepare(name, directory, endpoint, f"project-{index + 1}")
                    try:
                        run = measure([executables[name], *flags], workspace, env, ready, server)
                    except (RuntimeError, TimeoutError, AssertionError, OSError, KeyError) as error:
                        result["errors"][name] = str(error)
                        print(f"{name}: {error}", flush=True)
                        write_json(args.out, result)
                        continue
                    result["runs"][name].append(run)
                    write_json(args.out, result)
                    print(f"{name} #{index + 1}: {run}", flush=True)
        server.shutdown()
    if result["errors"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
