//! Parses raw input bytes into [`super::Action`]s.
//!
//! Three input sources feed this: normal keystrokes (passed through to the
//! focused pane), portable Ctrl-Space prefix sequences, and optional
//! Kitty-forwarded Cmd chords. [`BINDINGS`] is the shared source of truth
//! for parsing, generated Kitty config, CLI help, and custom-alias actions.
//!
//! # Chord encoding
//!
//! There is no pre-existing "Cmd chord to escape sequence" standard to
//! match, so this module defines its own. Every dimax chord is encoded as
//! an APC-style private escape sequence:
//!
//! ```text
//! ESC _ D <tag> ESC \
//! 0x1B 0x5F 0x44 <tag byte(s)> 0x1B 0x5C
//! ```
//!
//! - `ESC _` (`0x1B 0x5F`) opens an Application Program Command (APC)
//!   string — a control sequence real terminal programs essentially never
//!   emit on their own, and Kitty never generates from normal keyboard
//!   input. Using it as a private prefix means we will never misinterpret
//!   an ordinary keystroke or a program's own output as a dimax chord.
//! - `D` identifies this APC string as a "dimax chord" (as opposed to some
//!   other private APC use some other tool might pick).
//! - `<tag>` is one or more ASCII bytes identifying which chord.
//! - `ESC \` (`0x1B 0x5C`, the standard "String Terminator", `ST`)
//!   terminates the APC string.
//!
//! Full table of `kitty.conf` `send_text` payloads (each written here as
//! the literal bytes to configure, using `\x1b` for `ESC`):
//!
//! | Chord         | Tag  | Bytes                        |
//! |---------------|------|-------------------------------|
//! | `cmd-1`       | `1`  | `\x1b_D1\x1b\\`               |
//! | `cmd-2`       | `2`  | `\x1b_D2\x1b\\`               |
//! | `cmd-3`       | `3`  | `\x1b_D3\x1b\\`               |
//! | `cmd-4`       | `4`  | `\x1b_D4\x1b\\`               |
//! | `cmd-5`       | `5`  | `\x1b_D5\x1b\\`               |
//! | `cmd-6`       | `6`  | `\x1b_D6\x1b\\`               |
//! | `cmd-7`       | `7`  | `\x1b_D7\x1b\\`               |
//! | `cmd-8`       | `8`  | `\x1b_D8\x1b\\`               |
//! | `cmd-9`       | `9`  | `\x1b_D9\x1b\\`               |
//! | `cmd-d`       | `d`  | `\x1b_Dd\x1b\\`               |
//! | `cmd-shift-d` | `D`  | `\x1b_DD\x1b\\`               |
//! | `cmd-w`       | `w`  | `\x1b_Dw\x1b\\`               |
//! | `cmd-shift-w` | `W`  | `\x1b_DW\x1b\\`               |
//! | `cmd-shift-z` | `Z`  | `\x1b_DZ\x1b\\`               |
//! | `cmd-h`       | `h`  | `\x1b_Dh\x1b\\`               |
//! | `cmd-j`       | `j`  | `\x1b_Dj\x1b\\`               |
//! | `cmd-k`       | `k`  | `\x1b_Dk\x1b\\`               |
//! | `cmd-l`       | `l`  | `\x1b_Dl\x1b\\`               |
//! | `shift-enter` | `s`  | `\x1b_Ds\x1b\\`               |
//! | `cmd-t`       | `t`  | `\x1b_Dt\x1b\\`               |
//! | `cmd-]`       | `]`  | `\x1b_D]\x1b\\`               |
//! | `cmd-[`       | `[`  | `\x1b_D[\x1b\\`               |
//!
//! (Named-chord tags are case-sensitive letters, deliberately mirroring
//! the shift relationship: `cmd-shift-d` uses the uppercase of `cmd-d`'s
//! tag, etc. — easy to eyeball in a `kitty.conf` diff.)
//!
//! Anything that does not match an enabled input layer resolves to
//! [`ParsedInput::PassThrough`], so the caller forwards it to the focused
//! server-pane. Portable parsing is stateful so a prefix and its following
//! key can arrive in separate terminal reads.
//!
//! `shift-enter` is the one entry in this table with no [`Chord`]/
//! [`Action`] variant of its own: plain Enter and Shift+Enter are
//! otherwise indistinguishable (both just `\r`) without a terminal-level
//! remap, but nothing in the *main* keymap currently needs to tell them
//! apart -- only the attach menu's per-group spawn field does (spawn+
//! bind+send vs. spawn+send-but-leave-unbound). So this chord is
//! recognized as a raw byte sequence directly by `super::mod`'s
//! attach-menu input handling, bypassing [`parse`]/[`Action`] entirely,
//! via the [`SHIFT_ENTER_CHORD`] constant below.

use super::Action;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// APC opener: `ESC _ D`.
const PREFIX: &[u8] = b"\x1b_D";
/// String terminator: `ESC \`.
const TERMINATOR: &[u8] = b"\x1b\\";
/// Portable prefix emitted by Ctrl-Space in conventional terminal modes.
pub const PORTABLE_PREFIX: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BindingMode {
    Portable,
    Kitty,
    Both,
}

impl Default for BindingMode {
    fn default() -> Self {
        Self::Portable
    }
}

impl BindingMode {
    pub fn portable_enabled(self) -> bool {
        matches!(self, Self::Portable | Self::Both)
    }

    pub fn kitty_enabled(self) -> bool {
        matches!(self, Self::Kitty | Self::Both)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct KeybindingConfig {
    version: u8,
    mode: BindingMode,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
    #[serde(default)]
    kitty_bindings: BTreeMap<String, String>,
    /// Whether the one-time first-run wizard (keybinding mode + Claude
    /// skill install, shown on `dimax attach`) has already run against
    /// this config file. `#[serde(default)]` means an existing
    /// `keybindings.json` predating this field reads back as `false` --
    /// so upgrading users see the wizard once too, which is acceptable
    /// (Esc/defaults make it a two-second skip) and avoids a separate
    /// migration.
    #[serde(default)]
    first_run_seen: bool,
}

impl Default for KeybindingConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: BindingMode::default(),
            bindings: BTreeMap::new(),
            kitty_bindings: BTreeMap::new(),
            first_run_seen: false,
        }
    }
}

fn config_path() -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("cannot locate config directory: HOME is unset"))?,
        )
        .join(".config"),
    };
    Ok(base.join("dimax").join("keybindings.json"))
}

fn load_config() -> KeybindingConfig {
    let Ok(path) = config_path() else {
        return KeybindingConfig::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return KeybindingConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn load_mode() -> BindingMode {
    load_config().mode
}

pub fn save_mode(mode: BindingMode) -> anyhow::Result<PathBuf> {
    let mut config = load_config();
    config.mode = mode;
    save_config(&config)
}

/// Atomically check-and-set the first-run wizard's flag, mirroring
/// `daemon::state::consume_shell_fallback`'s pattern. Returns `true` the
/// first time this is ever called against a given `keybindings.json`
/// (the caller should show the wizard), `false` every time after --
/// including when saving the flag itself fails, so a transient write
/// error never wedges the wizard into showing on every single attach.
pub fn consume_first_run() -> bool {
    let mut config = load_config();
    let available = !config.first_run_seen;
    config.first_run_seen = true;
    let _ = save_config(&config);
    available
}

fn save_config(config: &KeybindingConfig) -> anyhow::Result<PathBuf> {
    let path = config_path()?;
    let parent = path
        .parent()
        .expect("keybinding config always has a parent");
    std::fs::create_dir_all(parent)?;
    let text = serde_json::to_string_pretty(config)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, format!("{text}\n"))?;
    std::fs::rename(&temporary, &path)?;
    Ok(path)
}

pub fn action_name(action: Action) -> &'static str {
    match action {
        Action::SwitchWorkspace(1) => "workspace-1",
        Action::SwitchWorkspace(2) => "workspace-2",
        Action::SwitchWorkspace(3) => "workspace-3",
        Action::SwitchWorkspace(4) => "workspace-4",
        Action::SwitchWorkspace(5) => "workspace-5",
        Action::SwitchWorkspace(6) => "workspace-6",
        Action::SwitchWorkspace(7) => "workspace-7",
        Action::SwitchWorkspace(8) => "workspace-8",
        Action::SwitchWorkspace(9) => "workspace-9",
        Action::JumpSession(1) => "session-1",
        Action::JumpSession(2) => "session-2",
        Action::JumpSession(3) => "session-3",
        Action::JumpSession(4) => "session-4",
        Action::JumpSession(5) => "session-5",
        Action::JumpSession(6) => "session-6",
        Action::JumpSession(7) => "session-7",
        Action::JumpSession(8) => "session-8",
        Action::JumpSession(9) => "session-9",
        Action::SplitVertical => "split-vertical",
        Action::SplitHorizontal => "split-horizontal",
        Action::CloseFocusedPane => "close-tab",
        Action::KillFocusedServerPane => "kill-session",
        Action::DetachAndAttach => "choose-session",
        Action::AddTab => "add-tab",
        Action::CycleTabForward => "next-tab",
        Action::CycleTabBackward => "previous-tab",
        Action::FocusLeft => "focus-left",
        Action::FocusRight => "focus-right",
        Action::FocusUp => "focus-up",
        Action::FocusDown => "focus-down",
        Action::Quit => "quit",
        Action::SwitchWorkspace(_) | Action::JumpSession(_) | Action::PassThrough => "unsupported",
    }
}

pub fn parse_action_name(name: &str) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|binding| action_name(binding.action) == name)
        .map(|binding| binding.action)
}

pub fn add_custom_binding(
    action: &str,
    portable: Option<&str>,
    kitty: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let resolved = parse_action_name(action)
        .ok_or_else(|| anyhow::anyhow!("unknown action {action:?}; run `dimax keys list`"))?;
    if portable.is_none() && kitty.is_none() {
        anyhow::bail!("provide --portable, --kitty, or both");
    }
    let mut config = load_config();
    if let Some(sequence) = portable {
        if sequence.is_empty()
            || sequence.len() > 3
            || !sequence.bytes().all(|byte| byte.is_ascii_graphic())
        {
            anyhow::bail!("portable sequence must be 1-3 printable ASCII bytes");
        }
        if let Some(binding) = BINDINGS
            .iter()
            .find(|binding| binding.portable == sequence.as_bytes())
            && binding.action != resolved
        {
            anyhow::bail!(
                "portable sequence {sequence:?} is already bound to {}",
                action_name(binding.action)
            );
        }
        config
            .bindings
            .insert(sequence.to_string(), action.to_string());
    }
    if let Some(combo) = kitty {
        if combo.is_empty() || !combo.bytes().all(|byte| byte.is_ascii_graphic()) {
            anyhow::bail!("Kitty chord must be non-empty printable ASCII without spaces");
        }
        if let Some(binding) = BINDINGS.iter().find(|binding| binding.kitty == combo)
            && binding.action != resolved
        {
            anyhow::bail!(
                "Kitty chord {combo:?} is already bound to {}",
                action_name(binding.action)
            );
        }
        config
            .kitty_bindings
            .insert(combo.to_string(), action.to_string());
    }
    save_config(&config)
}

pub fn remove_custom_binding(
    portable: Option<&str>,
    kitty: Option<&str>,
) -> anyhow::Result<PathBuf> {
    if portable.is_none() && kitty.is_none() {
        anyhow::bail!("provide --portable, --kitty, or both");
    }
    let mut config = load_config();
    if let Some(sequence) = portable {
        config.bindings.remove(sequence);
    }
    if let Some(combo) = kitty {
        config.kitty_bindings.remove(combo);
    }
    save_config(&config)
}

pub fn reset_custom_bindings() -> anyhow::Result<PathBuf> {
    let mut config = load_config();
    config.bindings.clear();
    config.kitty_bindings.clear();
    save_config(&config)
}

pub fn custom_kitty_bindings() -> Vec<(String, Vec<u8>)> {
    load_config()
        .kitty_bindings
        .into_iter()
        .filter_map(|(combo, action)| {
            let action = parse_action_name(&action)?;
            let tag = BINDINGS
                .iter()
                .find(|binding| binding.action == action)?
                .tag
                .to_vec();
            Some((combo, tag))
        })
        .collect()
}

pub fn render_portable_bindings() -> String {
    let mut out = String::from("Portable prefix: Ctrl-Space\n\n");
    for binding in BINDINGS {
        let sequence = String::from_utf8_lossy(binding.portable);
        out.push_str(&format!(
            "Ctrl-Space, {sequence:<3}  {:<18}  {}\n",
            action_name(binding.action),
            binding.description
        ));
    }
    out.push_str("Ctrl-Space, Ctrl-Space  send a literal Ctrl-Space\n");
    out
}

pub fn render_custom_bindings() -> String {
    let config = load_config();
    let mut out = String::new();
    if !config.bindings.is_empty() {
        out.push_str("\nCustom portable aliases:\n");
        for (sequence, action) in config.bindings {
            out.push_str(&format!("Ctrl-Space, {sequence:<3}  {action}\n"));
        }
    }
    if !config.kitty_bindings.is_empty() {
        out.push_str("\nCustom Kitty aliases:\n");
        for (combo, action) in config.kitty_bindings {
            out.push_str(&format!("{combo:<20}  {action}\n"));
        }
    }
    out
}

/// The `shift-enter` chord's full byte sequence -- see module doc for why
/// this is a bare constant rather than a [`Chord`]/[`Action`] variant.
pub const SHIFT_ENTER_CHORD: &[u8] = b"\x1b_Ds\x1b\\";

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub kitty: &'static str,
    pub portable: &'static [u8],
    pub tag: &'static [u8],
    pub action: Action,
    pub description: &'static str,
}

pub const BINDINGS: &[Binding] = &[
    Binding {
        kitty: "cmd+1",
        portable: b"1",
        tag: b"1",
        action: Action::SwitchWorkspace(1),
        description: "switch to workspace 1",
    },
    Binding {
        kitty: "cmd+2",
        portable: b"2",
        tag: b"2",
        action: Action::SwitchWorkspace(2),
        description: "switch to workspace 2",
    },
    Binding {
        kitty: "cmd+3",
        portable: b"3",
        tag: b"3",
        action: Action::SwitchWorkspace(3),
        description: "switch to workspace 3",
    },
    Binding {
        kitty: "cmd+4",
        portable: b"4",
        tag: b"4",
        action: Action::SwitchWorkspace(4),
        description: "switch to workspace 4",
    },
    Binding {
        kitty: "cmd+5",
        portable: b"5",
        tag: b"5",
        action: Action::SwitchWorkspace(5),
        description: "switch to workspace 5",
    },
    Binding {
        kitty: "cmd+6",
        portable: b"6",
        tag: b"6",
        action: Action::SwitchWorkspace(6),
        description: "switch to workspace 6",
    },
    Binding {
        kitty: "cmd+7",
        portable: b"7",
        tag: b"7",
        action: Action::SwitchWorkspace(7),
        description: "switch to workspace 7",
    },
    Binding {
        kitty: "cmd+8",
        portable: b"8",
        tag: b"8",
        action: Action::SwitchWorkspace(8),
        description: "switch to workspace 8",
    },
    Binding {
        kitty: "cmd+9",
        portable: b"9",
        tag: b"9",
        action: Action::SwitchWorkspace(9),
        description: "switch to workspace 9",
    },
    Binding {
        kitty: "cmd+alt+1",
        portable: b"s1",
        tag: b"s1",
        action: Action::JumpSession(1),
        description: "bind session 1",
    },
    Binding {
        kitty: "cmd+alt+2",
        portable: b"s2",
        tag: b"s2",
        action: Action::JumpSession(2),
        description: "bind session 2",
    },
    Binding {
        kitty: "cmd+alt+3",
        portable: b"s3",
        tag: b"s3",
        action: Action::JumpSession(3),
        description: "bind session 3",
    },
    Binding {
        kitty: "cmd+alt+4",
        portable: b"s4",
        tag: b"s4",
        action: Action::JumpSession(4),
        description: "bind session 4",
    },
    Binding {
        kitty: "cmd+alt+5",
        portable: b"s5",
        tag: b"s5",
        action: Action::JumpSession(5),
        description: "bind session 5",
    },
    Binding {
        kitty: "cmd+alt+6",
        portable: b"s6",
        tag: b"s6",
        action: Action::JumpSession(6),
        description: "bind session 6",
    },
    Binding {
        kitty: "cmd+alt+7",
        portable: b"s7",
        tag: b"s7",
        action: Action::JumpSession(7),
        description: "bind session 7",
    },
    Binding {
        kitty: "cmd+alt+8",
        portable: b"s8",
        tag: b"s8",
        action: Action::JumpSession(8),
        description: "bind session 8",
    },
    Binding {
        kitty: "cmd+alt+9",
        portable: b"s9",
        tag: b"s9",
        action: Action::JumpSession(9),
        description: "bind session 9",
    },
    Binding {
        kitty: "cmd+d",
        portable: b"d",
        tag: b"d",
        action: Action::SplitVertical,
        description: "split vertically",
    },
    Binding {
        kitty: "cmd+shift+d",
        portable: b"D",
        tag: b"D",
        action: Action::SplitHorizontal,
        description: "split horizontally",
    },
    Binding {
        kitty: "cmd+w",
        portable: b"w",
        tag: b"w",
        action: Action::CloseFocusedPane,
        description: "close focused tab",
    },
    Binding {
        kitty: "cmd+shift+w",
        portable: b"W",
        tag: b"W",
        action: Action::KillFocusedServerPane,
        description: "kill focused session",
    },
    Binding {
        kitty: "cmd+shift+z",
        portable: b"Z",
        tag: b"Z",
        action: Action::DetachAndAttach,
        description: "detach and choose a session",
    },
    Binding {
        kitty: "cmd+h",
        portable: b"h",
        tag: b"h",
        action: Action::FocusLeft,
        description: "focus left",
    },
    Binding {
        kitty: "cmd+j",
        portable: b"j",
        tag: b"j",
        action: Action::FocusDown,
        description: "focus down",
    },
    Binding {
        kitty: "cmd+k",
        portable: b"k",
        tag: b"k",
        action: Action::FocusUp,
        description: "focus up",
    },
    Binding {
        kitty: "cmd+l",
        portable: b"l",
        tag: b"l",
        action: Action::FocusRight,
        description: "focus right",
    },
    Binding {
        kitty: "cmd+t",
        portable: b"t",
        tag: b"t",
        action: Action::AddTab,
        description: "add a tab",
    },
    Binding {
        kitty: "cmd+]",
        portable: b"]",
        tag: b"]",
        action: Action::CycleTabForward,
        description: "next tab",
    },
    Binding {
        kitty: "cmd+[",
        portable: b"[",
        tag: b"[",
        action: Action::CycleTabBackward,
        description: "previous tab",
    },
    Binding {
        // Deliberately not `cmd+q` -- that's macOS/Kitty's own
        // "quit the application" shortcut; overriding it here would
        // silently repurpose a shortcut most users' muscle memory
        // expects to close the whole terminal, not just dimax.
        kitty: "cmd+shift+q",
        portable: b"q",
        tag: b"q",
        action: Action::Quit,
        description: "quit dimax",
    },
];

fn action_for_tag(tag: &[u8]) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|binding| binding.tag == tag)
        .map(|binding| binding.action)
}

/// Parse one input event's raw bytes into an [`Action`]. Returns
/// `Action::PassThrough` for anything not recognized as a dimax chord, so
/// the caller forwards it to the focused server-pane unchanged.
///
/// # Constraint on the caller
///
/// This function assumes `bytes` is exactly one already-delimited input
/// event — a single complete read, not an arbitrary chunk that might
/// split a multi-byte escape sequence across two calls. It does not
/// buffer or reassemble partial sequences. The event-loop implementer in
/// `mod.rs` must read input in a way that yields one complete escape
/// sequence (or one complete normal keystroke) per call — e.g. reading
/// with a short idle timeout so a fast-arriving multi-byte chord lands in
/// one read, the way Kitty emits `send_text` payloads as a single write.
pub fn parse(bytes: &[u8]) -> Action {
    let Some(rest) = bytes.strip_prefix(PREFIX) else {
        return Action::PassThrough;
    };
    let Some(tag) = rest.strip_suffix(TERMINATOR) else {
        return Action::PassThrough;
    };
    action_for_tag(tag).unwrap_or(Action::PassThrough)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInput {
    Action(Action),
    PassThrough(Vec<u8>),
    Pending,
}

#[derive(Debug, Default)]
pub struct PortableParser {
    pending: Option<Vec<u8>>,
    custom: BTreeMap<Vec<u8>, Action>,
}

impl PortableParser {
    pub fn from_config() -> Self {
        let custom = load_config()
            .bindings
            .into_iter()
            .filter_map(|(sequence, action)| {
                Some((sequence.into_bytes(), parse_action_name(&action)?))
            })
            .collect();
        Self {
            pending: None,
            custom,
        }
    }

    pub fn parse(&mut self, bytes: &[u8], mode: BindingMode) -> ParsedInput {
        let kitty_action = parse(bytes);
        if mode.kitty_enabled() && kitty_action != Action::PassThrough {
            return ParsedInput::Action(kitty_action);
        }
        if !mode.portable_enabled() {
            return ParsedInput::PassThrough(bytes.to_vec());
        }

        if self.pending.is_none() {
            let Some(rest) = bytes.strip_prefix(&[PORTABLE_PREFIX]) else {
                return ParsedInput::PassThrough(bytes.to_vec());
            };
            if rest.is_empty() {
                self.pending = Some(Vec::new());
                return ParsedInput::Pending;
            }
            return self.resolve_after_prefix(rest);
        }

        let mut sequence = self.pending.take().expect("pending checked above");
        if sequence.is_empty() && bytes == [PORTABLE_PREFIX] {
            return ParsedInput::PassThrough(vec![PORTABLE_PREFIX]);
        }
        sequence.extend_from_slice(bytes);
        self.resolve_sequence(sequence, bytes)
    }

    fn resolve_after_prefix(&mut self, bytes: &[u8]) -> ParsedInput {
        self.resolve_sequence(bytes.to_vec(), bytes)
    }

    fn resolve_sequence(&mut self, sequence: Vec<u8>, raw: &[u8]) -> ParsedInput {
        self.action_for_portable(&sequence)
            .map(ParsedInput::Action)
            .unwrap_or_else(|| {
                if self.has_portable_prefix(&sequence) {
                    self.pending = Some(sequence);
                    ParsedInput::Pending
                } else {
                    ParsedInput::PassThrough(raw.to_vec())
                }
            })
    }

    fn action_for_portable(&self, sequence: &[u8]) -> Option<Action> {
        self.custom
            .get(sequence)
            .copied()
            .or_else(|| action_for_portable(sequence))
    }

    fn has_portable_prefix(&self, sequence: &[u8]) -> bool {
        BINDINGS
            .iter()
            .any(|binding| binding.portable.starts_with(sequence))
            || self
                .custom
                .keys()
                .any(|binding| binding.starts_with(sequence))
    }
}

fn action_for_portable(sequence: &[u8]) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|binding| binding.portable == sequence)
        .map(|binding| binding.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `XDG_CONFIG_HOME` (process-global) --
    /// same pattern as `daemon::state`'s `PIN_ENV_LOCK`.
    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_fake_config_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir);
        }
        let result = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        result
    }

    #[test]
    fn consume_first_run_returns_true_once_then_false() {
        let dir = std::env::temp_dir().join(format!("dmx-keys-first-run-{}", std::process::id()));
        with_fake_config_home(&dir, || {
            assert!(consume_first_run(), "first call should grant the wizard");
            assert!(!consume_first_run(), "second call should not");
            assert!(!consume_first_run(), "third call should not");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn chord_bytes(tag: u8) -> Vec<u8> {
        let mut v = PREFIX.to_vec();
        v.push(tag);
        v.extend_from_slice(TERMINATOR);
        v
    }

    #[test]
    fn workspace_switch_digits() {
        for n in 1u8..=9 {
            let tag = b'0' + n;
            let action = parse(&chord_bytes(tag));
            assert_eq!(action, Action::SwitchWorkspace(n), "digit {n}");
        }
    }

    #[test]
    fn split_vertical() {
        assert_eq!(parse(&chord_bytes(b'd')), Action::SplitVertical);
    }

    #[test]
    fn split_horizontal() {
        assert_eq!(parse(&chord_bytes(b'D')), Action::SplitHorizontal);
    }

    #[test]
    fn close_focused_pane() {
        assert_eq!(parse(&chord_bytes(b'w')), Action::CloseFocusedPane);
    }

    #[test]
    fn kill_focused_server_pane() {
        assert_eq!(parse(&chord_bytes(b'W')), Action::KillFocusedServerPane);
    }

    #[test]
    fn detach_and_attach() {
        assert_eq!(parse(&chord_bytes(b'Z')), Action::DetachAndAttach);
    }

    #[test]
    fn focus_left() {
        assert_eq!(parse(&chord_bytes(b'h')), Action::FocusLeft);
    }

    #[test]
    fn focus_down() {
        assert_eq!(parse(&chord_bytes(b'j')), Action::FocusDown);
    }

    #[test]
    fn focus_up() {
        assert_eq!(parse(&chord_bytes(b'k')), Action::FocusUp);
    }

    #[test]
    fn focus_right() {
        assert_eq!(parse(&chord_bytes(b'l')), Action::FocusRight);
    }

    #[test]
    fn add_tab() {
        assert_eq!(parse(&chord_bytes(b't')), Action::AddTab);
    }

    #[test]
    fn cycle_tab_forward() {
        assert_eq!(parse(&chord_bytes(b']')), Action::CycleTabForward);
    }

    #[test]
    fn cycle_tab_backward() {
        assert_eq!(parse(&chord_bytes(b'[')), Action::CycleTabBackward);
    }

    #[test]
    fn quit_kitty_chord() {
        assert_eq!(parse(&chord_bytes(b'q')), Action::Quit);
    }

    #[test]
    fn quit_portable_binding() {
        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"q", BindingMode::Portable),
            ParsedInput::Action(Action::Quit)
        );
    }

    #[test]
    fn kitty_session_jump_uses_a_two_byte_tag() {
        let bytes = [PREFIX, b"s4", TERMINATOR].concat();
        assert_eq!(parse(&bytes), Action::JumpSession(4));
    }

    #[test]
    fn portable_workspace_binding_can_arrive_together_or_split() {
        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"3", BindingMode::Portable),
            ParsedInput::Action(Action::SwitchWorkspace(3))
        );

        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(&[0, b'3'], BindingMode::Portable),
            ParsedInput::Action(Action::SwitchWorkspace(3))
        );
    }

    #[test]
    fn portable_session_binding_can_arrive_across_three_reads() {
        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"s", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"7", BindingMode::Portable),
            ParsedInput::Action(Action::JumpSession(7))
        );
    }

    #[test]
    fn double_portable_prefix_passes_one_prefix_through() {
        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::PassThrough(vec![0])
        );
    }

    #[test]
    fn each_mode_only_enables_its_own_input_layer() {
        let kitty = chord_bytes(b'd');
        let portable = [0, b'd'];
        let mut parser = PortableParser::default();
        assert_eq!(
            parser.parse(&kitty, BindingMode::Portable),
            ParsedInput::PassThrough(kitty)
        );
        assert_eq!(
            parser.parse(&portable, BindingMode::Kitty),
            ParsedInput::PassThrough(portable.to_vec())
        );
    }

    #[test]
    fn custom_portable_alias_can_span_multiple_reads() {
        let mut parser = PortableParser {
            pending: None,
            custom: BTreeMap::from([(b"xx".to_vec(), Action::SplitVertical)]),
        };
        assert_eq!(
            parser.parse(b"\0", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"x", BindingMode::Portable),
            ParsedInput::Pending
        );
        assert_eq!(
            parser.parse(b"x", BindingMode::Portable),
            ParsedInput::Action(Action::SplitVertical)
        );
    }

    #[test]
    fn action_names_round_trip_for_every_bindable_action() {
        for binding in BINDINGS {
            assert_eq!(
                parse_action_name(action_name(binding.action)),
                Some(binding.action)
            );
        }
    }

    #[test]
    fn empty_input_passes_through() {
        assert_eq!(parse(b""), Action::PassThrough);
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(parse(b"hello"), Action::PassThrough);
    }

    #[test]
    fn unrecognized_tag_passes_through() {
        assert_eq!(parse(&chord_bytes(b'z')), Action::PassThrough);
    }

    #[test]
    fn removed_picker_tag_now_passes_through() {
        // `p` was `cmd-p`/`OpenPicker` before the picker was removed;
        // confirms the tag is genuinely gone from the grammar, not just
        // unreachable from `Action`.
        assert_eq!(parse(&chord_bytes(b'p')), Action::PassThrough);
    }

    #[test]
    fn bare_escape_passes_through() {
        assert_eq!(parse(b"\x1b"), Action::PassThrough);
    }

    #[test]
    fn truncated_chord_passes_through() {
        // Missing terminator.
        assert_eq!(parse(b"\x1b_D1"), Action::PassThrough);
    }
}
