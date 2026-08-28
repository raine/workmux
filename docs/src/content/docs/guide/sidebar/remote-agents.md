---
title: "Remote agents"
description: Show agents running on other machines in the local sidebar and jump to them over ssh
---

The sidebar (and dashboard) can display agents that run inside a tmux server on
**another machine** - for example claudes living on a work server you keep
attached in an `ssh ... 'tmux attach'` pane. Remote agents appear next to local
ones with a dimmed `@<host>` tag, show live status, and jumping to them works.

:::note
tmux backend only. Remote agents come from mirrored state files (see below);
workmux does not open any network connections on its own except when you jump.
:::

## How it works

Workmux stores one JSON state file per agent pane in
`~/.local/state/workmux/agents/`. The remote machine runs stock workmux with
normal status hooks, producing state files for **its** tmux server. A small
sync of your choosing mirrors those files to the local machine, rewriting the
`pane_key.instance` field to `ssh:<host>`. The local reconcile pass recognizes
the `ssh:` prefix and:

- exposes those agents with a namespaced pane id `ssh:<host>/%N`
- skips local liveness checks for them - there is no local pane to check
  against, so the mirror is taken at face value
- routes jumps to them over ssh instead of `switch-client`

That last point makes the sync responsible for liveness: a mirrored file is
assumed to describe an agent that exists. Getting that wrong is the one way
this feature goes bad, so the contract below spells it out.

## Setting up the mirror

Any sync loop works as long as it:

1. copies `~/.local/state/workmux/agents/*.json` from the remote host every
   second or so (a persistent ssh `ControlMaster` connection makes this cheap),
2. **mirrors an agent only while its pane is live on the remote** (see below),
3. rewrites `pane_key.instance` to `ssh:<host>` and renames the file to match
   (`tmux__ssh%3A<host>__<pane>.json` - the filename encodes `/ \ : %`),
4. deletes the mirrored files when the remote becomes unreachable, so dead
   tiles disappear instead of going stale,
5. optionally signals the sidebar daemon for an instant refresh:
   `pkill -USR1 -f 'workmux _sidebar-daemon'`.

### Why step 2 matters

State files outlive their panes - nothing prunes them unless a workmux daemon
happens to be running on that machine - so a wholesale copy pins dead agents to
your sidebar until you notice and delete the files by hand. Ask the remote tmux
in the same round trip and keep a state file only when it still agrees:

| state field | remote tmux | catches |
| --- | --- | --- |
| `pane_key.pane_id` | listed by `list-panes -a` | pane closed |
| `boot_id` | `#{start_time}` | tmux server restarted, every old pane is gone |
| `pane_pid` | `#{pane_pid}` | pane id recycled by a new shell |
| `command` | `#{pane_current_command}` | agent exited, shell took the pane back |

These are the same checks the local reconcile pass runs against local panes.

:::tip
If your only window into that machine is an `ssh`/`autossh` pane inside your
local tmux, consider mirroring **only while such a pane exists**. An agent you
cannot jump to is not worth a tile, and it makes the mirror collapse on its own
after a reboot instead of waiting for the next successful sync.
:::

Minimal example (run it under a process supervisor of your choice):

```bash
#!/usr/bin/env bash
# Mirror workmux agent state from HOST into the local state dir, keeping only
# agents whose pane is still live over there. Needs jq and base64.
HOST=s
AGENTS=~/.local/state/workmux/agents
ENC_HOST=$(printf '%s' "ssh:$HOST" | sed 's/%/%25/g; s/:/%3A/g; s#/#%2F#g')
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=5
          -o ControlMaster=auto -o ControlPath=/tmp/wm-sync-%C -o ControlPersist=120)
# One round trip: server boot id, live panes, marker, then the state files as
# a base64 tar (they are pretty-printed JSON, so they cannot be line-parsed).
REMOTE='tmux display-message -p "B #{start_time}" 2>/dev/null
        tmux list-panes -a -F "P #{pane_id} #{pane_pid} #{pane_current_command}" 2>/dev/null
        echo @@AGENTS@@
        cd ~/.local/state/workmux/agents 2>/dev/null &&
          ls *.json >/dev/null 2>&1 && tar cf - *.json | base64
        exit 0'
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
mkdir -p "$AGENTS"

while sleep 1; do
  if ! out=$(ssh "${SSH_OPTS[@]}" "$HOST" "$REMOTE" 2>/dev/null); then
    rm -f "$AGENTS"/tmux__"$ENC_HOST"__*.json     # remote gone -> clear mirror
    continue
  fi
  panes=${out%%@@AGENTS@@*}
  boot=$(awk '$1=="B" {print $2; exit}' <<<"$panes")
  rm -f "$tmp"/*.json
  printf '%s' "${out#*@@AGENTS@@}" | base64 -d 2>/dev/null | tar xf - -C "$tmp" 2>/dev/null

  seen=""
  for src in "$tmp"/*.json; do
    [ -e "$src" ] || continue
    pane=$(jq -r '.pane_key.pane_id // empty' "$src")
    [ -n "$pane" ] || continue
    # the pane as tmux sees it right now - no row means the agent is gone
    pid=""; cmd=""
    read -r _ _ pid cmd < <(awk -v p="$pane" '$1=="P" && $2==p {print; exit}' <<<"$panes")
    [ -n "$pid" ] || continue
    # server restarted / pane id recycled / agent exited -> not the same agent
    [ "$(jq -r '.boot_id  // empty' "$src")" = "$boot" ] || continue
    [ "$(jq -r '.pane_pid // empty' "$src")" = "$pid" ]  || continue
    [ "$(jq -r '.command  // empty' "$src")" = "$cmd" ]  || continue

    f="$AGENTS/tmux__${ENC_HOST}__$(printf '%s' "$pane" | sed 's/%/%25/g').json"
    jq -c --arg i "ssh:$HOST" '.pane_key.instance=$i' "$src" >"$f.tmp" && mv "$f.tmp" "$f"
    seen="$seen $f"
  done
  for f in "$AGENTS"/tmux__"$ENC_HOST"__*.json; do
    [ -e "$f" ] || continue
    case " $seen " in *" $f "*) ;; *) rm -f "$f";; esac
  done
  pkill -USR1 -f 'workmux _sidebar-daemon' 2>/dev/null
done
```

## Jumping to a remote agent

Selecting a remote agent (sidebar `Enter`/click, `workmux sidebar jump N`,
dashboard, `last-done`, `last-agent`) does two things:

1. **Locally**: if a local pane hosts the ssh/autossh client for that host
   (detected by the client process's argv on the pane's tty), the most
   recently used such pane is brought on screen - that pane is your window
   into the remote tmux. If the remote view lives outside tmux (e.g. a
   terminal tab), local focus is left alone.
2. **Remotely** (detached ssh, does not block the UI): the agent's window
   becomes the active window in **every attached session** of the remote
   server. Grouped sessions keep independent active-window pointers, so all
   attached views follow.

The jumped-to agent is highlighted as active in the sidebar until you switch
to a different local window.

## Display

- The `{remote}` template token renders a dimmed `@<host>` next to the agent
  name (wired into the default templates; see
  [Customization](/guide/sidebar/customization/)).
- Git stats, PR checks, and output-preview capture are skipped for remote
  agents - those need the paths and panes locally.
- Status icons behave as usual, except the done icon does not auto-clear on
  focus (the window that would clear it lives on the remote machine).
