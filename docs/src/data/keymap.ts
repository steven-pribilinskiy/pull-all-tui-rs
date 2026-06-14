// The keybinding data lives in keymap.json — the single source shared by the docs (this file +
// the in-page explorer + the keyboard viewer) AND the Rust TUI (which `include_str!`s the same
// file for its in-terminal keyboard viewer). Edit keymap.json; keep it in sync with src/main.rs.
import keymapData from './keymap.json';

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

export const keymap: KeymapSection[] = keymapData as KeymapSection[];
