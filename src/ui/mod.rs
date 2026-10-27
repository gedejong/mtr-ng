//! The interactive terminal interface.

pub mod events;
mod main;
mod render;
pub mod state;
pub mod visualization;

pub use main::run_interactive;
pub use state::UiState;
