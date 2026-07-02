---
description: Release notes and version history for workmux
---

<!-- skipped: v0.1.167 -->
<!-- skipped: v0.1.161 -->
<!-- skipped: v0.1.160 -->
<!-- skipped: v0.1.159 -->
<!-- skipped: v0.1.155 -->
<!-- skipped: v0.1.121 -->
<!-- skipped: v0.1.115 -->
<!-- skipped: v0.1.113 -->
<!-- skipped: v0.1.89 -->
<!-- skipped: v0.1.82 -->
<!-- skipped: v0.1.73 -->
<!-- skipped: v0.1.64 -->
<!-- skipped: v0.1.58 -->
<!-- skipped: v0.1.56 -->
<!-- skipped: v0.1.27 -->
<!-- skipped: v0.1.25 -->
<!-- skipped: v0.1.8 -->

# Changelog

## v0.1.218 (2026-07-02)

- Add a sidebar filter for switching between all sessions and the current tmux session, with jump navigation that works across sessions. ([#124](https://github.com/raine/workmux/pull/124))
- Add `window_placement` to choose whether new tmux windows open next to the current window or at the right edge of the session.
- Keep PR numbers and states visible in `workmux list --pr` when GitHub check rollup queries fail.
- Make concurrent agent state writes safer, preventing overlapping updates from clobbering each other. ([#188](https://github.com/raine/workmux/pull/188))
- Keep Nix package builds reliable in sandboxed build environments.

## v0.1.217 (2026-06-29)

- Keep the sidebar spinner moving while panes are active. ([#180](https://github.com/raine/workmux/issues/180))
- Respect configured sidebar width and height instead of replacing them with synced resize values. ([#172](https://github.com/raine/workmux/pull/172))
- Install the Pi agent sandbox from the current package name. ([#179](https://github.com/raine/workmux/pull/179))
- Recover automatically when a tmux resize leaves the sidebar excessively wide.

## v0.1.216 (2026-06-28)

- Fix Codex status tracking when hooks run without tmux environment details, so Codex agents appear in `workmux list` and the sidebar. ([#187](https://github.com/raine/workmux/issues/187))
- Keep Codex sidebar status accurate for subagents, so panes stay working while parent work continues and clear when the root turn finishes. ([#170](https://github.com/raine/workmux/issues/170))

## v0.1.215 (2026-06-26)

- Place new tmux windows from `workmux add` immediately beside the calling window, keeping new worktrees easy to find.

## v0.1.214 (2026-06-26)

- Add `workmux rebase` to rebase a worktree onto its saved base branch, with automatic fallback to the configured main branch.
- Add `workmux sidebar --position <left|top>` to choose sidebar placement when enabling the sidebar.
- Add structured [named agent profiles](https://workmux.raine.dev/guide/agents#named-agents) with separate `type`, `command`, `args`, and `env` fields, including `from_env` values and `<agent:name>` pane placeholders.
- Keep named agent profiles stable across repeated launches, so profile commands and arguments are not mutated between runs.
- Preserve the current tmux focus when merging a worktree from an inactive window.
- Place new tmux windows beside the current window for more predictable navigation.

## v0.1.213 (2026-06-12)

- Add first-party Oh My Pi (`omp`) support for prompt injection, status setup with waiting detection, skills, and sandbox credential mounts. ([#177](https://github.com/raine/workmux/pull/177))
- Fix dashboard paste handling so pasted input is processed reliably. ([#178](https://github.com/raine/workmux/issues/178))
- Refresh the sidebar immediately after removing a worktree.

## v0.1.212 (2026-06-06)

- Add an uninstall script that removes workmux hooks, bundled skills, cache, and state data while preserving unrelated agent configuration.
- Add `workmux reap-agents` to find stale tracked agents and optionally stop their agent process without tearing down the pane.
- Fix `workmux resurrect` so worktrees that used a non-default agent resume with the same agent after a crash. ([#175](https://github.com/raine/workmux/issues/175))
- Fix `workmux remove` leaving stale tmux windows behind in some cleanup cases.

## v0.1.211 (2026-05-22)

- Respect each repository's configured base branch when adding worktrees from the dashboard.
- Keep dashboard worktree creation and PR checkout actions tied to the selected repository instead of the shell's current repository.

## v0.1.210 (2026-05-22)

- Add custom tmux target names for `workmux add` and `workmux open`, so the worktree directory can stay separate from the window or session name. ([#139](https://github.com/raine/workmux/issues/139))
- Allow window-mode worktrees to open inside a chosen parent tmux session.
- Keep sidebar PR status tied to the correct branch, avoiding stale PR details on base branches.

## v0.1.209 (2026-05-19)

- Allow `workmux merge` to keep the worktree, window, and branch by default with `merge_keep: true`.
- Add `workmux merge --cleanup` to clean up for a single merge when keep-by-default behavior is enabled.
- Document how worktree dispatch should handle tasks that target another project.

## v0.1.208 (2026-05-15)

- Add a horizontal top bar option for the sidebar, with configurable height and chip width
- Allow showing PR status and checks in sidebar
- Make sidebar template errors visible in sidebar
- Improve sidebar loading, sync, spacing, and horizontal layout behavior

## v0.1.207 (2026-05-13)

- Fix `workmux merge` falling back incorrectly when a saved base branch only exists as a remote ref
- Update the Pi sandbox image to install the current Pi agent package ([#165](https://github.com/raine/workmux/pull/165))

## v0.1.206 (2026-05-12)

- Add Pi agent support for container and Lima sandboxes
- Fix `workmux add` hanging when launched with an open stdin pipe that has no input

## v0.1.205 (2026-05-12)

- Update Codex setup to enable hooks with the current `hooks` feature flag. ([#164](https://github.com/raine/workmux/issues/164))

## v0.1.204 (2026-05-09)

- Allow `workmux add --pr` to reuse an existing local PR branch when it does not already have a worktree. ([#163](https://github.com/raine/workmux/issues/163))

## v0.1.203 (2026-05-08)

- Stop treating ordinary single-worktree prompts as MiniJinja templates
- Let stale worktree directories be removed even when their Git metadata is missing, while still protecting branches unless `--keep-branch` is used.

## v0.1.202 (2026-05-05)

- Respect `CLAUDE_CONFIG_DIR` when installing Claude Code hooks, so custom Claude config locations are honored.

## v0.1.201 (2026-05-05)

- Keep sidebar layouts in sync immediately after terminal resizes, including inactive tmux windows.
- Fix prompt delivery for custom named agents that specify an agent type.

## v0.1.200 (2026-05-04)

- Allow `workmux close` to work from inside sandboxed worktrees by forwarding the request to the host. ([#158](https://github.com/raine/workmux/issues/158))
- Use the host workmux configuration for sandbox-launched host commands, avoiding accidental reuse of sandbox guest settings.

## v0.1.199 (2026-05-04)

- Keep sidebar widths synchronized across tmux windows after manual resizing and terminal resize events.

## v0.1.198 (2026-05-02)

- Allow `workmux sidebar --session` to hide or restore the sidebar for the current tmux session while global sidebar mode stays active.

## v0.1.197 (2026-05-02)

- Respect `CLAUDE_CONFIG_DIR` when installing skills, so custom Claude config locations are honored. ([#157](https://github.com/raine/workmux/issues/157))

## v0.1.196 (2026-05-01)

- Fix Codex status tracking so panes stay marked as working while nested subagents are still running ([#154](https://github.com/raine/workmux/issues/154))

## v0.1.195 (2026-05-01)

- Keep compact sidebar status icons aligned even when icons have different widths

## v0.1.194 (2026-05-01)

- Add `WORKMUX_DISABLE_SET_WINDOW_STATUS=1` to let nested agents skip status hook updates when launched from inside another agent pane. See the [status tracking guide](https://workmux.raine.dev/guide/status-tracking)

## v0.1.191 (2026-04-30)

- Allow sandbox deny-mode domains to opt in to trusted private network destinations, such as VPN-hosted package mirrors, while keeping loopback and link-local addresses blocked
- Keep sidebar tile rows stable when template fields are empty, so optional details no longer collapse and shift tile heights unexpectedly

## v0.1.190 (2026-04-29)

- Add sidebar template customization, letting you choose which labels, git stats, timers, and status details appear in compact and tile layouts. See the [sidebar customization guide](https://workmux.raine.dev/guide/sidebar/customization)
- Add customizable per-agent sidebar icons and colors, with built-in defaults for Claude, Codex, OpenCode, Gemini, Pi, Kiro, Vibe, and Copilot
- Support tmux-style colors and attributes inside sidebar templates
- Improve sidebar rendering for git stats, elapsed time, row highlighting, and synchronized width across windows

## v0.1.189 (2026-04-27)

- Fix pi agents lingering in the dashboard after exit ([#143](https://github.com/raine/workmux/issues/143))

## v0.1.188 (2026-04-26)

- Support `{project}` placeholder and `~` (tilde) expansion in `worktree_dir`,
  letting you use config like `worktree_dir = "~/worktrees/{project}"` ([#148](https://github.com/raine/workmux/issues/148))

## v0.1.187 (2026-04-25)

- Add `--config <path>` flag to `workmux add` and `workmux open` to use an
  alternate config file for a single invocation

## v0.1.186 (2026-04-25)

- Show a progress overlay when sweeping multiple worktrees in the dashboard,
  instead of freezing silently for several seconds
- Truncate long worktree names in the dashboard so they no longer overflow the
  panel
- Fix panics caused by Unicode characters in worktree names being truncated at
  non-character boundaries
- Install via `cargo binstall workmux` is now supported
  ([#137](https://github.com/raine/workmux/pull/137))

## v0.1.185 (2026-04-22)

- Add `--mode <window|session>` flag to `workmux add` and `workmux open` for
  per-command multiplexer mode overrides. Use `--mode window` to temporarily
  reopen a session-mode worktree as a window, or `--mode session` to create a
  one-off session without changing config. The existing `--session` flag is now
  shorthand for `--mode session`
  ([#139](https://github.com/raine/workmux/issues/139))
- Add `sandbox.container.excluded_files` config to hide sensitive worktree files
  from sandboxed containers by shadowing them with `/dev/null` (e.g. `.env`,
  `.env.local`). Configurable only in your global config for security
  ([#134](https://github.com/raine/workmux/pull/134))
- Restrict `excluded_files` to global config only, preventing malicious projects
  from disabling file masking via their own `.workmux.yaml`
- Skip `excluded_files` gracefully with a clear warning on runtimes that do not
  support file-level bind mounts (Apple Container)
- Fix sandbox relative gitdir path resolution and improve warning messages for
  directory entries

## v0.1.184 (2026-04-17)

- Add `workmux rename [old-name] <new-name>` command to rename a worktree, its
  tmux window or session, agent state, and sandbox container marker. Pass
  `--branch` to also rename the underlying git branch
  ([#138](https://github.com/raine/workmux/issues/138))
- Add status tracking support for Gemini CLI, so Gemini agents now report
  working, waiting, and done states in the dashboard like Claude Code, Codex,
  and OpenCode. `workmux setup` auto-detects Gemini and installs the required
  hooks

## v0.1.183 (2026-04-15)

- Add `container.devices` config to expose host device nodes (e.g. `/dev/kvm`,
  `/dev/ttyUSB0`) to sandboxed containers
- Add `container.group_add` config to add supplementary groups (e.g. `dialout`,
  `video`) to the sandboxed process

## v0.1.182 (2026-04-13)

- Fix OpenCode status tracking plugin distribution by moving shipped plugin
  files into `resources/opencode/` and updating `workmux setup` and manual
  install instructions to install both `package.json` and the plugin file to
  OpenCode's global config directory
- Fix duplicate OpenCode busy and idle status events causing stale window status
  transitions
- Fix dashboard agent title prefixes

## v0.1.181 (2026-04-11)

- Add command palette to the dashboard, accessible via `:`. Provides a
  fuzzy-searchable list of available actions for the current context with their
  key hints

## v0.1.180 (2026-04-09)

- Add custom theme color overrides in config. Define custom colors under
  `theme.custom` to override any built-in theme's palette, using hex colors,
  named colors, or terminal color indices
  ([#128](https://github.com/raine/workmux/issues/128))
- Render tmux style codes in status icons. Icons configured with tmux styles
  like `#[fg=#da8548]●` now display with proper colors instead of raw style
  strings ([#130](https://github.com/raine/workmux/issues/130))
- Add `--tab` flag to dashboard command to open directly on a specific tab (e.g.
  `workmux dashboard --tab agents`)
  ([#127](https://github.com/raine/workmux/issues/127))

## v0.1.179 (2026-04-09)

- Respect the XDG Base Directory Specification for config, cache, and state
  paths. Custom `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, and `XDG_STATE_HOME` values
  are now honored, with automatic fallback to defaults. Existing setups continue
  to work without changes. ([#126](https://github.com/raine/workmux/issues/126))
- Fix dashboard ignoring `mode: session` config when creating, opening, or
  checking out worktrees ([#129](https://github.com/raine/workmux/issues/129))

## v0.1.178 (2026-04-07)

- Add `--fork` flag to `workmux add` for forking an existing Claude Code
  conversation into a new worktree, allowing the new agent to resume with full
  context from a previous session. Currently Claude Code only.

## v0.1.177 (2026-04-03)

- Fix OpenCode sandbox passing unnecessary OPENCODE_CONFIG environment variable
  to containers

## v0.1.176 (2026-04-03)

- Fix pi agent exiting immediately after prompt injection instead of staying in
  interactive mode ([#118](https://github.com/raine/workmux/issues/118)
- Clean up worktree-not-found error messages

## v0.1.175 (2026-04-03)

- Mount OpenCode's global config directory (`~/.config/opencode/`) into the
  sandbox, providing access to `opencode.json`, plugins, and global MCP
  definitions ([#117](https://github.com/raine/workmux/issues/117))

## v0.1.174 (2026-04-03)

- Fix concurrent `workmux add` commands failing with "could not lock config
  file" errors when creating multiple worktrees in parallel
  ([#116](https://github.com/raine/workmux/issues/116))

## v0.1.173 (2026-04-02)

- Add `--session` flag to `workmux sidebar` to scope the sidebar to a single
  tmux session instead of all sessions
- Add sandbox image freshness checking for Apple Container runtime, notifying
  when a newer image is available
  ([#115](https://github.com/raine/workmux/issues/115))

## v0.1.172 (2026-04-02)

- Automatically pull sandbox images when missing or stale, removing the need for
  manual `workmux sandbox pull`
  ([#115](https://github.com/raine/workmux/issues/115))
- Fix `sandbox.host_commands` shims not available in agents that use login
  shells (e.g. Codex, OpenCode)
  ([#114](https://github.com/raine/workmux/issues/114))

## v0.1.171 (2026-04-02)

- Fix sidebar daemon dropping all clients after 30s, causing sidebar panes to
  stop updating
- Fix sidebar width being too narrow in some windows when the sidebar was first
  opened from a smaller terminal
- Make project name more readable in sidebar tiles

## v0.1.170 (2026-04-01)

- Fail early with a clear error when the sandbox image is missing from the
  selected runtime's store ([#111](https://github.com/raine/workmux/issues/111))

## v0.1.169 (2026-04-01)

- Allow `sandbox.image` in project config (`.workmux.yaml`), so custom container
  images are no longer silently ignored when set per-project
- Fix Codex sandbox warnings about helper binaries in temporary directories and
  missing bubblewrap ([#110](https://github.com/raine/workmux/issues/110))
- Fix high CPU usage on Linux caused by inotify recursive watches when running
  multiple worktrees with active agents

## v0.1.168 (2026-04-01)

- Add manual sleeping toggle (`z` key) in the sidebar to deprioritize agents you
  don't need to monitor, regardless of their actual status
- Fix stale indicator incorrectly overriding working or waiting status icons in
  the sidebar, hiding statuses that require attention
- Fix git diff stats permanently freezing in linked worktrees when filesystem
  events originated from the shared gitdir

## v0.1.166 (2026-03-31)

- Fix relative `worktree_dir` paths (e.g. `../wm/`) being passed literally to
  sandbox commands instead of resolving `..` segments
  ([#105](https://github.com/raine/workmux/issues/105))
- Fix sidebar daemon signal error appearing in tmux panes on window/session
  changes ([#107](https://github.com/raine/workmux/issues/107))
- Fix Codex binary not found in Apple Container sandbox
  ([#106](https://github.com/raine/workmux/issues/106))
- Improve worktree name detection in the dashboard and sidebar, especially when
  running in generic tmux sessions
  ([#103](https://github.com/raine/workmux/pull/103))

## v0.1.165 (2026-03-30)

- Add `zoom: true` pane option to maximize a pane to fullscreen after creation,
  useful for giving an agent pane the full window while keeping other panes
  available in the background
  ([#102](https://github.com/raine/workmux/issues/102))

## v0.1.163 (2026-03-30)

- Add named layouts: define reusable pane layout presets in your config and
  apply them with `workmux add -l/--layout <name>`
  ([#101](https://github.com/raine/workmux/issues/101))
- Add `sandbox.env` config option for setting explicit environment variables
  inside sandboxed agents, with values redacted in debug logs
  ([#100](https://github.com/raine/workmux/pull/100))
- Detect interrupted agents via pane inactivity and show interruption status in
  the sidebar and dashboard with elapsed time since interruption
- Fix sidebar incorrectly showing all agents as done when only one finishes

## v0.1.162 (2026-03-29)

- Add named agents: define short names for agent commands in your global config
  and use them anywhere you'd specify an agent. Useful for multiple accounts,
  wrapper scripts, or commands with long environment variable overrides

## v0.1.158 (2026-03-29)

- Add `sidebar`: an always-visible agent status panel in a tmux side pane,
  showing live status, git diff stats, and elapsed timers for all agents across
  windows. Toggle with `workmux sidebar`. See the
  [sidebar guide](https://workmux.raine.dev/guide/dashboard/sidebar)
  - Two layout modes: tiles (default) and compact, switchable with `v` or via
    `sidebar.layout` in config
  - Click, scroll, or use keyboard navigation (`j`/`k`/`Enter`) to jump between
    agents
  - Width adapts to terminal size and reflows on resize
- Show a rebase indicator icon when a git rebase is in progress

## v0.1.157 (2026-03-28)

- Fix `last-done` sometimes navigating to the wrong agent when multiple agents
  finish close together
- `last-done` cycling is now reliable across repeated invocations, persisting
  state so the cycle survives even if the sorted order shifts

## v0.1.156 (2026-03-27)

- Dashboard now uses configured `main_branch` for diff base detection
  ([#97](https://github.com/raine/workmux/issues/97))
- Fix PR status fetch being delayed 30 seconds on dashboard open
- `workmux last-done` now includes waiting agents

## v0.1.154 (2026-03-27)

- Add Codex status tracking support, showing working/done states in the tmux
  window list

## v0.1.153 (2026-03-26)

- Fix incorrect guest home directory with Lima 2.1.0, which changed the path
  from `.linux` to `.guest` ([#92](https://github.com/raine/workmux/issues/92))
- Dashboard: add `r` hotkey to remove worktree from the agents tab
- Dashboard: show spinner on PR column header while fetching
- Fix Nix build by adding missing output hashes for crossterm git dependency

## v0.1.152 (2026-03-25)

- Add `workmux resurrect` command to restore worktree windows after a tmux or
  computer crash, automatically resuming agent conversations from where they
  left off
- Dashboard: add worktree creation modal with fzf-style branch picker, fuzzy
  search, tab completion, and the ability to checkout open pull requests
  directly via Ctrl+p or by typing a PR number
- Dashboard: dim background behind modal overlays for better visual focus

## v0.1.151 (2026-03-25)

- Dashboard: fix immediate exit on startup caused by a stray Enter keypress from
  launching the command being processed before the UI was ready

## v0.1.150 (2026-03-25)

- Add `--continue` flag to `workmux open` to resume the last agent conversation
  when opening a worktree
- List: show worktree age in `wm ls` output as a new AGE column with
  human-friendly relative time (e.g., 2h, 3d, 1w)

## v0.1.149 (2026-03-24)

- Dashboard: show elapsed time for pending PR checks and the name of failing
  checks across all views (PR column, agents pane, worktree info panel)

## v0.1.148 (2026-03-24)

- Dashboard: add base branch picker (`b`) to change a worktree's base branch
  from either the worktrees or agents tab
- Dashboard: open PR checks page in browser with `O` (shift-o) to quickly see
  why checks failed
- Dashboard: agent task descriptions now stay up to date in real time instead of
  showing the initial title
- Fix a crash caused by mouse coordinate overflow when using tmux 3.6a

## v0.1.147 (2026-03-23)

- Dashboard: open a worktree's pull request in the browser with `o`

## v0.1.146 (2026-03-23)

- Dashboard: add project picker (`p`) to switch between projects' worktrees
- Dashboard: show PR status and CI checks in worktree table and preview panel
- Dashboard: add worktree sort modes (`s`) to cycle between natural and
  newest-first ordering
- Dashboard: add age column to worktree table
- Dashboard: add close mux window action (`c`) to stop an agent while keeping
  the worktree
- Dashboard: redesigned worktree preview with info panel and styled git log

## v0.1.145 (2026-03-23)

- Dashboard: add worktree view as a second tab (press Tab to switch between
  Agents and Worktrees)
- Dashboard: add bulk sweep (R) to identify and remove worktrees ready for
  cleanup based on merged/closed PRs, deleted remote branches, or locally merged
  branches
- Dashboard: add X hotkey to kill an agent directly
- Dashboard: add worktree remove (r) with a context-aware confirmation modal
  that warns about uncommitted changes or unmerged commits
- Add `workmux list --json` flag for machine-readable output

## v0.1.144 (2026-03-22)

- Add `sync-files` command to re-apply file operations (copy/symlink) to
  existing worktrees, with `--all` flag to sync all worktrees at once

## v0.1.143 (2026-03-21)

- Fix `focus: true` not switching to the correct pane in session mode
  ([#86](https://github.com/raine/workmux/issues/86))

## v0.1.142 (2026-03-20)

- Add automatic worktree naming support for the pi agent
  ([#84](https://github.com/raine/workmux/issues/84))

## v0.1.141 (2026-03-19)

- Add `--prompt-file-only` flag for editors with embedded agents (e.g., neovim
  with an agent plugin) that consume prompts from the filesystem instead of pane
  injection ([#82](https://github.com/raine/workmux/issues/82))
- `workmux open` now accepts multiple worktree names in a single command (e.g.,
  `workmux open foo bar`)

## v0.1.140 (2026-03-17)

- Add pi agent support for status tracking and setup
  ([#81](https://github.com/raine/workmux/issues/81))

## v0.1.139 (2026-03-16)

- Add configurable theme for dashboard
- Auto-detect dark/light mode from terminal background
- Press `T` (shift+t) in the dashboard to cycle through color schemes; selection
  persists to config
- Redesign dashboard footer

## v0.1.138 (2026-03-14)

- Add `/` hotkey in the dashboard to filter agents by project and worktree name
- Add `config reference` subcommand to display the full annotated default config
  with all available options
- Add `/workmux` skill that teaches agents how to use workmux

## v0.1.137 (2026-03-12)

- Fix multiline paste not being submitted automatically because the Enter
  keystroke arrived before the application finished processing the pasted
  content

## v0.1.136 (2026-03-11)

- Add `base_branch` config option to set a default base branch for new
  worktrees, so they always branch off a specific branch (e.g. main) instead of
  whatever is currently checked out. The `--base` CLI flag takes precedence over
  config ([#78](https://github.com/raine/workmux/issues/78))

## v0.1.135 (2026-03-11)

- Fix auto-generated branch names containing garbage characters with kiro-cli

## v0.1.134 (2026-03-09)

- Apple Container sandboxes now default to 16 GB memory limit, preventing OOM
  kills during heavy workloads. Memory and CPU limits are configurable via
  `container.memory` and `container.cpus` in your config
  ([#77](https://github.com/raine/workmux/issues/77))
- Fork PRs checked out with `--pr` now automatically prefix the local branch
  name with the fork owner (e.g., `forkowner-main`), preventing conflicts when
  the fork's branch name matches an existing local branch

## v0.1.133 (2026-03-09)

- Fixed OpenCode plugin's waiting (💬) status not triggering when the agent
  requests permission or asks a multiple-choice question, caused by event name
  changes in OpenCode v2 ([#75](https://github.com/raine/workmux/pull/75))
- Added logging for branch name auto-generation

## v0.1.132 (2026-03-06)

- Added support for Kiro CLI (`kiro-cli`) as a recognized agent
- Added support for Mistral Vibe (`vibe`) as a recognized agent
  ([#76](https://github.com/raine/workmux/issues/76))

## v0.1.131 (2026-03-05)

- New `--session` flag for `workmux open` lets you open worktrees in a dedicated
  tmux session instead of a window. Session mode is persisted, so reopening the
  same worktree remembers the preference
  ([#73](https://github.com/raine/workmux/pull/73))
- Dashboard now has a scope filter (toggle with `F`) to show only agents in the
  current session or all agents across sessions
  ([#74](https://github.com/raine/workmux/issues/74))
- `workmux setup` now offers to install bundled skills (merge, rebase, worktree,
  coordinator) during the first-run wizard
- Agents can now communicate across projects using the coordinator skill
- Fixed session mode not being detected correctly when reopening a worktree
  after a tmux restart

## v0.1.130 (2026-03-04)

- Window names are now automatically suffixed with the project directory name
  when a name collision is detected across different repositories, avoiding
  errors when multiple repos use the same worktree name. Explicit names set via
  `--name` are not modified ([#70](https://github.com/raine/workmux/issue/70))

## v0.1.129 (2026-03-02)

- Branch name generation now automatically uses your configured AI agent
  (Claude, Gemini, Codex, or OpenCode) instead of requiring the `llm` CLI to be
  installed ([#68](https://github.com/raine/workmux/pull/68))
  - **Note**: This is a breaking change. If you were previously using `llm` to
    generate branch names. Add `auto_name.command: 'llm'` to your global config.
- New `auto_name.command` config option lets you specify a custom command for
  branch name generation
- `workmux rm` no longer fails with "cannot delete branch used by worktree" when
  a previous `workmux add` was interrupted mid-creation
- Fixed zsh completions not working when installed via fpath autoloading
  ([#65](https://github.com/raine/workmux/pull/65))
- Fixed zsh tab completion suggesting file paths for commands that only accept
  worktree handles or branch names
  ([#65](https://github.com/raine/workmux/pull/65))
- Fixed phantom whitespace appearing in zsh completions when no candidates exist
  ([#65](https://github.com/raine/workmux/pull/65))

## v0.1.128 (2026-03-01)

- Dashboard now renders colored status icons correctly instead of showing raw
  tmux color codes as literal text
  ([#66](https://github.com/raine/workmux/issues/66))

## v0.1.127 (2026-02-28)

- Sandbox: Images copied to the host clipboard can now be pasted into sandboxed
  agents (Ctrl+V), enabling workflows like sharing screenshots with Claude Code
  running inside a container or VM
- Merge skill: Added `--no-verify` (`-n`) flag to skip pre-merge hooks, and `-k`
  as a shorthand alias for `--keep`

## v0.1.126 (2026-02-28)

- Added `workmux update` command for self-updating workmux directly from GitHub
  releases. Downloads the latest version, verifies checksums, and replaces the
  binary in place. Homebrew-managed installs are detected and directed to use
  `brew upgrade` instead
- workmux now automatically checks for updates in the background and shows a
  notification when a newer version is available. Checks happen at most once per
  day during `workmux add`. Disable with `auto_update_check: false` in config or
  by setting the `WORKMUX_NO_UPDATE_CHECK` environment variable

## v0.1.125 (2026-02-28)

- Added Apple Container as a sandbox runtime alongside Docker and Podman,
  enabling sandboxing on macOS using Apple's native container technology (macOS
  26+, Apple Silicon). Auto-detected when the `container` binary is available.
  Configure with `runtime: apple-container` or let workmux detect it
  automatically
- Fixed `close` command not finding the correct worktree when using a branch
  name that differs from the worktree handle (e.g., when created with `--name`)

## v0.1.124 (2026-02-24)

- Added experimental Zellij backend support. Zellij is auto-detected when
  running inside a Zellij session. Requires Zellij built from source (uses
  unreleased features). See the
  [Zellij guide](https://workmux.raine.dev/guide/zellij) for details and known
  limitations (contributed by [@Infonautica](https://github.com/Infonautica))

## v0.1.123 (2026-02-24)

- Add GitHub Copilot CLI as a supported agent for status tracking. Copilot hooks
  are installed per-repository via `workmux setup`. Note: the waiting state is
  not supported due to Copilot CLI hooks API limitations

## v0.1.122 (2026-02-23)

- Fork branch references (e.g., `someuser:feature`) now prefix the local branch
  name with the fork owner (`someuser-feature`), preventing conflicts with
  existing branches like `main`

## v0.1.120 (2026-02-21)

- Built-in agents (`claude`, `gemini`, `codex`, `opencode`) are now
  auto-detected in pane commands, so prompt injection works without the
  `<agent>` placeholder or a matching `agent` config. Just use the agent name
  directly as the pane command (e.g., `command: "codex --yolo"`) and prompts are
  delivered automatically. ([#57](https://github.com/raine/workmux/issues/57))

## v0.1.119 (2026-02-21)

- Added session mode: worktrees can now be created as their own tmux sessions
  instead of windows, giving each worktree a separate window list, history, and
  layout. Enable with `--session` flag or `mode: session` in config.
- Added multi-window sessions: use the `windows` config to create multiple
  windows per session, each with its own pane layout - useful for setups like an
  editor window alongside a test runner

## v0.1.118 (2026-02-19)

- Added `workmux setup` command to automatically detect installed agents and
  configure status tracking hooks, with a guided install prompt and a tmux
  status bar preview showing what the icons look like
- Fixed sandbox failing to start with Colima (Docker Desktop alternative for
  macOS) due to shim directories being created in system temp paths that
  Colima's VM cannot access

## v0.1.117 (2026-02-16)

- Fixed backend detection for nested multiplexers (e.g., tmux inside kitty or
  wezterm) so workmux correctly targets the innermost multiplexer
  ([#53](https://github.com/raine/workmux/issues/53))
- Added `WORKMUX_BACKEND` environment variable to explicitly override backend
  auto-detection (accepts `tmux`, `wezterm`, or `kitty`)

## v0.1.116 (2026-02-15)

- Fixed hooks and run commands failing when they use bash-specific syntax (e.g.,
  arrays, process substitution), by using bash instead of sh for execution
  ([#52](https://github.com/raine/workmux/pull/52))

## v0.1.114 (2026-02-15)

- Sandbox: Host git identity (user.name, user.email) is now automatically
  available inside sandbox environments, so git commits from sandboxed agents
  use the correct author

## v0.1.112 (2026-02-13)

- Added sandbox support for running agents in isolated environments. Two
  backends: containers (Docker/Podman) for ephemeral sessions, and Lima VMs for
  persistent machines with built-in Nix/Devbox toolchain support. See the
  [sandbox guide](https://workmux.raine.dev/guide/sandbox/) for setup.

## v0.1.111 (2026-02-12)

- Dashboard: Added Ctrl+N/Ctrl+P as alternative keybindings for navigating
  between rows
- Fixed bash completion panic when generating completions
  ([#51](https://github.com/raine/workmux/pull/51))

## v0.1.110 (2026-02-11)

- Added kitty as an alternative terminal backend -- detected automatically when
  running inside kitty
- Improved window cleanup handling for non-tmux backends

## v0.1.109 (2026-02-09)

<!-- summary: run command, coordinator commands, light theme -->

- Added coordinator commands for scripting multi-agent workflows:
  - `send` sends text or file contents to a worktree's agent pane
  - `capture` reads the last N lines from a worktree's pane output
  - `status` shows the current state of worktree agents with elapsed time and
    git info (use `--git` for staged/unstaged indicators)
  - `wait` blocks until agents reach a target status (working, waiting, or done)
  - `run` executes a command in a worktree's pane and streams the output in real
    time.
- Dashboard now supports light theme via `theme: light` in config

## v0.1.108 (2026-02-07)

- The `list` command now shows an AGENT column displaying the status of agents
  running in each worktree (working, waiting, done icons)
- Added positional arguments to `list` for filtering by worktree handle or
  branch name
- When piping output, agent status icons are replaced with text labels for
  compatibility with scripts

## v0.1.107 (2026-02-04)

- Shell autocompletion now suggests worktree names for the `close` command
  (bash, zsh, fish) ([#47](https://github.com/raine/workmux/issues/47))

## v0.1.106 (2026-02-04)

- Fixed dashboard incorrectly showing the worktree directory name instead of the
  project name when using a custom `worktree_dir` configuration
  ([#48](https://github.com/raine/workmux/pull/48))

## v0.1.105 (2026-01-31)

- Nerdfont setup now handles read-only config files gracefully (e.g., when
  symlinked to a Nix store), showing a helpful message instead of failing

## v0.1.104 (2026-01-31)

- Added `-o`/`--open-if-exists` flag to `workmux add` for idempotent worktree
  creation: if the worktree already exists, switches to it instead of failing.

## v0.1.103 (2026-01-30)

- Dashboard now shows PR status column with number and state icon (open, merged,
  closed, draft) for each agent's worktree
- Dashboard displays CI/CD check status alongside PRs with pass/fail/pending
  icons. Enable `dashboard.show_check_counts` to show pass/total counts
- Added last-agent toggle (Tab key in dashboard, `workmux last-agent` CLI) to
  quickly switch between current and previous agent
- Added `auto_name.background` config option to always run `--auto-name` agents
  in background mode
- Improved dashboard startup performance

## v0.1.102 (2026-01-29)

- Moved internal state management from tmux-specific mechanisms to
  filesystem-based JSON storage, laying the groundwork for multi-backend support
- Added experimental WezTerm backend support. workmux auto-detects the backend
  from environment variables. See the
  [WezTerm guide](https://workmux.raine.dev/guide/wezterm) for setup
  instructions. (contributed by [@JeremyBYU](https://github.com/JeremyBYU))
- New worktrees now automatically get a symlink to a gitignored
  `CLAUDE.local.md` from your main worktree, so your local Claude Code
  instructions are available without manual setup

## v0.1.101 (2026-01-29)

- Fixed status icons breaking tmux themes that use padding spaces in window
  format strings ([#45](https://github.com/raine/workmux/pull/45))

## v0.1.100 (2026-01-26)

- Added nested config support for monorepos: place a `.workmux.yaml` in any
  subdirectory to configure that project independently. When you run workmux
  from a subdirectory, it finds the nearest config. Working directory, file
  operations, and hooks are all scoped to the config directory.
  ([#39](https://github.com/raine/workmux/issues/39))
- Added `WM_CONFIG_DIR` environment variable for hooks, pointing to the
  directory containing the `.workmux.yaml` that was used

## v0.1.99 (2026-01-24)

- Dashboard: Detect and clear working agents that have stalled, for example due
  to being interrupted
- Fixed `last-done` command intermittently failing when switching to recently
  completed agents

## v0.1.98 (2026-01-23)

- Tmux window names now use a nerdfont git branch icon as the default prefix
  when nerdfonts are available, replacing the previous "wm-" prefix. This can be
  overridden with `window_prefix`
- Fixed cleanup commands outputting noise to the terminal after merging or
  removing worktrees

## v0.1.97 (2026-01-23)

- Added bash installer script for easier installation (`curl -fsSL ... | bash`)
- Added automatic nerdfont detection with fallback icons for users without
  nerdfonts installed
- Added `last-done` command to quickly switch to recently completed agents
- Fixed race condition when running merge from inside a worktree agent

## v0.1.96 (2026-01-20)

- Reduced crate download size by excluding unnecessary files from the published
  package

## v0.1.95 (2026-01-20)

- Fixed prompts failing when branch names contain slashes
  ([#37](https://github.com/raine/workmux/issues/37))

## v0.1.94 (2026-01-17)

- Fixed dashboard commit and merge commands not working in Claude Code when
  using bash command prefix (`!`)
- Fixed dashboard commands including a literal newline that caused issues with
  OpenCode ([#35](https://github.com/raine/workmux/issues/35))
- Dashboard: Added `--diff` flag to open diff view directly for the current
  worktree, skipping the agent list

## v0.1.93 (2026-01-15)

- Added Nix flake (https://workmux.raine.dev/guide/nix)
- Fixed bash completion not passing arguments to the fallback completion
  function

## v0.1.92 (2026-01-14)

- Duplicate windows created with `open --new` are now placed immediately after
  the original window instead of at the end of the window list
- `open --new` can now be run without a name argument when inside a worktree,
  inferring the current worktree automatically

## v0.1.91 (2026-01-14)

- Fixed `merge` command failing with bare repo setups that use linked worktrees
  ([#31](https://github.com/raine/workmux/issues/31))

## v0.1.90 (2026-01-13)

- Fixed false "unmerged commits" warning when local main branch is ahead of the
  remote ([#30](https://github.com/raine/workmux/issues/30))

## v0.1.88 (2026-01-13)

- The `merge` command now works when the target branch is checked out in a
  linked worktree ([#29](https://github.com/raine/workmux/issues/29))

## v0.1.87 (2026-01-13)

- Fixed `workmux add user/feature` incorrectly treating `user` as a remote name
  instead of creating a local branch named `user/feature`
  ([#28](https://github.com/raine/workmux/issues/28))
- Fixed worktree cleanup failing to run process stop hooks by deferring
  directory deletion

## v0.1.86 (2026-01-11)

- Dashboard: Preview pane size is now configurable via config file, CLI flag
  (`--preview-size`/`-P`), or interactively with `+`/`-` keys

## v0.1.85 (2026-01-11)

- Dashboard: Selection now stays on the same agent when the list reorders due to
  status changes or sorting

## v0.1.84 (2026-01-11)

- Dashboard: Improved file list layout with full paths and right-aligned stats

## v0.1.83 (2026-01-10)

- Dashboard: Added file list sidebar to diff and patch views
- Dashboard: Added Ctrl+D/U scrolling in patch mode
- Dashboard: Improved diff coloring fallback when delta is not available

## v0.1.81 (2026-01-10)

- Dashboard: Added help screen accessible with `?` key, showing keybindings for
  each view (dashboard, diff, patch mode)
- Dashboard: Added mouse scroll support in diff views
- Dashboard: The active worktree is now highlighted with a subtle background and
  white text for easier identification
- Dashboard: Git column header shows a spinner while refreshing
- Fixed agent status not showing "working" when launching with a prompt (works
  around
  [Claude Code v2.0.77 regression](https://github.com/anthropics/claude-code/issues/17284))

## v0.1.80 (2026-01-09)

- Dashboard: Commit and merge actions are now configurable via
  `dashboard.commit` and `dashboard.merge` in your config file

## v0.1.79 (2026-01-09)

- Dashboard: Fixed patch mode showing already-staged hunks
- Dashboard: Uncommitted changes are now shown for the main worktree

## v0.1.78 (2026-01-08)

- Dashboard: Added patch mode for interactive hunk-by-hunk staging with `p` key
- Dashboard: Added hunk splitting to stage partial changes within a hunk
- Dashboard: Added ability to undo staged changes in patch mode
- Dashboard: Added hunk commenting for review workflows
- Dashboard: Added diff browsing with `d` to view uncommitted changes and `D`
  for committed changes (toggle between WIP/review views with Tab)
- Dashboard: Diffs now use delta for syntax highlighting when available, with
  fallback coloring
- Dashboard: Added filter to hide stale agents with `f` key (persists across
  sessions)
- Dashboard: Added `c` to commit and `m` to merge directly from the main view
- Dashboard: Working agents now show an animated spinner

## v0.1.77 (2026-01-07)

- Dashboard: Added git status column showing diff stats (+/- lines), conflict
  indicator, dirty state, and ahead/behind counts for each worktree
- Dashboard: Non-default base branches (not main/master) are now displayed in
  the git column
- Added `--notification` flag to `merge` command to show a system notification
  on successful merge

## v0.1.76 (2026-01-07)

- Dashboard: Renamed "Agent" column to "Worktree" for clarity; non-workmux
  agents now display "main" instead of their window name
- The `merge` command now auto-detects the base branch from when the worktree
  was created, instead of always defaulting to main
- Window status icons for "waiting" and "done" again auto-clear when returning
  to the pane

## v0.1.75 (2026-01-06)

- Added OpenCode support for agent status tracking in tmux window names
- Fixed passing prompt to OpenCode

## v0.1.74 (2026-01-06)

- Dashboard: Stale agents (inactive for over an hour) now show a timer icon
  instead of "stale"
- Dashboard: Preview updates are now faster in input mode

## v0.1.72 (2026-01-06)

- Renamed "status popup" to "dashboard"

## v0.1.71 (2026-01-06)

- Added pane preview to the status dashboard, showing live terminal output from
  the selected agent
- Added input mode: press `i` to send keystrokes directly to the selected
  agent's pane without switching windows, press Escape to exit
- Added preview scrolling with Ctrl+U/D
- Agents are now automatically removed from the status list when they exit
- Priority sorting now uses elapsed time as a tiebreaker

## v0.1.70 (2026-01-06)

- Added smart sorting to the status dashboard with four modes: Priority (by
  status importance), Project (grouped by project), Recency (newest first), and
  Natural (tmux order). Press `s` to cycle through modes; it is saved across
  sessions.

## v0.1.69 (2026-01-05)

- Added `status` command: a TUI dashboard for monitoring all active agents
  across tmux sessions, with quick-jump keys (1-9), peek mode, and keyboard
  navigation
- The "done" (✅) status no longer gets replaced by "waiting" (💬) when Claude
  sends idle prompts, so completed sessions stay marked as done

## v0.1.68 (2026-01-05)

- Added `docs` command to view the README

## v0.1.67 (2026-01-04)

- Improved compatibility with non-POSIX shells like nushell
- Commands for starting agent with a prompt no longer pollute shell history

## v0.1.66 (2026-01-03)

- Added `--no-verify` (`-n`) flag to `merge` command to skip pre-merge hooks
- The `merge` command now works when run from subdirectories within a worktree

## v0.1.65 (2026-01-02)

- The `open` command now switches to an existing window by default instead of
  erroring when a window already exists
- Added `--new` (`-n`) flag to `open` command to force opening a duplicate
  window (creates suffix like `-2`, `-3`)
- The `open` command now supports prompts via `-p`, `-P`, and `-e` flags,
  matching the `add` command

## v0.1.63 (2026-01-02)

- Linux binaries now use musl for better compatibility across different Linux
  distributions

## v0.1.62 (2025-12-29)

- The `merge` command with `--keep` no longer requires a clean worktree, since
  the worktree won't be deleted anyway

## v0.1.61 (2025-12-27)

- Log files are now stored in the XDG state directory
  (`~/.local/state/workmux/`)

## v0.1.60 (2025-12-26)

- Added `close` command to close a worktree's tmux window while keeping the
  worktree on disk. It's basically an alias for tmux's `kill-window`

## v0.1.59 (2025-12-26)

- Added `pre_merge` hook to run commands (like tests or linters) before merging,
  allowing you to catch issues before they land in your main branch
- Added `pre_remove` hook that runs before worktree removal, with environment
  variables (`WM_HANDLE`, `WM_WORKTREE_PATH`, `WM_PROJECT_ROOT`) for backup or
  cleanup workflows
- The `post_create` hook now receives `WM_WORKTREE_PATH` and `WM_PROJECT_ROOT`
  environment variables, matching the other hooks

## v0.1.57 (2025-12-23)

- Fixed terminal input not being displayed after creating a worktree with
  `workmux add` on bash ([#17](https://github.com/raine/workmux/pull/17))

## v0.1.55 (2025-12-21)

- The `merge` command now allows untracked files in the target worktree, only
  blocking when there are uncommitted changes to tracked files

## v0.1.54 (2025-12-17)

- The `remove` command now accepts multiple worktree names, allowing you to
  clean up several worktrees in a single command (e.g.,
  `workmux rm feature-a feature-b`)

## v0.1.53 (2025-12-17)

- Added JSON lines support for stdin input: pipe JSON objects to `workmux add`
  and each key automatically becomes a template variable, making it easy to use
  structured data from tools like `jq` in prompts and branch names
- Template errors now show which variables are missing and list available ones,
  helping catch typos in branch name templates or prompts before worktrees are
  created
- Fixed "directory already exists" errors when creating worktrees after a
  previous cleanup was interrupted by background processes recreating files

## v0.1.52 (2025-12-17)

- Added `--max-concurrent` flag to limit how many worktrees run simultaneously,
  useful for creating worker pools that process items without overwhelming
  system resources or hitting API rate limits
- Added `{{ index }}` template variable for branch names and prompts in
  multi-worktree modes, providing a 1-indexed counter across all generated
  worktrees

## v0.1.51 (2025-12-16)

- Added `--wait` (`-W`) flag to `add` command to block until the created tmux
  window is closed, useful for scripting workflows
- Added stdin input support for multi-worktree generation: pipe lines to
  `workmux add` to create multiple worktrees, with each line available as
  `{{ input }}` in prompts
- Fixed duplicate remote fetch when using `--pr` or fork branch syntax
  (`user:branch`)

## v0.1.50 (2025-12-15)

- Fixed a crash in `workmux completions bash`
  ([#14](https://github.com/raine/workmux/issues/14))

## v0.1.49 (2025-12-15)

- Added `--all` flag to `remove` command to remove all worktrees at once (except
  the main worktree), with safety checks for uncommitted changes and unmerged
  commits
- Now shows an error when using `-p`/`--prompt` without an agent pane
  configured, instead of silently ignoring the prompt

## v0.1.48 (2025-12-10)

- Removed automatic `node_modules` symlink default for Node.js projects

## v0.1.47 (2025-12-09)

- Added `--gone` flag to `rm` command to clean up worktrees whose remote
  branches have been deleted (e.g., after PRs are merged)

## v0.1.46 (2025-12-09)

- Added `--pr` flag to `list` command to show PR status alongside worktrees,
  displaying PR numbers and state icons (open, draft, merged, closed)
- Added spinner feedback for slow operations like GitHub API calls

## v0.1.45 (2025-12-08)

- Shell completions now suggest proper values for `--base`, `--into`, and
  `--prompt-file` flags (bash, zsh)
- Fixed an error with the `pre_delete` hook when removing worktrees that were
  manually deleted from the filesystem

## v0.1.44 (2025-12-06)

- In agent status tracking, the "waiting" (💬) status icon now auto-clears
  window is focused, matching the behavior of the "done" (✅️) status.

## v0.1.43 (2025-12-05)

- Improved the default config template generated by `workmux init`

## v0.1.42 (2025-12-05)

- Added pre-built binaries for Linux ARM64 (aarch64) architecture

## v0.1.41 (2025-12-04)

- Commands `open`, `path`, `remove`, and `merge` now accept worktree names (the
  directory name shown in tmux) in addition to branch names, making it easier to
  work with worktrees when the directory name differs from the branch

## v0.1.40 (2025-12-03)

- Added `--auto-name` (`-A`) flag to automatically generate branch names from
  your prompt using an LLM (uses the `llm` tool), so you can skip naming
  branches yourself
- Added `auto_name.model` and `auto_name.system_prompt` config options to
  customize the LLM model and prompt used for branch name generation

## v0.1.39 (2025-12-03)

- New worktree windows are now inserted after the last workmux window instead of
  at the end of the window list, keeping your worktree windows grouped together

## v0.1.38 (2025-12-03)

- Fixed branches created with `--base` not having upstream tracking
  configuration properly unset from the base branch

## v0.1.37 (2025-12-03)

- Fixed panes not loading shell profiles, which broke tools like nvm etc. that
  depend on login shell initialization

## v0.1.36 (2025-12-01)

- Added `--into` flag to `merge` command for merging into branches other than
  main (e.g., `workmux merge feature --into develop`)
- Fixed config loading and file operations when running commands from inside a
  worktree
- Removed `--delete-remote` flag from `merge` and `remove` commands

## v0.1.35 (2025-12-01)

- Added agent status tracking in tmux window names, showing icons for different
  Claude Code states (🤖 working, 💬 waiting, ✅ done). The "done" status
  auto-clears when you focus the window.

## v0.1.34 (2025-11-30)

- Fixed worktree path calculation when running `add` from inside an existing
  worktree, which previously created nested paths instead of sibling worktrees

## v0.1.33 (2025-11-30)

- Added support for GitHub fork branch format (`user:branch`) in `add` command,
  allowing direct checkout of fork branches copied from GitHub's UI

## v0.1.32 (2025-11-30)

- Added OpenCode agent support: prompts are now automatically passed using the
  `-p` flag when using `--prompt-file` or `--prompt-editor` with
  `--agent opencode`

## v0.1.31 (2025-11-29)

- Added `path` command to get the filesystem path of a worktree by branch name
- Added `--name` flag to `add` command for explicit worktree directory and tmux
  window naming
- Added `worktree_naming` config option to control how worktree names are
  derived from branches (`full` or `basename`)
- Added `worktree_prefix` config option to add a prefix to all worktree
  directory names
- Added `merge_strategy` config option to set default merge behavior (merge,
  rebase, or squash)

## v0.1.30 (2025-11-27)

- Added nushell support for pane startup commands
- Improved reliability of pane command execution across different shells

## v0.1.29 (2025-11-26)

- Shell completions now suggest git branch names when using the `add` command

## v0.1.28 (2025-11-26)

- Shell completions now dynamically suggest branch names when pressing TAB for
  `open`, `merge`, and `remove` commands (bash, zsh, fish)

## v0.1.26 (2025-11-25)

- Added `--pr` flag to checkout a GitHub pull request directly into a new
  worktree
- Fixed version managers (nvm, pnpm, mise, etc.) being shadowed by stale PATH
  entries when running pane commands
- Improved list output with cleaner table formatting and relative paths
- Fixed duplicate command announcement when running merge workflow

## v0.1.24 (2025-11-22)

- Fixed "can't find pane: 0" errors when using `pane-base-index 1` in tmux
  configuration
- Merge conflicts now abort cleanly, keeping your main worktree in a usable
  state with guidance on how to resolve

## v0.1.23 (2025-11-22)

- Added `--keep` flag to merge command to merge without cleaning up the
  worktree, useful for verifying the merge before removing the branch
- Fixed a bug where multi-agent worktrees had incorrect agent configuration for
  worktrees after the first one
- After closing a worktree (merge or remove), the terminal now navigates back to
  the main worktree instead of staying in the deleted directory

## v0.1.22 (2025-11-21)

- Added YAML frontmatter support in prompt files for defining variable matrices
  (`foreach`), making it easier to specify multi-worktree generation without CLI
  flags
- Added `size` and `percentage` options for pane configuration to control pane
  dimensions when splitting
- Fixed prompt editor temporary file now using `.md` extension for better editor
  syntax highlighting
- Fixed Gemini agent startup issues

## v0.1.21 (2025-11-18)

- Switched templating engine from Tera to MiniJinja (Jinja2-compatible) for
  branch names and prompts. Existing templates should work unchanged.

## v0.1.20 (2025-11-18)

- Fixed prompts starting with a dash (e.g. "- foo") being incorrectly
  interpreted as CLI flags
- The `rm` command now automatically uses the correct base branch that was used
  when the worktree was created, instead of defaulting to the main branch

## v0.1.19 (2025-11-17)

- Added `--with-changes` flag to `add` command: move uncommitted changes from
  your current worktree to a new one, useful when you've started working on the
  wrong branch
- Added `--patch` flag: interactively select which changes to move when using
  `--with-changes`
- Added `--include-untracked` (`-u`) flag: include untracked files when moving
  changes

## v0.1.18 (2025-11-17)

- New branches now default to branching from your currently checked out branch
  instead of the main branch's remote tracking branch
- Removed the `--from-current` flag (no longer needed since this is now the
  default behavior)

## v0.1.17 (2025-11-17)

- Added multi-agent workflows: create multiple worktrees from a single command
  using `-a agent1 -a agent2`, `-n count`, or `--foreach` matrix options
- Added background mode (`-b`, `--background`) to create worktrees without
  switching to them
- Added support for prompt templating with variables like `{{ agent }}`,
  `{{ num }}`, and custom `--foreach` variables
- Added `--branch-template` option to customize generated branch names

## v0.1.16 (2025-11-16)

- Added `--prompt-editor` (`-e`) flag to write prompts using your `$EDITOR`
- Added configurable agent support with `--agent` (`-a`) flag and config option
- Added flags to skip setup steps: `--no-hooks`, `--no-file-ops`,
  `--no-pane-cmds`
- Defaulted to current branch as base for `workmux add` (errors on detached HEAD
  without explicit `--base`)
- Fixed aliases containing `<agent>` placeholder not resolving correctly

## v0.1.15 (2025-11-15)

- Added `--prompt` (`-p`) and `--prompt-file` (`-P`) options to `workmux add`
  for attaching a prompt to new worktrees
- Added `--keep-branch` (`-k`) option to `workmux remove` to preserve the local
  branch while removing the worktree and tmux window

## v0.1.14 (2025-11-14)

- Added `--base` option to specify a base branch, commit, or tag when creating a
  new worktree
- Added `--from-current` (`-c`) flag to use the current branch as the base,
  useful for stacking feature branches
- Added support for creating worktrees from remote branches (e.g.,
  `workmux add origin/feature-branch`)
- Added support for copying directories (not just files) in file operations

## v0.1.13 (2025-11-13)

- Fixed `merge` and `remove` commands failing when run from within the worktree
  being deleted
- Added safety check to prevent accidentally deleting a branch that's checked
  out in the main worktree
- Fixed pane startup commands not loading shell environment tools (like direnv,
  nvm, rbenv) before running

## v0.1.11 (2025-11-11)

- Added `pre_delete` hooks that run before worktree deletion, with automatic
  detection of Node.js projects to fast-delete `node_modules` directories in the
  background
- Pane commands now keep an interactive shell open after completion, and panes
  can be created without a command (just a shell)
- Added `target` option for panes to split from any existing pane, not just the
  most recent one
- Tmux panes now use login shells for consistent environment across all panes
- The `create` command now displays which base branch was used
- Improved validation for pane configurations with helpful error messages

## v0.1.10 (2025-11-09)

- Post-create hooks now run before the tmux window opens, so the new window
  appears ready to use instead of showing setup commands running

## v0.1.9 (2025-11-09)

- Fixed cleanup when removing a worktree from within its own tmux window

## v0.1.7 (2025-11-09)

- Fixed a race condition where cleaning up a worktree could fail if the tmux
  window hadn't fully closed yet

## v0.1.6 (2025-11-09)

- Automatically run `pnpm install` when creating worktrees in pnpm projects

## v0.1.5 (2025-11-08)

- Fixed global config to always load from `~/.config/workmux/` instead of
  platform-specific locations (e.g., `~/Library/Application Support/` on macOS)

## v0.1.4 (2025-11-07)

- Added `--from` flag to `add` command to specify which branch, commit, or tag
  to branch from
- Fixed `rm` command failing when run from within the worktree being removed
- New worktree branches no longer track a remote upstream by default

## v0.1.3 (2025-11-06)

- Added global configuration support with XDG compliance—you can now set shared
  defaults in `~/.config/workmux/config.yaml` that apply across all projects
- Project configs can inherit from global settings using `<global>` placeholder
  in lists
- After merging or removing a worktree, automatically switches to the main
  branch tmux window if it exists
- Fixed an issue where removing a worktree could fail if the current directory
  was inside that worktree

## v0.1.2 (2025-11-05)

- Fixed `prune` command to correctly parse Claude Code's config file structure

## v0.1.1 (2025-11-05)

Initial release.

- Added `open` command to switch to an existing worktree's tmux window
- Added `--rebase` and `--squash` merge strategies to the `merge` command
- Added `claude prune` command to clean up stale worktree entries from Claude's
  config
- Added configurable window name prefix via `window_prefix` setting
- Allowed `remove` command to work without arguments to remove the current
  branch
- Shell completion now works with command aliases
- Fixed merge command not cleaning up worktrees after merging
- Fixed worktree deletion issues when running from within the worktree
- Fixed new branches being incorrectly flagged as unmerged
