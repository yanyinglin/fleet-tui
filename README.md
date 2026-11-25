# fleet

A terminal dashboard for people who run many coding agents in tmux. It shows every tmux session on one screen: which
agents are working, which are waiting for an answer, which are idle, how full each context window is, and a live
preview of the selected pane.

It never calls a model, so watching it costs no tokens. It reads state with `tmux list-panes` and `tmux capture-pane`
and never sends keys into a session.

## Install

Requires tmux and a Rust toolchain.

```sh
cargo build --release
install -m 755 target/release/fleet /usr/local/bin/fleet
fleet
```

`fleet.sh` is a plain-text version of the same table that needs only bash and python3. It suits `watch -n 30 ./fleet.sh`
or a quick look over ssh. Flags: `-a` all sessions, `-w` also print the last pane lines of busy sessions, and an
optional name filter.

## Keys

| Key | Action |
|---|---|
| `↑` `↓` `j` `k`, `PgUp` `PgDn`, `g` `G` | move |
| `Enter` | open the session; detach (`prefix` + `d`) to come back |
| `n` | rename the session |
| `x` | end the session, after a confirmation that needs a capital `Y` |
| `/` | filter by session name, task or last line |
| `v` | cycle the view: active in the last 24 h, waiting and working, all |
| `r` | refresh now |
| `q` | quit |

Inside tmux, `Enter` opens the session in a popup, because tmux refuses a nested attach. The dashboard will not open
or end the session it runs in.

## How state is decided

State comes from the text on the pane, so it depends on what the agent prints. The rules were checked against Claude
Code panes.

| State | Rule |
|---|---|
| ASK | a selection dialog is on screen: an option list such as `❯ 1. Yes`, or an "Enter to select" footer |
| WORK | the newest spinner line has the live form `✻ Verb… (12s · …)` and not the finished form `✻ Verbed for 12s`, or a listed subagent's timer changed since the last refresh |
| idle | an agent pane that matches neither |
| sh | no agent in the pane |

**IDLE** is the time since the agent's last turn ended, read from the `· done HH:MM` stamp Claude Code prints
under a finished turn. tmux's own activity time is used only when no stamp is on screen or the session has been quiet
for over a day, because every status-line redraw or toast resets it. A working session shows `-`. Panes parked on the
agent list screen keep redrawing, so their IDLE stays near 0.

**JOBS** counts the commands the agent is running right now, read from the process tree under the pane (`ps`): each
Bash tool call, foreground or background, and any other child process except MCP servers. The panel above the
preview lists them with their age, the command (unwrapped from Claude Code's shell wrapper) and, after `→`, the
innermost process running under it at the moment. Processes that were detached from the agent (`nohup`, `setsid`,
pods on other hosts) are not attributed to it.

A subagent list alone does not count as work, because finished subagents stay listed with a frozen timer. Other agents
(Codex, cursor-agent) are recognised as agent panes, but their working state is detected only through an
`esc to interrupt` hint.

## Optional settings

Settings live in `~/.config/fleet/config` as `key=value` lines. An environment variable overrides the file.

| Key | Variable | Meaning |
|---|---|---|
| `pinned` | `FLEET_PINNED` | comma-separated session names shown as status lights in the header and marked `◆` in the table |
| `workspace` | `FLEET_WORKSPACE` | directory prefix shortened to `~w` |
| `rate_file` | `FLEET_RATE_FILE` | quota reading, default `~/.claude/rate-limits.json` |

Two columns need help from your Claude Code status line, and stay blank without it.

The CONTEXT and TASK columns are parsed from a status line shaped like `Model | Ctx:14% | … | task name | …`: the
number after `Ctx:` and the fourth `|` field.

The quota gauge reads a JSON file that your status line script writes:

```json
{"five_hour": {"used_percentage": 27.0, "resets_at": 1795290000}, "written_at": 1795282000}
```

Times are Unix seconds. A reading older than 15 minutes is shown as stale.

## License

MIT
