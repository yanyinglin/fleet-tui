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
//! IDLE = time since the last "· done HH:MM" turn stamp (tmux activity only as a fallback); JOBS = running child
//! commands of the agent process, from one `ps` snapshot per refresh.
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
    cache_ttl: Option<u64>,
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
            cache_ttl: get("cache_ttl", "FLEET_CACHE_TTL").parse().ok(),
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
    jobs: Vec<Job>,
    /// seconds since the session's last model response, the moment its prompt cache was last written
    since_api: Option<u64>,
    /// the pane shows Claude Code's agent list ("describe a task for a new session"), not a conversation
    listing: bool,
    /// the session's task list (TaskCreate / TaskUpdate), oldest first
    plan: Vec<Task>,
}

#[derive(Clone)]
struct Task {
    status: String,
    subject: String,
    active: String,
}

/// Session id of a running Claude Code process, from `~/.claude/sessions/<pid>.json`.
fn session_id(pid: u32) -> Option<String> {
    let reg = std::fs::read_to_string(format!("{}/.claude/sessions/{pid}.json", cfg().home)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&reg).ok()?;
    v["sessionId"].as_str().map(|s| s.to_string())
}

/// Claude Code keeps each session's task list as `~/.claude/tasks/<sessionId>/<id>.json`.
fn plan_of(sid: &str) -> Vec<Task> {
    let mut v: Vec<(u64, Task)> = std::fs::read_dir(format!("{}/.claude/tasks/{sid}", cfg().home))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
                        return None;
                    }
                    let d: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                    d["subject"].as_str()?;
                    let id = d["id"].as_str().and_then(|x| x.parse().ok()).unwrap_or(u64::MAX);
                    let st = d["status"].as_str().unwrap_or("pending").to_string();
                    if st == "deleted" {
                        return None;
                    }
                    Some((id, Task {
                        status: st,
                        subject: d["subject"].as_str().unwrap_or("").to_string(),
                        active: d["activeForm"].as_str().unwrap_or("").to_string(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

/// Prompt-cache lifetime in seconds: Claude Code asks for a 1-hour TTL, which drops to 5 minutes once the account is
/// in usage overage (5-hour quota at 100 %). Override with FLEET_CACHE_TTL / `cache_ttl` (seconds).
fn cache_ttl(u: &Usage) -> u64 {
    if let Some(v) = cfg().cache_ttl {
        return v;
    }
    if u.pct.is_some_and(|p| p >= 100.0) { 300 } else { 3600 }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "2026-11-24T23:13:14.731Z" -> epoch seconds.
fn parse_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let days = days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?);
    let secs = days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?;
    u64::try_from(secs).ok()
}

thread_local! {
    static API_MEMO: std::cell::RefCell<HashMap<String, (u64, Option<u64>)>> = std::cell::RefCell::new(HashMap::new());
}

/// Claude Code registers each running session in `~/.claude/sessions/<pid>.json` with its sessionId; the transcript is
/// `~/.claude/projects/<dir>/<sessionId>.jsonl`. Returns the epoch time of the last assistant entry, read from the
/// file's tail only. Results are cached by (path, size) so an unchanged transcript is not reread.
fn last_api(pid: u32) -> Option<u64> {
    let home = &cfg().home;
    let sid = session_id(pid)?;
    let path = std::fs::read_dir(format!("{home}/.claude/projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(format!("{sid}.jsonl")))
        .find(|p| p.exists())?;
    let key = path.to_string_lossy().into_owned();
    let size = std::fs::metadata(&path).ok()?.len();
    if let Some((s, t)) = API_MEMO.with(|m| m.borrow().get(&key).copied()) {
        if s == size {
            return t;
        }
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&path).ok()?;
    let mut t = None;
    // grow the tail window until an assistant entry is found (big tool results can push it back)
    for win in [256_u64 << 10, 2 << 20, 16 << 20] {
        let start = size.saturating_sub(win);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let txt = String::from_utf8_lossy(&buf);
        t = txt
            .lines()
            .rev()
            .filter(|l| l.contains("\"type\":\"assistant\""))
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok().and_then(|d| d["timestamp"].as_str().and_then(parse_iso)));
        if t.is_some() || start == 0 {
            break;
        }
    }
    API_MEMO.with(|m| m.borrow_mut().insert(key, (size, t)));
    t
}

/// A command the agent started that is still running: a Bash tool call (foreground or background) or another child.
#[derive(Clone)]
struct Job {
    secs: u64,
    cmd: String,
    /// the innermost process under the command right now (e.g. `sleep 300`, `python run.py`), if different
    now: String,
}

/// One `ps` snapshot per refresh: pid -> (ppid, elapsed seconds, full command line).
struct Procs {
    info: HashMap<u32, (u32, u64, String)>,
    kids: HashMap<u32, Vec<u32>>,
}

fn procs() -> Procs {
    let mut info = HashMap::new();
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for l in sh(&["ps", "-eo", "pid=,ppid=,etimes=,args="]).lines() {
        let mut it = l.split_whitespace();
        let (Some(pid), Some(ppid), Some(et)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(ppid), Ok(et)) = (pid.parse::<u32>(), ppid.parse::<u32>(), et.parse::<u64>()) else { continue };
        let args = it.collect::<Vec<_>>().join(" ");
        kids.entry(ppid).or_default().push(pid);
        info.insert(pid, (ppid, et, args));
    }
    Procs { info, kids }
}

impl Procs {
    fn args(&self, pid: u32) -> &str {
        self.info.get(&pid).map(|x| x.2.as_str()).unwrap_or("")
    }
    fn children(&self, pid: u32) -> &[u32] {
        self.kids.get(&pid).map(|v| v.as_slice()).unwrap_or(&[])
    }
    /// The agent process in a pane: the first process at or below the pane's shell that runs claude, codex or cursor-agent.
    fn agent(&self, root: u32) -> Option<u32> {
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            let a = self.args(p);
            let exe = a.split_whitespace().next().unwrap_or("");
            let base = exe.rsplit('/').next().unwrap_or("");
            if matches!(base, "claude" | "codex" | "cursor-agent") || (base == "node" && a.contains("claude")) {
                return Some(p);
            }
            stack.extend(self.children(p).iter().rev());
        }
        None
    }
    /// The deepest, newest process under `pid` (what the command is doing right now).
    fn leaf(&self, mut pid: u32) -> u32 {
        while let Some(&c) = self.children(pid).iter().max() {
            pid = c;
        }
        pid
    }
    /// Commands the agent is running. MCP servers are skipped; Claude Code's shell wrapper is unwrapped to the
    /// command inside its `eval '...'`.
    fn jobs(&self, agent: u32) -> Vec<Job> {
        let mut out = Vec::new();
        for &c in self.children(agent) {
            let a = self.args(c);
            if a.contains("mcp-server") || a.contains("-mcp") || a.contains("/mcp") || a.contains("mcp_server") {
                continue;
            }
            let cmd = unwrap_shell(a);
            let lf = self.leaf(c);
            let now = if lf == c { String::new() } else { self.args(lf).to_string() };
            let now = if cmd.starts_with(&now) { String::new() } else { now };
            out.push(Job { secs: self.info.get(&c).map(|x| x.1).unwrap_or(0), cmd, now });
        }
        out.sort_by_key(|j| j.secs);
        out
    }
}

/// Claude Code runs each Bash call as `zsh -c source <snapshot> ... && eval '<command>' < /dev/null && pwd -P >| ...`.
/// Return the command inside eval, or the whole line for anything else.
fn unwrap_shell(a: &str) -> String {
    if let Some(i) = a.find("eval '") {
        let rest = &a[i + 6..];
        let end = rest.rfind("' < /dev/null").or_else(|| rest.rfind('\'')).unwrap_or(rest.len());
        return rest[..end].replace("'\\''", "'").replace("'\"'\"'", "'").split_whitespace().collect::<Vec<_>>().join(" ");
    }
    a.to_string()
}

/// Seconds since local midnight offset: `date +%z` once, e.g. -0500 -> -18000.
fn tz_offset() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let z = sh(&["date", "+%z"]);
        let z = z.trim();
        if z.len() == 5 {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let h: i64 = z[1..3].parse().unwrap_or(0);
            let m: i64 = z[3..5].parse().unwrap_or(0);
            sign * (h * 3600 + m * 60)
        } else {
            0
        }
    })
}

/// Seconds since a local wall-clock time `HH:MM` that lies in the past 24 hours.
fn since_clock(hh: u64, mm: u64, t: u64) -> u64 {
    let local = t as i64 + tz_offset();
    let today = local - local.rem_euclid(86_400);
    let mut at = today + (hh * 3600 + mm * 60) as i64;
    if at > local + 60 {
        at -= 86_400;
    }
    (local - at).max(0) as u64
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
    let fmt = "#{session_name}\t#{window_activity}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_id}\t#{pane_pid}";
    let t = now();
    let ps = procs();
    let done_at = Regex::new(r"· done (\d{1,2}):(\d{2})").unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for line in sh(&["tmux", "list-panes", "-a", "-F", fmt]).lines() {
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 5 || !seen.insert(p[0].to_string()) {
            continue;
        }
        // window_activity moves on every redraw (status line, toasts), so it understates idle time; prefer the
        // "· done HH:MM" stamp of the last finished turn when the pane shows one (see below).
        let mut idle = t.saturating_sub(p[1].parse::<u64>().unwrap_or(0));
        let agent_pid = p.get(5).and_then(|x| x.parse::<u32>().ok()).and_then(|pp| ps.agent(pp));
        let jobs = agent_pid.map(|a| ps.jobs(a)).unwrap_or_default();
        let since_api = agent_pid.and_then(last_api).map(|a| t.saturating_sub(a));
        let plan = agent_pid.and_then(session_id).map(|sid| plan_of(&sid)).unwrap_or_default();
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
        // the agent-list screen animates its rows, so tmux activity is always "now"; it has no turn of its own
        let listing = tail.iter().take(8).any(|l| l.trim() == "❯ describe a task for a new session");
        match st {
            St::Work => idle = 0,
            St::Idle | St::Ask if since_api.is_some() => idle = since_api.unwrap_or(idle),
            St::Idle | St::Ask => {
                if let Some(c) = spin.and_then(|l| done_at.captures(l)) {
                    let (hh, mm) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0));
                    // the stamp has no date; beyond a day the tmux activity time is the better bound
                    if idle < 86_400 {
                        idle = since_clock(hh, mm, t);
                    }
                }
            }
            St::Shell => {}
        }
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
            jobs,
            since_api,
            listing,
            plan,
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

/// Time left before the session's prompt cache expires; after that its next turn rewrites the whole context.
fn cache_cell(r: &Row, ttl: u64) -> Cell<'static> {
    let Some(since) = r.since_api else { return Cell::from("") };
    let line = if r.st == St::Work {
        Line::from("live").style(Style::new().fg(Color::LightGreen))
    } else if since >= ttl {
        Line::from("cold").style(Style::new().fg(Color::DarkGray))
    } else {
        let left = ttl - since;
        let col = if left < 300 { Color::LightRed } else if left < 900 { Color::Yellow } else { Color::Cyan };
        Line::from(age(left)).style(Style::new().fg(col))
    };
    Cell::from(line.alignment(Alignment::Right))
}

/// done/total of the session's task list; green while a task is in progress.
fn plan_cell(r: &Row) -> Cell<'static> {
    if r.plan.is_empty() {
        return Cell::from("");
    }
    let done = r.plan.iter().filter(|t| t.status == "completed").count();
    let live = r.plan.iter().any(|t| t.status == "in_progress");
    let col = if live { Color::LightGreen } else if done == r.plan.len() { Color::DarkGray } else { Color::Gray };
    Cell::from(Line::from(format!("{done}/{}", r.plan.len())).alignment(Alignment::Right)).style(Style::new().fg(col))
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
    let ttl = cache_ttl(&app.usage);
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
                Cell::from(Line::from(if r.listing { "list".to_string() } else if r.st == St::Work { "-".to_string() } else { age(r.idle) }).alignment(Alignment::Right)),
                Cell::from(Line::from(if r.jobs.is_empty() { String::new() } else { r.jobs.len().to_string() }).alignment(Alignment::Right))
                    .style(Style::new().fg(Color::LightYellow)),
                cache_cell(r, ttl),
                plan_cell(r),
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
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(13),
            Constraint::Length(26),
            Constraint::Length(34),
            Constraint::Min(20),
        ],
    )
    .header(
        ratatui::widgets::Row::new(["STATE", " IDLE", "JOBS", " CACHE", " PLAN", "CONTEXT", "  SESSION", "TASK", "LAST"])
            .style(Style::new().fg(Color::DarkGray).bold()),
    )
    .column_spacing(2)
    .row_highlight_style(Style::new().bg(Color::Rgb(40, 44, 60)))
    .highlight_symbol("▌")
    .block(Block::bordered().title(title));
    f.render_stateful_widget(table, list, &mut app.table);

    // right column beside the pane preview: the task plan on top, running commands below it
    let plan: Vec<Task> = app.selected().map(|r| r.plan.clone()).unwrap_or_default();
    let jobs: Vec<Job> = app.selected().map(|r| r.jobs.clone()).unwrap_or_default();
    let prev = if plan.is_empty() && jobs.is_empty() {
        prev
    } else {
        let [left, right] = Layout::horizontal([Constraint::Min(40), Constraint::Percentage(40)]).areas(prev);
        let jobs_h = if jobs.is_empty() { 0 } else { (jobs.len().min(6) as u16) + 2 + u16::from(jobs.len() > 6) };
        let [plan_area, jobs_area] = if plan.is_empty() {
            [Rect::default(), right]
        } else {
            Layout::vertical([Constraint::Min(4), Constraint::Length(jobs_h)]).areas(right)
        };
        if !plan.is_empty() {
            let h = plan_area.height.saturating_sub(2) as usize;
            // keep the current task in view: start a little above the first unfinished one
            let first_open = plan.iter().position(|t| t.status != "completed").unwrap_or(plan.len().saturating_sub(1));
            let start = first_open.saturating_sub(2).min(plan.len().saturating_sub(h));
            let pl: Vec<Line> = plan[start..]
                .iter()
                .take(h)
                .map(|t| match t.status.as_str() {
                    "completed" => Line::styled(format!("✓ {}", t.subject), Style::new().fg(Color::DarkGray)),
                    "in_progress" => Line::styled(
                        format!("▶ {}", if t.active.is_empty() { &t.subject } else { &t.active }),
                        Style::new().fg(Color::LightGreen).bold(),
                    ),
                    _ => Line::styled(format!("○ {}", t.subject), Style::new().fg(Color::White)),
                })
                .collect();
            let done = plan.iter().filter(|t| t.status == "completed").count();
            f.render_widget(
                Paragraph::new(pl).block(Block::bordered().title(format!(" plan · {done}/{} ", plan.len()))),
                plan_area,
            );
        }
        if !jobs.is_empty() {
            let mut jl: Vec<Line> = jobs
                .iter()
                .take(6)
                .map(|j| {
                    let mut s = vec![
                        Span::styled(format!("{:>5}  ", age(j.secs)), Style::new().fg(Color::LightYellow)),
                        Span::styled(j.cmd.clone(), Style::new().fg(Color::White)),
                    ];
                    if !j.now.is_empty() {
                        s.push(Span::styled(format!("   → {}", j.now), Style::new().fg(Color::DarkGray)));
                    }
                    Line::from(s)
                })
                .collect();
            if jobs.len() > 6 {
                jl.push(Line::styled(format!("  … {} more", jobs.len() - 6), Style::new().fg(Color::DarkGray)));
            }
            f.render_widget(
                Paragraph::new(jl).block(Block::bordered().title(format!(" running commands · {} ", jobs.len()))),
                jobs_area,
            );
        }
        left
    };

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
