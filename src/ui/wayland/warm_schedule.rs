//! When the server refreshes script filter results by itself.
//!
//! Every list a filter can produce (one per warmed query and per view the
//! script offers) is a target with its own state. A target is due once its
//! snapshot has outlived the script's own idea of how long it stays fresh,
//! capped by the user's config, and a failed refresh is retried with a
//! backoff while the last good snapshot keeps being served.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::Duration;

use super::cache::{
    effective_ttl, parse_list, script_filter_list_key, SnapshotStamp, SnapshotUpdate, MAX_STALE,
};
use super::script_filters::view_config;
use super::state::ScriptFilterResult;
use super::types::ScriptFilterView;
use crate::ScriptFilterConfig;

/// Views of one query's default output that are warmed, besides the default.
pub(super) const MAX_WARM_VIEWS: usize = 16;
/// Queries a script may add to what the config warms, in all.
pub(super) const MAX_WARM_ADVERTISED: usize = 4;
/// Longest query a script may ask to have warmed, in characters.
pub(super) const MAX_WARM_QUERY_CHARS: usize = 64;
/// First retry after a failed refresh; each further failure doubles it.
const RETRY_MIN_SECS: u64 = 60;
const RETRY_MAX_SECS: u64 = 60 * 60;

/// How often a filter's targets may be refreshed.
#[derive(Debug, Clone, Copy)]
pub(super) struct Policy {
    /// The config's `cache_seconds`.
    pub ttl: Duration,
    /// The longest freshness a script may ask for.
    pub ceiling: Duration,
    /// Fewest seconds between two successful runs of one target.
    pub floor: u64,
}

impl Policy {
    pub fn of(config: &ScriptFilterConfig) -> Option<Self> {
        let ttl = config.cache_ttl()?;
        Some(Self {
            ttl,
            ceiling: config.cache_ceiling().unwrap_or(ttl),
            floor: config.warm_interval()?.as_secs(),
        })
    }

    /// Seconds a snapshot is left alone: its own freshness, never less than
    /// the floor, so a script can only make the server refresh less often.
    pub fn interval(&self, hint_secs: Option<u64>) -> u64 {
        effective_ttl(hint_secs, self.ttl, self.ceiling)
            .as_secs()
            .max(self.floor)
    }
}

/// What the scheduler remembers about one target between ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TargetState {
    last_attempt: u64,
    failures: u32,
    /// When a failed target may be tried again.
    retry_at: u64,
}

/// Seconds to wait after the `failures`-th failure in a row.
pub(super) fn retry_delay(failures: u32) -> u64 {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_MIN_SECS
        .saturating_mul(1 << doublings)
        .min(RETRY_MAX_SECS)
}

/// A stable offset of up to a tenth of `interval`, so that targets created
/// together do not all come due together. It only ever delays a run.
pub(super) fn jitter(key: &str, interval: u64) -> u64 {
    let spread = interval / 10;
    if spread == 0 {
        return 0;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish() % spread
}

/// Whether a target is due. `fetched_at` is when its snapshot was written,
/// if there is one.
pub(super) fn is_due(
    now: u64,
    fetched_at: Option<u64>,
    interval: u64,
    floor: u64,
    jitter: u64,
    state: &TargetState,
) -> bool {
    if state.failures > 0 {
        return now >= state.retry_at;
    }
    let wait = interval.saturating_add(jitter);
    if fetched_at.is_some_and(|fetched| now.saturating_sub(fetched) < wait) {
        return false;
    }
    // A list nobody has fetched or tried yet is due at once; otherwise the
    // last write or attempt must be at least `floor` ago.
    let recent = fetched_at.unwrap_or(0).max(state.last_attempt);
    recent == 0 || now.saturating_sub(recent) >= floor
}

/// Asks, once a run holds a list, whether it is still due.
pub(super) type StillDue<'a> = &'a dyn Fn() -> bool;

/// What running a target came to.
pub(super) enum Run {
    /// Nothing ran: another run holds the list, or it was no longer due once
    /// the run had it.
    Skipped,
    Done(Result<ScriptFilterResult, String>),
}

/// Outcome of one pass over a filter.
#[derive(Debug, Default)]
pub(super) struct Report {
    pub ran: usize,
    pub error: Option<String>,
}

/// The views a default list names, and which one it is itself.
#[derive(Clone, Default)]
struct Offered {
    default: Option<String>,
    views: Vec<ScriptFilterView>,
    warm: Vec<String>,
}

impl From<ScriptFilterResult> for Offered {
    fn from(result: ScriptFilterResult) -> Self {
        Self {
            default: result
                .view
                .or_else(|| result.views.first().map(|view| view.id.clone())),
            views: result.views,
            warm: result.cache_warm,
        }
    }
}

struct SnapshotSummary {
    fetched_at: u64,
    hint_secs: Option<u64>,
    offered: Option<Offered>,
}

#[derive(Default)]
struct Target {
    state: TargetState,
    stamp: Option<SnapshotStamp>,
    snapshot: Option<SnapshotSummary>,
}

impl Target {
    fn snapshot(&self, now: u64) -> Option<&SnapshotSummary> {
        self.snapshot
            .as_ref()
            .filter(|snapshot| now.saturating_sub(snapshot.fetched_at) < MAX_STALE.as_secs())
    }

    fn is_due(&mut self, pass: &Pass, key: &str, default_list: bool) -> bool {
        let now = (pass.clock)();
        match (pass.read)(key, self.stamp) {
            SnapshotUpdate::Unchanged => {}
            SnapshotUpdate::Missing => {
                self.stamp = None;
                self.snapshot = None;
            }
            SnapshotUpdate::Changed(stamp, snapshot) => {
                self.stamp = Some(stamp);
                self.snapshot = snapshot.map(|snapshot| SnapshotSummary {
                    fetched_at: snapshot.fetched_at,
                    hint_secs: snapshot.hint_secs,
                    offered: if default_list {
                        parse_list(&snapshot, pass.config.icon.clone()).map(Offered::from)
                    } else {
                        None
                    },
                });
                // An F5 write can land while a failed attempt is still running
                if self.snapshot(now).is_some() {
                    self.state.failures = 0;
                }
            }
        }
        let snapshot = self.snapshot(now);
        let interval = pass
            .policy
            .interval(snapshot.and_then(|list| list.hint_secs));
        is_due(
            now,
            snapshot.map(|list| list.fetched_at),
            interval,
            pass.policy.floor,
            jitter(key, interval),
            &self.state,
        )
    }
}

/// The queries a script asked to have warmed that the config does not
/// already warm: bounded, whatever the script prints.
pub(super) fn advertised_queries<'a>(
    config: &ScriptFilterConfig,
    asked: impl IntoIterator<Item = &'a String>,
) -> Vec<String> {
    let mut chosen: Vec<String> = Vec::new();
    for query in asked {
        let allowed = query.chars().count() <= MAX_WARM_QUERY_CHARS
            && !query.chars().any(char::is_control)
            && !config.warm_queries.contains(query)
            && !chosen.contains(query);
        if allowed && chosen.len() < MAX_WARM_ADVERTISED {
            chosen.push(query.clone());
        }
    }
    chosen
}

/// What one pass over a filter reads, shared by its targets.
struct Pass<'a> {
    clock: &'a dyn Fn() -> u64,
    config: &'a ScriptFilterConfig,
    policy: &'a Policy,
    read: &'a dyn Fn(&str, Option<SnapshotStamp>) -> SnapshotUpdate,
}

/// The scheduling state of one script filter's lists.
#[derive(Default)]
pub(super) struct Warmer {
    targets: HashMap<String, Target>,
    /// Lists seen during the pass under way. Anything not seen has been
    /// dropped by the script, and its state goes with it.
    seen: HashSet<String>,
}

impl Warmer {
    /// Run what is due for `config`: each warmed query, then the views its
    /// default output offers, then the few queries those outputs asked to
    /// have warmed. `read` gives the snapshot of a list key.
    pub fn tick(
        &mut self,
        clock: &dyn Fn() -> u64,
        config: &ScriptFilterConfig,
        policy: &Policy,
        read: &dyn Fn(&str, Option<SnapshotStamp>) -> SnapshotUpdate,
        run: &mut dyn FnMut(Option<&ScriptFilterView>, &str, StillDue) -> Run,
    ) -> Report {
        let pass = Pass {
            clock,
            config,
            policy,
            read,
        };
        self.seen.clear();
        let mut report = Report::default();
        let mut asked = Vec::new();
        for query in &config.warm_queries {
            asked.extend(self.warm_query(&pass, run, query, &mut report));
        }
        for query in advertised_queries(config, &asked) {
            self.warm_query(&pass, run, &query, &mut report);
        }
        // A script that keeps changing what it asks for must not make the
        // server remember every name it ever used.
        self.targets.retain(|key, _| self.seen.contains(key));
        report
    }

    /// Warm one query: its default list, then the views that list offers.
    /// Returns the queries the default list asked to have warmed.
    fn warm_query(
        &mut self,
        pass: &Pass,
        run: &mut dyn FnMut(Option<&ScriptFilterView>, &str, StillDue) -> Run,
        query: &str,
        report: &mut Report,
    ) -> Vec<String> {
        let config = pass.config;
        let default_key = script_filter_list_key(&view_config(config, None), query);
        let ran = self.visit(
            pass,
            &default_key,
            &mut |view, still_due| run(view, query, still_due),
            None,
        );
        let fresh = match ran {
            Some(Ok(result)) => {
                report.ran += 1;
                Some(Offered::from(result))
            }
            Some(Err(message)) => {
                report.ran += 1;
                report
                    .error
                    .get_or_insert(format!("{}: {message}", config.name));
                None
            }
            None => None,
        };
        let Offered {
            default,
            views,
            warm,
        } = fresh
            .or_else(|| {
                self.targets[&default_key]
                    .snapshot((pass.clock)())?
                    .offered
                    .clone()
            })
            .unwrap_or_default();
        for view in views
            .iter()
            .filter(|view| Some(&view.id) != default.as_ref())
            .take(MAX_WARM_VIEWS)
        {
            let key = script_filter_list_key(&view_config(config, Some(view)), query);
            let outcome = self.visit(
                pass,
                &key,
                &mut |view, still_due| run(view, query, still_due),
                Some(view),
            );
            if let Some(outcome) = outcome {
                report.ran += 1;
                if let Err(message) = outcome {
                    report
                        .error
                        .get_or_insert(format!("{} ({}): {message}", config.name, view.title));
                }
            }
        }
        warm
    }

    /// Run one target if it is due. `None` when it was not, or was skipped.
    /// `run` gets a check to repeat once it holds the list, because the
    /// snapshot may have been refreshed since this one was read.
    fn visit(
        &mut self,
        pass: &Pass,
        key: &str,
        run: &mut dyn FnMut(Option<&ScriptFilterView>, StillDue) -> Run,
        view: Option<&ScriptFilterView>,
    ) -> Option<Result<ScriptFilterResult, String>> {
        self.seen.insert(key.to_string());
        let target = self.targets.entry(key.to_string()).or_default();
        let default_list = view.is_none();
        if !target.is_due(pass, key, default_list) {
            return None;
        }
        let tracked = RefCell::new(target);
        let still_due = || tracked.borrow_mut().is_due(pass, key, default_list);
        let Run::Done(outcome) = run(view, &still_due) else {
            return None;
        };
        let state = &mut tracked.into_inner().state;
        // Timed when the run ended, not when the pass began: an earlier slow
        // target must not eat into this one's backoff.
        let ended = (pass.clock)();
        state.last_attempt = ended;
        match &outcome {
            Ok(_) => state.failures = 0,
            Err(_) => {
                state.failures = state.failures.saturating_add(1);
                state.retry_at = ended.saturating_add(retry_delay(state.failures));
            }
        }
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::Snapshot;
    use super::*;
    use std::cell::Cell;

    const NOW: u64 = 1_000_000;

    fn snapshot(fetched_at: u64, hint: Option<u64>, payload: &str) -> Snapshot {
        Snapshot {
            version: 1,
            fetched_at,
            ttl_secs: 300,
            payload: payload.to_string(),
            hint_secs: hint,
        }
    }

    fn policy(ttl: u64, max: u64, floor: u64) -> Policy {
        Policy {
            ttl: Duration::from_secs(ttl),
            ceiling: Duration::from_secs(ttl.max(max)),
            floor,
        }
    }

    fn config(queries: &[&str]) -> ScriptFilterConfig {
        let mut config: ScriptFilterConfig =
            serde_yaml::from_str("name: repos\ncommand: r\nkeyword: r\ncache_seconds: 300\n")
                .unwrap();
        config.warm_queries = queries.iter().map(|query| query.to_string()).collect();
        config
    }

    fn view(id: &str) -> ScriptFilterView {
        serde_json::from_str(&format!(r#"{{"id":"{id}","title":"{id}"}}"#)).unwrap()
    }

    fn views_payload(default: &str, others: &[&str]) -> String {
        let views: Vec<_> = std::iter::once(&default)
            .chain(others)
            .map(|id| format!(r#"{{"id":"{id}","title":"{id}"}}"#))
            .collect();
        format!(
            r#"{{"view":"{default}","views":[{}],"items":[]}}"#,
            views.join(",")
        )
    }

    fn result(payload: &str) -> ScriptFilterResult {
        super::super::script_filters::parse_script_filter_output(payload, None).unwrap()
    }

    fn key(config: &ScriptFilterConfig, view: Option<&ScriptFilterView>, query: &str) -> String {
        script_filter_list_key(&view_config(config, view), query)
    }

    #[test]
    fn retry_delay_doubles_and_caps() {
        let delays: Vec<_> = (0..=8).map(retry_delay).collect();
        assert_eq!(delays, [60, 60, 120, 240, 480, 960, 1920, 3600, 3600]);
        assert_eq!(retry_delay(u32::MAX), 3600);
    }

    #[test]
    fn jitter_only_delays_and_stays_small() {
        for interval in [0, 1, 9, 10, 29, 30, 300, 86_400, u64::MAX] {
            for name in ["a", "list:b", "c"] {
                assert!(jitter(name, interval) <= interval / 10, "{interval}");
            }
        }
        assert_eq!(jitter("anything", 9), 0);
        assert_eq!(jitter("same", 86_400), jitter("same", 86_400));
    }

    #[test]
    fn jitter_spreads_targets_out() {
        let spread: std::collections::HashSet<_> = (0..20)
            .map(|index| jitter(&format!("list-{index}"), 86_400))
            .collect();
        assert!(spread.len() > 10, "{spread:?}");
        assert!(spread.iter().all(|offset| *offset < 8_640));
    }

    #[test]
    fn the_interval_follows_the_hint_but_never_drops_below_the_floor() {
        let p = policy(300, 86_400, 300);
        assert_eq!(p.interval(None), 300);
        assert_eq!(p.interval(Some(86_400)), 86_400);
        assert_eq!(p.interval(Some(60)), 300);
        // Past the user's ceiling and the staleness limit it is capped.
        assert_eq!(policy(300, 3_600, 300).interval(Some(86_400)), 3_600);
        assert_eq!(p.interval(Some(u64::MAX)), 86_400);
        let week = MAX_STALE.as_secs();
        assert_eq!(policy(300, u64::MAX, 300).interval(Some(u64::MAX)), week);
        // An explicit warm interval is a floor the script cannot undercut.
        assert_eq!(policy(300, 86_400, 3_600).interval(Some(600)), 3_600);
    }

    #[test]
    fn policy_needs_caching_to_be_on() {
        assert!(Policy::of(&config(&[""])).is_some());
        let off: ScriptFilterConfig =
            serde_yaml::from_str("name: x\ncommand: y\nkeyword: z\nwarm_interval_seconds: 60\n")
                .unwrap();
        assert!(Policy::of(&off).is_none());
    }

    fn due(fetched: Option<u64>, interval: u64, floor: u64, state: &TargetState) -> bool {
        is_due(NOW, fetched, interval, floor, 0, state)
    }

    #[test]
    fn a_target_waits_for_its_interval() {
        let idle = TargetState::default();
        assert!(due(None, 300, 300, &idle), "no snapshot: run now");
        assert!(!due(Some(NOW - 299), 300, 300, &idle));
        assert!(due(Some(NOW - 300), 300, 300, &idle));
        assert!(!due(Some(NOW - 3_600), 86_400, 300, &idle));
        assert!(due(Some(NOW - 86_400), 86_400, 300, &idle));
    }

    #[test]
    fn jitter_never_brings_a_run_forward() {
        let idle = TargetState::default();
        assert!(!is_due(NOW, Some(NOW - 300), 300, 300, 29, &idle));
        assert!(is_due(NOW, Some(NOW - 329), 300, 300, 29, &idle));
    }

    #[test]
    fn successful_runs_are_never_closer_than_the_floor() {
        // The script asked for 60 s but the floor is an hour: a recent attempt
        // counts even when the snapshot is old.
        let recent = TargetState {
            last_attempt: NOW - 100,
            ..Default::default()
        };
        assert!(!due(Some(NOW - 7_200), 3_600, 3_600, &recent));
        assert!(due(
            Some(NOW - 7_200),
            3_600,
            3_600,
            &TargetState::default()
        ));
    }

    #[test]
    fn a_failed_target_retries_despite_a_long_floor() {
        let failed = TargetState {
            last_attempt: NOW - 10,
            failures: 1,
            retry_at: NOW + 50,
        };
        assert!(!due(Some(NOW - 100_000), 86_400, 86_400, &failed));
        let later = |secs| is_due(NOW + secs, Some(NOW - 100_000), 86_400, 86_400, 0, &failed);
        assert!(!later(49));
        assert!(later(50));
    }

    #[test]
    fn a_target_nobody_has_fetched_or_tried_is_due_at_once() {
        let idle = TargetState::default();
        assert!(is_due(100, None, 300, 300, 0, &idle));
        assert!(is_due(100, None, 300, u64::MAX, 0, &idle));
    }

    #[test]
    fn the_interval_holds_at_the_edge_of_the_clock() {
        let idle = TargetState::default();
        assert!(!is_due(u64::MAX, Some(u64::MAX - 5), 10, 1, 0, &idle));
        assert!(is_due(u64::MAX, Some(u64::MAX - 10), 10, 1, 0, &idle));
        assert!(!is_due(50, Some(u64::MAX), 10, 1, 0, &idle));
    }

    #[test]
    fn tiny_intervals_neither_panic_nor_underflow() {
        let idle = TargetState::default();
        for interval in [0, 1, 9, 10, 29, 30] {
            for floor in [0, 1, 30] {
                let jitter = jitter("k", interval);
                let _ = is_due(NOW, Some(NOW - 1), interval, floor, jitter, &idle);
                let _ = is_due(0, Some(u64::MAX), interval, floor, jitter, &idle);
                let _ = is_due(
                    u64::MAX,
                    Some(u64::MAX),
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                    &idle,
                );
            }
        }
    }

    type Script = Box<dyn FnMut(Option<&str>, &str) -> Run>;
    type Snapshots = RefCell<HashMap<String, (SnapshotStamp, Snapshot)>>;

    fn put_snapshot(snapshots: &Snapshots, key: &str, snapshot: Snapshot) {
        let mut snapshots = snapshots.borrow_mut();
        let revision = snapshots.get(key).map_or(1, |(stamp, _)| stamp.1 + 1);
        snapshots.insert(key.into(), ((0, revision, 0, 0), snapshot));
    }

    struct Fixture {
        config: ScriptFilterConfig,
        policy: Policy,
        clock: std::rc::Rc<Cell<u64>>,
        snapshots: std::rc::Rc<Snapshots>,
        runs: RefCell<Vec<(Option<String>, String)>>,
        script: RefCell<Script>,
        /// Runs between the decision to refresh a list and the run holding it.
        before_hold: RefCell<Option<Box<dyn FnMut()>>>,
    }

    impl Fixture {
        fn new(queries: &[&str]) -> Self {
            Self {
                config: config(queries),
                policy: policy(300, 86_400, 300),
                clock: std::rc::Rc::new(Cell::new(NOW)),
                snapshots: std::rc::Rc::default(),
                runs: RefCell::default(),
                script: RefCell::new(Box::new(|_, _| Run::Done(Ok(result(r#"{"items":[]}"#))))),
                before_hold: RefCell::new(None),
            }
        }

        fn store(&self, view: Option<&str>, query: &str, snapshot: Snapshot) {
            let view = view.map(view_of);
            let key = key(&self.config, view.as_ref(), query);
            put_snapshot(&self.snapshots, &key, snapshot);
        }

        fn tick(&self, warmer: &mut Warmer, now: u64) -> Report {
            self.clock.set(now);
            let read = |key: &str, known| match self.snapshots.borrow().get(key) {
                Some((stamp, _)) if Some(*stamp) == known => SnapshotUpdate::Unchanged,
                Some((stamp, snapshot)) => SnapshotUpdate::Changed(*stamp, Some(snapshot.clone())),
                None => SnapshotUpdate::Missing,
            };
            let clock = || self.clock.get();
            warmer.tick(
                &clock,
                &self.config,
                &self.policy,
                &read,
                &mut |view, query, still_due| {
                    if let Some(hook) = self.before_hold.borrow_mut().as_mut() {
                        hook();
                    }
                    if !still_due() {
                        return Run::Skipped;
                    }
                    self.runs
                        .borrow_mut()
                        .push((view.map(|view| view.id.clone()), query.to_string()));
                    (self.script.borrow_mut())(view.map(|view| view.id.as_str()), query)
                },
            )
        }

        fn ran(&self) -> Vec<(Option<String>, String)> {
            self.runs.borrow_mut().drain(..).collect()
        }
    }

    fn view_of(id: &str) -> ScriptFilterView {
        view(id)
    }

    fn ok(payload: &str) -> Run {
        Run::Done(Ok(result(payload)))
    }

    #[test]
    fn startup_schedules_each_target_from_its_own_snapshot() {
        let fixture = Fixture::new(&["", "late"]);
        // The first query is fresh, the second has no snapshot at all.
        fixture.store(None, "", snapshot(NOW - 10, None, r#"{"items":[]}"#));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        assert_eq!(fixture.ran(), [(None, "late".to_string())]);
    }

    #[test]
    fn a_restart_does_not_rewarm_fresh_snapshots() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 10, Some(86_400), r#"{"items":[]}"#),
        );
        fixture.tick(&mut Warmer::default(), NOW);
        assert!(fixture.ran().is_empty());
        // A day later it is due again.
        fixture.tick(&mut Warmer::default(), NOW + 100_000);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn targets_are_skipped_while_their_snapshot_is_recent() {
        let fixture = Fixture::new(&[""]);
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        assert_eq!(fixture.ran().len(), 1);
        fixture.store(None, "", snapshot(NOW, None, r#"{"items":[]}"#));
        for later in [30, 60, 299] {
            fixture.tick(&mut warmer, NOW + later);
        }
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, NOW + 400);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn views_of_a_fresh_default_are_warmed_from_its_snapshot() {
        let fixture = Fixture::new(&[""]);
        let payload = views_payload("mine", &["review", "assigned"]);
        // The default is fresh for a day, the views have never been warmed.
        fixture.store(None, "", snapshot(NOW - 10, Some(86_400), &payload));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(view, _)| view).collect();
        assert_eq!(ran, [Some("review".into()), Some("assigned".into())]);
    }

    #[test]
    fn a_short_lived_view_is_not_hidden_by_a_long_lived_default() {
        let fixture = Fixture::new(&[""]);
        let payload = views_payload("mine", &["review"]);
        fixture.store(None, "", snapshot(NOW - 10, Some(86_400), &payload));
        fixture.store(
            Some("review"),
            "",
            snapshot(NOW - 1_000, None, r#"{"items":[]}"#),
        );
        fixture.tick(&mut Warmer::default(), NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(view, _)| view).collect();
        assert_eq!(ran, [Some("review".into())]);
    }

    #[test]
    fn views_are_remembered_for_as_long_as_the_snapshot_is_the_same() {
        let fixture = Fixture::new(&[""]);
        let payload = views_payload("mine", &["review"]);
        fixture.store(None, "", snapshot(NOW - 10, Some(86_400), &payload));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        fixture.ran();
        let key = key(&fixture.config, None, "");
        let summary = warmer.targets[&key].snapshot(NOW).unwrap();
        assert_eq!(summary.offered.as_ref().unwrap().views.len(), 2);
        // The same snapshot answers from the memo, and the view is due again.
        fixture.tick(&mut warmer, NOW + 400);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(view, _)| view).collect();
        assert_eq!(ran, [Some("review".into())]);
        // A rewritten payload is read afresh, even with the same timestamp.
        let payload = views_payload("mine", &["other"]);
        fixture.store(None, "", snapshot(NOW - 10, Some(86_400), &payload));
        fixture.tick(&mut warmer, NOW + 800);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(view, _)| view).collect();
        assert_eq!(ran, [Some("other".into())]);
    }

    #[test]
    fn warming_caps_the_views_a_script_advertises() {
        let fixture = Fixture::new(&[""]);
        let names: Vec<String> = (0..100).map(|index| format!("v{index}")).collect();
        let others: Vec<&str> = names.iter().map(String::as_str).collect();
        let payload = views_payload("mine", &others);
        fixture.store(None, "", snapshot(NOW - 10, Some(86_400), &payload));
        fixture.tick(&mut Warmer::default(), NOW);
        assert_eq!(fixture.ran().len(), 16);
    }

    #[test]
    fn a_failing_target_backs_off_and_keeps_its_snapshot() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        let mut warmer = Warmer::default();
        let report = fixture.tick(&mut warmer, NOW);
        assert_eq!(report.error.as_deref(), Some("repos: offline"));
        assert_eq!(fixture.ran().len(), 1);
        // Retried after 60 s, then 120 s, even though the interval is a day.
        fixture.tick(&mut warmer, NOW + 59);
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, NOW + 60);
        assert_eq!(fixture.ran().len(), 1);
        fixture.tick(&mut warmer, NOW + 60 + 119);
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, NOW + 60 + 120);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn a_success_ends_the_backoff() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        *fixture.script.borrow_mut() = Box::new(|_, _| ok(r#"{"items":[]}"#));
        fixture.tick(&mut warmer, NOW + 60);
        // The run wrote a snapshot; the next try waits for its interval.
        fixture.store(
            None,
            "",
            snapshot(NOW + 60, Some(86_400), r#"{"items":[]}"#),
        );
        fixture.ran();
        fixture.tick(&mut warmer, NOW + 3_600);
        assert!(fixture.ran().is_empty());
    }

    #[test]
    fn a_busy_list_is_left_alone_and_not_counted_as_a_failure() {
        let fixture = Fixture::new(&[""]);
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Skipped);
        let mut warmer = Warmer::default();
        let report = fixture.tick(&mut warmer, NOW);
        assert_eq!((report.ran, report.error), (0, None));
        // Due again on the very next tick, with no backoff.
        fixture.ran();
        fixture.tick(&mut warmer, NOW + 30);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn repeated_failures_never_wait_longer_than_an_hour() {
        let fixture = Fixture::new(&[""]);
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        let mut warmer = Warmer::default();
        let mut now = NOW;
        for _ in 0..20 {
            fixture.tick(&mut warmer, now);
            now += 3_600;
        }
        assert_eq!(
            fixture.ran().len(),
            20,
            "one attempt per hour at most waits"
        );
    }

    #[test]
    fn views_of_a_failed_default_are_still_scheduled_from_its_snapshot() {
        let fixture = Fixture::new(&[""]);
        let payload = views_payload("mine", &["review"]);
        fixture.store(None, "", snapshot(NOW - 100_000, Some(86_400), &payload));
        *fixture.script.borrow_mut() = Box::new(|view, _| match view {
            None => Run::Done(Err("offline".into())),
            Some(_) => ok(r#"{"items":[]}"#),
        });
        fixture.tick(&mut Warmer::default(), NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(view, _)| view).collect();
        assert_eq!(ran, [None, Some("review".into())]);
    }

    #[test]
    fn an_unstored_output_counts_as_a_failure() {
        // `server_run` turns it into an error before it gets here.
        let fixture = Fixture::new(&[""]);
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("GitHub error".into())));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        fixture.ran();
        fixture.tick(&mut warmer, NOW + 30);
        assert!(fixture.ran().is_empty(), "backing off");
    }

    #[test]
    fn a_list_refreshed_while_the_run_took_hold_is_left_alone() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        // F5 writes a fresh snapshot between the decision and the hold.
        let snapshots = fixture.snapshots.clone();
        let key = key(&fixture.config, None, "");
        *fixture.before_hold.borrow_mut() = Some(Box::new(move || {
            let fresh = snapshot(NOW, Some(86_400), r#"{"items":[]}"#);
            put_snapshot(&snapshots, &key, fresh);
        }));
        let mut warmer = Warmer::default();
        let report = fixture.tick(&mut warmer, NOW);
        assert!(fixture.ran().is_empty(), "the refreshed list was run again");
        assert_eq!(report.ran, 0);
    }

    #[test]
    fn a_backoff_counts_from_the_end_of_the_run_not_the_start_of_the_pass() {
        let fixture = Fixture::new(&["slow", "fails"]);
        fixture.store(
            None,
            "slow",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        fixture.store(
            None,
            "fails",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        let clock = fixture.clock.clone();
        *fixture.script.borrow_mut() = Box::new(move |_, query| {
            if query == "slow" {
                clock.set(clock.get() + 50);
                ok(r#"{"items":[]}"#)
            } else {
                Run::Done(Err("offline".into()))
            }
        });
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        assert_eq!(fixture.ran().len(), 2);
        // The failure happened at NOW + 50, so 60 s later is NOW + 110.
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        fixture.tick(&mut warmer, NOW + 109);
        assert!(fixture.ran().iter().all(|(_, query)| query != "fails"));
        fixture.tick(&mut warmer, NOW + 110);
        assert!(fixture.ran().iter().any(|(_, query)| query == "fails"));
    }

    #[test]
    fn a_refresh_by_someone_else_starts_a_new_failure_sequence() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        let mut warmer = Warmer::default();
        let mut now = NOW;
        for _ in 0..7 {
            fixture.tick(&mut warmer, now);
            now += 3_600;
        }
        fixture.ran();
        // F5 succeeds, and a day later the list is due again.
        fixture.store(None, "", snapshot(now, Some(86_400), r#"{"items":[]}"#));
        fixture.tick(&mut warmer, now + 10);
        now += 100_000;
        fixture.tick(&mut warmer, now);
        assert_eq!(fixture.ran().len(), 1);
        // One failure again: the retry comes after a minute, not an hour.
        fixture.tick(&mut warmer, now + 59);
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, now + 60);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn a_backoff_counts_from_when_the_failed_run_ended() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        let clock = fixture.clock.clone();
        *fixture.script.borrow_mut() = Box::new(move |_, _| {
            clock.set(clock.get() + 40);
            Run::Done(Err("timed out".into()))
        });
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        fixture.ran();
        // Failed at NOW + 40: a minute later is NOW + 100, not NOW + 60.
        fixture.tick(&mut warmer, NOW + 99);
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, NOW + 100);
        assert_eq!(fixture.ran().len(), 1);
    }

    #[test]
    fn a_refresh_written_during_a_failed_run_overrides_backoff() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 100_000, Some(86_400), r#"{"items":[]}"#),
        );
        let clock = fixture.clock.clone();
        let snapshots = fixture.snapshots.clone();
        let key = key(&fixture.config, None, "");
        let target = key.clone();
        *fixture.script.borrow_mut() = Box::new(move |_, _| {
            let fresh = snapshot(NOW + 10, Some(86_400), r#"{"items":[]}"#);
            put_snapshot(&snapshots, &key, fresh);
            clock.set(NOW + 40);
            Run::Done(Err("timed out".into()))
        });
        let mut warmer = Warmer::default();
        assert!(fixture.tick(&mut warmer, NOW).error.is_some());
        assert_eq!(fixture.ran().len(), 1);
        assert_eq!(warmer.targets[&target].state.failures, 1);
        fixture.tick(&mut warmer, NOW + 100);
        assert!(fixture.ran().is_empty());
        assert_eq!(warmer.targets[&target].state.failures, 0);
    }

    #[test]
    fn a_refresh_seen_after_taking_the_list_still_clears_the_failures() {
        let mut fixture = Fixture::new(&[""]);
        fixture.policy = policy(1, 1, 1);
        fixture.store(None, "", snapshot(NOW - 100, None, r#"{"items":[]}"#));
        *fixture.script.borrow_mut() = Box::new(|_, _| Run::Done(Err("offline".into())));
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        fixture.tick(&mut warmer, NOW + 60);
        fixture.tick(&mut warmer, NOW + 60 + 120);
        fixture.ran();
        // The retry is due; F5 writes a snapshot just before the run holds the
        // list, and the clock moves on a second, so that snapshot is due again.
        let retry = NOW + 180 + 240;
        let snapshots = fixture.snapshots.clone();
        let clock = fixture.clock.clone();
        let key = key(&fixture.config, None, "");
        *fixture.before_hold.borrow_mut() = Some(Box::new(move || {
            let fresh = snapshot(retry, None, r#"{"items":[]}"#);
            put_snapshot(&snapshots, &key, fresh);
            clock.set(retry + 1);
        }));
        fixture.tick(&mut warmer, retry);
        *fixture.before_hold.borrow_mut() = None;
        assert_eq!(fixture.ran().len(), 1, "the refreshed list ran again");
        // A new failure sequence: back after a minute, not four.
        fixture.tick(&mut warmer, retry + 1 + 59);
        assert!(fixture.ran().is_empty());
        fixture.tick(&mut warmer, retry + 1 + 60);
        assert_eq!(fixture.ran().len(), 1);
    }

    fn asked(config: &ScriptFilterConfig, queries: &[&str]) -> Vec<String> {
        let queries: Vec<String> = queries.iter().map(|query| query.to_string()).collect();
        advertised_queries(config, &queries)
    }

    #[test]
    fn advertised_queries_are_bounded_and_deduplicated() {
        let config = config(&[""]);
        // Already warmed by the config, repeated, too long, or not text.
        let long = "x".repeat(65);
        let fine = "y".repeat(64);
        assert_eq!(
            asked(
                &config,
                &["", "review", "review", &long, "bad\nquery", &fine]
            ),
            ["review".to_string(), fine.clone()]
        );
        let many: Vec<String> = (0..20).map(|index| format!("q{index}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(asked(&config, &refs), ["q0", "q1", "q2", "q3"]);
        assert!(asked(&config, &[]).is_empty());
    }

    fn warm_payload(warm: &[&str]) -> String {
        let warm: Vec<String> = warm.iter().map(|query| format!("{query:?}")).collect();
        format!(
            r#"{{"cache":{{"seconds":86400,"warm":[{}]}},"items":[]}}"#,
            warm.join(",")
        )
    }

    #[test]
    fn queries_a_default_list_asks_for_are_warmed_too() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(
                NOW - 10,
                Some(86_400),
                &warm_payload(&["review", "assigned"]),
            ),
        );
        fixture.tick(&mut Warmer::default(), NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(_, query)| query).collect();
        assert_eq!(ran, ["review", "assigned"]);
    }

    #[test]
    fn advertised_queries_do_not_advertise_more() {
        let fixture = Fixture::new(&[""]);
        // Everything is due, and each query's own output asks for another.
        *fixture.script.borrow_mut() = Box::new(|_, query| match query {
            "" => ok(&warm_payload(&["review"])),
            "review" => ok(&warm_payload(&["deeper"])),
            _ => ok(r#"{"items":[]}"#),
        });
        fixture.tick(&mut Warmer::default(), NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(_, query)| query).collect();
        assert_eq!(ran, ["", "review"]);
    }

    #[test]
    fn advertised_queries_come_from_a_run_made_in_the_same_pass() {
        let fixture = Fixture::new(&[""]);
        *fixture.script.borrow_mut() = Box::new(|_, query| {
            if query.is_empty() {
                ok(&warm_payload(&["review"]))
            } else {
                ok(r#"{"items":[]}"#)
            }
        });
        fixture.tick(&mut Warmer::default(), NOW);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(_, query)| query).collect();
        assert_eq!(ran, ["", "review"]);
    }

    #[test]
    fn views_of_an_advertised_query_are_warmed_with_it() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 10, Some(86_400), &warm_payload(&["review"])),
        );
        fixture.store(
            None,
            "review",
            snapshot(NOW - 10, Some(86_400), &views_payload("mine", &["theirs"])),
        );
        fixture.tick(&mut Warmer::default(), NOW);
        assert_eq!(
            fixture.ran(),
            [(Some("theirs".to_string()), "review".to_string())]
        );
    }

    #[test]
    fn a_script_rotating_what_it_asks_for_cannot_grow_the_server() {
        let fixture = Fixture::new(&[""]);
        let pass = std::rc::Rc::new(Cell::new(0));
        let counter = pass.clone();
        *fixture.script.borrow_mut() = Box::new(move |_, query| {
            if query.is_empty() {
                let round = counter.get();
                counter.set(round + 1);
                let names: Vec<String> = (0..4).map(|n| format!("round{round}-{n}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                ok(&warm_payload(&names))
            } else {
                ok(r#"{"items":[]}"#)
            }
        });
        // The default list is rewritten on disk by each run, as it is for real.
        let mut warmer = Warmer::default();
        let mut now = NOW;
        for _ in 0..40 {
            fixture.tick(&mut warmer, now);
            let ran = fixture.ran();
            if let Some((_, _)) = ran.first() {
                let round = pass.get() - 1;
                let names: Vec<String> = (0..4).map(|n| format!("round{round}-{n}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                fixture.store(None, "", snapshot(now, Some(86_400), &warm_payload(&names)));
                for name in &names {
                    fixture.store(None, name, snapshot(now, None, r#"{"items":[]}"#));
                }
            }
            now += 100_000;
            assert!(
                warmer.targets.len() <= 1 + MAX_WARM_ADVERTISED,
                "{}",
                warmer.targets.len()
            );
        }
    }

    #[test]
    fn a_rewritten_snapshot_is_read_again_even_within_the_same_second() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 10, Some(86_400), &warm_payload(&["old"])),
        );
        let mut warmer = Warmer::default();
        fixture.tick(&mut warmer, NOW);
        assert_eq!(fixture.ran(), [(None, "old".to_string())]);
        // Same time, new payload.
        fixture.store(
            None,
            "",
            snapshot(NOW - 10, Some(86_400), &warm_payload(&["new"])),
        );
        fixture.tick(&mut warmer, NOW + 400);
        let ran: Vec<_> = fixture.ran().into_iter().map(|(_, query)| query).collect();
        assert_eq!(ran, ["new"]);
    }

    #[test]
    fn what_a_snapshot_asked_for_is_remembered_between_ticks() {
        let fixture = Fixture::new(&[""]);
        fixture.store(
            None,
            "",
            snapshot(NOW - 10, Some(86_400), &warm_payload(&["review"])),
        );
        let mut warmer = Warmer::default();
        for later in [0, 400, 800] {
            fixture.tick(&mut warmer, NOW + later);
            let ran: Vec<_> = fixture.ran().into_iter().map(|(_, query)| query).collect();
            assert_eq!(ran, ["review"], "at +{later}");
        }
    }
}
