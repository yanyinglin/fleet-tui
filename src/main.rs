//! fleet: a read-only dashboard of every tmux session that runs a coding agent.
//! It reads with `tmux list-panes` and `tmux capture-pane` and never calls a model. It never sends keys into a session.
//! Two actions change anything, both started by the person at the keyboard: Enter attaches to a session (detach returns
//! here), x kills a session after a typed confirmation, and n renames a session.
//!
//! State rules (checked against real panes, 2026-11-21):
//!   ASK   a selection dialog is on screen (`❯ 1. ...` option list, or an "Enter to select/confirm" footer).
//!   WORK  the newest spinner line is the live form `✻ Verb… (12s · ...)` rather than the finished form
//!         `✻ Verbed for 12s`, or a listed subagent's timer moved since the last refresh.
//!         A subagent list alone is not work: finished subagents stay listed with a frozen timer for days.
//!   idle  an agent pane with neither; sh = no agent in the pane.
use std::{
    collections::HashMap,
    io,
    process::Command,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*};
use regex::Regex;

/// Local settings, read once from `~/.config/fleet/config` (`key=value` lines) and overridden by environment variables.
///   pinned    / FLEET_PINNED     comma-separated session names shown as status lights in the header and marked in the table
///   workspace / FLEET_WORKSPACE  directory prefix shortened to `~w` in the TASK column
///   rate_file / FLEET_RATE_FILE  JSON file with the 5-hour quota reading (default `~/.claude/rate-limits.json`)
struct Config {
    pinned: Vec<String>,
    workspace: String,
    rate_file: String,
    home: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

fn cfg() -> &'static Config {
    CONFIG.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut kv: HashMap<String, String> = HashMap::new();
        if let Ok(txt) = std::fs::read_to_string(format!("{home}/.config/fleet/config")) {
            for l in txt.lines().filter(|l| !l.trim_start().starts_with('#')) {
                if let Some((k, v)) = l.split_once('=') {
                    kv.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        let get = |key: &str, env: &str| std::env::var(env).ok().or_else(|| kv.get(key).cloned()).unwrap_or_default();
        let rate = get("rate_file", "FLEET_RATE_FILE");
        Config {
            pinned: get("pinned", "FLEET_PINNED").split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
            workspace: get("workspace", "FLEET_WORKSPACE").trim_end_matches('/').to_string(),
            rate_file: if rate.is_empty() { format!("{home}/.claude/rate-limits.json") } else { rate },
            home,
        }
    })
}

fn pinned(name: &str) -> bool {
    cfg().pinned.iter().any(|p| p == name)
}

fn short_dir(path: &str) -> String {
    let c = cfg();
    if !c.workspace.is_empty() && path.starts_with(&c.workspace) {
        return path.replacen(&c.workspace, "~w", 1);
    }
    if !c.home.is_empty() && path.starts_with(&c.home) {
        return path.replacen(&c.home, "~", 1);
    }
    path.to_string()
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum St {
    Ask,
    Work,
    Idle,
    Shell,
}

#[derive(Clone)]
struct Row {
    st: St,
    idle: u64,
    name: String,
    pane: String,
    cwd: String,
    task: String,
    model: String,
    ctx: Option<u16>,
    last: String,
    subs: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Active,
    Busy,
    All,
}

struct Rx {
    ask: Regex,
    sub: Regex,
    live: Regex,
    done: Regex,
    ctx: Regex,
    frame: Regex,
}

struct Usage {
    pct: Option<f64>,
    resets_in: Option<u64>,
    age: Option<u64>,
}

struct App {
    rx: Rx,
    rows: Vec<Row>,
    shown: Vec<usize>,
    table: TableState,
    view: View,
    query: String,
    typing: bool,
    preview: Vec<String>,
    preview_of: String,
    usage: Usage,
    refreshed: Instant,
    previewed: Instant,
    confirm: Option<String>,
    rename: Option<(String, String)>,
    prev_subs: HashMap<String, String>,
    note: String,
    own: String,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sh(args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn age(s: u64) -> String {
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

fn capture(pane: &str, lines: u32) -> Vec<String> {
    let start = format!("-{lines}");
    sh(&["tmux", "capture-pane", "-p", "-t", pane, "-S", &start])
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn read_usage() -> Usage {
    let mut u = Usage { pct: None, resets_in: None, age: None };
    if let Ok(txt) = std::fs::read_to_string(&cfg().rate_file) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            let t = now();
            u.pct = v["five_hour"]["used_percentage"].as_f64();
            u.resets_in = v["five_hour"]["resets_at"].as_u64().map(|r| r.saturating_sub(t));
            u.age = v["written_at"].as_u64().map(|w| t.saturating_sub(w));
        }
    }
    u
}

fn collect(rx: &Rx, prev_subs: &mut HashMap<String, String>) -> Vec<Row> {
    let fmt = "#{session_name}\t#{window_activity}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_id}";
    let t = now();
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for line in sh(&["tmux", "list-panes", "-a", "-F", fmt]).lines() {
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 5 || !seen.insert(p[0].to_string()) {
            continue;
        }
        let idle = t.saturating_sub(p[1].parse::<u64>().unwrap_or(0));
        let pane = capture(p[4], 40);
        let tail: Vec<&String> = pane.iter().rev().take(25).collect();
        let text = tail.iter().rev().map(|s| s.as_str()).collect::<Vec<_>>().join("\n");
        let agent = matches!(p[2], "claude" | "node" | "codex" | "cursor-agent")
            || text.contains("bypass permissions")
            || text.contains("? for shortcuts")
            || text.contains("auto mode on");
        let (mut ctx, mut task, mut model) = (None, String::new(), String::new());
        if let Some(sl) = pane.iter().rev().find(|l| rx.ctx.is_match(l)) {
            ctx = rx.ctx.captures(sl).and_then(|c| c[1].parse().ok());
            let f: Vec<&str> = sl.split('|').map(|x| x.trim()).collect();
            model = f.first().unwrap_or(&"").to_string();
            task = f.get(3).unwrap_or(&"").to_string();
        }
        let subs: Vec<&str> = tail.iter().rev().filter(|l| rx.sub.is_match(l)).map(|l| l.trim()).collect();
        // newest spinner line decides: live form means generating, finished form means the turn ended
        let spin = tail.iter().find(|l| rx.live.is_match(l) || rx.done.is_match(l));
        let live = spin.is_some_and(|l| rx.live.is_match(l)) || text.contains("esc to interrupt");
        // a subagent counts only if its line (timer, tokens) changed since the previous refresh
        let sub_key = subs.join("\n");
        let ticking = !subs.is_empty() && prev_subs.get(p[0]).is_some_and(|old| *old != sub_key);
        prev_subs.insert(p[0].to_string(), sub_key);
        let st = if rx.ask.is_match(&text) {
            St::Ask
        } else if live || ticking {
            St::Work
        } else if agent {
            St::Idle
        } else {
            St::Shell
        };
        let skip = ["bypass permissions", "for shortcuts", "auto mode on", "Update installed", "| Ctx:"];
        let body: Vec<&str> = pane
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !rx.frame.is_match(l) && !skip.iter().any(|k| l.contains(k)) && !l.starts_with('>'))
            .filter(|l| ticking || !(rx.sub.is_match(l) || *l == "● main" || *l == "◯ main"))
            .collect();
        let said = body.iter().rev().find(|l| l.starts_with('●') || l.starts_with('•') || l.starts_with('⏺'));
        let last = (if ticking { subs.last().copied() } else { None })
            .or(said.copied())
            .or(body.last().copied())
            .unwrap_or("")
            .to_string();
        rows.push(Row {
            st,
            idle,
            name: p[0].to_string(),
            pane: p[4].to_string(),
            cwd: short_dir(p[3]),
            task,
            model,
            ctx,
            last,
            subs: subs.len(),
        });
    }
    rows.sort_by(|a, b| (a.st, a.idle).cmp(&(b.st, b.idle)));
    rows
}

impl App {
    fn new() -> Self {
        let rx = Rx {
            ask: Regex::new(r"(?m)^\s*❯\s+\d+\.\s|Enter to (select|confirm)|\(y/n\)\s*$").unwrap(),
            live: Regex::new(r"^\s*[^\w\s❯◯●>]\s+\S+…(\s+\(|\s*$)").unwrap(),
            done: Regex::new(r"^\s*[^\w\s❯◯●>]\s+\S+ for (\d+[dhm] )*\d+[dhms]").unwrap(),
            sub: Regex::new(r"^\s*[◯●◐◑◒◓]\s+\S+\s{2,}").unwrap(),
            ctx: Regex::new(r"\| Ctx:(\d+)%").unwrap(),
            frame: Regex::new(r"^[\s─━╭╰│❯⏵]*$|^[╭╰]").unwrap(),
        };
        let mut a = App {
            rx,
            rows: vec![],
            shown: vec![],
            table: TableState::default(),
            view: View::Active,
            query: String::new(),
            typing: false,
            preview: vec![],
            preview_of: String::new(),
            usage: read_usage(),
            refreshed: Instant::now(),
            previewed: Instant::now(),
            confirm: None,
            rename: None,
            prev_subs: HashMap::new(),
            note: String::new(),
            own: if std::env::var_os("TMUX").is_some() { sh(&["tmux", "display-message", "-p", "#S"]).trim().to_string() } else { String::new() },
        };
        a.refresh();
        a
    }

    fn selected(&self) -> Option<&Row> {
        self.table.selected().and_then(|i| self.shown.get(i)).map(|&i| &self.rows[i])
    }

    fn refilter(&mut self, keep: Option<String>) {
        let q = self.query.to_lowercase();
        self.shown = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                let by_view = match self.view {
                    View::All => true,
                    View::Busy => matches!(r.st, St::Ask | St::Work),
                    View::Active => r.st == St::Ask || (r.st != St::Shell && r.idle <= 86_400),
                };
                by_view
                    && (q.is_empty()
                        || r.name.to_lowercase().contains(&q)
                        || r.task.to_lowercase().contains(&q)
                        || r.last.to_lowercase().contains(&q))
            })
            .collect();
        let pos = keep.and_then(|n| self.shown.iter().position(|&i| self.rows[i].name == n));
        let sel = pos.or(if self.shown.is_empty() { None } else { Some(self.table.selected().unwrap_or(0).min(self.shown.len() - 1)) });
        self.table.select(sel);
        self.load_preview();
    }

    fn refresh(&mut self) {
        let keep = self.selected().map(|r| r.name.clone());
        self.rows = collect(&self.rx, &mut self.prev_subs);
        self.usage = read_usage();
        self.refreshed = Instant::now();
        self.refilter(keep);
    }

    fn load_preview(&mut self) {
        match self.selected().map(|r| (r.name.clone(), r.pane.clone())) {
            Some((name, pane)) => {
                self.preview = capture(&pane, 200);
                while self.preview.last().is_some_and(|l| l.trim().is_empty()) {
                    self.preview.pop();
                }
                self.preview_of = name;
            }
            None => {
                self.preview.clear();
                self.preview_of.clear();
            }
        }
        self.previewed = Instant::now();
    }

    fn step(&mut self, d: i64) {
        if self.shown.is_empty() {
            return;
        }
        let n = self.shown.len() as i64;
        let i = (self.table.selected().unwrap_or(0) as i64 + d).clamp(0, n - 1);
        self.table.select(Some(i as usize));
        self.load_preview();
    }
}

/// Attach to a session and come back when the person detaches (prefix + d).
/// Inside tmux a nested attach is refused, so the session opens in a popup over the dashboard instead.
fn attach(name: &str) -> String {
    let target = format!("={name}");
    let ok = if std::env::var_os("TMUX").is_some() {
        let inner = format!("env -u TMUX tmux attach-session -t '{}'", target.replace('\'', ""));
        Command::new("tmux").args(["display-popup", "-E", "-w", "96%", "-h", "96%", &inner]).status()
    } else {
        Command::new("tmux").args(["attach-session", "-t", &target]).status()
    };
    match ok {
        Ok(s) if s.success() => format!("left {name}"),
        Ok(_) => format!("could not attach to {name}"),
        Err(e) => format!("tmux failed: {e}"),
    }
}

fn kill(name: &str) -> String {
    let target = format!("={name}");
    match Command::new("tmux").args(["kill-session", "-t", &target]).status() {
        Ok(s) if s.success() => format!("killed {name}"),
        _ => format!("could not kill {name}"),
    }
}

/// Rename a session. tmux itself turns '.' and ':' into '_', so those are refused up front to keep the name predictable.
fn rename(old: &str, new: &str, taken: bool) -> Result<String, String> {
    if new.is_empty() {
        return Err("name is empty".into());
    }
    if new == old {
        return Err("name unchanged".into());
    }
    if new.chars().any(|c| c == '.' || c == ':' || c.is_whitespace() || c.is_control()) {
        return Err("no spaces, '.' or ':' in a session name".into());
    }
    if taken {
        return Err(format!("a session named {new} already exists"));
    }
    let target = format!("={old}");
    match Command::new("tmux").args(["rename-session", "-t", &target, new]).status() {
        Ok(s) if s.success() => Ok(format!("renamed {old} to {new}")),
        _ => Err(format!("tmux could not rename {old}")),
    }
}

fn st_cell(r: &Row) -> Cell<'static> {
    let (txt, style) = match r.st {
        St::Ask => ("▲ ASK ", Style::new().fg(Color::Black).bg(Color::LightRed).bold()),
        St::Work => ("● WORK", Style::new().fg(Color::LightGreen).bold()),
        St::Idle => ("○ idle", Style::new().fg(Color::DarkGray)),
        St::Shell => ("  sh  ", Style::new().fg(Color::DarkGray)),
    };
    Cell::from(txt).style(style)
}

fn ctx_cell(c: Option<u16>) -> Cell<'static> {
    match c {
        None => Cell::from(""),
        Some(p) => {
            let fill = ((p as usize).min(100) * 8 + 50) / 100;
            let color = if p >= 45 { Color::LightRed } else if p >= 30 { Color::Yellow } else { Color::Cyan };
            Cell::from(Line::from(vec![
                Span::styled("█".repeat(fill), Style::new().fg(color)),
                Span::styled("░".repeat(8 - fill), Style::new().fg(Color::DarkGray)),
                Span::raw(format!(" {p:>3}%")),
            ]))
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let [head, body, foot] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(8), Constraint::Length(1)]).areas(f.area());
    let [list, prev] = Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(body);
    let [h_use, h_cnt] = Layout::horizontal([Constraint::Length(46), Constraint::Min(20)]).areas(head);

    // header: 5h quota gauge
    let u = &app.usage;
    let stale = u.age.map_or(true, |a| a > 900);
    let pct = u.pct.unwrap_or(0.0);
    let gcol = if stale { Color::DarkGray } else if pct >= 85.0 { Color::LightRed } else if pct >= 60.0 { Color::Yellow } else { Color::LightGreen };
    let label = match (u.pct, stale) {
        (Some(p), false) => format!("{p:.0}% used · resets in {}", u.resets_in.map(age).unwrap_or_else(|| "?".into())),
        (Some(p), true) => format!("{p:.0}% (stale reading, {} old)", u.age.map(age).unwrap_or_else(|| "?".into())),
        _ => "unknown".into(),
    };
    f.render_widget(
        Gauge::default()
            .block(Block::bordered().title(" Claude 5h quota "))
            .gauge_style(Style::new().fg(gcol).bg(Color::Black))
            .ratio((pct / 100.0).clamp(0.0, 1.0))
            .label(label),
        h_use,
    );

    // header: counts
    let cnt = |s: St| app.rows.iter().filter(|r| r.st == s).count();
    let orch: Vec<Span> = cfg()
        .pinned
        .iter()
        .flat_map(|o| {
            let st = app.rows.iter().find(|r| r.name == *o).map(|r| r.st);
            let col = match st {
                Some(St::Ask) => Color::LightRed,
                Some(St::Work) => Color::LightGreen,
                Some(_) => Color::Gray,
                None => Color::DarkGray,
            };
            let mark = if st.is_none() { "✗" } else { "◆" };
            vec![Span::styled(format!("{mark} {o}"), Style::new().fg(col)), Span::raw("  ")]
        })
        .collect();
    let mut top = vec![
        Span::styled(format!(" ▲ {} waiting ", cnt(St::Ask)), Style::new().fg(Color::LightRed).bold()),
        Span::styled(format!(" ● {} working ", cnt(St::Work)), Style::new().fg(Color::LightGreen).bold()),
        Span::styled(format!(" ○ {} idle ", cnt(St::Idle)), Style::new().fg(Color::Gray)),
        Span::styled(format!("  {} sessions", app.rows.len()), Style::new().fg(Color::DarkGray)),
    ];
    if stale {
        top.push(Span::styled("   quota reading stale", Style::new().fg(Color::Yellow)));
    }
    f.render_widget(
        Paragraph::new(vec![Line::from(top), Line::from(orch)]).block(Block::bordered().title(" fleet ")),
        h_cnt,
    );

    // session table
    let rows: Vec<ratatui::widgets::Row> = app
        .shown
        .iter()
        .map(|&i| {
            let r = &app.rows[i];
            let is_orch = pinned(&r.name);
            let name_style = if is_orch { Style::new().fg(Color::LightMagenta).bold() } else { Style::new().bold() };
            let dim = r.st == St::Idle && r.idle > 3600;
            let last_style = match r.st {
                St::Ask => Style::new().fg(Color::LightRed),
                St::Work => Style::new().fg(Color::White),
                _ => Style::new().fg(Color::Gray),
            };
            let task = if r.task.is_empty() { r.cwd.clone() } else { r.task.clone() };
            let row = ratatui::widgets::Row::new(vec![
                st_cell(r),
                Cell::from(Line::from(age(r.idle)).alignment(Alignment::Right)),
                ctx_cell(r.ctx),
                Cell::from(format!("{}{}", if is_orch { "◆ " } else { "  " }, r.name)).style(name_style),
                Cell::from(task).style(Style::new().fg(Color::Cyan)),
                Cell::from(r.last.clone()).style(last_style),
            ]);
            if dim { row.style(Style::new().add_modifier(Modifier::DIM)) } else { row }
        })
        .collect();
    let view = match app.view {
        View::Active => "active 24h",
        View::Busy => "waiting + working",
        View::All => "all",
    };
    let title = if app.query.is_empty() && !app.typing {
        format!(" sessions · {view} · {} shown ", app.shown.len())
    } else {
        format!(" sessions · {view} · /{}{} · {} shown ", app.query, if app.typing { "▏" } else { "" }, app.shown.len())
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Length(13),
            Constraint::Length(26),
            Constraint::Length(34),
            Constraint::Min(20),
        ],
    )
    .header(
        ratatui::widgets::Row::new(["STATE", " IDLE", "CONTEXT", "  SESSION", "TASK", "LAST"])
            .style(Style::new().fg(Color::DarkGray).bold()),
    )
    .column_spacing(2)
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)))
    .highlight_symbol("▌")
    .block(Block::bordered().title(title));
    f.render_stateful_widget(table, list, &mut app.table);

    // preview of the selected pane
    let h = prev.height.saturating_sub(2) as usize;
    let from = app.preview.len().saturating_sub(h);
    let lines: Vec<Line> = app.preview[from..]
        .iter()
        .map(|l| {
            let t = l.trim_start();
            let style = if t.starts_with('●') || t.starts_with('⏺') || t.starts_with('•') {
                Style::new().fg(Color::White)
            } else if t.starts_with('❯') || t.starts_with('>') {
                Style::new().fg(Color::LightYellow)
            } else {
                Style::new().fg(Color::Gray)
            };
            Line::styled(l.clone(), style)
        })
        .collect();
    let ptitle = match app.selected() {
        Some(r) => format!(
            " {} · {}{} · {} ",
            r.name,
            if r.model.is_empty() { "agent".to_string() } else { r.model.clone() },
            if r.subs > 0 { format!(" · {} subagent(s) listed", r.subs) } else { String::new() },
            r.cwd
        ),
        None => " no session selected ".into(),
    };
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(ptitle)), prev);

    let help = if app.typing {
        " type to filter · Enter keep · Esc clear ".to_string()
    } else {
        format!(
            " ↑↓/jk move · Enter open (detach = back) · n rename · x kill · / filter · v view · r refresh · q quit    {}s ago  {}",
            app.refreshed.elapsed().as_secs(),
            app.note
        )
    };
    f.render_widget(Paragraph::new(help).style(Style::new().fg(Color::DarkGray)), foot);

    if let Some((old, buf)) = &app.rename {
        let mut lines = vec![
            Line::from(vec![Span::raw("Rename tmux session "), Span::styled(old.clone(), Style::new().bold().fg(Color::LightYellow))]),
            Line::raw(""),
            Line::from(vec![Span::raw("new name: "), Span::styled(format!("{buf}▏"), Style::new().bold().fg(Color::White))]),
        ];
        if pinned(old) {
            lines.push(Line::styled("Pinned session: scripts and other agents may address it by this name.", Style::new().fg(Color::LightRed)));
        } else {
            lines.push(Line::styled("Scripts that address this session by name will need the new one.", Style::new().fg(Color::DarkGray)));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" Enter ", Style::new().fg(Color::Black).bg(Color::LightGreen).bold()),
            Span::raw(" rename     "),
            Span::styled(" Esc ", Style::new().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" cancel"),
        ]));
        let w = 76.min(f.area().width.saturating_sub(4));
        let hgt = lines.len() as u16 + 2;
        let area = Rect::new((f.area().width - w) / 2, f.area().height.saturating_sub(hgt) / 2, w, hgt.min(f.area().height));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" rename ").border_style(Style::new().fg(Color::LightGreen))), area);
    }

    if let Some(name) = &app.confirm {
        let r = app.rows.iter().find(|r| &r.name == name);
        let busy = r.is_some_and(|r| matches!(r.st, St::Work | St::Ask));
        let orch = pinned(name);
        let mut lines = vec![
            Line::from(vec![Span::raw("End tmux session "), Span::styled(name.clone(), Style::new().bold().fg(Color::LightYellow)), Span::raw(" ?")]),
            Line::raw(""),
            Line::raw("Stops the agent and every process in it. Cannot be undone."),
        ];
        if busy {
            lines.push(Line::styled("It is working or waiting on a prompt right now.", Style::new().fg(Color::LightRed)));
        }
        if orch {
            lines.push(Line::styled("It is a pinned session: agents that depend on it lose it.", Style::new().fg(Color::LightRed)));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" Y ", Style::new().fg(Color::Black).bg(Color::LightRed).bold()),
            Span::raw(" end it (capital Y)     "),
            Span::styled(" any other key ", Style::new().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" cancel"),
        ]));
        let w = 76.min(f.area().width.saturating_sub(4));
        let hgt = lines.len() as u16 + 3;
        let area = Rect::new((f.area().width - w) / 2, f.area().height.saturating_sub(hgt) / 2, w, hgt.min(f.area().height));
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().title(" confirm ").border_style(Style::new().fg(Color::LightRed))),
            area,
        );
    }
}

fn main() -> io::Result<()> {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        println!("fleet: read-only dashboard of tmux agent sessions.\nKeys: arrows or j/k move, Enter open a session (detach with prefix+d to come back), n rename a session, x end a session (asks first),\n/ filter, v cycle view, r refresh, q quit.\nSettings: ~/.config/fleet/config or FLEET_PINNED, FLEET_WORKSPACE, FLEET_RATE_FILE (see README).\nPlain-text version: fleet.sh in the repository.");
        return Ok(());
    }
    let mut app = App::new();
    let mut term = ratatui::init();
    loop {
        term.draw(|f| draw(f, &mut app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(name) = app.confirm.take() {
                    app.note = if k.code == KeyCode::Char('Y') { kill(&name) } else { "cancelled".into() };
                    app.refresh();
                    continue;
                }
                if let Some((old, mut buf)) = app.rename.take() {
                    match k.code {
                        KeyCode::Esc => app.note = "rename cancelled".into(),
                        KeyCode::Enter => {
                            let new = buf.trim().to_string();
                            let taken = app.rows.iter().any(|r| r.name == new);
                            match rename(&old, &new, taken) {
                                Ok(msg) => {
                                    if app.own == old {
                                        app.own = new.clone();
                                    }
                                    if let Some(v) = app.prev_subs.remove(&old) {
                                        app.prev_subs.insert(new.clone(), v);
                                    }
                                    app.note = msg;
                                    app.rows = collect(&app.rx, &mut app.prev_subs);
                                    app.refreshed = Instant::now();
                                    app.refilter(Some(new));
                                }
                                Err(e) => {
                                    app.note = e;
                                    app.rename = Some((old, buf));
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            buf.pop();
                            app.rename = Some((old, buf));
                        }
                        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => app.rename = Some((old, String::new())),
                        KeyCode::Char(c) => {
                            buf.push(c);
                            app.rename = Some((old, buf));
                        }
                        _ => app.rename = Some((old, buf)),
                    }
                    continue;
                }
                if app.typing {
                    match k.code {
                        KeyCode::Enter => app.typing = false,
                        KeyCode::Esc => {
                            app.typing = false;
                            app.query.clear();
                        }
                        KeyCode::Backspace => {
                            app.query.pop();
                        }
                        KeyCode::Char(c) => app.query.push(c),
                        _ => {}
                    }
                    let keep = app.selected().map(|r| r.name.clone());
                    app.refilter(keep);
                    continue;
                }
                match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => app.step(1),
                    KeyCode::Up | KeyCode::Char('k') => app.step(-1),
                    KeyCode::PageDown => app.step(10),
                    KeyCode::PageUp => app.step(-10),
                    KeyCode::Home | KeyCode::Char('g') => app.step(-100_000),
                    KeyCode::End | KeyCode::Char('G') => app.step(100_000),
                    KeyCode::Char('r') => app.refresh(),
                    KeyCode::Char('/') => app.typing = true,
                    KeyCode::Char('v') => {
                        app.view = match app.view {
                            View::Active => View::Busy,
                            View::Busy => View::All,
                            View::All => View::Active,
                        };
                        let keep = app.selected().map(|r| r.name.clone());
                        app.refilter(keep);
                    }
                    KeyCode::Enter => {
                        if let Some(name) = app.selected().map(|r| r.name.clone()) {
                            if name == app.own {
                                app.note = "this is the session the dashboard runs in".into();
                            } else {
                                ratatui::restore();
                                app.note = attach(&name);
                                term = ratatui::init();
                                term.clear()?;
                                app.refresh();
                            }
                        }
                    }
                    KeyCode::Char('n') => {
                        if let Some(name) = app.selected().map(|r| r.name.clone()) {
                            app.rename = Some((name.clone(), name));
                        }
                    }
                    KeyCode::Char('x') => {
                        if let Some(name) = app.selected().map(|r| r.name.clone()) {
                            if name == app.own {
                                app.note = "refusing to end the session the dashboard runs in".into();
                            } else {
                                app.confirm = Some(name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if app.refreshed.elapsed() >= Duration::from_secs(5) {
            app.refresh();
        } else if app.previewed.elapsed() >= Duration::from_secs(2) {
            app.load_preview();
        }
    }
    ratatui::restore();
    Ok(())
}
