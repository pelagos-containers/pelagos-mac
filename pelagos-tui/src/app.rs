//! Application state and update logic.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

use crate::runner::{Container, Runner};

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    ProfilePicker,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub mode: Mode,
    pub containers: Vec<Container>,
    /// Index of the highlighted row in Normal mode.
    pub selected: usize,
    /// Whether to pass `--all` to `pelagos ps`.
    pub show_all: bool,
    /// Currently active profile name.
    pub profile: String,
    /// All known profiles (populated at startup and on profile change).
    pub profiles: Vec<String>,
    /// Whether the VM daemon is running (last known state).
    pub vm_running: bool,
    /// When the last successful refresh completed.
    pub last_refresh: Instant,
    /// How often to auto-refresh.
    pub refresh_interval: Duration,
    /// Highlighted row index inside the profile picker overlay.
    pub profile_picker_selected: usize,
    /// Set to true to break the event loop.
    pub should_quit: bool,
}

impl App {
    pub fn new(profile: String, profiles: Vec<String>) -> Self {
        // Pre-select the index of the current profile in the picker.
        let picker_idx = profiles.iter().position(|p| p == &profile).unwrap_or(0);

        Self {
            mode: Mode::Normal,
            containers: Vec::new(),
            selected: 0,
            show_all: false,
            profile,
            profiles,
            vm_running: false,
            last_refresh: Instant::now(),
            refresh_interval: Duration::from_secs(2),
            profile_picker_selected: picker_idx,
            should_quit: false,
        }
    }

    // -----------------------------------------------------------------------
    // Data refresh
    // -----------------------------------------------------------------------

    /// Fetch containers and VM status from the runner.  Errors are swallowed
    /// (logged at debug level) so a stopped VM never crashes the TUI.
    pub fn refresh(&mut self, runner: &impl Runner) {
        self.vm_running = runner.vm_status();

        match runner.ps(self.show_all) {
            Ok(containers) => {
                self.containers = containers;
                // Clamp selected index in case the list shrank.
                if !self.containers.is_empty() && self.selected >= self.containers.len() {
                    self.selected = self.containers.len() - 1;
                }
            }
            Err(e) => {
                log::debug!("refresh: ps failed: {}", e);
                self.containers.clear();
            }
        }

        self.last_refresh = Instant::now();
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent, runner: &impl Runner) {
        match self.mode {
            Mode::Normal => self.on_key_normal(key, runner),
            Mode::ProfilePicker => self.on_key_profile_picker(key, runner),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent, runner: &impl Runner) {
        match key.code {
            // Quit
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.containers.is_empty() {
                    self.selected = (self.selected + 1).min(self.containers.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }

            // Toggle --all
            KeyCode::Char('a') => {
                self.show_all = !self.show_all;
                self.refresh(runner);
            }

            // Open profile picker
            KeyCode::Char('p') => {
                // Sync picker selection to current profile.
                self.profile_picker_selected = self
                    .profiles
                    .iter()
                    .position(|p| p == &self.profile)
                    .unwrap_or(0);
                self.mode = Mode::ProfilePicker;
            }

            _ => {}
        }
    }

    fn on_key_profile_picker(&mut self, key: KeyEvent, runner: &impl Runner) {
        match key.code {
            // Navigate
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.profiles.is_empty() {
                    self.profile_picker_selected =
                        (self.profile_picker_selected + 1).min(self.profiles.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.profile_picker_selected = self.profile_picker_selected.saturating_sub(1);
            }

            // Confirm selection
            KeyCode::Enter => {
                if let Some(chosen) = self.profiles.get(self.profile_picker_selected).cloned() {
                    log::debug!("profile switch: {} -> {}", self.profile, chosen);
                    self.profile = chosen;
                }
                self.mode = Mode::Normal;
                self.refresh(runner);
            }

            // Cancel
            KeyCode::Esc | KeyCode::Char('p') => {
                self.mode = Mode::Normal;
            }

            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    // Used in M2+ (log viewer, stop/rm actions).
    #[allow(dead_code)]
    pub fn selected_container(&self) -> Option<&Container> {
        self.containers.get(self.selected)
    }

    /// How many seconds (rounded) since the last refresh.
    pub fn refresh_age_secs(&self) -> u64 {
        self.last_refresh.elapsed().as_secs()
    }
}
