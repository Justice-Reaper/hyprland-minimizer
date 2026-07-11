# Hyprland Minimizer

Minimize any window to the system tray on Hyprland — it parks the window on a hidden
workspace and gives you a clickable tray icon to bring it back. Ships helper commands so a
small rofi script can be your whole tray, no bar tray module needed

## Features

- **Minimize to tray** — hide the focused window (or one by address) and get a tray icon

- **Real icons** — resolves the freedesktop icon from `.desktop` files (`/proc` fallback),
  so even odd window classes (`python -m oomox_gui` → `__main__.py`) get the right one

- **Clicks & menus** — left restores, middle closes, right opens the item's own menu

- **Standalone watcher** — `hyprland-minimizer watch` replaces your bar's tray module

- **Thin rofi reader** — all D-Bus/icon work stays in the binary (`list-tray` /
  `tray-menu` / `tray-menu-click`)

## Install

From the AUR:

```
paru -S hyprland-minimizer-git
```

From source:

```
cargo build --release
sudo cp target/release/hyprland-minimizer /usr/local/bin/
```

## Usage

```
hyprland-minimizer [ADDRESS]                           Minimize the active window (or one by address)
hyprland-minimizer list-tray [mine|native]             List tray items; optionally only yours / only app trays
hyprland-minimizer tray-menu <bus> <path> [id]         List a tray item's menu entries (id 0 = root)
hyprland-minimizer tray-menu-click <bus> <path> <id>   Trigger a menu entry
hyprland-minimizer resolve <class> <pid>               Debug: print "name|icon" for a window
hyprland-minimizer watch                               Run a StatusNotifierWatcher daemon
```

```
hl.bind("SUPER + T", hl.dsp.exec_cmd("hyprland-minimizer"))   -- minimize keybind
```

| Left click | Middle click | Right click |
|---|---|---|
| Restore & focus | Close | Menu: *Open* · *Open on original workspace* · *Close* |

## How it works

Minimizing parks the window on `special:minimized` and runs a tiny per-window daemon
serving a StatusNotifierItem; clicking the icon restores or closes the window. `list-tray`
prints one `0x1f`-separated `name`/`icon`/`bus`/`path`/`pid` line per item, with name and
icon read from each item's own SNI (icon = its name mapped to a `.desktop` `Icon=`, else
`application-x-addon`)

## Requirements

Hyprland 0.55+ · a StatusNotifier watcher (your bar's tray, or `hyprland-minimizer watch`)
· a D-Bus session bus
