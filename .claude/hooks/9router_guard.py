#!/usr/bin/env python3
"""Enforce that fluidb code is authored by 9router models, not by Claude.

PostToolUse on mcp__9router__ask|panel: append the model's answer to a per-session ledger.
PreToolUse on Edit|Write|MultiEdit|NotebookEdit: on a guarded code path, allow the edit only
if the text it introduces appears (whitespace-insensitive) in the ledger.
PreToolUse on Bash: block shell writes to guarded code paths (sed -i, redirects, tee, cp, ...).

Escape hatch: start Claude Code with FLUIDB_CLAUDE_EDITS=1 in the environment.
"""
import json
import os
import re
import sys
from pathlib import Path

GUARDED = re.compile(
    r"(^|/)(crates|vendor)/|(^|/)Cargo\.toml$|(^|/)ci\.sh$|(^|/)rust-toolchain\.toml$|\.rs$|(^|/)Dockerfile[^/]*$"
)
GUARDED_IN_CMD = re.compile(
    r"(crates|vendor)/|Cargo\.toml|\bci\.sh\b|rust-toolchain\.toml|\.rs\b|Dockerfile"
)
SHELL_WRITERS = re.compile(
    r"\bsed\s+(-[a-zA-Z]*i|--in-place)|\bperl\s+-[a-zA-Z]*i|\btee\b|\b(cp|mv|install|touch|truncate|patch|dd)\s"
    r"|open\([^)]*['\"][wax+]|write_text\(|\bcargo\s+(add|remove|rm)\b"
)
REDIRECT = re.compile(r"(?<![0-9&<])>{1,2}\s*[\"']?([^\s\"';|&)]+)")


def ledger_path(data):
    base = Path.home() / ".cache" / "claude-9router-ledger"
    base.mkdir(parents=True, exist_ok=True)
    return base / f"{data.get('session_id', 'unknown')}.txt"


def strings(obj):
    if isinstance(obj, str):
        yield obj
    elif isinstance(obj, dict):
        for v in obj.values():
            yield from strings(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from strings(v)


def norm(text):
    return re.sub(r"\s+", "", text)


def deny(reason):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }))
    sys.exit(0)


def rel(path, cwd):
    try:
        return os.path.relpath(os.path.abspath(os.path.join(cwd, path)), cwd)
    except ValueError:
        return path


def main():
    data = json.load(sys.stdin)
    event = data.get("hook_event_name")
    tool = data.get("tool_name", "")

    if event == "PostToolUse" and tool.startswith("mcp__9router__"):
        with open(ledger_path(data), "a", encoding="utf-8") as f:
            for s in strings(data.get("tool_response")):
                f.write(s + "\n")
        return

    if event != "PreToolUse" or os.environ.get("FLUIDB_CLAUDE_EDITS") == "1":
        return

    cwd = data.get("cwd") or os.getcwd()
    inp = data.get("tool_input") or {}
    who = data.get("agent_type") or "main session"
    how = ("Code under crates/, vendor/, Cargo.toml, ci.sh and *.rs must be written by 9router: call "
           "mcp__9router__ask with model \"cx/gpt-5.6-terra\", then apply its SEARCH/REPLACE blocks verbatim.")

    if tool == "Bash":
        cmd = inp.get("command", "")
        targets = [t for t in REDIRECT.findall(cmd) if t != "/dev/null" and GUARDED.search(rel(t, cwd))]
        if targets or (SHELL_WRITERS.search(cmd) and GUARDED_IN_CMD.search(cmd)):
            deny(f"9router guard ({who}): shell writes to code paths are blocked. {how} "
                 "Use Edit/Write with the model's text; cargo fmt/test/clippy are fine.")
        return

    path = inp.get("file_path") or inp.get("notebook_path") or ""
    if not path or not GUARDED.search(rel(path, cwd)):
        return

    if tool == "Write":
        pieces = [inp.get("content", "")]
    elif tool == "Edit":
        pieces = [inp.get("new_string", "")] if inp.get("new_string") else [inp.get("old_string", "")]
    elif tool == "MultiEdit":
        pieces = [e.get("new_string") or e.get("old_string", "") for e in inp.get("edits", [])]
    else:
        pieces = [inp.get("new_source", "")]

    lp = ledger_path(data)
    ledger = norm(lp.read_text(encoding="utf-8")) if lp.exists() else ""
    missing = [p for p in pieces if norm(p) and norm(p) not in ledger]
    if missing:
        snippet = missing[0].strip().splitlines()[0][:120] if missing[0].strip() else ""
        deny(f"9router guard ({who}): this edit to {rel(path, cwd)} contains text no 9router response "
             f"in this session produced (first line: {snippet!r}). {how}")


if __name__ == "__main__":
    main()
