#!/usr/bin/env python3
"""
claude-sessions: find every Claude Code session on this computer, show them in a
way you can actually recognise, and relaunch any of them with one keypress.

Zero dependencies. Python 3.8+. Windows / macOS / Linux.

    python claude_sessions.py            interactive picker
    python claude_sessions.py --list     plain table, no prompt
    python claude_sessions.py --json     machine-readable dump
    python claude_sessions.py --here     launch in THIS terminal instead of a new window
    python claude_sessions.py -s word    pre-filter by text (title, prompt, folder)
    python claude_sessions.py --all      include sidechain / print-mode / tiny sessions

How it works (all verified against Claude Code 2.1.x and the official docs):
  * Transcripts live at  <config>/projects/<encoded-cwd>/<session-id>.jsonl
    where <config> is $CLAUDE_CONFIG_DIR or ~/.claude.
    Sub-agent transcripts live in <session-id>/subagents/ and are skipped.
  * Every entry carries sessionId, cwd, timestamp, type, isSidechain, gitBranch,
    and (for user/assistant) message.content. We take cwd from the file, never by
    decoding the folder name (that encoding is lossy).
  * A name you gave the session (/rename or --name) and the auto-generated title
    are looked for in dedicated entries; if absent we fall back to the first real
    prompt you typed. These entry formats are internal and may change between
    versions, so parsing is defensive.
  * Sessions still running are listed in <config>/sessions/<pid>.json (a live
    registry) and are marked with a dot.
  * Relaunch = `cd <original cwd> && claude --resume <session-id>`. Resuming by id
    is cwd-independent since 2.1.223, but cd-ing first works on every version and
    puts you back in the right project.
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Iterable, Optional

UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
IS_WIN = platform.system() == "Windows"
IS_MAC = platform.system() == "Darwin"


# --------------------------------------------------------------------------- #
# Discovery
# --------------------------------------------------------------------------- #

def config_dir() -> Path:
    env = os.environ.get("CLAUDE_CONFIG_DIR")
    return Path(env).expanduser() if env else Path.home() / ".claude"


@dataclass
class Session:
    session_id: str
    path: Path
    cwd: str = ""
    title: str = ""            # user-set name or auto title, if found
    title_source: str = ""     # "name" | "auto" | "summary" | "prompt"
    first_prompt: str = ""
    last_prompt: str = ""
    started: Optional[_dt.datetime] = None
    last_active: Optional[_dt.datetime] = None
    user_turns: int = 0
    assistant_turns: int = 0
    git_branch: str = ""
    version: str = ""
    entrypoint: str = ""
    size_bytes: int = 0
    running_pid: Optional[int] = None
    running_name: str = ""
    tools_used: dict = field(default_factory=dict)

    # -- derived ---------------------------------------------------------- #
    @property
    def label(self) -> str:
        return self.title or self.first_prompt or "(empty session)"

    @property
    def project(self) -> str:
        return _basename(self.cwd) if self.cwd else self.path.parent.name

    @property
    def is_running(self) -> bool:
        return self.running_pid is not None

    def launch_command(self) -> str:
        """Shell command that reopens this session in its original folder."""
        if IS_WIN:
            # /d switches drive as well as directory
            return f'cd /d "{self.cwd}" && claude --resume {self.session_id}'
        return f"cd {shlex.quote(self.cwd)} && claude --resume {self.session_id}"


def _parse_ts(value) -> Optional[_dt.datetime]:
    if not isinstance(value, str):
        return None
    try:
        return _dt.datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone()
    except ValueError:
        return None


def _text_of(content) -> str:
    """Flatten a message.content (str or list of blocks) into user-visible text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict) and block.get("type") == "text":
                parts.append(str(block.get("text", "")))
        return "\n".join(parts)
    return ""


_INJECTED_BLOCK = re.compile(
    r"<(system-reminder|command-name|command-message|command-args|local-command-stdout|"
    r"local-command-caveat|ide_[a-z_]+|antml:[a-z_]+)[^>]*>.*?</\1>", re.S)


def _strip_injected(text: str) -> str:
    """Remove blocks Claude Code injects around what the human typed."""
    text = _INJECTED_BLOCK.sub(" ", text)
    text = re.sub(r"<(system-reminder|command-name)[^>]*>.*$", " ", text, flags=re.S)  # unterminated
    if text.lstrip().startswith("Caveat: The messages below were generated"):
        return ""
    return text


def _looks_like_real_prompt(text: str) -> bool:
    return bool(_strip_injected(text).strip())


def _clean(text: str, limit: int = 110) -> str:
    text = _strip_injected(text)
    text = re.sub(r"\s+", " ", text).strip()
    if len(text) > limit:
        text = text[: limit - 1].rstrip() + "…"
    return text


def _basename(p: str) -> str:
    """Last path component for either Windows or POSIX paths, on any host."""
    p = p.rstrip("/\\")
    return re.split(r"[/\\]", p)[-1] if p else ""


def parse_session(path: Path) -> Optional[Session]:
    s = Session(session_id=path.stem, path=path, size_bytes=path.stat().st_size)
    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return None
    with fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(d, dict):
                continue
            t = d.get("type")

            # ---- metadata present on most entries
            if not s.cwd and isinstance(d.get("cwd"), str):
                s.cwd = d["cwd"]
            if not s.version and isinstance(d.get("version"), str):
                s.version = d["version"]
            if not s.entrypoint and isinstance(d.get("entrypoint"), str):
                s.entrypoint = d["entrypoint"]
            if isinstance(d.get("gitBranch"), str) and d["gitBranch"]:
                s.git_branch = d["gitBranch"]
            ts = _parse_ts(d.get("timestamp"))
            if ts:
                s.started = s.started or ts
                if not s.last_active or ts > s.last_active:
                    s.last_active = ts

            # ---- title / name entries (internal format; handle every spelling seen)
            # user-set name via /rename or --name
            for key in ("customTitle", "sessionName", "name"):
                if t in ("custom-title", "session-name", "rename") and isinstance(d.get(key), str) and d[key].strip():
                    s.title, s.title_source = _clean(d[key]), "name"
            # auto-generated title (Haiku summary of first prompt)
            if t in ("ai-title", "session-title") and s.title_source != "name":
                for key in ("aiTitle", "title", "sessionTitle"):
                    if isinstance(d.get(key), str) and d[key].strip():
                        s.title, s.title_source = _clean(d[key]), "auto"
            # older compaction summaries: {"type":"summary","summary":"..."}
            if t == "summary" and isinstance(d.get("summary"), str) and s.title_source in ("", "prompt"):
                s.title, s.title_source = _clean(d["summary"]), "summary"

            # ---- conversation turns
            if d.get("isSidechain"):
                continue
            msg = d.get("message")
            if t == "user" and isinstance(msg, dict):
                content = msg.get("content")
                is_tool_result = isinstance(content, list) and any(
                    isinstance(b, dict) and b.get("type") == "tool_result" for b in content
                )
                if is_tool_result:
                    continue
                text = _text_of(content)
                if _looks_like_real_prompt(text):
                    s.user_turns += 1
                    cleaned = _clean(text)
                    if not s.first_prompt:
                        s.first_prompt = cleaned
                    s.last_prompt = cleaned
            elif t == "assistant" and isinstance(msg, dict):
                s.assistant_turns += 1
                content = msg.get("content")
                if isinstance(content, list):
                    for b in content:
                        if isinstance(b, dict) and b.get("type") == "tool_use":
                            name = str(b.get("name", "?"))
                            s.tools_used[name] = s.tools_used.get(name, 0) + 1

    if not s.title:
        s.title_source = "prompt"
    return s


def discover(cfg: Path) -> list[Session]:
    projects = cfg / "projects"
    out: list[Session] = []
    if not projects.is_dir():
        return out
    for proj in sorted(projects.iterdir()):
        if not proj.is_dir():
            continue
        for f in proj.iterdir():
            # only top-level <session-id>.jsonl files; <session-id>/subagents/* are sidechains
            if f.is_file() and f.suffix == ".jsonl" and UUID_RE.match(f.stem):
                sess = parse_session(f)
                if sess:
                    out.append(sess)
    return out


def _pid_alive(pid: int) -> bool:
    try:
        if IS_WIN:
            # tasklist is the dependency-free way on Windows
            r = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/NH"],
                               capture_output=True, text=True, timeout=5)
            return str(pid) in r.stdout
        os.kill(pid, 0)
        return True
    except (OSError, subprocess.SubprocessError, ValueError):
        return False


def mark_running(cfg: Path, sessions: list[Session]) -> None:
    reg = cfg / "sessions"
    if not reg.is_dir():
        return
    by_id = {s.session_id: s for s in sessions}
    for f in reg.glob("*.json"):
        try:
            d = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        sid, pid = d.get("sessionId"), d.get("pid")
        if sid in by_id and isinstance(pid, int) and _pid_alive(pid):
            by_id[sid].running_pid = pid
            by_id[sid].running_name = str(d.get("name", ""))


def load_sessions(cfg: Path, include_all: bool = False) -> list[Session]:
    sessions = discover(cfg)
    mark_running(cfg, sessions)
    if not include_all:
        # hide empty shells and non-interactive (-p / SDK) runs, like the built-in picker does
        sessions = [s for s in sessions if s.user_turns > 0 and s.entrypoint not in ("sdk-cli", "sdk-ts", "sdk-py")]
    sessions.sort(key=lambda s: s.last_active or _dt.datetime.min.replace(tzinfo=_dt.timezone.utc), reverse=True)
    return sessions


# --------------------------------------------------------------------------- #
# Presentation
# --------------------------------------------------------------------------- #

def human_age(ts: Optional[_dt.datetime]) -> str:
    if not ts:
        return "?"
    delta = _dt.datetime.now(_dt.timezone.utc) - ts.astimezone(_dt.timezone.utc)
    secs = int(delta.total_seconds())
    if secs < 60:
        return "just now"
    if secs < 3600:
        return f"{secs // 60}m ago"
    if secs < 86400:
        return f"{secs // 3600}h ago"
    if secs < 86400 * 14:
        return f"{secs // 86400}d ago"
    return ts.strftime("%Y-%m-%d")


def human_size(n: int) -> str:
    for unit in ("B", "K", "M", "G"):
        if n < 1024:
            return f"{n:.0f}{unit}"
        n /= 1024
    return f"{n:.0f}T"


def _use_color() -> bool:
    if os.environ.get("NO_COLOR"):
        return False
    if not sys.stdout.isatty():
        return False
    if IS_WIN:
        try:  # enable VT processing on Win10+ consoles
            import ctypes
            k = ctypes.windll.kernel32
            h = k.GetStdHandle(-11)
            mode = ctypes.c_uint32()
            if k.GetConsoleMode(h, ctypes.byref(mode)):
                k.SetConsoleMode(h, mode.value | 0x0004)
        except Exception:
            return False
    return True


COLOR = _use_color()


def c(code: str, text: str) -> str:
    return f"\033[{code}m{text}\033[0m" if COLOR else text


def term_width() -> int:
    return shutil.get_terminal_size((100, 24)).columns


def render_table(sessions: list[Session], start: int = 1) -> str:
    if not sessions:
        return c("33", "No sessions found.")
    w = term_width()
    num_w = len(str(start + len(sessions) - 1))
    proj_w = min(22, max(7, max(len(s.project) for s in sessions)))
    age_w = 10
    turns_w = 5
    fixed = num_w + 2 + 2 + proj_w + 1 + age_w + 1 + turns_w + 3
    label_w = max(20, w - fixed)
    lines = []
    hdr = f"{' ' * (num_w + 2)}  {'project':<{proj_w}} {'last':<{age_w}} {'turns':>{turns_w}}   {'session'}"
    lines.append(c("2", hdr))
    for i, s in enumerate(sessions, start):
        dot = c("32", "●") if s.is_running else " "
        proj = (s.project[: proj_w - 1] + "…") if len(s.project) > proj_w else s.project
        label = s.label
        if len(label) > label_w:
            label = label[: label_w - 1] + "…"
        if s.title_source == "name":
            label = c("1;36", label)      # you named it: bold cyan
        elif s.title_source == "auto":
            label = c("36", label)
        branch = f" ({s.git_branch})" if s.git_branch and s.git_branch not in ("HEAD", "main", "master") else ""
        lines.append(
            f"{c('1', str(i).rjust(num_w))}  {dot} {c('33', proj.ljust(proj_w))} "
            f"{human_age(s.last_active).ljust(age_w)} {str(s.user_turns).rjust(turns_w)}   {label}{c('2', branch)}"
        )
    return "\n".join(lines)


def render_detail(s: Session) -> str:
    tools = ", ".join(f"{k}×{v}" for k, v in sorted(s.tools_used.items(), key=lambda kv: -kv[1])[:8])
    rows = [
        ("Session", s.label),
        ("Title from", {"name": "you named it", "auto": "auto-generated", "summary": "compaction summary", "prompt": "first prompt"}[s.title_source]),
        ("Folder", s.cwd or "(unknown)"),
        ("Branch", s.git_branch or "-"),
        ("Started", s.started.strftime("%Y-%m-%d %H:%M") if s.started else "?"),
        ("Last active", f"{s.last_active.strftime('%Y-%m-%d %H:%M') if s.last_active else '?'}  ({human_age(s.last_active)})"),
        ("Turns", f"{s.user_turns} prompts / {s.assistant_turns} replies, {human_size(s.size_bytes)} on disk"),
        ("First prompt", s.first_prompt or "-"),
        ("Last prompt", s.last_prompt or "-"),
        ("Tools", tools or "-"),
        ("Claude Code", s.version or "?"),
        ("Running", f"yes, pid {s.running_pid} ({s.running_name})" if s.is_running else "no"),
        ("ID", s.session_id),
        ("Transcript", str(s.path)),
        ("Command", s.launch_command()),
    ]
    return "\n".join(f"  {c('2', k.rjust(12))}  {v}" for k, v in rows)


def matches(s: Session, needle: str) -> bool:
    n = needle.lower()
    hay = " ".join([s.title, s.first_prompt, s.last_prompt, s.cwd, s.git_branch, s.session_id]).lower()
    return all(tok in hay for tok in n.split())


# --------------------------------------------------------------------------- #
# Launching
# --------------------------------------------------------------------------- #

def claude_available() -> bool:
    return shutil.which("claude") is not None or (IS_WIN and shutil.which("claude.cmd") is not None)


def launch_here(s: Session) -> None:
    """Replace this process with claude --resume in the session's folder."""
    if not Path(s.cwd).is_dir():
        print(c("31", f"Folder no longer exists: {s.cwd}"))
        print("Resuming from here instead (claude looks sessions up by id across projects).")
    else:
        os.chdir(s.cwd)
    exe = shutil.which("claude") or shutil.which("claude.cmd")
    if not exe:
        sys.exit("'claude' is not on PATH. Install Claude Code first.")
    args = [exe, "--resume", s.session_id]
    print(c("2", f"$ cd {s.cwd} && claude --resume {s.session_id}"))
    sys.stdout.flush()
    if IS_WIN:
        sys.exit(subprocess.call(args))      # os.exec* is unreliable on Windows consoles
    os.execv(exe, args)


def launch_new_window(s: Session) -> bool:
    """Open the session in a fresh terminal window. Returns True on success."""
    cmd = s.launch_command()
    cwd = s.cwd if Path(s.cwd).is_dir() else None
    try:
        if IS_WIN:
            # Windows Terminal if present (nicer tabs), else classic cmd window.
            if shutil.which("wt"):
                subprocess.Popen(["wt", "-d", cwd or ".", "cmd", "/k", f"claude --resume {s.session_id}"])
            else:
                subprocess.Popen(f'start "Claude: {s.project}" cmd /k "{cmd}"', shell=True)
            return True
        if IS_MAC:
            script = f'tell application "Terminal" to do script {json.dumps(cmd)}\ntell application "Terminal" to activate'
            subprocess.Popen(["osascript", "-e", script])
            return True
        # Linux / BSD: try the common emulators in order.
        shell_cmd = f"{cmd}; exec $SHELL"
        candidates = [
            (["x-terminal-emulator", "-e", "sh", "-c", shell_cmd], "x-terminal-emulator"),
            (["gnome-terminal", "--", "sh", "-c", shell_cmd], "gnome-terminal"),
            (["konsole", "-e", "sh", "-c", shell_cmd], "konsole"),
            (["kitty", "sh", "-c", shell_cmd], "kitty"),
            (["alacritty", "-e", "sh", "-c", shell_cmd], "alacritty"),
            (["wezterm", "start", "--", "sh", "-c", shell_cmd], "wezterm"),
            (["xfce4-terminal", "-e", f"sh -c {shlex.quote(shell_cmd)}"], "xfce4-terminal"),
            (["xterm", "-e", "sh", "-c", shell_cmd], "xterm"),
        ]
        for argv, name in candidates:
            if shutil.which(name):
                subprocess.Popen(argv, cwd=cwd)
                return True
        return False
    except OSError as e:
        print(c("31", f"Could not open a terminal: {e}"))
        return False


def copy_to_clipboard(text: str) -> bool:
    try:
        if IS_WIN:
            subprocess.run("clip", input=text, text=True, check=True, shell=True)
        elif IS_MAC:
            subprocess.run("pbcopy", input=text, text=True, check=True)
        elif shutil.which("wl-copy"):
            subprocess.run(["wl-copy"], input=text, text=True, check=True)
        elif shutil.which("xclip"):
            subprocess.run(["xclip", "-selection", "clipboard"], input=text, text=True, check=True)
        else:
            return False
        return True
    except (OSError, subprocess.SubprocessError):
        return False


# --------------------------------------------------------------------------- #
# Interactive picker
# --------------------------------------------------------------------------- #

HELP = """
  <number>      open that session in a new terminal window
  <number> h    open it HERE, in this terminal (replaces this picker)
  <number> i    show details (first/last prompt, folder, tools used...)
  <number> c    copy its resume command to the clipboard
  /text         filter by title, prompt, folder, branch or id  (/ alone clears)
  r             rescan
  q             quit
"""


def interactive(cfg: Path, sessions: list[Session], initial_filter: str, here: bool) -> None:
    needle = initial_filter
    while True:
        view = [s for s in sessions if matches(s, needle)] if needle else sessions
        print()
        hdr = f"Claude Code sessions  {c('2', str(cfg))}"
        if needle:
            hdr += f"   filter: {c('33', needle)} ({len(view)}/{len(sessions)})"
        else:
            hdr += f"   {len(sessions)} sessions"
        print(hdr)
        print(render_table(view))
        print(c("2", "  ● = currently running.  number = launch, number+i = info, /text = filter, ? = help, q = quit"))
        try:
            raw = input("> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return
        if not raw:
            continue
        if raw in ("q", "quit", "exit"):
            return
        if raw == "?":
            print(HELP)
            continue
        if raw == "r":
            sessions = load_sessions(cfg)
            continue
        if raw.startswith("/"):
            needle = raw[1:].strip()
            continue
        m = re.match(r"^(\d+)\s*([hic]?)$", raw)
        if not m:
            print(c("33", "Didn't understand that. Type ? for help."))
            continue
        idx, action = int(m.group(1)), m.group(2)
        if not 1 <= idx <= len(view):
            print(c("33", f"Pick 1–{len(view)}."))
            continue
        s = view[idx - 1]
        if action == "i":
            print()
            print(render_detail(s))
            continue
        if action == "c":
            ok = copy_to_clipboard(s.launch_command())
            print(c("32", "Copied: ") + s.launch_command() if ok else c("33", "No clipboard tool found. Command:\n  ") + s.launch_command())
            continue
        if s.is_running:
            print(c("33", f"That session is already open (pid {s.running_pid}). Switch to that window, or run `claude attach {s.session_id}`."))
            continue
        if action == "h" or here:
            launch_here(s)
            return
        if launch_new_window(s):
            print(c("32", f"Opened '{s.label}' in a new window."))
        else:
            print(c("33", "Couldn't find a terminal emulator to open. Run this instead:"))
            print("  " + s.launch_command())
            print("  (or pick the same number followed by h to open it right here)")


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #

def main(argv: Optional[Iterable[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="Find and relaunch Claude Code sessions.")
    ap.add_argument("-s", "--search", default="", help="pre-filter sessions by text")
    ap.add_argument("--list", action="store_true", help="print the table and exit")
    ap.add_argument("--json", action="store_true", help="dump sessions as JSON and exit")
    ap.add_argument("--here", action="store_true", help="launch in this terminal instead of a new window")
    ap.add_argument("--all", action="store_true", help="include empty, print-mode and SDK sessions")
    ap.add_argument("--config-dir", help="override Claude config dir (default: $CLAUDE_CONFIG_DIR or ~/.claude)")
    ap.add_argument("--open", metavar="ID_OR_PREFIX", help="skip the picker; launch the session whose id starts with this")
    a = ap.parse_args(argv)

    cfg = Path(a.config_dir).expanduser() if a.config_dir else config_dir()
    if not (cfg / "projects").is_dir():
        print(f"No Claude Code data at {cfg} (no 'projects' folder). Has Claude Code been run on this machine?")
        return 1

    sessions = load_sessions(cfg, include_all=a.all)
    if a.search:
        sessions_view = [s for s in sessions if matches(s, a.search)]
    else:
        sessions_view = sessions

    if a.json:
        def enc(o):
            if isinstance(o, _dt.datetime):
                return o.isoformat()
            if isinstance(o, Path):
                return str(o)
            raise TypeError
        rows = []
        for s in sessions_view:
            d = asdict(s)
            d["label"], d["project"], d["command"] = s.label, s.project, s.launch_command()
            rows.append(d)
        print(json.dumps(rows, default=enc, indent=2))
        return 0

    if a.open:
        hits = [s for s in sessions if s.session_id.startswith(a.open.lower())]
        if len(hits) != 1:
            print(f"{len(hits)} sessions match '{a.open}'.")
            return 1
        if a.here:
            launch_here(hits[0])
        elif not launch_new_window(hits[0]):
            print(hits[0].launch_command())
        return 0

    if a.list or not sys.stdin.isatty():
        print(render_table(sessions_view))
        return 0

    if not claude_available():
        print(c("33", "Note: 'claude' isn't on PATH here, so launching will fail. Listing anyway."))
    interactive(cfg, sessions, a.search, a.here)
    return 0


if __name__ == "__main__":
    sys.exit(main())
