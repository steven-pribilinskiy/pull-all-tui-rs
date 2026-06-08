// Single source of truth for the keymap, mirroring src/main.rs + src/render.rs (v0.17.0).
// Keep this in sync with the event loop when bindings change.

export type Binding = {
  /** Keys that trigger the action, rendered as <kbd>. Alternatives in one entry. */
  keys: string[];
  action: string;
  /** Optional clarifying note shown in a dimmer column. */
  note?: string;
  /** Extra search terms (synonyms) that don't appear verbatim in the action text. */
  keywords?: string[];
};

export type KeymapSection = {
  id: string;
  title: string;
  blurb: string;
  bindings: Binding[];
};

export const keymap: KeymapSection[] = [
  {
    id: 'list',
    title: 'Repo list',
    blurb: 'The main two-pane dashboard: repos on the left, live log/diff on the right.',
    bindings: [
      { keys: ['j', '↓'], action: 'Next repo' },
      { keys: ['k', '↑'], action: 'Previous repo' },
      { keys: ['g'], action: 'Jump to top' },
      { keys: ['G'], action: 'Jump to bottom', note: 'the Result summary row' },
      { keys: ['Space'], action: 'Toggle the Result/Errors summary in the preview', note: 'any navigation clears it' },
      { keys: ['Tab'], action: 'Toggle focus: list [1] ↔ preview [2]', note: 'the active pane gets a bright rounded border' },
      { keys: ['1', '2'], action: 'Focus the list / preview pane directly', keywords: ['panel', 'pane', 'focus'] },
      { keys: ['PgUp', 'PgDn'], action: 'Scroll the preview', note: 'when focused' },
      { keys: ['End'], action: 'Resume auto-scroll in the preview' },
      { keys: ['[', ']'], action: 'Narrow / widen the left pane' },
      { keys: ['Enter', 'double-click'], action: 'Open the dedicated repo page' },
      { keys: ['r'], action: 'Retry selected repo if it failed or was skipped' },
      { keys: ['R'], action: 'Retry all repos with an issue' },
      { keys: ['e'], action: 'Refetch selected repo', note: 're-pull regardless of status' },
      { keys: ['E'], action: 'Refetch all repos not in progress' },
      { keys: ['i'], action: 'Toggle the info panel above the log/diff', note: 'additive block; tracks the selection' },
      { keys: ['d'], action: 'Toggle the per-repo diff view' },
      { keys: ['t'], action: 'Column-toggle leader', note: 'then a/d/l/w/b/s; stays active until Esc' },
      { keys: ['s'], action: 'Sort leader', note: 'then n/s/a/d/l/w/b/k/o → name/status/ahead-behind/dirty/last/worktrees/branches/stashes/none; re-pick flips ▲▼ (or click a header)', keywords: ['sort', 'order', 'ascending', 'descending'] },
      { keys: ['f'], action: 'Status-filter leader', note: 'then a/u/c/s/f/i → all/updated/up-to-date/skipped/failed/issues', keywords: ['filter', 'status'] },
      { keys: ['o'], action: "Open the selected repo's remote in the browser" },
      { keys: ['y'], action: "Copy the selected repo's absolute path" },
      { keys: ['Y'], action: "Copy the selected repo's remote (origin) URL" },
      { keys: ['c'], action: 'Start claude code in the selected repo', note: 'suspends the TUI; PULL_CLAUDE_CMD overrides' },
      { keys: ['l'], action: 'Open lazygit in the selected repo', note: 'suspends the TUI; warns if lazygit is not installed', keywords: ['lazygit', 'git ui'] },
      { keys: ['x'], action: "Clear this repo's log buffer", note: 'empties the streamed pull output' },
      { keys: ['D'], action: 'Open the documentation website in the browser', keywords: ['docs', 'help website'] },
      { keys: [','], action: 'Open the settings modal', note: 'panel padding · icon style (Unicode/emoji)', keywords: ['settings', 'preferences', 'config', 'emoji', 'padding'] },
      { keys: ['?'], action: 'Open the help modal' },
      { keys: ['/'], action: 'Filter repos by name' },
      { keys: ['Esc'], action: 'Clear the filter, or quit when no filter' },
      { keys: ['q'], action: 'Quit' },
      { keys: ['Ctrl', 'C'], action: 'Quit', note: 'exit code 130' },
    ],
  },
  {
    id: 'page',
    title: 'Repo page',
    blurb: 'Full-screen view of one repo: every local branch, worktree, and stash, with fresh ahead/behind.',
    bindings: [
      { keys: ['j', 'k', 'g', 'G', 'Home', 'End'], action: 'Navigate rows' },
      { keys: ['PgUp', 'PgDn'], action: 'Scroll' },
      { keys: ['Enter', 'double-click'], action: 'Open the diff modal', note: 'on a stash or a dirty branch/worktree' },
      { keys: ['Shift', 'Enter'], action: 'Check out the selected branch', note: 'clean, non-current branch', keywords: ['checkout', 'switch'] },
      { keys: ['p'], action: 'Fast-forward the selected branch/worktree', keywords: ['pull', 'ff'] },
      { keys: ['P'], action: 'Fast-forward every fast-forwardable branch', keywords: ['pull all', 'ff'] },
      { keys: ['d'], action: 'Delete branch / drop stash / remove worktree / discard changes', note: 'confirmation dialog, scaled to danger' },
      { keys: ['o'], action: 'Open the selected branch on the remote' },
      { keys: ['y'], action: "Copy the selected row's path" },
      { keys: ['c'], action: "Start claude code in the row's path" },
      { keys: ['l'], action: "Open lazygit in the row's path" },
      { keys: ['Esc', 'q'], action: 'Back to the repo list' },
    ],
  },
  {
    id: 'diff',
    title: 'Diff modal',
    blurb: 'A 90%-of-screen overlay: a scrollable file-list panel over the selected file’s diff.',
    bindings: [
      { keys: ['↑', '↓', 'j', 'k'], action: 'Pick a file in the list', note: 'loads that file’s diff below' },
      { keys: ['click', 'double-click'], action: 'Pick the clicked file' },
      { keys: ['g', 'G'], action: 'Jump to the first / last file' },
      { keys: ['PgUp', 'PgDn', 'Home', 'End'], action: 'Scroll the diff panel' },
      { keys: ['t'], action: 'Toggle uncommitted ⇄ vs base branch', note: 'dirty rows only; re-lists files' },
      { keys: ['d'], action: 'Discard changes (current branch) / remove worktree / drop stash', note: 'with confirmation' },
      { keys: ['Esc', 'q'], action: 'Close the modal' },
    ],
  },
];
