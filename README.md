# game-launcher

A full-screen, controller-first game library for Wayland, in the style of
Steam Big Picture. It is built for low resource use:

- Rendering uses Slint's **software renderer** only. No GPU features are
  compiled in, the process never creates a GPU context, and it doesn't load
  GL or Vulkan.
- Frames are drawn only when something changes. Measured idle CPU is 0, and a
  move costs about 21 ms of CPU for one 4K redraw.
- It runs only while it is open, and it exits as soon as a game starts.

## What it shows

The library is read on every start, so installs and uninstalls appear with no
background service.

| Source | Found from | Launched with |
|---|---|---|
| Steam | `steamapps/appmanifest_*.acf` (Proton, runtimes and redistributables are skipped) | `steam steam://rungameid/<id>` (a `~/.local/bin/steam` wrapper is preferred if present) |
| Epic / GOG | [Heroic](https://heroicgameslauncher.com/)'s installed lists | `~/.local/bin/heroic-lite launch <runner> <app>` (see below) |
| Other | `.desktop` entries in the `Game` category | `gio launch <file>` |

Posters come from Steam's own Big Picture cache (`appcache/librarycache`) or
from Heroic's store art (downloaded once with `curl`). Each poster is resized
once into `~/.cache/game-launcher/`. Entries without art get a generated title
card with their icon. A desktop entry with `Icon=steam_icon_<appid>` borrows
that Steam game's art.

"Recent" sorting combines Steam's `LastPlayed`, Heroic's play times, and
launches recorded by this launcher (`~/.local/state/game-launcher/played.json`).

**Epic/GOG launching** expects a small helper, `heroic-lite`, which is not part
of this repository. It replays the launch command Heroic logged for the game,
so the game starts without Heroic's Electron UI. If the helper is missing,
launching an Epic or GOG entry reports an error. Steam and `.desktop` games
don't need it.

## Controls

| Controller | Keyboard | Action |
|---|---|---|
| Stick / D-pad | Arrows | Move |
| South face button (A on Xbox) | Enter | Play |
| East face button (B on Xbox) / Home | Esc | Close |
| LB / RB | Shift+Tab / Tab | Category |
| North or west face button | S | Sort: Recent ↔ A–Z |
| Start | A | Full app menu (runs `~/.local/bin/hypr-app-menu` if present) |

Controllers are read directly from evdev, so your user needs read access to
`/dev/input/event*`; systemd-logind's seat ACLs usually grant this. While the
launcher is open it grabs the controllers, so a game underneath doesn't react
to navigation.

Starting `game-launcher` a second time closes the running instance, so a
single key or button binding works as a toggle. Opening it from a controller
button needs a separate daemon that runs `game-launcher`; none is included.

## Hyprland setup

A special workspace overlays the current one, so the launcher can open above
a running full-screen game without un-fullscreening it. With Hyprland's Lua
config:

```lua
hl.window_rule({
    name  = "game-launcher-overlay",
    match = { class = "^(game-launcher)$" },
    workspace  = "special:games",
    fullscreen = true,
})
hl.bind("SUPER + G", hl.dsp.exec_cmd("~/.local/bin/game-launcher"))
```

On other compositors it opens as an ordinary full-screen window.

## Configuration

`~/.config/game-launcher/hidden.txt` lists keys to hide, one per line. It is
created on first run with a few launchers and tools that aren't games. The
keys look like `steam-<appid>`, `legendary-<app>`, `gog-<id>`, or
`desktop-<file name>`. Run `game-launcher --list` to see every key.

## Build and install

```sh
cargo build --release
install -Dm755 target/release/game-launcher ~/.local/bin/game-launcher
```

Run-time tools it calls: `curl` (to fetch missing art), `gio` (for `.desktop`
games) and `notify-send` (optional).

Test hooks:

- `--windowed` opens a normal window instead of full-screen.
- `--list` prints the discovered library and exits.
- `GAME_LAUNCHER_APP_ID=<id>` changes the Wayland app id.
- `GAME_LAUNCHER_NO_GAMEPAD=1` runs without opening or grabbing controllers.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). The UI is built with
[Slint](https://slint.dev/), used under its GPLv3 option.
