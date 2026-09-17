#!/usr/bin/env python3
"""Enforce that fluidb code is authored by 9router models, not by Claude.

PostToolUse on mcp__9router__ask|panel: record (prompt, answer) in a ledger keyed by session and agent.
PreToolUse on Edit|Write|MultiEdit|NotebookEdit: on a guarded code path, allow the edit only if
  - its text appears (whitespace-insensitive) in an answer this same agent received, and
  - the lines it adds were not dictated in the prompt that produced that answer.
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
AUTHOR = "cx/gpt-5.6-terra"
# Added lines shorter than this (normalized) are structural (`}`, `)?;`) and prove nothing either way.
MIN_LINE = 6
# An edit is treated as dictated when at least this share of its significant added lines was already in the prompt.
DICTATED_SHARE = 0.5


def ledger_path(data):
    base = Path.home() / ".cache" / "claude-9router-ledger"
    base.mkdir(parents=True, exist_ok=True)
    agent = re.sub(r"[^A-Za-z0-9_-]", "", data.get("agent_id") or "main")
    return base / f"{data.get('session_id', 'unknown')}-{agent}.jsonl"


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


def load_ledger(data):
    lp = ledger_path(data)
    records = []
    if lp.exists():
        for line in lp.read_text(encoding="utf-8").splitlines():
            try:
                rec = json.loads(line)
                records.append((norm(rec["prompt"]), norm(rec["answer"])))
            except (ValueError, KeyError):
                continue
    return records


def added_lines(new, old):
    old_lines = {norm(l) for l in old.splitlines()}
    return [n for n in (norm(l) for l in new.splitlines()) if len(n) >= MIN_LINE and n not in old_lines]


def check_piece(new, old, records):
    """Return None if allowed, else a short reason."""
    text = norm(new) or norm(old)
    if not text:
        return None
    answered = [prompt for prompt, answer in records if text in answer]
    if not answered:
        return "no 9router answer this agent received contains this text"
    added = added_lines(new, old)
    if not added:
        return None
    for prompt in answered:
        dictated = sum(1 for a in added if a in prompt)
        if dictated / len(added) < DICTATED_SHARE:
            return None
    return "the new lines were written into the 9router prompt, so the model only echoed Claude's code"


def main():
    data = json.load(sys.stdin)
    event = data.get("hook_event_name")
    tool = data.get("tool_name", "")

    if event == "PostToolUse" and tool.startswith("mcp__9router__"):
        inp = data.get("tool_input") or {}
        rec = {
            "prompt": "\n".join(str(inp.get(k) or "") for k in ("system", "prompt")),
            "answer": "\n".join(strings(data.get("tool_response"))),
        }
        with open(ledger_path(data), "a", encoding="utf-8") as f:
            f.write(json.dumps(rec) + "\n")
        return

    if event != "PreToolUse" or os.environ.get("FLUIDB_CLAUDE_EDITS") == "1":
        return

    cwd = data.get("cwd") or os.getcwd()
    inp = data.get("tool_input") or {}
    who = data.get("agent_type") or "main session"
    how = (f"Code under crates/, vendor/, Cargo.toml, ci.sh and *.rs must be written by 9router: call "
           f"mcp__9router__ask with model \"{AUTHOR}\" yourself, describing the task or pasting the raw cargo "
           "error output and attaching files by path. Never put the new code (or 'change X to Y') in the prompt. "
           "Then apply its SEARCH/REPLACE blocks verbatim.")

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
        try:
            current = Path(os.path.join(cwd, path)).read_text(encoding="utf-8")
        except OSError:
            current = ""
        pieces = [(inp.get("content", ""), current)]
    elif tool == "Edit":
        pieces = [(inp.get("new_string", ""), inp.get("old_string", ""))]
    elif tool == "MultiEdit":
        pieces = [(e.get("new_string", ""), e.get("old_string", "")) for e in inp.get("edits", [])]
    else:
        pieces = [(inp.get("new_source", ""), "")]

    records = load_ledger(data)
    for new, old in pieces:
        reason = check_piece(new, old, records)
        if reason:
            first = (new.strip() or old.strip()).splitlines()
            snippet = first[0][:120] if first else ""
            deny(f"9router guard ({who}): edit to {rel(path, cwd)} denied: {reason} "
                 f"(first line: {snippet!r}). {how}")


if __name__ == "__main__":
    main()
