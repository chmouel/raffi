//! F5 and Ctrl+Shift+F5: refresh through the optional cache service.
//!
//! Raffi asks a running `raffi --cache-daemon` to re-fetch a source, waits
//! for it, then reloads the view from the snapshot the service wrote.

use std::sync::Arc;

use iced::Task;

use super::browser_data::off_thread;
use super::cache::{self, CallError, Request, ServerInfo};
use super::command_list::CommandListAddon;
use super::slack::View;
use super::state::LauncherApp;
use super::types::Message;

#[derive(Debug, Default)]
pub(super) struct ServerState {
    /// The running cache service, if the startup probe found one.
    pub info: Option<ServerInfo>,
    /// The service was started with other addon settings than this Raffi.
    pub config_differs: bool,
    /// The refresh waiting for the service.
    pub pending: Option<Pending>,
    pub token: u64,
    /// Outcome of the last refresh, until the query changes.
    pub notice: Option<Notice>,
}

impl ServerState {
    /// Whether F5 can be used: a service speaking the waited protocol.
    pub fn ready(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|info| info.protocol >= cache::PROTOCOL)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Notice {
    Refreshed,
    Failed(String),
    Unreachable,
}

#[derive(Debug)]
pub(super) struct Pending {
    token: u64,
    target: Option<Target>,
}

/// The view a refresh reloads once the service is done, with what tells
/// whether it is still shown. Lists are told apart by their session and
/// view, not their load generation: filtering rebuilds the rows meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Slack {
        session: u64,
        view: View,
    },
    BrowserHistory {
        session: u64,
        group: String,
    },
    ScriptFilter {
        index: usize,
        query: String,
        generation: u64,
    },
}

/// A view whose rows the cache service can re-fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backed {
    Slack,
    BrowserHistory,
    ScriptFilter(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Muted,
    Warning,
    Error,
}

/// One footer hint about the cache service: an optional key, then a label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServerHint {
    pub key: Option<&'static str>,
    pub label: String,
    pub tone: Tone,
}

impl ServerHint {
    fn new(key: Option<&'static str>, label: impl Into<String>, tone: Tone) -> Self {
        Self {
            key,
            label: label.into(),
            tone,
        }
    }
}

const MAX_NOTICE_CHARS: usize = 80;

impl LauncherApp {
    pub(super) fn handle_cache_server_status(&mut self, info: Option<ServerInfo>) -> Task<Message> {
        self.server.config_differs = info
            .as_ref()
            .is_some_and(|info| info.config_digest != cache::addons_digest(&self.addons));
        self.server.info = info;
        Task::none()
    }

    /// The shown view F5 can refresh through the cache service.
    pub(super) fn server_backed_view(&self) -> Option<Backed> {
        if self.view.help_active || self.vault.prompt.is_some() || self.clipboard.active {
            return None;
        }
        if let Some(addon) = self.command_list.addon {
            return match addon {
                CommandListAddon::Slack
                    if matches!(
                        self.slack.view,
                        Some(View::Home | View::Unread | View::Threads | View::Activity)
                    ) =>
                {
                    Some(Backed::Slack)
                }
                CommandListAddon::BrowserHistory
                    if self.browser_history.initialized
                        && self.browser_history.active_group().is_some() =>
                {
                    Some(Backed::BrowserHistory)
                }
                _ => None,
            };
        }
        if self.file_browser.active || self.text_snippets.active {
            return None;
        }
        let index = self.script_filter.config_index?;
        self.addons
            .script_filters
            .get(index)
            .and_then(|config| config.cache_ttl())
            .map(|_| Backed::ScriptFilter(index))
    }

    /// Whether the footer offers F5. It then never also offers Ctrl+R, which
    /// still refreshes locally.
    pub(super) fn offers_f5(&self) -> bool {
        self.server.ready() && self.server_backed_view().is_some()
    }

    /// Footer hints about the cache service. Ctrl+Shift+F5 is left out on
    /// purpose; the help lists it.
    pub(super) fn server_hints(&self) -> Vec<ServerHint> {
        let mut hints = Vec::new();
        if self.server.pending.is_some() {
            hints.push(ServerHint::new(None, "server refreshing…", Tone::Muted));
            return hints;
        }
        match &self.server.notice {
            Some(Notice::Failed(message)) => {
                let mut short: String = message.chars().take(MAX_NOTICE_CHARS).collect();
                if short.len() < message.len() {
                    short.push('…');
                }
                hints.push(ServerHint::new(
                    None,
                    format!("server: {short}"),
                    Tone::Error,
                ));
            }
            Some(Notice::Unreachable) => {
                hints.push(ServerHint::new(None, "server not reachable", Tone::Error));
            }
            _ => {}
        }
        if self.server.info.is_none() {
            return hints;
        }
        if !self.server.ready() {
            hints.push(ServerHint::new(
                None,
                "server outdated, restart it",
                Tone::Warning,
            ));
            return hints;
        }
        if self.server.config_differs {
            hints.push(ServerHint::new(
                None,
                "server config differs, restart it",
                Tone::Warning,
            ));
        }
        let refreshed = self.server.notice == Some(Notice::Refreshed);
        if self.offers_f5() {
            let label = if refreshed {
                " server refreshed"
            } else {
                " server refresh"
            };
            hints.push(ServerHint::new(Some("F5"), label, Tone::Muted));
        } else if hints.is_empty() {
            let label = if refreshed {
                "server refreshed"
            } else {
                "server"
            };
            hints.push(ServerHint::new(None, label, Tone::Muted));
        }
        hints
    }

    /// F5 refreshes the shown view, Ctrl+Shift+F5 (`all`) every source the
    /// service knows, both through the service.
    pub(super) fn handle_server_refresh(&mut self, all: bool) -> Task<Message> {
        if !self.server.ready() || self.server.pending.is_some() {
            return Task::none();
        }
        let backed = self.server_backed_view();
        if backed.is_none() && !all {
            return Task::none();
        }
        let session = self.command_list.session;
        let target = match backed {
            Some(Backed::Slack) => self
                .slack
                .view
                .clone()
                .map(|view| Target::Slack { session, view }),
            Some(Backed::BrowserHistory) => {
                self.browser_history
                    .active_group()
                    .map(|group| Target::BrowserHistory {
                        session,
                        group: group.key.clone(),
                    })
            }
            Some(Backed::ScriptFilter(index)) => Some(Target::ScriptFilter {
                index,
                query: self.script_filter.query.clone(),
                generation: self.script_filter.generation,
            }),
            None => None,
        };
        let mut requests = Vec::new();
        // RefreshAll only warms the configured queries, not the shown one.
        if let Some(Target::ScriptFilter { index, query, .. }) = &target {
            let view = self.script_filter.active_view();
            requests.extend(
                self.addons
                    .script_filters
                    .get(*index)
                    .map(|config| super::script_filters::view_config(config, view))
                    .and_then(|config| cache::script_filter_refresh_request(&config, query)),
            );
        }
        let source = match backed {
            Some(Backed::Slack) => match self.slack.view {
                Some(View::Home) => Some("slack:home"),
                Some(View::Unread) => Some("slack:unread"),
                Some(View::Threads) => Some("slack:threads"),
                _ => Some("slack:activity"),
            },
            Some(Backed::BrowserHistory) => Some("browser_history"),
            _ => None,
        };
        if all {
            requests.push(Request::RefreshAll);
        } else if let Some(source) = source {
            requests.push(Request::RefreshSource {
                source: source.into(),
                wait: true,
            });
        }
        self.server.token = self.server.token.wrapping_add(1);
        let token = self.server.token;
        self.server.pending = Some(Pending { token, target });
        self.server.notice = None;
        Task::perform(
            off_thread(move || cache::refresh(requests)),
            move |result| {
                Message::ServerRefreshDone(
                    token,
                    result.unwrap_or_else(|| {
                        Err(CallError::Failed("the refresh stopped unexpectedly".into()))
                    }),
                )
            },
        )
    }

    pub(super) fn handle_server_refresh_done(
        &mut self,
        token: u64,
        result: Result<(), CallError>,
    ) -> Task<Message> {
        if self
            .server
            .pending
            .as_ref()
            .is_none_or(|pending| pending.token != token)
        {
            return Task::none();
        }
        let target = self
            .server
            .pending
            .take()
            .and_then(|pending| pending.target);
        if let Err(error) = result {
            self.server.notice = Some(match error {
                CallError::Unreachable => {
                    self.server.info = None;
                    Notice::Unreachable
                }
                CallError::Failed(message) => Notice::Failed(message),
            });
            return Task::none();
        }
        self.server.notice = Some(Notice::Refreshed);
        // A load started meanwhile brings rows at least as fresh.
        let idle = |app: &Self, addon, session| {
            app.command_list.addon == Some(addon)
                && app.command_list.session == session
                && !app.command_list.loading
                && !app.command_list.refreshing
        };
        match target {
            Some(Target::Slack { session, view })
                if idle(self, CommandListAddon::Slack, session)
                    && self.slack.view.as_ref() == Some(&view) =>
            {
                let generation = self.command_list.begin_refresh();
                self.reload_slack_snapshot(generation)
            }
            Some(Target::BrowserHistory { session, group })
                if idle(self, CommandListAddon::BrowserHistory, session)
                    && self.browser_history_group_is(session, &group) =>
            {
                self.reload_browser_history_snapshot()
            }
            Some(Target::ScriptFilter {
                index,
                query,
                generation,
            }) if self.script_filter.config_index == Some(index)
                && self.script_filter.query == query
                && self.script_filter.generation == generation =>
            {
                let mut tasks = Vec::new();
                self.handle_script_filter_query(index, query, false, &mut tasks);
                Task::batch(tasks)
            }
            _ => Task::none(),
        }
    }

    /// Show the rows the service just cached for the Slack view.
    fn reload_slack_snapshot(&mut self, generation: u64) -> Task<Message> {
        super::slack_api::reload_directory();
        let config = &self.addons.slack;
        let rows = match self.slack.view {
            Some(View::Home) => super::slack::cached_home(config),
            Some(View::Unread) => super::slack::cached_unread(config),
            Some(View::Threads) => super::slack::cached_threads(config),
            Some(View::Activity) => super::slack::cached_activity(config),
            _ => None,
        };
        let Some(rows) = rows else {
            // Nothing cached for this account: fetch it here instead.
            self.command_list.refreshing = false;
            return self.load_slack(false);
        };
        // F5 stands in for Ctrl+R here, so the preview is fetched again too.
        self.mark_slack_previews_stale();
        let loaded = self.handle_command_list_loaded(generation, Ok(rows));
        Task::batch([loaded, self.ensure_slack_preview()])
    }

    /// Read the active browser view from the snapshot the service wrote.
    fn reload_browser_history_snapshot(&mut self) -> Task<Message> {
        let Some(group) = self.browser_history.active_group().cloned() else {
            return Task::none();
        };
        let session = self.command_list.session;
        let key = group.key.clone();
        let config = self.addons.browser_history.clone();
        Task::perform(
            off_thread(move || {
                Arc::new(super::browser_history::load_group(&config, &group, false))
            }),
            move |library| Message::ServerHistoryReloaded(session, key, library),
        )
    }

    /// Whether the browser history list of `session` still shows `group`.
    pub(super) fn browser_history_group_is(&self, session: u64, group: &str) -> bool {
        self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && self.command_list.session == session
            && self.browser_history.active_group().map(|g| g.key.as_str()) == Some(group)
    }

    pub(super) fn handle_server_history_reloaded(
        &mut self,
        session: u64,
        key: String,
        library: Option<Arc<super::browser_history::Library>>,
    ) -> Task<Message> {
        if !self.browser_history_group_is(session, &key) {
            return Task::none();
        }
        self.apply_browser_history_library(key, library)
    }
}
