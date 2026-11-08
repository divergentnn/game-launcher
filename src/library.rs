// SPDX-License-Identifier: GPL-3.0-or-later
//! Game discovery: Steam manifests, Heroic (Epic/GOG via heroic-lite), and
//! `.desktop` entries in the Game category. Read fresh on every start so
//! installs and uninstalls show up without any background service.

use crate::vdf;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Steam,
    Heroic,
    Desktop,
}

#[derive(Clone, Debug, Default)]
pub struct Art {
    /// Local portrait images, best first.
    pub poster_files: Vec<PathBuf>,
    /// Remote portrait images, tried in order if no local file exists.
    pub poster_urls: Vec<String>,
    /// Local landscape images for the blurred backdrop.
    pub backdrop_files: Vec<PathBuf>,
    pub backdrop_urls: Vec<String>,
    /// Small square icon shown on generated title cards.
    pub icon_file: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Game {
    /// Stable id, e.g. `steam-220`, `legendary-<app>`, `gog-<id>`, `desktop-eden`.
    pub key: String,
    pub title: String,
    pub source: Source,
    pub store: &'static str,
    /// Unix seconds; 0 means never played.
    pub last_played: i64,
    pub launch: Vec<String>,
    pub art: Art,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

fn read_json(path: &Path) -> serde_json::Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse `2026-10-15T06:30:49.543Z` (UTC) into Unix seconds.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (y, mo, d) = (num(&text[0..4])?, num(&text[5..7])?, num(&text[8..10])?);
    let (h, mi, s) = (num(&text[11..13])?, num(&text[14..16])?, num(&text[17..19])?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s)
}

// ---------------------------------------------------------------- Steam ----

const STEAM_TOOL_PREFIXES: &[&str] = &["Proton", "Steam Linux Runtime", "Steamworks Common"];

fn steam_root() -> PathBuf {
    home().join(".local/share/Steam")
}

fn steam_launcher() -> String {
    // The wrapper works around a Steam D-Bus crash; Hyprland's PATH lacks ~/.local/bin.
    let wrapper = home().join(".local/bin/steam");
    if wrapper.exists() {
        wrapper.to_string_lossy().into_owned()
    } else {
        "steam".into()
    }
}

/// Find `<cache>/<appid>/<name>` or `<cache>/<appid>/<hash>/<name>`.
fn steam_cached(appid: &str, name: &str) -> Option<PathBuf> {
    let dir = steam_root().join("appcache/librarycache").join(appid);
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(&dir).ok()?.flatten().map(|e| e.path().join(name)).find(|p| p.is_file())
}

pub fn steam_art(appid: &str) -> Art {
    let cdn = |file: &str| {
        vec![
            format!("https://shared.steamstatic.com/store_item_assets/steam/apps/{appid}/{file}"),
            format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{file}"),
        ]
    };
    Art {
        // Newer Steam clients cache the portrait art as library_capsule.jpg.
        poster_files: ["library_600x900.jpg", "library_capsule.jpg"]
            .iter()
            .filter_map(|name| steam_cached(appid, name))
            .collect(),
        poster_urls: [cdn("library_600x900_2x.jpg"), cdn("library_600x900.jpg")].concat(),
        backdrop_files: ["library_hero.jpg", "header.jpg", "library_header.jpg"]
            .iter()
            .filter_map(|name| steam_cached(appid, name))
            .collect(),
        backdrop_urls: cdn("library_hero.jpg"),
        icon_file: None,
    }
}

fn steam_games(out: &mut Vec<Game>) {
    let root = steam_root();
    let mut libraries = vec![root.clone()];
    if let Ok(text) = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
        if let Some(folders) = vdf::parse(&text).get("libraryfolders").and_then(|v| v.map().cloned()) {
            for folder in folders.values() {
                if let Some(path) = folder.str("path") {
                    let path = PathBuf::from(path);
                    if !libraries.contains(&path) {
                        libraries.push(path);
                    }
                }
            }
        }
    }
    let launcher = steam_launcher();
    let mut seen = HashSet::new();
    for library in libraries {
        let Ok(entries) = fs::read_dir(library.join("steamapps")) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(entry.path()) else { continue };
            let doc = vdf::parse(&text);
            let Some(app) = doc.get("AppState") else { continue };
            let (Some(appid), Some(title)) = (app.str("appid"), app.str("name")) else { continue };
            let flags: u64 = app.str("StateFlags").and_then(|s| s.parse().ok()).unwrap_or(0);
            if flags & 4 == 0 || !seen.insert(appid.to_string()) {
                continue; // not fully installed, or listed in two libraries
            }
            if STEAM_TOOL_PREFIXES.iter().any(|p| title.starts_with(p)) || appid == "228980" {
                continue;
            }
            out.push(Game {
                key: format!("steam-{appid}"),
                title: title.to_string(),
                source: Source::Steam,
                store: "Steam",
                last_played: app.str("LastPlayed").and_then(|s| s.parse().ok()).unwrap_or(0),
                launch: vec![launcher.clone(), format!("steam://rungameid/{appid}")],
                art: steam_art(appid),
            });
        }
    }
}

// --------------------------------------------------------------- Heroic ----

fn heroic_games(out: &mut Vec<Game>) {
    let heroic = home().join(".config/heroic");
    let lite = home().join(".local/bin/heroic-lite").to_string_lossy().into_owned();
    let timestamps = read_json(&heroic.join("store/timestamp.json"));
    let last_played = |app: &str| {
        timestamps[app]["lastPlayed"].as_str().and_then(parse_iso8601).unwrap_or(0)
    };
    let library_art = |file: &str| -> HashMap<String, (String, String, String)> {
        let doc = read_json(&heroic.join("store_cache").join(file));
        let list = doc.get("library").or_else(|| doc.get("games")).and_then(|v| v.as_array()).cloned();
        list.unwrap_or_default()
            .iter()
            .filter_map(|g| {
                let app = g["app_name"].as_str()?.to_string();
                let s = |k: &str| g[k].as_str().unwrap_or_default().to_string();
                Some((app, (s("title"), s("art_square"), s("art_cover"))))
            })
            .collect()
    };
    let art_for = |app: &str, square: &str, cover: &str| {
        let icon = ["png", "jpg"]
            .iter()
            .map(|ext| heroic.join("icons").join(format!("{app}.{ext}")))
            .find(|p| p.is_file());
        Art {
            poster_files: vec![],
            poster_urls: [square].iter().filter(|u| !u.is_empty()).map(|u| u.to_string()).collect(),
            backdrop_files: vec![],
            backdrop_urls: [cover].iter().filter(|u| !u.is_empty()).map(|u| u.to_string()).collect(),
            icon_file: icon,
        }
    };

    let epic_art = library_art("legendary_library.json");
    let installed = read_json(&heroic.join("legendaryConfig/legendary/installed.json"));
    if let Some(map) = installed.as_object() {
        for (app, info) in map {
            if info["is_dlc"].as_bool().unwrap_or(false) {
                continue;
            }
            let (lib_title, square, cover) = epic_art.get(app).cloned().unwrap_or_default();
            let title = info["title"].as_str().map(str::to_string).filter(|t| !t.is_empty()).unwrap_or(lib_title);
            out.push(Game {
                key: format!("legendary-{app}"),
                title: if title.is_empty() { app.clone() } else { title },
                source: Source::Heroic,
                store: "Epic",
                last_played: last_played(app),
                launch: vec![lite.clone(), "launch".into(), "legendary".into(), app.clone()],
                art: art_for(app, &square, &cover),
            });
        }
    }

    let gog_art = library_art("gog_library.json");
    let installed = read_json(&heroic.join("gog_store/installed.json"));
    for info in installed["installed"].as_array().cloned().unwrap_or_default() {
        let Some(app) = info["appName"].as_str() else { continue };
        if info["is_dlc"].as_bool().unwrap_or(false) {
            continue;
        }
        let (title, square, cover) = gog_art.get(app).cloned().unwrap_or_default();
        let fallback = info["install_path"].as_str().and_then(|p| Path::new(p).file_name()).map(|n| n.to_string_lossy().into_owned());
        let mut art = art_for(app, &square, &cover);
        if let Some(path) = info["install_path"].as_str() {
            let ico = Path::new(path).join(format!("goggame-{app}.ico"));
            if art.icon_file.is_none() && ico.is_file() {
                art.icon_file = Some(ico);
            }
        }
        out.push(Game {
            key: format!("gog-{app}"),
            title: if title.is_empty() { fallback.unwrap_or_else(|| app.to_string()) } else { title },
            source: Source::Heroic,
            store: "GOG",
            last_played: last_played(app),
            launch: vec![lite.clone(), "launch".into(), "gog".into(), app.to_string()],
            art,
        });
    }
}

// -------------------------------------------------------------- Desktop ----

fn application_dirs() -> Vec<PathBuf> {
    let h = home();
    vec![
        h.join(".local/share/applications"),
        h.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ]
}

fn icon_dirs() -> Vec<PathBuf> {
    let h = home();
    let mut bases = vec![
        h.join(".local/share/icons/hicolor"),
        h.join(".local/share/flatpak/exports/share/icons/hicolor"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor"),
        PathBuf::from("/usr/share/icons/hicolor"),
    ];
    let mut dirs = Vec::new();
    for size in ["512x512", "256x256", "192x192", "128x128", "96x96", "64x64", "48x48"] {
        for base in &bases {
            dirs.push(base.join(size).join("apps"));
        }
    }
    bases.clear();
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

/// Resolve an `Icon=` value to a PNG/JPEG file (SVG is not supported).
fn resolve_icon(icon: &str) -> Option<PathBuf> {
    if icon.is_empty() {
        return None;
    }
    let path = Path::new(icon);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    for dir in icon_dirs() {
        for ext in ["png", "jpg"] {
            let candidate = dir.join(format!("{icon}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn desktop_games(out: &mut Vec<Game>, played: &HashMap<String, i64>) {
    let mut seen = HashSet::new();
    for dir in application_dirs() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".desktop")) else {
                continue;
            };
            if !seen.insert(id.to_string()) {
                continue; // a higher-priority directory already defines this id
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let mut fields = HashMap::new();
            let mut in_main = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_main = line == "[Desktop Entry]";
                } else if in_main {
                    if let Some((k, v)) = line.split_once('=') {
                        fields.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
                    }
                }
            }
            let get = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
            let truthy = |k: &str| get(k).eq_ignore_ascii_case("true");
            if get("Type") != "Application" || truthy("NoDisplay") || truthy("Hidden") {
                continue;
            }
            if !get("Categories").split(';').any(|c| c == "Game") {
                continue;
            }
            let exec = get("Exec");
            if exec.contains("steam://rungameid") || exec.contains("heroic://") || exec.contains("heroic-lite") {
                continue; // covered by the Steam and Heroic sources
            }
            let icon = get("Icon");
            // Custom wrappers often reuse a Steam game's icon: borrow its Big Picture art.
            let art = match icon.strip_prefix("steam_icon_") {
                Some(appid) if appid.chars().all(|c| c.is_ascii_digit()) => {
                    let mut art = steam_art(appid);
                    art.icon_file = resolve_icon(icon);
                    art
                }
                _ => Art { icon_file: resolve_icon(icon), ..Art::default() },
            };
            let key = format!("desktop-{id}");
            out.push(Game {
                last_played: played.get(&key).copied().unwrap_or(0),
                key,
                title: get("Name").to_string(),
                source: Source::Desktop,
                store: "App",
                launch: vec!["gio".into(), "launch".into(), path.to_string_lossy().into_owned()],
                art,
            });
        }
    }
}

// ------------------------------------------------------------ Assembly ----

pub fn state_dir() -> PathBuf {
    home().join(".local/state/game-launcher")
}

pub fn config_dir() -> PathBuf {
    home().join(".config/game-launcher")
}

/// Launch times recorded by this launcher (heroic-lite launches bypass Heroic's own records).
pub fn load_played() -> HashMap<String, i64> {
    let doc = read_json(&state_dir().join("played.json"));
    doc.as_object()
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_i64()?))).collect())
        .unwrap_or_default()
}

pub fn record_played(key: &str, when: i64) {
    let mut played = load_played();
    played.insert(key.to_string(), when);
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let tmp = dir.join("played.json.tmp");
    if fs::write(&tmp, serde_json::to_string_pretty(&played).unwrap_or_default()).is_ok() {
        let _ = fs::rename(&tmp, dir.join("played.json"));
    }
}

const DEFAULT_HIDDEN: &str = "\
# Games and apps hidden from the launcher, one key per line.
# Keys: steam-<appid>, legendary-<app>, gog-<id>, desktop-<.desktop file name>.
steam-250820
desktop-steam
desktop-com.heroicgameslauncher.hgl
desktop-net.davidotek.pupgui2
desktop-ALVR
desktop-ALVR Launcher
desktop-valve-vrmonitor
desktop-com.outerwildsmods.owmods_gui
";

pub fn load_hidden() -> HashSet<String> {
    let path = config_dir().join("hidden.txt");
    let text = fs::read_to_string(&path).unwrap_or_else(|_| {
        let _ = fs::create_dir_all(config_dir());
        let _ = fs::write(&path, DEFAULT_HIDDEN);
        DEFAULT_HIDDEN.to_string()
    });
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn discover() -> Vec<Game> {
    let played = load_played();
    let hidden = load_hidden();
    let mut games = Vec::new();
    steam_games(&mut games);
    heroic_games(&mut games);
    desktop_games(&mut games, &played);
    games.retain(|g| !hidden.contains(&g.key) && !g.title.is_empty());
    for game in &mut games {
        if let Some(&ours) = played.get(&game.key) {
            game.last_played = game.last_played.max(ours);
        }
    }
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601("2026-10-15T06:30:49.543Z"), Some(1792045849));
    }
}
