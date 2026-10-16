//! Terminal picker for Claude Code sessions (same core as the GUI).

use claude_sessions::export::Format;
use claude_sessions::*;
use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

static mut COLOR: bool = false;

fn init_color() {
    if env::var_os("NO_COLOR").is_some() {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode: CONSOLE_MODE = 0;
            if GetConsoleMode(h, &mut mode) == 0 || SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                return;
            }
            COLOR = true;
        }
    }
    #[cfg(not(windows))]
    {
        use std::io::IsTerminal;
        if io::stdout().is_terminal() {
            unsafe { COLOR = true };
        }
    }
}

fn is_tty_stdin() -> bool {
    use std::io::IsTerminal;
    io::stdin().is_terminal()
}

fn c(code: &str, text: &str) -> String {
    if unsafe { COLOR } { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
}

fn term_width() -> usize {
    if let Some(n) = env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return n;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::*;
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) != 0 {
                let w = (info.srWindow.Right - info.srWindow.Left + 1) as usize;
                if w > 20 {
                    return w;
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(o) = std::process::Command::new("stty").arg("size").stdin(std::process::Stdio::inherit()).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(w) = s.split_whitespace().nth(1).and_then(|x| x.parse::<usize>().ok()) {
                if w > 20 {
                    return w;
                }
            }
        }
    }
    100
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - n)) }
}

fn render_table(sessions: &[&Session]) -> String {
    if sessions.is_empty() {
        return c("33", "No sessions found.");
    }
    let w = term_width();
    let num_w = sessions.len().to_string().len();
    let proj_w = sessions.iter().map(|s| s.project().chars().count()).max().unwrap_or(7).clamp(7, 22);
    let (age_w, turns_w) = (10, 5);
    let fixed = num_w + 2 + 2 + proj_w + 1 + age_w + 1 + turns_w + 3;
    let label_w = w.saturating_sub(fixed).max(20);
    let mut lines = vec![c(
        "2",
        &format!("{}  {} {} {}   session", " ".repeat(num_w + 2), pad("project", proj_w), pad("last", age_w), pad("turns", turns_w)),
    )];
    for (i, s) in sessions.iter().enumerate() {
        let dot = if s.is_running() { c("32", "●") } else { " ".into() };
        let proj = truncate_chars(&s.project(), proj_w);
        let label = truncate_chars(s.label(), label_w);
        let label = match s.title_source {
            "name" => c("1;36", &label),
            "auto" => c("36", &label),
            _ => label,
        };
        let branch = if !s.git_branch.is_empty() && !matches!(s.git_branch.as_str(), "HEAD" | "main" | "master") {
            c("2", &format!(" ({})", s.git_branch))
        } else {
            String::new()
        };
        lines.push(format!(
            "{}  {} {} {} {}   {}{}",
            c("1", &format!("{:>num_w$}", i + 1)),
            dot,
            c("33", &pad(&proj, proj_w)),
            pad(&human_age(s.last_active), age_w),
            format!("{:>turns_w$}", s.user_turns),
            label,
            branch
        ));
    }
    lines.join("\n")
}

fn render_detail(s: &Session) -> String {
    let tools = s.top_tools(8);
    let rows: Vec<(&str, String)> = vec![
        ("Session", s.label().to_string()),
        ("Title from", s.title_source_label().into()),
        ("Folder", if s.cwd.is_empty() { "(unknown)".into() } else { s.cwd.clone() }),
        ("Branch", if s.git_branch.is_empty() { "-".into() } else { s.git_branch.clone() }),
        ("Started", fmt_local(s.started)),
        ("Last active", format!("{}  ({})", fmt_local(s.last_active), human_age(s.last_active))),
        ("Turns", format!("{} prompts / {} replies, {} on disk", s.user_turns, s.assistant_turns, human_size(s.size_bytes as f64))),
        ("First prompt", if s.first_prompt.is_empty() { "-".into() } else { s.first_prompt.clone() }),
        ("Last prompt", if s.last_prompt.is_empty() { "-".into() } else { s.last_prompt.clone() }),
        ("Tools", if tools.is_empty() { "-".into() } else { tools }),
        ("Claude Code", if s.version.is_empty() { "?".into() } else { s.version.clone() }),
        ("Running", match s.running_pid { Some(p) => format!("yes, pid {p} ({})", s.running_name), None => "no".into() }),
        ("ID", s.id.clone()),
        ("Transcript", s.path.display().to_string()),
        ("Command", s.launch_command()),
    ];
    rows.iter().map(|(k, v)| format!("  {}  {}", c("2", &format!("{k:>12}")), v)).collect::<Vec<_>>().join("\n")
}

fn do_launch_here(s: &Session) -> ! {
    if !Path::new(&s.cwd).is_dir() {
        println!("{}", c("31", &format!("Folder no longer exists: {}", s.cwd)));
        println!("Resuming from here instead (claude looks sessions up by id across projects).");
    }
    println!("{}", c("2", &format!("$ {}", s.launch_command())));
    let _ = io::stdout().flush();
    match launch_here(s) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Could not start 'claude': {e}. Is Claude Code installed and on PATH?");
            std::process::exit(1)
        }
    }
}

fn do_launch_window(s: &Session) {
    match launch_new_window(s) {
        Ok(true) => println!("{}", c("32", &format!("Opened '{}' in a new window.", s.label()))),
        Ok(false) => {
            println!("{}", c("33", "Couldn't find a terminal emulator to open. Run this instead:"));
            println!("  {}", s.launch_command());
            println!("  (or pick the same number followed by h to open it right here)");
        }
        Err(e) => println!("{}", c("31", &format!("Could not open a terminal: {e}"))),
    }
}

fn do_export(s: &Session, fmt: Format, out_dir: &Path) -> io::Result<PathBuf> {
    let conv = load_conversation(&s.path)?;
    let path = out_dir.join(format!("{}.{}", s.file_stem(), fmt.ext()));
    export::write_file(s, &conv, fmt, &path)?;
    Ok(path)
}

const HELP: &str = "
  <number>      open that session in a new terminal window
  <number> h    open it HERE, in this window
  <number> i    show details (first/last prompt, folder, tools used...)
  <number> c    copy its resume command to the clipboard
  <number> e    export the chat (markdown + html) to the current folder
  <number> b    back up that session to a zip in the current folder
  /text         filter by title, prompt, folder, branch or id  (/ alone clears)
  B             back up ALL listed sessions to a zip in the current folder
  r             rescan
  q             quit
";

fn interactive(cfg: &Path, mut sessions: Vec<Session>, initial_filter: &str, here: bool) {
    let mut needle = initial_filter.to_string();
    let stdin = io::stdin();
    loop {
        let view: Vec<&Session> = sessions.iter().filter(|s| needle.is_empty() || s.matches(&needle)).collect();
        println!();
        let mut hdr = format!("Claude Code sessions  {}", c("2", &cfg.display().to_string()));
        if needle.is_empty() {
            hdr.push_str(&format!("   {} sessions", sessions.len()));
        } else {
            hdr.push_str(&format!("   filter: {} ({}/{})", c("33", &needle), view.len(), sessions.len()));
        }
        println!("{hdr}");
        println!("{}", render_table(&view));
        println!("{}", c("2", "  ● = running.  number = launch, +i info, +e export, +b backup, /text filter, ? help, q quit"));
        print!("> ");
        let _ = io::stdout().flush();
        let mut raw = String::new();
        if stdin.lock().read_line(&mut raw).unwrap_or(0) == 0 {
            println!();
            return;
        }
        let raw = raw.trim();
        match raw {
            "" => continue,
            "q" | "quit" | "exit" => return,
            "?" => {
                println!("{HELP}");
                continue;
            }
            "r" => {
                sessions = load_sessions(cfg, false);
                continue;
            }
            "B" => {
                let out = env::current_dir().unwrap_or_default().join(backup::default_backup_name());
                match backup::backup(&view, &out) {
                    Ok(r) => println!("{}", c("32", &format!("Backed up {} sessions ({} files, {}) → {}", r.sessions, r.files, human_size(r.bytes as f64), r.path.display()))),
                    Err(e) => println!("{}", c("31", &format!("Backup failed: {e}"))),
                }
                continue;
            }
            _ => {}
        }
        if let Some(f) = raw.strip_prefix('/') {
            needle = f.trim().to_string();
            continue;
        }
        let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
        let action = raw[digits.len()..].trim();
        let (Ok(idx), true) = (digits.parse::<usize>(), matches!(action, "" | "h" | "i" | "c" | "e" | "b")) else {
            println!("{}", c("33", "Didn't understand that. Type ? for help."));
            continue;
        };
        if idx == 0 || idx > view.len() {
            println!("{}", c("33", &format!("Pick 1–{}.", view.len())));
            continue;
        }
        let s = view[idx - 1].clone();
        let cwd = env::current_dir().unwrap_or_default();
        match action {
            "i" => println!("\n{}", render_detail(&s)),
            "c" => {
                let cmd = s.launch_command();
                if copy_to_clipboard(&cmd) {
                    println!("{}{cmd}", c("32", "Copied: "));
                } else {
                    println!("{}\n  {cmd}", c("33", "No clipboard tool found. Command:"));
                }
            }
            "e" => {
                for fmt in [Format::Markdown, Format::Html] {
                    match do_export(&s, fmt, &cwd) {
                        Ok(p) => println!("{}", c("32", &format!("Exported → {}", p.display()))),
                        Err(e) => println!("{}", c("31", &format!("Export failed: {e}"))),
                    }
                }
            }
            "b" => {
                let out = cwd.join(format!("{}.zip", s.file_stem()));
                match backup::backup(&[&s], &out) {
                    Ok(r) => println!("{}", c("32", &format!("Backed up ({} files, {}) → {}", r.files, human_size(r.bytes as f64), r.path.display()))),
                    Err(e) => println!("{}", c("31", &format!("Backup failed: {e}"))),
                }
            }
            _ => {
                if s.is_running() {
                    println!("{}", c("33", &format!("That session is already open (pid {}). Switch to that window, or run `claude attach {}`.", s.running_pid.unwrap(), s.id)));
                    continue;
                }
                if action == "h" || here {
                    do_launch_here(&s);
                }
                do_launch_window(&s);
            }
        }
    }
}

const USAGE: &str = "claude-sessions-cli — find, relaunch, back up and export Claude Code sessions

USAGE: claude-sessions-cli [options]

  -s, --search TEXT        pre-filter sessions by text
      --list               print the table and exit
      --json               dump sessions as JSON and exit
      --here               launch in this window instead of a new one
      --all                include empty, print-mode and SDK sessions
      --open ID_PREFIX     skip the picker; launch the session whose id starts with this
      --export FMT DIR     export every listed session's chat (md|html|json) into DIR
      --backup FILE.zip    back up every listed session into FILE.zip
      --restore FILE.zip   restore sessions from a backup (never overwrites existing files)
      --config-dir PATH    override Claude config dir (default: $CLAUDE_CONFIG_DIR or ~/.claude)
  -h, --help               this help
";

fn pause_if_own_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;
        let mut pids = [0u32; 2];
        if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) } <= 1 {
            print!("\nPress Enter to close...");
            let _ = io::stdout().flush();
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
        }
    }
}

fn main() {
    let mut search = String::new();
    let (mut list, mut json, mut here, mut all) = (false, false, false, false);
    let mut open: Option<String> = None;
    let mut cfg_override: Option<PathBuf> = None;
    let mut export_opt: Option<(Format, PathBuf)> = None;
    let mut backup_opt: Option<PathBuf> = None;
    let mut restore_opt: Option<PathBuf> = None;
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-s" | "--search" => search = it.next().unwrap_or_default(),
            "--list" => list = true,
            "--json" => json = true,
            "--here" => here = true,
            "--all" => all = true,
            "--open" => open = it.next(),
            "--config-dir" => cfg_override = it.next().map(PathBuf::from),
            "--backup" => backup_opt = it.next().map(PathBuf::from),
            "--restore" => restore_opt = it.next().map(PathBuf::from),
            "--export" => {
                let f = match it.next().as_deref() {
                    Some("md") | Some("markdown") => Format::Markdown,
                    Some("html") => Format::Html,
                    Some("json") => Format::Json,
                    _ => {
                        eprintln!("--export needs a format: md|html|json");
                        std::process::exit(2)
                    }
                };
                export_opt = Some((f, it.next().map(PathBuf::from).unwrap_or_else(|| ".".into())));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("Unknown option: {other}\n{USAGE}");
                std::process::exit(2)
            }
        }
    }
    init_color();
    let cfg = cfg_override.unwrap_or_else(config_dir);

    if let Some(z) = restore_opt {
        match backup::restore(&z, &cfg, false) {
            Ok(r) => println!("Restored {} files ({} sessions in archive, {} already present and skipped) into {}", r.restored, r.sessions, r.skipped_existing, cfg.display()),
            Err(e) => {
                eprintln!("Restore failed: {e}");
                std::process::exit(1)
            }
        }
        return;
    }
    if !cfg.join("projects").is_dir() {
        println!("No Claude Code data at {} (no 'projects' folder). Has Claude Code been run on this machine?", cfg.display());
        pause_if_own_console();
        std::process::exit(1);
    }

    let sessions = load_sessions(&cfg, all);
    let view: Vec<&Session> = sessions.iter().filter(|s| search.is_empty() || s.matches(&search)).collect();

    if json {
        let rows: Vec<serde_json::Value> = view.iter().map(|s| to_json(s)).collect();
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return;
    }
    if let Some((fmt, dir)) = export_opt {
        let _ = std::fs::create_dir_all(&dir);
        for s in &view {
            match do_export(s, fmt, &dir) {
                Ok(p) => println!("{}", p.display()),
                Err(e) => eprintln!("{}: {e}", s.label()),
            }
        }
        return;
    }
    if let Some(out) = backup_opt {
        match backup::backup(&view, &out) {
            Ok(r) => println!("Backed up {} sessions ({} files, {}) → {}", r.sessions, r.files, human_size(r.bytes as f64), r.path.display()),
            Err(e) => {
                eprintln!("Backup failed: {e}");
                std::process::exit(1)
            }
        }
        return;
    }
    if let Some(prefix) = open {
        let p = prefix.to_lowercase();
        let hits: Vec<&Session> = sessions.iter().filter(|s| s.id.starts_with(&p)).collect();
        if hits.len() != 1 {
            println!("{} sessions match '{}'.", hits.len(), prefix);
            std::process::exit(1);
        }
        if here {
            do_launch_here(hits[0]);
        }
        do_launch_window(hits[0]);
        return;
    }
    if list || !is_tty_stdin() {
        println!("{}", render_table(&view));
        return;
    }
    if !claude_on_path() {
        println!("{}", c("33", "Note: 'claude' isn't on PATH here, so launching will fail. Listing anyway."));
    }
    interactive(&cfg, sessions, &search, here);
    pause_if_own_console();
}
