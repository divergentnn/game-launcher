// SPDX-License-Identifier: GPL-3.0-or-later
//! game-launcher: a Big Picture-style, controller-first library for Steam,
//! Heroic (Epic/GOG via heroic-lite) and other installed games.
//!
//!   game-launcher            open full-screen (a second invocation closes it)
//!   game-launcher --windowed open in a normal window
//!   game-launcher --list     print the discovered library and exit

mod art;
mod gamepad;
mod library;
mod vdf;

slint::include_modules!();

use gamepad::{Action, Dir};
use library::{Game, Source};
use slint::{Color, ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TABS: [(&str, Option<Source>); 4] = [
    ("All", None),
    ("Steam", Some(Source::Steam)),
    ("Epic & GOG", Some(Source::Heroic)),
    ("Other", Some(Source::Desktop)),
];
const BACKDROP_CACHE: usize = 10;
const ACCENTS: [(u8, u8, u8); 8] = [
    (0x2f, 0x5d, 0xa8),
    (0x7b, 0x3f, 0xa0),
    (0xa8, 0x3f, 0x5a),
    (0x2f, 0x8a, 0x7a),
    (0xb0, 0x6a, 0x2c),
    (0x3f, 0x6e, 0x3a),
    (0x5a, 0x4f, 0xb0),
    (0x8a, 0x34, 0x34),
];

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Recent,
    Alpha,
}

struct State {
    games: Vec<Game>,
    /// Indices into `games`, in display order for the current tab and sort.
    view: Vec<usize>,
    tabs: Vec<(&'static str, Option<Source>)>,
    tab: usize,
    sort: Sort,
    posters: HashMap<String, Image>,
    icons: HashMap<String, Image>,
    backdrops: VecDeque<(String, Image)>,
    model: Rc<VecModel<Tile>>,
}

thread_local! {
    static APP: RefCell<Option<(Rc<RefCell<State>>, slint::Weak<AppWindow>)>> = const { RefCell::new(None) };
}

fn with_app(f: impl FnOnce(&mut State, &AppWindow)) {
    APP.with(|app| {
        let app = app.borrow();
        if let Some((state, weak)) = app.as_ref() {
            if let Some(ui) = weak.upgrade() {
                f(&mut state.borrow_mut(), &ui);
            }
        }
    });
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn accent(key: &str) -> Color {
    let hash = key.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    let (r, g, b) = ACCENTS[hash as usize % ACCENTS.len()];
    Color::from_rgb_u8(r, g, b)
}

fn played_text(last: i64) -> String {
    if last <= 0 {
        return "Not played yet".into();
    }
    let days = (now() - last).max(0) / 86400;
    match days {
        0 => "Played today".into(),
        1 => "Played yesterday".into(),
        2..=44 => format!("Played {days} days ago"),
        45..=364 => format!("Played {} months ago", (days + 15) / 30),
        _ => format!("Played {} years ago", days / 365),
    }
}

fn clock_text() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

fn sort_key(title: &str) -> String {
    let lower = title.to_lowercase();
    lower.strip_prefix("the ").map(str::to_string).unwrap_or(lower)
}

impl State {
    fn tile(&self, game: &Game) -> Tile {
        let poster = self.posters.get(&game.key);
        let icon = self.icons.get(&game.key);
        Tile {
            key: game.key.as_str().into(),
            title: game.title.as_str().into(),
            poster: poster.cloned().unwrap_or_default(),
            has_poster: poster.is_some(),
            icon: icon.cloned().unwrap_or_default(),
            has_icon: icon.is_some(),
            accent: accent(&game.key),
        }
    }

    fn compute_view(&mut self) {
        let filter = self.tabs[self.tab].1;
        let mut view: Vec<usize> = (0..self.games.len())
            .filter(|&i| filter.is_none_or(|s| self.games[i].source == s))
            .collect();
        let games = &self.games;
        match self.sort {
            Sort::Recent => view.sort_by(|&a, &b| {
                games[b].last_played.cmp(&games[a].last_played).then_with(|| sort_key(&games[a].title).cmp(&sort_key(&games[b].title)))
            }),
            Sort::Alpha => view.sort_by_key(|&i| sort_key(&games[i].title)),
        }
        self.view = view;
    }

    fn selected_game(&self, ui: &AppWindow) -> Option<&Game> {
        let index = usize::try_from(ui.get_selected()).ok()?;
        self.view.get(index).map(|&g| &self.games[g])
    }

    fn backdrop(&mut self, key: &str) -> Option<Image> {
        if let Some(pos) = self.backdrops.iter().position(|(k, _)| k == key) {
            let entry = self.backdrops.remove(pos).unwrap();
            let image = entry.1.clone();
            self.backdrops.push_front(entry);
            return Some(image);
        }
        let buffer = art::load_rgb(&art::backdrop_cache(key))?;
        let image = Image::from_rgb8(buffer);
        self.backdrops.push_front((key.to_string(), image.clone()));
        self.backdrops.truncate(BACKDROP_CACHE);
        Some(image)
    }
}

fn refresh_selection(state: &mut State, ui: &AppWindow) {
    let Some(game) = state.selected_game(ui).cloned() else {
        ui.set_sel_title(SharedString::new());
        ui.set_sel_meta(SharedString::new());
        ui.set_has_backdrop(false);
        return;
    };
    ui.set_sel_title(game.title.as_str().into());
    ui.set_sel_meta(format!("{}   ·   {}", game.store, played_text(game.last_played)).into());
    match state.backdrop(&game.key) {
        Some(image) => {
            ui.set_backdrop(image);
            ui.set_has_backdrop(true);
        }
        None => ui.set_has_backdrop(false),
    }
    ui.invoke_ensure_visible();
}

fn rebuild(state: &mut State, ui: &AppWindow, keep_key: Option<String>) {
    state.compute_view();
    let tiles: Vec<Tile> = state.view.iter().map(|&g| state.tile(&state.games[g])).collect();
    state.model.set_vec(tiles);
    let selected = keep_key
        .and_then(|key| state.view.iter().position(|&g| state.games[g].key == key))
        .unwrap_or(0);
    ui.set_selected(selected as i32);
    ui.set_tab_index(state.tab as i32);
    ui.set_sort_label(if state.sort == Sort::Recent { "Recent" } else { "A–Z" }.into());
    refresh_selection(state, ui);
}

fn move_selection(state: &mut State, ui: &AppWindow, dx: i32, dy: i32) {
    let len = state.view.len() as i32;
    if len == 0 {
        return;
    }
    let cols = ui.get_columns().max(1);
    let current = ui.get_selected().clamp(0, len - 1);
    let col = current % cols;
    let mut next = current;
    if dx < 0 && col > 0 {
        next = current - 1;
    } else if dx > 0 && col < cols - 1 && current + 1 < len {
        next = current + 1;
    } else if dy < 0 && current - cols >= 0 {
        next = current - cols;
    } else if dy > 0 {
        let last_row = (len - 1) / cols;
        if current / cols < last_row {
            next = (current + cols).min(len - 1);
        }
    }
    if next != current {
        ui.set_selected(next);
        refresh_selection(state, ui);
    }
}

fn spawn_detached(argv: &[String]) -> std::io::Result<()> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
}

fn notify(summary: &str) {
    let _ = spawn_detached(&["notify-send".into(), "-a".into(), "Games".into(), summary.into()]);
}

fn launch_selected(state: &mut State, ui: &AppWindow, index: i32) {
    let Some(&g) = usize::try_from(index).ok().and_then(|i| state.view.get(i)) else { return };
    let game = state.games[g].clone();
    library::record_played(&game.key, now());
    match spawn_detached(&game.launch) {
        Ok(()) => {
            // heroic-lite posts its own notification.
            if game.source != Source::Heroic {
                notify(&format!("Starting {}", game.title));
            }
            ui.hide().ok();
            let _ = slint::quit_event_loop();
        }
        Err(error) => notify(&format!("Could not start {}: {error}", game.title)),
    }
}

fn change_tab(state: &mut State, ui: &AppWindow, delta: i32) {
    let count = state.tabs.len() as i32;
    let tab = (state.tab as i32 + delta).rem_euclid(count) as usize;
    if tab != state.tab {
        state.tab = tab;
        rebuild(state, ui, None);
    }
}

fn handle(action: Action) {
    with_app(|state, ui| match action {
        Action::Move(Dir::Left) => move_selection(state, ui, -1, 0),
        Action::Move(Dir::Right) => move_selection(state, ui, 1, 0),
        Action::Move(Dir::Up) => move_selection(state, ui, 0, -1),
        Action::Move(Dir::Down) => move_selection(state, ui, 0, 1),
        Action::Accept => launch_selected(state, ui, ui.get_selected()),
        Action::Back => {
            let _ = slint::quit_event_loop();
        }
        Action::PrevTab => change_tab(state, ui, -1),
        Action::NextTab => change_tab(state, ui, 1),
        Action::Sort => toggle_sort(state, ui),
        Action::AllApps => open_all_apps(),
    });
}

fn toggle_sort(state: &mut State, ui: &AppWindow) {
    let keep = state.selected_game(ui).map(|g| g.key.clone());
    state.sort = if state.sort == Sort::Recent { Sort::Alpha } else { Sort::Recent };
    rebuild(state, ui, keep);
}

fn open_all_apps() {
    let menu = library::home().join(".local/bin/hypr-app-menu");
    if spawn_detached(&[menu.to_string_lossy().into_owned()]).is_ok() {
        let _ = slint::quit_event_loop();
    }
}

/// Single instance: a second start closes the open launcher (toggle).
fn take_instance_lock() -> Option<File> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join("game-launcher.lock");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut pid = String::new();
        let _ = file.read_to_string(&mut pid);
        if let Ok(pid) = pid.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        return None;
    }
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
    Some(file)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for game in library::discover() {
            let poster = if game.art.poster_files.iter().any(|p| p.is_file()) { "local" } else if game.art.poster_urls.is_empty() { "none" } else { "url" };
            println!("{:<34} {:<6} poster={:<5} last={:<11} {}", game.key, game.store, poster, game.last_played, game.title);
        }
        return Ok(());
    }
    // Test hooks: a separate app id (so window rules can target a test window)
    // and running without grabbing the controllers.
    let app_id = std::env::var("GAME_LAUNCHER_APP_ID").unwrap_or_else(|_| "game-launcher".into());
    let use_gamepads = std::env::var_os("GAME_LAUNCHER_NO_GAMEPAD").is_none();
    let Some(_lock) = (if app_id == "game-launcher" { take_instance_lock() } else { File::open("/dev/null").ok() }) else {
        return Ok(());
    };
    let windowed = args.iter().any(|a| a == "--windowed");

    let games = library::discover();
    let tabs: Vec<_> = TABS
        .iter()
        .copied()
        .filter(|(_, source)| source.is_none_or(|s| games.iter().any(|g| g.source == s)))
        .collect();

    let ui = AppWindow::new()?;
    // Needs the platform that AppWindow::new() creates; must precede showing the window.
    slint::set_xdg_app_id(app_id)?;
    let model = Rc::new(VecModel::<Tile>::default());
    ui.set_tiles(ModelRc::from(model.clone()));
    ui.set_tabs(ModelRc::new(VecModel::from(tabs.iter().map(|(n, _)| SharedString::from(*n)).collect::<Vec<_>>())));
    ui.set_clock(clock_text().into());

    let state = Rc::new(RefCell::new(State {
        games,
        view: Vec::new(),
        tabs,
        tab: 0,
        sort: Sort::Recent,
        posters: HashMap::new(),
        icons: HashMap::new(),
        backdrops: VecDeque::new(),
        model,
    }));
    APP.with(|app| *app.borrow_mut() = Some((state.clone(), ui.as_weak())));

    ui.on_move(|dx, dy| with_app(|s, ui| move_selection(s, ui, dx, dy)));
    ui.on_activate(|index| with_app(|s, ui| launch_selected(s, ui, index)));
    ui.on_close_launcher(|| {
        let _ = slint::quit_event_loop();
    });
    ui.on_next_tab(|delta| with_app(|s, ui| change_tab(s, ui, delta)));
    ui.on_toggle_sort(|| with_app(toggle_sort));
    ui.on_all_apps(open_all_apps);

    // Build the grid (sorted) before art arrives, then load art in display order.
    with_app(|s, ui| rebuild(s, ui, None));
    let jobs: Vec<art::Job> = {
        let s = state.borrow();
        let mut order: Vec<usize> = s.view.clone();
        order.extend((0..s.games.len()).filter(|i| !s.view.contains(i)));
        order.iter().map(|&i| art::Job { key: s.games[i].key.clone(), art: s.games[i].art.clone() }).collect()
    };
    art::spawn_worker(jobs, |msg| {
        let _ = slint::invoke_from_event_loop(move || {
            with_app(|state, ui| match msg {
                art::Msg::Poster(key, buffer) => {
                    state.posters.insert(key.clone(), Image::from_rgb8(buffer));
                    update_tile(state, &key);
                }
                art::Msg::Icon(key, buffer) => {
                    state.icons.insert(key.clone(), Image::from_rgba8(buffer));
                    update_tile(state, &key);
                }
                art::Msg::BackdropReady(key) => {
                    state.backdrops.retain(|(k, _)| *k != key);
                    if state.selected_game(ui).is_some_and(|g| g.key == key) {
                        refresh_selection(state, ui);
                    }
                }
            })
        });
    });

    if use_gamepads {
        gamepad::start(|action| {
            let _ = slint::invoke_from_event_loop(move || handle(action));
        });
    }

    let clock = Timer::default();
    let weak = ui.as_weak();
    clock.start(TimerMode::Repeated, Duration::from_secs(15), move || {
        if let Some(ui) = weak.upgrade() {
            let text: SharedString = clock_text().into();
            if ui.get_clock() != text {
                ui.set_clock(text);
            }
        }
    });

    if !windowed {
        ui.window().set_fullscreen(true);
    }
    ui.run()?;
    Ok(())
}

fn update_tile(state: &State, key: &str) {
    if let Some(row) = state.view.iter().position(|&g| state.games[g].key == key) {
        let tile = state.tile(&state.games[state.view[row]]);
        state.model.set_row_data(row, tile);
    }
}
