//! Platform-agnostic screensaver state machine.
//!
//! The `Engine` turns backend events (idle, resume, input, signals, display
//! changes) into commands the backend executes (show/hide surfaces, hold or
//! release the keep-awake assertion, quit). It owns the activation policy and
//! the dismiss/resume grace periods, and has no dependency on any windowing
//! system, so it is unit-tested without a display.

use std::time::{Duration, Instant};

use crate::config::{Activation, DisplayScope, KeepAwake};

pub type DisplayId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    Idle,
    /// Emitted only by the Wayland backend (macOS dismisses via `DismissInput`).
    #[allow(dead_code)]
    Resume,
    DismissInput,
    /// The backend could not create any saver surface for a `Show` (e.g. zero
    /// attached displays). Emitted only by the macOS backend.
    #[allow(dead_code)]
    ShowFailed,
    ActivateSignal,
    /// Reserved graceful-shutdown event; no backend emits it yet.
    #[allow(dead_code)]
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    Show { displays: Vec<DisplayId> },
    Hide,
    SetKeepAwake(bool),
    Quit,
}

/// Ignore an input dismiss this long after activation (creating the surface can
/// itself look like input on some platforms).
const DISMISS_GRACE: Duration = Duration::from_secs(1);
/// Ignore an idle-resume this long after activation (showing the surface can
/// reset the compositor's idle timer).
const RESUME_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum State {
    Dormant,
    Active { activated_at: Instant },
}

pub struct Engine {
    activation: Activation,
    displays: Vec<DisplayId>,
    state: State,
    keep_awake_held: bool,
}

impl Engine {
    pub fn new(activation: Activation, displays: Vec<DisplayId>) -> Self {
        Self {
            activation,
            displays,
            state: State::Dormant,
            keep_awake_held: false,
        }
    }

    /// Commands to emit once at startup (holds keep-awake under the "always" policy).
    pub fn init(&mut self) -> Vec<EngineCommand> {
        if self.activation.keep_awake == KeepAwake::Always {
            self.keep_awake_held = true;
            vec![EngineCommand::SetKeepAwake(true)]
        } else {
            vec![]
        }
    }

    fn target_displays(&self) -> Vec<DisplayId> {
        match self.activation.displays {
            DisplayScope::All => self.displays.clone(),
            DisplayScope::Primary => self.displays.iter().copied().take(1).collect(),
        }
    }

    fn activate(&mut self, now: Instant) -> Vec<EngineCommand> {
        if matches!(self.state, State::Active { .. }) {
            return vec![];
        }
        self.state = State::Active { activated_at: now };
        let mut cmds = vec![EngineCommand::Show {
            displays: self.target_displays(),
        }];
        if self.activation.keep_awake == KeepAwake::WhileActive && !self.keep_awake_held {
            self.keep_awake_held = true;
            cmds.push(EngineCommand::SetKeepAwake(true));
        }
        cmds
    }

    fn deactivate(&mut self) -> Vec<EngineCommand> {
        if !matches!(self.state, State::Active { .. }) {
            return vec![];
        }
        self.state = State::Dormant;
        let mut cmds = vec![EngineCommand::Hide];
        if self.activation.keep_awake == KeepAwake::WhileActive && self.keep_awake_held {
            self.keep_awake_held = false;
            cmds.push(EngineCommand::SetKeepAwake(false));
        }
        cmds
    }

    pub fn handle(&mut self, ev: BackendEvent, now: Instant) -> Vec<EngineCommand> {
        match ev {
            BackendEvent::Idle | BackendEvent::ActivateSignal => self.activate(now),
            // No surface exists, so the grace periods (which guard a *visible*
            // saver against its own activation side effects) must not apply.
            BackendEvent::ShowFailed => self.deactivate(),
            BackendEvent::Resume => {
                if let State::Active { activated_at } = self.state
                    && now.duration_since(activated_at) < RESUME_GRACE
                {
                    return vec![];
                }
                self.deactivate()
            }
            BackendEvent::DismissInput => {
                if let State::Active { activated_at } = self.state
                    && now.duration_since(activated_at) < DISMISS_GRACE
                {
                    return vec![];
                }
                self.deactivate()
            }
            BackendEvent::Quit => {
                let mut cmds = self.deactivate();
                if self.keep_awake_held {
                    self.keep_awake_held = false;
                    cmds.push(EngineCommand::SetKeepAwake(false));
                }
                cmds.push(EngineCommand::Quit);
                cmds
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Activation, DisplayScope, KeepAwake};

    fn eng(displays: DisplayScope, keep: KeepAwake, ids: &[DisplayId]) -> Engine {
        Engine::new(
            Activation {
                displays,
                keep_awake: keep,
            },
            ids.to_vec(),
        )
    }

    #[test]
    fn idle_shows_on_primary_by_default() {
        let mut e = eng(DisplayScope::Primary, KeepAwake::WhileActive, &[1, 2, 3]);
        let cmds = e.handle(BackendEvent::Idle, Instant::now());
        assert_eq!(cmds[0], EngineCommand::Show { displays: vec![1] });
        assert!(cmds.contains(&EngineCommand::SetKeepAwake(true)));
    }

    #[test]
    fn idle_shows_on_all_when_configured() {
        let mut e = eng(DisplayScope::All, KeepAwake::WhileActive, &[1, 2, 3]);
        let cmds = e.handle(BackendEvent::Idle, Instant::now());
        assert_eq!(
            cmds[0],
            EngineCommand::Show {
                displays: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn keep_awake_always_holds_from_init_and_not_released_on_hide() {
        let mut e = eng(DisplayScope::All, KeepAwake::Always, &[1]);
        assert_eq!(e.init(), vec![EngineCommand::SetKeepAwake(true)]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        let cmds = e.handle(BackendEvent::DismissInput, now + Duration::from_secs(2));
        assert!(cmds.contains(&EngineCommand::Hide));
        assert!(!cmds.contains(&EngineCommand::SetKeepAwake(false)));
    }

    #[test]
    fn dismiss_within_1s_is_ignored() {
        let mut e = eng(DisplayScope::Primary, KeepAwake::WhileActive, &[1]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        let cmds = e.handle(BackendEvent::DismissInput, now + Duration::from_millis(500));
        assert!(cmds.is_empty());
    }

    #[test]
    fn resume_within_5s_is_ignored() {
        let mut e = eng(DisplayScope::Primary, KeepAwake::WhileActive, &[1]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        let cmds = e.handle(BackendEvent::Resume, now + Duration::from_secs(2));
        assert!(cmds.is_empty());
    }

    #[test]
    fn dismiss_after_grace_hides_and_releases_while_active() {
        let mut e = eng(DisplayScope::Primary, KeepAwake::WhileActive, &[1]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        let cmds = e.handle(BackendEvent::DismissInput, now + Duration::from_secs(2));
        assert!(cmds.contains(&EngineCommand::Hide));
        assert!(cmds.contains(&EngineCommand::SetKeepAwake(false)));
    }

    #[test]
    fn show_failed_returns_to_dormant_immediately_despite_grace() {
        let mut e = eng(DisplayScope::All, KeepAwake::WhileActive, &[1]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        // Reported right after Show, well inside the dismiss grace — must not be ignored.
        let cmds = e.handle(BackendEvent::ShowFailed, now);
        assert!(cmds.contains(&EngineCommand::Hide));
        assert!(cmds.contains(&EngineCommand::SetKeepAwake(false)));
    }

    #[test]
    fn idle_can_reactivate_after_show_failed() {
        let mut e = eng(DisplayScope::All, KeepAwake::WhileActive, &[1, 2]);
        let now = Instant::now();
        e.handle(BackendEvent::Idle, now);
        e.handle(BackendEvent::ShowFailed, now);
        let cmds = e.handle(BackendEvent::Idle, now + Duration::from_secs(1));
        assert_eq!(
            cmds[0],
            EngineCommand::Show {
                displays: vec![1, 2]
            }
        );
    }

    #[test]
    fn show_failed_while_dormant_is_a_no_op() {
        let mut e = eng(DisplayScope::All, KeepAwake::WhileActive, &[1]);
        let cmds = e.handle(BackendEvent::ShowFailed, Instant::now());
        assert!(cmds.is_empty());
    }
}
