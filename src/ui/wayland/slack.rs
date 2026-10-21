//! Slack addon: open conversations, people and bots, search messages, read
//! unread conversations, threads and activity, and set the status, presence and notification snooze, with a user (`xoxp-`)
//! or browser session (`xoxc-`) token. Web API calls live in `slack_api`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem, QUERY_ROW};
use super::preview::{ChatEntry, ChatMessage, ChatMessageGroup, PreviewBody};
use super::slack_api::{self, Client, Conversation, ConversationKind, Directory, User, Workspace};
use super::slack_emoji::slack_emoji;
use super::tab_preview::Wanted;
use crate::SlackOpenIn;

pub(super) const SLACK_ICON: &str = "slack";
const CHANNEL_ICONS: [&str; 3] = [SLACK_ICON, "internet-group-chat", "user-available"];
const USER_ICONS: [&str; 3] = ["avatar-default", "user-available", SLACK_ICON];
/// Search hits shown, at most.
const SEARCH_COUNT: &str = "40";
/// Conversations checked for unread messages, at most, as in Raycast.
const MAX_UNREAD: usize = 30;
/// Heading of thread previews, which keep their parent message on top.
const THREAD_HEADING: &str = "Thread";
/// Messages shown in a preview.
const PREVIEW_MESSAGES: usize = 12;
/// Recent messages kept around a search hit inside its conversation.
const CONVERSATION_CONTEXT_MESSAGES: usize = 6;
const MAX_MESSAGE_CHARS: usize = 600;
const MAX_TITLE_CHARS: usize = 120;
const MAX_AVATAR_BYTES: u64 = 512 * 1024;
const MAX_IMAGE_SIDE: u32 = 256;
/// Wait before loading a preview, so moving quickly through the rows does
/// not send one request per row.
const PREVIEW_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);
/// Previews kept on disk, at most.
const MAX_CACHED_PREVIEWS: usize = 500;
/// Searches kept in memory, at most.
const MAX_CACHED_SEARCHES: usize = 50;
/// Threads in a page of the threads view; Slack gives no more.
const THREAD_PAGE: &str = "10";
/// Pages of the threads view looked through for unread replies, at most.
const MAX_THREAD_PAGES: usize = 5;
/// Activity items asked for, at most.
const ACTIVITY_COUNT: &str = "50";
/// The Activity tab items listed: mentions, reactions, thread replies and
/// new messages.
const ACTIVITY_TYPES: &str = "at_user,at_user_group,at_channel,at_everyone,keyword,\
                              message_reaction,thread_v2,dm,bot_dm_bundle,channel";

/// What the text after the keyword asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum View {
    /// Actions, conversations and people, filtered locally.
    Home,
    /// Messages matching the search terms.
    Search(String),
    /// Conversations with unread messages.
    Unread,
    /// Followed threads with unread replies.
    Threads,
    /// Unread mentions, reactions and other activity.
    Activity,
    /// People matching the name.
    People(String),
    /// Search, people, status, presence and snooze.
    Actions,
}

impl View {
    /// Name shown after Ctrl+Tab in the footer.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search(_) => "Search",
            Self::Unread => "Unread",
            Self::Threads => "Threads",
            Self::Activity => "Activity",
            Self::People(_) => "People",
            Self::Actions => "Actions",
        }
    }

    /// The text a debounced view looks up.
    pub fn terms(&self) -> Option<&str> {
        match self {
            Self::Search(terms) | Self::People(terms) => Some(terms),
            Self::Home | Self::Unread | Self::Threads | Self::Activity | Self::Actions => None,
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
        "threads" => (View::Threads, rest.to_string()),
        "activity" => (View::Activity, rest.to_string()),
        "people" => (View::People(rest.trim().to_string()), String::new()),
        "actions" => (View::Actions, rest.to_string()),
        _ => (View::Home, query.to_string()),
    }
}

/// `query` switched to the next view Ctrl+Tab cycles through, Home, Unread,
/// Threads, Activity, People then Actions, or the previous one when
/// `backwards`, keeping the text typed after the view word. A search counts as Home. Home is skipped when the
/// text would read as another view, such as `search errors`.
pub(super) fn cycle_view(query: &str, backwards: bool) -> String {
    const STOPS: [&str; 6] = ["", "unread", "threads", "activity", "people", "actions"];
    let (view, filter) = parse_view(query);
    let (at, text) = match view {
        View::Home => (0, filter),
        View::Search(terms) => (0, terms),
        View::Unread => (1, filter),
        View::Threads => (2, filter),
        View::Activity => (3, filter),
        View::People(terms) => (4, terms),
        View::Actions => (5, filter),
    };
    let home_fits = matches!(parse_view(&text).0, View::Home);
    let step = if backwards { STOPS.len() - 1 } else { 1 };
    let mut next = (at + step) % STOPS.len();
    if next == 0 && !home_fits {
        next = (next + step) % STOPS.len();
    }
    match STOPS[next] {
        "" => text,
        word => format!("{word} {text}"),
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

/// Replace known `:shortcode:` tokens in `text` with Unicode emojis,
/// stripping optional `:skin-tone-N:` suffixes after a resolved shortcode.
pub(super) fn replace_emoji_shortcodes(text: &str) -> String {
    if !text.contains(':') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(colon) = rest.find(':') {
        out.push_str(&rest[..colon]);
        let after = &rest[colon + 1..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_+-'".contains(c)))
            .unwrap_or(after.len());
        let name = &after[..end];
        if !name.is_empty() && after[end..].starts_with(':') {
            if let Some(emoji) = slack_emoji(name) {
                out.push_str(emoji);
                rest = &after[end + 1..];
                if let Some(tone_rest) = rest.strip_prefix(":skin-tone-") {
                    if let Some((_, after_tone)) = tone_rest.split_once(':') {
                        if tone_rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                            rest = after_tone;
                        }
                    }
                }
                continue;
            }
        }
        out.push(':');
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Slack message markup as plain text: mentions and links by name, entities
/// decoded, and standard `:emoji:` shortcodes converted to Unicode.
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
    let decoded = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    replace_emoji_shortcodes(&decoded)
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

/// Time of day (`HH:MM`) for a message inside a day-grouped feed.
fn format_hhmm(ts: &str) -> String {
    ts_seconds(ts)
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Calendar date and human-readable day divider label (`Today`, `Yesterday`,
/// `September 1`, or `Sep 1, 2025`).
fn day_label(ts: &str, now: DateTime<Local>) -> Option<(chrono::NaiveDate, String)> {
    let time = ts_seconds(ts).and_then(|secs| Local.timestamp_opt(secs, 0).single())?;
    let date = time.date_naive();
    let today = now.date_naive();
    let label = if date == today {
        "Today".to_string()
    } else if today.signed_duration_since(date).num_days() == 1 {
        "Yesterday".to_string()
    } else if date.year() == today.year() {
        time.format("%B %-d").to_string()
    } else {
        time.format("%b %-d, %Y").to_string()
    };
    Some((date, label))
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

pub(super) fn open_action(url: String) -> DeferredAction {
    if url.starts_with("slack://") {
        DeferredAction::Slack(SlackAction::Open(url))
    } else {
        DeferredAction::Command(vec!["xdg-open".to_string(), url])
    }
}

/// Open `url`, marking `read` (`(channel, ts)`) as read when given.
fn open(url: String) -> CommandAction {
    CommandAction {
        action: open_action(url),
        confirm: None,
        keep_open: false,
        progress: None,
    }
}

fn open_unread(url: String, channel: String, ts: String) -> CommandAction {
    CommandAction {
        action: DeferredAction::Slack(SlackAction::OpenUnread { url, channel, ts }),
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
    /// Open an unread conversation and mark it as read.
    OpenUnread {
        url: String,
        channel: String,
        ts: String,
    },
    /// Mark the replies of a followed thread up to `ts` as read.
    MarkThreadRead {
        channel: String,
        thread_ts: String,
        ts: String,
    },
    /// Open a thread with unread replies and mark them as read.
    OpenThread {
        url: String,
        channel: String,
        thread_ts: String,
        ts: String,
    },
}

/// Run `action` against Slack. Blocking: run it off the UI thread.
pub(super) fn run_action(action: &SlackAction) -> Result<(), String> {
    match action {
        SlackAction::Open(url) => return open_url(url),
        SlackAction::OpenUnread { url, channel, ts } => {
            forget_cached_unread(channel);
            let opened = open_url(url);
            if !ts.is_empty() {
                if let Err(error) = slack_api::current_client().and_then(|client| {
                    client
                        .call("conversations.mark", &[("channel", channel), ("ts", ts)])
                        .map(drop)
                }) {
                    crate::debug_log!("slack: cannot mark {channel} as read: {error}");
                }
            }
            return opened;
        }
        SlackAction::OpenThread {
            url,
            channel,
            thread_ts,
            ts,
        } => {
            forget_cached_thread(channel, thread_ts);
            let opened = open_url(url);
            if let Err(error) = slack_api::current_client()
                .and_then(|client| mark_thread(&client, channel, thread_ts, ts))
            {
                crate::debug_log!("slack: cannot mark thread {thread_ts} as read: {error}");
            }
            return opened;
        }
        _ => {}
    }
    let client = slack_api::current_client()?;
    let now = Local::now();
    call_action(&client, action, now)?;
    if let Some(account) = client.known_identity() {
        remember_action(action, account, now);
    }
    Ok(())
}

/// Drop `channel` from the unread cache at once, even before `auth.test`
/// has answered in this run.
pub(super) fn forget_cached_unread(channel: &str) {
    if let Some(account) = slack_api::cached_account() {
        slack_api::update_cache(
            slack_api::UNREAD_FILE,
            &account,
            |unread: &mut Vec<Unread>| {
                unread.retain(|entry| entry.channel != channel);
            },
        );
    }
}

/// Drop the thread from the threads cache at once, as
/// [`forget_cached_unread`] does for conversations.
pub(super) fn forget_cached_thread(channel: &str, thread_ts: &str) {
    if let Some(account) = slack_api::cached_account() {
        slack_api::update_cache(
            slack_api::THREADS_FILE,
            &account,
            |threads: &mut Vec<UnreadThread>| {
                threads
                    .retain(|thread| !(thread.channel == channel && thread.thread_ts == thread_ts));
            },
        );
    }
}

fn mark_thread(client: &Client, channel: &str, thread_ts: &str, ts: &str) -> Result<(), String> {
    client
        .call(
            "subscriptions.thread.mark",
            &[
                ("channel", channel),
                ("thread_ts", thread_ts),
                ("ts", ts),
                ("read", "1"),
            ],
        )
        .map(drop)
}

/// Bring the cached state in line with what `action` did, so a list shown
/// from the cache before Slack answers does not show the old state.
fn remember_action(action: &SlackAction, account: &Workspace, now: DateTime<Local>) {
    if let SlackAction::MarkThreadRead {
        channel, thread_ts, ..
    } = action
    {
        slack_api::update_cache(
            slack_api::THREADS_FILE,
            account,
            |threads: &mut Vec<UnreadThread>| {
                threads.retain(|thread| {
                    !(&thread.channel == channel && &thread.thread_ts == thread_ts)
                });
            },
        );
        return;
    }
    if matches!(
        action,
        SlackAction::MarkRead { .. } | SlackAction::OpenUnread { .. }
    ) {
        slack_api::update_cache(slack_api::UNREAD_FILE, account, |unread| {
            apply_to_unread(unread, action)
        });
    } else {
        slack_api::update_cache(slack_api::LIVE_FILE, account, |live| {
            apply_to_live(live, action, now)
        });
    }
}

pub(super) fn apply_to_live(live: &mut Live, action: &SlackAction, now: DateTime<Local>) {
    match action {
        SlackAction::SetStatus(status) => {
            live.status = Some((
                status.emoji.clone(),
                status.text.clone(),
                status.expiry.epoch(now),
            ));
        }
        SlackAction::SetPresence { away } => live.away = Some(*away),
        SlackAction::Snooze(snooze) => {
            if let Ok(minutes) = snooze_minutes(snooze, now) {
                live.snoozed_until = Some(now.timestamp() + minutes as i64 * 60);
            }
        }
        SlackAction::EndSnooze => live.snoozed_until = None,
        _ => {}
    }
}

pub(super) fn apply_to_unread(unread: &mut Vec<Unread>, action: &SlackAction) {
    if let SlackAction::MarkRead { channel, .. } | SlackAction::OpenUnread { channel, .. } = action
    {
        unread.retain(|entry| &entry.channel != channel);
    }
}

fn snooze_minutes(snooze: &Snooze, now: DateTime<Local>) -> Result<u64, String> {
    match snooze {
        Snooze::Minutes(minutes) => Ok(*minutes),
        Snooze::UntilMorning => local_at(now, 1, 9)
            .map(|morning| (morning.timestamp() - now.timestamp()).max(60) as u64 / 60)
            .ok_or_else(|| "cannot tell when tomorrow morning is".to_string()),
    }
}

fn call_action(client: &Client, action: &SlackAction, now: DateTime<Local>) -> Result<(), String> {
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
            let minutes = snooze_minutes(snooze, now)?;
            client
                .call("dnd.setSnooze", &[("num_minutes", &minutes.to_string())])
                .map(drop)
        }
        SlackAction::EndSnooze => client.call("dnd.endSnooze", &[]).map(drop),
        SlackAction::MarkRead { channel, ts } | SlackAction::OpenUnread { channel, ts, .. } => {
            client
                .call("conversations.mark", &[("channel", channel), ("ts", ts)])
                .map(drop)
        }
        SlackAction::MarkThreadRead {
            channel,
            thread_ts,
            ts,
        } => mark_thread(client, channel, thread_ts, ts),
        SlackAction::ReloadDirectory => slack_api::directory(client, true).map(drop),
        SlackAction::Open(_) | SlackAction::OpenThread { .. } => Ok(()),
    }
}

/// How long to wait for the Slack window when the link starts Slack.
const SLACK_START_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Open `url`, focusing the Slack window when it is a `slack://` link.
fn open_url(url: &str) -> Result<(), String> {
    DeferredAction::Command(vec!["xdg-open".to_string(), url.to_string()]).run()?;
    if url.starts_with("slack://") {
        if let Err(error) = super::windows::raise_app_window(&["slack"], SLACK_START_WAIT) {
            crate::debug_log!("slack: cannot focus the Slack window: {error}");
        }
    }
    Ok(())
}

/// The user's own status, presence and snooze, shown on the action rows.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Live {
    /// Emoji, text and expiry in seconds since the epoch, 0 for never.
    pub status: Option<(String, String, i64)>,
    pub snoozed_until: Option<i64>,
    pub away: Option<bool>,
    /// Whether Slack has unread threads and unread activity; None until it
    /// says, which keeps their action rows.
    #[serde(default)]
    pub unread_threads: Option<bool>,
    #[serde(default)]
    pub unread_activity: Option<bool>,
}

/// The replies of the live state calls, made in parallel; None for those
/// that failed.
type LiveReplies = (Option<Value>, Option<Value>, Option<Value>);

fn fetch_live(client: &Client) -> LiveReplies {
    std::thread::scope(|scope| {
        let profile = scope.spawn(|| client.call("users.profile.get", &[]).ok());
        let dnd = scope.spawn(|| client.call("dnd.info", &[]).ok());
        let presence = scope.spawn(|| client.call("users.getPresence", &[]).ok());
        let join =
            |handle: std::thread::ScopedJoinHandle<'_, Option<Value>>| handle.join().ok().flatten();
        (join(profile), join(dnd), join(presence))
    })
}

/// The live state from `replies`, keeping from `previous` what the calls
/// that failed would have said.
pub(super) fn merge_live(previous: Option<Live>, (profile, dnd, presence): LiveReplies) -> Live {
    let previous = previous.unwrap_or_default();
    let (has_profile, has_dnd, has_presence) =
        (profile.is_some(), dnd.is_some(), presence.is_some());
    let fresh = parse_live(profile, dnd, presence);
    Live {
        status: if has_profile {
            fresh.status
        } else {
            previous.status
        },
        snoozed_until: if has_dnd {
            fresh.snoozed_until
        } else {
            previous.snoozed_until
        },
        away: if has_presence {
            fresh.away
        } else {
            previous.away
        },
        ..previous
    }
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
        ..Live::default()
    }
}

/// Whether `client.counts` reports unread threads and unread activity.
fn parse_counts_badges(reply: &Value) -> (Option<bool>, Option<bool>) {
    let threads = reply["threads"]["has_unreads"].as_bool();
    let activity = reply["activity_v2"].as_object().map(|counts| {
        ACTIVITY_TYPES
            .split(',')
            .any(|kind| counts.get(kind).and_then(Value::as_u64).unwrap_or(0) > 0)
    });
    (threads, activity)
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

/// The rows that lead to the other views, listed first on Home.
fn shortcut_rows(live: &Live, now: DateTime<Local>) -> Vec<CommandItem> {
    let mut rows = vec![action_row(
        "unread",
        "Unread messages",
        "Conversations with unread messages".to_string(),
        "new",
        &["mail-unread", "mail-message-new", SLACK_ICON],
        fill("unread "),
    )];
    if live.unread_threads != Some(false) {
        rows.push(action_row(
            "threads",
            "Unread threads",
            "Threads you follow with unread replies".to_string(),
            "replies",
            &["mail-reply-all", "mail-replied", SLACK_ICON],
            fill("threads "),
        ));
    }
    if live.unread_activity != Some(false) {
        rows.push(action_row(
            "activity",
            "Activity",
            "Mentions, reactions and replies you have not seen".to_string(),
            "mentions reactions",
            &[
                "preferences-system-notifications",
                "notifications",
                SLACK_ICON,
            ],
            fill("activity "),
        ));
    }
    rows.push(action_row(
        "actions",
        "Actions",
        actions_summary(live, now),
        "search find people status presence away active snooze dnd do not disturb notifications",
        &["preferences-system", "emblem-system", SLACK_ICON],
        fill("actions "),
    ));
    rows
}

/// The current status, presence and snooze, for the Actions row.
fn actions_summary(live: &Live, now: DateTime<Local>) -> String {
    let mut parts = Vec::new();
    if let Some((emoji, text, expiry)) = &live.status {
        if !emoji.is_empty() || !text.is_empty() {
            parts.push(describe_status(emoji, text, *expiry, now));
        }
    }
    match live.away {
        Some(true) => parts.push("Away".to_string()),
        Some(false) => parts.push("Active".to_string()),
        None => {}
    }
    if let Some(until) = live.snoozed_until.filter(|until| *until > now.timestamp()) {
        parts.push(format!("Snoozed until {}", time_of(until, now)));
    }
    if parts.is_empty() {
        "Search, people, status, presence and snooze".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Search, people, status, presence and snooze rows, listed by the Actions
/// view.
pub(super) fn account_rows(live: &Live, now: DateTime<Local>) -> Vec<CommandItem> {
    let mut rows = vec![
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
    now: DateTime<Local>,
) -> CommandItem {
    let workspace = &directory.workspace;
    let mut item = row(
        &format!("slack:user:{}", user.id),
        user.label(),
        &USER_ICONS,
    );
    let status = replace_emoji_shortcodes(&describe_status(
        &user.status_emoji,
        &user.status_text,
        0,
        now,
    ));
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
    item.badge = Some(if user.is_bot { "Bot" } else { "Person" }.to_string());
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
    let offset = chrono::FixedOffset::east_opt(user.tz_offset as i32).filter(|_| !user.is_bot);
    if let Some(offset) = offset {
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

/// Actions, prioritized unread rows, then remaining conversations and people.
pub(super) fn home_rows(
    directory: &Directory,
    live: &Live,
    unread: Option<&[Unread]>,
    open_in: SlackOpenIn,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let mut rows = shortcut_rows(live, now);
    rows.extend(account_rows(live, now).into_iter().map(|mut row| {
        row.typed_only = true;
        row
    }));
    for row in &mut rows {
        row.sort_priority = 2;
    }
    let unread_by_channel: HashMap<&str, &Unread> = unread
        .unwrap_or_default()
        .iter()
        .filter(|entry| !entry.muted)
        .map(|entry| (entry.channel.as_str(), entry))
        .collect();
    let mut promoted = Vec::new();
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
    let mut remaining_conversations = Vec::new();
    for conversation in conversations {
        let item = conversation_row(conversation, directory, open_in);
        if let Some(unread) = unread_by_channel.get(conversation.id.as_str()) {
            let mut item = item;
            item.sort_priority = 1;
            promoted.push((unread.latest.clone(), item.title.to_lowercase(), item));
        } else {
            remaining_conversations.push(item);
        }
    }
    // Only the bots the user talks to, after the people.
    let mut users: Vec<&User> = directory
        .users
        .iter()
        .filter(|user| !user.is_bot || directory.direct_with(&user.id).is_some())
        .collect();
    users.sort_by_key(|user| (user.is_bot, user.label().to_lowercase()));
    let mut remaining_users = Vec::new();
    for user in users {
        let item = user_row(user, directory, open_in, now);
        let unread = directory
            .direct_with(&user.id)
            .and_then(|direct| unread_by_channel.get(direct.id.as_str()));
        if let Some(unread) = unread {
            let mut item = item;
            item.sort_priority = 1;
            promoted.push((unread.latest.clone(), item.title.to_lowercase(), item));
        } else {
            remaining_users.push(item);
        }
    }
    promoted.sort_by(|(latest_a, label_a, _), (latest_b, label_b, _)| {
        latest_b.cmp(latest_a).then_with(|| label_a.cmp(label_b))
    });
    rows.extend(promoted.into_iter().map(|(_, _, item)| item));
    rows.extend(remaining_conversations);
    rows.extend(remaining_users);
    rows
}

fn avatar_dir() -> Option<PathBuf> {
    crate::ui::get_slack_cache_dir()
        .ok()
        .map(|dir| dir.join("avatars"))
}

/// The downloaded avatars by user id, and the workspace icon by team id.
pub(super) fn cached_avatars(dir: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    let ids: std::collections::HashSet<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            // Downloads being written, and markers for icons Slack lacks.
            if name.starts_with('.') || name.ends_with(".none") {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect();
    ids.into_iter()
        .filter_map(|id| {
            let path = super::favicons::newest(dir, &id)?;
            Some((id, path.to_str()?.to_string()))
        })
        .collect()
}

/// The pictures on disk, for [`apply_images`].
pub(super) fn cached_images() -> HashMap<String, String> {
    avatar_dir()
        .map(|dir| cached_avatars(&dir))
        .unwrap_or_default()
}

fn image_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .https_only(true)
        .build()
        .into()
}

/// Download the picture at `url` as `<id>.<ext>` in `dir`. Blocking.
fn download_image(agent: &ureq::Agent, dir: &Path, id: &str, url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let data = super::favicons::download(agent, url, MAX_AVATAR_BYTES)?;
    // Slack serves 192px avatars and 132px workspace icons; anything much
    // larger costs memory on every row that shows it.
    if !super::favicons::fits(&data, MAX_IMAGE_SIDE) {
        return None;
    }
    let path = super::favicons::store(dir, id, &data)?;
    path.to_str().map(str::to_string)
}

/// The user's avatar, downloaded once. Blocking.
fn fetch_avatar(user: &User) -> Option<String> {
    let dir = avatar_dir()?;
    if let Some(path) = cached_avatars(&dir).remove(&user.id) {
        return Some(path);
    }
    download_image(&image_agent(), &dir, &user.id, &user.avatar)
}

/// The key of the picture `identity` shows: a user id for a person or a
/// direct conversation, the team id for a channel. Group conversations
/// have none.
pub(super) fn image_key(identity: &str, directory: Option<&Directory>) -> Option<String> {
    let parts: Vec<&str> = identity.split(':').collect();
    let channel = match parts.as_slice() {
        ["slack", "user", user] => return Some(user.to_string()),
        ["slack", "conv" | "unread" | "msg" | "thread" | "activity", channel, ..] => *channel,
        _ => return None,
    };
    let directory = directory?;
    let team = || Some(directory.workspace.team_id.clone()).filter(|team| !team.is_empty());
    let Some(conversation) = directory.conversation(channel) else {
        return is_search_channel(channel).then(team).flatten();
    };
    match conversation.kind {
        ConversationKind::Channel | ConversationKind::Private => team(),
        // A direct message hit shows its author, not this picture.
        ConversationKind::Direct if parts[1] != "msg" => conversation.user.clone(),
        _ => None,
    }
}

/// Show the known picture of each row in `items`, over its theme icon.
pub(super) fn apply_images(
    items: &mut [CommandItem],
    directory: Option<&Directory>,
    images: &HashMap<String, String>,
) {
    if images.is_empty() {
        return;
    }
    for item in items {
        if let Some(path) = image_key(&item.identity, directory).and_then(|key| images.get(&key)) {
            item.icon = Some(path.clone());
        }
    }
}

/// Avatars downloaded at most per batch.
pub(super) const MAX_IMAGE_BATCH: usize = 30;
/// How long a workspace icon, or the lack of one, is trusted.
const WORKSPACE_ICON_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Pictures to download for rows in view.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct ImageWants {
    /// User ids and avatar URLs.
    pub users: Vec<(String, String)>,
    /// The team id, when a channel row is in view.
    pub team: Option<String>,
}

impl ImageWants {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.team.is_none()
    }
}

/// The pictures the rows `identities` lack, not tried already. The
/// workspace icon is wanted even when on disk, since it may be due for a
/// refresh: [`fetch_images`] decides.
pub(super) fn image_wants<'a>(
    identities: impl IntoIterator<Item = &'a str>,
    directory: &Directory,
    images: &HashMap<String, String>,
    tried: &HashSet<String>,
) -> ImageWants {
    let mut wants = ImageWants::default();
    let mut seen: HashSet<String> = HashSet::new();
    let team = &directory.workspace.team_id;
    for identity in identities {
        let Some(key) = image_key(identity, Some(directory)) else {
            continue;
        };
        if tried.contains(&key) || !seen.insert(key.clone()) {
            continue;
        }
        if key == *team {
            wants.team = Some(key);
            continue;
        }
        if wants.users.len() >= MAX_IMAGE_BATCH || images.contains_key(&key) {
            continue;
        }
        if let Some(user) = directory
            .user(&key)
            .filter(|user| user.avatar.starts_with("https://"))
        {
            wants.users.push((key, user.avatar.clone()));
        }
    }
    wants
}

/// What [`fetch_images`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ImagesOutcome {
    /// Slack is not connected yet, or to another account than the rows
    /// belong to, so nothing was tried.
    Skipped,
    /// The pictures downloaded, by key, out of all those asked for.
    Attempted(Vec<(String, String)>),
}

/// Channels search found outside the directory, such as ones not joined,
/// so their hits still show the workspace icon.
static SEARCH_CHANNELS: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

fn note_search_channel(channel: &Value) {
    // Hits carry is_private and is_mpim but not always is_channel, so a
    // channel is whatever is neither a direct nor a group conversation.
    let flag = |name: &str| channel[name].as_bool().unwrap_or(false);
    let Some(id) = channel["id"].as_str() else {
        return;
    };
    let name = channel["name"].as_str().unwrap_or_default();
    let is_channel = (id.starts_with('C') || id.starts_with('G'))
        && !flag("is_mpim")
        && !flag("is_im")
        && !name.starts_with("mpdm-");
    if is_channel {
        if let Ok(mut channels) = SEARCH_CHANNELS.lock() {
            channels.insert(id.to_string());
        }
    }
}

fn is_search_channel(channel: &str) -> bool {
    SEARCH_CHANNELS
        .lock()
        .is_ok_and(|channels| channels.contains(channel))
}

/// Bumped to stop the batch downloading for rows no longer listed.
static IMAGE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Held by the batch downloading, so batches never overlap.
static IMAGE_BATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn image_generation() -> u64 {
    IMAGE_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stop the batch downloading before its next picture.
pub(super) fn cancel_image_downloads() {
    IMAGE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Download `wants` for the rows of `account`, four at a time, unless
/// [`cancel_image_downloads`] moved past `generation`. Blocking.
pub(super) fn fetch_images(
    wants: ImageWants,
    account: &Workspace,
    generation: u64,
) -> ImagesOutcome {
    let _batch = IMAGE_BATCH.lock();
    let current = || image_generation() == generation;
    if !current() {
        return ImagesOutcome::Attempted(Vec::new());
    }
    let Some(dir) = avatar_dir() else {
        return ImagesOutcome::Attempted(Vec::new());
    };
    // Pictures are only stored for the account the rows belong to.
    let client = slack_api::current_client().ok();
    let Some(client) = client.filter(|client| {
        client
            .known_identity()
            .is_some_and(|known| known.same_account(account))
    }) else {
        return ImagesOutcome::Skipped;
    };
    let agent = image_agent();
    let team = wants.team.clone();
    let found = download_avatars(&agent, &dir, wants.users, &current, &mut || {
        // The workspace icon rides the scope's main thread, the fourth
        // worker, while the avatar downloads run.
        let Some(team) = team.as_ref().filter(|_| current()) else {
            return Vec::new();
        };
        match fetch_workspace_icon(&client, &agent, &dir, team, account) {
            Some(path) => vec![(team.clone(), path)],
            None => Vec::new(),
        }
    });
    ImagesOutcome::Attempted(found)
}

/// Download the `(id, url)` avatar pairs in `users` with three scoped
/// workers, polling `current` before each download so a stale generation
/// stops early. `alongside` runs on the scope's main thread while the
/// workers download and returns its own `(id, path)` finds to merge in.
fn download_avatars(
    agent: &ureq::Agent,
    dir: &Path,
    users: Vec<(String, String)>,
    current: &(dyn Fn() -> bool + Sync),
    alongside: &mut dyn FnMut() -> Vec<(String, String)>,
) -> Vec<(String, String)> {
    let queue = std::sync::Mutex::new(users.into_iter());
    let found = std::sync::Mutex::new(Vec::new());
    let mut alongside_finds = Vec::new();
    std::thread::scope(|scope| {
        // Three workers overlap the slow avatar downloads; extra work like
        // the workspace icon runs on the scope's main thread meanwhile.
        for _ in 0..3 {
            scope.spawn(|| loop {
                if !current() {
                    break;
                }
                let next = queue.lock().ok().and_then(|mut queue| queue.next());
                let Some((id, url)) = next else {
                    break;
                };
                if let Some(path) = download_image(agent, dir, &id, &url) {
                    if let Ok(mut found) = found.lock() {
                        found.push((id, path));
                    }
                }
            });
        }
        alongside_finds = alongside();
    });
    let mut found = found.into_inner().unwrap_or_default();
    found.append(&mut alongside_finds);
    found
}

/// The workspace icon, asked again once a week. A failed refresh keeps the
/// icon there is; with none, Slack is not asked again for a week.
fn fetch_workspace_icon(
    client: &Client,
    agent: &ureq::Agent,
    dir: &Path,
    team: &str,
    account: &Workspace,
) -> Option<String> {
    let current = workspace_icon_due(dir, team)?;
    let url = slack_api::workspace_icon(client, account).unwrap_or_else(|error| {
        crate::debug_log!("slack: no workspace icon: {error}");
        None
    });
    let stored = url.and_then(|url| download_image(agent, dir, team, &url));
    if stored.is_none() && current.is_none() {
        super::favicons::mark_miss(dir, team);
    }
    stored
}

/// None when the workspace icon on disk, or the lack of one, is recent
/// enough; otherwise the icon there is, if any.
fn workspace_icon_due(dir: &Path, team: &str) -> Option<Option<std::path::PathBuf>> {
    let current = match super::favicons::cached(dir, team) {
        super::favicons::Cached::Icon(path) => Some(path),
        super::favicons::Cached::Miss => return None,
        super::favicons::Cached::Unknown => None,
    };
    let fresh = current.as_ref().is_some_and(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < WORKSPACE_ICON_TTL)
    });
    (!fresh).then_some(current)
}

/// The client and directory, refreshing a directory past its freshness in
/// the background unless `refresh` asks for a fresh one now.
fn connect(
    config: &crate::SlackAddonConfig,
    refresh: bool,
) -> Result<(std::sync::Arc<Client>, std::sync::Arc<Directory>), String> {
    let client = slack_api::client(
        config.token_command.as_deref(),
        config.cookie_command.as_deref(),
    )?;
    let directory = slack_api::directory(&client, refresh)?;
    if !refresh && !directory.is_fresh(slack_api::now_secs()) {
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
pub(super) fn load_home(
    config: crate::SlackAddonConfig,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config, refresh)?;
    let (replies, counts, muted) = std::thread::scope(|scope| {
        let counts =
            scope.spawn(|| client.call("client.counts", &[("thread_counts_by_channel", "true")]));
        let muted_state = scope.spawn(|| muted(&client, refresh));
        let replies = fetch_live(&client);
        let counts = counts
            .join()
            .unwrap_or_else(|_| Err("Slack unread counts lookup panicked".to_string()));
        (replies, counts, muted_state.join().unwrap_or_default())
    });
    // The directory may come from the cache, so these calls also tell
    // whether the user has been logged out.
    if let Some(error) = client.rejection() {
        return Err(error);
    }
    let badges = match &counts {
        Ok(reply) => Some(parse_counts_badges(reply)),
        // Both lists need a session token, so hide them for any other.
        Err(_) if !client.is_session() => Some((Some(false), Some(false))),
        Err(_) => None,
    };
    let unread = match counts {
        Ok(reply) => match muted {
            Some(muted) => Some(parse_counts_unread(&reply, &muted)),
            None => {
                crate::debug_log!("slack: mute state unavailable; keeping the Home order");
                None
            }
        },
        Err(error) => {
            crate::debug_log!("slack: client.counts failed; keeping the Home order: {error}");
            None
        }
    };
    let answered = replies.0.is_some() || replies.1.is_some() || replies.2.is_some();
    let account = client.known_identity();
    let previous = account
        .and_then(|account| slack_api::read_cache(slack_api::LIVE_FILE, account))
        .map(|(live, _)| live);
    let mut live = merge_live(previous, replies);
    if let Some((threads, activity)) = badges {
        live.unread_threads = threads.or(live.unread_threads);
        live.unread_activity = activity.or(live.unread_activity);
    }
    if let Some(account) = account.filter(|_| answered || badges.is_some()) {
        slack_api::write_cache(slack_api::LIVE_FILE, account, &live);
    }
    Ok(home_list(
        &directory,
        &live,
        unread.as_deref(),
        config.open_in,
    ))
}

/// The home rows from the cache, without asking Slack, to show until
/// [`load_home`] lands.
pub(super) fn cached_home(config: &crate::SlackAddonConfig) -> Option<Vec<CommandItem>> {
    let directory = slack_api::cached_directory()?;
    let live = slack_api::read_cache(slack_api::LIVE_FILE, &directory.workspace)
        .map(|(live, _)| live)
        .unwrap_or_default();
    Some(home_list(&directory, &live, None, config.open_in))
}

/// The Actions rows. Blocking.
pub(super) fn load_actions(
    config: crate::SlackAddonConfig,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, _) = connect(&config, refresh)?;
    let replies = fetch_live(&client);
    if let Some(error) = client.rejection() {
        return Err(error);
    }
    let answered = replies.0.is_some() || replies.1.is_some() || replies.2.is_some();
    let account = client.known_identity();
    let previous = account
        .and_then(|account| slack_api::read_cache(slack_api::LIVE_FILE, account))
        .map(|(live, _)| live);
    let live = merge_live(previous, replies);
    if let Some(account) = account.filter(|_| answered) {
        slack_api::write_cache(slack_api::LIVE_FILE, account, &live);
    }
    Ok(account_rows(&live, Local::now()))
}

/// The Actions rows from the cache, to show until [`load_actions`] lands.
pub(super) fn cached_actions() -> Option<Vec<CommandItem>> {
    let directory = slack_api::cached_directory()?;
    let (live, _) = slack_api::read_cache(slack_api::LIVE_FILE, &directory.workspace)?;
    Some(account_rows(&live, Local::now()))
}

fn home_list(
    directory: &Directory,
    live: &Live,
    unread: Option<&[Unread]>,
    open_in: SlackOpenIn,
) -> Vec<CommandItem> {
    let mut rows = home_rows(directory, live, unread, open_in, Local::now());
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
    rows
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
            if directory.conversation(channel).is_none() {
                note_search_channel(&hit["channel"]);
            }
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
    let (client, directory) = connect(&config, false)?;
    let reply = client.call(
        "search.messages",
        &[
            ("query", &terms),
            ("count", SEARCH_COUNT),
            ("highlight", "false"),
        ],
    )?;
    let mut rows = search_rows(&reply, &directory, config.open_in, Local::now());
    if rows.is_empty() {
        rows.push(row(
            "slack:none",
            format!("No messages match \u{201c}{terms}\u{201d}"),
            &["edit-find", SLACK_ICON],
        ));
    }
    remember_search(View::Search(terms), &rows);
    Ok(rows)
}

/// A search or people lookup of this process, with the account epoch it
/// was made in.
type Search = (View, u64, std::time::Instant, Vec<CommandItem>);

/// Searches and people lookups of this process, newest last.
static SEARCHES: std::sync::Mutex<Vec<Search>> = std::sync::Mutex::new(Vec::new());

fn remember_search(view: View, rows: &[CommandItem]) {
    let epoch = slack_api::account_epoch();
    let Ok(mut searches) = SEARCHES.lock() else {
        return;
    };
    searches.retain(|(seen, ..)| seen != &view);
    if searches.len() >= MAX_CACHED_SEARCHES {
        searches.remove(0);
    }
    searches.push((view, epoch, std::time::Instant::now(), rows.to_vec()));
}

/// The rows of a search or people lookup done while they are fresh, for the
/// account of the credentials.
pub(super) fn cached_search(view: &View) -> Option<Vec<CommandItem>> {
    let searches = SEARCHES.lock().ok()?;
    let epoch = slack_api::account_epoch();
    searches
        .iter()
        .find(|(seen, made, at, _)| {
            seen == view && *made == epoch && at.elapsed() < slack_api::cache_fresh()
        })
        .map(|(.., rows)| rows.clone())
}

/// People whose name matches `query`: the whole organization on Enterprise
/// Grid, where the directory only holds the people of direct conversations,
/// else the directory. Blocking.
pub(super) fn load_people(
    config: crate::SlackAddonConfig,
    query: String,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config, refresh)?;
    let found: Vec<User> = if directory.workspace.enterprise && client.is_session() {
        slack_api::search_people(&client, &directory.workspace, &query)?
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
    let mut found = found;
    // People before bots, those you have a conversation with first.
    found.sort_by_key(|user| (user.is_bot, directory.direct_with(&user.id).is_none()));
    let now = Local::now();
    let mut rows: Vec<CommandItem> = found
        .iter()
        .map(|user| user_row(user, &directory, config.open_in, now))
        .collect();
    if rows.is_empty() {
        rows.push(row(
            "slack:none",
            format!("Nobody is called \u{201c}{query}\u{201d}"),
            &["system-users", SLACK_ICON],
        ));
    }
    remember_search(View::People(query), &rows);
    Ok(rows)
}

/// A conversation with unread messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Unread {
    pub channel: String,
    pub last_read: String,
    /// Timestamp of the newest message.
    pub latest: String,
    pub mentions: u64,
    /// Unread messages, when counted.
    pub count: Option<usize>,
    pub muted: bool,
}

/// The conversations muted in a `users.prefs.get` reply: from
/// `all_notifications_prefs`, a JSON document in a string, and the older
/// `muted_channels` list.
pub(super) fn muted_channels(reply: &Value) -> HashSet<String> {
    let prefs = &reply["prefs"];
    let notifications = match &prefs["all_notifications_prefs"] {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let mut muted: HashSet<String> = notifications["channels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, channel)| channel["muted"].as_bool() == Some(true))
        .map(|(id, _)| id.clone())
        .collect();
    muted.extend(
        prefs["muted_channels"]
            .as_str()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    );
    muted
}

/// Unmuted conversations first, newest first in each group, so muted ones
/// only fill what the cap leaves.
fn sort_unread(unread: &mut Vec<Unread>) {
    unread.sort_by(|a, b| a.muted.cmp(&b.muted).then_with(|| b.latest.cmp(&a.latest)));
    unread.truncate(MAX_UNREAD);
}

fn parse_counts_unread(reply: &Value, muted: &HashSet<String>) -> Vec<Unread> {
    ["channels", "mpims", "ims"]
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
                muted: muted.contains(entry["id"].as_str()?),
            })
        })
        .collect()
}

/// Conversations `client.counts` reports with unread messages, unmuted
/// then muted, newest first.
pub(super) fn counts_unread(reply: &Value, muted: &HashSet<String>) -> Vec<Unread> {
    let mut unread = parse_counts_unread(reply, muted);
    sort_unread(&mut unread);
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
fn check_configured(
    client: &Client,
    directory: &Directory,
    entries: &[String],
    muted: &HashSet<String>,
) -> Checked {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let channels: Vec<String> = entries
        .iter()
        .filter_map(|entry| resolve_conversation(entry, directory))
        .take(MAX_UNREAD)
        .collect();
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                while let Some(channel) = channels.get(next.fetch_add(1, Ordering::Relaxed)) {
                    match check_conversation(client, channel) {
                        Ok(Some(unread)) => {
                            found
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(Unread {
                                    muted: muted.contains(channel),
                                    ..unread
                                })
                        }
                        Ok(None) => {}
                        Err(error) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            crate::debug_log!("slack: cannot check {channel}: {error}");
                        }
                    }
                }
            });
        }
    });
    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    sort_unread(&mut found);
    Checked {
        found,
        asked: channels.len(),
        failed: failed.into_inner(),
    }
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
        muted: false,
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
            item.badge = if entry.muted {
                Some("Muted")
            } else {
                directory
                    .conversation(&entry.channel)
                    .map(|c| match c.kind {
                        ConversationKind::Direct => "Direct",
                        ConversationKind::Group => "Group",
                        _ => "Channel",
                    })
            }
            .map(str::to_string);
            item.dimmed = entry.muted;
            item.details = vec![
                ("Enter".to_string(), target_name(open_in).to_string()),
                ("Ctrl+Enter".to_string(), "Mark as read".to_string()),
            ];
            item.primary = Some(open_unread(
                conversation_url(open_in, workspace, &entry.channel),
                entry.channel.clone(),
                entry.latest.clone(),
            ));
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

/// The conversations the user muted, from the cache while it is fresh
/// unless `refresh`, else from Slack, or none when Slack does not say.
/// Blocking unless cached.
fn muted(client: &std::sync::Arc<Client>, refresh: bool) -> Option<HashSet<String>> {
    if refresh {
        return fetch_muted(client);
    }
    let cached = client
        .known_identity()
        .and_then(|account| slack_api::read_cache(slack_api::MUTED_FILE, account));
    let Some((muted, age)) = cached else {
        return fetch_muted(client);
    };
    if age >= slack_api::cache_fresh() {
        refresh_muted_in_background(client.clone());
    }
    Some(muted)
}

fn refresh_muted_in_background(client: std::sync::Arc<Client>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        fetch_muted(&client);
        RUNNING.store(false, Ordering::SeqCst);
    });
}

fn fetch_muted(client: &Client) -> Option<HashSet<String>> {
    match client.call("users.prefs.get", &[]) {
        Ok(reply) => {
            let muted = muted_channels(&reply);
            if let Some(account) = client.known_identity() {
                slack_api::write_cache(slack_api::MUTED_FILE, account, &muted);
            }
            Some(muted)
        }
        Err(error) => {
            crate::debug_log!("slack: users.prefs.get failed: {error}");
            None
        }
    }
}

/// Conversations with unread messages, from `client.counts` when the token
/// allows it, else from the configured list, muted ones last. Blocking.
pub(super) fn load_unread(
    config: crate::SlackAddonConfig,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config, refresh)?;
    let (counts, muted) = std::thread::scope(|scope| {
        let muted = scope.spawn(|| muted(&client, refresh));
        let counts = client.call("client.counts", &[("thread_counts_by_channel", "true")]);
        (counts, muted.join().unwrap_or_default())
    });
    let muted = muted.unwrap_or_default();
    let checked = match counts {
        Ok(reply) => Checked::complete(counts_unread(&reply, &muted)),
        Err(error) => {
            if let Some(rejection) = client.rejection() {
                return Err(rejection);
            }
            if config.unread_conversations.is_empty() {
                crate::debug_log!("slack: client.counts failed: {error}");
                return Err("Slack does not list unread conversations for this token: \
                     set addons.slack.unread_conversations to the ones to check"
                    .to_string());
            }
            crate::debug_log!("slack: client.counts failed, checking the configured list: {error}");
            check_configured(&client, &directory, &config.unread_conversations, &muted)
        }
    };
    if let Some(error) = client.rejection() {
        return Err(error);
    }
    let account = client.known_identity();
    let cached = || {
        account
            .and_then(|account| slack_api::read_cache(slack_api::UNREAD_FILE, account))
            .map(|(unread, _)| unread)
    };
    let (unread, warning, keep) = unread_outcome(checked, cached)?;
    if let Some(account) = account.filter(|_| keep) {
        slack_api::write_cache(slack_api::UNREAD_FILE, account, &unread);
    }
    let mut rows = unread_list(&unread, &directory, config.open_in);
    if let Some(warning) = warning {
        rows.insert(
            0,
            row("slack:warning", warning, &["dialog-warning", SLACK_ICON]),
        );
    }
    Ok(rows)
}

/// The unread conversations found, and how many of those asked about could
/// not be checked.
pub(super) struct Checked {
    pub found: Vec<Unread>,
    pub asked: usize,
    pub failed: usize,
}

impl Checked {
    fn complete(found: Vec<Unread>) -> Self {
        Self {
            found,
            asked: 0,
            failed: 0,
        }
    }
}

/// What to show for `checked`, a warning to show above it, and whether it is
/// complete enough to cache. When nothing could be checked, the last list
/// from `cached` is shown rather than an empty one.
pub(super) fn unread_outcome(
    checked: Checked,
    cached: impl FnOnce() -> Option<Vec<Unread>>,
) -> Result<(Vec<Unread>, Option<String>, bool), String> {
    let Checked {
        found,
        asked,
        failed,
    } = checked;
    if failed == 0 {
        return Ok((found, None, true));
    }
    if failed < asked {
        let warning = format!("Could not check {failed} of {asked} conversations");
        return Ok((found, Some(warning), false));
    }
    let message = "Could not check the unread conversations";
    match cached() {
        Some(last) => Ok((
            last,
            Some(format!("{message}, showing the last list")),
            false,
        )),
        None => Err(message.to_string()),
    }
}

/// The unread rows from the cache, without asking Slack, to show until
/// [`load_unread`] lands.
pub(super) fn cached_unread(config: &crate::SlackAddonConfig) -> Option<Vec<CommandItem>> {
    let directory = slack_api::cached_directory()?;
    let (unread, _): (Vec<Unread>, _) =
        slack_api::read_cache(slack_api::UNREAD_FILE, &directory.workspace)?;
    Some(unread_list(&unread, &directory, config.open_in))
}

fn unread_list(unread: &[Unread], directory: &Directory, open_in: SlackOpenIn) -> Vec<CommandItem> {
    if unread.is_empty() {
        return vec![row(
            "slack:none",
            "No unread messages",
            &["mail-read", SLACK_ICON],
        )];
    }
    unread_rows(unread, directory, open_in, Local::now())
}

/// Shown when Slack refuses a web client list to a user token.
fn session_only(what: &str) -> String {
    format!("Slack only lists {what} for a browser session (xoxc-) token")
}

/// The link to `ts` inside the thread `thread_ts`, for the browser.
fn thread_permalink(workspace: &Workspace, channel: &str, ts: &str, thread_ts: &str) -> String {
    let message = format!(
        "{}archives/{channel}/p{}",
        workspace.url,
        ts.replace('.', "")
    );
    if thread_ts.is_empty() || thread_ts == ts {
        return message;
    }
    format!("{message}?thread_ts={thread_ts}&cid={channel}")
}

/// The link to `ts` inside the thread `thread_ts`, carrying the thread on the
/// desktop path too; `permalink` is the browser link as built by
/// `thread_permalink`.
fn thread_message_url(
    open_in: SlackOpenIn,
    workspace: &Workspace,
    channel: &str,
    ts: &str,
    thread_ts: &str,
    permalink: &str,
) -> String {
    let mut url = message_url(open_in, workspace, channel, ts, permalink);
    if open_in == SlackOpenIn::Desktop && !thread_ts.is_empty() && thread_ts != ts {
        url.push_str(&format!("&thread_ts={thread_ts}"));
    }
    url
}

/// A followed thread with unread replies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct UnreadThread {
    pub channel: String,
    pub thread_ts: String,
    /// The parent message as plain text.
    pub text: String,
    /// Who wrote the parent message.
    pub author: String,
    /// Timestamp of the newest unread reply.
    pub latest: String,
    pub unread: usize,
}

/// The threads with unread replies in a `subscriptions.thread.getView`
/// page.
pub(super) fn parse_threads(reply: &Value, directory: &Directory) -> Vec<UnreadThread> {
    reply["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|thread| {
            let root = &thread["root_msg"];
            let replies = thread["unread_replies"]
                .as_array()
                .filter(|replies| !replies.is_empty())?;
            let channel = root["channel"].as_str()?;
            let thread_ts = root["thread_ts"].as_str().or(root["ts"].as_str())?;
            let latest = replies.iter().filter_map(|r| r["ts"].as_str()).max()?;
            Some(UnreadThread {
                channel: channel.to_string(),
                thread_ts: thread_ts.to_string(),
                text: message_text(root, directory),
                author: author(root, directory),
                latest: latest.to_string(),
                unread: replies.len(),
            })
        })
        .collect()
}

/// The followed threads with unread replies, newest first, paging through
/// the threads view until Slack's count of unread replies is reached.
/// Blocking.
fn fetch_threads(
    client: &Client,
    directory: &std::sync::Arc<Directory>,
) -> Result<Vec<UnreadThread>, String> {
    let mut directory = directory.clone();
    let mut found: Vec<UnreadThread> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_THREAD_PAGES {
        let mut params = vec![("limit", THREAD_PAGE)];
        if let Some(cursor) = &cursor {
            params.push(("current_ts", cursor));
        }
        let reply = client.call("subscriptions.thread.getView", &params)?;
        let threads = reply["threads"].as_array().cloned().unwrap_or_default();
        let messages: Vec<Value> = threads
            .iter()
            .flat_map(|thread| {
                std::iter::once(thread["root_msg"].clone()).chain(
                    thread["unread_replies"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        directory = slack_api::learn_users(client, &directory, &message_users(&messages));
        found.extend(parse_threads(&reply, &directory));
        let total = reply["total_unread_replies"].as_u64().unwrap_or(0) as usize;
        let seen: usize = found.iter().map(|thread| thread.unread).sum();
        // Threads come newest reply first; the next page starts below the
        // oldest one.
        cursor = threads
            .iter()
            .filter_map(|thread| thread["root_msg"]["latest_reply"].as_str())
            .min()
            .map(str::to_string);
        if reply["has_more"].as_bool() != Some(true) || seen >= total || cursor.is_none() {
            break;
        }
    }
    found.sort_by(|a, b| b.latest.cmp(&a.latest));
    found.dedup_by(|a, b| a.channel == b.channel && a.thread_ts == b.thread_ts);
    found.truncate(MAX_UNREAD);
    Ok(found)
}

pub(super) fn thread_rows(
    threads: &[UnreadThread],
    directory: &Directory,
    open_in: SlackOpenIn,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let workspace = &directory.workspace;
    threads
        .iter()
        .map(|thread| {
            let where_ = channel_label(&thread.channel, "", directory);
            let title = match first_line(&thread.text) {
                line if line.trim().is_empty() => format!("Thread in {where_}"),
                line => line,
            };
            let mut item = row(
                &format!(
                    "slack:thread:{}:{}:{}",
                    thread.channel, thread.latest, thread.thread_ts
                ),
                title,
                &["mail-unread", SLACK_ICON],
            );
            let replies = if thread.unread == 1 {
                "reply"
            } else {
                "replies"
            };
            item.subtitle = format!(
                "{where_} · {} · {} unread {replies} · latest {}",
                thread.author,
                thread.unread,
                format_ts(&thread.latest, now)
            );
            item.match_text = format!("{} {}", where_.trim_start_matches(['#', '@']), thread.text);
            item.badge = Some("Thread".to_string());
            item.details = vec![
                ("Conversation".to_string(), where_),
                ("Started by".to_string(), thread.author.clone()),
                ("Enter".to_string(), target_name(open_in).to_string()),
                ("Ctrl+Enter".to_string(), "Mark as read".to_string()),
            ];
            let permalink = thread_permalink(
                workspace,
                &thread.channel,
                &thread.latest,
                &thread.thread_ts,
            );
            item.primary = Some(CommandAction {
                action: DeferredAction::Slack(SlackAction::OpenThread {
                    url: thread_message_url(
                        open_in,
                        workspace,
                        &thread.channel,
                        &thread.latest,
                        &thread.thread_ts,
                        &permalink,
                    ),
                    channel: thread.channel.clone(),
                    thread_ts: thread.thread_ts.clone(),
                    ts: thread.latest.clone(),
                }),
                confirm: None,
                keep_open: false,
                progress: None,
            });
            item.secondary = Some(run(
                SlackAction::MarkThreadRead {
                    channel: thread.channel.clone(),
                    thread_ts: thread.thread_ts.clone(),
                    ts: thread.latest.clone(),
                },
                "Marking as read...",
            ));
            item
        })
        .collect()
}

fn threads_list(
    threads: &[UnreadThread],
    directory: &Directory,
    open_in: SlackOpenIn,
) -> Vec<CommandItem> {
    if threads.is_empty() {
        return vec![row(
            "slack:none",
            "No unread threads",
            &["mail-read", SLACK_ICON],
        )];
    }
    thread_rows(threads, directory, open_in, Local::now())
}

/// Followed threads with unread replies. Blocking.
pub(super) fn load_threads(
    config: crate::SlackAddonConfig,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config, refresh)?;
    let threads = fetch_threads(&client, &directory)
        .map_err(|error| web_client_error(&client, error, "unread threads"))?;
    if let Some(account) = client.known_identity() {
        slack_api::write_cache(slack_api::THREADS_FILE, account, &threads);
    }
    let directory = slack_api::cached_directory().unwrap_or(directory);
    Ok(threads_list(&threads, &directory, config.open_in))
}

/// The thread rows from the cache, to show until [`load_threads`] lands.
pub(super) fn cached_threads(config: &crate::SlackAddonConfig) -> Option<Vec<CommandItem>> {
    let directory = slack_api::cached_directory()?;
    let (threads, _): (Vec<UnreadThread>, _) =
        slack_api::read_cache(slack_api::THREADS_FILE, &directory.workspace)?;
    Some(threads_list(&threads, &directory, config.open_in))
}

/// The error to show for a failed web client list: a logged out session,
/// a token that cannot ask, or Slack's own error.
fn web_client_error(client: &Client, error: String, what: &str) -> String {
    if let Some(rejection) = client.rejection() {
        return rejection;
    }
    crate::debug_log!("slack: cannot list {what}: {error}");
    if client.is_session() {
        error
    } else {
        session_only(what)
    }
}

/// An unread item of the Activity tab.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Activity {
    /// The `activity.feed` item type, such as `at_user` or
    /// `message_reaction`.
    pub kind: String,
    pub key: String,
    pub channel: String,
    /// The message, or the newest one of a bundle.
    pub ts: String,
    /// The thread the message is in, if any.
    #[serde(default)]
    pub thread_ts: String,
    /// Who mentioned or reacted.
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub reaction: String,
    /// Unread messages the item bundles, when Slack says.
    #[serde(default)]
    pub count: u64,
    pub feed_ts: String,
}

impl Activity {
    fn badge(&self) -> &'static str {
        match self.kind.as_str() {
            "message_reaction" => "Reaction",
            "thread_v2" => "Thread",
            "dm" | "bot_dm_bundle" | "channel" => "Message",
            _ => "Mention",
        }
    }
}

/// The unread items of an `activity.feed` reply that point at a message.
pub(super) fn parse_activity(reply: &Value) -> Vec<Activity> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    reply["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["is_unread"].as_bool() != Some(false))
        .filter_map(|entry| {
            let item = &entry["item"];
            let kind = item["type"].as_str()?;
            let bundle = &item["bundle_info"];
            let payload = &bundle["payload"];
            let mut activity = Activity {
                kind: kind.to_string(),
                key: text(&entry["key"]),
                feed_ts: text(&entry["feed_ts"]),
                ..Default::default()
            };
            let (message, count) = match kind {
                "thread_v2" => {
                    let thread = &payload["thread_entry"];
                    activity.channel = text(&thread["channel_id"]);
                    activity.ts = text(&thread["latest_ts"]);
                    activity.thread_ts = text(&thread["thread_ts"]);
                    activity.count = thread["unread_msg_count"].as_u64().unwrap_or(0);
                    return (!activity.channel.is_empty() && !activity.ts.is_empty())
                        .then_some(activity);
                }
                "dm" => (
                    &payload["dm_entry"]["latest_message"],
                    &payload["dm_entry"]["unread_msg_count"],
                ),
                "channel" => (
                    &payload["channel_entry"]["latest_message"],
                    &payload["channel_entry"]["unread_msg_count"],
                ),
                "bot_dm_bundle" => (&payload["message"], &bundle["unread_count"]),
                "at_user" | "at_user_group" | "at_channel" | "at_everyone" | "keyword"
                | "message_reaction" => (&item["message"], &Value::Null),
                _ => return None,
            };
            activity.channel = text(&message["channel"]);
            activity.ts = text(&message["ts"]);
            activity.thread_ts = text(&message["thread_ts"]);
            activity.actor = text(&message["author_user_id"]);
            activity.count = count.as_u64().unwrap_or(0);
            if kind == "message_reaction" {
                activity.actor = text(&item["reaction"]["user"]);
                activity.reaction = text(&item["reaction"]["name"]);
            }
            (!activity.channel.is_empty() && !activity.ts.is_empty()).then_some(activity)
        })
        .collect()
}

fn activity_title(activity: &Activity, where_: &str, directory: &Directory) -> String {
    let actor = directory
        .user(&activity.actor)
        .map(|user| user.label().to_string())
        .or_else(|| (!activity.actor.is_empty()).then(|| activity.actor.clone()))
        .unwrap_or_else(|| "Someone".to_string());
    let plural = |one: &str, many: &str| if activity.count == 1 { one } else { many }.to_string();
    match activity.kind.as_str() {
        "at_user" => format!("{actor} mentioned you in {where_}"),
        "at_user_group" => format!("{actor} mentioned your group in {where_}"),
        "at_channel" => format!("{actor} mentioned @channel in {where_}"),
        "at_everyone" => format!("{actor} mentioned @everyone in {where_}"),
        "keyword" => format!("A keyword you follow in {where_}"),
        "message_reaction" => {
            let emoji = replace_emoji_shortcodes(&format!(":{}:", activity.reaction));
            format!("{actor} reacted {emoji} to your message in {where_}")
        }
        "thread_v2" if activity.count > 0 => format!(
            "{} new {} in a thread in {where_}",
            activity.count,
            plural("reply", "replies")
        ),
        "thread_v2" => format!("New replies in a thread in {where_}"),
        "channel" if activity.count > 0 => format!(
            "{} new {} in {where_}",
            activity.count,
            plural("message", "messages")
        ),
        _ => format!("New messages in {where_}"),
    }
}

pub(super) fn activity_rows(
    activity: &[Activity],
    directory: &Directory,
    open_in: SlackOpenIn,
    now: DateTime<Local>,
) -> Vec<CommandItem> {
    let workspace = &directory.workspace;
    activity
        .iter()
        .map(|entry| {
            let where_ = channel_label(&entry.channel, "", directory);
            let thread = if entry.thread_ts == entry.ts {
                ""
            } else {
                entry.thread_ts.as_str()
            };
            let identity = format!(
                "slack:activity:{}:{}:{thread}:{}",
                entry.channel,
                entry.ts,
                entry.key.replace(':', "-")
            );
            let title = activity_title(entry, &where_, directory);
            let mut item = row(&identity, title.clone(), &["mail-unread", SLACK_ICON]);
            let when = if entry.feed_ts.is_empty() {
                &entry.ts
            } else {
                &entry.feed_ts
            };
            item.subtitle = format_ts(when, now);
            item.match_text = title;
            item.badge = Some(entry.badge().to_string());
            item.details = vec![("Conversation".to_string(), where_)];
            item.details.extend(open_details(open_in));
            let permalink = thread_permalink(workspace, &entry.channel, &entry.ts, thread);
            item.primary = Some(open(thread_message_url(
                open_in,
                workspace,
                &entry.channel,
                &entry.ts,
                thread,
                &permalink,
            )));
            item.secondary = Some(open(thread_message_url(
                other(open_in),
                workspace,
                &entry.channel,
                &entry.ts,
                thread,
                &permalink,
            )));
            item
        })
        .collect()
}

fn activity_list(
    activity: &[Activity],
    directory: &Directory,
    open_in: SlackOpenIn,
) -> Vec<CommandItem> {
    if activity.is_empty() {
        return vec![row(
            "slack:none",
            "No unread activity",
            &["mail-read", SLACK_ICON],
        )];
    }
    activity_rows(activity, directory, open_in, Local::now())
}

/// The unread items of the Activity tab, newest first. Blocking.
pub(super) fn load_activity(
    config: crate::SlackAddonConfig,
    refresh: bool,
) -> Result<Vec<CommandItem>, String> {
    let (client, directory) = connect(&config, refresh)?;
    let reply = client
        .call(
            "activity.feed",
            &[
                ("types", ACTIVITY_TYPES),
                ("mode", "chrono_reads_and_unreads"),
                ("unread_only", "true"),
                ("limit", ACTIVITY_COUNT),
            ],
        )
        .map_err(|error| web_client_error(&client, error, "activity"))?;
    let activity = parse_activity(&reply);
    let actors: Vec<String> = activity.iter().map(|entry| entry.actor.clone()).collect();
    let directory = slack_api::learn_users(&client, &directory, &actors);
    if let Some(account) = client.known_identity() {
        slack_api::write_cache(slack_api::ACTIVITY_FILE, account, &activity);
    }
    Ok(activity_list(&activity, &directory, config.open_in))
}

/// The activity rows from the cache, to show until [`load_activity`] lands.
pub(super) fn cached_activity(config: &crate::SlackAddonConfig) -> Option<Vec<CommandItem>> {
    let directory = slack_api::cached_directory()?;
    let (activity, _): (Vec<Activity>, _) =
        slack_api::read_cache(slack_api::ACTIVITY_FILE, &directory.workspace)?;
    Some(activity_list(&activity, &directory, config.open_in))
}

/// Preview bodies and, for a person, the avatar file.
pub(super) type Preview = (Vec<PreviewBody>, Option<String>);

/// Whether rows with `identity` have a preview to load.
pub(super) fn has_preview(identity: &str) -> bool {
    [
        "slack:conv:",
        "slack:user:",
        "slack:msg:",
        "slack:unread:",
        "slack:thread:",
        "slack:activity:",
    ]
    .iter()
    .any(|prefix| identity.starts_with(prefix))
}

/// Maximum gap in seconds between consecutive messages from the same sender
/// before starting a new message group header.
const GROUP_WINDOW_SECS: i64 = 10 * 60;

fn message_reactions(message: &Value) -> Vec<(String, u64)> {
    message["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reaction| {
            let raw = reaction["name"].as_str()?.trim();
            if raw.is_empty() {
                return None;
            }
            let base = raw.split("::").next().unwrap_or(raw);
            let count = reaction["count"].as_u64().unwrap_or(1).max(1);
            let label = slack_emoji(base)
                .map(str::to_string)
                .unwrap_or_else(|| format!(":{base}:"));
            Some((label, count))
        })
        .collect()
}

/// Cached and freshly downloaded avatars for the authors of `list`.
fn fetch_message_avatars(directory: &Directory, list: &[Value]) -> HashMap<String, String> {
    let Some(dir) = avatar_dir() else {
        return HashMap::new();
    };
    let mut avatars = cached_avatars(&dir);
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for message in list {
        let Some(id) = message["user"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if avatars.contains_key(id) || !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(user) = directory
            .user(id)
            .filter(|user| user.avatar.starts_with("https://"))
        {
            missing.push((id.to_string(), user.avatar.clone()));
        }
    }
    if missing.is_empty() {
        return avatars;
    }
    let agent = image_agent();
    let found = download_avatars(&agent, &dir, missing, &|| true, &mut || Vec::new());
    avatars.extend(found);
    avatars
}

fn chat_message_urls(
    workspace: &Workspace,
    channel: &str,
    message: &Value,
) -> (Option<String>, Option<String>) {
    let ts = message["ts"].as_str().unwrap_or_default();
    if channel.is_empty() || ts.is_empty() {
        return (None, None);
    }
    let permalink = message["permalink"].as_str().unwrap_or_default();
    let thread_ts = message["thread_ts"]
        .as_str()
        .filter(|t| !t.is_empty())
        .or_else(|| permalink_thread(permalink));

    let desktop_url = (!workspace.team_id.is_empty()).then(|| {
        let mut url = message_url(SlackOpenIn::Desktop, workspace, channel, ts, permalink);
        if let Some(thread_ts) = thread_ts {
            url.push_str(&format!("&thread_ts={thread_ts}"));
        }
        url
    });
    let browser_url = if !permalink.is_empty() {
        Some(permalink.to_string())
    } else if !workspace.url.is_empty() {
        let mut url = message_url(SlackOpenIn::Browser, workspace, channel, ts, "");
        if let Some(thread_ts) = thread_ts {
            url.push_str(&format!("?thread_ts={thread_ts}&cid={channel}"));
        }
        Some(url)
    } else if !workspace.team_id.is_empty() {
        Some(conversation_url(SlackOpenIn::Browser, workspace, channel))
    } else {
        None
    };
    (desktop_url, browser_url)
}

/// Messages as a structured chat feed with day dividers and grouped consecutive
/// messages from the same sender.
pub(super) fn messages_body(
    messages: &[Value],
    channel: &str,
    heading: &str,
    directory: &Directory,
    avatars: &HashMap<String, String>,
    highlighted_ts: Option<&str>,
    now: DateTime<Local>,
) -> PreviewBody {
    let mut entries: Vec<ChatEntry> = Vec::new();
    let mut last_date: Option<chrono::NaiveDate> = None;
    let mut last_sender: Option<String> = None;
    let mut last_secs: Option<i64> = None;
    let mut last_hhmm = String::new();

    for message in messages {
        let ts = message["ts"].as_str().unwrap_or_default();
        if let Some((date, label)) = day_label(ts, now) {
            if last_date != Some(date) {
                entries.push(ChatEntry::DayDivider(label));
                last_date = Some(date);
                last_sender = None;
                last_secs = None;
            }
        }

        let sender_id = message["user"].as_str().filter(|id| !id.is_empty());
        let author_name = author(message, directory);
        let sender_key = sender_id.unwrap_or(&author_name).to_string();
        let is_self = sender_id.is_some_and(|id| {
            !directory.workspace.user_id.is_empty() && id == directory.workspace.user_id
        });
        let avatar = sender_id.and_then(|id| avatars.get(id).cloned());
        let hhmm = format_hhmm(ts);
        let secs = ts_seconds(ts);

        let (text, truncated) = truncate(&message_text(message, directory), MAX_MESSAGE_CHARS);
        let reactions = message_reactions(message);
        let replies = match message["reply_count"].as_u64().unwrap_or(0) {
            n if n > 0 && message["thread_ts"].as_str() == Some(ts) => n,
            _ => 0,
        };
        let highlighted = highlighted_ts.is_some_and(|hit| !hit.is_empty() && hit == ts);
        let (desktop_url, browser_url) = chat_message_urls(&directory.workspace, channel, message);

        let can_group = last_sender.as_deref() == Some(sender_key.as_str())
            && match (last_secs, secs) {
                (Some(prev), Some(curr)) => (curr - prev).abs() <= GROUP_WINDOW_SECS,
                _ => false,
            };

        if can_group {
            if let Some(ChatEntry::Messages(group)) = entries.last_mut() {
                let time = (!hhmm.is_empty() && hhmm != last_hhmm).then(|| hhmm.clone());
                group.messages.push(ChatMessage {
                    time,
                    text,
                    reactions,
                    replies,
                    highlighted,
                    truncated,
                    desktop_url,
                    browser_url,
                });
                last_secs = secs;
                last_hhmm = hhmm;
                continue;
            }
        }

        last_sender = Some(sender_key);
        last_secs = secs;
        last_hhmm = hhmm.clone();
        entries.push(ChatEntry::Messages(ChatMessageGroup {
            author: author_name,
            is_self,
            avatar,
            time: hhmm,
            messages: vec![ChatMessage {
                time: None,
                text,
                reactions,
                replies,
                highlighted,
                truncated,
                desktop_url,
                browser_url,
            }],
        }));
    }

    PreviewBody::Chat {
        heading: Some(heading.to_string()),
        entries,
    }
}

/// `list` (oldest first) in display order. In a thread the parent stays on top.
fn display_order(mut list: Vec<Value>, newest_first: bool, thread: bool) -> Vec<Value> {
    if newest_first {
        let skip = usize::from(thread).min(list.len());
        list[skip..].reverse();
    }
    list
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
/// asked for again, unless a cached one is shown: then it comes back
/// instead. Blocking.
///
/// Nothing comes back from a load started before the credentials turned
/// out to be another account's (`epoch`).
pub(super) fn load_preview(
    identity: &str,
    wanted: &Wanted,
    cached: bool,
    epoch: u64,
    newest_first: bool,
) -> Option<Preview> {
    std::thread::sleep(PREVIEW_SETTLE);
    if !wanted.is(identity) {
        return None;
    }
    let loaded = slack_api::current_client().and_then(|client| {
        let directory = slack_api::verified_directory(&client)?
            .ok_or("Slack conversations are not loaded yet")?;
        Ok((
            preview_bodies(identity, &client, directory, newest_first),
            client,
        ))
    });
    if slack_api::account_epoch() != epoch {
        return None;
    }
    let (error, avatar) = match loaded {
        Ok(((Ok(bodies), avatar), client)) => {
            if let Some(account) = client.known_identity() {
                store_preview(identity, account, &bodies, avatar.as_deref(), newest_first);
            }
            return Some((bodies, avatar));
        }
        Ok(((Err(error), avatar), _)) => (error, avatar),
        Err(error) => (error, None),
    };
    if cached {
        if let Some(preview) = cached_preview(identity, newest_first) {
            return Some(preview);
        }
    }
    let body = PreviewBody::Prose {
        heading: None,
        content: error,
        truncated: false,
    };
    Some((vec![body], avatar))
}

#[derive(Serialize, Deserialize)]
enum CachedBody {
    Text {
        heading: Option<String>,
        content: String,
        truncated: bool,
    },
    Chat {
        heading: Option<String>,
        entries: Vec<ChatEntry>,
    },
}

/// A preview as kept on disk.
#[derive(Serialize, Deserialize)]
struct CachedPreview {
    #[serde(default)]
    bodies: Vec<CachedBody>,
    avatar: Option<String>,
    /// Message order the bodies were built in.
    #[serde(default)]
    newest_first: bool,
}

fn preview_cache_name(identity: &str) -> String {
    let name: String = identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}/{name}.json", slack_api::PREVIEW_DIR)
}

/// The preview last loaded for `identity`, to show until a fresh one lands.
/// None when it was built in the other message order.
pub(super) fn cached_preview(identity: &str, newest_first: bool) -> Option<Preview> {
    let directory = slack_api::cached_directory()?;
    let (cached, _): (CachedPreview, _) =
        slack_api::read_cache(&preview_cache_name(identity), &directory.workspace)?;
    (!cached.bodies.is_empty() && cached.newest_first == newest_first)
        .then(|| from_cached_preview(cached))
}

fn from_cached_preview(cached: CachedPreview) -> Preview {
    let bodies = cached
        .bodies
        .into_iter()
        .map(|body| match body {
            CachedBody::Text {
                heading,
                content,
                truncated,
            } => PreviewBody::Prose {
                heading,
                content,
                truncated,
            },
            CachedBody::Chat {
                heading,
                mut entries,
            } => {
                for entry in &mut entries {
                    if let ChatEntry::Messages(group) = entry {
                        group.avatar = group.avatar.take().filter(|path| Path::new(path).exists());
                    }
                }
                PreviewBody::Chat { heading, entries }
            }
        })
        .collect();
    let avatar = cached.avatar.filter(|path| Path::new(path).exists());
    (bodies, avatar)
}

fn store_preview(
    identity: &str,
    account: &Workspace,
    bodies: &[PreviewBody],
    avatar: Option<&str>,
    newest_first: bool,
) {
    let Some(cached) = to_cached_preview(bodies, avatar, newest_first) else {
        return;
    };
    slack_api::write_cache(&preview_cache_name(identity), account, &cached);
    if let Some(dir) = slack_api::cache_path(slack_api::PREVIEW_DIR) {
        prune_previews(&dir, MAX_CACHED_PREVIEWS, slack_api::cache_max_age());
    }
}

/// The preview as kept on disk; none when it contains non-cacheable bodies.
fn to_cached_preview(
    bodies: &[PreviewBody],
    avatar: Option<&str>,
    newest_first: bool,
) -> Option<CachedPreview> {
    let bodies: Option<Vec<_>> = bodies
        .iter()
        .map(|body| match body {
            PreviewBody::Prose {
                heading,
                content,
                truncated,
            } => Some(CachedBody::Text {
                heading: heading.clone(),
                content: content.clone(),
                truncated: *truncated,
            }),
            PreviewBody::Chat { heading, entries } => Some(CachedBody::Chat {
                heading: heading.clone(),
                entries: entries.clone(),
            }),
            _ => None,
        })
        .collect();
    Some(CachedPreview {
        bodies: bodies?,
        avatar: avatar.map(str::to_string),
        newest_first,
    })
}

/// Remove the previews older than `max_age`, then the oldest past `keep`.
pub(super) fn prune_previews(dir: &Path, keep: usize, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    files.retain(|(modified, path)| {
        let old = now.duration_since(*modified).is_ok_and(|age| age > max_age);
        if old {
            let _ = std::fs::remove_file(path);
        }
        !old
    });
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    for (_, path) in &files[..files.len() - keep] {
        let _ = std::fs::remove_file(path);
    }
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
    newest_first: bool,
) -> (Result<Vec<PreviewBody>, String>, Option<String>) {
    let now = Local::now();
    let parts: Vec<&str> = identity.split(':').collect();
    let mut avatar = None;
    // On Enterprise Grid the directory lacks most people, so the ones the
    // messages show are looked up first.
    let build_chat = |list: &[Value],
                      channel: &str,
                      heading: &str,
                      highlighted_ts: Option<&str>,
                      directory: &std::sync::Arc<Directory>| {
        let directory = slack_api::learn_users(client, directory, &message_users(list));
        let avatars = fetch_message_avatars(&directory, list);
        let list = display_order(list.to_vec(), newest_first, heading == THREAD_HEADING);
        messages_body(
            &list,
            channel,
            heading,
            &directory,
            &avatars,
            highlighted_ts,
            now,
        )
    };
    let directory = &directory;
    let bodies = match parts.as_slice() {
        ["slack", "conv", channel] => recent(client, channel, &[]).map(|list| {
            vec![build_chat(
                &list,
                channel,
                "Recent messages",
                None,
                directory,
            )]
        }),
        ["slack", "user", user] => {
            let directory = &slack_api::learn_users(client, directory, &[user.to_string()]);
            avatar = directory.user(user).and_then(fetch_avatar);
            match directory.direct_with(user) {
                Some(direct) => recent(client, &direct.id, &[]).map(|list| {
                    vec![build_chat(
                        &list,
                        &direct.id,
                        "Direct messages",
                        None,
                        directory,
                    )]
                }),
                None => Ok(Vec::new()),
            }
        }
        ["slack", "msg" | "thread", channel, ts, thread @ ..]
        | ["slack", "activity", channel, ts, thread @ .., _] => match thread
            .first()
            .filter(|thread| !thread.is_empty())
        {
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
                        vec![build_chat(
                            &shown,
                            channel,
                            THREAD_HEADING,
                            Some(ts),
                            directory,
                        )]
                    })
            }
            None => recent(client, channel, &[("latest", ts), ("inclusive", "true")]).map(|list| {
                let start = list.len().saturating_sub(CONVERSATION_CONTEXT_MESSAGES);
                vec![build_chat(
                    &list[start..],
                    channel,
                    "Conversation",
                    Some(ts),
                    directory,
                )]
            }),
        },
        ["slack", "unread", channel, last_read] => {
            let oldest = [("oldest", *last_read)];
            let extra: &[(&str, &str)] = if last_read.is_empty() { &[] } else { &oldest };
            recent(client, channel, extra).map(|list| {
                vec![build_chat(
                    &list,
                    channel,
                    "Unread messages",
                    None,
                    directory,
                )]
            })
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
            DeferredAction::Slack(SlackAction::Open(url) | SlackAction::OpenUnread { url, .. }) => {
                Some(url.clone())
            }
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
    fn test_cycle_view_keeps_text_and_skips_an_ambiguous_home() {
        let cycle = |query: &str, backwards| cycle_view(query, backwards);
        assert_eq!(cycle("ops", false), "unread ops");
        assert_eq!(cycle("search deploy", false), "unread deploy");
        assert_eq!(cycle("people ada", false), "actions ada");
        assert_eq!(cycle("unread ops", false), "threads ops");
        assert_eq!(cycle("threads ops", false), "activity ops");
        assert_eq!(cycle("activity ops", false), "people ops");
        assert_eq!(cycle("activity ops", true), "threads ops");
        assert_eq!(cycle("actions ops", false), "ops");
        assert_eq!(cycle("ops", true), "actions ops");
        // Home can't hold text starting with a view word, so it is skipped
        // rather than turning into that view.
        assert_eq!(
            cycle("actions search errors", false),
            "unread search errors"
        );
        assert_eq!(cycle("unread people x", true), "actions people x");
        assert_eq!(parse_view(&cycle("unread people x", true)).0, View::Actions);
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
        assert_eq!(parse_view("threads"), (View::Threads, String::new()));
        assert_eq!(
            parse_view("actions snooze"),
            (View::Actions, "snooze".to_string())
        );
        assert_eq!(
            parse_view("activity ops"),
            (View::Activity, "ops".to_string())
        );
        assert_eq!(parse_view("searchy"), (View::Home, "searchy".to_string()));
        assert_eq!(
            parse_view("people  ada l"),
            (View::People("ada l".to_string()), String::new())
        );
    }

    #[test]
    fn test_bots_show_after_people_only_when_you_talk_to_them() {
        let base = directory();
        let bot = |id: &str, name: &str| User {
            is_bot: true,
            tz_offset: 3600,
            ..user(id, name, "")
        };
        let directory = Directory::new(
            base.workspace.clone(),
            [
                base.users.clone(),
                vec![bot("B1", "jirabot"), bot("B2", "webhook")],
            ]
            .concat(),
            [
                base.conversations.clone(),
                vec![conversation(
                    "D2",
                    ConversationKind::Direct,
                    "B1",
                    Some("B1"),
                )],
            ]
            .concat(),
            0,
        );
        let rows = home_rows(
            &directory,
            &Live::default(),
            None,
            SlackOpenIn::Desktop,
            now(),
        );
        let users: Vec<&CommandItem> = rows
            .iter()
            .filter(|row| row.identity.starts_with("slack:user:"))
            .collect();
        let ids: Vec<&str> = users.iter().map(|row| row.identity.as_str()).collect();
        assert_eq!(ids, ["slack:user:U1", "slack:user:U2", "slack:user:B1"]);
        let bot_row = users[2];
        assert_eq!(bot_row.badge.as_deref(), Some("Bot"));
        assert!(!bot_row.details.iter().any(|(key, _)| key == "Local time"));
        assert_eq!(
            command(&bot_row.primary).as_deref(),
            Some("slack://channel?team=T1&id=D2")
        );
    }

    #[test]
    fn test_threads_page_until_every_unread_reply_is_found() {
        let fake = FakeHttp::with(vec![
            (
                200,
                None,
                json!({"ok": true, "has_more": true, "total_unread_replies": 3, "threads": [
                    {
                        "root_msg": {"channel": "C1", "ts": "1.0", "thread_ts": "1.0",
                                     "user": "U1", "text": "deploy?", "latest_reply": "5.0"},
                        "unread_replies": [{"ts": "4.0"}, {"ts": "5.0"}]
                    },
                    {
                        "root_msg": {"channel": "C1", "ts": "2.0", "thread_ts": "2.0",
                                     "user": "U2", "text": "read", "latest_reply": "3.0"},
                        "latest_replies": [{"ts": "3.0"}]
                    }
                ]}),
            ),
            (
                200,
                None,
                json!({"ok": true, "has_more": true, "total_unread_replies": 3, "threads": [
                    {
                        "root_msg": {"channel": "G1", "ts": "0.5", "thread_ts": "0.5",
                                     "user": "U2", "text": "", "latest_reply": "2.5"},
                        "unread_replies": [{"ts": "2.5"}]
                    }
                ]}),
            ),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        let directory = std::sync::Arc::new(directory());
        let threads = fetch_threads(&client, &directory).unwrap();
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "stops once every unread reply is found");
        assert!(calls[1]
            .1
            .contains(&("current_ts".to_string(), "3.0".to_string())));

        let rows = thread_rows(&threads, &directory, SlackOpenIn::Browser, now());
        let summary: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.identity.as_str(), row.title.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("slack:thread:C1:5.0:1.0", "deploy?"),
                ("slack:thread:G1:2.5:0.5", "Thread in #secret"),
            ]
        );
        assert!(rows[0]
            .subtitle
            .starts_with("#general · Alice · 2 unread replies · latest"));
        assert!(rows[1].subtitle.contains("1 unread reply"));
        assert_eq!(
            slack_action(&rows[0].primary),
            Some(SlackAction::OpenThread {
                url: "https://acme.slack.com/archives/C1/p50?thread_ts=1.0&cid=C1".to_string(),
                channel: "C1".to_string(),
                thread_ts: "1.0".to_string(),
                ts: "5.0".to_string(),
            })
        );
        assert_eq!(
            slack_action(&rows[0].secondary),
            Some(SlackAction::MarkThreadRead {
                channel: "C1".to_string(),
                thread_ts: "1.0".to_string(),
                ts: "5.0".to_string(),
            })
        );
        let desktop_rows = thread_rows(&threads, &directory, SlackOpenIn::Desktop, now());
        assert_eq!(
            slack_action(&desktop_rows[0].primary),
            Some(SlackAction::OpenThread {
                url: "slack://channel?team=T1&id=C1&message=5.0&thread_ts=1.0".to_string(),
                channel: "C1".to_string(),
                thread_ts: "1.0".to_string(),
                ts: "5.0".to_string(),
            })
        );
    }

    #[test]
    fn test_activity_reads_each_kind_and_skips_read_items() {
        let reply = json!({"ok": true, "items": [
            {"is_unread": true, "feed_ts": "3.0", "key": "k1", "item": {
                "type": "at_user",
                "message": {"channel": "C1", "ts": "3.0", "thread_ts": "1.0", "author_user_id": "U1"}
            }},
            {"is_unread": true, "feed_ts": "2.0", "key": "k2", "item": {
                "type": "message_reaction",
                "message": {"channel": "C1", "ts": "2.0"},
                "reaction": {"user": "U2", "name": "tada"}
            }},
            {"is_unread": true, "feed_ts": "6.0", "key": "k3", "item": {
                "type": "thread_v2",
                "bundle_info": {"payload": {"thread_entry": {
                    "channel_id": "G1", "latest_ts": "6.0", "thread_ts": "5.0", "unread_msg_count": 2
                }}}
            }},
            {"is_unread": true, "feed_ts": "7.0", "key": "k4", "item": {
                "type": "dm",
                "bundle_info": {"payload": {"dm_entry": {"latest_message": {"channel": "D1", "ts": "7.0"}}}}
            }},
            {"is_unread": true, "feed_ts": "8.0", "key": "k5", "item": {
                "type": "list_record_assigned", "message": {"channel": "C1", "ts": "8.0"}
            }},
            {"is_unread": false, "feed_ts": "9.0", "key": "k6", "item": {
                "type": "at_user", "message": {"channel": "C1", "ts": "9.0"}
            }}
        ]});
        let activity = parse_activity(&reply);
        let rows = activity_rows(&activity, &directory(), SlackOpenIn::Desktop, now());
        let summary: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row.identity.as_str(),
                    row.title.as_str(),
                    row.badge.as_deref().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "slack:activity:C1:3.0:1.0:k1",
                    "Alice mentioned you in #general",
                    "Mention"
                ),
                (
                    "slack:activity:C1:2.0::k2",
                    "bob reacted 🎉 to your message in #general",
                    "Reaction"
                ),
                (
                    "slack:activity:G1:6.0:5.0:k3",
                    "2 new replies in a thread in #secret",
                    "Thread"
                ),
                (
                    "slack:activity:D1:7.0::k4",
                    "New messages in @Alice",
                    "Message"
                ),
            ]
        );
        assert_eq!(
            command(&rows[0].primary).as_deref(),
            Some("slack://channel?team=T1&id=C1&message=3.0&thread_ts=1.0")
        );
    }

    #[test]
    fn test_activity_and_thread_previews_open_the_thread_or_the_conversation() {
        let reply = || {
            (
                200,
                None,
                json!({"ok": true, "messages": [{"ts": "1.0", "user": "U1", "text": "hi"}]}),
            )
        };
        let fake = FakeHttp::with(vec![reply(), reply(), reply()]);
        let client = Client::fake(Box::new(fake.clone()));
        let directory = std::sync::Arc::new(directory());
        for identity in [
            "slack:activity:C1:3.0:1.0:k1",
            "slack:activity:C1:2.0::k2",
            "slack:thread:C1:5.0:1.0",
        ] {
            assert!(has_preview(identity));
            let (bodies, _) = preview_bodies(identity, &client, directory.clone(), true);
            assert!(bodies.is_ok_and(|bodies| bodies.len() == 1), "{identity}");
        }
        assert_eq!(
            fake.methods(),
            [
                "conversations.replies",
                "conversations.history",
                "conversations.replies"
            ]
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
            ..Live::default()
        };
        let rows = home_rows(&directory, &live, None, SlackOpenIn::Desktop, now());
        let ids: Vec<&str> = rows.iter().map(|row| row.identity.as_str()).collect();
        assert!(ids.contains(&"slack:do:clear-status"));
        assert!(ids.contains(&"slack:do:active"));
        assert!(!ids.contains(&"slack:do:away"));
        assert!(ids.contains(&"slack:do:resume"));
        assert!(ids.contains(&"slack:do:threads"), "kept until Slack says");
        let account: Vec<&CommandItem> = rows
            .iter()
            .filter(|row| {
                [
                    "slack:do:search",
                    "slack:do:people",
                    "slack:do:clear-status",
                    "slack:do:active",
                    "slack:do:resume",
                ]
                .contains(&row.identity.as_str())
            })
            .collect();
        assert_eq!(account.len(), 5);
        assert!(
            account.iter().all(|row| row.typed_only),
            "listed under Actions"
        );
        let actions = rows
            .iter()
            .find(|row| row.identity == "slack:do:actions")
            .unwrap();
        assert!(!actions.typed_only);
        assert!(actions
            .subtitle
            .starts_with(":pizza: Lunch · Away · Snoozed until"));
        assert!(ids.contains(&"slack:do:activity"));
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
            None,
            SlackOpenIn::Desktop,
            now(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.identity.as_str()).collect();
        assert!(!ids.contains(&"slack:do:clear-status"));
        assert!(!ids.contains(&"slack:do:resume"));
        assert!(ids.contains(&"slack:do:active") && ids.contains(&"slack:do:away"));
    }

    #[test]
    fn test_home_rows_promote_unmuted_unread_channels_groups_and_direct_messages() {
        let mut directory = directory();
        directory = Directory::new(
            directory.workspace.clone(),
            directory.users.clone(),
            [
                directory.conversations.clone(),
                vec![
                    conversation("A1", ConversationKind::Channel, "alpha", None),
                    conversation("M1", ConversationKind::Channel, "mute", None),
                    conversation("G2", ConversationKind::Group, "group", None),
                ],
            ]
            .concat(),
            0,
        );
        let unread = [
            Unread {
                channel: "D1".to_string(),
                latest: "9.0".to_string(),
                ..Unread::default()
            },
            Unread {
                channel: "G2".to_string(),
                latest: "8.0".to_string(),
                ..Unread::default()
            },
            Unread {
                channel: "C1".to_string(),
                latest: "7.0".to_string(),
                ..Unread::default()
            },
            Unread {
                channel: "M1".to_string(),
                latest: "99.0".to_string(),
                muted: true,
                ..Unread::default()
            },
        ];
        let rows = home_rows(
            &directory,
            &Live::default(),
            Some(&unread),
            SlackOpenIn::Desktop,
            now(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.identity.as_str()).collect();
        let first_conversation = ids
            .iter()
            .position(|id| id.starts_with("slack:conv:") || id.starts_with("slack:user:"))
            .unwrap();
        assert_eq!(
            &ids[first_conversation..],
            [
                "slack:user:U1",
                "slack:conv:G2",
                "slack:conv:C1",
                "slack:conv:A1",
                "slack:conv:M1",
                "slack:conv:G1",
                "slack:user:U2",
            ]
        );
        assert!(rows[..first_conversation]
            .iter()
            .all(|row| row.sort_priority == 2));
        assert!(rows[first_conversation..first_conversation + 3]
            .iter()
            .all(|row| row.sort_priority == 1));
    }

    #[test]
    fn test_apply_images_shows_avatars_and_the_workspace_icon() {
        let mut directory = directory();
        directory = Directory::new(
            directory.workspace.clone(),
            directory.users.clone(),
            [
                directory.conversations.clone(),
                vec![conversation("M1", ConversationKind::Group, "mpdm", None)],
            ]
            .concat(),
            0,
        );
        let ids = [
            "slack:user:U1",
            "slack:unread:D1:1.0",
            "slack:conv:C1",
            "slack:unread:G1:1.0",
            "slack:msg:C1:1.0",
            "slack:msg:D1:1.0",
            "slack:conv:M1",
            "slack:user:U2",
        ];
        let mut items: Vec<CommandItem> = ids.iter().map(|id| row(id, *id, &USER_ICONS)).collect();
        let images: HashMap<String, String> = [("U1", "/a/U1.png"), ("T1", "/a/T1.png")]
            .into_iter()
            .map(|(key, path)| (key.to_string(), path.to_string()))
            .collect();
        apply_images(&mut items, Some(&directory), &images);
        let icons: Vec<Option<&str>> = items.iter().map(|item| item.icon.as_deref()).collect();
        assert_eq!(
            icons,
            [
                Some("/a/U1.png"),
                Some("/a/U1.png"),
                Some("/a/T1.png"),
                Some("/a/T1.png"),
                Some("/a/T1.png"),
                None,
                None,
                None
            ]
        );
        assert_eq!(
            items[0].icon_names, USER_ICONS,
            "theme icons stay as fallbacks"
        );

        // Without the directory, only people are known.
        let mut items = vec![
            row("slack:user:U1", "a", &USER_ICONS),
            row("slack:conv:C1", "b", &USER_ICONS),
        ];
        apply_images(&mut items, None, &images);
        assert_eq!(items[0].icon.as_deref(), Some("/a/U1.png"));
        assert_eq!(items[1].icon, None);
    }

    #[test]
    fn test_image_wants_skips_known_and_tried_and_caps_the_batch() {
        let mut users: Vec<User> = (0..40)
            .map(|n| User {
                avatar: format!("https://a/{n}.png"),
                ..user(&format!("U{n:02}"), "x", "")
            })
            .collect();
        users.push(User {
            avatar: "http://insecure/x.png".to_string(),
            ..user("UH", "h", "")
        });
        let base = directory();
        let directory = Directory::new(base.workspace, users, base.conversations, 0);
        let mut identities: Vec<String> = (0..40).map(|n| format!("slack:user:U{n:02}")).collect();
        identities.push("slack:user:U00".to_string());
        identities.push("slack:user:UH".to_string());
        identities.push("slack:conv:C1".to_string());
        identities.push("slack:unread:G1:1.0".to_string());
        let images: HashMap<String, String> = [
            ("U00".to_string(), "/a/U00.png".to_string()),
            ("T1".to_string(), "/a/T1.png".to_string()),
        ]
        .into_iter()
        .collect();
        let tried: HashSet<String> = ["U01".to_string()].into_iter().collect();
        let wants = image_wants(
            identities.iter().map(String::as_str),
            &directory,
            &images,
            &tried,
        );
        assert_eq!(wants.users.len(), MAX_IMAGE_BATCH);
        assert_eq!(
            wants.users[0],
            ("U02".to_string(), "https://a/2.png".to_string())
        );
        assert!(!wants
            .users
            .iter()
            .any(|(id, _)| id == "UH" || id == "U00" || id == "U01"));
        // Asked even when on disk, since it may be due for a refresh.
        assert_eq!(wants.team.as_deref(), Some("T1"));

        let tried: HashSet<String> = ["T1".to_string()].into_iter().collect();
        let wants = image_wants(["slack:unread:C1:1.0"], &directory, &images, &tried);
        assert!(wants.is_empty());
    }

    #[test]
    fn test_cached_avatars_skips_downloads_in_progress_and_misses() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-avatars-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["U1.png", "U1.jpg", ".U2.1.0.tmp", "T1.none"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(dir.join("U1.png"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let found = cached_avatars(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let newest = dir.join("U1.jpg").to_str().unwrap().to_string();
        assert_eq!(found, HashMap::from([("U1".to_string(), newest)]));
    }

    #[test]
    fn test_workspace_icon_is_due_weekly_and_not_after_a_miss() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-team-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(workspace_icon_due(&dir, "T1"), Some(None));
        super::super::favicons::mark_miss(&dir, "T1");
        assert_eq!(workspace_icon_due(&dir, "T1"), None);
        let icon = dir.join("T1.png");
        std::fs::write(&icon, "").unwrap();
        assert_eq!(workspace_icon_due(&dir, "T1"), None);
        let old = std::time::SystemTime::now() - WORKSPACE_ICON_TTL * 2;
        std::fs::File::options()
            .write(true)
            .open(&icon)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let due = workspace_icon_due(&dir, "T1");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(due, Some(Some(icon)));
    }

    #[test]
    fn test_search_hits_outside_the_directory_show_the_workspace_icon() {
        let reply = json!({"messages": {"matches": [
            {"channel": {"id": "CSEARCH1", "name": "ops", "is_private": false, "is_mpim": false},
             "ts": "1.0", "text": "a"},
            {"channel": {"id": "GSEARCH2", "name": "mpdm-a--b-1", "is_private": true, "is_mpim": true},
             "ts": "2.0", "text": "b"},
            {"channel": {"id": "DSEARCH3", "name": "U2", "is_private": true},
             "ts": "3.0", "text": "c"},
        ]}});
        let directory = directory();
        let mut rows = search_rows(&reply, &directory, SlackOpenIn::Desktop, now());
        let images = HashMap::from([("T1".to_string(), "/team.png".to_string())]);
        apply_images(&mut rows, Some(&directory), &images);
        assert_eq!(rows[0].icon.as_deref(), Some("/team.png"));
        assert_eq!(rows[1].icon, None);
        assert_eq!(rows[2].icon, None);
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
        let unread = counts_unread(&reply, &HashSet::new());
        let ids: Vec<&str> = unread.iter().map(|u| u.channel.as_str()).collect();
        assert_eq!(ids, ["D1", "C1"]);
        assert_eq!(unread[1].mentions, 2);

        let rows = unread_rows(&unread, &directory(), SlackOpenIn::Desktop, now());
        assert_eq!(rows[0].title, "@Alice");
        assert_eq!(rows[0].identity, "slack:unread:D1:3.0");
        assert_eq!(
            slack_action(&rows[0].primary),
            Some(SlackAction::OpenUnread {
                url: "slack://channel?team=T1&id=D1".to_string(),
                channel: "D1".to_string(),
                ts: "7.0".to_string(),
            })
        );
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
    fn test_muted_unread_come_last_dimmed_and_only_fill_the_cap() {
        let prefs = json!({"prefs": {
            "all_notifications_prefs": r#"{"channels": {"M1": {"muted": true}, "C1": {"muted": false}}}"#,
            "muted_channels": "M2,",
        }});
        let muted = muted_channels(&prefs);
        assert_eq!(muted, HashSet::from(["M1".to_string(), "M2".to_string()]));

        let entry = |id: &str, latest: usize| json!({"id": id, "has_unreads": true, "last_read": "0", "latest": format!("{latest:03}.0")});
        let mut channels = vec![entry("M1", 999), entry("M2", 998)];
        channels.extend((0..MAX_UNREAD).map(|i| entry(&format!("C{i}"), i)));
        let unread = counts_unread(&json!({ "channels": channels }), &muted);
        assert_eq!(unread.len(), MAX_UNREAD);
        assert!(
            unread.iter().all(|u| !u.muted),
            "unmuted fill the cap first"
        );

        let unread = counts_unread(
            &json!({"channels": [entry("M1", 9), entry("C1", 1)], "ims": [entry("D1", 5)]}),
            &muted,
        );
        let rows = unread_rows(&unread, &directory(), SlackOpenIn::Desktop, now());
        let ids: Vec<&str> = unread.iter().map(|u| u.channel.as_str()).collect();
        assert_eq!(ids, ["D1", "C1", "M1"]);
        assert_eq!(
            rows.iter().map(|r| r.dimmed).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(rows[2].badge.as_deref(), Some("Muted"));
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
        let reply = || {
            (
                200,
                None,
                json!({"ok": true, "messages": [
                    message("1.0", "parent"),
                    message("2.0", "first reply"),
                    message("3.0", "the hit"),
                    message("4.0", "later reply"),
                ]}),
            )
        };
        let fake = FakeHttp::with(vec![reply(), reply()]);
        let client = Client::fake(Box::new(fake.clone()));
        for (newest_first, expected) in [
            (
                false,
                [("parent", false), ("first reply", false), ("the hit", true)],
            ),
            (
                true,
                [("parent", false), ("the hit", true), ("first reply", false)],
            ),
        ] {
            let (bodies, avatar) = preview_bodies(
                "slack:msg:C1:3.0:1.0",
                &client,
                std::sync::Arc::new(directory()),
                newest_first,
            );
            assert_eq!(avatar, None);
            let Ok(bodies) = bodies else {
                panic!("preview failed");
            };
            let [PreviewBody::Chat { heading, entries }] = bodies.as_slice() else {
                panic!("expected one chat body");
            };
            assert_eq!(heading.as_deref(), Some("Thread"));
            let texts: Vec<(&str, bool)> = entries
                .iter()
                .filter_map(|entry| match entry {
                    ChatEntry::Messages(group) => Some(&group.messages),
                    ChatEntry::DayDivider(_) => None,
                })
                .flatten()
                .map(|msg| (msg.text.as_str(), msg.highlighted))
                .collect();
            assert_eq!(texts, expected, "newest_first: {newest_first}");
        }
        assert_eq!(
            fake.methods(),
            ["conversations.replies", "conversations.replies"]
        );
    }

    #[test]
    fn test_channel_preview_lists_newest_first_under_day_dividers() {
        let midnight = Local::now()
            .date_naive()
            .and_hms_opt(0, 1, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap()
            .timestamp();
        let t1 = format!("{}.0001", midnight - 86_400);
        let t2 = format!("{midnight}.0001");
        let t3 = format!("{}.0001", midnight + 60);
        // conversations.history answers newest first.
        let fake = FakeHttp::with(vec![(
            200,
            None,
            json!({"ok": true, "messages": [
                {"ts": t3, "user": "U1", "text": "later today"},
                {"ts": t2, "user": "U1", "text": "today"},
                {"ts": t1, "user": "U2", "text": "yesterday"},
            ]}),
        )]);
        let client = Client::fake(Box::new(fake));
        let (bodies, _) = preview_bodies(
            "slack:conv:C1",
            &client,
            std::sync::Arc::new(directory()),
            true,
        );
        let Ok(bodies) = bodies else {
            panic!("preview failed");
        };
        let [PreviewBody::Chat { entries, .. }] = bodies.as_slice() else {
            panic!("expected one chat body");
        };
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0], ChatEntry::DayDivider("Today".into()));
        let ChatEntry::Messages(today) = &entries[1] else {
            panic!("expected message group");
        };
        assert_eq!(today.time, format_hhmm(&t3), "header shows the newest time");
        let texts: Vec<&str> = today.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["later today", "today"]);
        assert_eq!(entries[2], ChatEntry::DayDivider("Yesterday".into()));
        let ChatEntry::Messages(yesterday) = &entries[3] else {
            panic!("expected message group");
        };
        assert_eq!(yesterday.messages[0].text, "yesterday");
    }

    #[test]
    fn test_messages_body_groups_consecutive_messages_and_divides_days() {
        let mut dir = directory();
        dir.workspace.user_id = "U1".into();
        let today = now();
        let yesterday = today - chrono::Duration::days(1);
        let t1 = format!("{}.0001", yesterday.timestamp());
        let t2 = format!("{}.0001", today.timestamp() - 120);
        let t3 = format!("{}.0001", today.timestamp() - 60);
        let t4 = format!("{}.0001", today.timestamp() + 900);
        let messages = [
            json!({
                "ts": t1,
                "user": "U2",
                "text": "deploy starting :rocket:",
                "reply_count": 2,
                "thread_ts": t1,
                "reactions": [{"name": "+1::skin-tone-3", "count": 3}],
            }),
            json!({"ts": t2, "user": "U1", "text": "first line"}),
            json!({"ts": t3, "user": "U1", "text": "follow up"}),
            json!({"ts": t4, "user": "U1", "text": "15m later"}),
        ];
        let avatars = HashMap::from([("U1".to_string(), "/tmp/u1.png".to_string())]);
        let PreviewBody::Chat { heading, entries } = messages_body(
            &messages,
            "C1",
            "Recent messages",
            &dir,
            &avatars,
            Some(&t3),
            today,
        ) else {
            panic!("expected chat preview body");
        };
        assert_eq!(heading.as_deref(), Some("Recent messages"));
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0], ChatEntry::DayDivider("Yesterday".into()));
        let ChatEntry::Messages(first_group) = &entries[1] else {
            panic!("expected message group");
        };
        assert_eq!(first_group.author, "bob");
        assert!(!first_group.is_self);
        assert_eq!(first_group.messages[0].text, "deploy starting 🚀");
        assert_eq!(first_group.messages[0].replies, 2);
        assert_eq!(
            first_group.messages[0].reactions,
            vec![("👍".to_string(), 3)]
        );
        assert_eq!(
            first_group.messages[0].desktop_url.as_deref(),
            Some(format!("slack://channel?team=T1&id=C1&message={t1}&thread_ts={t1}").as_str())
        );
        assert_eq!(
            first_group.messages[0].browser_url.as_deref(),
            Some(
                format!(
                    "https://acme.slack.com/archives/C1/p{}?thread_ts={t1}&cid=C1",
                    t1.replace('.', "")
                )
                .as_str()
            )
        );

        assert_eq!(entries[2], ChatEntry::DayDivider("Today".into()));
        let ChatEntry::Messages(grouped) = &entries[3] else {
            panic!("expected grouped messages");
        };
        assert_eq!(grouped.author, "Alice");
        assert!(grouped.is_self);
        assert_eq!(grouped.avatar.as_deref(), Some("/tmp/u1.png"));
        assert_eq!(grouped.messages.len(), 2);
        assert_eq!(grouped.messages[0].text, "first line");
        assert!(!grouped.messages[0].highlighted);
        assert_eq!(grouped.messages[1].text, "follow up");
        assert!(grouped.messages[1].highlighted);

        let ChatEntry::Messages(later) = &entries[4] else {
            panic!("expected separate group after 15m gap");
        };
        assert_eq!(later.messages.len(), 1);
        assert_eq!(later.messages[0].text, "15m later");
    }

    #[test]
    fn test_replace_emoji_shortcodes() {
        assert_eq!(
            replace_emoji_shortcodes("ship it :rocket: :+1::skin-tone-4: and :unknown_code:"),
            "ship it 🚀 👍 and :unknown_code:"
        );
        assert_eq!(
            replace_emoji_shortcodes("https://example.com:8080/path"),
            "https://example.com:8080/path"
        );
    }

    #[test]
    fn test_searches_are_remembered_per_view() {
        let rows = vec![row("slack:none", "No messages match", &[SLACK_ICON])];
        let view = View::Search("remembered terms".into());
        assert!(cached_search(&view).is_none());
        remember_search(view.clone(), &rows);
        let found = cached_search(&view).unwrap();
        assert_eq!(found[0].identity, "slack:none");
        assert!(cached_search(&View::People("remembered terms".into())).is_none());
    }

    #[test]
    fn test_prune_previews_drops_old_then_oldest() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-previews-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        for (name, age) in [("old", 3 * 3600), ("a", 30), ("b", 20), ("c", 10)] {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(now - std::time::Duration::from_secs(age))
                .unwrap();
        }
        prune_previews(&dir, 2, std::time::Duration::from_secs(3600));
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["b", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_thread_and_activity_rows_hide_when_nothing_is_unread() {
        let counts = json!({
            "threads": {"has_unreads": false},
            "activity_v2": {"at_user": 0, "thread_v2": 0, "list_record_edited": 4}
        });
        assert_eq!(parse_counts_badges(&counts), (Some(false), Some(false)));
        let counts = json!({
            "threads": {"has_unreads": true},
            "activity_v2": {"message_reaction": 2}
        });
        assert_eq!(parse_counts_badges(&counts), (Some(true), Some(true)));

        let live = Live {
            unread_threads: Some(false),
            unread_activity: Some(true),
            ..Live::default()
        };
        let ids: Vec<String> = shortcut_rows(&live, now())
            .into_iter()
            .map(|row| row.identity)
            .collect();
        assert_eq!(
            &ids[..3],
            ["slack:do:unread", "slack:do:activity", "slack:do:actions"]
        );
    }

    #[test]
    fn test_failed_live_calls_keep_the_last_known_state() {
        let previous = Live {
            status: Some(("🍕".into(), "Lunch".into(), 0)),
            snoozed_until: Some(1_900),
            away: Some(true),
            unread_threads: Some(true),
            unread_activity: Some(false),
        };
        let fresh = merge_live(
            Some(previous.clone()),
            (None, Some(json!({"snooze_enabled": false})), None),
        );
        assert_eq!(fresh.status, previous.status, "profile call failed");
        assert_eq!(fresh.snoozed_until, None, "snooze ended");
        assert_eq!(fresh.away, Some(true), "presence call failed");
        assert_eq!(
            (fresh.unread_threads, fresh.unread_activity),
            (Some(true), Some(false)),
            "counts are kept"
        );
    }

    #[test]
    fn test_actions_update_the_cached_state() {
        let mut live = Live::default();
        let status = StatusArgs {
            text: "Lunch".into(),
            emoji: ":pizza:".into(),
            expiry: Expiry::Minutes(30),
        };
        apply_to_live(&mut live, &SlackAction::SetStatus(status), now());
        apply_to_live(&mut live, &SlackAction::SetPresence { away: true }, now());
        apply_to_live(&mut live, &SlackAction::Snooze(Snooze::Minutes(10)), now());
        let at = now().timestamp();
        assert_eq!(
            live,
            Live {
                status: Some((":pizza:".into(), "Lunch".into(), at + 30 * 60)),
                snoozed_until: Some(at + 10 * 60),
                away: Some(true),
                ..Live::default()
            }
        );
        apply_to_live(&mut live, &SlackAction::EndSnooze, now());
        assert_eq!(live.snoozed_until, None);

        let entry = |channel: &str| Unread {
            channel: channel.into(),
            ..Unread::default()
        };
        let mut unread = vec![entry("C1"), entry("C2"), entry("C3")];
        let read = SlackAction::MarkRead {
            channel: "C1".into(),
            ts: "1.0".into(),
        };
        apply_to_unread(&mut unread, &read);
        assert_eq!(unread, [entry("C2"), entry("C3")]);
        let opened = SlackAction::OpenUnread {
            url: "slack://channel?team=T1&id=C2".into(),
            channel: "C2".into(),
            ts: "2.0".into(),
        };
        apply_to_unread(&mut unread, &opened);
        assert_eq!(unread, [entry("C3")]);
    }

    #[test]
    fn test_unread_refresh_failures_keep_the_last_list() {
        let entry = |channel: &str| Unread {
            channel: channel.into(),
            ..Unread::default()
        };
        let checked = |found: Vec<Unread>, asked, failed| Checked {
            found,
            asked,
            failed,
        };
        let last = || Some(vec![entry("C9")]);

        let (rows, warning, keep) = unread_outcome(checked(vec![entry("C1")], 2, 0), last).unwrap();
        assert_eq!((rows, warning, keep), (vec![entry("C1")], None, true));

        let (rows, warning, keep) = unread_outcome(checked(vec![entry("C1")], 2, 1), last).unwrap();
        assert_eq!(rows, [entry("C1")]);
        assert_eq!(
            warning.as_deref(),
            Some("Could not check 1 of 2 conversations")
        );
        assert!(!keep, "a partial list is not cached");

        let (rows, warning, keep) = unread_outcome(checked(Vec::new(), 2, 2), last).unwrap();
        assert_eq!(rows, [entry("C9")], "the last list, not an empty one");
        assert!(warning.unwrap().contains("showing the last list"));
        assert!(!keep);

        let error = unread_outcome(checked(Vec::new(), 2, 2), || None).unwrap_err();
        assert_eq!(error, "Could not check the unread conversations");
    }

    #[test]
    fn test_text_and_chat_previews_survive_the_disk_cache() {
        let text_body = PreviewBody::Prose {
            heading: Some("Error".into()),
            content: "network failure".into(),
            truncated: false,
        };
        let chat_body = PreviewBody::Chat {
            heading: Some("Unread messages".into()),
            entries: vec![
                ChatEntry::DayDivider("Today".into()),
                ChatEntry::Messages(ChatMessageGroup {
                    author: "Alice".into(),
                    is_self: false,
                    avatar: Some("/no/such/avatar.png".into()),
                    time: "09:00".into(),
                    messages: vec![ChatMessage {
                        time: None,
                        text: "hello".into(),
                        reactions: vec![("👍".into(), 2)],
                        replies: 1,
                        highlighted: false,
                        truncated: false,
                        desktop_url: Some(
                            "slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into(),
                        ),
                        browser_url: Some(
                            "https://acme.slack.com/archives/C1/p10?thread_ts=1.0&cid=C1".into(),
                        ),
                    }],
                }),
            ],
        };
        let input = [text_body.clone(), chat_body];
        let cached = to_cached_preview(&input, Some("/no/avatar.png"), true).unwrap();
        let data = serde_json::to_vec(&cached).unwrap();
        let (bodies, avatar) = from_cached_preview(serde_json::from_slice(&data).unwrap());
        assert_eq!(bodies[0], text_body);
        let PreviewBody::Chat { entries, .. } = &bodies[1] else {
            panic!("expected chat body");
        };
        let ChatEntry::Messages(group) = &entries[1] else {
            panic!("expected message group");
        };
        assert_eq!(group.avatar, None, "missing group avatar is dropped");
        assert_eq!(avatar, None, "an avatar gone from disk is dropped");
        let image = PreviewBody::Image {
            path: "/tmp/image.png".into(),
        };
        assert!(to_cached_preview(&[image], None, true).is_none());
    }

    #[test]
    fn test_preview_cache_names_stay_in_the_previews_directory() {
        assert_eq!(
            preview_cache_name("slack:msg:C1:1700000000.000100:../x"),
            "previews/slack_msg_C1_1700000000.000100_.._x.json"
        );
    }
}
