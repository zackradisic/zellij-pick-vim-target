use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use zellij_tile::prelude::*;

/// A floating fuzzy-picker for file paths in the scrollback of the pane you
/// launched it from. Pick one and it lands on your clipboard as a Vim target:
///   src/util/lib.ts:450:12  ->  +call cursor(450,12) src/util/lib.ts
///   /etc/hosts:21           ->  +21 /etc/hosts
/// Replaces the tmux `prefix+f` (find) / `Y` (copy-as-vim-target) combo.

/// One pickable target: the `path[:line[:col]]` token plus the scrollback line
/// it came from, so the list can show the surrounding context (e.g. the code on
/// that line) — useful when many matches share the same file.
#[derive(Default, Clone, Debug)]
struct Target {
    token: String,
    context: String,
}

#[derive(Default)]
struct State {
    permissions_granted: bool,
    /// Our own pane id, so we can tell when the user (re)focuses us.
    plugin_id: u32,
    /// Whether our pane was focused as of the last PaneUpdate.
    was_focused: bool,
    /// Set when we refocus ourselves after opening a file, so the resulting
    /// focus-regain isn't mistaken for the user reopening the picker.
    suppress_reopen: bool,
    /// Latest pane manifest, refreshed on every PaneUpdate. Used to find the
    /// terminal pane the user launched us from.
    manifest: Option<PaneManifest>,
    /// The non-plugin pane focused in the active tab the last time we saw one.
    /// While our floating pane is focused, the user's *tiled* pane is still
    /// focused in the tiled layer, so this keeps tracking it; for a floating
    /// source pane it retains the value from just before we opened.
    last_focused: Option<PaneId>,
    /// Set when we (re)become visible and need to re-read scrollback. Cleared
    /// once a build succeeds, so navigation/filtering never triggers a re-read.
    pending_build: bool,
    /// The pane we actually read scrollback from, captured at build time. Opens
    /// resolve relative paths against this, so repeated opens stay anchored to
    /// the original pane even after focus drifts to a freshly opened editor.
    built_source: Option<PaneId>,
    /// Extracted targets, most-recent (bottom of scrollback) first, deduped.
    targets: Vec<Target>,
    query: String,
    selected: usize,
    scroll_offset: usize,
    /// Transient message shown in the footer (e.g. errors).
    status: Option<String>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        self.plugin_id = get_plugin_ids().plugin_id;
        // Zellij doesn't emit Visible(true) on first load (we're born visible),
        // so arm the first build here; PaneUpdate + perms will carry it out.
        self.pending_build = true;
        request_permission(&[
            PermissionType::ReadApplicationState,  // PaneUpdate / focused pane / cwd
            PermissionType::ReadPaneContents,      // get_pane_scrollback
            PermissionType::WriteToClipboard,      // copy_to_clipboard
            PermissionType::OpenFiles,             // open_file
            PermissionType::ChangeApplicationState, // stack_panes
        ]);
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::PaneUpdate,
            EventType::Visible,
            EventType::Key,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permissions_granted = true;
                self.build_targets()
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.permissions_granted = false;
                true
            }
            Event::PaneUpdate(manifest) => {
                self.manifest = Some(manifest);
                self.track_focus();
                // Detect our own floating pane (re)gaining focus — i.e. the user
                // just opened us via LaunchOrFocus. This is the reliable reopen
                // trigger since Visible(true) isn't dependable.
                let now_focused = self.plugin_focused();
                if now_focused && !self.was_focused {
                    if self.suppress_reopen {
                        // We refocused ourselves after opening a file; don't
                        // treat it as a fresh open (which would re-read the
                        // now-focused editor pane and wipe the filter).
                        self.suppress_reopen = false;
                    } else {
                        self.pending_build = true;
                        self.query.clear();
                        self.selected = 0;
                        self.scroll_offset = 0;
                        self.status = None;
                    }
                }
                self.was_focused = now_focused;
                self.build_targets();
                true
            }
            Event::Visible(true) => {
                // The user just (re)opened us: re-read the scrollback fresh and
                // reset the picker state. (Not reliably emitted on first load,
                // hence the PaneUpdate focus-transition trigger above.)
                self.pending_build = true;
                self.query.clear();
                self.selected = 0;
                self.scroll_offset = 0;
                self.status = None;
                self.build_targets();
                true
            }
            Event::Visible(false) => false,
            Event::Key(key) => self.handle_key(key),
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if !self.permissions_granted {
            print_text_with_coordinates(
                Text::new("Waiting for permissions..."),
                0,
                0,
                Some(cols),
                None,
            );
            return;
        }

        // Title ribbon.
        print_ribbon_with_coordinates(
            Text::new(" pick vim-target ").selected(),
            0,
            0,
            Some(cols),
            Some(1),
        );

        // Search line with a block cursor.
        let search = format!("> {}\u{2588}", self.query);
        print_text_with_coordinates(
            Text::new(&search).color_range(2, 0..1),
            0,
            1,
            Some(cols),
            Some(1),
        );

        // Footer: either the Vim target for the current selection, or hints.
        let footer_y = rows.saturating_sub(1);
        let matches = self.filtered();
        let footer = if let Some(msg) = &self.status {
            Text::new(msg).color_range(1, ..)
        } else if let Some(&ti) = matches.get(self.selected) {
            let target = vim_target(&self.targets[ti].token);
            Text::new(&format!("⏎ open   ^y copy: {}", target))
                .color_range(3, 0..1)
                .color_range(3, 9..11)
        } else {
            keyhints(&[
                ("↑↓/^jk", "navigate"),
                ("Enter", "open"),
                ("^y", "copy"),
                ("Esc", "close"),
            ])
        };
        print_text_with_coordinates(footer, 0, footer_y, Some(cols), Some(1));

        // Body.
        let body_top = 2usize;
        let body_height = rows.saturating_sub(3); // ribbon + search + footer
        if body_height == 0 {
            return;
        }

        if self.targets.is_empty() {
            print_text_with_coordinates(
                Text::new("  No file paths found in scrollback").color_range(1, ..),
                0,
                body_top,
                Some(cols),
                Some(1),
            );
            return;
        }
        if matches.is_empty() {
            print_text_with_coordinates(
                Text::new("  No matches").color_range(1, ..),
                0,
                body_top,
                Some(cols),
                Some(1),
            );
            return;
        }

        // Keep the selection within the visible window.
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + body_height {
            self.scroll_offset = self.selected + 1 - body_height;
        }

        for (row, &ti) in matches
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(body_height)
        {
            let is_selected = row == self.selected;
            let t = &self.targets[ti];
            let prefix = if is_selected { "▸ " } else { "  " };
            // Show the whole scrollback line for context; highlight the token
            // (path in one color, :line[:col] in another) within it.
            let text = format!("{}{}", prefix, t.context);
            let pfx = prefix.chars().count();
            let mut item = Text::new(&text);
            if let Some(b) = t.context.find(&t.token) {
                let start = pfx + t.context[..b].chars().count();
                let boundary = start + line_col_start(&t.token);
                let end = start + t.token.chars().count();
                item = item.color_range(0, start..boundary);
                if boundary < end {
                    item = item.color_range(2, boundary..end);
                }
            }
            if is_selected {
                item = item.selected();
            }
            print_text_with_coordinates(item, 0, body_top + (row - self.scroll_offset), Some(cols), Some(1));
        }
    }
}

impl State {
    /// Whether our own plugin pane is currently focused.
    fn plugin_focused(&self) -> bool {
        self.manifest
            .as_ref()
            .map(|m| {
                m.panes
                    .values()
                    .flatten()
                    .any(|p| p.is_plugin && p.id == self.plugin_id && p.is_focused)
            })
            .unwrap_or(false)
    }

    /// Record the focused non-plugin pane in the active tab, if any.
    fn track_focus(&mut self) {
        if let Some(pane) = self.focused_source_pane() {
            self.last_focused = Some(pane);
        }
    }

    /// The terminal pane focused in the active tab right now. Prefers a tiled
    /// pane (the common case when we float over the user's work), then any
    /// focused non-plugin pane.
    fn focused_source_pane(&self) -> Option<PaneId> {
        let (tab, _) = get_focused_pane_info().ok()?;
        let panes = self.manifest.as_ref()?.panes.get(&tab)?;
        panes
            .iter()
            .find(|p| p.is_focused && !p.is_plugin && !p.is_floating)
            .or_else(|| panes.iter().find(|p| p.is_focused && !p.is_plugin))
            .map(|p| PaneId::Terminal(p.id))
    }

    /// Resolve the pane to read scrollback from: the live focused source pane,
    /// falling back to the last one we tracked (covers a floating source pane,
    /// which loses its tiled-layer focus once we open).
    fn source_pane(&self) -> Option<PaneId> {
        self.focused_source_pane().or(self.last_focused)
    }

    /// Read the source pane's full scrollback and extract targets. No-op unless
    /// we're permitted and a build is pending. Returns whether anything changed.
    fn build_targets(&mut self) -> bool {
        if !self.permissions_granted || !self.pending_build {
            return false;
        }
        let Some(pane) = self.source_pane() else {
            return false;
        };
        match get_pane_scrollback(pane, true) {
            Ok(contents) => {
                self.targets = extract_targets(&contents);
                self.selected = 0;
                self.scroll_offset = 0;
                self.pending_build = false;
                self.built_source = Some(pane);
                self.status = None;
                true
            }
            Err(e) => {
                self.status = Some(format!("Couldn't read scrollback: {e}"));
                true
            }
        }
    }

    /// Indices into `self.targets` matching the current query (subsequence,
    /// case-insensitive over the token and its context line), in display order.
    fn filtered(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let hay = format!("{} {}", t.token, t.context).to_lowercase();
                subsequence(&hay, &q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        let match_count = self.filtered().len();

        if key.is_key_without_modifier(BareKey::Enter) {
            // Open but keep the picker up, so you can open several in a row.
            self.open_selected();
            return true;
        }

        if key.is_key_with_ctrl_modifier(BareKey::Char('y')) {
            self.copy_selected();
            close_self();
            return false;
        }

        if key.is_key_with_ctrl_modifier(BareKey::Char('c')) {
            close_self();
            return false;
        }

        if key.is_key_without_modifier(BareKey::Esc) {
            if self.query.is_empty() {
                close_self();
                return false;
            }
            self.query.clear();
            self.selected = 0;
            return true;
        }

        if key.is_key_without_modifier(BareKey::Up)
            || key.is_key_with_ctrl_modifier(BareKey::Char('p'))
            || key.is_key_with_ctrl_modifier(BareKey::Char('k'))
        {
            if self.selected > 0 {
                self.selected -= 1;
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Down)
            || key.is_key_with_ctrl_modifier(BareKey::Char('n'))
            || key.is_key_with_ctrl_modifier(BareKey::Char('j'))
        {
            if match_count > 0 && self.selected + 1 < match_count {
                self.selected += 1;
            }
            return true;
        }

        if key.is_key_without_modifier(BareKey::Backspace) {
            self.query.pop();
            self.selected = 0;
            return true;
        }

        // Printable characters (optionally Shift) edit the filter.
        if key.has_no_modifiers() || key.has_modifiers(&[KeyModifier::Shift]) {
            if let BareKey::Char(c) = key.bare_key {
                self.query.push(c);
                self.selected = 0;
                return true;
            }
        }

        false
    }

    fn copy_selected(&mut self) {
        let matches = self.filtered();
        if let Some(&ti) = matches.get(self.selected) {
            copy_to_clipboard(vim_target(&self.targets[ti].token));
        }
    }

    /// Open the selected target in the editor at its line, in a pane stacked
    /// with the source pane (so it flips in place rather than shrinking the
    /// layout). Relative paths resolve against the source pane's cwd.
    /// Note: only the line is honored — Zellij's open_file has no column.
    fn open_selected(&mut self) {
        let matches = self.filtered();
        let Some(&ti) = matches.get(self.selected) else {
            return;
        };
        let (base, line, _col) = parse_target(&self.targets[ti].token);
        let source = self.built_source.or_else(|| self.source_pane());
        let file = FileToOpen {
            path: PathBuf::from(base),
            line_number: line.map(|l| l as usize),
            cwd: source.and_then(|p| get_pane_cwd(p).ok()),
        };
        if let Some(new_pane) = open_file(file, BTreeMap::new()) {
            if let Some(src) = source {
                stack_panes(vec![src, new_pane]);
            }
        }
        // Opening focuses the new tiled editor pane, which hides our floating
        // pane. Refocus ourselves so the picker stays up for the next open.
        self.suppress_reopen = true;
        focus_plugin_pane(self.plugin_id, true, false);
    }
}

/// Pull every path-like token out of the scrollback, most-recent first, deduped.
/// Each target keeps the scrollback line it came from, for display context.
fn extract_targets(contents: &PaneContents) -> Vec<Target> {
    let lines = contents
        .lines_above_viewport
        .iter()
        .chain(contents.viewport.iter())
        .chain(contents.lines_below_viewport.iter());

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    // Reverse so the bottom of the scrollback (most recent output) comes first.
    let collected: Vec<&String> = lines.collect();
    for line in collected.iter().rev() {
        let context = line.trim();
        for raw in line.split_whitespace() {
            let tok = trim_junk(raw);
            if let Some(t) = as_target(tok) {
                if seen.insert(t.to_string()) {
                    out.push(Target {
                        token: t.to_string(),
                        context: context.to_string(),
                    });
                }
            }
        }
    }
    out
}

/// Strip surrounding quotes/brackets/punctuation, mirroring the original
/// copy-vim-target script's lead/trail trim sets.
fn trim_junk(s: &str) -> &str {
    const LEAD: &[char] = &[' ', '\t', '"', '\'', '`', '(', '[', '{', '<'];
    const TRAIL: &[char] = &[
        ' ', '\t', '"', '\'', '`', ')', ']', '}', '>', '.', ',', ';', ':',
    ];
    s.trim_start_matches(|c| LEAD.contains(&c))
        .trim_end_matches(|c| TRAIL.contains(&c))
}

/// A token qualifies as a target if it contains a `/` (a path, line optional)
/// or it's a dotted filename WITH an explicit `:line` (so prose like "see the
/// readme." isn't matched). URLs are excluded.
fn as_target(tok: &str) -> Option<&str> {
    if tok.is_empty() || tok.contains("://") {
        return None;
    }
    let (base, line, _col) = parse_target(tok);
    let has_line = line.is_some();
    let qualifies = (base.contains('/') || (has_line && base.contains('.')))
        && base.chars().any(|c| c.is_alphanumeric());
    qualifies.then_some(tok)
}

/// Peel up to two trailing `:<digits>` segments → (base, line?, col?).
fn parse_target(tok: &str) -> (&str, Option<u32>, Option<u32>) {
    let mut base = tok;
    let mut nums: Vec<u32> = Vec::new();
    for _ in 0..2 {
        if let Some(idx) = base.rfind(':') {
            let num = &base[idx + 1..];
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = num.parse() {
                    nums.push(n);
                    base = &base[..idx];
                    continue;
                }
            }
        }
        break;
    }
    nums.reverse(); // collected col-first; want [line, col]
    (base, nums.first().copied(), nums.get(1).copied())
}

/// Byte offset within `tok` where the trailing `:line[:col]` span begins, or
/// the token length if there is none. Used only for coloring.
fn line_col_start(tok: &str) -> usize {
    let (base, line, _) = parse_target(tok);
    if line.is_some() {
        base.len()
    } else {
        tok.len()
    }
}

/// Transform a "path[:line[:col]]" token into a Vim target string. Mirrors
/// tmux-copy-vim-target.sh exactly.
fn vim_target(tok: &str) -> String {
    let (base, line, col) = parse_target(tok);
    match (line, col) {
        (Some(l), Some(c)) => format!("+call cursor({l},{c}) {base}"),
        (Some(l), None) => format!("+{l} {base}"),
        _ => tok.to_string(),
    }
}

/// Case-insensitive subsequence test (fzf-style). Caller lowercases both.
fn subsequence(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars().peekable();
    for h in haystack.chars() {
        if let Some(&n) = chars.peek() {
            if h == n {
                chars.next();
            }
        } else {
            return true;
        }
    }
    chars.peek().is_none()
}

/// Build a Text with key names highlighted (color 3) and labels plain.
fn keyhints(pairs: &[(&str, &str)]) -> Text {
    let mut s = String::new();
    let mut ranges = Vec::new();
    let mut char_pos = 0usize;
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str("  ");
            char_pos += 2;
        }
        let start = char_pos;
        s.push_str(key);
        char_pos += key.chars().count();
        ranges.push((start, char_pos));
        s.push(' ');
        char_pos += 1;
        s.push_str(label);
        char_pos += label.chars().count();
    }
    let mut text = Text::new(&s);
    for (start, end) in ranges {
        text = text.color_range(3, start..end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_match_the_tmux_script() {
        assert_eq!(
            vim_target("src/util/lib.ts:450:12"),
            "+call cursor(450,12) src/util/lib.ts"
        );
        assert_eq!(vim_target("/Users/z/.config/.zshrc:21"), "+21 /Users/z/.config/.zshrc");
        assert_eq!(vim_target("main.go:42"), "+42 main.go");
        // No line number: copied verbatim.
        assert_eq!(vim_target("src/foo/bar.rs"), "src/foo/bar.rs");
    }

    #[test]
    fn parse_peels_line_and_col() {
        assert_eq!(parse_target("a/b.rs:10:5"), ("a/b.rs", Some(10), Some(5)));
        assert_eq!(parse_target("a/b.rs:10"), ("a/b.rs", Some(10), None));
        assert_eq!(parse_target("a/b.rs"), ("a/b.rs", None, None));
        // A trailing non-numeric ":" is not a line number.
        assert_eq!(parse_target("a/b.rs:foo"), ("a/b.rs:foo", None, None));
        // Only the last two numeric segments are line/col.
        assert_eq!(parse_target("a:1:2:3"), ("a:1", Some(2), Some(3)));
    }

    #[test]
    fn qualifies_paths_and_dotted_files_with_lines() {
        // Paths (slash) qualify with or without a line.
        assert!(as_target("src/util/lib.ts:450:12").is_some());
        assert!(as_target("src/foo/bar.rs").is_some());
        assert!(as_target("./scripts/build.rs:10").is_some());
        // A bare dotted filename qualifies only with an explicit line.
        assert!(as_target("main.go:42").is_some());
        assert!(as_target("word.txt").is_none());
        // Prose and URLs are rejected.
        assert!(as_target("hello").is_none());
        assert!(as_target("https://example.com").is_none());
        assert!(as_target("").is_none());
    }

    #[test]
    fn trims_surrounding_punctuation() {
        assert_eq!(trim_junk("(src/foo.rs:3),"), "src/foo.rs:3");
        assert_eq!(trim_junk("\"main.go:42\""), "main.go:42");
        assert_eq!(trim_junk("lib.ts."), "lib.ts");
        // A leading ./ or ../ is preserved (not in the lead set).
        assert_eq!(trim_junk("[./a/b.rs:1]"), "./a/b.rs:1");
    }

    #[test]
    fn extraction_is_recent_first_deduped_and_filters_prose() {
        let contents = PaneContents {
            lines_above_viewport: vec![
                "Error in src/util/lib.ts:450:12 while compiling".to_string(),
                "warning: see /etc/hosts:21 for details".to_string(),
            ],
            viewport: vec![
                "plain prose with a word.txt and no line".to_string(),
                "ran ./scripts/build.rs:10 ok".to_string(),
            ],
            lines_below_viewport: vec![
                "referencing main.go:42 in the handler".to_string(),
                "src/util/lib.ts:450:12 again (dup)".to_string(),
            ],
            selected_text: None,
        };

        let tokens: Vec<String> = extract_targets(&contents)
            .into_iter()
            .map(|t| t.token)
            .collect();
        assert_eq!(
            tokens,
            vec![
                // bottom of scrollback first; the lib.ts dup keeps the recent one
                "src/util/lib.ts:450:12".to_string(),
                "main.go:42".to_string(),
                "./scripts/build.rs:10".to_string(),
                "/etc/hosts:21".to_string(),
            ]
        );
        assert!(!tokens.iter().any(|t| t.contains("word.txt")));
    }

    #[test]
    fn target_keeps_its_scrollback_line_as_context() {
        let contents = PaneContents {
            lines_above_viewport: vec![],
            viewport: vec!["src/main.rs:511:    let _ = delete_dead_session(name);".to_string()],
            lines_below_viewport: vec![],
            selected_text: None,
        };
        let got = extract_targets(&contents);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].token, "src/main.rs:511");
        assert_eq!(
            got[0].context,
            "src/main.rs:511:    let _ = delete_dead_session(name);"
        );
        assert_eq!(vim_target(&got[0].token), "+511 src/main.rs");
    }

    #[test]
    fn fuzzy_subsequence() {
        assert!(subsequence("src/util/lib.ts:450", "slts"));
        assert!(subsequence("src/util/lib.ts:450", "lib450"));
        assert!(!subsequence("src/util/lib.ts", "xyz"));
        assert!(subsequence("anything", ""));
    }

    #[test]
    fn line_col_span_for_coloring() {
        // Start of the ":line[:col]" tail, used to color it separately.
        assert_eq!(line_col_start("a/b.rs:10:5"), "a/b.rs".len());
        assert_eq!(line_col_start("a/b.rs:10"), "a/b.rs".len());
        assert_eq!(line_col_start("a/b.rs"), "a/b.rs".len());
    }
}
