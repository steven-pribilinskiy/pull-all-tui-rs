# pull-all

Interactive multi-repo git pull dashboard. Pulls every git repo in a directory in parallel with live per-repo logs, retry/refetch support, and a two-pane TUI layout. This is the canonical Rust implementation; it also fronts the Go, Bun, and bash alternatives via subcommands.

📖 **Documentation: https://steven-pribilinskiy.github.io/pull-all**

## Features

- Parallel pulls with configurable concurrency (default: nproc)
- Live log streaming per repo in a scrollable preview pane
- Status glyphs: queued / running / up-to-date / updated / skipped / failed
- Automatic one-shot retry of a failed pull before marking it failed
- Dynamic `Errors (N)` page (after `Result`) listing each failed repo with its error output
- Retry repos with an issue (`r` / `R`) and refetch any repo from scratch (`f` / `F`)
- Action hints dim when they'd be a no-op
- Worktree discovery (`.worktrees/*/.git`)
- Filter repos by name with `/`
- Non-TUI fallback (same output as bash reference) when not on a TTY or with `--no-tui`
- Exit codes: 0 (all ok), 1 (any failed), 2 (user quit mid-run), 130 (Ctrl-C)

## Building

```bash
# Requires Rust stable (cargo)
make build              # binary at: bin/pull-all
make install            # also copies to ~/bin/pull-all
```

## Running

```bash
# TUI mode (auto-detected when stderr is a TTY)
pull-all [DIR]

# Plain streaming output (matches bash reference)
pull-all --no-tui [DIR]

# Custom concurrency
pull-all -j 8 [DIR]
PULL_JOBS=8 pull-all [DIR]

# Custom timeout per pull (default: 30s)
pull-all --timeout 60 [DIR]

# Skip worktree discovery
pull-all --no-worktrees [DIR]
```

## Sibling implementations

`pull-all` forwards to the other builds when the first argument is `go`, `bun`, or `cli`; all remaining arguments are passed through verbatim:

```bash
pull-all go  [args]   # Go / bubbletea build (pull-all-tui-go)
pull-all bun [args]   # Bun / ink build, JIT (pull-all-tui-bun-jit)
pull-all cli [args]   # bash streaming version (pull-all-repos)
```

A directory literally named `go`/`bun`/`cli` is still reachable as `pull-all ./go`.

The backends live in `pull-all-siblings/` next to the `pull-all` binary (e.g. `~/bin/pull-all-siblings/`), deliberately **off `$PATH`** so they aren't top-level commands — they're reachable only through `pull-all go|bun|cli`. The dispatcher resolves them relative to its own location and falls back to `$PATH` if that directory is absent.

The `cli` backend (`pull-all-repos`, the original parallel-pull bash script that `src/plain.rs` was ported from) is tracked in this repo under [`pull-all-siblings/`](pull-all-siblings/) and deployed by `make install`. The `go` and `bun` backends are built from their own source trees.

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `↓` | Next repo |
| `k` / `↑` | Previous repo |
| `g` | Jump to top |
| `G` | Jump to bottom (Result item) |
| `Space` | Toggle the Result summary in the preview without moving selection (any navigation clears it) |
| `[` / `]` | Narrow / widen the left pane |
| `Tab` | Toggle focus: list ↔ preview |
| `PgUp` / `PgDn` | Scroll preview (when focused) |
| `End` | Resume auto-scroll in preview |
| `Enter` / double-click | Open the dedicated repo page for the selected repo (on the repo list) |
| `r` | Retry selected repo if it has an issue (failed or skipped) |
| `R` | Retry all repos with an issue (failed or skipped) |
| `f` | Refetch selected repo (re-pull regardless of status, unless it's in progress) |
| `F` | Refetch all repos that aren't currently in progress |
| `i` | Toggle the info panel — an additive block above the log/diff (status, branch, ahead/behind, remote, last commit, worktrees, changes, path) |
| `d` | Toggle the per-repo diff view (working-tree changes, or the last pull's diff) |
| `t` | Column-toggle leader: press `t` then `a`/`d`/`l`/`w`/`b`/`s` to show/hide a column (mode stays active until `Esc`) |
| `o` | Open the selected repo's remote in the browser |
| `y` | Copy the selected repo's **absolute path** to the clipboard |
| `Y` | Copy the selected repo's **remote (origin) URL** to the clipboard |
| `c` | Start claude code in the selected repo (suspends the TUI, returns on exit) |
| `x` | Clear **this repo's log buffer** (empties the streamed pull output) |
| `?` | Open the help modal (GitHub/notes links, all keys, flags & env) |
| `/` | Filter repos by name |
| `Esc` | Clear filter (or quit when no filter) |
| `q` | Quit |
| `Ctrl-C` | Quit (exit 130) |

**Retry vs refetch:** retry only re-runs repos that need it (failed/skipped); refetch re-runs any repo even if it was already up to date. In the status bar, `r`/`R` dim when no repo has an issue, and `f`/`F` dim when there's nothing eligible (the selected repo is still in progress).

The repo list, the log/diff preview, the help modal, and the repo page all show a scrollbar when their content overflows. **Clickable commands:** the action hints in the status bar (and the `t` column menu) are mouse-clickable — clicking one runs the same command as the key.

### Repo page (`Enter` / double-click)

Opens a full-screen page for the selected repo that runs `git fetch` and lists every local branch (with HEAD marker, fresh ahead/behind vs upstream, upstream name, last-commit date, subject), every worktree (branch + path), and every stash (`STASHES` section). A red `●` marks branches/worktrees with uncommitted changes. Navigate rows with `j`/`k`/`g`/`G`/`Home`/`End` (or the wheel / click); `Enter` (or double-click) opens the diff modal on a stash or dirty row; `Shift+Enter` checks out the selected branch (clean, non-current); `p` fast-forwards the selected branch, `P` all fast-forwardable branches; `d` deletes a branch / drops a stash / removes a worktree / discards the current branch's uncommitted changes (with a confirmation dialog whose severity scales with danger); `c` starts claude code in that branch/worktree's path; `o` opens the branch on the remote; `y` copies its path; `Esc`/`q` returns.

`Enter` or a double-click on a **stash** or a **dirty** branch/worktree opens a 90%-of-screen **diff modal**: a stash shows `git stash show -p`; a dirty row shows its uncommitted changes, and `t` toggles between *uncommitted* (vs HEAD) and *vs base branch* (everything changed since forking from `origin/HEAD`). Scroll with `↑↓`/`PgUp`/`PgDn`/`Home`/`End` or the wheel; `Esc` closes.

### Columns (`t` leader)

The list always shows the status glyph + name + branch. Press `t` then a column key to toggle extra columns: `a` ahead/behind, `d` dirty marker (`•N`), `l` last-commit age, `w` worktree count (`⑂N`, cyan), `b` feature-branch count (`⑂N`, green — local branches excluding `main`/`dev`), `s` stash count (`≡N`). The git-derived columns (`a`/`d`/`l`/`b`/`s`) fetch per-repo details in the background the first time one is enabled (cells show `…` until ready); `w` is free from worktree discovery. Enabled columns persist across runs.

### Info panel (`i`)

`i` toggles an info block above the right pane's content (the pull log or the diff) for the selected repo: status + elapsed, branch, ahead/behind vs upstream, remote, last commit (hash · subject · author · relative date), worktrees, uncommitted/stash counts, and the local path. The block is additive — the log/diff stays beneath it — and tracks the selection as you move. The extra git facts are fetched lazily for the selected repo only. `c` starts claude code (`cc`, i.e. `claude --dangerously-skip-permissions`, in the repo dir; override with `PULL_CLAUDE_CMD`).

### Help modal (`?`)

`?` opens an in-app reference: links to this repo on GitHub and the design notes on `notes.lvh.me`, the `go`/`bun`/`cli` subcommands, every flag and environment variable, the hotkeys grouped by purpose, and exit codes. The links are clickable (open in your browser via `$BROWSER`/`wslview`/`xdg-open`). Scroll with `j`/`k`, `g`/`G`, `PgUp`/`PgDn`, or the wheel; close with `?`/`Esc`/`q`.

### Mouse

Click a repo row to select it, scroll the wheel over the left pane to move the
selection or over the right pane to scroll the preview, and drag the divider
between the panes to resize. While the TUI is running it captures the mouse, so
native terminal text-selection is suspended until you quit (same tradeoff as
lazygit/htop).

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
- `src/git.rs` — Git operations (`discover_repos`, `get_branch`, `is_dirty`, `diff_stat`, `classify_pull_output`) + unit tests
- `src/worker.rs` — Async pull workers with semaphore concurrency control
- `src/render.rs` — Ratatui rendering (list pane, preview pane, status bar, ANSI color support)
- `src/plain.rs` — Non-TUI streaming output (byte-compatible with bash reference)
