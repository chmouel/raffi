use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use super::actions::{run_bounded, DeferredAction};
use super::command_list::{CommandAction, CommandItem};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
pub(super) const NO_DEVICES: &str = "No audio devices found";
const DEFAULT_MARK: &str = "✓";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Sink,
    Source,
}

impl Kind {
    fn noun(self) -> &'static str {
        match self {
            Self::Sink => "sink",
            Self::Source => "source",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Volume {
    #[serde(default)]
    value_percent: String,
}

#[derive(Debug, Default, Deserialize)]
struct Port {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "type")]
    kind: String,
}

/// One entry of `pactl -f json list sinks|sources`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct Device {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    volume: BTreeMap<String, Volume>,
    #[serde(default)]
    channel_map: String,
    #[serde(default)]
    ports: Vec<Port>,
    #[serde(default)]
    active_port: Option<String>,
    /// On sources, pactl puts the sink a monitor listens to here.
    #[serde(default)]
    monitor_source: Option<String>,
    #[serde(default)]
    monitor_of_sink: Option<String>,
    #[serde(default)]
    properties: BTreeMap<String, serde_json::Value>,
}

impl Device {
    fn is_monitor(&self) -> bool {
        let set = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.is_empty() && value != "n/a")
        };
        self.name.ends_with(".monitor") || set(&self.monitor_of_sink) || set(&self.monitor_source)
    }

    fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).and_then(serde_json::Value::as_str)
    }

    fn port(&self) -> Option<&Port> {
        let active = self.active_port.as_deref()?;
        self.ports.iter().find(|port| port.name == active)
    }

    fn title(&self) -> &str {
        if self.description.is_empty() {
            &self.name
        } else {
            &self.description
        }
    }

    /// Distinct channel volumes in channel order, such as `95%` or
    /// `80% / 100%`.
    fn volume(&self) -> String {
        let mut percents: Vec<&str> = Vec::new();
        let channels = self.channel_map.split(',').filter(|c| !c.is_empty());
        let ordered = channels
            .filter_map(|channel| self.volume.get(channel))
            .chain(self.volume.values());
        for volume in ordered {
            let percent = volume.value_percent.as_str();
            if !percent.is_empty() && !percents.contains(&percent) {
                percents.push(percent);
            }
        }
        percents.join(" / ")
    }

    /// Whether the form factor or active port points at headphones.
    fn is_headphones(&self) -> bool {
        let port = self.port();
        [
            self.property("device.form_factor").unwrap_or_default(),
            port.map(|port| port.kind.as_str()).unwrap_or_default(),
            port.map(|port| port.name.as_str()).unwrap_or_default(),
        ]
        .iter()
        .any(|hint| {
            let hint = hint.to_lowercase();
            hint.contains("headphone") || hint.contains("headset")
        })
    }

    fn icon_names(&self, kind: Kind) -> Vec<String> {
        let mut names = Vec::new();
        match kind {
            Kind::Source => names.push("audio-input-microphone".to_string()),
            Kind::Sink => {
                if self.is_headphones() {
                    names.push("audio-headphones".to_string());
                }
                names.push("audio-speakers".to_string());
            }
        }
        if let Some(icon) = self.property("device.icon_name") {
            names.push(icon.to_string());
        }
        names.push("audio-card".to_string());
        names
    }
}

/// Parse `pactl -f json list sinks|sources`, leaving out monitor sources.
pub(super) fn parse_devices(output: &[u8], kind: Kind) -> Result<Vec<Device>, String> {
    let devices: Vec<Device> =
        serde_json::from_slice(output).map_err(|error| format!("cannot parse output: {error}"))?;
    Ok(devices
        .into_iter()
        .filter(|device| kind == Kind::Sink || !device.is_monitor())
        .collect())
}

pub(super) fn set_default_argv(kind: Kind, name: &str) -> Vec<String> {
    vec![
        "pactl".to_string(),
        format!("set-default-{}", kind.noun()),
        name.to_string(),
    ]
}

pub(super) fn toggle_mute_argv(kind: Kind, name: &str) -> Vec<String> {
    vec![
        "pactl".to_string(),
        format!("set-{}-mute", kind.noun()),
        name.to_string(),
        "toggle".to_string(),
    ]
}

/// List the audio outputs and inputs. Blocking: run it off the UI thread.
pub(super) fn load_devices(keep_open: bool) -> Vec<CommandItem> {
    load_devices_with(
        |args| {
            let mut command = Command::new("pactl");
            command.args(args);
            run_bounded(command, TIMEOUT, MAX_OUTPUT)
        },
        keep_open,
    )
}

/// `load_devices` with `pactl` run by `pactl`. The four queries run at the
/// same time. A failed listing, such as an unreachable sound server, gives a
/// single error row.
pub(super) fn load_devices_with(
    pactl: impl Fn(&[&str]) -> Result<Vec<u8>, String> + Sync,
    keep_open: bool,
) -> Vec<CommandItem> {
    let (sinks, sources, default_sink, default_source) = std::thread::scope(|scope| {
        let pactl = &pactl;
        let sinks = scope.spawn(|| pactl(&["-f", "json", "list", "sinks"]));
        let sources = scope.spawn(|| pactl(&["-f", "json", "list", "sources"]));
        let default_sink = scope.spawn(|| pactl(&["get-default-sink"]));
        let default_source = pactl(&["get-default-source"]);
        let join = |handle: std::thread::ScopedJoinHandle<'_, Result<Vec<u8>, String>>| {
            handle
                .join()
                .unwrap_or_else(|_| Err("stopped unexpectedly".to_string()))
        };
        (
            join(sinks),
            join(sources),
            join(default_sink),
            default_source,
        )
    });
    let devices = sinks
        .and_then(|output| parse_devices(&output, Kind::Sink))
        .and_then(|sinks| Ok((sinks, parse_devices(&sources?, Kind::Source)?)));
    let (sinks, sources) = match devices {
        Ok(devices) => devices,
        Err(error) => {
            crate::debug_log!("audio: listing failed: {error}");
            return vec![CommandItem::info(
                "error",
                format!("Cannot list audio devices, is the sound server running? (pactl: {error})"),
                "dialog-error",
            )];
        }
    };
    let default_name = |result: Result<Vec<u8>, String>| match result {
        Ok(output) => String::from_utf8_lossy(&output).trim().to_string(),
        Err(error) => {
            crate::debug_log!("audio: no default device: {error}");
            String::new()
        }
    };
    audio_items(
        &sinks,
        &sources,
        &default_name(default_sink),
        &default_name(default_source),
        keep_open,
    )
}

/// An "Outputs" header followed by the sinks, then an "Inputs" header
/// followed by the sources.
pub(super) fn audio_items(
    sinks: &[Device],
    sources: &[Device],
    default_sink: &str,
    default_source: &str,
    keep_open: bool,
) -> Vec<CommandItem> {
    if sinks.is_empty() && sources.is_empty() {
        return vec![CommandItem::info(
            "no-devices",
            NO_DEVICES.to_string(),
            "audio-card",
        )];
    }
    let mut items = Vec::new();
    for (kind, devices, default, header, icon) in [
        (Kind::Sink, sinks, default_sink, "Outputs", "audio-speakers"),
        (
            Kind::Source,
            sources,
            default_source,
            "Inputs",
            "audio-input-microphone",
        ),
    ] {
        if devices.is_empty() {
            continue;
        }
        let mut header_item = CommandItem::info(kind.noun(), header.to_string(), icon);
        header_item.match_text = format!("{header} {}s", kind.noun());
        items.push(header_item);
        items.extend(
            devices
                .iter()
                .map(|device| device_item(kind, device, device.name == default, keep_open)),
        );
    }
    items
}

fn device_item(kind: Kind, device: &Device, is_default: bool, keep_open: bool) -> CommandItem {
    let port = device.port().map(|port| {
        if port.description.is_empty() {
            port.name.clone()
        } else {
            port.description.clone()
        }
    });
    let volume = device.volume();
    let mut subtitle = Vec::new();
    if is_default {
        subtitle.push("Default".to_string());
    }
    if device.mute {
        subtitle.push("Muted".to_string());
    }
    subtitle.extend(port.clone());
    if !volume.is_empty() {
        subtitle.push(volume.clone());
    }
    subtitle.push(device.name.clone());

    let set_default = set_default_argv(kind, &device.name);
    let toggle_mute = toggle_mute_argv(kind, &device.name);
    let mut details = vec![("Name".to_string(), device.name.clone())];
    if let Some(port) = &port {
        details.push(("Port".to_string(), port.clone()));
    }
    if !volume.is_empty() {
        details.push(("Volume".to_string(), volume));
    }
    details.extend([
        (
            "Muted".to_string(),
            if device.mute { "Yes" } else { "No" }.to_string(),
        ),
        (
            "Default".to_string(),
            if is_default { "Yes" } else { "No" }.to_string(),
        ),
        ("Enter".to_string(), set_default.join(" ")),
        ("Ctrl+Enter".to_string(), toggle_mute.join(" ")),
    ]);
    let role = match kind {
        Kind::Sink if device.is_headphones() => "outputs headphones",
        Kind::Sink => "outputs speakers",
        Kind::Source => "inputs microphones",
    };
    CommandItem {
        title: if is_default {
            format!("{DEFAULT_MARK} {}", device.title())
        } else {
            device.title().to_string()
        },
        subtitle: subtitle.join(" · "),
        match_text: format!(
            "{} {role} {} {}",
            device.title(),
            port.unwrap_or_default(),
            device.name
        ),
        icon_names: device.icon_names(kind),
        icon: None,
        identity: format!("{}/{}", kind.noun(), device.name),
        unavailable: None,
        details,
        primary: Some(CommandAction {
            action: DeferredAction::Command(set_default),
            confirm: None,
            keep_open,
            progress: None,
        }),
        secondary: Some(CommandAction {
            action: DeferredAction::Command(toggle_mute),
            confirm: None,
            keep_open,
            progress: None,
        }),
        badge: None,
        domains: Vec::new(),
    }
}

/// Trimmed from `pactl -f json list sinks` on PipeWire, with one
/// Bluetooth headset added.
#[cfg(test)]
pub(super) const SINKS: &str = r#"[
  {"index":70,"state":"RUNNING","name":"alsa_output.pci-0000_c4_00.6.pro-output-0",
   "description":"Ryzen HD Audio Controller Pro","driver":"PipeWire",
   "channel_map":"aux0,aux1","mute":false,
   "volume":{"aux0":{"value":65536,"value_percent":"100%","db":"0.00 dB"},
             "aux1":{"value":65536,"value_percent":"100%","db":"0.00 dB"}},
   "monitor_source":"alsa_output.pci-0000_c4_00.6.pro-output-0.monitor",
   "properties":{"device.icon_name":"audio-card-analog","device.bus":"pci"},
   "ports":[],"active_port":null,"formats":["pcm"]},
  {"index":72,"state":"SUSPENDED","name":"alsa_output.usb-Razer_Seiren-00.analog-stereo",
   "description":"Razer Seiren V2 Pro Analog Stereo","driver":"PipeWire",
   "channel_map":"front-left,front-right","mute":true,
   "volume":{"front-left":{"value":62259,"value_percent":"95%","db":"-1.34 dB"},
             "front-right":{"value":52429,"value_percent":"80%","db":"-5.81 dB"}},
   "monitor_source":"alsa_output.usb-Razer_Seiren-00.analog-stereo.monitor",
   "properties":{"device.icon_name":"audio-card-analog"},
   "ports":[{"name":"analog-output-speaker","description":"Speakers","type":"Speaker",
             "priority":10000,"availability":"available"}],
   "active_port":"analog-output-speaker","formats":["pcm"]},
  {"index":90,"state":"IDLE","name":"bluez_output.00_11_22_33_44_55.1",
   "description":"WH-1000XM4","channel_map":"front-left,front-right","mute":false,
   "volume":{"front-left":{"value":32768,"value_percent":"50%","db":"-18.06 dB"},
             "front-right":{"value":32768,"value_percent":"50%","db":"-18.06 dB"}},
   "properties":{"device.form_factor":"headphone","device.icon_name":"audio-headphones-bluetooth"},
   "ports":[],"active_port":null}
]"#;

/// Trimmed from `pactl -f json list sources`: pactl names the sink a
/// monitor listens to in `monitor_source`.
#[cfg(test)]
pub(super) const SOURCES: &str = r#"[
  {"index":70,"name":"alsa_output.pci-0000_c4_00.6.pro-output-0.monitor",
   "description":"Monitor of Ryzen HD Audio Controller Pro","mute":false,
   "channel_map":"aux0,aux1",
   "volume":{"aux0":{"value":65536,"value_percent":"100%","db":"0.00 dB"}},
   "monitor_source":"alsa_output.pci-0000_c4_00.6.pro-output-0",
   "properties":{},"ports":[],"active_port":null},
  {"index":71,"name":"alsa_input.pci-0000_c4_00.6.pro-input-0",
   "description":"Ryzen HD Audio Controller Pro","mute":false,"channel_map":"aux0,aux1",
   "volume":{"aux0":{"value":65536,"value_percent":"100%","db":"0.00 dB"}},
   "monitor_source":"","properties":{"device.icon_name":"audio-card-analog"},
   "ports":[],"active_port":null},
  {"index":73,"name":"alsa_input.usb-Razer_Seiren-00.mono-fallback",
   "description":"Razer Seiren V2 Pro Mono","mute":false,"channel_map":"mono",
   "volume":{"mono":{"value":49152,"value_percent":"75%","db":"-7.50 dB"}},
   "monitor_source":"",
   "properties":{},
   "ports":[{"name":"analog-input-mic","description":"Microphone","type":"Mic"}],
   "active_port":"analog-input-mic"},
  {"index":80,"name":"virtual.capture","description":"Old style monitor",
   "monitor_of_sink":"virtual.sink","mute":false,"properties":{}}
]"#;

#[cfg(test)]
pub(super) const DEFAULT_SINK: &str = "alsa_output.usb-Razer_Seiren-00.analog-stereo";
#[cfg(test)]
pub(super) const DEFAULT_SOURCE: &str = "alsa_input.usb-Razer_Seiren-00.mono-fallback";

/// The rows for the fixtures, with their default devices.
#[cfg(test)]
pub(super) fn fixture_items(keep_open: bool) -> Vec<CommandItem> {
    audio_items(
        &parse_devices(SINKS.as_bytes(), Kind::Sink).unwrap(),
        &parse_devices(SOURCES.as_bytes(), Kind::Source).unwrap(),
        DEFAULT_SINK,
        DEFAULT_SOURCE,
        keep_open,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pactl(
        sinks: Result<&'static str, &'static str>,
    ) -> impl Fn(&[&str]) -> Result<Vec<u8>, String> + Sync {
        move |args| {
            let output = match args {
                ["-f", "json", "list", "sinks"] => sinks,
                ["-f", "json", "list", "sources"] => Ok(SOURCES),
                ["get-default-sink"] => Ok("alsa_output.usb-Razer_Seiren-00.analog-stereo\n"),
                ["get-default-source"] => Ok("alsa_input.usb-Razer_Seiren-00.mono-fallback\n"),
                _ => panic!("unexpected pactl call {args:?}"),
            };
            output
                .map(|output| output.as_bytes().to_vec())
                .map_err(str::to_string)
        }
    }

    #[test]
    fn test_parse_devices_skips_monitors() {
        let sources = parse_devices(SOURCES.as_bytes(), Kind::Source).unwrap();
        let names: Vec<&str> = sources.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "alsa_input.pci-0000_c4_00.6.pro-input-0",
                "alsa_input.usb-Razer_Seiren-00.mono-fallback"
            ]
        );
        assert_eq!(
            parse_devices(SINKS.as_bytes(), Kind::Sink).unwrap().len(),
            3
        );
        assert!(parse_devices(b"[]", Kind::Source).unwrap().is_empty());
        assert!(parse_devices(b"Connection failure", Kind::Sink).is_err());
    }

    #[test]
    fn test_audio_items_rows_defaults_and_icons() {
        let items = load_devices_with(fake_pactl(Ok(SINKS)), true);
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Outputs",
                "Ryzen HD Audio Controller Pro",
                "✓ Razer Seiren V2 Pro Analog Stereo",
                "WH-1000XM4",
                "Inputs",
                "Ryzen HD Audio Controller Pro",
                "✓ Razer Seiren V2 Pro Mono",
            ]
        );
        assert!(items[0].primary.is_none() && items[4].primary.is_none());
        assert_eq!(
            items[2].subtitle,
            "Default · Muted · Speakers · 95% / 80% · alsa_output.usb-Razer_Seiren-00.analog-stereo"
        );
        assert_eq!(
            items[1].subtitle,
            "100% · alsa_output.pci-0000_c4_00.6.pro-output-0"
        );
        assert_eq!(items[2].identity, format!("sink/{DEFAULT_SINK}"));
        assert_eq!(items[6].identity, format!("source/{DEFAULT_SOURCE}"));
        assert_eq!(items[1].icon_names[0], "audio-speakers");
        assert_eq!(
            items[3].icon_names[..2],
            ["audio-headphones", "audio-speakers"]
        );
        assert_eq!(items[6].icon_names[0], "audio-input-microphone");
    }

    #[test]
    fn test_audio_item_argv() {
        let items = load_devices_with(fake_pactl(Ok(SINKS)), true);
        let argv = |action: Option<&CommandAction>| match action.map(|a| &a.action) {
            Some(DeferredAction::Command(argv)) => argv.clone(),
            other => panic!("unexpected action {other:?}"),
        };
        let sink = &items[3];
        assert_eq!(
            argv(sink.primary.as_ref()),
            [
                "pactl",
                "set-default-sink",
                "bluez_output.00_11_22_33_44_55.1"
            ]
        );
        assert_eq!(
            argv(sink.secondary.as_ref()),
            [
                "pactl",
                "set-sink-mute",
                "bluez_output.00_11_22_33_44_55.1",
                "toggle"
            ]
        );
        let source = &items[5];
        assert_eq!(
            argv(source.primary.as_ref()),
            [
                "pactl",
                "set-default-source",
                "alsa_input.pci-0000_c4_00.6.pro-input-0"
            ]
        );
        assert_eq!(
            argv(source.secondary.as_ref()),
            [
                "pactl",
                "set-source-mute",
                "alsa_input.pci-0000_c4_00.6.pro-input-0",
                "toggle"
            ]
        );
        assert!(sink.primary.as_ref().unwrap().keep_open);
        let closing = load_devices_with(fake_pactl(Ok(SINKS)), false);
        assert!(!closing[3].secondary.as_ref().unwrap().keep_open);
    }

    #[test]
    fn test_load_devices_failures_give_an_error_row() {
        for sinks in [Err("exited with exit status: 1"), Ok("not json")] {
            let items = load_devices_with(fake_pactl(sinks), true);
            assert_eq!(items.len(), 1);
            assert!(items[0].title.starts_with("Cannot list audio devices"));
            assert!(items[0].primary.is_none());
        }
        let items = load_devices_with(fake_pactl(Ok("[]")), true);
        assert_eq!(items[0].title, "Inputs");
        assert_eq!(audio_items(&[], &[], "", "", true)[0].title, NO_DEVICES);
    }

    #[test]
    fn test_load_devices_without_defaults() {
        let pactl = |args: &[&str]| match args {
            ["-f", "json", "list", "sinks"] => Ok(SINKS.as_bytes().to_vec()),
            ["-f", "json", "list", "sources"] => Ok(b"[]".to_vec()),
            _ => Err("timed out after 2s".to_string()),
        };
        let items = load_devices_with(pactl, true);
        assert_eq!(items.len(), 4);
        assert!(items
            .iter()
            .all(|item| !item.title.starts_with(DEFAULT_MARK)));
    }
}
