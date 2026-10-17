use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::actions::{run_bounded_status, DeferredAction};
use super::command_list::{CommandAction, CommandItem};

/// The whole listing, adapter and device details included.
const SCAN_DEADLINE: Duration = Duration::from_secs(3);
/// Connecting can take several seconds while the device wakes up.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_INFO_CALLS: usize = 4;
const MAX_OUTPUT: usize = 256 * 1024;
const CONNECTED_MARK: &str = "✓";
pub(super) const ICON_ON: &str = "bluetooth-active";
pub(super) const ICON_OFF: &str = "bluetooth-disabled";
const ICON_DEVICE: &str = "bluetooth";
pub(super) const NO_CONTROLLER: &str = "No Bluetooth adapter found";
pub(super) const NO_DEVICES: &str = "No paired devices";
pub(super) const UNKNOWN_STATE: &str = "State unknown, open the list again to retry";

/// What a `bluetoothctl` run printed, kept even when it failed since the
/// reason is printed on stdout.
#[derive(Debug, Clone)]
pub(super) struct Output {
    pub success: bool,
    pub stdout: String,
}

/// Runs `bluetoothctl` with the arguments and timeout.
pub(super) type Runner = dyn Fn(&[&str], Duration) -> Result<Output, String> + Send + Sync;

fn bluetoothctl(args: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut command = Command::new("bluetoothctl");
    command.args(args);
    let (status, stdout) = run_bounded_status(command, timeout, MAX_OUTPUT)?;
    Ok(Output {
        success: status.success(),
        stdout: strip_ansi(&String::from_utf8_lossy(&stdout)),
    })
}

/// Drop the colour codes bluetoothctl prints around some messages.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// `Key: value` pairs of `bluetoothctl show` and `info` output.
fn fields(output: &str) -> impl Iterator<Item = (&str, &str)> {
    output
        .lines()
        .filter_map(|line| line.trim().split_once(": "))
        .map(|(key, value)| (key, value.trim()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Adapter {
    pub powered: bool,
    pub alias: String,
}

/// Parse `bluetoothctl show`; `None` when there is no adapter.
pub(super) fn parse_show(output: &str) -> Result<Option<Adapter>, String> {
    if output.contains("No default controller available") {
        return Ok(None);
    }
    let mut powered = None;
    let mut alias = String::new();
    for (key, value) in fields(output) {
        match key {
            "Powered" if powered.is_none() => powered = Some(value == "yes"),
            "Alias" => alias = value.to_string(),
            "Name" if alias.is_empty() => alias = value.to_string(),
            _ => {}
        }
    }
    let powered = powered.ok_or("no adapter state in the output")?;
    Ok(Some(Adapter { powered, alias }))
}

fn is_address(text: &str) -> bool {
    text.len() == 17
        && text.split(':').count() == 6
        && text
            .split(':')
            .all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Parse `bluetoothctl devices Paired` lines, `Device <address> <name>`.
pub(super) fn parse_devices(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (address, name) = line.trim_end().strip_prefix("Device ")?.split_once(' ')?;
            is_address(address).then(|| (address.to_string(), name.to_string()))
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DeviceInfo {
    pub name: String,
    pub alias: String,
    pub icon: String,
    pub connected: bool,
    pub paired: bool,
    pub battery: Option<u8>,
}

/// Parse `bluetoothctl info <address>`.
pub(super) fn parse_info(output: &str) -> Result<DeviceInfo, String> {
    let mut info = DeviceInfo {
        paired: true,
        ..DeviceInfo::default()
    };
    let mut connected = None;
    for (key, value) in fields(output) {
        match key {
            "Name" => info.name = value.to_string(),
            "Alias" => info.alias = value.to_string(),
            "Icon" => info.icon = value.to_string(),
            "Connected" => connected = Some(value == "yes"),
            "Paired" => info.paired = value == "yes",
            // `0x50 (80)`
            "Battery Percentage" => {
                info.battery = value
                    .split_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                    .and_then(|percent| percent.parse().ok());
            }
            _ => {}
        }
    }
    info.connected = connected.ok_or("no connection state in the output")?;
    Ok(info)
}

/// The paired devices. Versions of bluetoothctl that reject the `Paired`
/// filter have `paired-devices` instead.
fn list_paired(run: &Runner, deadline: Instant) -> Result<Vec<(String, String)>, String> {
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let first = match run(&["devices", "Paired"], remaining()) {
        Ok(output) if output.success => return Ok(parse_devices(&output.stdout)),
        Ok(_) => "devices Paired failed".to_string(),
        Err(error) => error,
    };
    match run(&["paired-devices"], remaining()) {
        Ok(output) if output.success => Ok(parse_devices(&output.stdout)),
        _ => Err(first),
    }
}

/// `bluetoothctl info` for each address, at most `MAX_INFO_CALLS` at a time.
/// Addresses not answered by `deadline` are `None`.
fn scan_info(
    run: &Arc<Runner>,
    addresses: &[String],
    deadline: Instant,
) -> Vec<Option<DeviceInfo>> {
    let addresses = Arc::new(addresses.to_vec());
    let next = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel();
    for _ in 0..MAX_INFO_CALLS.min(addresses.len()) {
        let (run, addresses, next, sender) = (
            Arc::clone(run),
            Arc::clone(&addresses),
            Arc::clone(&next),
            sender.clone(),
        );
        std::thread::spawn(move || loop {
            let index = next.fetch_add(1, Ordering::SeqCst);
            let Some(address) = addresses.get(index) else {
                break;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let info = run(&["info", address], remaining).and_then(|output| {
                if output.success {
                    parse_info(&output.stdout)
                } else {
                    Err(output.stdout.trim().to_string())
                }
            });
            if sender.send((index, info)).is_err() {
                break;
            }
        });
    }
    drop(sender);
    let mut infos: Vec<Option<DeviceInfo>> = addresses.iter().map(|_| None).collect();
    while let Ok((index, info)) =
        receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        match info {
            Ok(info) => infos[index] = Some(info),
            Err(error) => crate::debug_log!("bluetooth: no details for a device: {error}"),
        }
    }
    infos
}

/// List the adapter and paired devices. Blocking: run it off the UI thread.
pub(super) fn load_devices(keep_open: bool) -> Vec<CommandItem> {
    load_devices_with(Arc::new(bluetoothctl), SCAN_DEADLINE, keep_open)
}

/// `load_devices` with `bluetoothctl` run by `run` and the whole listing
/// bounded by `budget`.
pub(super) fn load_devices_with(
    run: Arc<Runner>,
    budget: Duration,
    keep_open: bool,
) -> Vec<CommandItem> {
    let deadline = Instant::now() + budget;
    let show = run(
        &["show"],
        deadline.saturating_duration_since(Instant::now()),
    );
    let adapter = match show.and_then(|output| parse_show(&output.stdout)) {
        Ok(Some(adapter)) => adapter,
        Ok(None) => {
            return vec![CommandItem::info(
                "no-controller",
                NO_CONTROLLER.to_string(),
                ICON_OFF,
            )]
        }
        Err(error) => {
            crate::debug_log!("bluetooth: show failed: {error}");
            return vec![error_item(format!(
                "Cannot read the Bluetooth adapter, is bluetoothd running? (bluetoothctl: {error})"
            ))];
        }
    };
    let mut items = vec![power_item(&adapter, keep_open)];
    if !adapter.powered {
        return items;
    }
    let devices = match list_paired(&*run, deadline) {
        Ok(devices) => devices,
        Err(error) => {
            crate::debug_log!("bluetooth: listing devices failed: {error}");
            items.push(error_item(format!(
                "Cannot list paired devices (bluetoothctl: {error})"
            )));
            return items;
        }
    };
    let addresses: Vec<String> = devices.iter().map(|(address, _)| address.clone()).collect();
    let infos = scan_info(&run, &addresses, deadline);
    items.extend(
        devices
            .iter()
            .zip(infos)
            .filter(|(_, info)| info.as_ref().is_none_or(|info| info.paired))
            .map(|((address, name), info)| device_item(address, name, info.as_ref(), keep_open)),
    );
    if items.len() == 1 {
        items.push(CommandItem::info(
            "no-devices",
            NO_DEVICES.to_string(),
            ICON_DEVICE,
        ));
    }
    items
}

fn error_item(message: String) -> CommandItem {
    CommandItem::info("error", message, "dialog-error")
}

fn action(
    args: &[&str],
    confirm: Option<String>,
    progress: &str,
    keep_open: bool,
) -> CommandAction {
    CommandAction {
        action: DeferredAction::Bluetooth(args.iter().map(|arg| arg.to_string()).collect()),
        confirm,
        keep_open,
        progress: Some(progress.to_string()),
    }
}

/// The adapter power toggle. Without match text, filtering keeps it first.
pub(super) fn power_item(adapter: &Adapter, keep_open: bool) -> CommandItem {
    let (state, next, icon, primary) = if adapter.powered {
        (
            "on",
            "off",
            ICON_ON,
            action(
                &["power", "off"],
                Some("turn Bluetooth off".to_string()),
                "Turning Bluetooth off...",
                keep_open,
            ),
        )
    } else {
        (
            "off",
            "on",
            ICON_OFF,
            action(&["power", "on"], None, "Turning Bluetooth on...", keep_open),
        )
    };
    let mut subtitle = format!("Enter turns it {next}");
    if !adapter.alias.is_empty() {
        subtitle.push_str(&format!(" · {}", adapter.alias));
    }
    CommandItem {
        title: format!("Bluetooth is {state}"),
        subtitle,
        match_text: String::new(),
        icon_names: vec![icon.to_string(), ICON_DEVICE.to_string()],
        icon: None,
        identity: format!("adapter/power-{next}"),
        unavailable: None,
        primary: Some(primary),
        secondary: None,
        details: vec![
            ("Adapter".to_string(), adapter.alias.clone()),
            (
                "Powered".to_string(),
                if adapter.powered { "Yes" } else { "No" }.to_string(),
            ),
            ("Enter".to_string(), format!("bluetoothctl power {next}")),
        ],
        badge: None,
        domains: Vec::new(),
    }
}

/// A paired device. Its identity is its address alone, so the selection
/// stays on the device when a connect or disconnect reloads the list.
pub(super) fn device_item(
    address: &str,
    listed_name: &str,
    info: Option<&DeviceInfo>,
    keep_open: bool,
) -> CommandItem {
    let name = info
        .map(|info| {
            if info.alias.is_empty() {
                &info.name
            } else {
                &info.alias
            }
        })
        .filter(|name| !name.is_empty())
        .map_or(listed_name, String::as_str);
    let mut icon_names = Vec::new();
    if let Some(info) = info.filter(|info| !info.icon.is_empty()) {
        icon_names.push(info.icon.clone());
    }
    icon_names.push(ICON_DEVICE.to_string());
    let mut details = vec![("Address".to_string(), address.to_string())];
    let Some(info) = info else {
        return CommandItem {
            title: name.to_string(),
            subtitle: address.to_string(),
            match_text: format!("{name} {address}"),
            icon_names,
            icon: None,
            identity: format!("device/{address}"),
            unavailable: Some(UNKNOWN_STATE.to_string()),
            primary: None,
            secondary: None,
            details,
            badge: None,
            domains: Vec::new(),
        };
    };
    let battery = info.battery.map(|percent| format!("{percent}% battery"));
    let mut subtitle = vec![if info.connected {
        "Connected"
    } else {
        "Not connected"
    }
    .to_string()];
    subtitle.extend(battery.clone());
    subtitle.push(address.to_string());
    let (verb, primary) = if info.connected {
        (
            "disconnect",
            action(
                &["disconnect", address],
                Some(format!("disconnect {name}")),
                "Disconnecting...",
                keep_open,
            ),
        )
    } else {
        (
            "connect",
            action(&["connect", address], None, "Connecting...", keep_open),
        )
    };
    details.push((
        "Connected".to_string(),
        if info.connected { "Yes" } else { "No" }.to_string(),
    ));
    if let Some(battery) = &battery {
        details.push(("Battery".to_string(), battery.clone()));
    }
    if !info.icon.is_empty() {
        details.push(("Type".to_string(), info.icon.clone()));
    }
    details.push((
        "Enter".to_string(),
        format!("bluetoothctl {verb} {address}"),
    ));
    CommandItem {
        title: if info.connected {
            format!("{CONNECTED_MARK} {name}")
        } else {
            name.to_string()
        },
        subtitle: subtitle.join(" · "),
        match_text: format!(
            "{name} {} {} {address} {}",
            info.name,
            info.icon,
            if info.connected {
                "connected"
            } else {
                "disconnected"
            }
        ),
        icon_names,
        icon: None,
        identity: format!("device/{address}"),
        unavailable: None,
        primary: Some(primary),
        secondary: None,
        details,
        badge: None,
        domains: Vec::new(),
    }
}

/// The line reporting a failure. bluetoothctl sometimes exits with success
/// after printing one.
fn failure_line(stdout: &str) -> Option<&str> {
    stdout.lines().map(str::trim).find(|line| {
        line.contains("Failed")
            || line.contains("not available")
            || line.contains("org.bluez.Error")
            || line.starts_with("Error")
    })
}

fn action_result(result: Result<Output, String>) -> Result<(), String> {
    let output = result.map_err(|error| format!("bluetoothctl: {error}"))?;
    if let Some(line) = failure_line(&output.stdout) {
        return Err(line.to_string());
    }
    if !output.success {
        return Err("bluetoothctl failed".to_string());
    }
    Ok(())
}

/// Run `bluetoothctl` with `args`, such as `connect <address>`. Blocking:
/// run it off the UI thread.
pub(super) fn run_action(args: &[String]) -> Result<(), String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    action_result(bluetoothctl(&args, ACTION_TIMEOUT))
}

#[cfg(test)]
pub(super) const SHOW: &str = include_str!("../../../tests/fixtures/bluetooth/show.txt");
#[cfg(test)]
pub(super) const DEVICES: &str =
    include_str!("../../../tests/fixtures/bluetooth/devices-paired.txt");
#[cfg(test)]
const INFO_HEADPHONES: &str = include_str!("../../../tests/fixtures/bluetooth/info-headphones.txt");
#[cfg(test)]
const INFO_EARBUDS: &str = include_str!("../../../tests/fixtures/bluetooth/info-earbuds.txt");
#[cfg(test)]
const INFO_KEYBOARD: &str = include_str!("../../../tests/fixtures/bluetooth/info-keyboard.txt");

/// A runner answering from the fixtures, with the adapter powered as
/// given.
#[cfg(test)]
pub(super) fn fixture_runner(powered: bool) -> Arc<Runner> {
    Arc::new(move |args: &[&str], _timeout: Duration| {
        let stdout = match args {
            ["show"] if powered => SHOW.to_string(),
            ["show"] => SHOW.replace("Powered: yes", "Powered: no"),
            ["devices", "Paired"] => DEVICES.to_string(),
            ["info", "00:11:22:33:44:01"] => INFO_HEADPHONES.to_string(),
            ["info", "00:11:22:33:44:02"] => INFO_EARBUDS.to_string(),
            ["info", "00:11:22:33:44:03"] => INFO_KEYBOARD.to_string(),
            _ => return Err(format!("unexpected bluetoothctl call {args:?}")),
        };
        Ok(Output {
            success: true,
            stdout,
        })
    })
}

/// The rows for the fixtures.
#[cfg(test)]
pub(super) fn fixture_items(powered: bool) -> Vec<CommandItem> {
    load_devices_with(fixture_runner(powered), SCAN_DEADLINE, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn ok(stdout: &str) -> Result<Output, String> {
        Ok(Output {
            success: true,
            stdout: stdout.to_string(),
        })
    }

    fn argv(item: &CommandItem) -> Vec<String> {
        match item.primary.as_ref().map(|action| &action.action) {
            Some(DeferredAction::Bluetooth(args)) => args.clone(),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn test_parse_show_and_devices() {
        assert_eq!(
            parse_show(SHOW).unwrap(),
            Some(Adapter {
                powered: true,
                alias: "laptop".to_string()
            })
        );
        assert_eq!(
            parse_show("No default controller available\n").unwrap(),
            None
        );
        assert!(parse_show("Waiting to connect to bluetoothd...").is_err());
        assert_eq!(
            parse_devices(DEVICES),
            vec![
                (
                    "00:11:22:33:44:01".to_string(),
                    "Studio  Headphones".to_string()
                ),
                ("00:11:22:33:44:02".to_string(), "Ear Buds".to_string()),
                ("00:11:22:33:44:03".to_string(), "Keyboard K3".to_string()),
            ]
        );
        assert!(parse_devices("Invalid command in menu main: x\nDevice nope Name\n").is_empty());
    }

    #[test]
    fn test_parse_info() {
        let headphones = parse_info(INFO_HEADPHONES).unwrap();
        assert!(headphones.connected && headphones.paired);
        assert_eq!(headphones.battery, Some(80));
        assert_eq!(headphones.icon, "audio-headset");
        let earbuds = parse_info(INFO_EARBUDS).unwrap();
        assert_eq!(
            (earbuds.alias.as_str(), earbuds.name.as_str()),
            ("My Buds", "Ear Buds")
        );
        assert!(!earbuds.connected);
        assert_eq!(earbuds.battery, None);
        assert!(parse_info("Device 00:11:22:33:44:09 not available").is_err());
    }

    #[test]
    fn test_rows_power_toggle_first_then_devices() {
        let items = fixture_items(true);
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Bluetooth is on",
                "✓ Studio  Headphones",
                "My Buds",
                "✓ Keyboard K3"
            ]
        );
        assert_eq!(items[0].identity, "adapter/power-off");
        assert_eq!(argv(&items[0]), ["power", "off"]);
        assert!(items[0].primary.as_ref().unwrap().confirm.is_some());
        assert_eq!(items[0].icon_names[0], ICON_ON);

        assert_eq!(
            items[1].subtitle,
            "Connected · 80% battery · 00:11:22:33:44:01"
        );
        assert_eq!(items[1].identity, "device/00:11:22:33:44:01");
        assert_eq!(argv(&items[1]), ["disconnect", "00:11:22:33:44:01"]);
        let disconnect = items[1].primary.as_ref().unwrap();
        assert_eq!(
            disconnect.confirm.as_deref(),
            Some("disconnect Studio  Headphones")
        );
        assert_eq!(disconnect.progress.as_deref(), Some("Disconnecting..."));
        assert_eq!(items[1].icon_names, ["audio-headset", "bluetooth"]);

        assert_eq!(items[2].subtitle, "Not connected · 00:11:22:33:44:02");
        assert_eq!(items[2].identity, "device/00:11:22:33:44:02");
        assert_eq!(argv(&items[2]), ["connect", "00:11:22:33:44:02"]);
        let connect = items[2].primary.as_ref().unwrap();
        assert!(connect.confirm.is_none() && connect.keep_open);
        assert_eq!(connect.progress.as_deref(), Some("Connecting..."));
        assert_eq!(items[3].icon_names[0], "input-keyboard");
    }

    #[test]
    fn test_powered_off_adapter_hides_devices() {
        let items = fixture_items(false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Bluetooth is off");
        assert_eq!(items[0].identity, "adapter/power-on");
        assert_eq!(argv(&items[0]), ["power", "on"]);
        assert!(items[0].primary.as_ref().unwrap().confirm.is_none());
        assert_eq!(items[0].icon_names[0], ICON_OFF);
    }

    #[test]
    fn test_status_rows() {
        let titles = |run: Arc<Runner>| -> Vec<String> {
            load_devices_with(run, SCAN_DEADLINE, true)
                .into_iter()
                .map(|item| item.title)
                .collect()
        };
        let no_controller: Arc<Runner> = Arc::new(|_: &[&str], _: Duration| {
            Ok(Output {
                success: false,
                stdout: "No default controller available\n".to_string(),
            })
        });
        assert_eq!(titles(no_controller), [NO_CONTROLLER]);

        let no_daemon: Arc<Runner> =
            Arc::new(|_: &[&str], _: Duration| Err("timed out after 3s".to_string()));
        let rows = titles(no_daemon);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("Cannot read the Bluetooth adapter"));

        let no_devices: Arc<Runner> = Arc::new(|args: &[&str], _: Duration| match args {
            ["show"] => ok(SHOW),
            _ => ok(""),
        });
        assert_eq!(titles(no_devices), ["Bluetooth is on", NO_DEVICES]);
    }

    #[test]
    fn test_falls_back_to_paired_devices() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let run: Arc<Runner> = Arc::new(move |args: &[&str], _: Duration| {
            seen.lock().unwrap().push(args.join(" "));
            match args {
                ["show"] => ok(SHOW),
                ["devices", "Paired"] => Ok(Output {
                    success: false,
                    stdout: "Invalid argument Paired\n".to_string(),
                }),
                ["paired-devices"] => ok("Device 00:11:22:33:44:02 Ear Buds\n"),
                ["info", _] => ok(INFO_EARBUDS),
                _ => Err("unexpected".to_string()),
            }
        });
        let items = load_devices_with(run, SCAN_DEADLINE, true);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].title, "My Buds");
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "show",
                "devices Paired",
                "paired-devices",
                "info 00:11:22:33:44:02"
            ]
        );
    }

    #[test]
    fn test_info_calls_are_bounded_and_late_ones_are_unknown() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (running_in, peak_in) = (Arc::clone(&running), Arc::clone(&peak));
        let devices: String = (1..=9)
            .map(|n| format!("Device 00:11:22:33:44:0{n} Device {n}\n"))
            .collect();
        let run: Arc<Runner> = Arc::new(move |args: &[&str], _: Duration| match args {
            ["show"] => ok(SHOW),
            ["devices", "Paired"] => ok(&devices),
            ["info", address] => {
                let now = running_in.fetch_add(1, Ordering::SeqCst) + 1;
                peak_in.fetch_max(now, Ordering::SeqCst);
                // The last device never answers in time.
                let delay = if address.ends_with('9') { 2000 } else { 30 };
                std::thread::sleep(Duration::from_millis(delay));
                running_in.fetch_sub(1, Ordering::SeqCst);
                ok(INFO_EARBUDS)
            }
            _ => Err("unexpected".to_string()),
        });
        let started = Instant::now();
        let items = load_devices_with(run, Duration::from_millis(500), true);
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert!(peak.load(Ordering::SeqCst) <= MAX_INFO_CALLS);
        assert_eq!(items.len(), 10);
        assert!(items[1..9].iter().all(|item| item.primary.is_some()));
        let late = &items[9];
        assert_eq!(late.title, "Device 9");
        assert_eq!(late.unavailable.as_deref(), Some(UNKNOWN_STATE));
        assert!(late.primary.is_none());
        assert_eq!(late.identity, "device/00:11:22:33:44:09");
    }

    #[test]
    fn test_unpaired_devices_are_left_out() {
        let run: Arc<Runner> = Arc::new(|args: &[&str], _: Duration| match args {
            ["show"] => ok(SHOW),
            ["devices", "Paired"] => ok("Device 00:11:22:33:44:02 Ear Buds\n"),
            _ => ok(&INFO_EARBUDS.replace("Paired: yes", "Paired: no")),
        });
        let titles: Vec<String> = load_devices_with(run, SCAN_DEADLINE, true)
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert_eq!(titles, ["Bluetooth is on", NO_DEVICES]);
    }

    #[test]
    fn test_action_result_reads_failures_from_stdout() {
        assert!(action_result(ok("Attempting to connect to X\nConnection successful\n")).is_ok());
        let failed = "Attempting to connect to 00:11:22:33:44:02\n\u{1b}[1;39mFailed to connect: org.bluez.Error.Failed br-connection-unknown\u{1b}[0m\n";
        assert_eq!(
            action_result(ok(&strip_ansi(failed))).unwrap_err(),
            "Failed to connect: org.bluez.Error.Failed br-connection-unknown"
        );
        assert_eq!(
            action_result(ok("Device 00:11:22:33:44:09 not available\n")).unwrap_err(),
            "Device 00:11:22:33:44:09 not available"
        );
        assert!(action_result(Ok(Output {
            success: false,
            stdout: String::new()
        }))
        .is_err());
        assert_eq!(
            action_result(Err("timed out after 15s".to_string())).unwrap_err(),
            "bluetoothctl: timed out after 15s"
        );
    }
}
