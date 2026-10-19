use std::process::Command;
use std::time::Duration;

use super::actions::{run_bounded, DeferredAction};
use super::command_list::{CommandAction, CommandItem};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 256 * 1024;
const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';
/// One record per player, in rarely used control characters so titles with
/// tabs or newlines stay in one field.
const FORMAT: &str = "\u{1e}{{playerInstance}}\u{1f}{{playerName}}\u{1f}{{status}}\u{1f}{{title}}\u{1f}{{artist}}\u{1f}{{album}}";
pub(super) const NO_PLAYERS: &str = "No media players running";
const HEADER_ICONS: [&str; 2] = ["multimedia-player", "audio-x-generic"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Player {
    /// The name `playerctl -p` takes, such as `firefox.instance_1_23`.
    pub instance: String,
    pub name: String,
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
}

/// List the media players with their controls. Blocking: run it off the UI
/// thread.
pub(super) fn load_players(keep_open: bool) -> Result<Vec<CommandItem>, String> {
    load_players_with(
        |args| {
            let mut command = Command::new("playerctl");
            command.args(args);
            run_bounded(command, TIMEOUT, MAX_OUTPUT)
        },
        keep_open,
    )
}

/// `load_players` with `playerctl` run by `playerctl`.
pub(super) fn load_players_with(
    playerctl: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
    keep_open: bool,
) -> Result<Vec<CommandItem>, String> {
    let names = playerctl(&["-l"]).map_err(|error| format!("playerctl: {error}"))?;
    let names = String::from_utf8_lossy(&names);
    if names.trim().is_empty() {
        return Ok(media_items(&[], keep_open));
    }
    let metadata = match playerctl(&["-a", "metadata", "--format", FORMAT]) {
        Ok(output) => parse_metadata(&String::from_utf8_lossy(&output)),
        // playerctl exits with an error when no player has track
        // information, which still leaves the players controllable.
        Err(error) if error.starts_with("exited with") => {
            crate::debug_log!("media: no metadata: {error}");
            Vec::new()
        }
        Err(error) => return Err(format!("playerctl: {error}")),
    };
    let players = merge_players(&names, metadata);
    Ok(media_items(&players, keep_open))
}

/// Parse `playerctl -a metadata --format FORMAT` output.
pub(super) fn parse_metadata(output: &str) -> Vec<Player> {
    output
        .split(RECORD)
        .filter_map(|record| {
            let record = record.strip_suffix('\n').unwrap_or(record);
            let mut fields = record.splitn(6, FIELD).map(str::to_string);
            let instance = fields.next().filter(|instance| !instance.is_empty())?;
            let name = fields.next().unwrap_or_default();
            Some(Player {
                name: if name.is_empty() {
                    instance.clone()
                } else {
                    name
                },
                instance,
                status: fields.next().unwrap_or_default(),
                title: fields.next().unwrap_or_default(),
                artist: fields.next().unwrap_or_default(),
                album: fields.next().unwrap_or_default(),
            })
        })
        .collect()
}

/// The players from `playerctl -l`, in its order, with their metadata.
pub(super) fn merge_players(names: &str, mut metadata: Vec<Player>) -> Vec<Player> {
    let mut players: Vec<Player> = names
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            metadata
                .iter()
                .position(|player| player.instance == name)
                .map(|index| metadata.remove(index))
                .unwrap_or_else(|| Player {
                    instance: name.to_string(),
                    name: name.split('.').next().unwrap_or(name).to_string(),
                    ..Player::default()
                })
        })
        .collect();
    players.extend(metadata);
    players
}

pub(super) fn playerctl_argv(instance: &str, command: &str) -> Vec<String> {
    ["playerctl", "-p", instance, command]
        .iter()
        .map(|arg| arg.to_string())
        .collect()
}

/// A header row per player followed by its controls.
pub(super) fn media_items(players: &[Player], keep_open: bool) -> Vec<CommandItem> {
    if players.is_empty() {
        return vec![CommandItem::info(
            "no-players",
            NO_PLAYERS.to_string(),
            HEADER_ICONS[0],
        )];
    }
    let mut items = Vec::new();
    for player in players {
        items.push(header_item(player));
        let (play_title, play_icon) = match player.status.as_str() {
            "Playing" => ("Pause", "media-playback-pause"),
            "Paused" | "Stopped" => ("Play", "media-playback-start"),
            _ => ("Play/Pause", "media-playback-start"),
        };
        let controls = [
            ("play-pause", play_title, play_icon, "Play or pause"),
            (
                "next",
                "Next",
                "media-skip-forward",
                "Skip to the next track",
            ),
            (
                "previous",
                "Previous",
                "media-skip-backward",
                "Go back to the previous track",
            ),
            ("stop", "Stop", "media-playback-stop", "Stop playback"),
        ];
        for (command, title, icon, enter) in controls {
            let argv = playerctl_argv(&player.instance, command);
            items.push(CommandItem {
                title: title.to_string(),
                subtitle: player.name.clone(),
                // The player name on both sides matches "next mpv" and
                // "mpv next" alike.
                match_text: format!("{0} {title} {command} {0}", player.name),
                icon_names: vec![icon.to_string()],
                icon: None,
                identity: format!("{}/{command}", player.instance),
                unavailable: None,
                details: vec![
                    ("Player".to_string(), player.name.clone()),
                    ("Command".to_string(), argv.join(" ")),
                    ("Enter".to_string(), enter.to_string()),
                ],
                primary: Some(CommandAction {
                    action: DeferredAction::Command(argv),
                    confirm: None,
                    keep_open,
                    progress: None,
                }),
                secondary: None,
                badge: None,
                domains: Vec::new(),
                dimmed: false,
            });
        }
    }
    items
}

fn header_item(player: &Player) -> CommandItem {
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = match (one_line(&player.title), one_line(&player.artist)) {
        (title, artist) if !title.is_empty() && !artist.is_empty() => {
            format!("{title} · {artist}")
        }
        _ if player.status.is_empty() => "No track information".to_string(),
        (title, artist) if title.is_empty() && artist.is_empty() => "Nothing playing".to_string(),
        (title, artist) => title + &artist,
    };
    let subtitle = if player.status.is_empty() {
        player.name.clone()
    } else {
        format!("{} · {}", player.status, player.name)
    };
    let mut icon_names = vec![player.name.clone(), player.name.to_lowercase()];
    icon_names.dedup();
    icon_names.extend(HEADER_ICONS.iter().map(|icon| icon.to_string()));
    let mut details = vec![("Player".to_string(), player.name.clone())];
    for (label, value) in [
        ("Status", &player.status),
        ("Title", &player.title),
        ("Artist", &player.artist),
        ("Album", &player.album),
    ] {
        if !value.is_empty() {
            details.push((label.to_string(), value.clone()));
        }
    }
    CommandItem {
        match_text: format!("{} {} {}", player.name, player.title, player.artist),
        title,
        subtitle,
        icon_names,
        icon: None,
        identity: player.instance.clone(),
        unavailable: None,
        primary: None,
        secondary: None,
        details,
        badge: None,
        domains: Vec::new(),
        dimmed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METADATA: &str =
        "\u{1e}spotify\u{1f}spotify\u{1f}Playing\u{1f}Song\twith tab\u{1f}Band\u{1f}Album\n\
        \u{1e}firefox.instance_1_23\u{1f}firefox\u{1f}Paused\u{1f}A video\u{1f}\u{1f}\n";

    #[test]
    fn test_parse_metadata_multiple_players() {
        let players = parse_metadata(METADATA);
        assert_eq!(players.len(), 2);
        assert_eq!(
            players[0],
            Player {
                instance: "spotify".into(),
                name: "spotify".into(),
                status: "Playing".into(),
                title: "Song\twith tab".into(),
                artist: "Band".into(),
                album: "Album".into(),
            }
        );
        assert_eq!(players[1].instance, "firefox.instance_1_23");
        assert_eq!(players[1].name, "firefox");
        assert_eq!(players[1].artist, "");
        assert!(parse_metadata("").is_empty());
        assert!(parse_metadata("\n").is_empty());
    }

    #[test]
    fn test_parse_metadata_keeps_newlines_in_titles_and_short_records() {
        let players = parse_metadata(
            "\u{1e}mpv\u{1f}mpv\u{1f}Paused\u{1f}two\nlines\u{1f}\u{1f}\n\u{1e}vlc\n",
        );
        assert_eq!(players[0].title, "two\nlines");
        assert_eq!(players[1].instance, "vlc");
        assert_eq!(players[1].name, "vlc");
        assert_eq!(players[1].status, "");
    }

    #[test]
    fn test_merge_players_keeps_players_without_metadata() {
        let players = merge_players("chromium.instance42\nspotify\n", parse_metadata(METADATA));
        let instances: Vec<&str> = players.iter().map(|p| p.instance.as_str()).collect();
        assert_eq!(
            instances,
            vec!["chromium.instance42", "spotify", "firefox.instance_1_23"]
        );
        assert_eq!(players[0].name, "chromium");
        assert_eq!(players[0].title, "");
    }

    #[test]
    fn test_media_items_rows_and_argv() {
        let items = media_items(&parse_metadata(METADATA), true);
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Song with tab · Band",
                "Pause",
                "Next",
                "Previous",
                "Stop",
                "A video",
                "Play",
                "Next",
                "Previous",
                "Stop",
            ]
        );
        assert_eq!(items[0].subtitle, "Playing · spotify");
        assert!(items[0].primary.is_none());
        assert_eq!(items[0].icon_names[0], "spotify");
        assert_eq!(items[1].icon_names, vec!["media-playback-pause"]);
        assert_eq!(items[6].icon_names, vec!["media-playback-start"]);

        let next = items[7].primary.as_ref().unwrap();
        assert!(next.keep_open && next.confirm.is_none());
        assert!(matches!(
            &next.action,
            DeferredAction::Command(argv)
                if argv == &["playerctl", "-p", "firefox.instance_1_23", "next"]
        ));
        assert_eq!(items[7].identity, "firefox.instance_1_23/next");

        let closing = media_items(&parse_metadata(METADATA), false);
        assert!(!closing[1].primary.as_ref().unwrap().keep_open);
    }

    #[test]
    fn test_media_items_without_players() {
        let items = media_items(&[], true);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, NO_PLAYERS);
        assert!(items[0].primary.is_none());

        let header = header_item(&Player {
            instance: "mpv".into(),
            name: "mpv".into(),
            ..Player::default()
        });
        assert_eq!(header.title, "No track information");
        assert_eq!(header.subtitle, "mpv");
    }

    fn fake_playerctl(
        metadata: Result<&'static str, &'static str>,
    ) -> impl Fn(&[&str]) -> Result<Vec<u8>, String> {
        move |args| match args {
            ["-l"] => Ok(b"spotify\nmpv\n".to_vec()),
            ["-a", "metadata", "--format", format] if *format == FORMAT => metadata
                .map(|output| output.as_bytes().to_vec())
                .map_err(str::to_string),
            _ => panic!("unexpected playerctl call {args:?}"),
        }
    }

    #[test]
    fn test_load_players_without_metadata_keeps_controls() {
        let items =
            load_players_with(fake_playerctl(Err("exited with exit status: 1")), true).unwrap();
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles[0], "No track information");
        assert_eq!(titles[1], "Play/Pause");
        assert_eq!(items.len(), 10);

        let items = load_players_with(fake_playerctl(Ok(METADATA)), true).unwrap();
        assert_eq!(items[0].title, "Song with tab · Band");
        assert_eq!(items[5].title, "No track information");
    }

    #[test]
    fn test_load_players_reports_failures() {
        for error in ["timed out after 2s", "output is larger than 256 KiB"] {
            assert_eq!(
                load_players_with(fake_playerctl(Err(error)), true).unwrap_err(),
                format!("playerctl: {error}")
            );
        }
        let failing_list = |_: &[&str]| Err::<Vec<u8>, _>("timed out after 2s".to_string());
        assert_eq!(
            load_players_with(failing_list, true).unwrap_err(),
            "playerctl: timed out after 2s"
        );
        let empty = |args: &[&str]| match args {
            ["-l"] => Ok(Vec::new()),
            _ => panic!("no metadata lookup without players"),
        };
        assert_eq!(load_players_with(empty, true).unwrap()[0].title, NO_PLAYERS);
    }
}
