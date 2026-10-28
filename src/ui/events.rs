//! Key bindings. One table per mode drives both key handling and the help screen,
//! so the two cannot disagree.

use super::state::{Mode, UiState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Quit,
    ToggleHelp,
    OpenColumns,
    Reset,
    ToggleHostnames,
    ToggleScale,
    ToggleVisualization,
    CycleColors,
    ToggleFlows,
    ToggleMpls,
    OpenDetail,
    PreviousHop,
    NextHop,
    SelectPrevious,
    SelectNext,
    PageUp,
    PageDown,
    SelectFirst,
    SelectLast,
    ClearSelection,
    Close,
    CursorUp,
    CursorDown,
    ToggleColumn,
    ShiftEarlier,
    ShiftLater,
}

pub struct Binding {
    pub keys: &'static [KeyCode],
    pub label: &'static str,
    pub help: &'static str,
    pub action: Action,
}

const fn bind(
    keys: &'static [KeyCode],
    label: &'static str,
    help: &'static str,
    action: Action,
) -> Binding {
    Binding {
        keys,
        label,
        help,
        action,
    }
}

use KeyCode::{Char, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Up};

pub const TABLE_KEYS: &[Binding] = &[
    bind(&[Char('q')], "q", "Quit (also Ctrl-C)", Action::Quit),
    bind(
        &[Char('?')],
        "?",
        "Show or hide this help",
        Action::ToggleHelp,
    ),
    bind(
        &[Char('o')],
        "o",
        "Choose and order columns",
        Action::OpenColumns,
    ),
    bind(&[Char('r')], "r", "Reset statistics", Action::Reset),
    bind(
        &[Char('h')],
        "h",
        "Host names or IP addresses",
        Action::ToggleHostnames,
    ),
    bind(
        &[Char('s')],
        "s",
        "Linear or logarithmic RTT scale",
        Action::ToggleScale,
    ),
    bind(
        &[Char('v')],
        "v",
        "Sparkline or heatmap",
        Action::ToggleVisualization,
    ),
    bind(&[Char('c')], "c", "Cycle colour modes", Action::CycleColors),
    bind(
        &[Char('f')],
        "f",
        "Hop table or flow matrix",
        Action::ToggleFlows,
    ),
    bind(
        &[Char('e')],
        "e",
        "Show or hide MPLS labels",
        Action::ToggleMpls,
    ),
    bind(
        &[Up, Char('k')],
        "↑ k",
        "Select previous hop",
        Action::SelectPrevious,
    ),
    bind(
        &[Down, Char('j')],
        "↓ j",
        "Select next hop",
        Action::SelectNext,
    ),
    bind(&[PageUp], "PgUp", "Scroll up a page", Action::PageUp),
    bind(&[PageDown], "PgDn", "Scroll down a page", Action::PageDown),
    bind(&[Home], "Home", "First hop", Action::SelectFirst),
    bind(&[End], "End", "Last hop", Action::SelectLast),
    bind(
        &[Enter],
        "Enter",
        "Details of the selected hop",
        Action::OpenDetail,
    ),
    bind(&[Esc], "Esc", "Clear the selection", Action::ClearSelection),
];

pub const COLUMN_KEYS: &[Binding] = &[
    bind(
        &[Up, Char('k')],
        "↑ k",
        "Move the cursor up",
        Action::CursorUp,
    ),
    bind(
        &[Down, Char('j')],
        "↓ j",
        "Move the cursor down",
        Action::CursorDown,
    ),
    bind(
        &[Char(' '), Enter],
        "Space",
        "Show or hide the column",
        Action::ToggleColumn,
    ),
    bind(
        &[Left, Char('K')],
        "← Shift+↑",
        "Move the column earlier",
        Action::ShiftEarlier,
    ),
    bind(
        &[Right, Char('J')],
        "→ Shift+↓",
        "Move the column later",
        Action::ShiftLater,
    ),
    bind(&[Esc, Char('o'), Char('q')], "Esc", "Close", Action::Close),
];

pub const DETAIL_KEYS: &[Binding] = &[
    bind(&[Up, Char('k')], "↑ k", "Previous hop", Action::PreviousHop),
    bind(&[Down, Char('j')], "↓ j", "Next hop", Action::NextHop),
    bind(&[Esc, Enter, Char('q')], "Esc", "Close", Action::Close),
];

const HELP_KEYS: &[Binding] = &[bind(
    &[Esc, Char('?'), Char('q')],
    "Esc",
    "Close",
    Action::Close,
)];

fn bindings(mode: Mode) -> &'static [Binding] {
    match mode {
        Mode::Table => TABLE_KEYS,
        Mode::Help => HELP_KEYS,
        Mode::Columns => COLUMN_KEYS,
        Mode::Detail => DETAIL_KEYS,
    }
}

/// What `key` does in `mode`, if anything. Ctrl-C is handled before this.
pub fn action_for(mode: Mode, key: KeyEvent) -> Option<Action> {
    if mode == Mode::Columns && key.modifiers.contains(KeyModifiers::SHIFT) {
        match key.code {
            Up => return Some(Action::ShiftEarlier),
            Down => return Some(Action::ShiftLater),
            _ => {}
        }
    }
    bindings(mode)
        .iter()
        .find(|b| b.keys.contains(&key.code))
        .map(|b| b.action)
}

/// What the event loop must do after an action that reaches beyond the UI state.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    Reset,
}

/// Rows the page keys move by; ratatui clamps the selection to the table.
const PAGE: u16 = 10;

pub fn apply(action: Action, ui: &mut UiState) -> Effect {
    match action {
        Action::Quit => return Effect::Quit,
        Action::Reset => return Effect::Reset,
        Action::ToggleHelp => ui.mode = Mode::Help,
        Action::OpenColumns => ui.open_chooser(),
        Action::Close => ui.mode = Mode::Table,
        Action::ToggleHostnames => ui.show_hostnames = !ui.show_hostnames,
        Action::ToggleScale => ui.toggle_scale(),
        Action::ToggleVisualization => ui.toggle_visualization(),
        Action::CycleColors => ui.cycle_colors(),
        Action::ToggleFlows => ui.toggle_view(),
        Action::ToggleMpls => ui.show_mpls = !ui.show_mpls,
        Action::OpenDetail => ui.open_detail(),
        Action::PreviousHop => ui.step_detail(-1),
        Action::NextHop => ui.step_detail(1),
        Action::SelectPrevious => ui.table.select_previous(),
        Action::SelectNext => ui.table.select_next(),
        Action::PageUp => ui.table.scroll_up_by(PAGE),
        Action::PageDown => ui.table.scroll_down_by(PAGE),
        Action::SelectFirst => ui.table.select_first(),
        Action::SelectLast => ui.table.select_last(),
        Action::ClearSelection => ui.table.select(None),
        Action::CursorUp => ui.chooser.move_cursor(-1),
        Action::CursorDown => ui.chooser.move_cursor(1),
        Action::ToggleColumn => {
            ui.chooser.toggle();
            ui.apply_chooser();
        }
        Action::ShiftEarlier => {
            ui.chooser.shift(-1);
            ui.apply_chooser();
        }
        Action::ShiftLater => {
            ui.chooser.shift(1);
            ui.apply_chooser();
        }
    }
    Effect::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::Parser;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn no_key_is_bound_twice_in_a_mode() {
        for mode in [Mode::Table, Mode::Help, Mode::Columns, Mode::Detail] {
            let keys: Vec<_> = bindings(mode).iter().flat_map(|b| b.keys).collect();
            for (i, k) in keys.iter().enumerate() {
                assert!(!keys[i + 1..].contains(k), "{mode:?}: {k:?} bound twice");
            }
        }
    }

    #[test]
    fn esc_closes_overlays_instead_of_quitting() {
        let mut ui = UiState::new(&Args::parse_from(["mtr-ng", "example.com"]));
        ui.row_hops = vec![1];
        for (open, mode) in [
            (Action::ToggleHelp, Mode::Help),
            (Action::OpenColumns, Mode::Columns),
            (Action::OpenDetail, Mode::Detail),
        ] {
            apply(open, &mut ui);
            assert_eq!(ui.mode, mode);
            let action = action_for(ui.mode, key(Esc)).unwrap();
            assert_eq!(apply(action, &mut ui), Effect::None);
            assert_eq!(ui.mode, Mode::Table);
        }
        assert_eq!(action_for(Mode::Table, key(Char('q'))), Some(Action::Quit));
        assert_eq!(action_for(Mode::Help, key(Char('q'))), Some(Action::Close));
    }

    #[test]
    fn shift_arrows_reorder_columns() {
        let shift_up = KeyEvent::new(Up, KeyModifiers::SHIFT);
        assert_eq!(
            action_for(Mode::Columns, shift_up),
            Some(Action::ShiftEarlier)
        );
        assert_eq!(action_for(Mode::Columns, key(Up)), Some(Action::CursorUp));
        assert_eq!(
            action_for(Mode::Table, shift_up),
            Some(Action::SelectPrevious)
        );
    }
}
