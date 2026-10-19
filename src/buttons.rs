//! Button assignments for a Logitech mouse running from software (Host mode).
//!
//! A mouse on the Host profile stops sending its side buttons as ordinary
//! clicks; it reports them as events instead, and something on the PC has to
//! act on them (G HUB does). While a game's profile has assignments, Bridge
//! runs `native-hid/src/button-spy.mjs`, which puts the mouse on Host, turns
//! those events on and prints them, and then presses the assigned key, or the
//! side button's normal Back/Forward click when nothing is assigned. When the
//! game ends the helper stops the events and puts the mouse back in the mode
//! it was in.
//!
//! The assignments live in a game profile's `snapshot.hostButtons`:
//! `{"8": {"key": 58, "modifiers": 0}, "16": {...}}`, keyed by the button's bit
//! in the mouse's report (8 the back side button, 16 the front one), each a HID
//! keyboard usage and a modifier mask (1 ctrl, 2 shift, 4 alt, 8 win).

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin},
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::ApplicationProfile;

/// The back side button's bit in the mouse's button report.
pub const BACK: u8 = 0x08;
/// The front side button's bit.
pub const FRONT: u8 = 0x10;
/// The only buttons a Host mode mouse stops sending as normal clicks.
const SIDE_BUTTONS: [u8; 2] = [BACK, FRONT];

/// How long the helper gets to stop the events and restore the mouse's mode.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting helper may hold the mouse before Bridge stops waiting.
const START_TIMEOUT: Duration = Duration::from_secs(12);
/// How long to leave a mouse alone after the helper could not start or died.
const RETRY_DELAY: Duration = Duration::from_secs(15);

/// A keyboard key and its modifiers, as a profile stores them (HID usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyAction {
    pub usage: u8,
    pub modifiers: u8,
}

/// One thing to do to the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    KeyDown(KeyAction),
    KeyUp(KeyAction),
    /// Press the side button's normal click: `BACK` is Back, `FRONT` is Forward.
    SideDown(u8),
    SideUp(u8),
}

/// What a game's profile wants the mouse's side buttons to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignments {
    /// The brand the profile names ("Logitech"), which the helper picks its driver by.
    pub brand: String,
    pub keys: BTreeMap<u8, KeyAction>,
}

impl Assignments {
    /// The assignments a profile carries, or `None` when it has none or is not
    /// for a Logitech mouse (the only brand with the button events).
    pub fn from_profile(profile: &ApplicationProfile) -> Option<Self> {
        let brand = profile.device.id.split(':').next().unwrap_or_default();
        if !brand.eq_ignore_ascii_case("logitech") {
            return None;
        }
        let buttons = profile.settings.snapshot.as_ref()?.get("hostButtons")?.as_object()?;
        let mut keys = BTreeMap::new();
        for (bit, action) in buttons {
            let bit: u8 = bit.parse().ok().filter(|bit| SIDE_BUTTONS.contains(bit))?;
            let usage = u8::try_from(action.get("key")?.as_u64()?).ok()?;
            let modifiers = u8::try_from(action.get("modifiers").and_then(Value::as_u64).unwrap_or(0))
                .ok()
                .filter(|modifiers| *modifiers <= 0x0f)?;
            keys.insert(bit, KeyAction { usage, modifiers });
        }
        (!keys.is_empty()).then(|| Self {
            brand: brand.to_owned(),
            keys,
        })
    }
}

/// What a change of the held buttons asks for: a press or release of a side
/// button becomes its assigned key, or its normal click when none is assigned.
pub fn steps(assignments: &Assignments, previous: u8, now: u8) -> Vec<Step> {
    let mut steps = Vec::new();
    for bit in SIDE_BUTTONS {
        let (was, is) = (previous & bit != 0, now & bit != 0);
        if was == is {
            continue;
        }
        steps.push(match (assignments.keys.get(&bit), is) {
            (Some(action), true) => Step::KeyDown(*action),
            (Some(action), false) => Step::KeyUp(*action),
            (None, true) => Step::SideDown(bit),
            (None, false) => Step::SideUp(bit),
        });
    }
    steps
}

/// The Windows virtual-key code for a HID keyboard usage, for the keys a
/// profile can hold.
pub fn virtual_key(usage: u8) -> Option<u16> {
    Some(match usage {
        0x04..=0x1d => 0x41 + u16::from(usage - 0x04), // A-Z
        0x1e..=0x26 => 0x31 + u16::from(usage - 0x1e), // 1-9
        0x27 => 0x30,                                  // 0
        0x28 => 0x0d,                                  // Enter
        0x29 => 0x1b,                                  // Escape
        0x2a => 0x08,                                  // Backspace
        0x2b => 0x09,                                  // Tab
        0x2c => 0x20,                                  // Space
        0x2d => 0xbd,                                  // -
        0x2e => 0xbb,                                  // =
        0x2f => 0xdb,                                  // [
        0x30 => 0xdd,                                  // ]
        0x31 => 0xdc,                                  // \
        0x33 => 0xba,                                  // ;
        0x34 => 0xde,                                  // '
        0x35 => 0xc0,                                  // `
        0x36 => 0xbc,                                  // ,
        0x37 => 0xbe,                                  // .
        0x38 => 0xbf,                                  // /
        0x3a..=0x45 => 0x70 + u16::from(usage - 0x3a), // F1-F12
        0x46 => 0x2c,                                  // Print Screen
        0x47 => 0x91,                                  // Scroll Lock
        0x48 => 0x13,                                  // Pause
        0x49 => 0x2d,                                  // Insert
        0x4a => 0x24,                                  // Home
        0x4b => 0x21,                                  // Page Up
        0x4c => 0x2e,                                  // Delete
        0x4d => 0x23,                                  // End
        0x4e => 0x22,                                  // Page Down
        0x4f => 0x27,                                  // Right
        0x50 => 0x25,                                  // Left
        0x51 => 0x28,                                  // Down
        0x52 => 0x26,                                  // Up
        0x68..=0x73 => 0x7c + u16::from(usage - 0x68), // F13-F24
        _ => return None,
    })
}

/// The modifier keys a mask names, in the order they are pressed.
pub fn modifier_keys(modifiers: u8) -> Vec<u16> {
    [(0x01, 0x11), (0x02, 0x10), (0x04, 0x12), (0x08, 0x5b)] // ctrl, shift, alt, win
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, vk)| vk)
        .collect()
}

/// Runs the helper while a profile has assignments, and stops it afterwards.
#[derive(Default)]
pub struct ButtonListener {
    running: Mutex<Option<Running>>,
    /// Not before this moment is a helper that failed started again.
    retry_after: Mutex<Option<Instant>>,
}

struct Running {
    assignments: Assignments,
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl Running {
    /// Asks the helper to stop and waits for it to restore the mouse.
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }

    fn matches(&self, desired: Option<&Assignments>) -> bool {
        Some(&self.assignments) == desired && !self.thread.is_finished()
    }
}

impl ButtonListener {
    /// Makes the helper match `desired`: started for new assignments, stopped
    /// when there are none. `may_start` is false while a control panel holds
    /// the mouse, so a new helper does not open it underneath. Stopping and
    /// starting wait on the helper, so they run off the caller's thread.
    pub fn update(self: &Arc<Self>, desired: Option<Assignments>, may_start: bool) {
        if self.in_step_with(desired.as_ref()) || (desired.is_some() && !may_start) {
            return;
        }
        let waiting = *self.retry_after.lock().unwrap_or_else(PoisonError::into_inner);
        if desired.is_some() && waiting.is_some_and(|until| Instant::now() < until) {
            return;
        }
        let listener = Arc::clone(self);
        thread::spawn(move || listener.apply(desired));
    }

    fn in_step_with(&self, desired: Option<&Assignments>) -> bool {
        let running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.as_ref().map_or(desired.is_none(), |current| current.matches(desired))
    }

    fn apply(&self, desired: Option<Assignments>) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.as_ref().map_or(desired.is_none(), |current| current.matches(desired.as_ref())) {
            return;
        }
        let died = running.as_ref().is_some_and(|current| current.thread.is_finished());
        if let Some(old) = running.take() {
            old.stop();
        }
        let mut retry_after = self.retry_after.lock().unwrap_or_else(PoisonError::into_inner);
        *retry_after = died.then(|| Instant::now() + RETRY_DELAY);
        if let Some(assignments) = desired {
            match start(assignments) {
                Ok(started) => *running = Some(started),
                Err(error) => {
                    tracing::warn!(%error, "Could not start the button listener");
                    *retry_after = Some(Instant::now() + RETRY_DELAY);
                }
            }
        }
    }
}

impl Drop for ButtonListener {
    fn drop(&mut self) {
        let running = self.running.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = running.take() {
            old.stop();
        }
    }
}

enum Event {
    Buttons(u8),
    Ready,
    Failed(String),
    Closed,
}

fn start(assignments: Assignments) -> Result<Running> {
    // The helper talks to the mouse while it starts, and so does a profile
    // apply, so they take turns: this is held until the helper is listening.
    let access = crate::drivers::device_access();
    let mut child = crate::drivers::spawn_button_spy(&assignments.brand)?;
    let stdin = child.stdin.take().context("the button helper's stdin was unavailable")?;
    let stdout = child.stdout.take().context("the button helper's stdout was unavailable")?;
    let (stop, stopped) = mpsc::channel();
    let (ready, started) = mpsc::channel();
    let keys = assignments.clone();
    let thread = thread::Builder::new()
        .name("openmouse-bridge-buttons".into())
        .spawn(move || run(child, stdin, stdout, &keys, &stopped, &ready))
        .context("could not start the button listener thread")?;
    let _ = started.recv_timeout(START_TIMEOUT);
    drop(access);
    Ok(Running {
        assignments,
        stop,
        thread,
    })
}

fn run(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: std::process::ChildStdout,
    assignments: &Assignments,
    stopped: &mpsc::Receiver<()>,
    ready: &mpsc::Sender<()>,
) {
    let request = format!("{{\"brand\":{}}}\n", Value::from(assignments.brand.as_str()));
    if stdin.write_all(request.as_bytes()).and_then(|()| stdin.flush()).is_err() {
        tracing::warn!("Could not start the button helper");
    }

    let (events, incoming) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let message: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let event = match message.get("event").and_then(Value::as_str) {
                Some("buttons") => message
                    .get("mask")
                    .and_then(Value::as_u64)
                    .and_then(|mask| u8::try_from(mask).ok())
                    .map(Event::Buttons),
                Some("ready") => Some(Event::Ready),
                Some("error") => Some(Event::Failed(
                    message.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_owned(),
                )),
                _ => None,
            };
            if let Some(event) = event
                && events.send(event).is_err()
            {
                return;
            }
        }
        let _ = events.send(Event::Closed);
    });

    let mut held = 0u8;
    loop {
        if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            break;
        }
        match incoming.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Buttons(mask)) => {
                for step in steps(assignments, held, mask) {
                    execute(step);
                }
                held = mask;
            }
            Ok(Event::Ready) => {
                tracing::info!("The mouse's buttons are being listened to");
                let _ = ready.send(());
            }
            Ok(Event::Failed(message)) => {
                tracing::warn!(%message, "The button helper reported a problem");
                let _ = ready.send(());
            }
            Ok(Event::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = ready.send(());
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    // Let go of anything still held, then close stdin: the helper stops the
    // events and puts the mouse back in the mode it was in.
    for step in steps(assignments, held, 0) {
        execute(step);
    }
    drop(stdin);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    tracing::warn!("The button helper did not stop in time; ending it");
    let _ = child.kill();
}

#[cfg(target_os = "windows")]
fn execute(step: Step) {
    inject::execute(step);
}

#[cfg(not(target_os = "windows"))]
fn execute(step: Step) {
    tracing::debug!(?step, "Button actions are only carried out on Windows for now");
}

#[cfg(target_os = "windows")]
mod inject {
    use std::mem::size_of;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW,
        SendInput,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{XBUTTON1, XBUTTON2};

    use super::{BACK, KeyAction, Step, modifier_keys, virtual_key};

    pub fn execute(step: Step) {
        match step {
            Step::KeyDown(action) => keys(action, true),
            Step::KeyUp(action) => keys(action, false),
            Step::SideDown(bit) => side(bit, true),
            Step::SideUp(bit) => side(bit, false),
        }
    }

    /// Modifiers go down before the key and come up after it.
    fn keys(action: KeyAction, down: bool) {
        let Some(key) = virtual_key(action.usage) else {
            tracing::warn!(usage = action.usage, "That key cannot be pressed by Bridge");
            return;
        };
        let mut order = modifier_keys(action.modifiers);
        order.push(key);
        if !down {
            order.reverse();
        }
        for vk in order {
            send(&[key_input(vk, down)]);
        }
    }

    fn key_input(vk: u16, down: bool) -> INPUT {
        // The scan code lets games that read it see the key; the high byte
        // 0xE0 marks an extended key (arrows, Insert, Home, ...).
        let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
        let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
        if scan != 0 {
            flags |= KEYEVENTF_SCANCODE;
        }
        if scan >> 8 == 0xe0 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: if scan != 0 { 0 } else { vk },
                    wScan: (scan & 0xff) as u16,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn side(bit: u8, down: bool) {
        let button = if bit == BACK { XBUTTON1 } else { XBUTTON2 };
        send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: u32::from(button),
                    dwFlags: if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    fn send(inputs: &[INPUT]) {
        let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            tracing::warn!("Windows did not accept the button input");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::{ProfileApplication, ProfileDevice, ProfileSettings};

    fn profile(device: &str, snapshot: Value) -> ApplicationProfile {
        ApplicationProfile {
            application: ProfileApplication {
                name: "Game".into(),
                executable: String::new(),
                path: String::new(),
            },
            device: ProfileDevice {
                id: device.into(),
                name: "Mouse".into(),
            },
            settings: ProfileSettings {
                dpi: None,
                polling_rate_hz: None,
                snapshot: Some(snapshot),
            },
            enabled: true,
        }
    }

    fn assignments(keys: &[(u8, u8, u8)]) -> Assignments {
        Assignments {
            brand: "Logitech".into(),
            keys: keys
                .iter()
                .map(|&(bit, usage, modifiers)| (bit, KeyAction { usage, modifiers }))
                .collect(),
        }
    }

    #[test]
    fn a_profile_names_its_assignments_by_button_bit() {
        let parsed = Assignments::from_profile(&profile(
            "Logitech:PRO X3 SUPERSTRIKE",
            json!({"hostButtons": {"16": {"key": 104, "modifiers": 0}, "8": {"key": 4, "modifiers": 3}}}),
        ))
        .expect("assignments");
        assert_eq!(parsed.brand, "Logitech");
        assert_eq!(parsed.keys[&FRONT], KeyAction { usage: 104, modifiers: 0 });
        assert_eq!(parsed.keys[&BACK], KeyAction { usage: 4, modifiers: 3 });
    }

    #[test]
    fn profiles_without_usable_assignments_start_nothing() {
        let logitech = |snapshot| Assignments::from_profile(&profile("Logitech:Mouse", snapshot));
        assert_eq!(logitech(json!({})), None);
        assert_eq!(logitech(json!({"hostButtons": {}})), None);
        assert_eq!(logitech(json!({"hostButtons": {"4": {"key": 4}}})), None, "the middle button is not assignable");
        assert_eq!(logitech(json!({"hostButtons": {"8": {"key": 999}}})), None);
        assert_eq!(logitech(json!({"hostButtons": {"8": {"key": 4, "modifiers": 99}}})), None);
        let other = profile("Razer:Viper", json!({"hostButtons": {"8": {"key": 4}}}));
        assert_eq!(Assignments::from_profile(&other), None, "only Logitech has the button events");
    }

    #[test]
    fn a_press_and_release_become_one_key_down_and_up() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        let action = KeyAction { usage: 0x68, modifiers: 0 };
        assert_eq!(steps(&assigned, 0, FRONT), [Step::KeyDown(action)]);
        assert_eq!(steps(&assigned, FRONT, FRONT), [], "holding changes nothing");
        assert_eq!(steps(&assigned, FRONT, 0), [Step::KeyUp(action)]);
    }

    #[test]
    fn an_unassigned_side_button_keeps_its_normal_click() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        assert_eq!(steps(&assigned, 0, BACK), [Step::SideDown(BACK)]);
        assert_eq!(steps(&assigned, BACK, 0), [Step::SideUp(BACK)]);
    }

    #[test]
    fn the_other_buttons_are_left_to_the_mouse() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        // left, right and middle already click on their own
        assert_eq!(steps(&assigned, 0, 0x01 | 0x02 | 0x04), []);
    }

    #[test]
    fn two_buttons_pressed_together_are_both_handled() {
        let assigned = assignments(&[(FRONT, 0x68, 0)]);
        assert_eq!(
            steps(&assigned, 0, BACK | FRONT),
            [Step::SideDown(BACK), Step::KeyDown(KeyAction { usage: 0x68, modifiers: 0 })]
        );
    }

    #[test]
    fn keys_map_to_windows_virtual_keys() {
        assert_eq!(virtual_key(0x04), Some(0x41), "A");
        assert_eq!(virtual_key(0x1d), Some(0x5a), "Z");
        assert_eq!(virtual_key(0x1e), Some(0x31), "1");
        assert_eq!(virtual_key(0x27), Some(0x30), "0");
        assert_eq!(virtual_key(0x3a), Some(0x70), "F1");
        assert_eq!(virtual_key(0x45), Some(0x7b), "F12");
        assert_eq!(virtual_key(0x68), Some(0x7c), "F13");
        assert_eq!(virtual_key(0x73), Some(0x87), "F24");
        assert_eq!(virtual_key(0x2c), Some(0x20), "Space");
        assert_eq!(virtual_key(0x52), Some(0x26), "Up");
        assert_eq!(virtual_key(0x01), None);
        assert_eq!(modifier_keys(0b0101), [0x11, 0x12], "ctrl and alt");
        assert_eq!(modifier_keys(0), Vec::<u16>::new());
    }
}
