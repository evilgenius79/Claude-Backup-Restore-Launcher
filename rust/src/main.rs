//! claude-sessions GUI: find, recognise, relaunch, back up and export Claude Code sessions.
#![cfg_attr(windows, windows_subsystem = "windows")]

use claude_sessions::export::Format;
use claude_sessions::*;
use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};
use egui_extras::{Column, TableBuilder};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

const ACCENT: Color32 = Color32::from_rgb(201, 100, 66);
const GREEN: Color32 = Color32::from_rgb(70, 170, 90);
const PROJECT_COL: Color32 = Color32::from_rgb(190, 140, 50);
const TITLE_COL: Color32 = Color32::from_rgb(80, 170, 200);

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Claude Sessions")
            .with_inner_size(Vec2::new(1180.0, 720.0))
            .with_min_inner_size(Vec2::new(760.0, 440.0))
            .with_icon(make_icon()),
        ..Default::default()
    };
    eframe::run_native("claude-sessions", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// A simple 64×64 icon: accent disc with a lighter "chat bubble". No asset files needed.
fn make_icon() -> egui::IconData {
    let n = 64u32;
    let mut rgba = vec![0u8; (n * n * 4) as usize];
    let (cx, cy, r) = (31.5f32, 31.5f32, 30.0f32);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            let d = (dx * dx + dy * dy).sqrt();
            let i = ((y * n + x) * 4) as usize;
            if d <= r {
                let inner = (18..=46).contains(&x) && (22..=42).contains(&y);
                let (cr, cg, cb) = if inner { (255, 244, 236) } else { (ACCENT.r(), ACCENT.g(), ACCENT.b()) };
                let a = ((r - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
                rgba[i..i + 4].copy_from_slice(&[cr, cg, cb, a]);
            }
        }
    }
    egui::IconData { rgba, width: n, height: n }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    LastActive,
    Project,
    Turns,
    Title,
}

struct App {
    cfg: PathBuf,
    sessions: Vec<Session>,
    loading: Option<Receiver<Vec<Session>>>,
    show_all: bool,
    search: String,
    sort: Sort,
    sort_desc: bool,
    selected: Option<String>, // session id
    checked: HashSet<String>,
    conv: Option<(String, Conversation)>,
    conv_err: String,
    status: String,
    status_ok: bool,
    claude_ok: bool,
    export_fmt: Format,
    overwrite_on_restore: bool,
    confirm_restore: Option<PathBuf>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.05);
        let mut app = App {
            cfg: config_dir(),
            sessions: vec![],
            loading: None,
            show_all: false,
            search: String::new(),
            sort: Sort::LastActive,
            sort_desc: true,
            selected: None,
            checked: HashSet::new(),
            conv: None,
            conv_err: String::new(),
            status: String::new(),
            status_ok: true,
            claude_ok: claude_on_path(),
            export_fmt: Format::Html,
            overwrite_on_restore: false,
            confirm_restore: None,
        };
        app.rescan(&cc.egui_ctx);
        app
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let cfg = self.cfg.clone();
        let all = self.show_all;
        let ctx = ctx.clone();
        thread::spawn(move || {
            let v = load_sessions(&cfg, all);
            let _ = tx.send(v);
            ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.set_status("Scanning…", true);
    }

    fn set_status(&mut self, msg: impl Into<String>, ok: bool) {
        self.status = msg.into();
        self.status_ok = ok;
    }

    fn poll_loading(&mut self) {
        let Some(rx) = &self.loading else { return };
        let Ok(v) = rx.try_recv() else { return };
        let running = v.iter().filter(|s| s.is_running()).count();
        self.set_status(
            format!(
                "{} sessions in {}{}",
                v.len(),
                self.cfg.display(),
                if running > 0 { format!(" · {running} running") } else { String::new() }
            ),
            true,
        );
        self.sessions = v;
        self.loading = None;
        let ids: HashSet<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        self.checked.retain(|id| ids.contains(id));
        if self.selected.as_ref().map_or(true, |id| !ids.contains(id)) {
            self.selected = self.visible().first().map(|&i| self.sessions[i].id.clone());
            self.conv = None;
        }
    }

    fn visible(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| self.search.trim().is_empty() || self.sessions[i].matches(&self.search))
            .collect();
        let s = &self.sessions;
        idx.sort_by(|&a, &b| {
            let o = match self.sort {
                Sort::LastActive => s[a].last_active.cmp(&s[b].last_active),
                Sort::Project => s[a]
                    .project()
                    .to_lowercase()
                    .cmp(&s[b].project().to_lowercase())
                    .then(s[b].last_active.cmp(&s[a].last_active)),
                Sort::Turns => s[a].user_turns.cmp(&s[b].user_turns),
                Sort::Title => s[a].label().to_lowercase().cmp(&s[b].label().to_lowercase()),
            };
            if self.sort_desc { o.reverse() } else { o }
        });
        idx
    }

    fn selected_session(&self) -> Option<&Session> {
        self.selected.as_ref().and_then(|id| self.sessions.iter().find(|s| &s.id == id))
    }

    fn ensure_conversation(&mut self) {
        let Some(s) = self.selected_session() else { return };
        if self.conv.as_ref().map_or(false, |(id, _)| id == &s.id) {
            return;
        }
        let (id, path) = (s.id.clone(), s.path.clone());
        match load_conversation(&path) {
            Ok(c) => {
                self.conv = Some((id, c));
                self.conv_err.clear();
            }
            Err(e) => {
                self.conv = Some((id, Conversation::default()));
                self.conv_err = format!("Could not read transcript: {e}");
            }
        }
    }

    fn sort_header(&mut self, ui: &mut egui::Ui, label: &str, sort: Sort) {
        let active = self.sort == sort;
        let text = if active { format!("{label} {}", if self.sort_desc { "v" } else { "^" }) } else { label.to_string() };
        if ui.add(egui::Label::new(RichText::new(text).strong()).sense(Sense::click())).clicked() {
            if active {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort = sort;
                self.sort_desc = matches!(sort, Sort::LastActive | Sort::Turns);
            }
        }
    }

    // ---- actions ------------------------------------------------------- //

    fn launch(&mut self, s: &Session) {
        if let Some(pid) = s.running_pid {
            self.set_status(format!("'{}' is already open (pid {pid}). Switch to that window instead.", s.label()), false);
            return;
        }
        match launch_new_window(s) {
            Ok(true) => self.set_status(format!("Opened '{}' in a new terminal window.", s.label()), true),
            Ok(false) => {
                copy_to_clipboard(&s.launch_command());
                self.set_status("No terminal emulator found — resume command copied to clipboard instead.", false)
            }
            Err(e) => self.set_status(format!("Could not open a terminal: {e}"), false),
        }
    }

    fn export_one(&mut self, s: &Session, fmt: Format) {
        let Some(path) = rfd::FileDialog::new()
            .set_title(format!("Export chat as {}", fmt.label()))
            .set_file_name(format!("{}.{}", s.file_stem(), fmt.ext()))
            .add_filter(fmt.label(), &[fmt.ext()])
            .save_file()
        else {
            return;
        };
        match load_conversation(&s.path).and_then(|c| export::write_file(s, &c, fmt, &path)) {
            Ok(()) => self.set_status(format!("Exported → {}", path.display()), true),
            Err(e) => self.set_status(format!("Export failed: {e}"), false),
        }
    }

    fn export_many(&mut self, ids: &[String], fmt: Format) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title(format!("Export {} chats as {} into folder…", ids.len(), fmt.label()))
            .pick_folder()
        else {
            return;
        };
        let (mut ok, mut bad) = (0, 0);
        for id in ids {
            let Some(s) = self.sessions.iter().find(|s| &s.id == id) else { continue };
            let path = dir.join(format!("{}.{}", s.file_stem(), fmt.ext()));
            match load_conversation(&s.path).and_then(|c| export::write_file(s, &c, fmt, &path)) {
                Ok(()) => ok += 1,
                Err(_) => bad += 1,
            }
        }
        self.set_status(
            format!("Exported {ok} chats to {}{}", dir.display(), if bad > 0 { format!(" ({bad} failed)") } else { String::new() }),
            bad == 0,
        );
    }

    fn backup_ids(&mut self, ids: &[String], suggested: String) {
        let Some(path) = rfd::FileDialog::new()
            .set_title(format!("Back up {} session{}", ids.len(), if ids.len() == 1 { "" } else { "s" }))
            .set_file_name(suggested)
            .add_filter("Zip archive", &["zip"])
            .save_file()
        else {
            return;
        };
        let picked: Vec<&Session> = self.sessions.iter().filter(|s| ids.contains(&s.id)).collect();
        match backup::backup(&picked, &path) {
            Ok(r) => self.set_status(
                format!("Backed up {} sessions ({} files, {}) → {}", r.sessions, r.files, human_size(r.bytes as f64), r.path.display()),
                true,
            ),
            Err(e) => self.set_status(format!("Backup failed: {e}"), false),
        }
    }

    fn restore(&mut self, ctx: &egui::Context, zip: &Path) {
        match backup::restore(zip, &self.cfg, self.overwrite_on_restore) {
            Ok(r) => {
                self.set_status(
                    format!(
                        "Restored {} files ({} sessions in archive, {} already present{})",
                        r.restored,
                        r.sessions,
                        r.skipped_existing,
                        if self.overwrite_on_restore { ", overwritten" } else { " and skipped" }
                    ),
                    true,
                );
                self.rescan(ctx);
            }
            Err(e) => self.set_status(format!("Restore failed: {e}"), false),
        }
    }

    // ---- UI ------------------------------------------------------------ //

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("Claude Sessions").heading().color(ACCENT));
            ui.add_space(12.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search title, prompts, folder, branch, id…")
                    .desired_width(300.0),
            );
            if resp.changed() {
                let vis = self.visible();
                if !vis.iter().any(|&i| Some(&self.sessions[i].id) == self.selected.as_ref()) {
                    self.selected = vis.first().map(|&i| self.sessions[i].id.clone());
                }
            }
            if !self.search.is_empty() && ui.small_button("x").clicked() {
                self.search.clear();
            }
            if ui.button("Rescan").clicked() {
                self.rescan(ui.ctx());
            }
            if ui
                .checkbox(&mut self.show_all, "Show all")
                .on_hover_text("Include empty sessions and non-interactive (-p / SDK) runs")
                .changed()
            {
                self.rescan(ui.ctx());
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let n_checked = self.checked.len();
            let vis = self.visible();
            let target_ids: Vec<String> = if n_checked > 0 {
                self.checked.iter().cloned().collect()
            } else {
                vis.iter().map(|&i| self.sessions[i].id.clone()).collect()
            };
            let scope = if n_checked > 0 { format!("{n_checked} checked") } else { format!("all {} shown", target_ids.len()) };
            ui.label(RichText::new(format!("Bulk ({scope}):")).weak());
            if ui.add_enabled(!target_ids.is_empty(), egui::Button::new("Export chats as")).clicked() {
                let fmt = self.export_fmt;
                self.export_many(&target_ids, fmt);
            }
            egui::ComboBox::from_id_salt("fmt").selected_text(self.export_fmt.label()).width(95.0).show_ui(ui, |ui| {
                for f in Format::ALL {
                    ui.selectable_value(&mut self.export_fmt, f, f.label());
                }
            });
            ui.add_space(8.0);
            if ui
                .add_enabled(!target_ids.is_empty(), egui::Button::new("Back up to zip..."))
                .on_hover_text("Raw transcripts + sidechains, restorable later")
                .clicked()
            {
                self.backup_ids(&target_ids, backup::default_backup_name());
            }
            if n_checked > 0 && ui.small_button("clear checks").clicked() {
                self.checked.clear();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                if ui.button("Restore backup...").on_hover_text("Restore sessions from a zip made by this tool").clicked() {
                    if let Some(z) = rfd::FileDialog::new()
                        .set_title("Restore sessions from backup")
                        .add_filter("Zip archive", &["zip"])
                        .pick_file()
                    {
                        self.confirm_restore = Some(z);
                    }
                }
            });
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        // labels must not swallow clicks, or rows can't be selected
        ui.style_mut().interaction.selectable_labels = false;
        let vis = self.visible();
        let mut clicked: Option<(usize, bool)> = None; // (session index, double)
        let mut toggle: Option<usize> = None;
        let mut toggle_all: Option<bool> = None;
        let all_checked = !vis.is_empty() && vis.iter().all(|i| self.checked.contains(&self.sessions[*i].id));

        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(22.0))
            .column(Column::exact(14.0))
            .column(Column::initial(150.0).at_least(60.0).clip(true).resizable(true))
            .column(Column::exact(80.0))
            .column(Column::exact(48.0))
            .column(Column::remainder().clip(true))
            .min_scrolled_height(0.0)
            .header(24.0, |mut h| {
                h.col(|ui| {
                    let mut v = all_checked;
                    if ui.checkbox(&mut v, "").on_hover_text("Check all shown").changed() {
                        toggle_all = Some(v);
                    }
                });
                h.col(|_| {});
                h.col(|ui| self.sort_header(ui, "Project", Sort::Project));
                h.col(|ui| self.sort_header(ui, "Last", Sort::LastActive));
                h.col(|ui| self.sort_header(ui, "Turns", Sort::Turns));
                h.col(|ui| self.sort_header(ui, "Session", Sort::Title));
            })
            .body(|body| {
                body.rows(26.0, vis.len(), |mut row| {
                    let i = vis[row.index()];
                    let s = &self.sessions[i];
                    row.set_selected(self.selected.as_deref() == Some(&s.id));
                    row.col(|ui| {
                        let mut v = self.checked.contains(&s.id);
                        if ui.checkbox(&mut v, "").changed() {
                            toggle = Some(i);
                        }
                    });
                    row.col(|ui| {
                        if let Some(pid) = s.running_pid {
                            let (r, resp) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 4.0, GREEN);
                            resp.on_hover_text(format!("Running (pid {pid})"));
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(s.project()).color(PROJECT_COL));
                    });
                    row.col(|ui| {
                        ui.label(human_age(s.last_active)).on_hover_text(fmt_local(s.last_active));
                    });
                    row.col(|ui| {
                        ui.label(s.user_turns.to_string());
                    });
                    row.col(|ui| {
                        let t = match s.title_source {
                            "name" => RichText::new(s.label()).strong().color(TITLE_COL),
                            "auto" => RichText::new(s.label()).color(TITLE_COL),
                            _ => RichText::new(s.label()),
                        };
                        ui.add(egui::Label::new(t).truncate());
                        if !s.git_branch.is_empty() && !matches!(s.git_branch.as_str(), "HEAD" | "main" | "master") {
                            ui.label(RichText::new(format!("({})", s.git_branch)).weak().small());
                        }
                    });
                    let r = row.response();
                    if r.double_clicked() {
                        clicked = Some((i, true));
                    } else if r.clicked() {
                        clicked = Some((i, false));
                    }
                });
            });

        if let Some(v) = toggle_all {
            for &i in &vis {
                let id = self.sessions[i].id.clone();
                if v {
                    self.checked.insert(id);
                } else {
                    self.checked.remove(&id);
                }
            }
        }
        if let Some(i) = toggle {
            let id = self.sessions[i].id.clone();
            if !self.checked.remove(&id) {
                self.checked.insert(id);
            }
        }
        if let Some((i, dbl)) = clicked {
            self.selected = Some(self.sessions[i].id.clone());
            if dbl {
                let s = self.sessions[i].clone();
                self.launch(&s);
            }
        }
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.selected_session().cloned() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a session").weak());
            });
            return;
        };
        self.ensure_conversation();

        egui::ScrollArea::vertical().id_salt("detail").auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(s.label()).heading()).wrap());
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(s.project()).color(PROJECT_COL).strong());
                ui.label(
                    RichText::new(format!("· {} · {} prompts · {}", human_age(s.last_active), s.user_turns, human_size(s.size_bytes as f64)))
                        .weak(),
                );
                if s.is_running() {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, GREEN);
                    ui.label(RichText::new("running").color(GREEN));
                }
            });
            ui.add_space(8.0);

            ui.horizontal_wrapped(|ui| {
                let launch_btn = egui::Button::new(RichText::new("  Launch  ").strong().color(Color32::WHITE)).fill(ACCENT);
                if ui
                    .add_enabled(!s.is_running(), launch_btn)
                    .on_hover_text("Open in a new terminal window, in the project folder")
                    .clicked()
                {
                    self.launch(&s);
                }
                if ui.button("Copy command").on_hover_text(s.launch_command()).clicked() {
                    ui.ctx().copy_text(s.launch_command());
                    self.set_status("Resume command copied to clipboard.", true);
                }
                if ui.button("Open folder").clicked() {
                    let p = PathBuf::from(&s.cwd);
                    if p.is_dir() {
                        let _ = open_folder(&p);
                    } else {
                        self.set_status(format!("Folder no longer exists: {}", s.cwd), false);
                    }
                }
                let mut export_pick: Option<Format> = None;
                ui.menu_button("Export chat...", |ui| {
                    for f in Format::ALL {
                        if ui.button(format!("as {}…", f.label())).clicked() {
                            export_pick = Some(f);
                            ui.close();
                        }
                    }
                });
                if let Some(f) = export_pick {
                    self.export_one(&s, f);
                }
                if ui
                    .button("Back up…")
                    .on_hover_text("Zip this session's raw transcript (resumable after restore)")
                    .clicked()
                {
                    self.backup_ids(&[s.id.clone()], format!("{}.zip", s.file_stem()));
                }
            });
            ui.add_space(8.0);

            egui::Grid::new("meta").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
                let row = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).weak());
                    ui.add(egui::Label::new(RichText::new(v).monospace().small()).wrap());
                    ui.end_row();
                };
                row(ui, "Folder", if s.cwd.is_empty() { "(unknown)".into() } else { s.cwd.clone() });
                row(ui, "Branch", if s.git_branch.is_empty() { "-".into() } else { s.git_branch.clone() });
                row(ui, "Started", fmt_local(s.started));
                row(ui, "Last active", fmt_local(s.last_active));
                row(ui, "Title from", s.title_source_label().into());
                let tools = s.top_tools(6);
                row(ui, "Tools", if tools.is_empty() { "-".into() } else { tools });
                row(ui, "Claude Code", if s.version.is_empty() { "?".into() } else { s.version.clone() });
                row(ui, "Session id", s.id.clone());
            });
            ui.add_space(10.0);
            ui.separator();

            if !self.conv_err.is_empty() {
                ui.colored_label(Color32::RED, &self.conv_err);
            }
            if let Some((_, conv)) = &self.conv {
                ui.label(RichText::new(format!("Conversation · {} turns", conv.turns.len())).strong());
                ui.add_space(4.0);
                let dark = ui.visuals().dark_mode;
                let user_fill = if dark { Color32::from_rgb(34, 40, 58) } else { Color32::from_rgb(236, 240, 255) };
                let asst_fill = if dark { Color32::from_rgb(34, 34, 32) } else { Color32::from_rgb(252, 252, 250) };
                let border = ui.visuals().widgets.noninteractive.bg_stroke.color;
                let text_col = ui.visuals().text_color();
                for t in &conv.turns {
                    let (who, fill) = match t.role {
                        Role::User => ("You", user_fill),
                        Role::Assistant => ("Claude", asst_fill),
                    };
                    egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(who).strong().color(if t.role == Role::User { ACCENT } else { text_col }));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(fmt_local(t.ts)).weak().small());
                            });
                        });
                        if !t.text.is_empty() {
                            ui.add(egui::Label::new(truncate_chars(&t.text, 1500)).wrap().selectable(true));
                        }
                        if !t.tools.is_empty() {
                            ui.add_space(2.0);
                            for tc in t.tools.iter().take(12) {
                                ui.add(
                                    egui::Label::new(RichText::new(format!("> {}  {}", tc.name, tc.summary)).weak().small().monospace())
                                        .truncate(),
                                );
                            }
                            if t.tools.len() > 12 {
                                ui.label(RichText::new(format!("… {} more tool calls", t.tools.len() - 12)).weak().small());
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
            }
        });
    }

    fn restore_dialog(&mut self, ctx: &egui::Context) {
        let Some(zip) = self.confirm_restore.clone() else { return };
        let mut open = true;
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Restore sessions").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.label(format!("Restore sessions from\n{}\ninto\n{}", zip.display(), self.cfg.display()));
            ui.add_space(6.0);
            ui.checkbox(&mut self.overwrite_on_restore, "Overwrite sessions that already exist");
            ui.label(RichText::new("Restored sessions become resumable with `claude --resume` again.").weak().small());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Restore").strong()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if go {
            self.confirm_restore = None;
            self.restore(ctx, &zip);
        } else if cancel || !open {
            self.confirm_restore = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_loading();
        if self.loading.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            self.top_bar(ui);
            ui.add_space(6.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let col = if self.status_ok { ui.visuals().weak_text_color() } else { Color32::from_rgb(220, 90, 70) };
                ui.add(egui::Label::new(RichText::new(&self.status).color(col).small()).truncate());
                if !self.claude_ok {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("'claude' not found on PATH - launching will fail").color(Color32::from_rgb(220, 160, 50)).small());
                    });
                }
            });
        });

        egui::Panel::right("detail").resizable(true).default_size(470.0).min_size(300.0).show(ui, |ui| {
            self.detail_panel(ui);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.sessions.is_empty() && self.loading.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(format!("No sessions found in {}\n\nHas Claude Code been run on this machine?", self.cfg.display())).weak(),
                    );
                });
            } else {
                self.table(ui);
            }
        });

        self.restore_dialog(ui.ctx());
    }
}
