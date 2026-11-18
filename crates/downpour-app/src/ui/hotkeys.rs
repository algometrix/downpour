//! Keyboard shortcuts of the main window.
//!
//! One table serves both the key handling and the labels ("Remove (Del)"),
//! so a button can never advertise a key that does something else. Keys
//! work only while no dialog is open and no text field has focus, so typing
//! in the search box never triggers them.
//!
//! Main-window keys are taken out of the raw input before egui sees it:
//! egui itself moves widget focus with the arrows (scrolling to whatever it
//! focuses) and lets Space press a focused button.

use eframe::egui::{self, Key, Modifiers};

use super::selection::Pick;

/// Whether a shortcut is a bare key or needs Ctrl (⌘ on macOS) or Shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chord {
    Plain,
    Command,
    /// Shift alone; in the list it extends the selection, as in Explorer.
    Shift,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    key: Key,
    chord: Chord,
    /// How the key is written on the label.
    name: &'static str,
}

impl Hotkey {
    const fn plain(key: Key, name: &'static str) -> Self {
        Self {
            key,
            chord: Chord::Plain,
            name,
        }
    }

    const fn command(key: Key, name: &'static str) -> Self {
        Self {
            key,
            chord: Chord::Command,
            name,
        }
    }

    const fn shift(key: Key, name: &'static str) -> Self {
        Self {
            key,
            chord: Chord::Shift,
            name,
        }
    }

    /// The keys as written on screen, e.g. "N" or "Ctrl+F".
    pub fn keys(self) -> String {
        match self.chord {
            Chord::Plain => self.name.to_owned(),
            Chord::Command if cfg!(target_os = "macos") => format!("⌘{}", self.name),
            Chord::Command => format!("Ctrl+{}", self.name),
            Chord::Shift => format!("Shift+{}", self.name),
        }
    }

    /// `text` with its shortcut in brackets: "Add URL (N)".
    pub fn label(self, text: &str) -> String {
        format!("{text} ({})", self.keys())
    }

    fn matches(self, key: Key, modifiers: Modifiers) -> bool {
        let chord_held = match self.chord {
            Chord::Plain => modifiers.is_none(),
            Chord::Command => modifiers.command_only(),
            Chord::Shift => {
                modifiers.shift && !modifiers.ctrl && !modifiers.alt && !modifiers.command
            }
        };
        self.key == key && chord_held
    }

    /// Takes the key press out of this frame's input, if it happened.
    pub(super) fn pressed(self, ctx: &egui::Context) -> bool {
        let modifiers = match self.chord {
            Chord::Plain => Modifiers::NONE,
            Chord::Command => Modifiers::COMMAND,
            Chord::Shift => Modifiers::SHIFT,
        };
        ctx.input_mut(|input| input.consume_key(modifiers, self.key))
    }
}

pub const ADD_URL: Hotkey = Hotkey::plain(Key::N, "N");
pub const ADD_LINKS: Hotkey = Hotkey::plain(Key::L, "L");
pub const PAUSE_ALL: Hotkey = Hotkey::plain(Key::P, "P");
pub const RESUME_ALL: Hotkey = Hotkey::plain(Key::R, "R");
pub const SEARCH: Hotkey = Hotkey::command(Key::F, "F");
pub const SWITCH_THEME: Hotkey = Hotkey::plain(Key::T, "T");
/// Starts or stops the queue being shown.
pub const START_STOP_QUEUE: Hotkey = Hotkey::plain(Key::S, "S");
pub const CLEAR_COMPLETED: Hotkey = Hotkey::plain(Key::X, "X");

// For the selected download.
/// Pause, resume, or retry, whichever the download offers.
pub const PRIMARY: Hotkey = Hotkey::plain(Key::Space, "Space");
pub const OPEN_FILE: Hotkey = Hotkey::plain(Key::Enter, "Enter");
pub const SHOW_IN_FOLDER: Hotkey = Hotkey::plain(Key::O, "O");
pub const COPY_ADDRESS: Hotkey = Hotkey::plain(Key::C, "C");
pub const REMOVE: Hotkey = Hotkey::plain(Key::Delete, "Del");
/// Renames a download's file: now if finished, otherwise when it finishes,
/// as F2 does in Explorer.
pub const RENAME: Hotkey = Hotkey::plain(Key::F2, "F2");
pub const NEXT: Hotkey = Hotkey::plain(Key::ArrowDown, "↓");
pub const PREVIOUS: Hotkey = Hotkey::plain(Key::ArrowUp, "↑");
pub const DESELECT: Hotkey = Hotkey::plain(Key::Escape, "Esc");
pub const SELECT_ALL: Hotkey = Hotkey::command(Key::A, "A");
/// Shift with an arrow, Home, or End picks the run from the last click to
/// the row the key moves to.
pub const EXTEND_NEXT: Hotkey = Hotkey::shift(Key::ArrowDown, "↓");
pub const EXTEND_PREVIOUS: Hotkey = Hotkey::shift(Key::ArrowUp, "↑");
pub const FIRST: Hotkey = Hotkey::plain(Key::Home, "Home");
pub const LAST: Hotkey = Hotkey::plain(Key::End, "End");
pub const EXTEND_FIRST: Hotkey = Hotkey::shift(Key::Home, "Home");
pub const EXTEND_LAST: Hotkey = Hotkey::shift(Key::End, "End");
/// Move the selected download one place earlier or later in its queue.
pub const MOVE_UP: Hotkey = Hotkey::command(Key::ArrowUp, "↑");
pub const MOVE_DOWN: Hotkey = Hotkey::command(Key::ArrowDown, "↓");

/// Cancel buttons: Esc already closes every dialog.
pub const CANCEL: Hotkey = Hotkey::plain(Key::Escape, "Esc");

// In the download dialogs (file info and link checker).
pub const DIALOG_START: Hotkey = Hotkey::plain(Key::S, "S");
pub const DIALOG_LATER: Hotkey = Hotkey::plain(Key::L, "L");
pub const DIALOG_CANCEL: Hotkey = Hotkey::plain(Key::C, "C");

/// Confirms removing a download. Not `REMOVE`'s Del: holding that key to open
/// the dialog would repeat and delete straight away.
pub const CONFIRM_REMOVE: Hotkey = Hotkey::plain(Key::Enter, "Enter");

/// A shortcut the main window acts on this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    AddUrl,
    AddLinks,
    PauseAll,
    ResumeAll,
    Search,
    SwitchTheme,
    StartStopQueue,
    ClearCompleted,
    Primary,
    OpenFile,
    ShowInFolder,
    CopyAddress,
    Remove,
    Rename,
    Deselect,
    SelectAll,
    /// Go to a row: alone, or with the run from the anchor (Shift).
    Go(Row, Pick),
    MoveUp,
    MoveDown,
}

/// The row a key goes to in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Next,
    Previous,
    First,
    Last,
}

const BINDINGS: [(Hotkey, Command); 26] = [
    (ADD_URL, Command::AddUrl),
    (ADD_LINKS, Command::AddLinks),
    (PAUSE_ALL, Command::PauseAll),
    (RESUME_ALL, Command::ResumeAll),
    (SEARCH, Command::Search),
    (SWITCH_THEME, Command::SwitchTheme),
    (START_STOP_QUEUE, Command::StartStopQueue),
    (CLEAR_COMPLETED, Command::ClearCompleted),
    (PRIMARY, Command::Primary),
    (OPEN_FILE, Command::OpenFile),
    (SHOW_IN_FOLDER, Command::ShowInFolder),
    (COPY_ADDRESS, Command::CopyAddress),
    (REMOVE, Command::Remove),
    (RENAME, Command::Rename),
    (NEXT, Command::Go(Row::Next, Pick::Only)),
    (PREVIOUS, Command::Go(Row::Previous, Pick::Only)),
    (DESELECT, Command::Deselect),
    (SELECT_ALL, Command::SelectAll),
    (EXTEND_NEXT, Command::Go(Row::Next, Pick::Range)),
    (EXTEND_PREVIOUS, Command::Go(Row::Previous, Pick::Range)),
    (FIRST, Command::Go(Row::First, Pick::Only)),
    (LAST, Command::Go(Row::Last, Pick::Only)),
    (EXTEND_FIRST, Command::Go(Row::First, Pick::Range)),
    (EXTEND_LAST, Command::Go(Row::Last, Pick::Range)),
    (MOVE_UP, Command::MoveUp),
    (MOVE_DOWN, Command::MoveDown),
];

/// Whether the main window may act on its shortcuts now: not while the
/// keyboard belongs to a text field, a dialog, or an open menu.
pub fn main_window_has_keyboard(ctx: &egui::Context, dialog_open: bool) -> bool {
    !dialog_open && !ctx.wants_keyboard_input() && !ctx.memory(|memory| memory.any_popup_open())
}

/// Whether text in a label is selected, as by dragging across an address
/// in the details panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelText {
    Selected,
    NotSelected,
}

/// What the copy event (Ctrl+C, Ctrl+Insert) does: selected label text owns
/// it, as everywhere else; otherwise it copies the picked addresses.
fn copy_command(label_text: LabelText) -> Option<Command> {
    match label_text {
        LabelText::Selected => None,
        LabelText::NotSelected => Some(Command::CopyAddress),
    }
}

/// Removes this frame's shortcut presses from `raw` and returns them.
pub fn take_pressed(ctx: &egui::Context, raw: &mut egui::RawInput) -> Vec<Command> {
    let label_text = if egui::text_selection::LabelSelectionState::load(ctx).has_selection() {
        LabelText::Selected
    } else {
        LabelText::NotSelected
    };
    let mut commands = Vec::new();
    raw.events.retain(|event| {
        // The window turns Ctrl+C into a copy event instead of a key press,
        // so the copy shortcut is that event.
        if matches!(event, egui::Event::Copy) {
            let Some(command) = copy_command(label_text) else {
                return true;
            };
            commands.push(command);
            return false;
        }
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return true;
        };
        match command_for(*key, *modifiers) {
            Some(command) => {
                commands.push(command);
                false
            }
            None => true,
        }
    });
    commands
}

/// The command a key press with these modifiers is bound to.
fn command_for(key: Key, modifiers: Modifiers) -> Option<Command> {
    BINDINGS
        .iter()
        .find(|(hotkey, _)| hotkey.matches(key, modifiers))
        .map(|(_, command)| *command)
}

const FOCUS_SEARCH: &str = "downpour-focus-search";

/// Asks the search field to take focus on its next frame.
pub fn request_search_focus(ctx: &egui::Context) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(FOCUS_SEARCH), true));
}

/// Whether the search field should take focus now; asking clears it.
pub fn take_search_focus_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(egui::Id::new(FOCUS_SEARCH)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_put_the_keys_in_brackets() {
        assert_eq!(REMOVE.label("Remove"), "Remove (Del)");
        assert_eq!(ADD_URL.label("Add URL"), "Add URL (N)");
        let search = SEARCH.keys();
        assert!(search == "Ctrl+F" || search == "⌘F");
    }

    #[test]
    fn shift_and_ctrl_pick_their_own_commands() {
        let shift = Modifiers::SHIFT;
        let go = |row, pick| Some(Command::Go(row, pick));
        assert_eq!(
            command_for(Key::ArrowDown, Modifiers::NONE),
            go(Row::Next, Pick::Only)
        );
        assert_eq!(
            command_for(Key::ArrowDown, shift),
            go(Row::Next, Pick::Range)
        );
        assert_eq!(
            command_for(Key::ArrowDown, Modifiers::COMMAND),
            Some(Command::MoveDown)
        );
        assert_eq!(command_for(Key::End, shift), go(Row::Last, Pick::Range));
        assert_eq!(
            command_for(Key::A, Modifiers::COMMAND),
            Some(Command::SelectAll)
        );
        // Ctrl+Shift is no binding of its own.
        assert_eq!(
            command_for(Key::ArrowDown, Modifiers::COMMAND | shift),
            None
        );
        assert_eq!(EXTEND_LAST.keys(), "Shift+End");
    }

    #[test]
    fn the_copy_event_copies_the_address() {
        let mut raw = egui::RawInput {
            events: vec![egui::Event::Copy],
            ..Default::default()
        };
        let ctx = egui::Context::default();
        assert_eq!(take_pressed(&ctx, &mut raw), [Command::CopyAddress]);
        assert!(raw.events.is_empty());
    }

    #[test]
    fn selected_label_text_keeps_the_copy_event() {
        assert_eq!(copy_command(LabelText::Selected), None);
        assert_eq!(
            copy_command(LabelText::NotSelected),
            Some(Command::CopyAddress)
        );
    }

    #[test]
    fn each_main_window_key_does_one_thing() {
        for (index, (hotkey, _)) in BINDINGS.iter().enumerate() {
            let clashes = BINDINGS[index + 1..]
                .iter()
                .any(|(other, _)| other.key == hotkey.key && other.chord == hotkey.chord);
            assert!(!clashes, "{} is bound twice", hotkey.keys());
        }
    }
}
