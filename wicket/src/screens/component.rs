// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inventory and control of individual rack components: sled,switch,psc

use super::colors::*;
use super::Screen;
use crate::widgets::Control;
use crate::widgets::ControlId;
use crate::widgets::HelpMenuState;
use crate::widgets::{HelpButton, HelpButtonState, HelpMenu};
use crate::widgets::{ScreenButton, ScreenButtonState};
use crate::Action;
use crate::Frame;
use crate::ScreenEvent;
use crate::ScreenId;
use crate::State;
use crossterm::event::Event as TermEvent;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tui::layout::Alignment;
use tui::style::Modifier;
use tui::style::{Color, Style};
use tui::text::{Span, Spans, Text};
use tui::widgets::Block;
use tui::widgets::Paragraph;

pub struct ComponentScreen {
    hovered: Option<ControlId>,
    help_data: Vec<(&'static str, &'static str)>,
    help_button_state: HelpButtonState,
    help_menu_state: HelpMenuState,
    rack_screen_button_state: ScreenButtonState,
}

impl ComponentScreen {
    pub fn new() -> ComponentScreen {
        let help_data = vec![
            ("<TAB>", "Cycle forward through components"),
            ("<SHIFT>-<TAB>", "Cycle backwards through components"),
            ("<ESC>", "Go back to the rack screen"),
            ("<CTRL-r", "Go back to the rack screen"),
            ("<CTRL-h", "Toggle this help menu"),
            ("<CTRL-c>", "Exit the program"),
        ];
        ComponentScreen {
            hovered: None,
            help_data,
            help_button_state: HelpButtonState::new(1, 0),
            help_menu_state: HelpMenuState::default(),
            rack_screen_button_state: ScreenButtonState::new(ScreenId::Rack),
        }
    }

    fn draw_background(&self, f: &mut Frame) {
        let style = Style::default().fg(OX_GREEN_DARK).bg(Color::Black);
        let block = Block::default().style(style);
        f.render_widget(block, f.size());
    }

    fn draw_status_bar(&self, f: &mut Frame, state: &State) {
        let mut rect = f.size();
        rect.height = 5;

        let style = Style::default().bg(OX_GREEN_DARK).fg(OX_GRAY);
        let selected_style = Style::default().fg(OX_GREEN_LIGHT);
        let help_menu_style =
            Style::default().fg(OX_OFF_WHITE).bg(OX_GREEN_DARK);
        let help_menu_command_style =
            Style::default().fg(OX_GREEN_LIGHT).bg(OX_GREEN_DARK);
        let button_style = Style::default().fg(OX_OFF_WHITE).bg(OX_GREEN_DARK);
        let hovered_style = Style::default().fg(OX_PINK).bg(OX_GREEN_DARK);

        let status_bar_block = Block::default().style(style);
        f.render_widget(status_bar_block, rect);

        let current = state.rack_state.get_current_component_id();

        // Draw the components list
        // TODO: Some sliding style animation?
        let title = Spans::from(vec![
            Span::styled(
                state.rack_state.get_next_component_id().name(),
                style,
            ),
            Span::raw("   "),
            Span::styled(current.name(), selected_style),
            Span::raw("   "),
            Span::styled(
                state.rack_state.get_next_component_id().name(),
                style,
            ),
        ]);

        let mut rect = f.size();
        rect.height = 1;
        rect.y = 1;
        let title_block = Block::default()
            .style(style)
            .title(title)
            .title_alignment(Alignment::Center);
        f.render_widget(title_block, rect);

        // Draw the power state
        let title = match state.inventory.get_power_state(&current) {
            Some(s) => {
                format!(
                    "⌁ Power State: {}",
                    s.description().to_ascii_uppercase()
                )
            }
            None => "⌁ Power State: UNKNOWN".to_string(),
        };

        let mut rect = f.size();
        rect.height = 1;
        rect.y = 3;
        let power_state_block = Block::default()
            .style(selected_style)
            .title(title)
            .title_alignment(Alignment::Center);
        f.render_widget(power_state_block, rect);

        // Draw the help button if the help menu is closed, otherwise draw the
        // help menu
        if !self.help_menu_state.is_closed() {
            let menu = HelpMenu {
                help: &self.help_data,
                style: help_menu_style,
                command_style: help_menu_command_style,
                // Unwrap is safe because we check that the menu is open (and
                // thus has an AnimationState).
                state: self.help_menu_state.get_animation_state().unwrap(),
            };
            f.render_widget(menu, f.size());
        } else {
            let border_style =
                if self.hovered == Some(self.help_button_state.id()) {
                    hovered_style
                } else {
                    button_style
                };
            let button = HelpButton::new(
                &self.help_button_state,
                button_style,
                border_style,
            );

            f.render_widget(button, f.size());
        }

        // Draw the RackSreenButton
        let border_style =
            if self.hovered == Some(self.rack_screen_button_state.id()) {
                hovered_style
            } else {
                button_style
            };
        let button = ScreenButton::new(
            &self.rack_screen_button_state,
            button_style,
            border_style,
        );
        f.render_widget(button, f.size());
    }

    fn draw_inventory(&self, f: &mut Frame, state: &State) {
        // Draw the header
        let selected_style = Style::default().fg(OX_GREEN_LIGHT);
        let inventory_style = Style::default().fg(OX_YELLOW_DIM);

        let mut header_style = selected_style;
        header_style =
            header_style.add_modifier(Modifier::UNDERLINED | Modifier::BOLD);

        let text = Text::styled("INVENTORY\n\n", header_style);
        let mut rect = f.size();
        rect.y = 6;
        rect.height -= 6;
        let center = (rect.width - text.width() as u16) / 2;
        rect.x += center;
        rect.width -= center;
        let header = Paragraph::new(text);
        f.render_widget(header, rect);

        // Draw the contents
        let text = match state
            .inventory
            .get_inventory(&state.rack_state.get_current_component_id())
        {
            Some(inventory) => {
                Text::styled(format!("{:#?}", inventory), inventory_style)
            }
            None => Text::styled("UNKNOWN", inventory_style),
        };

        let mut rect = f.size();
        rect.y = 9;
        rect.height -= 9;

        let center = (rect.width - text.width() as u16) / 2;
        rect.x += center;
        rect.width -= center;

        let inventory = Paragraph::new(text);
        f.render_widget(inventory, rect);
    }

    fn handle_key_event(
        &mut self,
        state: &mut State,
        event: KeyEvent,
    ) -> Vec<Action> {
        match event.code {
            KeyCode::Tab => {
                state.rack_state.inc_tab_index();
            }
            KeyCode::BackTab => {
                state.rack_state.dec_tab_index();
            }
            KeyCode::Char('r') => {
                if event.modifiers.contains(KeyModifiers::CONTROL) {
                    return vec![Action::SwitchScreen(ScreenId::Rack)];
                }
            }
            KeyCode::Char('h') => {
                if event.modifiers.contains(KeyModifiers::CONTROL) {
                    self.help_menu_state.toggle();
                }
            }
            _ => (),
        }
        vec![Action::Redraw]
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.rack_screen_button_state.rect.x =
            width - ScreenButtonState::width();
    }

    fn handle_mouse_event(
        &mut self,
        state: &mut State,
        event: MouseEvent,
    ) -> Vec<Action> {
        match event.kind {
            MouseEventKind::Moved => {
                self.set_hover_state(state, event.column, event.row)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_click(state)
            }
            _ => vec![],
        }
    }

    fn handle_mouse_click(&mut self, _: &mut State) -> Vec<Action> {
        match self.hovered {
            Some(control_id) if control_id == self.help_button_state.id() => {
                self.help_menu_state.open();
                vec![]
            }
            Some(control_id)
                if control_id == self.rack_screen_button_state.id() =>
            {
                vec![Action::SwitchScreen(ScreenId::Rack)]
            }
            _ => vec![],
        }
    }

    fn set_hover_state(
        &mut self,
        _state: &mut State,
        x: u16,
        y: u16,
    ) -> Vec<Action> {
        let current_id = self.find_intersection(x, y);
        if current_id == self.hovered {
            // No change
            vec![]
        } else {
            self.hovered = current_id;
            vec![Action::Redraw]
        }
    }

    // Return if the coordinates interesct a given control.
    // This assumes disjoint control rectangles.
    fn find_intersection(&self, x: u16, y: u16) -> Option<ControlId> {
        if self.help_button_state.intersects_point(x, y) {
            Some(self.help_button_state.id())
        } else if self.rack_screen_button_state.intersects_point(x, y) {
            Some(self.rack_screen_button_state.id())
        } else {
            None
        }
    }
}

impl Screen for ComponentScreen {
    fn draw(
        &self,
        state: &State,
        terminal: &mut crate::Term,
    ) -> anyhow::Result<()> {
        terminal.draw(|f| {
            self.draw_background(f);
            self.draw_status_bar(f, state);
            self.draw_inventory(f, state);
        })?;
        Ok(())
    }

    fn on(&mut self, state: &mut State, event: ScreenEvent) -> Vec<Action> {
        match event {
            ScreenEvent::Term(TermEvent::Key(key_event)) => {
                self.handle_key_event(state, key_event)
            }
            ScreenEvent::Term(TermEvent::Mouse(mouse_event)) => {
                self.handle_mouse_event(state, mouse_event)
            }
            ScreenEvent::Term(TermEvent::Resize(width, height)) => {
                self.resize(width, height);
                // A redraw always occurs in the wizard
                vec![]
            }
            ScreenEvent::Tick => {
                if !self.help_menu_state.is_closed() {
                    self.help_menu_state.step();
                    vec![Action::Redraw]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }
}
