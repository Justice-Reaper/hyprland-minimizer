# hyprland-minimizer

Minimize windows to the system tray on Hyprland, with **real icon resolution** and
helper commands to drive a rofi-based tray menu.

Send a window out of the way to a hidden workspace and get a clickable tray icon
(via D-Bus StatusNotifierItem) to bring it back — handy for keeping messengers,
mail or background apps alive while still receiving notifications.

## Key Features

- **Minimize to tray** — moves the focused window (or one given by address) to the
  `special:minimized` workspace and publishes a StatusNotifierItem tray icon for it.
- **Real icon resolution** — figures out the proper freedesktop icon by scanning
  `.desktop` files, with a `/proc/<pid>/cmdline` fallback. Apps that report a useless
  window class still get the right icon — e.g. themix runs as `python3 -m oomox_gui`
  and reports class `__main__.py`, yet resolves to `com.github.themix_project.Oomox`.
  **No icon cache or pacman hooks required.**
- **Clickable tray icon** — left-click restores, middle-click closes, right-click opens
  a context menu.
- **Restore where you want** — bring the window back to your current workspace, or back
  to the original workspace it was minimized from.
- **Survives bar restarts** — re-registers its icon when the StatusNotifierWatcher
  (e.g. Waybar) restarts.
- **Self-cleaning** — the per-window daemon exits on its own when the window is restored
  or closed by other means.
- **Graceful fallback** — if no tray/watcher is running, the window is put back instead
  of getting stuck hidden.
- **rofi-friendly** — `list-tray`, `tray-menu`, `tray-menu-click` and `resolve` keep all
  the D-Bus and icon logic inside the binary, so a rofi tray menu can be a tiny reader
  script (no `gdbus` or icon cache in bash).
- **Native per-app menus** — each tray item's own right-click menu (Discord, qBittorrent,
  ... and the minimizer's own Open/Close) is read straight from `com.canonical.dbusmenu`
  and walked one level at a time, submenus included — just like a real desktop tray.
- **Built-in tray watcher** — `hyprland-minimizer watch` runs a standalone
  `org.kde.StatusNotifierWatcher`, so you can drop your bar's tray module entirely and
  still have a fully working tray (apps register here; browse it via rofi).
- **Lightweight** — one small daemon per minimized window, no config files.

## Build from source

```
git clone https://github.com/Justice-Reaper/hyprland-minimizer
cd hyprland-minimizer
cargo build --release
# optional: put it on your PATH
sudo cp target/release/hyprland-minimizer /usr/local/bin/
```

## Usage

```
hyprland-minimizer [ADDRESS]                          Minimize the active window (or one by address)
hyprland-minimizer resolve <class> <pid>              Print "name|icon" resolved for a window
hyprland-minimizer list-tray                          Print "name|icon|bus|path|pid" per tray item
hyprland-minimizer tray-menu <bus> <path> [parent]    Print a tray item's menu entries (children of parent, 0=root)
hyprland-minimizer tray-menu-click <bus> <path> <id>  Trigger the menu entry <id>
hyprland-minimizer watch                              Run a StatusNotifierWatcher daemon
```

Minimize a specific window (get the address from `hyprctl clients`):

```
hyprland-minimizer 0x12345678
```

## Hyprland keybind

```
-- minimize the focused window to the tray
hl.bind("SUPER + T", hl.dsp.exec_cmd("/usr/local/bin/hyprland-minimizer"))
```

## Tray icon interactions

| Action       | Result                                                     |
|--------------|------------------------------------------------------------|
| Left click   | Restore the window to the current workspace and focus it    |
| Middle click | Close the window                                            |
| Right click  | Menu: *Open* · *Open on original workspace* · *Close* (labels include the window title and workspace id) |

## Icon resolution (`resolve`)

The window class alone is often a poor source for an icon: Electron and
`python -m <module>` apps report things like `chrome_status_icon_1` or `__main__.py`.
`hyprland-minimizer` resolves the real icon by

1. matching the class against `.desktop` `StartupWMClass` values and filenames, then
2. falling back to the process command line (`/proc/<pid>/cmdline`) to recover the real
   module/binary name.

Run it standalone to see what a window would resolve to:

```
$ hyprland-minimizer resolve __main__.py 22343
Themix/Oomox theme designer|com.github.themix_project.Oomox
```

## rofi tray menu

Because `list-tray`, `tray-menu` and `tray-menu-click` do all the D-Bus work, a rofi tray
menu becomes a thin reader — no `gdbus` parsing or desktop cache in the shell.

First, list the tray items:

```
$ hyprland-minimizer list-tray
Discord|discord|:1.407|/StatusNotifierItem|26956
qBittorrent|qbittorrent|:1.431|/StatusNotifierItem|29237
Flameshot|org.flameshot.Flameshot|:1.293|/StatusNotifierItem|8433
```

Then read the chosen item's own menu with `tray-menu <bus> <path> [parent]` (`parent` is
the entry id to descend into, `0` or omitted = root). Each line is `label`, `id` and
`kind` separated by the ASCII Unit Separator (`0x1f`), where `kind` is `submenu` (descend
with another `tray-menu` call) or `item` (clickable). Separators, hidden and disabled
entries are filtered out, so the reader just renders what it gets:

```
$ hyprland-minimizer tray-menu :1.407 /StatusNotifierItem 0
Mute<US>12<US>item
Deafen<US>14<US>item
Settings<US>20<US>submenu
Quit<US>30<US>item
```

Finally, trigger the selected entry with `tray-menu-click <bus> <path> <id>` and the app
runs that action itself. This walks each item's native menu one level at a time —
including the minimizer's own *Open* / *Open on original workspace* / *Close* entries —
exactly like a real desktop tray.

## Standalone tray watcher (no bar tray module)

A system tray needs a *watcher* (`org.kde.StatusNotifierWatcher`) for apps to register
their icons. Normally that's your bar's tray module (Waybar's `tray`). If you'd rather
not run a bar tray module at all, `hyprland-minimizer` can be the watcher itself:

```
hl.exec_cmd("hyprland-minimizer watch")   -- run at Hyprland startup
```

Then remove the tray module from your bar. Apps (Discord, qBittorrent, ...) and your
minimized windows register with this watcher, and you browse them via rofi (`list-tray`).
It reports `IsStatusNotifierHostRegistered = true` so Qt/Electron apps publish their
icons, and drops items whose process leaves the bus. Only one watcher can own the name,
so don't run it alongside a bar tray module.

## How it works

Minimizing moves the window to the `special:minimized` workspace and starts a small
per-window daemon that publishes a StatusNotifierItem over D-Bus. A StatusNotifier host
(your bar's tray module, e.g. Waybar) renders the icon; clicking it talks back to the
daemon, which restores or closes the window. The daemon also watches for the window
being restored or closed externally and exits on its own.

## Requirements

- **Hyprland 0.55+** (Lua dispatch API)
- A **StatusNotifier watcher** — either your bar's `tray` module (e.g. Waybar) or the
  built-in `hyprland-minimizer watch` daemon
- A **D-Bus session bus**
- **Rust** (to build)

## Acknowledgements

Fork of [hyprland-minimizer](https://github.com/Simon-Martens/hyprland-minimizer),
extended with self-contained icon resolution and the
`resolve` / `list-tray` / `tray-menu` / `tray-menu-click` helper commands.
