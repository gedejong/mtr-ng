//! What the TUI shows and how: the active mode, the columns, the display toggles,
//! and the table's selection and scroll position.

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
    pub columns: Vec<Column>,
    pub chooser: ColumnChooser,
    pub scale: SparklineScale,
    pub colors: ColorSupport,
    pub visualization: VisualizationMode,
    pub show_hostnames: bool,
    /// Selected row and scroll offset of the hop table.
    pub table: TableState,
    /// The trace ran all its `--count` rounds.
    pub finished: bool,
}

impl UiState {
    pub fn new(args: &Args) -> Self {
        let columns = args.get_columns();
        Self {
            mode: Mode::Table,
            chooser: ColumnChooser::new(&columns),
            columns,
            scale: args.sparkline_scale,
            colors: detect_color_support(),
            visualization: VisualizationMode::Sparkline,
            show_hostnames: !args.numeric,
            table: TableState::default(),
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
