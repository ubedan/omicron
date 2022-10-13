// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The rack update screen

use super::Screen;
use crate::widgets::{Control, ControlId};
use crate::widgets::{HelpButton, HelpButtonState};
use crate::widgets::{HelpMenu, HelpMenuState};
use crate::widgets::{ScreenButton, ScreenButtonState};
use crate::Action;
use crate::Frame;
use crate::ScreenEvent;
use crate::ScreenId;
use crate::State;
use crate::Term;

use crossterm::event::Event as TermEvent;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

pub struct UpdateScreen {
    hovered: Option<ControlId>,
    help_data: Vec<(&'static str, &'static str)>,
    help_button_state: HelpButtonState,
    help_menu_state: HelpMenuState,
    rack_screen_button_state: ScreenButtonState,
    component_screen_button_state: ScreenButtonState,
}

impl UpdateScreen {
    pub fn new() -> UpdateScreen {
        let help_data = vec![
            ("<TAB>", "Cycle forward through components"),
            ("<SHIFT>-<TAB>", "Cycle backwards through components"),
            ("<CTRL-r", "Go to the rack screen"),
            ("<CTRL-o", "Go to the component screen"),
            ("<CTRL-h", "Toggle this help menu"),
            ("<CTRL-c>", "Exit the program"),
        ];

        UpdateScreen {
            hovered: None,
            help_data,
            help_button_state: HelpButtonState::new(1, 0),
            help_menu_state: HelpMenuState::default(),
            rack_screen_button_state: ScreenButtonState::new(ScreenId::Rack),
            component_screen_button_state: ScreenButtonState::new(
                ScreenId::Component,
            ),
        }
    }
}

impl Screen for UpdateScreen {
    fn draw(&self, state: &State, terminal: &mut Term) -> anyhow::Result<()> {
        Ok(())
    }

    fn on(&mut self, state: &mut State, event: ScreenEvent) -> Vec<Action> {
        vec![]
    }
}
