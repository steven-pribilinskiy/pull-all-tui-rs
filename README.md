# polygit

Interactive polyrepo git dashboard. Discovers every git repo under a directory and shows their status in a two-pane TUI — pulling them in parallel with live per-repo logs, with retry/refetch support, a persisted status cache, and configurable auto-pull. This is the canonical Rust implementation; it also fronts the Go, Bun, and bash alternatives via subcommands.

📖 **Documentation: https://steven-pribilinskiy.github.io/polygit**

## Features

- **Recursive discovery** (default): crawls the directory tree in parallel for git repos, pruning hidden / `node_modules` / `vendor` / `target` / `*.worktrees` dirs and never descending into a found repo — so `polygit ~/projects` (or even `~`) just works. Repos stream in and start pulling as they're found; `--depth N` caps it, `--no-recursive` restores a single-level scan
- **Directory-tree view** (`v t`): render the repos as a collapsible folder tree, with per-folder status rollups; orthogonal to grouping, so you can have flat, grouped, tree, or **tree + groups** (groups subdivide repos inside each folder)
- Parallel pulls with configurable concurrency (default: nproc); the list title shows live concurrency (`⇄ active/cap`)
- Live log streaming per repo in a scrollable preview pane
- Status glyphs: queued / running / up-to-date / updated / no-upstream / skipped / throttled / failed — plus an optional **status text column** (`t u`) that spells them out and names the failure kind on failed repos (`not found` / `auth` / `diverged` / `not a repo` / `timeout` / `network` / `lock`)
- Branches with no upstream are a distinct **no-upstream** state (`▽`), not a failure — kept off the Errors page and counted as done. A branch whose tracked remote ref was deleted (PR merged → "no such ref was fetched") is treated the same way, not as a red failure
- **Throttle adaptation**: detects remote rate-limiting (HTTP 429 / "rate limit" / SSH connection throttling) as a distinct **throttled** state (`↯`), shows a warning banner, automatically halves concurrency, and re-queues throttled repos with exponential backoff — restoring full concurrency once the remote is quiet
- Automatic one-shot retry of a failed pull before marking it failed — skipped for permanent errors (repository not found, auth failure, diverged branch) where retrying can't change the result
- Dynamic `Errors (N)` page (after `Result`) listing each failed repo with its error output
- **What the pull delivered**: optional **pulled** (`t p`, `⇣N` commits) and **changed** (`t c`, `±N` files) columns showing what each repo's pull landed this run; the info panel's **Pulled** row spells out the full delta — `oldsha → newsha · N commits · M files (+ins −del) · N new tags · N new branches` — and the panel re-fetches after a pull so its numbers reflect the new HEAD
- **Optional auto-pull + status cache**: launches are useful instantly — the list is seeded from a persisted per-repo status cache (last-known status shown **dim with an age**, e.g. `up-to-date 2d`, until pulled). Auto-pull-on-launch is configurable in Settings (master on/off · a max-repos limit `50/100/250/∞` · suppress in tree view); by default it pulls small flat sets and **skips** large sets (>100) and the tree view. When auto-pull is suppressed, `E` pulls everything / `e` pulls the selected repo on demand
- Retry repos with an issue (`r` / `R`) and refetch any repo from scratch (`e` / `E`) — a refetch re-pulls **and** refreshes every cached fact (branch/dirty/stash counts, ahead/behind, worktrees)
- Action hints dim when they'd be a no-op
- Worktree discovery (`.worktrees/*/.git`)
- Sort the list (`s` leader, or click a column header) by name / branch / status / ahead-behind / dirty / last-commit / worktrees / branches / stashes / pulled / changed — re-pick or re-click flips `▲`/`▼` (persisted; the list is always sorted, Name asc by default). The sort menu lists only the columns currently visible
- Filter repos by name (`/`) or by status (`f` leader: updated / up-to-date / skipped / failed / issues)
- **Repo groups** (`z`): named list sections from `~/.config/polygit/groups.json` — membership by `*`-pattern, static list, shell command, or a fetched JSON document; sort/filter apply within each group, big groups collapse (`Enter`/`Space`/click on the header), dynamic memberships are cached and refreshed with `Z`
- Clickable 2-row column header with the active sort indicator; an always-on dirty marker (`•`) with the count (`•N`) when the dirty column is toggled. Count columns render a **dim zero** (not a blank), and a column every repo lacks (no worktrees/stashes, ≤1 branch) auto-hides — its `t`-menu chip goes dim and inert
- Lazygit-style panes: rounded borders, a bright border on the focused pane (`Tab` / `1` / `2`), and a draggable divider with a grip
- Open [lazygit](https://github.com/jesseduffield/lazygit) on the selected repo with `l`
- Diff modal with a clickable file list over the selected file's diff (stash, uncommitted, vs base branch, or **a branch's changes vs its base**); `Tab` switches focus between the file list and diff, with a footer that adapts to the focused pane; **status-filter chips** (`f` / click) with count badges when a change set has >10 files across ≥2 statuses; `Shift`/`Alt`+`PgUp`/`PgDn` page the file list; "no changes" shows a toast instead of an empty modal
- Draggable scrollbars everywhere (preview, diff panels, help, repo page), highlighted while dragged
- Tabbed, **context-aware** help modal (`?`): **Hotkeys** (for the current view) · **CLI & Flags** · **Legend** (every glyph, both icon sets) · **About**, switched with `Tab`/click (last tab remembered)
- Settings modal (`,`), three sections: **General** (panel padding · grouping · tree view), **Theming** (Unicode ⇄ emoji icons · **theme** auto/dark/light · independent **background** normal/soft/**terminal** · **contrast** normal/soft), and **Sync** (auto-pull on launch · auto-pull limit `50/100/250/∞` · auto-pull in tree view) — all persisted; rows and radio chips are mouse-clickable. The `auto` theme **re-detects** dark/light at runtime, so an OS light↔dark switch re-themes live (no restart)
- Web-like mouse support everywhere: full status-bar hints + active `⟪sort⟫`/`[filter]` tags clickable, every modal gets an `[x]` and closes on outside click, repo page has an `[esc back]` button
- **New-build reload notice**: detects a newer binary installed over the running one and offers a one-click `[reload]` (exec-restart in the same terminal)
- Non-TUI fallback (same output as bash reference) when not on a TTY or with `--no-tui`
- Exit codes: 0 (all ok), 1 (any failed), 2 (user quit mid-run), 130 (Ctrl-C)

## Installing

polygit is a single binary for **Linux and macOS** (on Windows, use WSL).

```bash
# Install script — grabs the prebuilt binary for your platform
curl -fsSL https://steven-pribilinskiy.github.io/polygit/install.sh | bash

# Or with cargo (no clone needed)
cargo install --git https://github.com/steven-pribilinskiy/polygit

# Or from source (also installs the bash/Go/Bun siblings)
make install            # release build → ~/bin/polygit
```

See the [installation docs](https://steven-pribilinskiy.github.io/polygit/start/installation/) for details.

## Running

```bash
# TUI mode (auto-detected when stderr is a TTY)
polygit [DIR]

# Recursive by default — scan a whole tree of projects
polygit ~/projects

# Plain streaming output (matches bash reference for a flat dir; lists nested repos too)
polygit --no-tui [DIR]

# Custom concurrency
polygit -j 8 [DIR]
PULL_JOBS=8 polygit [DIR]

# Cap scan depth (1 = immediate subdirs only — the legacy single-level scan)
polygit --depth 3 [DIR]
polygit --no-recursive [DIR]

# Custom timeout per pull (default: 30s)
polygit --timeout 60 [DIR]

# Skip worktree discovery
polygit --no-worktrees [DIR]
```

## Sibling implementations

`polygit` forwards to the other builds when the first argument is `go`, `bun`, or `cli`; all remaining arguments are passed through verbatim:

```bash
polygit go  [args]   # Go / bubbletea build (polygit-tui-go)
polygit bun [args]   # Bun / ink build, JIT (polygit-tui-bun-jit)
polygit cli [args]   # bash streaming version (polygit-repos)
```

A directory literally named `go`/`bun`/`cli` is still reachable as `polygit ./go`.

The backends live in `polygit-siblings/` next to the `polygit` binary (e.g. `~/bin/polygit-siblings/`), deliberately **off `$PATH`** so they aren't top-level commands — they're reachable only through `polygit go|bun|cli`. The dispatcher resolves them relative to its own location and falls back to `$PATH` if that directory is absent.

The `cli` backend (`polygit-repos`, the original parallel-pull bash script that `src/plain.rs` was ported from) is tracked in this repo under [`polygit-siblings/`](polygit-siblings/) and deployed by `make install`. The `go` and `bun` backends are built from their own source trees.

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `↓` | Next repo |
| `k` / `↑` | Previous repo |
| `g` | Jump to top |
| `G` | Jump to bottom (Result item) |
| `Space` | Toggle the Result summary in the preview without moving selection (any navigation clears it); on a folder/group header: collapse/expand |
| `v` `g` | Toggle the **grouped list view** (groups from `~/.config/polygit/groups.json`; persisted) |
| `v` `t` | Toggle the **directory-tree view** (folders from recursive discovery; persisted) |
| `z` `…` | **Fold leader** (vim-style): `za` toggle · `zo`/`zc` open/close · `zO` expand subtree · `zM` collapse all · `zR` expand all (on the selected folder/group) |
| `-` / `+` `=` / `*` | Collapse all / expand all / expand the selected subtree |
| `Z` | Refresh dynamic group memberships (`command`/`url` sources) now, bypassing the cache TTL |
| `←` / `→` | Tree-style fold nav: `←` collapses the selected header or jumps to its enclosing folder/group; `→` expands a collapsed header |
| `[` / `]` | Narrow / widen the left pane — clickable in the status bar (or drag the divider — its grip fills solid and brightens while dragging) |
| `Tab` | Toggle focus: list `[1]` ↔ preview `[2]` (the active pane gets a bright rounded border) |
| `1` / `2` | Focus the list / preview pane directly |
| `PgUp` / `PgDn` | Scroll preview (when focused) |
| `End` | Resume auto-scroll in preview |
| `Enter` / double-click | Open the dedicated repo page for the selected repo (on the repo list); on a folder/collapsible group header: collapse/expand |
| `r` | Retry selected repo if it has an issue (failed or skipped) |
| `R` | Retry all repos with an issue (failed or skipped) |
| `e` | Refetch selected repo (re-pull regardless of status, unless it's in progress) |
| `E` | Refetch all repos that aren't currently in progress |
| `i` | Toggle the info panel — an additive block above the log/diff (status, branch, pulled delta, ahead/behind, remote, last commit, worktrees, changes, path) |
| `d` | Toggle the per-repo diff view (working-tree changes, or the last pull's diff) |
| `t` | Column-toggle leader: press `t` then `u`/`a`/`d`/`l`/`w`/`b`/`s`/`p`/`c` to show/hide a column (mode stays active until `Esc`) |
| `s` | Sort leader: press `s` then `n`/`c`/`s`/`a`/`d`/`l`/`w`/`b`/`k` to sort by name / branch / status / ahead-behind / dirty / last-commit / worktrees / branches / stashes — re-pick flips `▲`/`▼` (or click a column header); the list is always sorted (Name asc by default) |
| `f` | Status-filter leader: press `f` then `a`/`u`/`c`/`s`/`f`/`i` to filter the list by all / updated / up-to-date / skipped / failed / issues (applies on top of `/`) |
| `o` | Open the selected repo's remote in the browser |
| `y` | Copy the selected repo's **absolute path** to the clipboard (every copy confirms with a toast previewing the copied text) |
| `Y` | Copy the selected repo's **remote (origin) URL** to the clipboard |
| `c` | Start claude code in the selected repo (suspends the TUI, returns on exit) |
| `l` | Open **lazygit** in the selected repo (suspends the TUI, returns on exit) |
| `x` | Clear **this repo's log buffer** (empties the streamed pull output) |
| `D` | Open the [documentation website](https://steven-pribilinskiy.github.io/polygit/) in the browser |
| `,` | Open the settings modal (panel padding, grouping, tree view, icon style, theme, background, contrast) |
| `?` | Open the help modal (docs/GitHub/notes links, all keys, flags & env) |
| `/` | Filter repos by name |
| `Esc` | Clear filter (or quit when no filter) |
| `q` | Quit |
| `Ctrl-C` | Quit (exit 130) |

**Retry vs refetch:** retry only re-runs repos that need it (failed/skipped); refetch re-runs any repo even if it was already up to date. In the status bar, `r`/`R` dim when no repo has an issue, and `e`/`E` dim when there's nothing eligible (the selected repo is still in progress).

The repo list, the log/diff preview, the help modal, and the repo page all show a scrollbar when their content overflows. **Clickable commands:** the action hints in the status bar (and the `t` column menu) are mouse-clickable — clicking one runs the same command as the key.

### Repo page (`Enter` / double-click)

Opens a full-screen page for the selected repo that runs `git fetch` and lists every local branch (with HEAD marker, fresh ahead/behind vs upstream, upstream name, last-commit date, subject), every worktree (branch + path), and every stash. A header row labels the branch columns, which are toggled with the page-local `t` menu (then `b`/`y`/`a`/`m`/`d`/`c`/`u`/`f`/`g`/`s` for ahead-behind / dirty / added / modified / deleted / total / upstream / base / age / subject — clickable chips, persisted). The **added/modified/deleted** counts are each branch's changes vs the merge-base with its **base branch** — the auto-detected fork parent (the branch it most directly diverged from, weighing both local heads and remote-tracking branches, so a branch cut from a non-`main` integration branch resolves correctly), or a per-branch **override** you set — loaded in the background (cells show `…` until ready); a column every branch leaves empty auto-hides and its chip goes dim. The **base** column shows that resolved base per branch (blue when auto-detected, magenta with a trailing `*` when overridden). Count cells show a dim zero rather than a blank. The bottom **info panel** (`i`, persisted) details the selected row: branch, upstream, base branch + merge-base sha, ahead/behind, change stats, and the tip commit (sha · author · date · subject). Sections are prefixed with type icons; worktrees/stashes sections only appear when non-empty. The selection starts on the current (HEAD) branch. Navigate rows with `j`/`k`/`g`/`G`/`Home`/`End` (or the wheel / click); `Enter` (or double-click) opens the diff modal on a stash, a dirty row, or **a branch (its changes vs the base branch)**; `Shift+Enter` checks out the selected branch (clean, non-current); `p`/`P` fast-forward; `d` performs the row-appropriate action (delete branch / drop stash / remove worktree / discard) — the footer hint is dynamic; `b` (or clicking the **base** cell) opens the **base-branch picker** to override which branch this branch's stats diff against — pick *auto-detect* to clear the override; the choice is persisted per repo+branch; `c` starts claude code; `l` opens lazygit; `o` opens the branch on the remote (e.g. GitHub) in the browser; `y` opens a copy menu (absolute path / branch name / both); `?` shows the page's hotkeys; `Esc`/`q` returns. An action result (e.g. "Dropped stash@{0}") shows in a banner at the bottom.

`Enter` or a double-click opens a 90%-of-screen **diff modal**, two bordered sub-panels: a scrollable **file-list panel** (top, ≤40% height) over the **selected file's diff** (bottom). The footer adapts to the focused pane. `Tab` switches focus between the panels (the focused one gets a bright border); `j`/`k`/`g`/`G` then drive that panel. Pick a file with `↑↓`/`j`/`k` or by clicking it; its diff loads beneath. `PgUp`/`PgDn` page the diff; `Shift`/`Alt`+`PgUp`/`PgDn` page the file list; `Shift`/`Alt`+wheel scrolls the file list. When a change set has more than 10 files across at least two statuses, a **status-filter chip row** appears (`[ all N ] [ M … ] [ A … ] …` with count badges) — click a chip or press `f` to cycle, and the list groups by status. The diff-panel title shows the full path, left-truncating only when it doesn't fit. For a dirty row, `t` toggles the file set between *uncommitted* (vs HEAD) and *vs base branch*; a stash lists its files; a clean branch shows its changes vs the base branch. `d` discards/removes/drops (with confirm); `Esc` closes. When there's nothing to show, a "no changes" toast appears instead of an empty modal.

### Columns (`t` leader)

The list always shows the status glyph + name + branch + a dirty marker (an amber `•` for any repo with uncommitted changes — amber, not red, since it's a "modified" state, not an error). Press `t` then a column key to toggle extra columns: `u` status text (a short label per state — `queued`/`running`/`up-to-date`/`updated`/`no upstream`/`dirty`/`throttled` — with the specific failure kind on failed repos: `not found`, `auth`, `diverged`, `not a repo`, `timeout`, `network`, `lock`, and `ref gone` for a deleted upstream ref), `a` ahead/behind, `d` adds the dirty **count** (`•N`) to the always-on marker, `l` last-commit age, `w` worktree count (`⑃N`, cyan), `b` feature-branch count (`⑂N`, green — local branches excluding `main`/`dev`), `s` stash count (`≡N`), `p` commits the last pull landed (`⇣N`, green), `c` files the last pull changed (`±N`, cyan). Count columns render a **dim zero** rather than a blank, so the column shape stays recognizable. A column every repo leaves empty (no worktrees, no stashes, or ≤1 branch everywhere) auto-hides once its data has loaded, and its `t`-menu chip goes dim and inert; the **pulled**/**changed** columns auto-hide once the run finishes having pulled nothing. The git-derived columns fetch per-repo details in the background the first time one is enabled (cells show `…` until ready); `w` is free from worktree discovery, and pulled/changed come straight from each pull. Enabled columns persist across runs.

### Info panel (`i`)

`i` toggles an info block above the right pane's content (the pull log or the diff) for the selected repo: status (with how long the pull took), branch, the **Pulled** delta when the repo updated this run (`oldsha → newsha` on one line, then `N commits · M files (+ins −del)`, then best-effort `N new tags · N new branches`), ahead/behind, remote, last commit (hash · subject · author · relative date), worktrees, uncommitted/stash counts, and the local path. The block is additive — the log/diff stays beneath it — and tracks the selection as you move. The extra git facts are fetched lazily for the selected repo only, and re-fetched after a pull so the panel reflects the new HEAD.

The panel is interactive (it's a web app in a terminal):

- **Bold field labels**; rows that would carry nothing are hidden — no `↑0 ↓0`, no all-zero Changes line, no empty Worktrees.
- **Clickable links** (when the remote is a browsable https host): the **branch** opens `…/tree/<branch>`, the **commit hash** opens `…/commit/<sha>`, and **Remote** opens the repo — all in your browser.
- **Truncated values expand on click.** The path is truncated from the *left* (keeping the filename tail); a long commit subject from the right. Click the underlined value to expand it — the full text wraps starting at the value column, never under the label. Click again to collapse.
- **Copy buttons**: a `⧉` next to **Path** copies the absolute path; a `⧉` on the log pane's top border copies the whole pull log.

`c` starts claude code (`cc`, i.e. `claude --dangerously-skip-permissions`, in the repo dir; override with `PULL_CLAUDE_CMD`).

### Settings modal (`,`)

`,` opens a small settings modal (from the list or the repo page), organized into **General** (panel padding, grouping, tree view) and **Theming** (icons, theme, background, contrast) sections. Move between rows with `j`/`k` (or `↑`/`↓`), toggle/cycle the selected setting with `Space`/`Enter`, and close with `Esc`/`q`/`,`/`[x]`/a click outside. With the mouse, click a row label to select it or a radio chip to set that value directly. All settings persist across runs (in `~/.config/polygit/state.json`):

- **Panel padding** — adds a 1-cell inner padding inside every bordered panel and modal.
- **Icons** — switches the status / column / marker glyphs **everywhere** (list, columns, repo page, Result/Errors pages, log markers) between the default Unicode set (`◌ ✓ ◇ ✗ ⑂ ≡ •`) and an emoji set (`✅ ✨ 🚫 ❌ 🌿 📦 📝`). Columns stay aligned in either mode — only single-codepoint, reliably-2-cell emoji are used (no variation-selector glyphs), and the tight ahead/behind column keeps compact `↑↓` arrows.
- **Theme** — `dark` / `light` paint a full explicit palette (background, text, and every accent color) so the app looks identical regardless of the terminal's own color scheme. `auto` detects whether the terminal background is dark or light at startup and applies the matching palette — via an OSC 11 query of the terminal, falling back to `COLORFGBG`, the Windows light/dark setting under WSL (covers terminals that follow the system theme but don't answer OSC 11, e.g. Tabby), and the macOS appearance setting.
- **Background** and **Contrast** — two independent `normal` / `soft` axes. **Background** softens the surface tones (background, selection, shadow); **Contrast** narrows the text/background distance and desaturates the accent + semantic colors. They compose, so you can soften the surface while keeping vivid text, or vice versa. (Pre-split state files, which had only `contrast`, load with both axes set from the old value.)
- **Grouping** — render the list as named group sections (same as `v g`). Shows a hint when no `groups.json` exists.
- **Tree view** — render the repos as a collapsible directory tree (same as `v t`). Inert when every repo is at the scan root (a flat directory).

### Repo groups (`v g`)

`v g` renders the list as named **group sections** defined in `~/.config/polygit/groups.json` (hand-edited, optional — never written by the app). When groups are configured, a clickable `vg groups` hint appears in the status bar (its label brightens while the grouped view is active). Each group header shows per-status counts and the member total; repos inside a group keep the global sort and filters; repos matching no group land in a dim `ungrouped` section at the bottom. Groups with more members than `collapse_threshold` (default: 5) get a collapsible header — selectable, with `▾`/`▸`, toggled by `Enter`/`Space`/click; smaller groups get static headers navigation skips. The grouping toggle and collapsed groups persist across runs.

Each group has a `name` and exactly one membership source:

```json
{
  "collapse_threshold": 5,
  "cache_ttl_minutes": 1440,
  "groups": [
    { "name": "frontend", "pattern": "mfe-*" },
    { "name": "tooling", "repos": ["polygit", "dotfiles"] },
    { "name": "team", "command": "curl -fsSL https://example.com/repos.txt" },
    { "name": "platform", "url": "https://example.com/remote-entries.json",
      "extract": { "pointer": "/entries", "kind": "keys" } }
  ]
}
```

`pattern` is a case-insensitive `*`-wildcard on repo names — **or, when it contains a `/`, on the repo's path relative to the scan root** (e.g. `work/*`); `repos` is a static list; `command` runs a shell command whose stdout lines are repo names; `url` fetches a JSON document and extracts names per `extract` (a JSON pointer + `keys`/`values`). Dynamic (`command`/`url`) sources resolve in the background — never blocking startup — and are cached in `~/.config/polygit/groups-cache.json` for `cache_ttl_minutes` (default: daily). `Z` forces a refresh; a failed resolve keeps the cached membership and marks the header with `⚠`. Selecting a group header shows a group summary (source, counts, cache age, errors) in the preview pane. Full reference: [Repo groups guide](https://steven-pribilinskiy.github.io/polygit/guides/groups/).

### Directory tree (`v t`)

Recursive discovery is the default: `polygit` crawls the target directory in parallel for git repos (pruning hidden dirs, `node_modules`/`vendor`/`target`/`dist`/… and `*.worktrees`, and never descending into a found repo), streaming each repo in and starting its pull as soon as it's found. `--depth N` caps the descent (`--depth 1` / `--no-recursive` is the legacy single-level scan). In flat and grouped views, each repo shows its path relative to the scan root (e.g. `personal/polygit`).

`v t` renders that result as a **collapsible directory tree**: folders become headers (`▾`/`▸`) with their subtree's status rollup and repo count, repos nest beneath by basename. Tree and grouping are **two independent toggles**, so four views are reachable:

- **flat** — every repo in one list (default)
- **grouped** (`v g`) — repos in `groups.json` sections, regardless of folder
- **tree** (`v t`) — the folder hierarchy
- **tree + groups** (both on) — groups subdivide the repos *inside each folder*; a group collapses independently per folder

Fold the tree with the mouse (click a folder header), `←`/`→` (collapse/expand or jump to the parent), `Enter`/`Space` (toggle the selected header), the direct keys `-` (collapse all) / `+` (expand all) / `*` (expand the selected subtree), or the vim-style `z` chord (`za`/`zo`/`zc`/`zO`/`zM`/`zR`). The tree toggle and collapsed-folder set persist across runs. Full reference: [Tree view guide](https://steven-pribilinskiy.github.io/polygit/guides/tree-view/).

### Sorting (`s` leader / column headers)

The list is always sorted — **Name ascending** is the default. Press `s` then a column key (`n` name, `c` branch, `s` status, `a` ahead/behind, `d` dirty, `l` last-commit, `w` worktrees, `b` branches, `k` stashes, `p` pulled commits, `g` changed files) — or click a column header (including the **branch** header). The sort menu only lists columns currently visible on screen. Re-picking the same column (or re-clicking the header) flips the direction; the header shows `▲`/`▼` on the active column and the footer shows a clickable `⟪column ▲⟫` tag. The order persists across runs.

### Help modal (`?`)

`?` opens an in-app reference with four tabs — **Hotkeys** (contextual, with short sections laid out side by side), **CLI & Flags**, **Legend** (every glyph in both icon sets with its meaning), and **About** — switched with `Tab` (the last tab is remembered across opens). It links to this repo on GitHub and the design notes on `notes.lvh.me`, lists the `go`/`bun`/`cli` subcommands, every flag and environment variable, the hotkeys grouped by purpose, and exit codes. The links are clickable (open in your browser via `$BROWSER`/`wslview`/`xdg-open`). Scroll with `j`/`k`, `g`/`G`, `PgUp`/`PgDn`, or the wheel; close with `?`/`Esc`/`q`/`[esc]`/a click outside.

### Mouse

Click a repo row to select it, scroll the wheel over the left pane to move the
selection or over the right pane to scroll the preview, click or drag the preview
scrollbar to jump/scroll, and drag the divider between the panes to resize. While
the TUI is running it captures the mouse, so native terminal text-selection is
suspended until you quit (same tradeoff as lazygit/htop).

Everything actionable is clickable like a web page:

- **Status-bar hints** — the whole hint ("s sort", "f by-status", "/ filter", …), not just the key. The active tags sit next to their hints and are clickable too: `⟪name ▲⟫` flips the sort direction, `[needle]` clears the name filter, `{failed}` resets the status filter. In "[ ] resize", `[` and `]` nudge the split directly. The right side shows the version, a clickable `built … ago` tag (opens the Build info modal), and clickable `, settings · ? help · q quit`.
- **Modals** (settings, copy menu, confirm, diff, help) — every modal has an `[x]` close button on its top border and closes/cancels when you click anywhere outside it. Clicks inside a modal never fall through to the view behind.
- **Settings** — click a row label to select it, or click a radio chip (`● dark`, `○ off`, …) to set that exact value.
- **Confirm dialogs** — `[y/enter] yes` and `[n] no` are clickable.
- **Copy menu** — click an option to copy it immediately.
- **Repo page** — a clickable `[esc back]` button on the top border returns to the list.

### New-build reload

While running, polygit watches its own binary on disk. When a newer build is installed (e.g. `make install`'s atomic rename), a persistent notice appears in the top-right (inset with the panel-padding setting, with a glint sweeping its border): `↺ new build installed · [reload] [x]`. It rides on top of every screen — the repo list, the full-screen repo page, and any open modal — so it's never hidden. `[reload]` restores the terminal and `exec`s the new binary with the same arguments — the fresh process re-scans and re-pulls (instant when everything is already up to date). `[x]` dismisses the notice; it re-arms if the binary changes again.

Clicking the **`built … ago`** tag in the status bar opens a **Build info** modal: the running version, the watched executable path, when it was built, how the new-build watch works, and whether a newer build is currently waiting. A `[restart]` button (or `r`) exec-restarts into the latest build; any other key or click closes it.

## Testing

```bash
make test
```

## Benchmark

```bash
make bench
```

## Architecture

- `src/main.rs` — CLI entry point, sibling dispatch, TUI setup, event loop
- `src/app.rs` — Application state types (`AppState`, `RepoState`, `LogBuffer`) + retry/refetch eligibility helpers
- `src/git.rs` — Git operations + the recursive repo walker (`spawn_repo_walker`, `should_descend`) and pull-output classification (`classify_pull_output`, incl. throttle detection) + unit tests
- `src/worker.rs` — Async pull workers + streaming discovery (`run_discovery`) and the throttle governor (`run_governor`), bounded by the shared `ThrottleControl` semaphore
- `src/groups.rs` — Repo-grouping config, membership resolution, and cache
- `src/theme.rs` — Color palettes + terminal-background detection
- `src/render.rs` — Ratatui rendering (list pane, preview pane, status bar, ANSI color support)
- `src/plain.rs` — Non-TUI streaming output (byte-compatible with bash reference)
