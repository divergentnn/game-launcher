// SPDX-License-Identifier: GPL-3.0-or-later
//! Controller input straight from evdev. Every thread blocks in the kernel,
//! so this costs no CPU while nothing is pressed. Devices are grabbed while
//! the launcher is open so the game underneath does not react to navigation.

use evdev::{AbsoluteAxisCode as Abs, Device, EventSummary, KeyCode as Key};
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Dir),
    Accept,
    Back,
    PrevTab,
    NextTab,
    Sort,
    AllApps,
}

enum Raw {
    /// A direction source (hat axis, stick axis, d-pad button) changed state.
    Hold { source: (u64, u8), dir: Option<Dir> },
    Press(Action),
}

const FIRST_REPEAT: Duration = Duration::from_millis(380);
const REPEAT: Duration = Duration::from_millis(110);

fn is_gamepad(device: &Device) -> bool {
    let keys = device.supported_keys();
    let has_face = keys.is_some_and(|k| k.contains(Key::BTN_SOUTH));
    let has_dpad = keys.is_some_and(|k| k.contains(Key::BTN_DPAD_UP));
    let has_axes = device
        .supported_absolute_axes()
        .is_some_and(|a| a.contains(Abs::ABS_X) || a.contains(Abs::ABS_HAT0X));
    has_face && (has_axes || has_dpad)
}

fn button_action(key: Key) -> Option<Action> {
    Some(match key {
        Key::BTN_SOUTH => Action::Accept,
        Key::BTN_EAST | Key::BTN_MODE => Action::Back,
        Key::BTN_TL => Action::PrevTab,
        Key::BTN_TR => Action::NextTab,
        // BTN_NORTH/BTN_WEST share codes with BTN_X/BTN_Y and drivers disagree
        // on which is which, so either top/left face button toggles sorting.
        Key::BTN_NORTH | Key::BTN_WEST => Action::Sort,
        Key::BTN_START => Action::AllApps,
        _ => return None,
    })
}

struct Stick {
    center: f32,
    half: f32,
    state: Option<Dir>,
}

impl Stick {
    /// Hysteresis: engage past 55% deflection, release under 35%.
    fn update(&mut self, value: i32, negative: Dir, positive: Dir) -> Option<Option<Dir>> {
        let pos = (value as f32 - self.center) / self.half.max(1.0);
        let next = if pos <= -0.55 {
            Some(negative)
        } else if pos >= 0.55 {
            Some(positive)
        } else if pos.abs() < 0.35 {
            None
        } else {
            self.state
        };
        (next != self.state).then(|| {
            self.state = next;
            next
        })
    }
}

fn read_device(id: u64, mut device: Device, path: PathBuf, raw: Sender<Raw>, open: Arc<Mutex<HashSet<PathBuf>>>) {
    let _ = device.grab(); // fails harmlessly if another program holds the grab
    let mut sticks: HashMap<Abs, Stick> = HashMap::new();
    if let Ok(infos) = device.get_absinfo() {
        for (axis, info) in infos {
            if axis == Abs::ABS_X || axis == Abs::ABS_Y {
                let (min, max) = (info.minimum() as f32, info.maximum() as f32);
                sticks.insert(axis, Stick { center: (min + max) / 2.0, half: (max - min) / 2.0, state: None });
            }
        }
    }
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events.collect::<Vec<_>>(),
            Err(_) => break, // unplugged
        };
        for event in events {
            let msg = match event.destructure() {
                EventSummary::Key(_, key, value) => {
                    let dir = match key {
                        Key::BTN_DPAD_UP => Some((4, Dir::Up)),
                        Key::BTN_DPAD_DOWN => Some((5, Dir::Down)),
                        Key::BTN_DPAD_LEFT => Some((6, Dir::Left)),
                        Key::BTN_DPAD_RIGHT => Some((7, Dir::Right)),
                        _ => None,
                    };
                    match (dir, value) {
                        (Some((source, d)), 1) => Some(Raw::Hold { source: (id, source), dir: Some(d) }),
                        (Some((source, _)), 0) => Some(Raw::Hold { source: (id, source), dir: None }),
                        (None, 1) => button_action(key).map(Raw::Press),
                        _ => None,
                    }
                }
                EventSummary::AbsoluteAxis(_, axis, value) => match axis {
                    Abs::ABS_HAT0X => Some(Raw::Hold {
                        source: (id, 0),
                        dir: (value != 0).then_some(if value < 0 { Dir::Left } else { Dir::Right }),
                    }),
                    Abs::ABS_HAT0Y => Some(Raw::Hold {
                        source: (id, 1),
                        dir: (value != 0).then_some(if value < 0 { Dir::Up } else { Dir::Down }),
                    }),
                    Abs::ABS_X | Abs::ABS_Y => {
                        let (negative, positive, source) =
                            if axis == Abs::ABS_X { (Dir::Left, Dir::Right, 2) } else { (Dir::Up, Dir::Down, 3) };
                        sticks
                            .get_mut(&axis)
                            .and_then(|s| s.update(value, negative, positive))
                            .map(|dir| Raw::Hold { source: (id, source), dir })
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(msg) = msg {
                if raw.send(msg).is_err() {
                    return;
                }
            }
        }
    }
    open.lock().unwrap().remove(&path);
}

fn try_open(path: &Path, next_id: &Mutex<u64>, raw: &Sender<Raw>, open: &Arc<Mutex<HashSet<PathBuf>>>) {
    let name_ok = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"));
    if !name_ok || open.lock().unwrap().contains(path) {
        return;
    }
    let Ok(device) = Device::open(path) else { return };
    if !is_gamepad(&device) {
        return;
    }
    open.lock().unwrap().insert(path.to_path_buf());
    let id = {
        let mut n = next_id.lock().unwrap();
        *n += 1;
        *n
    };
    let (raw, open, path) = (raw.clone(), open.clone(), path.to_path_buf());
    std::thread::spawn(move || read_device(id, device, path, raw, open));
}

/// Watch /dev/input with inotify so controllers plugged in later are picked up.
fn watch_hotplug(raw: Sender<Raw>, open: Arc<Mutex<HashSet<PathBuf>>>, next_id: Arc<Mutex<u64>>) {
    std::thread::spawn(move || unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 {
            return;
        }
        let dir = c"/dev/input";
        if libc::inotify_add_watch(fd, dir.as_ptr(), libc::IN_CREATE | libc::IN_ATTRIB) < 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                return;
            }
            let mut offset = 0usize;
            while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                let event = &*(buf.as_ptr().add(offset) as *const libc::inotify_event);
                let name_ptr = buf.as_ptr().add(offset + std::mem::size_of::<libc::inotify_event>());
                if event.len > 0 {
                    let name = CStr::from_ptr(name_ptr.cast()).to_string_lossy().into_owned();
                    let path = Path::new("/dev/input").join(name);
                    // Permissions (uaccess ACLs) can land just after creation.
                    for _ in 0..5 {
                        try_open(&path, &next_id, &raw, &open);
                        if open.lock().unwrap().contains(&path) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(150));
                    }
                }
                offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
            }
        }
    });
}

/// Start reading all controllers; `on_action` runs on a background thread.
pub fn start(on_action: impl Fn(Action) + Send + 'static) {
    let (raw_tx, raw_rx) = channel::<Raw>();
    let open = Arc::new(Mutex::new(HashSet::new()));
    let next_id = Arc::new(Mutex::new(0u64));
    watch_hotplug(raw_tx.clone(), open.clone(), next_id.clone());
    if let Ok(entries) = std::fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            try_open(&entry.path(), &next_id, &raw_tx, &open);
        }
    }
    drop(raw_tx);

    // Turn held directions into an initial move plus key-repeat.
    std::thread::spawn(move || {
        let mut held: HashMap<(u64, u8), (Dir, Instant)> = HashMap::new();
        loop {
            let msg = if held.is_empty() {
                match raw_rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                }
            } else {
                let next_due = held.values().map(|(_, due)| *due).min().unwrap();
                match raw_rx.recv_timeout(next_due.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            match msg {
                Some(Raw::Press(action)) => on_action(action),
                Some(Raw::Hold { source, dir: Some(dir) }) => {
                    if held.get(&source).map(|(d, _)| *d) != Some(dir) {
                        held.insert(source, (dir, Instant::now() + FIRST_REPEAT));
                        on_action(Action::Move(dir));
                    }
                }
                Some(Raw::Hold { source, dir: None }) => {
                    held.remove(&source);
                }
                None => {
                    let now = Instant::now();
                    for (dir, due) in held.values_mut() {
                        if *due <= now {
                            *due = now + REPEAT;
                            on_action(Action::Move(*dir));
                        }
                    }
                }
            }
        }
    });
}
