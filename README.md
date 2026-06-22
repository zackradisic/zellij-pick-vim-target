# zellij-pick-vim-target

A floating Zellij plugin that fuzzy-picks a file path out of the current pane's
scrollback and either **opens it in your editor** at that line, or **copies it
to the clipboard** as a Vim target:

```
src/util/lib.ts:450:12   ->   open at line 450  ·  +call cursor(450,12) src/util/lib.ts
/etc/hosts:21            ->   open at line 21   ·  +21 /etc/hosts
```

This replaces a tmux workflow: `prefix+f` searched the scrollback for a path and
`Y` copied it in Vim form. Zellij has no keyboard text-selection in the
scrollback, so instead the plugin reads the whole scrollback, extracts every
path-like token, and lets you fuzzy-filter them.

## Keys

- **Type** to fuzzy-filter (subsequence match over the path *and* its line of
  context).
- **`↑`/`↓`**, **`Ctrl-j`/`Ctrl-k`**, **`Ctrl-n`/`Ctrl-p`** — move the selection.
- **`Enter`** — open the selected file in your editor at its line, in a pane
  **stacked** with the source pane. The picker stays open so you can open
  several in a row.
- **`Ctrl-y`** — copy the Vim target to the clipboard and close. (Copy keeps the
  column; opening is line-only — see below.)
- **`Esc`** — clear the filter, or close if it's already empty. **`Ctrl-c`** —
  close.

## How it works

- On becoming focused it reads the **full scrollback** of the pane you launched
  it from (`get_pane_scrollback`). Because a floating plugin is focused only in
  the *floating* layer, the user's terminal pane is still focused in the *tiled*
  layer — so the source pane is found in the `PaneManifest` for the active tab,
  with a "last focused non-plugin pane" fallback for a floating source. (Zellij
  doesn't fire `Visible(true)` on first load, so the read is armed in `load()`
  and re-armed when our pane regains focus.)
- It extracts `path[:line[:col]]` tokens: anything containing a `/`, or a dotted
  filename with an explicit `:line` (so prose isn't matched). Each target keeps
  the scrollback line it came from, shown as context in the list. Results are
  most-recent-first and deduped.
- **Open** (`Enter`) uses `open_file` with the line number and the source pane's
  cwd (`get_pane_cwd`), so relative paths from grep/build output resolve
  correctly. The new editor pane is `stack_panes`'d with the source pane, then
  the plugin refocuses itself so the picker stays up. The editor is whatever
  Zellij is configured to use (`scrollback_editor` / `$EDITOR`).
- **Caveat:** `open_file` honors the line but not the column. Use `Ctrl-y` (copy)
  when you need the column — the clipboard form keeps `+call cursor(line,col)`.

## Build

```sh
cargo build --release   # -> target/wasm32-wasip1/release/zellij-pick-vim-target.wasm
```

(`.cargo/config.toml` pins the `wasm32-wasip1` target.)

## Test

The text-matching logic has unit tests in `src/main.rs`. They run on the host,
so override the wasm build target:

```sh
cargo test --target aarch64-apple-darwin
```

## Use

Bind a key to launch it floating (e.g. in `~/.config/zellij/config.kdl`). Load
it straight from the latest GitHub release:

```kdl
bind "Alt y" {
    LaunchOrFocusPlugin "https://github.com/zackradisic/zellij-pick-vim-target/releases/latest/download/zellij-pick-vim-target.wasm" {
        floating true
        move_to_focused_tab true
    }
}
```

Or point it at a local build with `file:/ABSOLUTE/PATH/target/wasm32-wasip1/release/zellij-pick-vim-target.wasm`.

On first launch Zellij prompts for these permissions:

- `ReadApplicationState` — pane manifest, focused pane, and pane cwd.
- `ReadPaneContents` — read the source pane's scrollback.
- `WriteToClipboard` — copy the Vim target (`Ctrl-y`).
- `OpenFiles` — open the selected file in the editor (`Enter`).
- `ChangeApplicationState` — stack the editor pane and refocus the picker.

## Dev

```sh
cargo build --release && zellij --layout plugin.kdl
```
