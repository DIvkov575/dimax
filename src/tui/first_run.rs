//! The one-time first-run wizard shown on the very first `dimax attach`
//! against a given `keybindings.json` (gated by
//! `keys::consume_first_run`). Two steps: pick a keybinding mode, then
//! confirm/decline installing the bundled Claude Code skill. Entirely
//! local -- no daemon requests, unlike almost everything else `App`
//! does -- so it's driven straight off raw stdin bytes in `run`'s loop,
//! before the daemon connection's `bootstrap` even happens.

use super::keys::BindingMode;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

/// Which of the two steps is currently on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Keybindings,
    SkillInstall,
}

/// Live state for the wizard. Constructed once at TUI startup (see
/// `Wizard::maybe_start`) and dropped the moment either step's choice is
/// committed or Esc skips the rest -- there is no persistent "reopen the
/// wizard" path, matching `keys::consume_first_run`'s one-shot contract.
pub struct Wizard {
    step: Step,
    /// Index into `BindingMode::ALL` -- `Portable` first since it's the
    /// least invasive default (no terminal config touched), matching
    /// `BindingMode::default()`.
    mode_selected: usize,
    /// Currently-highlighted choice on the skill-install step: `true` is
    /// "yes, install" (the default -- pressing Enter with no input
    /// installs it), `false` is "no, skip".
    install_skill: bool,
}

const MODES: [BindingMode; 4] = [
    BindingMode::Portable,
    BindingMode::Kitty,
    BindingMode::Both,
    BindingMode::Tmux,
];

fn mode_label(mode: BindingMode) -> &'static str {
    match mode {
        BindingMode::Portable => "Portable (Ctrl-Space prefix, no terminal config changes)",
        BindingMode::Kitty => "Kitty (Cmd-key chords, amends kitty.conf)",
        BindingMode::Both => "Both (Ctrl-Space prefix and Cmd-key chords)",
        BindingMode::Tmux => "Tmux (Ctrl-B prefix, tmux-compatible chords)",
    }
}

/// What committing a step should do, resolved by `App::handle_wizard_input`
/// into real work (`keys::save_mode`, `kitty_setup::install`,
/// `skills_setup::install`) -- kept here as plain data so this module
/// stays free of any filesystem/process side effects, mirroring how
/// `AttachMenuAction` separates "what was pressed" from "what happens".
pub enum WizardOutcome {
    /// Still in progress; nothing to apply yet.
    Continue,
    /// The wizard finished (committed both steps, or Esc skipped the
    /// rest) -- apply defaults for anything not explicitly chosen.
    Done {
        mode: BindingMode,
        install_skill: bool,
    },
}

impl Wizard {
    pub fn new() -> Self {
        Self {
            step: Step::Keybindings,
            mode_selected: 0,
            install_skill: true,
        }
    }

    /// Route one raw input chunk. Mirrors `App::handle_attach_menu_input`'s
    /// byte-level dispatch style rather than `keys::parse`'s chord
    /// grammar -- this is a plain modal with its own tiny grammar, not
    /// something a Kitty chord should ever address.
    pub fn handle_input(&mut self, bytes: &[u8]) -> WizardOutcome {
        match self.step {
            Step::Keybindings => match bytes {
                b"\x1b[A" | b"k" => {
                    self.mode_selected = (self.mode_selected + MODES.len() - 1) % MODES.len();
                    WizardOutcome::Continue
                }
                b"\x1b[B" | b"j" => {
                    self.mode_selected = (self.mode_selected + 1) % MODES.len();
                    WizardOutcome::Continue
                }
                b"\r" | b"\n" => {
                    self.step = Step::SkillInstall;
                    WizardOutcome::Continue
                }
                b"\x1b" => WizardOutcome::Done {
                    mode: MODES[self.mode_selected],
                    install_skill: self.install_skill,
                },
                _ => WizardOutcome::Continue,
            },
            Step::SkillInstall => match bytes {
                b"y" | b"Y" => {
                    self.install_skill = true;
                    WizardOutcome::Continue
                }
                b"n" | b"N" => {
                    self.install_skill = false;
                    WizardOutcome::Continue
                }
                b"\r" | b"\n" | b"\x1b" => WizardOutcome::Done {
                    mode: MODES[self.mode_selected],
                    install_skill: self.install_skill,
                },
                _ => WizardOutcome::Continue,
            },
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = super::render::centered_rect(70, 50, frame.area());
        frame.render_widget(Clear, area);

        match self.step {
            Step::Keybindings => self.draw_keybindings_step(frame, area),
            Step::SkillInstall => self.draw_skill_step(frame, area),
        }
    }

    fn draw_keybindings_step(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered().title("Welcome to dimax -- choose a keybinding mode");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::new(
            Direction::Vertical,
            MODES.iter().map(|_| Constraint::Length(1)),
        )
        .split(inner);
        for (index, mode) in MODES.iter().enumerate() {
            let selected = index == self.mode_selected;
            let text = format!("{} {}", if selected { ">" } else { " " }, mode_label(*mode));
            let style = if selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            frame.render_widget(Paragraph::new(Line::styled(text, style)), rows[index]);
        }
        let hint_area = *rows.last().expect("MODES is non-empty");
        let hint_area = ratatui::layout::Rect {
            y: hint_area.y + 1,
            height: 1,
            ..hint_area
        };
        if hint_area.y < inner.y + inner.height {
            frame.render_widget(
                Paragraph::new(Span::raw(
                    "j/k or arrows to move, Enter to confirm, Esc to skip with defaults",
                )),
                hint_area,
            );
        }
    }

    fn draw_skill_step(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let block = Block::bordered().title("Install the dimax-pane-control Claude skill?");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::new(
            Direction::Vertical,
            [Constraint::Length(1), Constraint::Length(1)],
        )
        .split(inner);
        frame.render_widget(
            Paragraph::new(
                "Lets Claude Code drive dimax server-panes (spawn/send/read) from any project.",
            ),
            rows[0],
        );
        let choice = if self.install_skill {
            "> Yes (default)    No"
        } else {
            "  Yes    > No (default)"
        };
        frame.render_widget(Paragraph::new(choice), rows[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn buffer_contains(terminal: &Terminal<TestBackend>, needle: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        content.contains(needle)
    }

    #[test]
    fn draw_keybindings_step_shows_every_mode_and_the_hint() {
        let wizard = Wizard::new();
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| wizard.draw(frame)).unwrap();
        assert!(buffer_contains(&terminal, "Portable"));
        assert!(buffer_contains(&terminal, "Kitty"));
        assert!(buffer_contains(&terminal, "Both"));
        assert!(buffer_contains(&terminal, "Esc to skip"));
    }

    #[test]
    fn draw_skill_step_shows_the_prompt_and_default_choice() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"\r");
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| wizard.draw(frame)).unwrap();
        assert!(buffer_contains(
            &terminal,
            "dimax-pane-control Claude skill"
        ));
        assert!(buffer_contains(&terminal, "Yes (default)"));
    }

    #[test]
    fn starts_on_keybindings_step_with_portable_selected() {
        let wizard = Wizard::new();
        assert_eq!(wizard.step, Step::Keybindings);
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Portable);
    }

    #[test]
    fn down_then_down_cycles_through_every_mode_and_wraps() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"j");
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Kitty);
        wizard.handle_input(b"j");
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Both);
        wizard.handle_input(b"j");
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Tmux);
        wizard.handle_input(b"j");
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Portable);
    }

    #[test]
    fn up_from_the_first_mode_wraps_to_the_last() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"k");
        assert_eq!(MODES[wizard.mode_selected], BindingMode::Tmux);
    }

    #[test]
    fn enter_on_keybindings_step_advances_to_skill_step_without_finishing() {
        let mut wizard = Wizard::new();
        let outcome = wizard.handle_input(b"\r");
        assert!(matches!(outcome, WizardOutcome::Continue));
        assert_eq!(wizard.step, Step::SkillInstall);
    }

    #[test]
    fn esc_on_keybindings_step_finishes_with_current_selection_and_default_skill_choice() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"j");
        let outcome = wizard.handle_input(b"\x1b");
        match outcome {
            WizardOutcome::Done {
                mode,
                install_skill,
            } => {
                assert_eq!(mode, BindingMode::Kitty);
                assert!(install_skill);
            }
            WizardOutcome::Continue => panic!("expected Done"),
        }
    }

    #[test]
    fn n_then_enter_on_skill_step_finishes_declining_the_skill() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"\r");
        wizard.handle_input(b"n");
        let outcome = wizard.handle_input(b"\r");
        match outcome {
            WizardOutcome::Done { install_skill, .. } => assert!(!install_skill),
            WizardOutcome::Continue => panic!("expected Done"),
        }
    }

    #[test]
    fn enter_on_skill_step_with_no_toggle_defaults_to_installing() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"\r");
        let outcome = wizard.handle_input(b"\r");
        match outcome {
            WizardOutcome::Done { install_skill, .. } => assert!(install_skill),
            WizardOutcome::Continue => panic!("expected Done"),
        }
    }

    #[test]
    fn esc_on_skill_step_finishes_with_current_toggle() {
        let mut wizard = Wizard::new();
        wizard.handle_input(b"\r");
        wizard.handle_input(b"n");
        let outcome = wizard.handle_input(b"\x1b");
        match outcome {
            WizardOutcome::Done { install_skill, .. } => assert!(!install_skill),
            WizardOutcome::Continue => panic!("expected Done"),
        }
    }
}
