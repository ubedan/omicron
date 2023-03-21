// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An executor for [`wicket::Event`]s in the [`wicket_dbg::Server`]

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use slog::Logger;
use std::io::{stdout, Stdout};
use tui::backend::CrosstermBackend;
use tui::Terminal;

use wicket::{Event, Frame, RunnerCore, Screen, Snapshot, State, Term};

/// A parallel to [`wicket::Runner`] that allows stepping through individual
/// events, and doesn't interact with any external services. The only goal
/// is to replay the visuals of wicket, given a recording.
pub struct Runner {
    core: RunnerCore,
    snapshot: Option<Snapshot>,
}

impl Runner {
    pub fn new(log: Logger) -> Runner {
        let backend = CrosstermBackend::new(stdout());
        let core = RunnerCore {
            screen: Screen::new(&log),
            state: State::new(&log),
            terminal: Terminal::new(backend).unwrap(),
            log,
        };
        Runner { core, snapshot: None }
    }

    /// Initialize the terminal for drawing
    pub fn init_terminal(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        execute!(self.core.terminal.backend_mut(), EnterAlternateScreen,)?;
        Ok(())
    }

    /// Restore the terminal to its original state
    pub fn fini_terminal(&mut self) -> anyhow::Result<()> {
        disable_raw_mode()?;
        execute!(self.core.terminal.backend_mut(), LeaveAlternateScreen,)?;
        Ok(())
    }

    /// Load a new snapshot
    pub fn load_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
    }

    /// Restart the debugger
    pub fn restart(&mut self) -> anyhow::Result<()> {
        self.core.init_screen()?;
        Ok(())
    }
}
