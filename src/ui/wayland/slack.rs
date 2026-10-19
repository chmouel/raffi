//! Slack addon: open conversations, search messages, read unread ones, and
//! set the status, presence and notification snooze, with a user (`xoxp-`)
//! or browser session (`xoxc-`) token. Web API calls live in `slack_api`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, TimeZone};
use serde_json::{json, Value};

use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem, QUERY_ROW};
use super::preview::PreviewBody;
use super::slack_api::{self, Client, Conversation, ConversationKind, Directory, User, Workspace};
use super::tab_preview::Wanted;
use crate::SlackOpenIn;

pub(super) const SLACK_ICON: &str = "slack";
const CHANNEL_ICONS: [&str; 3] = [SLACK_ICON, "internet-group-chat", "user-available"];
const USER_ICONS: [&str; 3] = ["avatar-default", "user-available", SLACK_ICON];
/// Search hits shown, at most.
const SEARCH_COUNT: &str = "40";
/// Conversations checked for unread messages, at most, as in Raycast.
const MAX_UNREAD: usize = 30;
/// Messages shown in a preview.
const PREVIEW_MESSAGES: usize = 12;
const MAX_MESSAGE_CHARS: usize = 600;
const MAX_TITLE_CHARS: usize = 120;
const MAX_AVATAR_BYTES: u64 = 512 * 1024;
/// Wait before loading a preview, so moving quickly through the rows does
/// not send one request per row.
const PREVIEW_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// What the text after the keyword asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum View {
    /// Actions, conversations and people, filtered locally.
    Home,
    /// Messages matching the search terms.
    Search(String),
    /// Conversations with unread messages.
    Unread,
    /// People matching the name.
    People(String),
}

impl View {
    /// The text a debounced view looks up.
    pub fn terms(&self) -> Option<&str> {
        match self {
            Self::Search(terms) | Self::People(terms) => Some(terms),
            Self::Home | Self::Unread => None,
        }
    }
}

/// The view `query` asks for and the text filtering its rows.
pub(super) fn parse_view(query: &str) -> (View, String) {
    let (word, rest) = match query.split_once(' ') {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (query, ""),
    };
    match word {
        "search" => (View::Search(rest.trim().to_string()), String::new()),
        "unread" => (View::Unread, rest.to_string()),
        "people" => (View::People(rest.trim().to_string()), String::new()),
        _ => (View::Home, query.to_string()),
    }
}

/// Minutes in `45`, `30m`, `1h`, `2h30m` or `1d`.
pub(super) fn parse_minutes(text: &str) -> Option<u64> {
    if let Ok(minutes) = text.parse::<u64>() {
        return (minutes > 0).then_some(minutes);
    }
    let mut total: u64 = 0;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            'm' => 1,
            'h' => 60,
            'd' => 24 * 60,
            _ => return None,
        };
        let value: u64 = number.parse().ok()?;
        number.clear();
        total = total.checked_add(value.checked_mul(unit)?)?;
    }
    (number.is_empty() && total > 0).then_some(total)
}

/// When a status clears itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Expiry {
    #[default]
    Never,
    Minutes(u64),
    /// At the end of the local day.
    Today,
}

impl Expiry {
    /// Seconds since the epoch, 0 for never, as `users.profile.set` wants.
    fn epoch(self, now: DateTime<Local>) -> i64 {
        match self {
            Self::Never => 0,
            Self::Minutes(minutes) => now.timestamp() + minutes as i64 * 60,
            Self::Today => local_at(now, 1, 0).map_or(0, |midnight| midnight.timestamp() - 1),
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Never => String::new(),
            Self::Minutes(minutes) => format!("for {}", describe_minutes(minutes)),
            Self::Today => "until the end of the day".to_string(),
        }
    }
}

/// `days` days after `now` at `hour`:00, local time.
fn local_at(now: DateTime<Local>, days: u64, hour: u32) -> Option<DateTime<Local>> {
    let date = now.date_naive().checked_add_days(chrono::Days::new(days))?;
    Local
        .from_local_datetime(&date.and_hms_opt(hour, 0, 0)?)
        .earliest()
}

pub(super) fn describe_minutes(minutes: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    match (minutes / 60, minutes % 60) {
        (0, m) => plural(m, "minute"),
        (h, 0) if h % 24 == 0 => plural(h / 24, "day"),
        (h, 0) => plural(h, "hour"),
        (h, m) => format!("{} {}", plural(h, "hour"), plural(m, "minute")),
    }
}

/// A status as typed: `<text> [:emoji:] [30m|1h|2h30m|1d|today]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StatusArgs {
    pub text: String,
    pub emoji: String,
    pub expiry: Expiry,
}

impl StatusArgs {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.emoji.is_empty()
    }
}

pub(super) fn parse_status(args: &str) -> StatusArgs {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let mut expiry = Expiry::Never;
    match words.last() {
        Some(last) if last.eq_ignore_ascii_case("today") => {
            expiry = Expiry::Today;
            words.pop();
        }
        Some(last) if last.starts_with(|c: char| c.is_ascii_digit()) => {
            // A bare number is part of the text, as in "room 42".
            if let Some(minutes) = parse_minutes(last).filter(|_| last.parse::<u64>().is_err()) {
                expiry = Expiry::Minutes(minutes);
                words.pop();
            }
        }
        _ => {}
    }
    let mut emoji = String::new();
    words.retain(|word| {
        if emoji.is_empty() && is_emoji_code(word) {
            emoji = word.to_string();
            return false;
        }
        true
    });
    StatusArgs {
        text: words.join(" "),
        emoji,
        expiry,
    }
}

fn is_emoji_code(word: &str) -> bool {
    word.len() > 2
        && word.starts_with(':')
        && word.ends_with(':')
        && word[1..word.len() - 1]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_+-'".contains(c))
}

/// Slack message markup as plain text: mentions and links by name, entities
/// decoded.
pub(super) fn plain_text(text: &str, directory: Option<&Directory>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        out.push_str(&render_reference(&after[..end], directory));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn render_reference(inner: &str, directory: Option<&Directory>) -> String {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(label).filter(|label| !label.is_empty())),
        None => (inner, None),
    };
    if let Some(user) = target.strip_prefix('@') {
        let name = label
            .map(|label| label.trim_start_matches('@').to_string())
            .or_else(|| directory?.user(user).map(|user| user.label().to_string()))
            .unwrap_or_else(|| user.to_string());
        return format!("@{name}");
    }
    if let Some(channel) = target.strip_prefix('#') {
        let name = label
            .map(str::to_string)
            .or_else(|| {
                directory?
                    .conversation(channel)
                    .map(|conversation| conversation.name.clone())
            })
            .unwrap_or_else(|| channel.to_string());
        return format!("#{name}");
    }
    if let Some(special) = target.strip_prefix('!') {
        return match label {
            Some(label) => label.to_string(),
            None => format!("@{}", special.split('^').next().unwrap_or(special)),
        };
    }
    label.unwrap_or(target).to_string()
}

/// Seconds since the epoch of a message timestamp such as `1700000000.0001`.
fn ts_seconds(ts: &str) -> Option<i64> {
    ts.split('.').next()?.parse().ok()
}

/// A message time: the time today, the day this year, the date before.
pub(super) fn format_ts(ts: &str, now: DateTime<Local>) -> String {
    let Some(time) = ts_seconds(ts).and_then(|secs| Local.timestamp_opt(secs, 0).single()) else {
        return String::new();
    };
    if time.date_naive() == now.date_naive() {
        time.format("%H:%M").to_string()
    } else if time.year() == now.year() {
        time.format("%b %-d %H:%M").to_string()
    } else {
        time.format("%Y-%m-%d").to_string()
    }
}

fn truncate(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((end, _)) => (format!("{}…", text[..end].trim_end()), true),
        None => (text.to_string(), false),
    }
}

fn other(open_in: SlackOpenIn) -> SlackOpenIn {
    match open_in {
        SlackOpenIn::Desktop => SlackOpenIn::Browser,
        SlackOpenIn::Browser => SlackOpenIn::Desktop,
    }
}

fn target_name(open_in: SlackOpenIn) -> &'static str {
    match open_in {
        SlackOpenIn::Desktop => "Open in the Slack app",
        SlackOpenIn::Browser => "Open in the browser",
    }
}

pub(super) fn conversation_url(open_in: SlackOpenIn, workspace: &Workspace, id: &str) -> String {
    match open_in {
        SlackOpenIn::Desktop => format!("slack://channel?team={}&id={id}", workspace.team_id),
        SlackOpenIn::Browser => format!("https://app.slack.com/client/{}/{id}", workspace.team_id),
    }
}

/// The direct conversation with the user when there is one, else the user.
pub(super) fn user_url(
    open_in: SlackOpenIn,
    workspace: &Workspace,
    user: &str,
    direct: Option<&str>,
) -> String {
    match (direct, open_in) {
        (Some(direct), _) => conversation_url(open_in, workspace, direct),
        (None, SlackOpenIn::Desktop) => {
            format!("slack://user?team={}&id={user}", workspace.team_id)
        }
        (None, SlackOpenIn::Browser) => {
            format!("https://app.slack.com/client/{}/{user}", workspace.team_id)
        }
    }
}

pub(super) fn message_url(
    open_in: SlackOpenIn,
    workspace: &Workspace,
    channel: &str,
    ts: &str,
    permalink: &str,
) -> String {
    match open_in {
        SlackOpenIn::Desktop => format!(
            "slack://channel?team={}&id={channel}&message={ts}",
            workspace.team_id
        ),
        SlackOpenIn::Browser if !permalink.is_empty() => permalink.to_string(),
        SlackOpenIn::Browser => format!(
            "{}archives/{channel}/p{}",
            workspace.url,
            ts.replace('.', "")
        ),
    }
}

/// Open `url`; a desktop link also focuses the Slack window.
fn open(url: String) -> CommandAction {
    let action = if url.starts_with("slack://") {
        DeferredAction::Slack(SlackAction::Open(url))
    } else {
        DeferredAction::Command(vec!["xdg-open".to_string(), url])
    };
    CommandAction {
        action,
        confirm: None,
        keep_open: false,
        progress: None,
    }
}

fn run(action: SlackAction, progress: &str) -> CommandAction {
    CommandAction {
        action: DeferredAction::Slack(action),
        confirm: None,
        keep_open: true,
        progress: Some(progress.to_string()),
    }
}

/// The name of a conversation as Slack shows it.
fn conversation_label(conversation: &Conversation, directory: &Directory) -> String {
    match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => format!("#{}", conversation.name),
        ConversationKind::Group => conversation.name.clone(),
        ConversationKind::Direct => conversation
            .user
            .as_deref()
            .and_then(|user| directory.user(user))
            .map_or_else(
                || conversation.name.clone(),
                |user| format!("@{}", user.label()),
            ),
    }
}

fn channel_label(id: &str, fallback: &str, directory: &Directory) -> String {
    match directory.conversation(id) {
        Some(conversation) => conversation_label(conversation, directory),
        None if fallback.is_empty() => id.to_string(),
        None => format!("#{fallback}"),
    }
}

/// How long notifications are snoozed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Snooze {
    Minutes(u64),
    /// Until 9:00 tomorrow, local time.
    UntilMorning,
}

/// What a Slack row does with Raffi kept open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlackAction {
    /// Replace the text after the keyword, such as `status `; handled by
    /// the app before anything runs.
    Fill(String),
    /// Set the status, or clear it when empty.
    SetStatus(StatusArgs),
    SetPresence {
        away: bool,
    },
    Snooze(Snooze),
    EndSnooze,
    MarkRead {
        channel: String,
        ts: String,
    },
    /// Fetch the conversations and people again.
    ReloadDirectory,
    /// Open a `slack://` link and focus the Slack window, which compositors
    /// such as Hyprland do not let Slack do itself.
    Open(String),
}

/// Run `action` against Slack. Blocking: run it off the UI thread.
pub(super) fn run_action(action: &SlackAction) -> Result<(), String> {
    if let SlackAction::Open(url) = action {
        return open_in_app(url);
    }
    let client = slack_api::current_client()?;
    let now = Local::now();
    match action {
        SlackAction::Fill(_) => Ok(()),
        SlackAction::SetStatus(status) => {
            let profile = json!({
                "status_text": status.text,
                "status_emoji": status.emoji,
                "status_expiration": status.expiry.epoch(now),
            })
            .to_string();
            client
                .call("users.profile.set", &[("profile", &profile)])
                .map(drop)
        }
        SlackAction::SetPresence { away } => client
            .call(
                "users.setPresence",
                &[("presence", if *away { "away" } else { "auto" })],
            )
            .map(drop),
        SlackAction::Snooze(snooze) => {
            let minutes = match snooze {
                Snooze::Minutes(minutes) => *minutes,
                Snooze::UntilMorning => local_at(now, 1, 9)
                    .map(|morning| (morning.timestamp() - now.timestamp()).max(60) as u64 / 60)
                    .ok_or("cannot tell when tomorrow morning is")?,
            };
            client
                .call("dnd.setSnooze", &[("num_minutes", &minutes.to_string())])
                .map(drop)
        }
        SlackAction::EndSnooze => client.call("dnd.endSnooze", &[]).map(drop),
        SlackAction::MarkRead { channel, ts } => client
            .call("conversations.mark", &[("channel", channel), ("ts", ts)])
            .map(drop),
        SlackAction::ReloadDirectory => slack_api::directory(&client, true).map(drop),
        SlackAction::Open(_) => Ok(()),
    }
}

/// How long to wait for the Slack window when the link starts Slack.
const SLACK_START_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Hand `url` to the Slack app, then focus its window. Slack moves to the
/// conversation whether or not it has focus.
fn open_in_app(url: &str) -> Result<(), String> {
    DeferredAction::Command(vec!["xdg-open".to_string(), url.to_string()]).run()?;
    if let Err(error) = super::windows::raise_app_window("slack", SLACK_START_WAIT) {
        crate::debug_log!("slack: cannot focus the Slack window: {error}");
    }
    Ok(())
}

/// The user's own status, presence and snooze, shown on the action rows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Live {
    /// Emoji, text and expiry in seconds since the epoch, 0 for never.
    pub status: Option<(String, String, i64)>,
    pub snoozed_until: Option<i64>,
    pub away: Option<bool>,
}

/// Ask for the live state in parallel; what fails is left unknown.
fn fetch_live(client: &Client) -> Live {
    std::thread::scope(|scope| {
        let profile = scope.spawn(|| client.call("users.profile.get", &[]).ok());
        let dnd = scope.spawn(|| client.call("dnd.info", &[]).ok());
        let presence = scope.spawn(|| client.call("users.getPresence", &[]).ok());
        let join =
            |handle: std::thread::ScopedJoinHandle<'_, Option<Value>>| handle.join().ok().flatten();
        parse_live(join(profile), join(dnd), join(presence))
    })
}

pub(super) fn parse_live(
    profile: Option<Value>,
    dnd: Option<Value>,
    presence: Option<Value>,
) -> Live {
    let status = profile.map(|value| {
        let profile = &value["profile"];
        let text = |key: &str| profile[key].as_str().unwrap_or_default().to_string();
        (
            text("status_emoji"),
            text("status_text"),
            profile["status_expiration"].as_i64().unwrap_or(0),
        )
    });
    Live {
        status,
        snoozed_until: dnd
            .filter(|dnd| dnd["snooze_enabled"].as_bool() == Some(true))
            .map(|dnd| dnd["snooze_endtime"].as_i64().unwrap_or(0)),
        away: presence.and_then(|presence| presence["presence"].as_str().map(|p| p == "away")),
    }
}

fn row(identity: &str, title: impl Into<String>, icons: &[&str]) -> CommandItem {
    let mut item = CommandItem::info(identity, title.into(), icons[0]);
    item.icon_names = icons.iter().map(|icon| icon.to_string()).collect();
    item
}

fn action_row(
    name: &str,
    title: &str,
    subtitle: String,
    keywords: &str,
    icons: &[&str],
    action: CommandAction,
) -> CommandItem {
    let mut item = row(&format!("slack:do:{name}"), title, icons);
    item.subtitle = subtitle;
    item.match_text = format!("{title} {keywords}");
    item.details = vec![("Enter".to_string(), title.to_string())];
    item.primary = Some(action);
    item
}

fn fill(text: &str) -> CommandAction {
    CommandAction {
        action: DeferredAction::Slack(SlackAction::Fill(text.to_string())),
        confirm: None,
        keep_open: true,
        progress: None,
    }
}

fn time_of(epoch: i64, now: DateTime<Local>) -> String {
    format_ts(&epoch.to_string(), now)
}

fn describe_status(emoji: &str, text: &str, expiry: i64, now: DateTime<Local>) -> String {
    let mut status = [emoji, text]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if expiry > 0 {
        status.push_str(&format!(", until {}", time_of(expiry, now)));
    }
    status
}

const SNOOZE_ICONS: [&str; 3] = [
    "notifications-disabled",
    "notification-disabled-symbolic",
    SLACK_ICON,
];

fn action_rows(live: &Live, now: DateTime<Local>) -> Vec<CommandItem> {
    let mut rows = vec![
        action_row(
            "unread",
            "Unread messages",
            "Conversations with unread messages".to_string(),
            "new",
            &["mail-unread", "mail-message-new", SLACK_ICON],
            fill("unread "),
        ),
        action_row(
            "search",
            "Search messages",
            "Type the words to look for".to_string(),
            "find",
            &["edit-find", "system-search", SLACK_ICON],
            fill("search "),
        ),
        action_row(
            "people",
            "Find people",
            "Type a name to look for".to_string(),
            "person user someone",
            &["system-users", "avatar-default", SLACK_ICON],
            fill("people "),
        ),
    ];
    let status = live
        .status
        .as_ref()
        .filter(|(emoji, text, _)| !emoji.is_empty() || !text.is_empty());
    rows.push(action_row(
        "status",
        "Set status",
        match status {
            Some((emoji, text, expiry)) => {
                format!("Now: {}", describe_status(emoji, text, *expiry, now))
            }
            None => "Type the status, an :emoji: and how long it lasts".to_string(),
        },
        "",
        &["user-info", "avatar-default", SLACK_ICON],
        fill("status "),
    ));
    if let Some((emoji, text, expiry)) = status {
        rows.push(action_row(
            "clear-status",
            "Clear status",
            describe_status(emoji, text, *expiry, now),
            "remove",
            &["edit-clear", "edit-delete", SLACK_ICON],
            run(
                SlackAction::SetStatus(StatusArgs::default()),
                "Clearing status...",
            ),
        ));
    }
    if live.away != Some(false) {
        rows.push(action_row(
            "active",
            "Set yourself active",
            if live.away == Some(true) {
                "You are away"
            } else {
                ""
            }
            .to_string(),
            "presence online available",
            &["user-available", SLACK_ICON],
            run(
                SlackAction::SetPresence { away: false },
                "Setting active...",
            ),
        ));
    }
    if live.away != Some(true) {
        rows.push(action_row(
            "away",
            "Set yourself away",
            if live.away == Some(false) {
                "You are active"
            } else {
                ""
            }
            .to_string(),
            "presence offline",
            &["user-away", SLACK_ICON],
            run(SlackAction::SetPresence { away: true }, "Setting away..."),
        ));
    }
    if let Some(until) = live.snoozed_until.filter(|until| *until > now.timestamp()) {
        rows.push(action_row(
            "resume",
            "Resume notifications",
            format!("Snoozed until {}", time_of(until, now)),
            "snooze dnd do not disturb end",
            &["preferences-desktop-notification", SLACK_ICON],
            run(SlackAction::EndSnooze, "Resuming notifications..."),
        ));
    }
    for minutes in [30, 60, 120] {
        let title = format!("Snooze notifications for {}", describe_minutes(minutes));
        rows.push(action_row(
            &format!("snooze-{minutes}"),
            &title,
            String::new(),
            "dnd do not disturb pause",
            &SNOOZE_ICONS,
            run(SlackAction::Snooze(Snooze::Minutes(minutes)), "Snoozing..."),
        ));
    }
    rows.push(action_row(
        "snooze-morning",
        "Snooze notifications until tomorrow 9:00",
        String::new(),
        "dnd do not disturb pause",
        &SNOOZE_ICONS,
        run(SlackAction::Snooze(Snooze::UntilMorning), "Snoozing..."),
    ));
    rows
}

/// The row run with the text typed after `status ` or `snooze `.
pub(super) fn query_row(query: &str, icon: Option<String>) -> Option<CommandItem> {
    // The keyword query arrives trimmed: `status ` is `status`.
    let (word, args) = query.split_once(' ').unwrap_or((query, ""));
    let args = args.trim();
    let mut item = match word {
        "status" => {
            let status = parse_status(args);
            let mut item = row(QUERY_ROW, "", &[SLACK_ICON]);
            if status.is_empty() {
                item.title = "Type the status to set".to_string();
                item.subtitle = "Like: Lunch :pizza: 1h, or Away :palm_tree: today".to_string();
            } else {
                item.title = format!(
                    "Set status to {}",
                    describe_status(&status.emoji, &status.text, 0, Local::now())
                );
                item.subtitle = status.expiry.describe();
                item.primary = Some(run(SlackAction::SetStatus(status), "Setting status..."));
            }
            item
        }
        "snooze" => {
            let mut item = row(QUERY_ROW, "", &[SLACK_ICON]);
            match parse_minutes(args) {
                Some(minutes) => {
                    item.title = format!("Snooze notifications for {}", describe_minutes(minutes));
                    item.primary = Some(run(
                        SlackAction::Snooze(Snooze::Minutes(minutes)),
                        "Snoozing...",
                    ));
                }
                None => {
                    item.title = "Type how long to snooze notifications".to_string();
                    item.subtitle = "Like: 45m, 1h or 2h30m".to_string();
                }
            }
            item
        }
        _ => return None,
    };
    item.icon = icon;
    if item.primary.is_some() {
        item.details = vec![("Enter".to_string(), item.title.clone())];
    }
    Some(item)
}

fn first_line(text: &str) -> String {
    truncate(
        text.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(""),
        MAX_TITLE_CHARS,
    )
    .0
}

fn open_details(open_in: SlackOpenIn) -> Vec<(String, String)> {
    vec![
        ("Enter".to_string(), target_name(open_in).to_string()),
        (
            "Ctrl+Enter".to_string(),
            target_name(other(open_in)).to_string(),
        ),
    ]
}

fn conversation_row(
    conversation: &Conversation,
    directory: &Directory,
    open_in: SlackOpenIn,
) -> CommandItem {
    let workspace = &directory.workspace;
    let mut item = row(
        &format!("slack:conv:{}", conversation.id),
        conversation_label(conversation, directory),
        &CHANNEL_ICONS,
    );
    let topic = plain_text(&conversation.topic, Some(directory));
    let purpose = plain_text(&conversation.purpose, Some(directory));
    item.subtitle = first_line(if topic.trim().is_empty() {
        &purpose
    } else {
        &topic
    });
    item.match_text = conversation.name.clone();
    item.badge = Some(
        match conversation.kind {
            ConversationKind::Channel => "Channel",
            ConversationKind::Private => "Private",
            ConversationKind::Group => "Group",
            ConversationKind::Direct => "Direct",
        }
        .to_string(),
    );
    if !topic.trim().is_empty() {
        item.details.push(("Topic".to_string(), topic.clone()));
    }
    if !purpose.trim().is_empty() && purpose != topic {
        item.details.push(("Purpose".to_string(), purpose));
    }
    if let Some(members) = conversation.members {
        item.details
            .push(("Members".to_string(), members.to_string()));
    }
    item.details.extend(open_details(open_in));
    item.primary = Some(open(conversation_url(open_in, workspace, &conversation.id)));
    item.secondary = Some(open(conversation_url(
        other(open_in),
        workspace,
        &conversation.id,
    )));
    item
}

fn user_row(
    user: &User,
    directory: &Directory,
    open_in: SlackOpenIn,
    avatar: Option<&String>,
    now: DateTime<Local>,
) -> CommandItem {
    let workspace = &directory.workspace;
    let mut icons: Vec<&str> = avatar.map(String::as_str).into_iter().collect();
    icons.extend(USER_ICONS);
    let mut item = row(&format!("slack:user:{}", user.id), user.label(), &icons);
    let status = describe_status(&user.status_emoji, &user.status_text, 0, now);
    item.subtitle = [
        format!("@{}", user.name),
        user.title.clone(),
        status.clone(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty() && part != "@")
    .collect::<Vec<_>>()
    .join(" · ");
    item.match_text = format!("{} {} {}", user.label(), user.real_name, user.name);
    item.badge = Some("Person".to_string());
    let mut details = vec![("Handle".to_string(), format!("@{}", user.name))];
    if user.real_name != user.label() && !user.real_name.is_empty() {
        details.push(("Name".to_string(), user.real_name.clone()));
    }
    if !user.title.is_empty() {
        details.push(("Title".to_string(), user.title.clone()));
    }
    if !status.is_empty() {
        details.push(("Status".to_string(), status));
    }
    if let Some(offset) = chrono::FixedOffset::east_opt(user.tz_offset as i32) {
        let local = now.with_timezone(&offset).format("%H:%M").to_string();
        let zone = if user.tz_label.is_empty() {
            String::new()
        } else {
            format!(" ({})", user.tz_label)
        };
        details.push(("Local time".to_string(), format!("{local}{zone}")));
    }
    item.details = details;
    item.details.extend(open_details(open_in));
    let direct = directory
        .direct_with(&user.id)
        .map(|direct| direct.id.as_str());
    item.primary = Some(open(user_url(open_in, workspace, &user.id, direct)));
    item.secondary = Some(open(user_url(other(open_in), workspace, &user.id, direct)));
    item
}

/// Actions, then channels and group conversations by name, then people.
pub(super) fn home_rows(
    directory: &Directory,
    live: &Live,
    open_in: SlackOpenIn,
    avatars: &HashMap<String, String>,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let mut rows = action_rows(live, now);
    let mut conversations: Vec<&Conversation> = directory
        .conversations
        .iter()
        .filter(|conversation| conversation.kind != ConversationKind::Direct)
        .collect();
    conversations.sort_by_key(|conversation| {
        (
            conversation.kind == ConversationKind::Group,
            conversation.name.to_lowercase(),
        )
    });
    rows.extend(
        conversations
            .into_iter()
            .map(|conversation| conversation_row(conversation, directory, open_in)),
    );
    let mut users: Vec<&User> = directory.users.iter().collect();
    users.sort_by_key(|user| user.label().to_lowercase());
    rows.extend(
        users
            .into_iter()
            .map(|user| user_row(user, directory, open_in, avatars.get(&user.id), now)),
    );
    rows
}

fn avatar_dir() -> Option<PathBuf> {
    crate::ui::get_slack_cache_dir()
        .ok()
        .map(|dir| dir.join("avatars"))
}

/// The downloaded avatars by user id.
pub(super) fn cached_avatars(dir: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = path.file_stem()?.to_str()?.to_string();
            Some((id, path.to_str()?.to_string()))
        })
        .collect()
}

/// The user's avatar, downloaded once. Blocking.
fn fetch_avatar(user: &User) -> Option<String> {
    let dir = avatar_dir()?;
    if let Some(path) = cached_avatars(&dir).remove(&user.id) {
        return Some(path);
    }
    if !user.avatar.starts_with("https://") {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let data = super::favicons::download(&agent, &user.avatar, MAX_AVATAR_BYTES)?;
    let extension = super::favicons::extension(&data)?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.{extension}", user.id));
    std::fs::write(&path, data).ok()?;
    path.to_str().map(str::to_string)
}

/// The client and directory, refreshing a directory past its freshness in
/// the background so this load does not wait for it.
fn connect(
    config: &crate::SlackAddonConfig,
) -> Result<(std::sync::Arc<Client>, std::sync::Arc<Directory>), String> {
    let client = slack_api::client(
        config.token_command.as_deref(),
        config.cookie_command.as_deref(),
    )?;
    let directory = slack_api::directory(&client, false)?;
    if !directory.is_fresh(slack_api::now_secs()) {
        refresh_directory_in_background(client.clone());
    }
    Ok((client, directory))
}

fn refresh_directory_in_background(client: std::sync::Arc<Client>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        if let Err(error) = slack_api::directory(&client, true) {
            crate::debug_log!("slack: directory refresh failed: {error}");
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// The home rows. Blocking.
pub(super) fn load_home(config: crate::SlackAddonConfig) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config)?;
    let live = fetch_live(&client);
    // The directory may come from the cache, so this is the first call that
    // tells whether the user has been logged out.
    if let Some(error) = client.rejection() {
        return Err(error);
    }
    let avatars = avatar_dir()
        .map(|dir| cached_avatars(&dir))
        .unwrap_or_default();
    let mut rows = home_rows(&directory, &live, config.open_in, &avatars, Local::now());
    rows.push(action_row(
        "reload",
        "Reload conversations and people",
        format!(
            "{} listed, fetched {} ago",
            directory.conversations.len() + directory.users.len(),
            describe_minutes((directory.age(slack_api::now_secs()).as_secs() / 60).max(1))
        ),
        "refresh",
        &["view-refresh", SLACK_ICON],
        run(SlackAction::ReloadDirectory, "Reloading..."),
    ));
    Ok(rows)
}

/// The author of a message as Slack shows it.
fn author(message: &Value, directory: &Directory) -> String {
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    text(&message["user"])
        .and_then(|id| directory.user(&id).map(|user| user.label().to_string()))
        .or_else(|| text(&message["user_profile"]["display_name"]))
        .or_else(|| text(&message["user_profile"]["real_name"]))
        .or_else(|| text(&message["username"]))
        .or_else(|| text(&message["bot_profile"]["name"]))
        .or_else(|| text(&message["user"]))
        .unwrap_or_else(|| "Someone".to_string())
}

/// A message as plain text, naming its files when it has no text.
fn message_text(message: &Value, directory: &Directory) -> String {
    let text = plain_text(
        message["text"].as_str().unwrap_or_default(),
        Some(directory),
    );
    if !text.trim().is_empty() {
        return text;
    }
    let files: Vec<&str> = message["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| file["name"].as_str().or(file["title"].as_str()))
        .collect();
    if !files.is_empty() {
        return format!("[file: {}]", files.join(", "));
    }
    message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|attachment| attachment["fallback"].as_str())
        .map(|fallback| plain_text(fallback, Some(directory)))
        .unwrap_or_default()
}

/// The `thread_ts` a permalink points into, if any.
fn permalink_thread(permalink: &str) -> Option<&str> {
    let query = permalink.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("thread_ts="))
        .filter(|ts| !ts.is_empty())
}

pub(super) fn search_rows(
    reply: &Value,
    directory: &Directory,
    open_in: SlackOpenIn,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let workspace = &directory.workspace;
    let matches = reply["messages"]["matches"].as_array();
    matches
        .into_iter()
        .flatten()
        .filter_map(|hit| {
            let channel = hit["channel"]["id"].as_str()?;
            let ts = hit["ts"].as_str()?;
            let permalink = hit["permalink"].as_str().unwrap_or_default();
            let thread = permalink_thread(permalink).filter(|thread| *thread != ts);
            let mut identity = format!("slack:msg:{channel}:{ts}");
            if let Some(thread) = thread {
                identity.push_str(&format!(":{thread}"));
            }
            let text = message_text(hit, directory);
            let where_ = channel_label(
                channel,
                hit["channel"]["name"].as_str().unwrap_or_default(),
                directory,
            );
            let from = author(hit, directory);
            let mut item = row(&identity, first_line(&text), &CHANNEL_ICONS);
            item.subtitle = format!("{where_} · {from} · {}", format_ts(ts, now));
            item.badge = Some(
                if thread.is_some() {
                    "Thread"
                } else {
                    "Message"
                }
                .to_string(),
            );
            item.details = vec![
                ("Conversation".to_string(), where_),
                ("From".to_string(), from),
                ("Sent".to_string(), full_time(ts)),
            ];
            item.details.extend(open_details(open_in));
            item.primary = Some(open(message_url(
                open_in, workspace, channel, ts, permalink,
            )));
            item.secondary = Some(open(message_url(
                other(open_in),
                workspace,
                channel,
                ts,
                permalink,
            )));
            Some(item)
        })
        .collect()
}

fn full_time(ts: &str) -> String {
    ts_seconds(ts)
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Messages matching `terms`. Blocking.
pub(super) fn load_search(
    config: crate::SlackAddonConfig,
    terms: String,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config)?;
    let reply = client.call(
        "search.messages",
        &[
            ("query", &terms),
            ("count", SEARCH_COUNT),
            ("highlight", "false"),
        ],
    )?;
    let rows = search_rows(&reply, &directory, config.open_in, Local::now());
    if rows.is_empty() {
        return Ok(vec![row(
            "slack:none",
            format!("No messages match \u{201c}{terms}\u{201d}"),
            &["edit-find", SLACK_ICON],
        )]);
    }
    Ok(rows)
}

/// People whose name matches `query`: the whole organization on Enterprise
/// Grid, where the directory only holds the people of direct conversations,
/// else the directory. Blocking.
pub(super) fn load_people(
    config: crate::SlackAddonConfig,
    query: String,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config)?;
    let found: Vec<User> = if directory.workspace.enterprise && client.is_session() {
        let mut found = slack_api::search_people(&client, &directory.workspace, &query)?;
        // People you have a conversation with first.
        found.sort_by_key(|user| directory.direct_with(&user.id).is_none());
        found
    } else {
        let query = query.to_lowercase();
        directory
            .users
            .iter()
            .filter(|user| {
                [user.label(), &user.real_name, &user.name]
                    .iter()
                    .any(|name| name.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    };
    let avatars = avatar_dir()
        .map(|dir| cached_avatars(&dir))
        .unwrap_or_default();
    let now = Local::now();
    let mut rows: Vec<CommandItem> = found
        .iter()
        .map(|user| user_row(user, &directory, config.open_in, avatars.get(&user.id), now))
        .collect();
    if rows.is_empty() {
        rows.push(row(
            "slack:none",
            format!("Nobody is called \u{201c}{query}\u{201d}"),
            &["system-users", SLACK_ICON],
        ));
    }
    Ok(rows)
}

/// A conversation with unread messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Unread {
    pub channel: String,
    pub last_read: String,
    /// Timestamp of the newest message.
    pub latest: String,
    pub mentions: u64,
    /// Unread messages, when counted.
    pub count: Option<usize>,
}

/// Conversations `client.counts` reports with unread messages, newest
/// first.
pub(super) fn counts_unread(reply: &Value) -> Vec<Unread> {
    let mut unread: Vec<Unread> = ["channels", "mpims", "ims"]
        .into_iter()
        .filter_map(|key| reply[key].as_array())
        .flatten()
        .filter(|entry| entry["has_unreads"].as_bool() == Some(true))
        .filter_map(|entry| {
            let text = |key: &str| entry[key].as_str().unwrap_or_default().to_string();
            Some(Unread {
                channel: entry["id"].as_str()?.to_string(),
                last_read: text("last_read"),
                latest: text("latest"),
                mentions: entry["mention_count"].as_u64().unwrap_or(0),
                count: None,
            })
        })
        .collect();
    unread.sort_by(|a, b| b.latest.cmp(&a.latest));
    unread.truncate(MAX_UNREAD);
    unread
}

/// The conversation id `entry` names: `#channel`, `@handle` for the direct
/// conversation, or an id.
pub(super) fn resolve_conversation(entry: &str, directory: &Directory) -> Option<String> {
    let entry = entry.trim();
    let direct = |user: &User| directory.direct_with(&user.id).map(|c| c.id.clone());
    if let Some(name) = entry.strip_prefix('#') {
        return directory
            .conversations
            .iter()
            .find(|c| c.kind != ConversationKind::Direct && c.name == name)
            .map(|c| c.id.clone());
    }
    if let Some(handle) = entry.strip_prefix('@') {
        return directory
            .users
            .iter()
            .find(|user| user.name == handle)
            .and_then(direct);
    }
    match directory.user(entry) {
        Some(user) => direct(user),
        None => (!entry.is_empty()).then(|| entry.to_string()),
    }
}

/// Count the unread messages of each configured conversation, a few at a
/// time. Blocking.
fn check_configured(client: &Client, directory: &Directory, entries: &[String]) -> Vec<Unread> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let channels: Vec<String> = entries
        .iter()
        .filter_map(|entry| resolve_conversation(entry, directory))
        .take(MAX_UNREAD)
        .collect();
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                while let Some(channel) = channels.get(next.fetch_add(1, Ordering::Relaxed)) {
                    match check_conversation(client, channel) {
                        Ok(Some(unread)) => {
                            found.lock().unwrap_or_else(|e| e.into_inner()).push(unread)
                        }
                        Ok(None) => {}
                        Err(error) => crate::debug_log!("slack: cannot check {channel}: {error}"),
                    }
                }
            });
        }
    });
    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort_by(|a, b| b.latest.cmp(&a.latest));
    found
}

fn check_conversation(client: &Client, channel: &str) -> Result<Option<Unread>, String> {
    let info = client.call("conversations.info", &[("channel", channel)])?;
    let last_read = info["channel"]["last_read"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let history = client.call(
        "conversations.history",
        &[
            ("channel", channel),
            ("oldest", &last_read),
            ("limit", "100"),
        ],
    )?;
    let messages = history["messages"].as_array().cloned().unwrap_or_default();
    let Some(latest) = messages.first().and_then(|m| m["ts"].as_str()) else {
        return Ok(None);
    };
    Ok(Some(Unread {
        channel: channel.to_string(),
        last_read,
        latest: latest.to_string(),
        mentions: 0,
        count: Some(messages.len()),
    }))
}

pub(super) fn unread_rows(
    unread: &[Unread],
    directory: &Directory,
    open_in: SlackOpenIn,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let workspace = &directory.workspace;
    unread
        .iter()
        .map(|entry| {
            let label = channel_label(&entry.channel, "", directory);
            let mut item = row(
                &format!("slack:unread:{}:{}", entry.channel, entry.last_read),
                label.clone(),
                &["mail-unread", SLACK_ICON],
            );
            let mut parts = Vec::new();
            match entry.count {
                Some(count) if count >= 100 => parts.push("100+ unread".to_string()),
                Some(count) => parts.push(format!("{count} unread")),
                None => {}
            }
            if entry.mentions > 0 {
                let s = if entry.mentions == 1 { "" } else { "s" };
                parts.push(format!("{} mention{s}", entry.mentions));
            }
            if !entry.latest.is_empty() {
                parts.push(format!("latest {}", format_ts(&entry.latest, now)));
            }
            item.subtitle = parts.join(" · ");
            item.match_text = label.trim_start_matches(['#', '@']).to_string();
            item.badge = directory
                .conversation(&entry.channel)
                .map(|c| match c.kind {
                    ConversationKind::Direct => "Direct",
                    ConversationKind::Group => "Group",
                    _ => "Channel",
                })
                .map(str::to_string);
            item.details = vec![
                ("Enter".to_string(), target_name(open_in).to_string()),
                ("Ctrl+Enter".to_string(), "Mark as read".to_string()),
            ];
            item.primary = Some(open(conversation_url(open_in, workspace, &entry.channel)));
            if !entry.latest.is_empty() {
                item.secondary = Some(run(
                    SlackAction::MarkRead {
                        channel: entry.channel.clone(),
                        ts: entry.latest.clone(),
                    },
                    "Marking as read...",
                ));
            }
            item
        })
        .collect()
}

/// Conversations with unread messages, from `client.counts` when the token
/// allows it, else from the configured list. Blocking.
pub(super) fn load_unread(config: crate::SlackAddonConfig) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config)?;
    let unread = match client.call("client.counts", &[("thread_counts_by_channel", "true")]) {
        Ok(reply) => counts_unread(&reply),
        Err(error) if config.unread_conversations.is_empty() => {
            crate::debug_log!("slack: client.counts failed: {error}");
            return Err("Slack does not list unread conversations for this token: \
                 set addons.slack.unread_conversations to the ones to check"
                .to_string());
        }
        Err(error) => {
            crate::debug_log!("slack: client.counts failed, checking the configured list: {error}");
            check_configured(&client, &directory, &config.unread_conversations)
        }
    };
    if unread.is_empty() {
        return Ok(vec![row(
            "slack:none",
            "No unread messages",
            &["mail-read", SLACK_ICON],
        )]);
    }
    Ok(unread_rows(
        &unread,
        &directory,
        config.open_in,
        Local::now(),
    ))
}

/// Preview bodies and, for a person, the avatar file.
pub(super) type Preview = (Vec<PreviewBody>, Option<String>);

/// Whether rows with `identity` have a preview to load.
pub(super) fn has_preview(identity: &str) -> bool {
    ["slack:conv:", "slack:user:", "slack:msg:", "slack:unread:"]
        .iter()
        .any(|prefix| identity.starts_with(prefix))
}

/// Messages as a preview text block, in the order given.
pub(super) fn messages_body(
    messages: &[Value],
    heading: &str,
    directory: &Directory,
    now: DateTime<Local>,
) -> PreviewBody {
    let mut truncated = false;
    let blocks: Vec<String> = messages
        .iter()
        .map(|message| {
            let (text, cut) = truncate(&message_text(message, directory), MAX_MESSAGE_CHARS);
            truncated |= cut;
            let ts = message["ts"].as_str().unwrap_or_default();
            let mut block = format!(
                "{} · {}\n{}",
                author(message, directory),
                format_ts(ts, now),
                text
            );
            let replies = message["reply_count"].as_u64().unwrap_or(0);
            if replies > 0 && message["thread_ts"].as_str() == Some(ts) {
                let s = if replies == 1 { "y" } else { "ies" };
                block.push_str(&format!("\n({replies} repl{s})"));
            }
            block
        })
        .collect();
    PreviewBody::Text {
        heading: Some(heading.to_string()),
        content: if blocks.is_empty() {
            "No messages".to_string()
        } else {
            blocks.join("\n\n")
        },
        truncated,
    }
}

fn messages(reply: &Value) -> Vec<Value> {
    reply["messages"].as_array().cloned().unwrap_or_default()
}

/// The newest messages of `channel`, oldest first.
fn recent(client: &Client, channel: &str, extra: &[(&str, &str)]) -> Result<Vec<Value>, String> {
    let limit = PREVIEW_MESSAGES.to_string();
    let mut params = vec![("channel", channel), ("limit", limit.as_str())];
    params.extend_from_slice(extra);
    let mut list = messages(&client.call("conversations.history", &params)?);
    list.reverse();
    Ok(list)
}

/// The preview of the row with `identity`, or None when another row was
/// selected meanwhile. Errors show in the pane, so a failing preview is not
/// asked for again. Blocking.
pub(super) fn load_preview(identity: &str, wanted: &Wanted) -> Option<Preview> {
    std::thread::sleep(PREVIEW_SETTLE);
    if !wanted.is(identity) {
        return None;
    }
    let bodies = slack_api::current_client()
        .and_then(|client| {
            let directory =
                slack_api::cached_directory().ok_or("Slack conversations are not loaded yet")?;
            Ok((client, directory))
        })
        .map(|(client, directory)| preview_bodies(identity, &client, directory));
    let (bodies, avatar) = match bodies {
        Ok((bodies, avatar)) => (bodies, avatar),
        Err(error) => (Err(error), None),
    };
    let bodies = bodies.unwrap_or_else(|error| {
        vec![PreviewBody::Text {
            heading: None,
            content: error,
            truncated: false,
        }]
    });
    Some((bodies, avatar))
}

/// The authors and mentioned people of `list`.
fn message_users(list: &[Value]) -> Vec<String> {
    let mut ids = Vec::new();
    for message in list {
        ids.extend(message["user"].as_str().map(str::to_string));
        let mut text = message["text"].as_str().unwrap_or_default();
        while let Some(start) = text.find("<@") {
            text = &text[start + 2..];
            let end = text.find(['>', '|']).unwrap_or(text.len());
            ids.push(text[..end].to_string());
        }
    }
    ids
}

fn preview_bodies(
    identity: &str,
    client: &Client,
    directory: std::sync::Arc<Directory>,
) -> (Result<Vec<PreviewBody>, String>, Option<String>) {
    let now = Local::now();
    let parts: Vec<&str> = identity.split(':').collect();
    let mut avatar = None;
    // On Enterprise Grid the directory lacks most people, so the ones the
    // messages show are looked up first.
    let messages_body = |list: &[Value], heading: &str, directory: &std::sync::Arc<Directory>| {
        let directory = slack_api::learn_users(client, directory, &message_users(list));
        messages_body(list, heading, &directory, now)
    };
    let directory = &directory;
    let bodies = match parts.as_slice() {
        ["slack", "conv", channel] => recent(client, channel, &[])
            .map(|list| vec![messages_body(&list, "Recent messages", directory)]),
        ["slack", "user", user] => {
            let directory = &slack_api::learn_users(client, directory, &[user.to_string()]);
            avatar = directory.user(user).and_then(fetch_avatar);
            match directory.direct_with(user) {
                Some(direct) => recent(client, &direct.id, &[])
                    .map(|list| vec![messages_body(&list, "Direct messages", directory)]),
                None => Ok(Vec::new()),
            }
        }
        ["slack", "msg", channel, ts, thread @ ..] => match thread.first() {
            Some(thread) => {
                let limit = (PREVIEW_MESSAGES * 4).to_string();
                client
                    .call(
                        "conversations.replies",
                        &[("channel", channel), ("ts", thread), ("limit", &limit)],
                    )
                    .map(|reply| {
                        let list = messages(&reply);
                        // The parent, then the replies up to the hit.
                        let upto = list
                            .iter()
                            .position(|m| m["ts"].as_str() == Some(ts))
                            .map_or(list.len(), |hit| hit + 1);
                        let start = upto.saturating_sub(PREVIEW_MESSAGES).max(1).min(upto);
                        let mut shown: Vec<Value> = list.first().cloned().into_iter().collect();
                        shown.extend(list[start..upto].iter().cloned());
                        vec![messages_body(&shown, "Thread", directory)]
                    })
            }
            None => recent(client, channel, &[("latest", ts), ("inclusive", "true")]).map(|list| {
                let start = list.len().saturating_sub(6);
                vec![messages_body(&list[start..], "Conversation", directory)]
            }),
        },
        ["slack", "unread", channel, last_read] => {
            let oldest = [("oldest", *last_read)];
            let extra: &[(&str, &str)] = if last_read.is_empty() { &[] } else { &oldest };
            recent(client, channel, extra)
                .map(|list| vec![messages_body(&list, "Unread messages", directory)])
        }
        _ => Ok(Vec::new()),
    };
    (bodies, avatar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::wayland::slack_api::tests::FakeHttp;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2024, 5, 10, 14, 0, 0).unwrap()
    }

    fn user(id: &str, name: &str, display: &str) -> User {
        User {
            id: id.to_string(),
            name: name.to_string(),
            display_name: display.to_string(),
            ..User::default()
        }
    }

    fn conversation(
        id: &str,
        kind: ConversationKind,
        name: &str,
        other: Option<&str>,
    ) -> Conversation {
        Conversation {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            user: other.map(str::to_string),
            topic: String::new(),
            purpose: String::new(),
            members: None,
        }
    }

    fn directory() -> Directory {
        Directory::new(
            Workspace {
                team_id: "T1".to_string(),
                user_id: "U0".to_string(),
                url: "https://acme.slack.com/".to_string(),
                name: "Acme".to_string(),
                enterprise: false,
            },
            vec![user("U1", "alice", "Alice"), user("U2", "bob", "")],
            vec![
                conversation("C1", ConversationKind::Channel, "general", None),
                conversation("G1", ConversationKind::Private, "secret", None),
                conversation("D1", ConversationKind::Direct, "U1", Some("U1")),
            ],
            0,
        )
    }

    fn command(action: &Option<CommandAction>) -> Option<String> {
        match &action.as_ref()?.action {
            DeferredAction::Command(args) => args.last().cloned(),
            DeferredAction::Slack(SlackAction::Open(url)) => Some(url.clone()),
            _ => None,
        }
    }

    fn slack_action(action: &Option<CommandAction>) -> Option<SlackAction> {
        match &action.as_ref()?.action {
            DeferredAction::Slack(action) => Some(action.clone()),
            _ => None,
        }
    }

    #[test]
    fn test_parse_view() {
        assert_eq!(parse_view(""), (View::Home, String::new()));
        assert_eq!(parse_view("gen"), (View::Home, "gen".to_string()));
        assert_eq!(
            parse_view("search  deploy failed "),
            (View::Search("deploy failed".to_string()), String::new())
        );
        assert_eq!(parse_view("unread ops"), (View::Unread, "ops".to_string()));
        assert_eq!(parse_view("searchy"), (View::Home, "searchy".to_string()));
        assert_eq!(
            parse_view("people  ada l"),
            (View::People("ada l".to_string()), String::new())
        );
    }

    #[test]
    fn test_message_users_finds_authors_and_mentions() {
        let list = [serde_json::json!({"user": "U1", "text": "hi <@U2> and <@W3|carol>"})];
        assert_eq!(message_users(&list), ["U1", "U2", "W3"]);
    }

    #[test]
    fn test_parse_minutes() {
        assert_eq!(parse_minutes("45"), Some(45));
        assert_eq!(parse_minutes("2h30m"), Some(150));
        assert_eq!(parse_minutes("1d"), Some(24 * 60));
        assert_eq!(parse_minutes("0"), None);
        assert_eq!(parse_minutes("h"), None);
        assert_eq!(parse_minutes("5x"), None);
        assert_eq!(parse_minutes("30m5"), None);
    }

    #[test]
    fn test_parse_status() {
        assert_eq!(
            parse_status("Lunch :pizza: 1h"),
            StatusArgs {
                text: "Lunch".to_string(),
                emoji: ":pizza:".to_string(),
                expiry: Expiry::Minutes(60),
            }
        );
        let status = parse_status("In room 42");
        assert_eq!(status.text, "In room 42");
        assert_eq!(status.expiry, Expiry::Never);
        let status = parse_status(":palm_tree: Away TODAY");
        assert_eq!(
            (status.text.as_str(), status.emoji.as_str()),
            ("Away", ":palm_tree:")
        );
        assert_eq!(status.expiry, Expiry::Today);
        assert!(parse_status("2h").is_empty());
    }

    #[test]
    fn test_expiry_today_ends_at_local_midnight() {
        let end = Expiry::Today.epoch(now());
        let midnight = Local.with_ymd_and_hms(2024, 5, 11, 0, 0, 0).unwrap();
        assert_eq!(end, midnight.timestamp() - 1);
        assert_eq!(Expiry::Never.epoch(now()), 0);
        assert_eq!(Expiry::Minutes(30).epoch(now()), now().timestamp() + 1800);
    }

    #[test]
    fn test_plain_text_names_mentions_and_links() {
        let directory = directory();
        assert_eq!(
            plain_text(
                "<@U1> see <#C1> or <#C9|ops>, <https://x.dev|docs> &amp; <!here> &lt;3",
                Some(&directory)
            ),
            "@Alice see #general or #ops, docs & @here <3"
        );
        assert_eq!(
            plain_text("<@U9> <https://x.dev>", None),
            "@U9 https://x.dev"
        );
        assert_eq!(plain_text("a < b", None), "a < b");
    }

    #[test]
    fn test_links() {
        let workspace = directory().workspace;
        assert_eq!(
            conversation_url(SlackOpenIn::Desktop, &workspace, "C1"),
            "slack://channel?team=T1&id=C1"
        );
        assert_eq!(
            conversation_url(SlackOpenIn::Browser, &workspace, "C1"),
            "https://app.slack.com/client/T1/C1"
        );
        assert_eq!(
            user_url(SlackOpenIn::Desktop, &workspace, "U2", None),
            "slack://user?team=T1&id=U2"
        );
        assert_eq!(
            user_url(SlackOpenIn::Browser, &workspace, "U1", Some("D1")),
            "https://app.slack.com/client/T1/D1"
        );
        assert_eq!(
            message_url(
                SlackOpenIn::Browser,
                &workspace,
                "C1",
                "1700000000.000100",
                ""
            ),
            "https://acme.slack.com/archives/C1/p1700000000000100"
        );
    }

    #[test]
    fn test_home_rows_follow_live_state() {
        let directory = directory();
        let live = Live {
            status: Some((":pizza:".to_string(), "Lunch".to_string(), 0)),
            snoozed_until: Some(now().timestamp() + 600),
            away: Some(true),
        };
        let rows = home_rows(
            &directory,
            &live,
            SlackOpenIn::Desktop,
            &HashMap::new(),
            now(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.identity.as_str()).collect();
        assert!(ids.contains(&"slack:do:clear-status"));
        assert!(ids.contains(&"slack:do:active"));
        assert!(!ids.contains(&"slack:do:away"));
        assert!(ids.contains(&"slack:do:resume"));
        // Direct conversations are reached through their person.
        assert!(!ids.contains(&"slack:conv:D1"));
        let tail = &ids[ids.len() - 4..];
        assert_eq!(
            tail,
            [
                "slack:conv:C1",
                "slack:conv:G1",
                "slack:user:U1",
                "slack:user:U2"
            ]
        );

        let alice = rows
            .iter()
            .find(|row| row.identity == "slack:user:U1")
            .unwrap();
        assert_eq!(
            command(&alice.primary).as_deref(),
            Some("slack://channel?team=T1&id=D1")
        );
        assert_eq!(
            command(&alice.secondary).as_deref(),
            Some("https://app.slack.com/client/T1/D1")
        );

        let rows = home_rows(
            &directory,
            &Live::default(),
            SlackOpenIn::Desktop,
            &HashMap::new(),
            now(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.identity.as_str()).collect();
        assert!(!ids.contains(&"slack:do:clear-status"));
        assert!(!ids.contains(&"slack:do:resume"));
        assert!(ids.contains(&"slack:do:active") && ids.contains(&"slack:do:away"));
    }

    #[test]
    fn test_query_row() {
        let item = query_row("status Lunch :pizza: 1h", None).unwrap();
        assert_eq!(item.title, "Set status to :pizza: Lunch");
        assert_eq!(item.subtitle, "for 1 hour");
        assert_eq!(
            slack_action(&item.primary),
            Some(SlackAction::SetStatus(parse_status("Lunch :pizza: 1h")))
        );
        let item = query_row("snooze 2h30m", None).unwrap();
        assert_eq!(item.title, "Snooze notifications for 2 hours 30 minutes");
        assert_eq!(
            slack_action(&item.primary),
            Some(SlackAction::Snooze(Snooze::Minutes(150)))
        );
        assert!(query_row("status ", None).unwrap().primary.is_none());
        assert!(query_row("snooze soon", None).unwrap().primary.is_none());
        assert!(query_row("status", None).unwrap().primary.is_none());
        assert!(query_row("general chat", None).is_none());
    }

    #[test]
    fn test_search_rows() {
        let reply = json!({"messages": {"matches": [
            {
                "channel": {"id": "C1", "name": "general"},
                "ts": "1715349600.000200",
                "user": "U1",
                "text": "deploy <@U2> done",
                "permalink": "https://acme.slack.com/archives/C1/p1715349600000200?thread_ts=1715340000.000100&cid=C1",
            },
            {
                "channel": {"id": "C9", "name": "ops"},
                "ts": "1715349000.000100",
                "username": "ci-bot",
                "text": "",
                "files": [{"name": "build.log"}],
                "permalink": "https://acme.slack.com/archives/C9/p1715349000000100",
            },
            {"channel": {"id": "C1"}, "text": "no ts"},
        ]}});
        let rows = search_rows(&reply, &directory(), SlackOpenIn::Desktop, now());
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].identity,
            "slack:msg:C1:1715349600.000200:1715340000.000100"
        );
        assert_eq!(rows[0].title, "deploy @bob done");
        assert_eq!(rows[0].badge.as_deref(), Some("Thread"));
        assert!(rows[0].subtitle.starts_with("#general · Alice · "));
        assert_eq!(
            command(&rows[0].primary).as_deref(),
            Some("slack://channel?team=T1&id=C1&message=1715349600.000200")
        );
        assert_eq!(
            command(&rows[0].secondary).as_deref(),
            reply["messages"]["matches"][0]["permalink"].as_str()
        );
        assert_eq!(rows[1].identity, "slack:msg:C9:1715349000.000100");
        assert_eq!(rows[1].title, "[file: build.log]");
        assert!(rows[1].subtitle.starts_with("#ops · ci-bot · "));
    }

    #[test]
    fn test_counts_unread_keeps_unread_newest_first() {
        let reply = json!({
            "channels": [
                {"id": "C1", "has_unreads": true, "last_read": "1.0", "latest": "5.0", "mention_count": 2},
                {"id": "C2", "has_unreads": false, "latest": "9.0"},
            ],
            "ims": [{"id": "D1", "has_unreads": true, "last_read": "3.0", "latest": "7.0"}],
        });
        let unread = counts_unread(&reply);
        let ids: Vec<&str> = unread.iter().map(|u| u.channel.as_str()).collect();
        assert_eq!(ids, ["D1", "C1"]);
        assert_eq!(unread[1].mentions, 2);

        let rows = unread_rows(&unread, &directory(), SlackOpenIn::Desktop, now());
        assert_eq!(rows[0].title, "@Alice");
        assert_eq!(rows[0].identity, "slack:unread:D1:3.0");
        assert_eq!(
            slack_action(&rows[1].secondary),
            Some(SlackAction::MarkRead {
                channel: "C1".to_string(),
                ts: "5.0".to_string()
            })
        );
        assert!(rows[1].subtitle.starts_with("2 mentions"));
    }

    #[test]
    fn test_resolve_conversation() {
        let directory = directory();
        assert_eq!(
            resolve_conversation("#general", &directory).as_deref(),
            Some("C1")
        );
        assert_eq!(
            resolve_conversation("@alice", &directory).as_deref(),
            Some("D1")
        );
        assert_eq!(
            resolve_conversation("U1", &directory).as_deref(),
            Some("D1")
        );
        assert_eq!(
            resolve_conversation("C42", &directory).as_deref(),
            Some("C42")
        );
        assert_eq!(resolve_conversation("#nope", &directory), None);
        assert_eq!(resolve_conversation("@bob", &directory), None);
    }

    #[test]
    fn test_thread_preview_shows_parent_and_replies_up_to_the_hit() {
        let message = |ts: &str, text: &str| json!({"ts": ts, "user": "U1", "text": text});
        let fake = FakeHttp::with(vec![(
            200,
            None,
            json!({"ok": true, "messages": [
                message("1.0", "parent"),
                message("2.0", "first reply"),
                message("3.0", "the hit"),
                message("4.0", "later reply"),
            ]}),
        )]);
        let client = Client::fake(Box::new(fake.clone()));
        let (bodies, avatar) = preview_bodies(
            "slack:msg:C1:3.0:1.0",
            &client,
            std::sync::Arc::new(directory()),
        );
        assert_eq!(avatar, None);
        let Ok(bodies) = bodies else {
            panic!("preview failed");
        };
        let [PreviewBody::Text {
            heading, content, ..
        }] = bodies.as_slice()
        else {
            panic!("expected one text body");
        };
        assert_eq!(heading.as_deref(), Some("Thread"));
        assert!(content.contains("parent") && content.contains("the hit"));
        assert!(!content.contains("later reply"));
        assert_eq!(fake.methods(), ["conversations.replies"]);
    }
}
