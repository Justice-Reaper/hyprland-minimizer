//! Minimize-to-tray utility for Hyprland: hides a window to a special workspace
//! and publishes a StatusNotifierItem for it, plus helper subcommands that let a
//! rofi script drive the tray (list items, read menus, resolve icons).

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures_util::stream::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{interval, Duration};
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{dbus_interface, ConnectionBuilder, Proxy};

// --- Command-Line Interface Definition ---
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The address of the window to minimize. If not provided, minimizes the active window.
    window_address: Option<String>,
}

// --- Hyprland Data Structures ---
// These structs are used to deserialize the JSON output from `hyprctl`.

#[derive(Deserialize, Debug, Clone)]
struct Workspace {
    id: i32,
}

#[derive(Deserialize, Debug, Clone)]
struct WindowInfo {
    address: String,
    workspace: Workspace,
    title: String,
    class: String,
    #[serde(default)]
    pid: i32,
    /// Resolved freedesktop icon name. Filled in after deserialization (not from
    /// hyprctl); falls back to `class` when nothing matches.
    #[serde(default)]
    icon: String,
    /// Resolved app display name (the desktop entry's Name). Filled in like
    /// `icon`; falls back to the capitalized class. This is what our tray item
    /// advertises as its label, so it reads "Vesktop" instead of the raw window
    /// title.
    #[serde(default)]
    app_name: String,
}

// --- Hyprland Interaction Functions ---

/// Executes a hyprctl command and returns the parsed JSON output.
fn hyprctl<T: for<'de> Deserialize<'de>>(command: &str) -> Result<T> {
    let output = Command::new("hyprctl")
        .arg("-j")
        .arg(command)
        .output()
        .with_context(|| format!("Failed to execute hyprctl command: {}", command))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("hyprctl command '{}' failed: {}", command, stderr);
    }

    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("Failed to parse JSON from hyprctl command: {}", command))
}

/// Executes a Lua expression via `hyprctl eval`.
/// Hyprland 0.55+ with a Lua config no longer accepts the classic
/// `hyprctl dispatch <name>` syntax, so all dispatches go through eval.
fn hyprctl_dispatch(lua: &str) -> Result<()> {
    let status = Command::new("hyprctl")
        .arg("eval")
        .arg(lua)
        .status()
        .with_context(|| format!("Failed to execute hyprctl eval: {}", lua))?;

    if !status.success() {
        anyhow::bail!("hyprctl eval '{}' failed", lua);
    }
    Ok(())
}

/// Moves a window (by address) to a workspace.
/// `silent = true` keeps the current workspace active (old `movetoworkspacesilent`);
/// `silent = false` switches to the target workspace (old `movetoworkspace`).
fn move_to_workspace(workspace: &str, address: &str, silent: bool) -> Result<()> {
    hyprctl_dispatch(&format!(
        "hl.dispatch(hl.dsp.window.move({{workspace = '{}', follow = {}, window = 'address:{}'}}))",
        workspace, !silent, address
    ))
}

/// Focuses a window by address (old `focuswindow`).
fn focus_window(address: &str) -> Result<()> {
    hyprctl_dispatch(&format!(
        "hl.dispatch(hl.dsp.focus({{window = 'address:{}'}}))",
        address
    ))
}

/// Closes a window by address (old `closewindow`).
fn close_window(address: &str) -> Result<()> {
    hyprctl_dispatch(&format!(
        "hl.dispatch(hl.dsp.window.close({{window = 'address:{}'}}))",
        address
    ))
}

/// Finds a window by its address from the list of all clients.
fn get_window_by_address(address: &str) -> Result<WindowInfo> {
    let clients: Vec<WindowInfo> =
        hyprctl("clients").context("Failed to get client list from Hyprland.")?;
    clients
        .into_iter()
        .find(|c| c.address == address)
        .ok_or_else(|| anyhow!("Could not find a window with address '{}'", address))
}

// --- Icon resolution ---
// Resolves a real freedesktop icon name from a window's class (with the PID's
// command line as a fallback), scanning .desktop files directly. This is what
// makes generic-class apps show the right tray icon instead of a broken one
// (e.g. themix runs as `python3 -m oomox_gui` and reports class "__main__.py",
// which is not an icon; its real icon is "com.github.themix_project.Oomox").

/// Substring of `s` up to (not including) the first non-alphanumeric character.
fn alnum_prefix(s: &str) -> &str {
    match s.find(|c: char| !c.is_ascii_alphanumeric()) {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Scans .desktop files into a sorted list of (key, name, icon) tuples. Keys are
/// the lowercased StartupWMClass and the lowercased file stem.
fn desktop_entries() -> Vec<(String, String, String)> {
    let mut dirs = vec!["/usr/share/applications".to_string()];
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{}/.local/share/applications", home));
    }

    let mut entries: Vec<(String, String, String)> = Vec::new();
    for dir in &dirs {
        let rd = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for de in rd.flatten() {
            let path = de.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut name = "";
            let mut icon = "";
            let mut wmclass = "";
            for line in content.lines() {
                if name.is_empty() {
                    if let Some(v) = line.strip_prefix("Name=") {
                        name = v;
                    }
                }
                if icon.is_empty() {
                    if let Some(v) = line.strip_prefix("Icon=") {
                        icon = v;
                    }
                }
                if wmclass.is_empty() {
                    if let Some(v) = line.strip_prefix("StartupWMClass=") {
                        wmclass = v;
                    }
                }
            }
            // Trim so a stray leading/trailing space in a value can't break an
            // exact Name match or a theme icon lookup.
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let name = name.to_string();
            let icon = icon.trim().to_string();
            let wmclass = wmclass.trim();
            if !wmclass.is_empty() {
                entries.push((wmclass.to_lowercase(), name.clone(), icon.clone()));
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                entries.push((stem.to_lowercase(), name, icon));
            }
        }
    }
    entries.sort();
    entries
}

/// Resolves (name, icon) for a window from its class, with the PID's command line
/// as a fallback (handles `python -m <module>`, electron, etc.). Returns None if
/// nothing matches.
fn resolve_entry(class: &str, pid: i32) -> Option<(String, String)> {
    let entries = desktop_entries();

    let exact = |q: &str| -> Option<(String, String)> {
        entries
            .iter()
            .find(|(k, _, ic)| k == q && !ic.is_empty())
            .map(|(_, n, ic)| (n.clone(), ic.clone()))
    };
    let prefixed = |q: &str| -> Option<(String, String)> {
        entries
            .iter()
            .find(|(k, _, ic)| k.starts_with(q) && !ic.is_empty())
            .map(|(_, n, ic)| (n.clone(), ic.clone()))
    };
    let suffixed = |q: &str| -> Option<(String, String)> {
        let needle = format!(".{}", q);
        entries
            .iter()
            .find(|(k, _, ic)| k.ends_with(&needle) && !ic.is_empty())
            .map(|(_, n, ic)| (n.clone(), ic.clone()))
    };

    let key = class.to_lowercase();

    // 1. exact match on the window class
    if let Some(r) = exact(&key) {
        return Some(r);
    }
    // 2. prefix of the class
    let prefix = alnum_prefix(&key);
    if !prefix.is_empty() {
        if let Some(r) = prefixed(prefix) {
            return Some(r);
        }
    }
    // 3. fall back to the process command line (handles `python -m <module>`, electron, etc.)
    if pid > 0 {
        if let Ok(raw) = fs::read(format!("/proc/{}/cmdline", pid)) {
            for arg in raw.split(|&b| b == 0) {
                if arg.is_empty() {
                    continue;
                }
                let arg = String::from_utf8_lossy(arg);
                if arg.starts_with('-') {
                    continue;
                }
                let base = arg.rsplit('/').next().unwrap_or(&arg).to_lowercase();
                if base.is_empty()
                    || base.starts_with("python")
                    || base.starts_with("electron")
                    || matches!(
                        base.as_str(),
                        "java" | "node" | "sh" | "bash" | "dash" | "env" | "perl" | "ruby"
                    )
                {
                    continue;
                }
                if let Some(r) = exact(&base) {
                    return Some(r);
                }
                let bprefix = alnum_prefix(&base);
                if !bprefix.is_empty() {
                    if let Some(r) = prefixed(bprefix) {
                        return Some(r);
                    }
                    if let Some(r) = suffixed(bprefix) {
                        return Some(r);
                    }
                }
            }
        }
    }
    None
}

/// Resolves (name, icon) for a window, falling back to (capitalized class, class)
/// when no desktop entry matches.
fn resolve_name_icon(class: &str, pid: i32) -> (String, String) {
    resolve_entry(class, pid).unwrap_or_else(|| (capitalize(class), class.to_string()))
}

/// Maps a human app name (e.g. a tray item's advertised "Discord"/"Vesktop") to a
/// themed icon by matching the `Name=` field of the installed .desktop entries
/// (passed in so the caller scans the database once for the whole tray). Pure
/// lookup on data the app hands us — no process inspection — so a tray icon that
/// only ships a generic Id can still get the proper themed icon. None if nothing
/// matches.
fn icon_for_app_name(entries: &[(String, String, String)], app_name: &str) -> Option<String> {
    let q = app_name.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|(_, name, icon)| name.to_lowercase() == q && !icon.is_empty())
        .map(|(_, _, icon)| icon.clone())
}

// --- System tray reading (for the rofi tray menu) ---

/// Uppercases the first character (mirrors bash `${var^}`).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Prints every registered StatusNotifierItem as `name<US>icon<US>bus<US>path<US>pid`
/// (fields joined by the ASCII Unit Separator, 0x1F).
///
/// `filter` picks which items to print: "mine"/"minimized" = the windows we
/// minimized (their bus name is our `…minimizer.pN`), "native"/"others" = real
/// app trays, anything else = all of them.
///
/// Everything is driven by the item's own SNI properties, so our minimized
/// windows and native app trays go through the same path (no /proc, no window
/// class guessing):
///   name  = ToolTip title -> Title -> capitalized Id
///   icon  = name mapped to a .desktop Icon -> application-x-addon
/// The themed .desktop icon keeps the tray consistent with the theme; the
/// fallback is an explicit generic icon that exists in the theme (rofi won't
/// apply its own fallback-icon to an unresolved explicit dmenu icon).
async fn list_tray(filter: &str) -> Result<()> {
    let conn = zbus::Connection::session().await?;

    let watcher: Proxy<'_> = zbus::ProxyBuilder::new_bare(&conn)
        .interface("org.kde.StatusNotifierWatcher")?
        .path("/StatusNotifierWatcher")?
        .destination("org.kde.StatusNotifierWatcher")?
        .build()
        .await?;

    let items: Vec<String> = watcher
        .get_property("RegisteredStatusNotifierItems")
        .await
        .unwrap_or_default();

    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;

    // Scan the .desktop database once for the whole tray, not once per item.
    let entries = desktop_entries();

    for item in items {
        let (bus, path) = match item.split_once('/') {
            Some((b, p)) => (b.to_string(), format!("/{}", p)),
            None => (item.clone(), "/StatusNotifierItem".to_string()),
        };

        // Our own minimized-window items own a `…minimizer.pN` bus name; native
        // app trays don't. Let the caller list one group or the other.
        let is_mine = bus.starts_with("org.kde.StatusNotifierItem.minimizer.");
        let keep = match filter {
            "mine" | "minimized" => is_mine,
            "native" | "others" => !is_mine,
            _ => true,
        };
        if !keep {
            continue;
        }

        // Read each item under a deadline so one frozen tray app (alive but not
        // answering D-Bus) can't hang the whole list.
        let line = tokio::time::timeout(Duration::from_millis(500), async {
            let proxy: Proxy<'_> = zbus::ProxyBuilder::new_bare(&conn)
                .interface("org.kde.StatusNotifierItem")?
                .destination(bus.as_str())?
                .path(path.as_str())?
                .build()
                .await?;

            let id: String = proxy.get_property("Id").await.unwrap_or_default();
            let title: String = proxy.get_property("Title").await.unwrap_or_default();
            let tooltip: ToolTip = proxy.get_property("ToolTip").await.unwrap_or_default();

            let pid = match zbus::names::BusName::try_from(bus.as_str()) {
                Ok(bn) => dbus.get_connection_unix_process_id(bn).await.unwrap_or(0) as i32,
                Err(_) => 0,
            };

            // name: the tooltip heading (apps put a clean app name here, e.g.
            // "Flameshot"/"Vesktop"), then the Title property, then the Id.
            let name = if !tooltip.2.trim().is_empty() {
                tooltip.2
            } else if !title.trim().is_empty() {
                title
            } else {
                capitalize(&id)
            };

            // icon: match the name the item advertises against a .desktop `Name=`
            // (so it gets the themed app icon that matches the rest of the theme),
            // else a generic icon. rofi does NOT apply its `fallback-icon` to an
            // unresolved explicit dmenu icon, so the fallback names one that exists
            // in the theme.
            let icon = icon_for_app_name(&entries, &name)
                .unwrap_or_else(|| "application-x-addon".to_string());

            // Fields are separated by the ASCII Unit Separator (0x1F), never '|',
            // because a name (a window Title / tooltip) can contain any character.
            println!("{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}", name, icon, bus, path, pid);
            Ok::<(), anyhow::Error>(())
        })
        .await;

        let _ = line; // skip items that error out or don't respond in time
    }

    Ok(())
}

// --- Native context menu reading (com.canonical.dbusmenu) ---
// Lets tray.sh show each item's *own* menu in rofi, one level at a time, exactly
// like a real tray (right-click in Windows/Plasma). Works for both our minimizer
// items (which serve their own Open/Close menu) and native apps (Discord & co.),
// so the same path handles everything.

/// A dbusmenu node from GetLayout: (id, properties, children-as-variants).
type MenuLayoutNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// Reads the dbusmenu object path a StatusNotifierItem points at (its Menu property).
async fn menu_object_path(conn: &zbus::Connection, bus: &str, path: &str) -> Result<String> {
    let sni: Proxy<'_> = zbus::ProxyBuilder::new_bare(conn)
        .interface("org.kde.StatusNotifierItem")?
        .destination(bus)?
        .path(path)?
        .build()
        .await?;
    let menu: zbus::zvariant::OwnedObjectPath = sni.get_property("Menu").await?;
    Ok(menu.into_inner().to_string())
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str, default: bool) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(default)
}

/// Strips GTK ('_') and Qt ('&') mnemonic markers from a menu label.
fn strip_mnemonics(label: &str) -> String {
    // Use \u{1} as a sentinel for the doubled (literal) marker while the single
    // marker is stripped, then restore it. \u{1} never appears in a real label,
    // and it is fully consumed before being reused for the next marker.
    label
        .replace("&&", "\u{1}")
        .replace('&', "")
        .replace('\u{1}', "&")
        .replace("__", "\u{1}")
        .replace('_', "")
        .replace('\u{1}', "_")
}

/// Prints the immediate children of `parent_id` (0 = root), one per line, as
/// `label<US>id<US>kind` where <US> is the ASCII Unit Separator (0x1F) so a literal
/// separator can never appear inside a label. kind is "submenu" (navigate into it)
/// or "item" (clickable). Separators, hidden and disabled entries are filtered out.
async fn tray_menu(bus: &str, path: &str, parent_id: i32) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let mpath = menu_object_path(&conn, bus, path).await?;
    if mpath == "/" {
        return Ok(()); // item exposes no menu
    }

    let menu: Proxy<'_> = zbus::ProxyBuilder::new_bare(&conn)
        .interface("com.canonical.dbusmenu")?
        .destination(bus)?
        .path(mpath.as_str())?
        .build()
        .await?;

    // Some apps populate a submenu lazily on AboutToShow; ignore failures.
    let _ = menu.call_method("AboutToShow", &(parent_id,)).await;

    // depth 1 -> the parent node plus its immediate children
    let reply = menu
        .call_method("GetLayout", &(parent_id, 1i32, Vec::<String>::new()))
        .await?;
    let (_revision, root): (u32, MenuLayoutNode) = reply.body()?;
    let (_id, _props, children) = root;

    for child in children {
        let (cid, cprops, _) = match MenuLayoutNode::try_from(child) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if prop_string(&cprops, "type").as_deref() == Some("separator") {
            continue;
        }
        if !prop_bool(&cprops, "visible", true) || !prop_bool(&cprops, "enabled", true) {
            continue;
        }
        let label = match prop_string(&cprops, "label") {
            Some(l) => strip_mnemonics(&l),
            None => continue,
        };
        if label.trim().is_empty() {
            continue;
        }
        let kind = if prop_string(&cprops, "children-display").as_deref() == Some("submenu") {
            "submenu"
        } else {
            "item"
        };
        println!("{}\u{1f}{}\u{1f}{}", label, cid, kind);
    }
    Ok(())
}

/// Triggers a menu entry by id (the app then runs that action itself).
async fn tray_menu_click(bus: &str, path: &str, id: i32) -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let mpath = menu_object_path(&conn, bus, path).await?;
    if mpath == "/" {
        return Ok(());
    }
    let menu: Proxy<'_> = zbus::ProxyBuilder::new_bare(&conn)
        .interface("com.canonical.dbusmenu")?
        .destination(bus)?
        .path(mpath.as_str())?
        .build()
        .await?;
    menu.call_method("Event", &(id, "clicked", Value::from(""), 0u32))
        .await?;
    Ok(())
}

// --- StatusNotifierWatcher daemon ---
// Lets hyprland-minimizer be the tray "watcher" itself, so Waybar's tray module is
// not needed. Apps and our own minimized-window items register here; tray.sh reads
// the list. IsStatusNotifierHostRegistered is reported true so Qt/Electron apps
// (Discord, qBittorrent, ...) actually publish their icons.

struct Watcher {
    items: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[dbus_interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::MessageHeader<'_>,
        #[zbus(signal_context)] ctxt: zbus::SignalContext<'_>,
    ) {
        let sender = header
            .sender()
            .ok()
            .flatten()
            .map(|s| s.to_string())
            .unwrap_or_default();
        // The argument is either a bus name or an object path (KDE spec).
        let entry = if service.starts_with('/') {
            format!("{}{}", sender, service)
        } else {
            format!("{}/StatusNotifierItem", service)
        };
        {
            let mut items = self.items.lock().unwrap();
            if items.contains(&entry) {
                return;
            }
            items.push(entry.clone());
        }
        let _ = Watcher::status_notifier_item_registered(&ctxt, &entry).await;
    }

    async fn register_status_notifier_host(
        &self,
        _service: String,
        #[zbus(signal_context)] ctxt: zbus::SignalContext<'_>,
    ) {
        let _ = Watcher::status_notifier_host_registered(&ctxt).await;
    }

    #[dbus_interface(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[dbus_interface(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[dbus_interface(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[dbus_interface(signal)]
    async fn status_notifier_item_registered(
        ctxt: &zbus::SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[dbus_interface(signal)]
    async fn status_notifier_item_unregistered(
        ctxt: &zbus::SignalContext<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[dbus_interface(signal)]
    async fn status_notifier_host_registered(ctxt: &zbus::SignalContext<'_>) -> zbus::Result<()>;

    #[dbus_interface(signal)]
    async fn status_notifier_host_unregistered(
        ctxt: &zbus::SignalContext<'_>,
    ) -> zbus::Result<()>;
}

/// Runs a StatusNotifierWatcher daemon — a drop-in replacement for Waybar's tray
/// module. Owns org.kde.StatusNotifierWatcher and tracks registered items.
async fn watch() -> Result<()> {
    let items = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let watcher = Watcher {
        items: items.clone(),
    };

    let conn = ConnectionBuilder::session()?
        .name("org.kde.StatusNotifierWatcher")?
        .serve_at("/StatusNotifierWatcher", watcher)?
        .build()
        .await
        .context(
            "Could not own org.kde.StatusNotifierWatcher \
             (is another tray/watcher running, e.g. Waybar's tray module?)",
        )?;

    println!("StatusNotifierWatcher running.");

    // Drop items whose owning process disappears from the bus.
    let ctxt = zbus::SignalContext::new(&conn, "/StatusNotifierWatcher")?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut changes = dbus.receive_name_owner_changed().await?;

    while let Some(signal) = changes.next().await {
        let args = match signal.args() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if args.new_owner().is_some() {
            continue; // name acquired, not lost
        }
        let gone = args.name().to_string();

        let mut removed = Vec::new();
        {
            let mut list = items.lock().unwrap();
            list.retain(|entry| {
                let bus = entry.split('/').next().unwrap_or("");
                if bus == gone {
                    removed.push(entry.clone());
                    false
                } else {
                    true
                }
            });
        }
        for entry in removed {
            let _ = Watcher::status_notifier_item_unregistered(&ctxt, &entry).await;
        }
    }

    Ok(())
}

// --- D-Bus protocol type aliases (shapes are dictated by the dbusmenu / StatusNotifierItem specs) ---

/// A dbusmenu node: (id, properties, children).
type MenuNode<'a> = (i32, HashMap<String, Value<'a>>, Vec<Value<'a>>);
/// Properties for a single dbusmenu item: (id, properties).
type MenuItemProps<'a> = (i32, HashMap<String, Value<'a>>);
/// StatusNotifierItem tooltip: (icon_name, icon_pixmap, title, description).
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

// --- D-Bus Menu Implementation ---

struct DbusMenu {
    window_info: WindowInfo,
    exit_notify: Arc<Notify>,
}

#[dbus_interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    /// Returns the menu layout.
    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, MenuNode<'_>) {
        // Item ID 1: Open on current workspace
        let mut open_props = HashMap::new();
        open_props.insert("type".to_string(), Value::from("standard"));
        open_props.insert(
            "label".to_string(),
            Value::from(format!("Open {}", self.window_info.title)),
        );
        let open_item = Value::from((1i32, open_props, Vec::<Value>::new()));

        // Item ID 2: Open on original workspace
        let mut last_ws_props = HashMap::new();
        last_ws_props.insert("type".to_string(), Value::from("standard"));
        last_ws_props.insert(
            "label".to_string(),
            Value::from(format!(
                "Open on original workspace ({})",
                self.window_info.workspace.id
            )),
        );
        let last_ws_item = Value::from((2i32, last_ws_props, Vec::<Value>::new()));

        // Item ID 3: Close the window
        let mut close_props = HashMap::new();
        close_props.insert("type".to_string(), Value::from("standard"));
        close_props.insert(
            "label".to_string(),
            Value::from(format!("Close {}", self.window_info.title)),
        );
        let close_item = Value::from((3i32, close_props, Vec::<Value>::new()));

        let mut root_props = HashMap::new();
        root_props.insert("children-display".to_string(), Value::from("submenu"));

        let root_layout = (
            0i32, // Root node ID is always 0
            root_props,
            vec![open_item, last_ws_item, close_item],
        );

        let revision = 2u32;
        (revision, root_layout)
    }

    /// Returns the properties for a group of menu items.
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<MenuItemProps<'_>> {
        let mut result = Vec::new();
        for id in ids {
            let mut props = HashMap::new();
            let label = match id {
                1 => format!("Open {}", self.window_info.title),
                2 => format!(
                    "Open on original workspace ({})",
                    self.window_info.workspace.id
                ),
                3 => format!("Close {}", self.window_info.title),
                _ => continue,
            };
            props.insert("label".to_string(), Value::from(label));
            props.insert("enabled".to_string(), Value::from(true));
            props.insert("visible".to_string(), Value::from(true));
            props.insert("type".to_string(), Value::from("standard"));
            result.push((id, props));
        }
        result
    }

    /// Handles a batch of click events. This is called by Waybar instead of the singular `Event`.
    fn event_group(&self, events: Vec<(i32, String, Value<'_>, u32)>) {
        for (id, event_id, data, timestamp) in events {
            self.event(id, &event_id, data, timestamp);
        }
    }

    /// Handles a single click event on a menu item.
    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" {
            let res = match id {
                1 => {
                    match hyprctl::<Workspace>("activeworkspace") {
                        Ok(active_workspace) => move_to_workspace(
                            &active_workspace.id.to_string(),
                            &self.window_info.address,
                            false,
                        )
                        .and_then(|_| focus_window(&self.window_info.address)),
                        Err(e) => {
                            eprintln!("[Error] Failed to get active workspace: {}", e);
                            Err(e)
                        }
                    }
                }
                2 => {
                    move_to_workspace(
                        &self.window_info.workspace.id.to_string(),
                        &self.window_info.address,
                        false,
                    )
                    .and_then(|_| focus_window(&self.window_info.address))
                }
                3 => {
                    close_window(&self.window_info.address)
                }
                _ => {
                    return;
                }
            };

            if let Err(e) = res {
                eprintln!(
                    "[Error] Failed to execute hyprctl eval from menu: {}",
                    e
                );
            }

            self.exit_notify.notify_one();
        }
    }

    /// Handles a batch of "about to show" requests.
    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (vec![], vec![])
    }

    /// Kept for compatibility.
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    #[dbus_interface(property)]
    fn version(&self) -> u32 {
        3
    }

    #[dbus_interface(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[dbus_interface(property)]
    fn status(&self) -> &str {
        "normal"
    }
}

// --- Status Notifier Item (Tray Icon) Implementation ---

struct StatusNotifierItem {
    window_info: WindowInfo,
    exit_notify: Arc<Notify>,
}

#[dbus_interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    // --- Properties ---
    #[dbus_interface(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[dbus_interface(property)]
    fn id(&self) -> &str {
        &self.window_info.icon
    }

    // Intentional: the SNI `Title` is the app label, which we set to the resolved
    // app name (so the tray reads "Vesktop", not the raw window title, which lives
    // in the tooltip body). Not a misnamed getter.
    #[allow(clippy::misnamed_getters)]
    #[dbus_interface(property)]
    fn title(&self) -> &str {
        &self.window_info.app_name
    }

    #[dbus_interface(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[dbus_interface(property)]
    fn icon_name(&self) -> &str {
        &self.window_info.icon
    }

    #[dbus_interface(property)]
    fn tool_tip(&self) -> ToolTip {
        (
            String::new(),
            Vec::new(),
            self.window_info.app_name.clone(), // heading: the app name ("Vesktop")
            self.window_info.title.clone(),    // body: the window title
        )
    }

    #[dbus_interface(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[dbus_interface(property)]
    fn menu(&self) -> ObjectPath<'_> {
        ObjectPath::try_from("/Menu").unwrap()
    }

    // --- Methods ---
    fn activate(&self, _x: i32, _y: i32) {
        if let Ok(active_workspace) = hyprctl::<Workspace>("activeworkspace") {
            if let Err(e) = move_to_workspace(
                &active_workspace.id.to_string(),
                &self.window_info.address,
                false,
            )
            .and_then(|_| focus_window(&self.window_info.address)) {
                eprintln!("[Error] Failed to execute activate action: {}", e);
            }
        } else {
            eprintln!("[Error] Failed to get active workspace");
        }
        self.exit_notify.notify_one();
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        if let Err(e) =
            close_window(&self.window_info.address)
        {
            eprintln!("[Error] Failed to execute secondary_activate action: {}", e);
        }
        self.exit_notify.notify_one();
    }
}

// --- Main Application Logic ---

#[tokio::main]
async fn main() -> Result<()> {
    // Helper subcommands used by tray.sh so it stays a pure reader:
    //   resolve <class> <pid>          -> prints "name|icon"
    //   list-tray [mine|native]        -> prints "name<US>icon<US>bus<US>path<US>pid" (0x1F); filter to our items / real app trays
    //   tray-menu <bus> <path> [pid]   -> prints the children of <pid> (0=root), one per line
    //   tray-menu-click <bus> <path> <id> -> triggers the menu entry <id>
    let raw: Vec<String> = std::env::args().collect();
    match raw.get(1).map(String::as_str) {
        Some("resolve") => {
            let class = raw.get(2).map(String::as_str).unwrap_or("");
            let pid = raw.get(3).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            if let Some((name, icon)) = resolve_entry(class, pid) {
                println!("{}|{}", name, icon);
            }
            return Ok(());
        }
        Some("list-tray") => {
            let filter = raw.get(2).map(String::as_str).unwrap_or("");
            list_tray(filter).await?;
            return Ok(());
        }
        Some("tray-menu") => {
            let bus = raw.get(2).map(String::as_str).unwrap_or("");
            let path = raw.get(3).map(String::as_str).unwrap_or("");
            let parent = raw.get(4).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            tray_menu(bus, path, parent).await?;
            return Ok(());
        }
        Some("tray-menu-click") => {
            let bus = raw.get(2).map(String::as_str).unwrap_or("");
            let path = raw.get(3).map(String::as_str).unwrap_or("");
            let id = raw.get(4).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
            tray_menu_click(bus, path, id).await?;
            return Ok(());
        }
        Some("watch") => {
            watch().await?;
            return Ok(());
        }
        _ => {}
    }

    let args = Args::parse();

    // 1. Get window info based on CLI arguments
    let mut window_info = if let Some(address) = args.window_address {
        get_window_by_address(&address)?
    } else {
        hyprctl("activewindow").context("Failed to get active window. Is a window focused?")?
    };

    println!(
        "Minimizing window: '{}' ({}) from workspace {}",
        window_info.title, window_info.class, window_info.workspace.id
    );

    if window_info.class.is_empty() {
        // Fallback to title if class is empty, for better icon matching
        window_info.class = window_info.title.clone();
    }

    // Resolve a real freedesktop name + icon so the tray shows a proper label and
    // icon. The raw window class is often useless (e.g. "__main__.py" for python).
    let (app_name, icon) = resolve_name_icon(&window_info.class, window_info.pid);
    window_info.app_name = app_name;
    window_info.icon = icon;

    // 2. Move the window to the special "minimized" workspace
    move_to_workspace("special:minimized", &window_info.address, true)?;

    // 3. Set up the D-Bus services
    let exit_notify = Arc::new(Notify::new());

    let notifier_item = StatusNotifierItem {
        window_info: window_info.clone(),
        exit_notify: Arc::clone(&exit_notify),
    };

    let dbus_menu = DbusMenu {
        window_info: window_info.clone(),
        exit_notify: Arc::clone(&exit_notify),
    };

    let bus_name = format!(
        "org.kde.StatusNotifierItem.minimizer.p{}",
        std::process::id()
    );

    let connection = ConnectionBuilder::session()?
        .name(bus_name.as_str())?
        .serve_at("/StatusNotifierItem", notifier_item)?
        .serve_at("/Menu", dbus_menu)?
        .build()
        .await?;

    // Create an Arc of the connection to share with the watcher task.
    let arc_conn = Arc::new(connection);

    // 4. Initial registration with the StatusNotifierWatcher
    let initial_registration_result = async {
        let watcher_proxy: Proxy<'_> = zbus::ProxyBuilder::new_bare(&arc_conn)
            .interface("org.kde.StatusNotifierWatcher")?
            .path("/StatusNotifierWatcher")?
            .destination("org.kde.StatusNotifierWatcher")?
            .build()
            .await?;
        watcher_proxy
            .call_method("RegisterStatusNotifierItem", &(bus_name.as_str(),))
            .await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(e) = initial_registration_result {
        eprintln!("Could not register with StatusNotifierWatcher: {}", e);
        eprintln!("Is a tray like Waybar running?");
        let _ = move_to_workspace(
            &window_info.workspace.id.to_string(),
            &window_info.address,
            false,
        );
        anyhow::bail!("Failed to register tray icon.");
    }
    println!("Registration successful.");

    // Re-register the icon if the StatusNotifierWatcher (e.g. Waybar) restarts.
    let conn_clone_watcher = Arc::clone(&arc_conn);
    let bus_name_clone = bus_name.clone();
    tokio::spawn(async move {
        let dbus_proxy = match zbus::fdo::DBusProxy::new(&conn_clone_watcher).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[Watcher] Failed to connect to D-Bus proxy: {}", e);
                return;
            }
        };

        let mut owner_changes = match dbus_proxy.receive_name_owner_changed().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[Watcher] Failed to listen for owner changes: {}", e);
                return;
            }
        };

        while let Some(signal) = owner_changes.next().await {
            if let Ok(args) = signal.args() {
                if args.name() == "org.kde.StatusNotifierWatcher" && args.new_owner().is_some() {
                    let re_register_result = async {
                        // Give the watcher a moment to get ready
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let watcher_proxy: Proxy<'_> =
                            zbus::ProxyBuilder::new_bare(&conn_clone_watcher)
                                .interface("org.kde.StatusNotifierWatcher")?
                                .path("/StatusNotifierWatcher")?
                                .destination("org.kde.StatusNotifierWatcher")?
                                .build()
                                .await?;
                        watcher_proxy
                            .call_method("RegisterStatusNotifierItem", &(bus_name_clone.as_str(),))
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;

                    if let Err(e) = re_register_result {
                        eprintln!("[Watcher] Failed to re-register icon: {}", e);
                    }
                }
            }
        }
    });

    // 5. Start a background check to see if the window is closed or moved
    let window_address = window_info.address.clone();
    let check_task_exit_notify = Arc::clone(&exit_notify);
    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            match hyprctl::<Vec<WindowInfo>>("clients") {
                Ok(clients) => {
                    if let Some(client) = clients.iter().find(|c| c.address == window_address) {
                        if client.workspace.id > 0 {
                            println!("Window restored externally. Exiting.");
                            check_task_exit_notify.notify_one();
                            break;
                        }
                    } else {
                        println!("Window closed externally. Exiting.");
                        check_task_exit_notify.notify_one();
                        break;
                    }
                }
                Err(e) => {
                    // A transient hyprctl failure must not kill the daemon: that
                    // would drop the tray icon and strand the window on the
                    // special workspace. Restoring here wouldn't work either (if
                    // hyprctl is down, move_to_workspace fails too), so just log
                    // and retry on the next tick.
                    eprintln!("Error checking window state (will retry): {}", e);
                }
            }
        }
    });

    // 6. Wait for a notification to exit
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nInterrupted by Ctrl+C. Restoring window.");
            let _ = move_to_workspace(
                &window_info.workspace.id.to_string(),
                &window_info.address,
                false,
            );
        }
        _ = exit_notify.notified() => {}
    }

    // 7. Cleanup is handled automatically when the connection is dropped.
    Ok(())
}
