//! Keyboard support: which keys do what, and the small pieces of logic
//! behind them.
//!
//! [`read`] turns this frame's key presses into [`Command`]s, and the app
//! carries them out (`App::run_command`). Keeping the decoding here, away
//! from the UI code, means it can be tested with a bare `egui::Context`. The
//! same table ([`GROUPS`]) feeds the Shortcuts window (F1), so the help can't
//! drift from what the keys actually do.
//!
//! Rules that keep shortcuts out of the way:
//!
//! * While you're **typing** in a text field, only Ctrl shortcuts and F-keys
//!   work, so arrows, Space, Delete, J/K and `/` reach the text field.
//! * After **Tab** gives a button or checkbox keyboard focus, egui uses the
//!   arrow keys and Space to move between and press widgets, so the library
//!   keys pause until Esc (or a click) clears that focus.
//! * While a **confirmation dialog** is open, it alone gets the keys
//!   (Enter confirms, Esc cancels).

use eframe::egui::{self, Key, Modifiers};

/// Something the keyboard asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Open another document in the library (`usize` = key repeats).
    Move(Move, usize),
    /// Move like [`Command::Move`] and tick every document passed over.
    Extend(Move, usize),
    /// Tick or untick the open document.
    TogglePick,
    /// Tick every document shown (or untick them, if all already are).
    PickAll,
    /// Close the topmost window, else untick everything.
    Escape,
    CombineEdition,
    Delete,
    EditTab,
    FormatTab,
    /// Rebuild the preview now (editions: with the articles' latest edits).
    Refresh,
    Print,
    OpenPdf,
    FocusLink,
    FocusSearch,
    Instapaper,
    Settings,
    Help,
}

/// A step through the library list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

/// Documents skipped by Page Up / Page Down.
pub const PAGE: usize = 10;

/// What's going on in the UI that changes which keys are free.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyContext {
    /// A text field has (or just had) keyboard focus.
    pub typing: bool,
    /// Some other widget has keyboard focus (after Tab).
    pub widget_focused: bool,
    /// A confirmation dialog is open; it handles its own keys.
    pub dialog_open: bool,
    /// A window (settings, Instapaper, shortcuts) is open over the library.
    pub window_open: bool,
}

/// Turn this frame's key presses into commands, consuming the keys used.
pub fn read(ctx: &egui::Context, cx: KeyContext) -> Vec<Command> {
    let mut out = Vec::new();
    if cx.dialog_open {
        return out;
    }
    ctx.input_mut(|i| {
        // Ctrl shortcuts and F-keys work everywhere, even while typing.
        // Shifted variants first: egui's matching ignores an extra Shift.
        const CTRL: [(Key, Command); 10] = [
            (Key::P, Command::Print),
            (Key::E, Command::CombineEdition),
            (Key::L, Command::FocusLink),
            (Key::F, Command::FocusSearch),
            (Key::O, Command::OpenPdf),
            (Key::R, Command::Refresh),
            (Key::I, Command::Instapaper),
            (Key::Comma, Command::Settings),
            (Key::Num1, Command::EditTab),
            (Key::Num2, Command::FormatTab),
        ];
        for (key, command) in CTRL {
            if i.consume_key(Modifiers::COMMAND, key) {
                out.push(command);
            }
        }
        if i.consume_key(Modifiers::NONE, Key::F1) {
            out.push(Command::Help);
        }
        if i.consume_key(Modifiers::NONE, Key::F5) {
            out.push(Command::Refresh);
        }
        if cx.typing {
            return;
        }
        if i.consume_key(Modifiers::NONE, Key::Escape) {
            out.push(Command::Escape);
        }
        if cx.window_open {
            return;
        }
        if i.consume_key(Modifiers::COMMAND, Key::A) {
            out.push(Command::PickAll);
        }
        if i.consume_key(Modifiers::NONE, Key::Questionmark) {
            out.push(Command::Help);
        }
        if cx.widget_focused {
            return; // arrows and Space belong to egui's widget navigation
        }
        // Ctrl/⌘+↑ ↓ jump to the ends, like Home/End (Mac keyboards have no
        // Home/End keys).
        for (key, step) in [(Key::ArrowUp, Move::First), (Key::ArrowDown, Move::Last)] {
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, key) {
                out.push(Command::Extend(step, 1));
            }
            if i.consume_key(Modifiers::COMMAND, key) {
                out.push(Command::Move(step, 1));
            }
        }
        const MOVES: [(Key, Move); 8] = [
            (Key::ArrowUp, Move::Up),
            (Key::K, Move::Up),
            (Key::ArrowDown, Move::Down),
            (Key::J, Move::Down),
            (Key::PageUp, Move::PageUp),
            (Key::PageDown, Move::PageDown),
            (Key::Home, Move::First),
            (Key::End, Move::Last),
        ];
        for (key, step) in MOVES {
            let extended = i.count_and_consume_key(Modifiers::SHIFT, key);
            if extended > 0 {
                out.push(Command::Extend(step, extended));
            }
            let plain = i.count_and_consume_key(Modifiers::NONE, key);
            if plain > 0 {
                out.push(Command::Move(step, plain));
            }
        }
        if i.consume_key(Modifiers::NONE, Key::Space) {
            out.push(Command::TogglePick);
        }
        // A Mac's "delete" key is Backspace; ⌘⌫ deletes, as in the Finder.
        if i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::COMMAND, Key::Backspace) {
            out.push(Command::Delete);
        }
        if i.consume_key(Modifiers::NONE, Key::Slash) {
            out.push(Command::FocusSearch);
        }
    });
    out
}

/// Index of the document `step` lands on (`count` times), starting from
/// `current` in `visible`. With no current document (or one filtered out
/// of view), moving down starts at the top and moving up at the bottom.
pub fn target(visible: &[String], current: Option<&str>, step: Move, count: usize) -> Option<usize> {
    let last = visible.len().checked_sub(1)?;
    let count = count.max(1);
    let at = current.and_then(|c| visible.iter().position(|v| v == c));
    Some(match (step, at) {
        (Move::First, _) => 0,
        (Move::Last, _) => last,
        (Move::Down | Move::PageDown, None) => 0,
        (Move::Up | Move::PageUp, None) => last,
        (Move::Down, Some(p)) => p.saturating_add(count).min(last),
        (Move::Up, Some(p)) => p.saturating_sub(count),
        (Move::PageDown, Some(p)) => p.saturating_add(PAGE * count).min(last),
        (Move::PageUp, Some(p)) => p.saturating_sub(PAGE * count),
    })
}

/// The documents from `from` to `to` (both included, as indexes into
/// `visible`), in the order they're passed over.
pub fn passed_over(visible: &[String], from: usize, to: usize) -> Vec<String> {
    if from <= to {
        visible[from..=to].to_vec()
    } else {
        visible[to..=from].iter().rev().cloned().collect()
    }
}

/// Shortcut names as this platform spells them. The table and hints say
/// "Ctrl"; on a Mac egui's COMMAND modifier is ⌘, so it reads ⌘P, ⌘-click,
/// ⌘⌫ there.
pub fn display(text: &str) -> String {
    if cfg!(target_os = "macos") { for_mac(text) } else { text.to_string() }
}

fn for_mac(text: &str) -> String {
    text.replace("Ctrl+click", "⌘-click").replace("Ctrl+Backspace", "⌘⌫").replace("Ctrl+", "⌘")
}

/// The Shortcuts window, grouped. Keep in step with [`read`].
pub const GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Library",
        &[
            ("↑ ↓   or   K J", "Open the previous / next document"),
            ("Home   End   or   Ctrl+↑ ↓", "Open the first / last document"),
            ("Page Up   Page Down", "Jump 10 documents"),
            ("Shift + any of these", "Move and tick every document passed over"),
            ("Space", "Tick or untick the open document"),
            ("Ctrl+A", "Tick everything shown (again: untick)"),
            ("Ctrl+click   Shift+click", "Tick one / tick a range with the mouse"),
            ("Esc", "Untick everything"),
            ("Ctrl+E", "Combine the ticked articles into an edition"),
            ("Delete   or   Ctrl+Backspace", "Delete the open document (asks first)"),
        ],
    ),
    (
        "Open document",
        &[
            ("Ctrl+1   Ctrl+2", "Edit (or Stories) tab / Format tab"),
            ("Ctrl+R   or   F5", "Update the preview now (editions: from the articles)"),
            ("Ctrl+P", "Print"),
            ("Ctrl+O", "Open the PDF in your viewer"),
        ],
    ),
    (
        "Anywhere",
        &[
            ("Ctrl+L", "Go to the link bar"),
            ("Ctrl+F   or   /", "Search the library"),
            ("Ctrl+V", "Fetch the links on the clipboard (when not typing)"),
            ("Ctrl+I", "Instapaper queue"),
            ("Ctrl+,", "Settings"),
            ("F1   or   ?", "Show this list"),
        ],
    ),
    (
        "Fields, windows and dialogs",
        &[
            ("Tab   Shift+Tab", "Move between buttons and fields (then ↑ ↓ ← → and Space)"),
            ("Enter", "Confirm a dialog; fetch from the link bar"),
            ("Esc", "Leave a field (clears the search), close a window, cancel a dialog"),
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("doc{i}")).collect()
    }

    #[test]
    fn moves_stay_in_bounds() {
        let v = ids(25);
        assert_eq!(target(&v, Some("doc3"), Move::Down, 1), Some(4));
        assert_eq!(target(&v, Some("doc3"), Move::Up, 2), Some(1));
        assert_eq!(target(&v, Some("doc0"), Move::Up, 1), Some(0));
        assert_eq!(target(&v, Some("doc24"), Move::Down, 3), Some(24));
        assert_eq!(target(&v, Some("doc3"), Move::PageDown, 1), Some(13));
        assert_eq!(target(&v, Some("doc20"), Move::PageDown, 1), Some(24));
        assert_eq!(target(&v, Some("doc5"), Move::PageUp, 1), Some(0));
        assert_eq!(target(&v, Some("doc5"), Move::First, 1), Some(0));
        assert_eq!(target(&v, Some("doc5"), Move::Last, 1), Some(24));
    }

    #[test]
    fn moves_without_a_current_document_start_at_an_end() {
        let v = ids(5);
        assert_eq!(target(&v, None, Move::Down, 1), Some(0));
        assert_eq!(target(&v, Some("filtered-out"), Move::Up, 1), Some(4));
        assert_eq!(target(&[], None, Move::Down, 1), None);
    }

    #[test]
    fn ranges_follow_the_direction_of_travel() {
        let v = ids(6);
        assert_eq!(passed_over(&v, 1, 3), ["doc1", "doc2", "doc3"]);
        assert_eq!(passed_over(&v, 3, 1), ["doc3", "doc2", "doc1"]);
        assert_eq!(passed_over(&v, 2, 2), ["doc2"]);
    }

    fn press(keys: &[(Key, Modifiers)], cx: KeyContext) -> Vec<Command> {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        for &(key, modifiers) in keys {
            input.events.push(egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
        }
        let mut out = Vec::new();
        ctx.run_ui(input, |ui| out = read(ui.ctx(), cx)).drop_without_applying_deltas();
        out
    }

    const NONE: Modifiers = Modifiers::NONE;

    #[test]
    fn mac_labels_use_the_command_key() {
        assert_eq!(for_mac("Print (Ctrl+P)"), "Print (⌘P)");
        assert_eq!(for_mac("Ctrl+click   Shift+click"), "⌘-click   Shift+click");
        assert_eq!(for_mac("Delete   or   Ctrl+Backspace"), "Delete   or   ⌘⌫");
        assert_eq!(for_mac("Space"), "Space");
        if !cfg!(target_os = "macos") {
            assert_eq!(display("Ctrl+P"), "Ctrl+P");
        }
    }

    #[test]
    fn plain_keys_drive_the_library() {
        let cx = KeyContext::default();
        assert_eq!(press(&[(Key::ArrowDown, NONE)], cx), [Command::Move(Move::Down, 1)]);
        assert_eq!(press(&[(Key::J, NONE), (Key::J, NONE)], cx), [Command::Move(Move::Down, 2)]);
        assert_eq!(press(&[(Key::ArrowUp, Modifiers::SHIFT)], cx), [Command::Extend(Move::Up, 1)], "Shift extends, not moves");
        assert_eq!(press(&[(Key::End, Modifiers::SHIFT)], cx), [Command::Extend(Move::Last, 1)]);
        assert_eq!(press(&[(Key::Space, NONE)], cx), [Command::TogglePick]);
        assert_eq!(press(&[(Key::A, Modifiers::COMMAND)], cx), [Command::PickAll]);
        assert_eq!(press(&[(Key::Delete, NONE)], cx), [Command::Delete]);
        assert_eq!(press(&[(Key::Escape, NONE)], cx), [Command::Escape]);
        assert_eq!(press(&[(Key::Slash, NONE)], cx), [Command::FocusSearch]);
        assert_eq!(press(&[(Key::Questionmark, Modifiers::SHIFT)], cx), [Command::Help]);
        assert_eq!(press(&[(Key::K, Modifiers::COMMAND)], cx), [], "Ctrl+K isn't K");
        assert_eq!(press(&[(Key::ArrowDown, Modifiers::COMMAND)], cx), [Command::Move(Move::Last, 1)]);
        assert_eq!(press(&[(Key::ArrowUp, Modifiers::COMMAND | Modifiers::SHIFT)], cx), [Command::Extend(Move::First, 1)]);
        assert_eq!(press(&[(Key::Backspace, Modifiers::COMMAND)], cx), [Command::Delete]);
        assert_eq!(press(&[(Key::Backspace, NONE)], cx), [], "plain Backspace does nothing");
    }

    #[test]
    fn ctrl_shortcuts_work_even_while_typing() {
        let typing = KeyContext { typing: true, ..Default::default() };
        assert_eq!(press(&[(Key::P, Modifiers::COMMAND)], typing), [Command::Print]);
        assert_eq!(press(&[(Key::Num2, Modifiers::COMMAND)], typing), [Command::FormatTab]);
        assert_eq!(press(&[(Key::F1, NONE)], typing), [Command::Help]);
        // ...but plain keys and Ctrl+A belong to the text field.
        for key in [Key::ArrowDown, Key::Space, Key::Delete, Key::J, Key::Slash, Key::Escape] {
            assert_eq!(press(&[(key, NONE)], typing), [], "{key:?} while typing");
        }
        assert_eq!(press(&[(Key::A, Modifiers::COMMAND)], typing), []);
    }

    #[test]
    fn focused_widgets_windows_and_dialogs_keep_their_keys() {
        let focused = KeyContext { widget_focused: true, ..Default::default() };
        assert_eq!(press(&[(Key::ArrowDown, NONE), (Key::Space, NONE)], focused), []);
        assert_eq!(press(&[(Key::Escape, NONE)], focused), [Command::Escape]);

        let window = KeyContext { window_open: true, ..Default::default() };
        assert_eq!(press(&[(Key::ArrowDown, NONE)], window), []);
        assert_eq!(press(&[(Key::Escape, NONE)], window), [Command::Escape]);

        let dialog = KeyContext { dialog_open: true, ..Default::default() };
        assert_eq!(press(&[(Key::Escape, NONE), (Key::P, Modifiers::COMMAND)], dialog), []);
    }
}
