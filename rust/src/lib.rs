//! claude-sessions core: discover Claude Code sessions, read their conversations,
//! relaunch them, back them up and export them.
//!
//! Layout and fields verified against Claude Code 2.1.x and the official docs:
//!   <config>/projects/<encoded-cwd>/<session-id>.jsonl         one file per session
//!   <config>/projects/<encoded-cwd>/<session-id>/subagents/     sidechains (not listed)
//!   <config>/sessions/<pid>.json                                live registry → "running"
//! where <config> = $CLAUDE_CONFIG_DIR or ~/.claude.
//! Relaunch = `cd <cwd> && claude --resume <session-id>`.

pub mod backup;
pub mod export;

use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ------------------------------------------------------------------------- //
// Model
// ------------------------------------------------------------------------- //

#[derive(Debug, Default, Clone)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: String,
    pub title_source: &'static str, // "name" | "auto" | "summary" | "prompt"
    pub first_prompt: String,
    pub last_prompt: String,
    pub started: Option<DateTime<Utc>>,
    pub last_active: Option<DateTime<Utc>>,
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub git_branch: String,
    pub version: String,
    pub entrypoint: String,
    pub size_bytes: u64,
    pub running_pid: Option<u32>,
    pub running_name: String,
    pub tools: HashMap<String, usize>,
}

impl Session {
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.first_prompt.is_empty() {
            &self.first_prompt
        } else {
            "(empty session)"
        }
    }
    pub fn project(&self) -> String {
        if self.cwd.is_empty() {
            return self
                .path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        basename(&self.cwd)
    }
    pub fn is_running(&self) -> bool {
        self.running_pid.is_some()
    }
    pub fn launch_command(&self) -> String {
        if cfg!(windows) {
            format!("cd /d \"{}\" && claude --resume {}", self.cwd, self.id)
        } else {
            format!("cd {} && claude --resume {}", sh_quote(&self.cwd), self.id)
        }
    }
    /// The directory holding this session's sidechains / spilled tool results, if any.
    pub fn side_dir(&self) -> PathBuf {
        self.path.with_extension("")
    }
    pub fn title_source_label(&self) -> &'static str {
        match self.title_source {
            "name" => "you named it",
            "auto" => "auto-generated",
            "summary" => "compaction summary",
            _ => "first prompt",
        }
    }
    pub fn top_tools(&self, n: usize) -> String {
        let mut tools: Vec<(&String, &usize)> = self.tools.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        tools.iter().take(n).map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ")
    }
    pub fn matches(&self, needle: &str) -> bool {
        let hay = format!(
            "{} {} {} {} {} {}",
            self.title, self.first_prompt, self.last_prompt, self.cwd, self.git_branch, self.id
        )
        .to_lowercase();
        needle.to_lowercase().split_whitespace().all(|tok| hay.contains(tok))
    }
    /// Safe, recognisable file stem for exports: "<project>-<label>-<id8>".
    pub fn file_stem(&self) -> String {
        let mut s = format!("{}-{}", self.project(), self.label());
        s = s
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-");
        let s: String = s.chars().take(48).collect();
        format!("{}-{}", s.trim_matches('-'), &self.id[..8])
    }
}

/// Last path component for either Windows or POSIX paths, on any host.
pub fn basename(p: &str) -> String {
    let t = p.trim_end_matches(['/', '\\']);
    t.rsplit(['/', '\\']).next().unwrap_or("").to_string()
}

pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

// ------------------------------------------------------------------------- //
// Discovery
// ------------------------------------------------------------------------- //

pub fn config_dir() -> PathBuf {
    if let Ok(d) = env::var("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    PathBuf::from(home_dir()).join(".claude")
}

pub fn home_dir() -> String {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}

/// Remove the blocks Claude Code injects around what the human typed.
pub fn strip_injected(text: &str) -> String {
    const TAGS: &[&str] = &[
        "system-reminder",
        "command-name",
        "command-message",
        "command-args",
        "local-command-stdout",
        "local-command-caveat",
        "ide_",
        "antml:",
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        for tag in TAGS {
            if after.starts_with(tag) {
                let name_end = after.find(|c: char| c == '>' || c == ' ' || c == '/').unwrap_or(after.len());
                let name = &after[..name_end];
                let close = format!("</{}>", name);
                match after.find(&close) {
                    Some(ci) => {
                        rest = &after[ci + close.len()..];
                        out.push(' ');
                    }
                    None => rest = "", // unterminated injected block: drop the remainder
                }
                continue 'outer;
            }
        }
        out.push('<');
        rest = after;
    }
    out.push_str(rest);
    if out.trim_start().starts_with("Caveat: The messages below were generated") {
        return String::new();
    }
    out
}

pub fn clean(text: &str, limit: usize) -> String {
    let s = strip_injected(text);
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&joined, limit)
}

pub fn truncate_chars(s: &str, limit: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > limit {
        let mut t: String = chars[..limit.saturating_sub(1)].iter().collect();
        t = t.trim_end().to_string();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_ts(v: Option<&Value>) -> Option<DateTime<Utc>> {
    v.and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn str_field<'a>(d: &'a Value, k: &str) -> Option<&'a str> {
    d.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn has_tool_result(content: &Value) -> bool {
    content
        .as_array()
        .map_or(false, |a| a.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")))
}

pub fn parse_session(path: &Path) -> Option<Session> {
    let file = fs::File::open(path).ok()?;
    let mut s = Session {
        id: path.file_stem()?.to_string_lossy().into_owned(),
        path: path.to_path_buf(),
        size_bytes: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        title_source: "prompt",
        ..Default::default()
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&line) else { continue };
        if !d.is_object() {
            continue;
        }
        let t = d.get("type").and_then(Value::as_str).unwrap_or("");

        if s.cwd.is_empty() {
            if let Some(c) = str_field(&d, "cwd") {
                s.cwd = c.to_string();
            }
        }
        if s.version.is_empty() {
            if let Some(v) = str_field(&d, "version") {
                s.version = v.to_string();
            }
        }
        if s.entrypoint.is_empty() {
            if let Some(e) = str_field(&d, "entrypoint") {
                s.entrypoint = e.to_string();
            }
        }
        if let Some(b) = str_field(&d, "gitBranch") {
            s.git_branch = b.to_string();
        }
        if let Some(ts) = parse_ts(d.get("timestamp")) {
            if s.started.is_none() {
                s.started = Some(ts);
            }
            if s.last_active.map_or(true, |la| ts > la) {
                s.last_active = Some(ts);
            }
        }

        // title / name entries (internal format; accept every spelling seen)
        if matches!(t, "custom-title" | "session-name" | "rename") {
            for k in ["customTitle", "sessionName", "name"] {
                if let Some(v) = str_field(&d, k) {
                    s.title = clean(v, 110);
                    s.title_source = "name";
                }
            }
        }
        if matches!(t, "ai-title" | "session-title") && s.title_source != "name" {
            for k in ["aiTitle", "title", "sessionTitle"] {
                if let Some(v) = str_field(&d, k) {
                    s.title = clean(v, 110);
                    s.title_source = "auto";
                }
            }
        }
        if t == "summary" && matches!(s.title_source, "prompt" | "summary") {
            if let Some(v) = str_field(&d, "summary") {
                s.title = clean(v, 110);
                s.title_source = "summary";
            }
        }

        if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(msg) = d.get("message").filter(|m| m.is_object()) else { continue };
        match t {
            "user" => {
                let content = msg.get("content").unwrap_or(&Value::Null);
                if has_tool_result(content) {
                    continue;
                }
                let text = text_of(content);
                if !strip_injected(&text).trim().is_empty() {
                    s.user_turns += 1;
                    let c = clean(&text, 110);
                    if s.first_prompt.is_empty() {
                        s.first_prompt = c.clone();
                    }
                    s.last_prompt = c;
                }
            }
            "assistant" => {
                s.assistant_turns += 1;
                if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                    for b in blocks {
                        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                            *s.tools.entry(name.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(s)
}

pub fn discover(cfg: &Path) -> Vec<Session> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(cfg.join("projects")) else { return out };
    for proj in dirs.flatten() {
        if !proj.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.is_file()
                && p.extension().map_or(false, |e| e == "jsonl")
                && p.file_stem().map_or(false, |st| is_uuid(&st.to_string_lossy()))
            {
                if let Some(s) = parse_session(&p) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// A Command that never flashes a console window on Windows (GUI builds).
pub fn quiet_command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        quiet_command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

pub fn mark_running(cfg: &Path, sessions: &mut [Session]) {
    let Ok(entries) = fs::read_dir(cfg.join("sessions")) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().map_or(true, |x| x != "json") {
            continue;
        }
        let Ok(txt) = fs::read_to_string(&p) else { continue };
        let Ok(d) = serde_json::from_str::<Value>(&txt) else { continue };
        let (Some(sid), Some(pid)) =
            (d.get("sessionId").and_then(Value::as_str), d.get("pid").and_then(Value::as_u64))
        else {
            continue;
        };
        if let Some(s) = sessions.iter_mut().find(|s| s.id == sid) {
            if pid_alive(pid as u32) {
                s.running_pid = Some(pid as u32);
                s.running_name = d.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            }
        }
    }
}

pub fn is_hidden_by_default(s: &Session) -> bool {
    s.user_turns == 0 || matches!(s.entrypoint.as_str(), "sdk-cli" | "sdk-ts" | "sdk-py")
}

pub fn load_sessions(cfg: &Path, include_all: bool) -> Vec<Session> {
    let mut v = discover(cfg);
    mark_running(cfg, &mut v);
    if !include_all {
        v.retain(|s| !is_hidden_by_default(s));
    }
    v.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    v
}

// ------------------------------------------------------------------------- //
// Full conversation (for preview + export)
// ------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub ts: Option<DateTime<Utc>>,
    pub text: String,
    pub tools: Vec<ToolCall>,
}

#[derive(Debug, Clone, Default)]
pub struct Conversation {
    pub turns: Vec<Turn>,
}

fn tool_summary(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| input.get(*k).and_then(Value::as_str).map(str::to_string))
    };
    let s = match name {
        "Bash" => pick(&["command"]),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => pick(&["file_path", "notebook_path"]),
        "Glob" | "Grep" => pick(&["pattern"]),
        "WebFetch" => pick(&["url"]),
        "WebSearch" => pick(&["query"]),
        "Agent" | "Task" => pick(&["description", "prompt"]),
        "Skill" => pick(&["skill"]),
        _ => input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_string))),
    };
    let s = s.unwrap_or_default();
    truncate_chars(&s.split_whitespace().collect::<Vec<_>>().join(" "), 160)
}

pub fn load_conversation(path: &Path) -> io::Result<Conversation> {
    let file = fs::File::open(path)?;
    let mut conv = Conversation::default();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(d) = serde_json::from_str::<Value>(&line) else { continue };
        if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let t = d.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(msg) = d.get("message").filter(|m| m.is_object()) else { continue };
        let ts = parse_ts(d.get("timestamp"));
        let content = msg.get("content").unwrap_or(&Value::Null);
        match t {
            "user" => {
                if has_tool_result(content) {
                    continue;
                }
                let text = strip_injected(&text_of(content)).trim().to_string();
                if text.is_empty() {
                    continue;
                }
                conv.turns.push(Turn { role: Role::User, ts, text, tools: vec![] });
            }
            "assistant" => {
                let mut text = String::new();
                let mut tools = Vec::new();
                if let Some(blocks) = content.as_array() {
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let tx = b.get("text").and_then(Value::as_str).unwrap_or("").trim();
                                if !tx.is_empty() {
                                    if !text.is_empty() {
                                        text.push_str("\n\n");
                                    }
                                    text.push_str(tx);
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
                                let summary = tool_summary(&name, b.get("input").unwrap_or(&Value::Null));
                                tools.push(ToolCall { name, summary });
                            }
                            _ => {}
                        }
                    }
                }
                if text.is_empty() && tools.is_empty() {
                    continue;
                }
                // merge consecutive assistant entries (one model turn is many API blocks)
                if let Some(last) = conv.turns.last_mut() {
                    if last.role == Role::Assistant {
                        if !text.is_empty() {
                            if !last.text.is_empty() {
                                last.text.push_str("\n\n");
                            }
                            last.text.push_str(&text);
                        }
                        last.tools.extend(tools);
                        continue;
                    }
                }
                conv.turns.push(Turn { role: Role::Assistant, ts, text, tools });
            }
            _ => {}
        }
    }
    Ok(conv)
}

// ------------------------------------------------------------------------- //
// Time formatting
// ------------------------------------------------------------------------- //

pub fn human_age(ts: Option<DateTime<Utc>>) -> String {
    let Some(ts) = ts else { return "?".into() };
    let secs = (Utc::now() - ts).num_seconds().max(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 14 => format!("{}d ago", s / 86_400),
        _ => ts.with_timezone(&Local).format("%Y-%m-%d").to_string(),
    }
}

pub fn human_size(mut n: f64) -> String {
    for unit in ["B", "KB", "MB", "GB"] {
        if n < 1024.0 {
            return format!("{n:.0} {unit}");
        }
        n /= 1024.0;
    }
    format!("{n:.0} TB")
}

pub fn fmt_local(ts: Option<DateTime<Utc>>) -> String {
    ts.map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

// ------------------------------------------------------------------------- //
// Launching
// ------------------------------------------------------------------------- //

pub fn which(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else { return false };
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    env::split_paths(&path).any(|d| exts.iter().any(|e| d.join(format!("{name}{e}")).is_file()))
}

pub fn claude_on_path() -> bool {
    which("claude")
}

/// Run `claude --resume` in the current console and wait for it.
pub fn launch_here(s: &Session) -> io::Result<i32> {
    let cwd_ok = Path::new(&s.cwd).is_dir();
    let mut cmd = if cfg!(windows) {
        let mut k = Command::new("cmd");
        k.args(["/c", "claude", "--resume", &s.id]);
        k
    } else {
        let mut k = Command::new("claude");
        k.args(["--resume", &s.id]);
        k
    };
    if cwd_ok {
        cmd.current_dir(&s.cwd);
    }
    cmd.status().map(|st| st.code().unwrap_or(0))
}

/// Open the session in a fresh terminal window. Ok(false) = no terminal found.
pub fn launch_new_window(s: &Session) -> io::Result<bool> {
    let cwd_ok = Path::new(&s.cwd).is_dir();
    let cmd = s.launch_command();
    #[cfg(windows)]
    {
        if which("wt") {
            let mut k = quiet_command("wt");
            if cwd_ok {
                k.args(["-d", &s.cwd]);
            }
            k.args(["cmd", "/k", &format!("claude --resume {}", s.id)]);
            k.spawn()?;
        } else {
            quiet_command("cmd")
                .args(["/c", "start", &format!("Claude: {}", s.project()), "cmd", "/k", &cmd])
                .spawn()?;
        }
        return Ok(true);
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Terminal\" to do script {}\ntell application \"Terminal\" to activate",
            serde_json::to_string(&cmd).unwrap()
        );
        Command::new("osascript").args(["-e", &script]).spawn()?;
        return Ok(true);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let shell_cmd = format!("{cmd}; exec $SHELL");
        let candidates: Vec<(&str, Vec<String>)> = vec![
            ("x-terminal-emulator", vec!["-e".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
            ("gnome-terminal", vec!["--".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
            ("konsole", vec!["-e".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
            ("kitty", vec!["sh".into(), "-c".into(), shell_cmd.clone()]),
            ("alacritty", vec!["-e".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
            ("wezterm", vec!["start".into(), "--".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
            ("xfce4-terminal", vec!["-e".into(), format!("sh -c {}", sh_quote(&shell_cmd))]),
            ("xterm", vec!["-e".into(), "sh".into(), "-c".into(), shell_cmd.clone()]),
        ];
        for (name, args) in candidates {
            if which(name) {
                let mut k = Command::new(name);
                k.args(&args);
                if cwd_ok {
                    k.current_dir(&s.cwd);
                }
                k.spawn()?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub fn open_folder(path: &Path) -> io::Result<()> {
    let p = path.to_string_lossy().to_string();
    if cfg!(windows) {
        quiet_command("explorer").arg(&p).spawn()?;
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(&p).spawn()?;
    } else {
        Command::new("xdg-open").arg(&p).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    }
    Ok(())
}

pub fn copy_to_clipboard(text: &str) -> bool {
    let candidates: Vec<(&str, Vec<&str>)> = if cfg!(windows) {
        vec![("clip", vec![])]
    } else if cfg!(target_os = "macos") {
        vec![("pbcopy", vec![])]
    } else {
        vec![("wl-copy", vec![]), ("xclip", vec!["-selection", "clipboard"]), ("xsel", vec!["--clipboard", "--input"])]
    };
    for (name, args) in candidates {
        if !which(name) {
            continue;
        }
        if let Ok(mut child) = quiet_command(name).args(&args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }
    false
}

pub fn to_json(s: &Session) -> Value {
    let mut tools: Vec<(&String, &usize)> = s.tools.iter().collect();
    tools.sort();
    serde_json::json!({
        "session_id": s.id,
        "path": s.path.display().to_string(),
        "cwd": s.cwd,
        "project": s.project(),
        "label": s.label(),
        "title": s.title,
        "title_source": s.title_source,
        "first_prompt": s.first_prompt,
        "last_prompt": s.last_prompt,
        "started": s.started.map(|t| t.to_rfc3339()),
        "last_active": s.last_active.map(|t| t.to_rfc3339()),
        "user_turns": s.user_turns,
        "assistant_turns": s.assistant_turns,
        "git_branch": s.git_branch,
        "version": s.version,
        "entrypoint": s.entrypoint,
        "size_bytes": s.size_bytes,
        "running_pid": s.running_pid,
        "running_name": s.running_name,
        "tools_used": tools.iter().map(|(k, v)| ((*k).clone(), Value::from(**v))).collect::<serde_json::Map<_, _>>(),
        "command": s.launch_command(),
    })
}
