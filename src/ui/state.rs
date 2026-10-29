//! What the TUI shows and how: the active mode and view, the columns, the display
//! toggles, and the table's selection and scroll position.

use crate::args::Column;
use crate::ui::visualization::{detect_color_support, ColorSupport, VisualizationMode};
use crate::{Args, SparklineScale};
use ratatui::widgets::TableState;

/// One mode at a time, so keys always mean one thing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Table,
    Help,
    Columns,
    /// Everything known about one hop
    Detail,
    /// Version, licence, what leaves this machine, and whose data is shown
    About,
}

/// What the main area shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    /// The statistics table
    Hops,
    /// Which address each flow met at each hop (`--flows`)
    Flows,
    /// The located hops on a world map
    Map,
}

/// The column chooser: every column, shown ones first in their current order.
#[derive(Debug, Clone)]
pub struct ColumnChooser {
    pub entries: Vec<(Column, bool)>,
    pub cursor: usize,
}

impl ColumnChooser {
    pub fn new(shown: &[Column]) -> Self {
        let hidden = Column::all().into_iter().filter(|c| !shown.contains(c));
        let entries = shown
            .iter()
            .map(|&c| (c, true))
            .chain(hidden.map(|c| (c, false)))
            .collect();
        Self { entries, cursor: 0 }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.entries.len() - 1);
    }

    /// Show or hide the column under the cursor. The last shown column stays.
    pub fn toggle(&mut self) {
        let shown = self.entries.iter().filter(|(_, on)| *on).count();
        let (_, on) = &mut self.entries[self.cursor];
        if !*on || shown > 1 {
            *on = !*on;
        }
    }

    /// Move the column under the cursor earlier (-1) or later (+1); the cursor follows.
    pub fn shift(&mut self, delta: isize) {
        let to = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.entries.len() - 1);
        self.entries.swap(self.cursor, to);
        self.cursor = to;
    }

    pub fn shown(&self) -> Vec<Column> {
        self.entries
            .iter()
            .filter(|(_, on)| *on)
            .map(|(c, _)| *c)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct UiState {
    pub mode: Mode,
    pub view: View,
    pub columns: Vec<Column>,
    pub chooser: ColumnChooser,
    pub scale: SparklineScale,
    pub colors: ColorSupport,
    pub visualization: VisualizationMode,
    pub show_hostnames: bool,
    /// MPLS labels on rows of their own under the hop (`-e`)
    pub show_mpls: bool,
    /// Selected row and scroll offset of the hop table or flow matrix.
    pub table: TableState,
    /// Hop number of each row in the last frame, so keys can find the selected hop.
    pub row_hops: Vec<u8>,
    /// Select this hop's first row in the next frame.
    pub follow: Option<u8>,
    /// The hop the detail pane shows
    pub detail_hop: u8,
    /// The trace ran all its `--count` rounds.
    pub finished: bool,
}

impl UiState {
    pub fn new(args: &Args) -> Self {
        let columns = args.get_columns();
        Self {
            mode: Mode::Table,
            view: View::Hops,
            chooser: ColumnChooser::new(&columns),
            columns,
            scale: args.sparkline_scale,
            colors: detect_color_support(),
            visualization: VisualizationMode::Sparkline,
            show_hostnames: !args.numeric,
            show_mpls: args.mpls,
            table: TableState::default(),
            row_hops: Vec::new(),
            follow: None,
            detail_hop: 0,
            finished: false,
        }
    }

    pub fn open_chooser(&mut self) {
        self.chooser = ColumnChooser::new(&self.columns);
        self.mode = Mode::Columns;
    }

    /// Changes in the chooser show up in the table straight away.
    pub fn apply_chooser(&mut self) {
        self.columns = self.chooser.shown();
    }

    /// Hop of the selected row, if a row is selected.
    pub fn selected_hop(&self) -> Option<u8> {
        self.table
            .selected()
            .and_then(|row| self.row_hops.get(row))
            .copied()
    }

    /// Show `view`, or the table again if it is showing; the selected hop stays
    /// selected.
    pub fn toggle_view(&mut self, view: View) {
        self.follow = self.selected_hop();
        self.table = TableState::default();
        self.view = if self.view == view { View::Hops } else { view };
    }

    /// Details of the selected hop, or of the first one when none is selected.
    pub fn open_detail(&mut self) {
        if let Some(hop) = self.selected_hop().or(self.row_hops.first().copied()) {
            self.detail_hop = hop;
            self.follow = Some(hop);
            self.mode = Mode::Detail;
        }
    }

    /// Show the hop `delta` places away among those on screen; the table follows.
    pub fn step_detail(&mut self, delta: isize) {
        let mut hops = self.row_hops.clone();
        hops.dedup();
        let Some(at) = hops.iter().position(|&hop| hop == self.detail_hop) else {
            return;
        };
        let to = at.saturating_add_signed(delta).min(hops.len() - 1);
        self.detail_hop = hops[to];
        self.follow = Some(self.detail_hop);
    }

    pub fn toggle_scale(&mut self) {
        self.scale = match self.scale {
            SparklineScale::Linear => SparklineScale::Logarithmic,
            SparklineScale::Logarithmic => SparklineScale::Linear,
        };
    }

    pub fn toggle_visualization(&mut self) {
        self.visualization = match self.visualization {
            VisualizationMode::Sparkline => VisualizationMode::Heatmap,
            VisualizationMode::Heatmap => VisualizationMode::Sparkline,
        };
    }

    pub fn cycle_colors(&mut self) {
        self.colors = match self.colors {
            ColorSupport::None => ColorSupport::Basic,
            ColorSupport::Basic => ColorSupport::Extended,
            ColorSupport::Extended => ColorSupport::TrueColor,
            ColorSupport::TrueColor => ColorSupport::None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn chooser_keeps_the_current_order() {
        let shown = [Column::Avg, Column::Host, Column::Hop];
        let chooser = ColumnChooser::new(&shown);
        assert_eq!(chooser.shown(), shown);
        assert_eq!(chooser.entries.len(), Column::all().len());
        assert!(
            !chooser.entries[3].1,
            "hidden columns follow the shown ones"
        );
    }

    #[test]
    fn the_selected_hop_survives_a_view_change_and_steers_the_detail() {
        let mut ui = UiState::new(&Args::parse_from(["mtr-ng", "example.com"]));
        ui.open_detail();
        assert_eq!(ui.mode, Mode::Table, "nothing to show yet");

        // Hop 2 has an alternate address, so it fills two rows
        ui.row_hops = vec![1, 2, 2, 3];
        ui.table.select(Some(2));
        ui.toggle_view(View::Flows);
        assert_eq!(
            (ui.view, ui.follow, ui.table.selected()),
            (View::Flows, Some(2), None)
        );
        ui.toggle_view(View::Map);
        assert_eq!(ui.view, View::Map, "straight from one view to another");
        ui.toggle_view(View::Map);
        assert_eq!(ui.view, View::Hops);

        ui.row_hops = vec![1, 2, 3];
        ui.table.select(Some(1));
        ui.open_detail();
        assert_eq!((ui.mode, ui.detail_hop), (Mode::Detail, 2));
        ui.step_detail(1);
        ui.step_detail(1);
        assert_eq!(
            (ui.detail_hop, ui.follow),
            (3, Some(3)),
            "stops at the last hop"
        );
        ui.step_detail(-5);
        assert_eq!(ui.detail_hop, 1);
    }

    #[test]
    fn chooser_reorders_and_never_hides_the_last_column() {
        let mut chooser = ColumnChooser::new(&[Column::Hop, Column::Host]);
        chooser.shift(1);
        assert_eq!(chooser.shown(), [Column::Host, Column::Hop]);
        assert_eq!(chooser.cursor, 1);
        chooser.toggle(); // hide Hop
        chooser.move_cursor(-1);
        chooser.toggle(); // Host is the last one shown: stays
        assert_eq!(chooser.shown(), [Column::Host]);
        chooser.move_cursor(-5);
        assert_eq!(chooser.cursor, 0);
        chooser.move_cursor(100);
        assert_eq!(chooser.cursor, chooser.entries.len() - 1);
    }
}
