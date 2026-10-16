//! Export a session's conversation as Markdown, HTML or JSON.

use crate::{fmt_local, Conversation, Role, Session};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
    Json,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Html => "html",
            Format::Json => "json",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Format::Markdown => "Markdown",
            Format::Html => "HTML",
            Format::Json => "JSON",
        }
    }
    pub const ALL: [Format; 3] = [Format::Markdown, Format::Html, Format::Json];
}

pub fn render(s: &Session, conv: &Conversation, fmt: Format) -> String {
    match fmt {
        Format::Markdown => markdown(s, conv),
        Format::Html => html(s, conv),
        Format::Json => json(s, conv),
    }
}

pub fn write_file(s: &Session, conv: &Conversation, fmt: Format, path: &Path) -> io::Result<()> {
    fs::write(path, render(s, conv, fmt))
}

fn header_lines(s: &Session) -> Vec<(&'static str, String)> {
    vec![
        ("Project folder", s.cwd.clone()),
        ("Branch", if s.git_branch.is_empty() { "-".into() } else { s.git_branch.clone() }),
        ("Started", fmt_local(s.started)),
        ("Last active", fmt_local(s.last_active)),
        ("Session id", s.id.clone()),
        ("Resume", s.launch_command()),
    ]
}

pub fn markdown(s: &Session, conv: &Conversation) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", s.label()));
    for (k, v) in header_lines(s) {
        out.push_str(&format!("- **{k}:** {}\n", if k == "Resume" { format!("`{v}`") } else { v }));
    }
    out.push('\n');
    for t in &conv.turns {
        let who = match t.role {
            Role::User => "You",
            Role::Assistant => "Claude",
        };
        out.push_str(&format!("## {who}  <sub>{}</sub>\n\n", fmt_local(t.ts)));
        if !t.text.is_empty() {
            out.push_str(&t.text);
            out.push_str("\n\n");
        }
        if !t.tools.is_empty() {
            for tc in &t.tools {
                out.push_str(&format!("> 🔧 **{}** `{}`\n", tc.name, tc.summary.replace('`', "'")));
            }
            out.push('\n');
        }
    }
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Minimal markdown-ish → HTML: fenced code blocks, inline code, paragraphs, line breaks.
fn md_to_html(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    let mut para: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<String>, out: &mut String| {
        if !para.is_empty() {
            out.push_str("<p>");
            out.push_str(&para.join("<br>"));
            out.push_str("</p>\n");
            para.clear();
        }
    };
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            flush(&mut para, &mut out);
            if in_code {
                out.push_str("</code></pre>\n");
            } else {
                out.push_str("<pre><code>");
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str(&esc(line));
            out.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut out);
            continue;
        }
        // inline code (only when backticks are balanced)
        let escaped = esc(line);
        let n = escaped.matches('`').count();
        let html = if n > 0 && n % 2 == 0 {
            escaped
                .split('`')
                .enumerate()
                .map(|(i, p)| if i % 2 == 1 { format!("<code>{p}</code>") } else { p.to_string() })
                .collect::<String>()
        } else {
            escaped
        };
        para.push(html);
    }
    flush(&mut para, &mut out);
    if in_code {
        out.push_str("</code></pre>\n");
    }
    out
}

pub fn html(s: &Session, conv: &Conversation) -> String {
    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>\n<dl class=\"meta\">\n", esc(s.label())));
    for (k, v) in header_lines(s) {
        let v = if k == "Resume" { format!("<code>{}</code>", esc(&v)) } else { esc(&v) };
        body.push_str(&format!("<dt>{k}</dt><dd>{v}</dd>\n"));
    }
    body.push_str("</dl>\n");
    for t in &conv.turns {
        let (cls, who) = match t.role {
            Role::User => ("user", "You"),
            Role::Assistant => ("assistant", "Claude"),
        };
        body.push_str(&format!(
            "<section class=\"turn {cls}\"><header><b>{who}</b><time>{}</time></header>\n",
            fmt_local(t.ts)
        ));
        if !t.text.is_empty() {
            body.push_str(&md_to_html(&t.text));
        }
        if !t.tools.is_empty() {
            body.push_str("<ul class=\"tools\">\n");
            for tc in &t.tools {
                body.push_str(&format!("<li><b>{}</b> <code>{}</code></li>\n", esc(&tc.name), esc(&tc.summary)));
            }
            body.push_str("</ul>\n");
        }
        body.push_str("</section>\n");
    }
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root{{--bg:#fafaf8;--fg:#1c1c1a;--muted:#6b6b66;--user:#eef2ff;--asst:#ffffff;--border:#e4e4df;--code:#f1f1ee;--accent:#c96442}}
@media(prefers-color-scheme:dark){{:root{{--bg:#161614;--fg:#ecebe6;--muted:#9a9a93;--user:#1f2433;--asst:#1d1d1b;--border:#2d2d2a;--code:#242421}}}}
body{{margin:0;padding:24px 16px;background:var(--bg);color:var(--fg);font:15px/1.55 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}}
main{{max-width:860px;margin:0 auto}}h1{{font-size:1.5rem;margin:0 0 12px}}
.meta{{display:grid;grid-template-columns:max-content 1fr;gap:2px 14px;font-size:.85rem;color:var(--muted);margin:0 0 28px}}.meta dt{{font-weight:600}}.meta dd{{margin:0;word-break:break-all}}
.turn{{border:1px solid var(--border);border-radius:10px;padding:12px 16px;margin:0 0 14px;background:var(--asst)}}.turn.user{{background:var(--user)}}
.turn header{{display:flex;justify-content:space-between;font-size:.8rem;color:var(--muted);margin-bottom:6px}}.turn.user header b{{color:var(--accent)}}
p{{margin:0 0 10px;white-space:pre-wrap;word-wrap:break-word}}pre{{background:var(--code);padding:10px 12px;border-radius:8px;overflow:auto;font-size:.85rem}}code{{background:var(--code);padding:1px 4px;border-radius:4px;font-size:.9em}}pre code{{padding:0;background:none}}
.tools{{list-style:none;padding:0;margin:6px 0 0;font-size:.82rem;color:var(--muted)}}.tools li{{margin:2px 0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
</style></head><body><main>
{body}
<p style="color:var(--muted);font-size:.8rem">Exported from Claude Code transcript by claude-sessions.</p>
</main></body></html>
"#,
        title = esc(s.label()),
        body = body
    )
}

pub fn json(s: &Session, conv: &Conversation) -> String {
    let turns: Vec<serde_json::Value> = conv
        .turns
        .iter()
        .map(|t| {
            serde_json::json!({
                "role": match t.role { Role::User => "user", Role::Assistant => "assistant" },
                "timestamp": t.ts.map(|x| x.to_rfc3339()),
                "text": t.text,
                "tools": t.tools.iter().map(|tc| serde_json::json!({"name": tc.name, "summary": tc.summary})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut root = crate::to_json(s);
    root["turns"] = serde_json::Value::Array(turns);
    serde_json::to_string_pretty(&root).unwrap()
}
