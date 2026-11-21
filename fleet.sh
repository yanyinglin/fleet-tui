#!/usr/bin/env bash
# One-screen plain-text view of every tmux session, with no model call (zero tokens).
# Usage: fleet.sh [-a] [-w] [filter]
#   default  agent sessions active in the last 24 h, plus anything waiting on a prompt
#   -a       all tmux sessions
#   -w       wide: also print the last 3 pane lines of each ASK/WORK session
#   filter   case-insensitive substring of the session name
# Columns: STATE  IDLE  CTX  SESSION  DIR  LAST LINE
#   WORK = agent is generating (live spinner line); ASK = waiting on a prompt or permission dialog;
#   IDLE = agent at its input box; SHELL = no agent in the pane.
#   CTX and TASK come from the Claude status line in the pane; blank when the pane shows none.
# Live view in a spare terminal:  watch -n 30 ./fleet.sh
# Optional: FLEET_WORKSPACE (prefix shortened to ~w), FLEET_RATE_FILE (default ~/.claude/rate-limits.json).
set -uo pipefail
ALL=0; WIDE=0; FILT=""
for a in "$@"; do case "$a" in -a) ALL=1;; -w) WIDE=1;; *) FILT="$a";; esac; done
python3 -I - "$ALL" "$WIDE" "$FILT" <<'PY'
import subprocess,sys,time,os,glob,json,re
ALL,WIDE,FILT=sys.argv[1]=='1',sys.argv[2]=='1',sys.argv[3].lower()
now=time.time()
def sh(*a):
    try: return subprocess.run(a,capture_output=True,text=True,timeout=10).stdout
    except Exception: return ''
WS=(os.environ.get('FLEET_WORKSPACE') or '').rstrip('/'); HOME=os.path.expanduser('~')
def short(d):
    if WS and d.startswith(WS): return '~w'+d[len(WS):]
    return '~'+d[len(HOME):] if d.startswith(HOME) else d
def age(s):
    s=int(s)
    return '%ds'%s if s<90 else '%dm'%(s//60) if s<5400 else '%dh'%(s//3600) if s<172800 else '%dd'%(s//86400)
rows=[]
fmt='#{session_name}\t#{window_activity}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_id}'
seen=set()
for line in sh('tmux','list-panes','-a','-F',fmt).splitlines():
    p=line.split('\t')
    if len(p)<5 or p[0] in seen: continue
    seen.add(p[0]); name,act,cmd,cwd,pid=p
    if FILT and FILT not in name.lower(): continue
    idle=now-int(act or 0)
    pane=[l.rstrip() for l in sh('tmux','capture-pane','-p','-t',pid,'-S','-40').splitlines()]
    text='\n'.join(pane[-25:])
    agent=cmd in('claude','node','codex','cursor-agent') or 'bypass permissions' in text or '? for shortcuts' in text or 'auto mode on' in text
    sl=[l for l in pane if re.search(r'\| Ctx:\d+%',l)]
    ctx=''; task=''
    if sl:
        f=[x.strip() for x in sl[-1].split('|')]
        m=re.search(r'Ctx:(\d+%)',sl[-1]); ctx=m.group(1) if m else ''
        task=f[3] if len(f)>3 else ''
    subs=[l.strip() for l in pane[-25:] if re.match(r'^\s*[◯●◐◑◒◓]\s+\S+\s{2,}',l)]
    # newest spinner line decides: live "✻ Verb… (12s" = generating, finished "✻ Verbed for 12s" = turn ended.
    # A subagent list alone is not work: finished subagents stay listed with a frozen timer (checked 2026-11-21).
    live=False
    for l in reversed(pane[-25:]):
        if re.match(r'^\s*[^\w\s❯◯●>]\s+\S+…(\s+\(|\s*$)',l): live=True; break
        if re.match(r'^\s*[^\w\s❯◯●>]\s+\S+ for (\d+[dhm] )*\d+[dhms]',l): break
    if re.search(r'(?m)^\s*❯\s+\d+\.\s|Enter to (select|confirm)|\(y/n\)\s*$',text): st='ASK'
    elif live or 'esc to interrupt' in text: st='WORK'
    elif agent: st='IDLE'
    else: st='SHELL'
    if not ALL and not FILT and (st=='SHELL' or idle>86400) and st!='ASK': continue
    skip=('bypass permissions','for shortcuts','auto mode on','Update installed','| Ctx:')
    body=[l.strip() for l in pane if l.strip() and not re.match(r'^[\s─━╭╰│>❯⏵]*$',l) and not any(k in l for k in skip) and not l.strip().startswith(('╭','╰','│ >','>'))]
    said=[l for l in body if l[:1] in '●•⏺']
    said=[l for l in said if l not in ('● main',) and l not in subs]
    last=said[-1] if said else (body[-1] if body else '')
    rows.append((st,idle,name,task or short(cwd)[-30:],last,body[-3:],ctx))
order={'ASK':0,'WORK':1,'IDLE':2,'SHELL':3}
rows.sort(key=lambda r:(order[r[0]],r[1]))
try:
    r=json.load(open(os.environ.get('FLEET_RATE_FILE') or os.path.expanduser('~/.claude/rate-limits.json'))); fh=r.get('five_hour') or {}
    print('5h used %s%%  resets in %s  reading age %s'%(fh.get('used_percentage','?'),age(max(0,fh.get('resets_at',now)-now)),age(now-r.get('written_at',now))))
except Exception: print('5h usage: unknown')
c={k:sum(1 for r in rows if r[0]==k) for k in order}
print('ASK %d  WORK %d  IDLE %d  SHELL %d   (%s)'%(c['ASK'],c['WORK'],c['IDLE'],c['SHELL'],time.strftime('%m-%d %H:%M')))
print('%-5s %5s %4s  %-22s %-34s %s'%('STATE','IDLE','CTX','SESSION','TASK (or dir)','LAST'))
for st,idle,name,cwd,last,tail3,ctx in rows:
    print('%-5s %5s %4s  %-22s %-34s %s'%(st,age(idle),ctx,name[:22],cwd[:34],last[:100]))
    if WIDE and st in('ASK','WORK'):
        for l in tail3[:-1]: print(' '*46+'| '+l.strip()[:110])
PY
