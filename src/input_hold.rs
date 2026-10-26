//! Exclusive hold on the user's physical input devices (`EVIOCGRAB`).
//!
//! uinput input goes to whichever window has focus (keyboard) or lies under the
//! pointer (pointer) at delivery time, and compositors offer no focus lock. A
//! user switching focus or moving the mouse between our pre-check and delivery
//! would redirect the input. While an [`InputHold`] lives, every physical
//! keyboard and pointer is grabbed, so the compositor sees no user input and the
//! check -> inject -> post-check sequence cannot be interleaved with it.
//!
//! The user's events during the hold are drained into a buffer and replayed
//! after the grab is released (ungrab first), so nothing is lost and no key
//! sticks. Replay goes through a dedicated uinput device
//! ([`REPLAY_DEVICE_NAME`]).
//!
//! Safety:
//! - A watchdog inside the drain thread ungrabs after [`MAX_HOLD`] even if the
//!   holder is stuck.
//! - `Drop` ungrabs and replays, so a panic in the holder releases the grab.
//! - The kernel releases a grab when the fd closes, so process exit (including
//!   SIGKILL) always releases it.
//!
//! Not replayed (dropped, never fabricated): absolute axes (touchpad
//! multitouch/gesture motion, tablet), `BTN_TOUCH`/`BTN_TOOL_*` contact state,
//! LED/switch/misc events, and keys outside keyboard keys and mouse buttons.
//! Autorepeat (value 2) is never replayed; the compositor generates repeats.
use anyhow::{bail, Context, Result};
use evdev::{
    uinput::VirtualDevice, AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, sleep, JoinHandle},
    time::{Duration, Instant},
};

/// Hard cap on one hold; the drain thread ungrabs on its own after this.
pub(crate) const MAX_HOLD: Duration = Duration::from_secs(3);
/// Upper bound on buffered user events; past it only key/button events are kept.
const MAX_BUFFERED_EVENTS: usize = 50_000;
const POLL_INTERVAL_MS: i32 = 10;
/// How long to wait for the user to let go of every key and button before
/// grabbing; a key held across the grab boundary would stick (see `acquire`).
const KEYS_UP_WAIT: Duration = Duration::from_secs(1);

/// Name of the uinput device user events are replayed through.
pub(crate) const REPLAY_DEVICE_NAME: &str = "niri-computer-use-input-replay";
/// Names of every uinput device this repo creates (`keyboard.rs`,
/// `abs_pointer.rs`, replay). They must never be grabbed.
const OWN_DEVICE_NAMES: [&str; 3] = [
    "niri-computer-use-keyboard",
    "computer-use-linux absolute pointer",
    REPLAY_DEVICE_NAME,
];

const EV_KEY: u16 = EventType::KEY.0;
const EV_REL: u16 = EventType::RELATIVE.0;
const BTN_LEFT: u16 = KeyCode::BTN_LEFT.0;
const BUS_VIRTUAL: u16 = 0x06;
const REL_X: u16 = RelativeAxisCode::REL_X.0;
const REL_Y: u16 = RelativeAxisCode::REL_Y.0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// Linux key codes of the 26 letter keys (KEY_Q..KEY_P, KEY_A..KEY_L, KEY_Z..KEY_M).
const LETTER_KEYS: [u16; 26] = [
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 30, 31, 32, 33, 34, 35, 36, 37, 38, 44, 45, 46, 47, 48,
    49, 50,
];
const REPLAYED_REL_AXES: [RelativeAxisCode; 6] = [
    RelativeAxisCode::REL_X,
    RelativeAxisCode::REL_Y,
    RelativeAxisCode::REL_HWHEEL,
    RelativeAxisCode::REL_WHEEL,
    RelativeAxisCode::REL_WHEEL_HI_RES,
    RelativeAxisCode::REL_HWHEEL_HI_RES,
];

// ---- pure logic -----------------------------------------------------------

/// What classification needs to know about an input device.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeviceCaps {
    pub name: String,
    /// Backed by a virtual (uinput-style) device rather than hardware.
    pub virtual_device: bool,
    pub keys: BTreeSet<u16>,
    pub rel: BTreeSet<u16>,
    pub abs: BTreeSet<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceRole {
    Keyboard,
    Pointer,
    KeyboardPointer,
}

pub(crate) fn is_own_device(name: &str) -> bool {
    OWN_DEVICE_NAMES.contains(&name)
}

/// Decide whether a device is a physical keyboard and/or pointer to grab.
/// Returns `None` for our own devices, other virtual devices, and anything else.
pub(crate) fn classify(caps: &DeviceCaps) -> Option<DeviceRole> {
    if caps.virtual_device || is_own_device(&caps.name) {
        return None;
    }
    let keyboard = LETTER_KEYS.iter().all(|key| caps.keys.contains(key));
    let motion = (caps.rel.contains(&REL_X) && caps.rel.contains(&REL_Y))
        || (caps.abs.contains(&ABS_X) && caps.abs.contains(&ABS_Y));
    let pointer = caps.keys.contains(&BTN_LEFT) && motion;
    match (keyboard, pointer) {
        (true, true) => Some(DeviceRole::KeyboardPointer),
        (true, false) => Some(DeviceRole::Keyboard),
        (false, true) => Some(DeviceRole::Pointer),
        (false, false) => None,
    }
}

/// A buffered user event, independent of the evdev types so planning is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayAction {
    Key {
        code: u16,
        value: i32,
    },
    /// Accumulated relative motion/scroll as `(axis code, summed delta)`.
    Rel(Vec<(u16, i32)>),
}

/// Keyboard keys and mouse buttons the replay device can carry.
fn replayable_key(code: u16) -> bool {
    (1..=0xff).contains(&code) || (0x110..=0x117).contains(&code)
}

fn replayable_rel(code: u16) -> bool {
    REPLAYED_REL_AXES.iter().any(|axis| axis.0 == code)
}

/// Turn buffered events into replay actions. Key events keep their order;
/// autorepeat (value 2) and unreplayable keys are dropped; consecutive relative
/// events are summed into one action so motion stays ordered relative to button
/// events.
///
/// libinput tracks pressed keys per device, and replay runs on its own device,
/// so a press and its release must both be replayed there or the key sticks: a
/// release whose press is not in the buffer is dropped (that press reached the
/// compositor from the physical device), and every key still down at the end is
/// released (its physical release will arrive on the physical device, which
/// libinput ignores because it never saw that press).
pub(crate) fn plan_replay(events: &[RawEvent]) -> Vec<ReplayAction> {
    let mut actions = Vec::new();
    let mut down: Vec<u16> = Vec::new();
    let mut pending: BTreeMap<u16, i32> = BTreeMap::new();
    let flush = |pending: &mut BTreeMap<u16, i32>, actions: &mut Vec<ReplayAction>| {
        let deltas: Vec<(u16, i32)> = std::mem::take(pending)
            .into_iter()
            .filter(|(_, delta)| *delta != 0)
            .collect();
        if !deltas.is_empty() {
            actions.push(ReplayAction::Rel(deltas));
        }
    };
    for event in events {
        match event.kind {
            EV_KEY if replayable_key(event.code) && matches!(event.value, 0 | 1) => {
                let was_down = down.contains(&event.code);
                if (event.value == 1) == was_down {
                    continue;
                }
                if event.value == 1 {
                    down.push(event.code);
                } else {
                    down.retain(|code| *code != event.code);
                }
                flush(&mut pending, &mut actions);
                actions.push(ReplayAction::Key {
                    code: event.code,
                    value: event.value,
                });
            }
            EV_REL if replayable_rel(event.code) => {
                let sum = pending.entry(event.code).or_insert(0);
                *sum = sum.saturating_add(event.value);
            }
            _ => {}
        }
    }
    flush(&mut pending, &mut actions);
    for code in down {
        actions.push(ReplayAction::Key { code, value: 0 });
    }
    actions
}

/// Keys and buttons the kernel reports down that have no press among the events
/// buffered since the grab: they were pressed before it, so their release would
/// be lost to the compositor and the key would stick.
pub(crate) fn pressed_before_grab(down_now: &[u16], buffered: &[RawEvent]) -> Vec<u16> {
    down_now
        .iter()
        .copied()
        .filter(|code| {
            !buffered
                .iter()
                .any(|event| event.kind == EV_KEY && event.code == *code && event.value == 1)
        })
        .collect()
}

// ---- replay device --------------------------------------------------------

static REPLAY_DEVICE: Mutex<Option<VirtualDevice>> = Mutex::new(None);

/// Create the replay device if needed. Done before any grab so its udev settle
/// delay is not charged to the watchdog.
fn ensure_replay_device() -> Result<()> {
    let mut slot = REPLAY_DEVICE
        .lock()
        .map_err(|_| anyhow::anyhow!("replay device lock poisoned"))?;
    if slot.is_some() {
        return Ok(());
    }
    let keys: AttributeSet<KeyCode> = (1..=0xffu16)
        .chain(0x110..=0x117)
        .map(KeyCode::new)
        .collect();
    let rel: AttributeSet<RelativeAxisCode> = REPLAYED_REL_AXES.into_iter().collect();
    let device = VirtualDevice::builder()
        .context("uinput builder (is /dev/uinput writable?)")?
        .name(REPLAY_DEVICE_NAME)
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()
        .context("failed to create the input replay device")?;
    sleep(Duration::from_millis(250));
    *slot = Some(device);
    Ok(())
}

fn replay(events: &[RawEvent]) {
    let actions = plan_replay(events);
    if actions.is_empty() {
        return;
    }
    let Ok(mut slot) = REPLAY_DEVICE.lock() else {
        return;
    };
    let Some(device) = slot.as_mut() else {
        return;
    };
    for action in actions {
        let batch: Vec<InputEvent> = match action {
            ReplayAction::Key { code, value } => {
                vec![InputEvent::new_now(EV_KEY, code, value)]
            }
            ReplayAction::Rel(deltas) => deltas
                .into_iter()
                .map(|(code, value)| InputEvent::new_now(EV_REL, code, value))
                .collect(),
        };
        if let Err(error) = device.emit(&batch) {
            eprintln!("computer-use-linux: input replay failed: {error}");
            return;
        }
    }
}

// ---- the hold -------------------------------------------------------------

struct Drained {
    events: Vec<RawEvent>,
}

/// RAII exclusive hold on the physical keyboards and pointers. Dropping it
/// ungrabs every device first, then replays the user's buffered input.
pub(crate) struct InputHold {
    stop: Arc<AtomicBool>,
    expired: Arc<AtomicBool>,
    thread: Option<JoinHandle<Drained>>,
    device_count: usize,
}

impl InputHold {
    /// Grab every physical keyboard and pointer. Any grab failure (for example
    /// `EBUSY` because another process already grabbed a device) is an error;
    /// the hold is never partial. With no physical devices at all it succeeds
    /// holding nothing.
    pub(crate) fn acquire() -> Result<Self> {
        let mut targets = Vec::new();
        for path in event_device_paths()? {
            let device = match Device::open(&path) {
                Ok(device) => device,
                Err(error) if matches!(error.kind(), ErrorKind::NotFound) => continue,
                Err(error) if error.raw_os_error() == Some(libc::ENODEV) => continue,
                Err(error) => {
                    bail!("cannot open {} to classify it: {error}", path.display())
                }
            };
            let caps = device_caps(&path, &device);
            if classify(&caps).is_some() {
                targets.push((path, caps.name, device));
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let expired = Arc::new(AtomicBool::new(false));
        if targets.is_empty() {
            return Ok(Self {
                stop,
                expired,
                thread: None,
                device_count: 0,
            });
        }
        ensure_replay_device()?;
        for (path, _, device) in &mut targets {
            device
                .set_nonblocking(true)
                .with_context(|| format!("set {} nonblocking", path.display()))?;
        }
        // A key held across the grab boundary would stick, so grab only with
        // every key and button up, and verify that after the grab.
        let wait_deadline = Instant::now() + KEYS_UP_WAIT;
        let (grabbed, initial) = loop {
            let any_down = targets
                .iter()
                .any(|(_, _, device)| !keys_down(device).is_empty());
            if !any_down {
                if let Some(result) = grab_all(&mut targets)? {
                    break result;
                }
            }
            if Instant::now() >= wait_deadline {
                bail!(
                    "a key or mouse button stayed pressed for {} ms; not grabbing input while the user holds one",
                    KEYS_UP_WAIT.as_millis()
                );
            }
            sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        };
        let device_count = grabbed.len();
        let deadline = Instant::now() + MAX_HOLD;
        let thread = {
            let (stop, expired) = (Arc::clone(&stop), Arc::clone(&expired));
            thread::Builder::new()
                .name("input-hold".into())
                .spawn(move || drain_until(grabbed, initial, &stop, &expired, deadline))
                .context("spawn input hold thread")?
        };
        Ok(Self {
            stop,
            expired,
            thread: Some(thread),
            device_count,
        })
    }

    /// Physical devices currently grabbed (0 when none existed).
    pub(crate) fn device_count(&self) -> usize {
        self.device_count
    }

    /// True when the watchdog released the grab before the holder finished.
    pub(crate) fn expired(&self) -> bool {
        self.expired.load(Ordering::SeqCst)
    }
}

impl Drop for InputHold {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // The thread ungrabs every device before returning its buffer.
            if let Ok(drained) = thread.join() {
                replay(&drained.events);
            }
        }
    }
}

/// Drain the grabbed devices until told to stop or the deadline passes, then
/// ungrab all of them and read what is still queued.
fn drain_until(
    mut devices: Vec<Device>,
    mut events: Vec<RawEvent>,
    stop: &AtomicBool,
    expired: &AtomicBool,
    deadline: Instant,
) -> Drained {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if Instant::now() >= deadline {
            expired.store(true, Ordering::SeqCst);
            break;
        }
        let mut fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|device| libc::pollfd {
                fd: device.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd entries.
        unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                POLL_INTERVAL_MS,
            )
        };
        for device in &mut devices {
            drain(device, &mut events);
        }
    }
    for device in &mut devices {
        let _ = device.ungrab();
    }
    for device in &mut devices {
        drain(device, &mut events);
    }
    Drained { events }
}

/// Keys and buttons the kernel currently reports down on `device`.
fn keys_down(device: &Device) -> Vec<u16> {
    device
        .get_key_state()
        .map(|state| state.iter().map(|key| key.0).collect())
        .unwrap_or_default()
}

/// Grab every target. Returns the grabbed devices with the events buffered since
/// the grab, or `None` (everything ungrabbed again) when a key turned out to be
/// pressed just before the grab, so the caller retries. A grab failure is an error.
fn grab_all(
    targets: &mut Vec<(PathBuf, String, Device)>,
) -> Result<Option<(Vec<Device>, Vec<RawEvent>)>> {
    // Discard what queued since open (already delivered to the compositor),
    // then grab back to back so those events are not replayed twice.
    for index in 0..targets.len() {
        let (path, name, device) = &mut targets[index];
        let mut discard = Vec::new();
        drain(device, &mut discard);
        if let Err(error) = device.grab() {
            let message = format!(
                "cannot grab {} ({name}): {error} (another process may already hold it)",
                path.display()
            );
            for (_, _, done) in targets.iter_mut().take(index) {
                let _ = done.ungrab();
            }
            bail!(message);
        }
    }
    let mut buffered = Vec::new();
    for (_, _, device) in targets.iter_mut() {
        drain(device, &mut buffered);
    }
    let down: Vec<u16> = targets
        .iter()
        .flat_map(|(_, _, device)| keys_down(device))
        .collect();
    if !pressed_before_grab(&down, &buffered).is_empty() {
        // Ungrab so the pending release reaches the compositor; drop what was
        // buffered (presses made after the grab were never delivered, so their
        // physical releases are ignored and nothing sticks).
        for (_, _, device) in targets.iter_mut() {
            let _ = device.ungrab();
        }
        return Ok(None);
    }
    let devices = std::mem::take(targets)
        .into_iter()
        .map(|(_, _, device)| device)
        .collect();
    Ok(Some((devices, buffered)))
}

/// Read everything queued on a nonblocking device into `out` (keys and
/// relative motion only; everything else is never replayed).
fn drain(device: &mut Device, out: &mut Vec<RawEvent>) {
    // Err is WouldBlock (queue empty) or the device went away.
    while let Ok(batch) = device.fetch_events() {
        for event in batch {
            let kind = event.event_type().0;
            let keep = match kind {
                EV_KEY => true,
                EV_REL => out.len() < MAX_BUFFERED_EVENTS,
                _ => false,
            };
            if keep {
                out.push(RawEvent {
                    kind,
                    code: event.code(),
                    value: event.value(),
                });
            }
        }
    }
}

fn event_device_paths() -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir("/dev/input")
        .context("read /dev/input")?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect();
    paths.sort();
    Ok(paths)
}

fn device_caps(path: &Path, device: &Device) -> DeviceCaps {
    let name = device.name().unwrap_or_default().to_string();
    let virtual_device = device.input_id().bus_type().0 == BUS_VIRTUAL || sysfs_is_virtual(path);
    DeviceCaps {
        name,
        virtual_device,
        keys: device
            .supported_keys()
            .map(|set| set.iter().map(|key| key.0).collect())
            .unwrap_or_default(),
        rel: device
            .supported_relative_axes()
            .map(|set| set.iter().map(|axis| axis.0).collect())
            .unwrap_or_default(),
        abs: device
            .supported_absolute_axes()
            .map(|set| set.iter().map(|axis| axis.0).collect())
            .unwrap_or_default(),
    }
}

/// uinput devices live under `/sys/devices/virtual/input/`, hardware does not.
fn sysfs_is_virtual(event_path: &Path) -> bool {
    let Some(name) = event_path.file_name() else {
        return false;
    };
    fs::canonicalize(Path::new("/sys/class/input").join(name))
        .map(|resolved| path_is_virtual(&resolved))
        .unwrap_or(false)
}

fn path_is_virtual(resolved: &Path) -> bool {
    resolved.starts_with("/sys/devices/virtual/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(name: &str, keys: &[u16], rel: &[u16], abs: &[u16]) -> DeviceCaps {
        DeviceCaps {
            name: name.into(),
            virtual_device: false,
            keys: keys.iter().copied().collect(),
            rel: rel.iter().copied().collect(),
            abs: abs.iter().copied().collect(),
        }
    }

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn rel(code: u16, value: i32) -> RawEvent {
        RawEvent {
            kind: EV_REL,
            code,
            value,
        }
    }

    #[test]
    fn keyboard_needs_every_letter_key() {
        let mut keys: Vec<u16> = LETTER_KEYS.to_vec();
        assert_eq!(
            classify(&caps("kbd", &keys, &[], &[])),
            Some(DeviceRole::Keyboard)
        );
        keys.pop();
        assert_eq!(classify(&caps("power button", &keys, &[], &[])), None);
        assert_eq!(classify(&caps("power", &[116], &[], &[])), None);
    }

    #[test]
    fn mouse_and_touchpad_are_pointers() {
        let mouse = caps("mouse", &[BTN_LEFT, 0x111], &[REL_X, REL_Y], &[]);
        assert_eq!(classify(&mouse), Some(DeviceRole::Pointer));
        let touchpad = caps("touchpad", &[BTN_LEFT, 0x145], &[], &[ABS_X, ABS_Y, 0x35]);
        assert_eq!(classify(&touchpad), Some(DeviceRole::Pointer));
    }

    #[test]
    fn button_without_motion_or_motion_without_button_is_ignored() {
        assert_eq!(classify(&caps("buttons", &[BTN_LEFT], &[], &[])), None);
        assert_eq!(classify(&caps("wheel", &[], &[REL_X, REL_Y], &[])), None);
        let touchscreen = caps("touchscreen", &[0x14a], &[], &[ABS_X, ABS_Y]);
        assert_eq!(classify(&touchscreen), None);
    }

    #[test]
    fn combo_device_is_both() {
        let mut keys: Vec<u16> = LETTER_KEYS.to_vec();
        keys.push(BTN_LEFT);
        assert_eq!(
            classify(&caps("combo", &keys, &[REL_X, REL_Y], &[])),
            Some(DeviceRole::KeyboardPointer)
        );
    }

    #[test]
    fn own_and_virtual_devices_are_never_grabbed() {
        let mut keys: Vec<u16> = LETTER_KEYS.to_vec();
        keys.push(BTN_LEFT);
        for name in OWN_DEVICE_NAMES {
            assert_eq!(classify(&caps(name, &keys, &[REL_X, REL_Y], &[])), None);
        }
        let mut other = caps("ydotoold virtual device", &keys, &[REL_X, REL_Y], &[]);
        other.virtual_device = true;
        assert_eq!(classify(&other), None);
        assert!(is_own_device("niri-computer-use-keyboard"));
        assert!(!is_own_device("AT Translated Set 2 keyboard"));
    }

    #[test]
    fn virtual_sysfs_paths_are_detected() {
        assert!(path_is_virtual(Path::new(
            "/sys/devices/virtual/input/input23/event7"
        )));
        assert!(!path_is_virtual(Path::new(
            "/sys/devices/platform/i8042/serio0/input/input3/event3"
        )));
    }

    #[test]
    fn replay_keeps_key_order_and_drops_autorepeat() {
        let events = [key(30, 1), key(30, 2), key(30, 2), key(30, 0), key(272, 1)];
        assert_eq!(
            plan_replay(&events),
            vec![
                ReplayAction::Key { code: 30, value: 1 },
                ReplayAction::Key { code: 30, value: 0 },
                ReplayAction::Key {
                    code: 272,
                    value: 1
                },
                ReplayAction::Key {
                    code: 272,
                    value: 0
                },
            ]
        );
    }

    #[test]
    fn release_without_a_buffered_press_is_not_replayed() {
        assert!(plan_replay(&[key(42, 0)]).is_empty());
    }

    #[test]
    fn keys_still_down_at_the_end_are_released_on_the_replay_device() {
        assert_eq!(
            plan_replay(&[key(42, 1), key(30, 1), key(30, 0)]),
            vec![
                ReplayAction::Key { code: 42, value: 1 },
                ReplayAction::Key { code: 30, value: 1 },
                ReplayAction::Key { code: 30, value: 0 },
                ReplayAction::Key { code: 42, value: 0 },
            ]
        );
    }

    #[test]
    fn a_key_down_without_a_buffered_press_was_pressed_before_the_grab() {
        let buffered = [key(30, 1)];
        assert_eq!(pressed_before_grab(&[30, 42], &buffered), vec![42]);
        assert!(pressed_before_grab(&[30], &buffered).is_empty());
    }

    #[test]
    fn relative_motion_is_summed_between_button_events() {
        let events = [
            rel(REL_X, 5),
            rel(REL_Y, -2),
            rel(REL_X, 3),
            key(272, 1),
            rel(REL_X, 1),
            rel(REL_X, -1),
            rel(REL_Y, 4),
        ];
        assert_eq!(
            plan_replay(&events),
            vec![
                ReplayAction::Rel(vec![(REL_X, 8), (REL_Y, -2)]),
                ReplayAction::Key {
                    code: 272,
                    value: 1
                },
                ReplayAction::Rel(vec![(REL_Y, 4)]),
                ReplayAction::Key {
                    code: 272,
                    value: 0
                },
            ]
        );
    }

    #[test]
    fn unreplayable_events_are_dropped() {
        let events = [
            key(0x14a, 1), // BTN_TOUCH
            key(0x145, 1), // BTN_TOOL_FINGER
            key(0, 1),
            RawEvent {
                kind: 3, // EV_ABS
                code: 0,
                value: 100,
            },
            RawEvent {
                kind: 0, // EV_SYN
                code: 0,
                value: 0,
            },
            rel(0x0a, 7), // REL_DIAL
            rel(REL_X, 4),
            rel(REL_X, -4),
        ];
        assert!(plan_replay(&events).is_empty());
    }

    #[test]
    fn scroll_axes_accumulate_per_axis() {
        let wheel = RelativeAxisCode::REL_WHEEL.0;
        let hi = RelativeAxisCode::REL_WHEEL_HI_RES.0;
        let actions = plan_replay(&[rel(wheel, -1), rel(hi, -120), rel(wheel, -1), rel(hi, -120)]);
        assert_eq!(
            actions,
            vec![ReplayAction::Rel(vec![(wheel, -2), (hi, -240)])]
        );
    }
}
