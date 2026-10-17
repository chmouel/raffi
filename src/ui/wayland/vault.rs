//! Secret operations of the password manager addons: which operation is
//! running, the master password or passphrase prompt, and the job that
//! acts on a fetched secret once Raffi has closed.
//!
//! Workers have no Raffi-side effects: they hand an outcome back to the UI
//! thread, which drops it unless its generation is still current, so
//! nothing is copied, typed or prompted for after Escape or Lock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::secret::Secret;

use super::bitwarden;
use super::command_list::{CommandItem, CommandListAddon};
use super::pass::{self, PassContext, Pinentry};
use super::secret_io::{
    copy_secret, type_secret, Cancel, ClipboardTools, Runner, SecretError, SystemRunner, TypeTool,
};
use super::support::MruEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Password,
    Username,
    Otp,
}

/// What to do with an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryAction {
    Copy(Field),
    Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VaultAddon {
    Pass,
    Bitwarden,
}

impl VaultAddon {
    pub fn of(addon: CommandListAddon) -> Option<Self> {
        match addon {
            CommandListAddon::Pass => Some(Self::Pass),
            CommandListAddon::Bitwarden => Some(Self::Bitwarden),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpKind {
    Load,
    /// An action on the entry (pass) or item id (Bitwarden) named here.
    Entry(EntryAction, String),
    Lock,
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Op {
    pub addon: VaultAddon,
    pub kind: OpKind,
}

impl Op {
    /// The operation without the entry name or item id, for logs.
    pub fn log_name(&self) -> String {
        match &self.kind {
            OpKind::Entry(action, _) => format!("{:?} {action:?}", self.addon),
            kind => format!("{:?} {kind:?}", self.addon),
        }
    }

    /// The row the operation acts on, for entry actions.
    pub fn identity(&self) -> Option<String> {
        let OpKind::Entry(_, name) = &self.kind else {
            return None;
        };
        Some(match self.addon {
            VaultAddon::Pass => pass::entry_identity(name),
            VaultAddon::Bitwarden => bitwarden::item_identity(name),
        })
    }

    /// The row text shown while the operation runs, if any.
    pub fn progress(&self) -> Option<&'static str> {
        match (&self.kind, self.addon) {
            (OpKind::Lock, _) => Some("Locking..."),
            (OpKind::Sync, _) => Some("Syncing..."),
            (OpKind::Entry(..), VaultAddon::Bitwarden) => Some("Reading from Bitwarden..."),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    GpgPassphrase,
}

impl Purpose {
    pub fn placeholder(self) -> &'static str {
        match self {
            Self::GpgPassphrase => "GPG passphrase",
        }
    }
}

/// Settings of the addon the operation belongs to.
#[derive(Debug, Clone)]
pub(super) enum Backend {
    Pass(PassContext),
    Bitwarden,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct JobSettings {
    pub clipboard_timeout: u64,
    pub type_delay: Duration,
}

/// Everything a worker needs; secrets inside are redacted from `Debug`.
#[derive(Debug, Clone)]
pub(super) struct Request {
    pub op: Op,
    pub backend: Backend,
    /// What the user typed in the prompt, if this run follows one.
    pub credential: Option<Secret>,
    /// Run `rbw unlock` (rbw's pinentry) before the operation.
    pub unlock_first: bool,
    pub settings: JobSettings,
}

#[derive(Debug, Clone)]
pub(super) enum Outcome {
    Loaded(Vec<CommandItem>),
    /// Close Raffi and run this.
    Ready(SecretJob),
    /// Lock or Sync finished; reload the list.
    Done,
    /// Ask for the passphrase, with why the last one failed.
    Prompt(Purpose, Option<SecretError>),
    /// The Bitwarden vault is locked: run again with `unlock_first`.
    NeedsUnlock,
    Failed(SecretError),
}

#[derive(Debug, Clone)]
pub(super) struct Report {
    pub outcome: Outcome,
    /// Something the user should know even though the operation worked.
    pub notice: Option<String>,
}

impl Report {
    fn of(outcome: Outcome) -> Self {
        Self {
            outcome,
            notice: None,
        }
    }

    pub fn failed(error: SecretError) -> Self {
        Self::of(Outcome::Failed(error))
    }
}

/// Run `request` to completion. Blocking: call it off the UI thread.
pub(super) fn run(runner: &dyn Runner, request: Request) -> Report {
    let report = match &request.backend {
        Backend::Pass(ctx) => Report::of(run_pass(runner, ctx, &request)),
        Backend::Bitwarden => Report::of(run_bitwarden(runner, &request)),
    };
    if runner.cancelled() {
        return Report::of(Outcome::Failed(SecretError::Cancelled));
    }
    report
}

fn run_pass(runner: &dyn Runner, ctx: &PassContext, request: &Request) -> Outcome {
    let entry_action = match &request.op.kind {
        OpKind::Load => {
            return match pass::load_entries(ctx) {
                Ok(items) => Outcome::Loaded(items),
                Err(error) => Outcome::Failed(SecretError::Tool(error)),
            }
        }
        OpKind::Lock => return done(pass::lock(runner)),
        OpKind::Sync => return done(pass::sync(runner, ctx)),
        OpKind::Entry(action, entry) => (*action, entry),
    };
    let (action, entry) = entry_action;
    let pinentry = match &request.credential {
        Some(passphrase) => Pinentry::Loopback(passphrase),
        None => Pinentry::Cached,
    };
    match pass::fetch(runner, ctx, entry, action, pinentry) {
        Ok(payload) => Outcome::Ready(SecretJob::fetched(action, payload, request.settings)),
        Err(SecretError::NeedPassphrase) if ctx.pinentry == crate::PassPinentry::Agent => {
            Outcome::Ready(SecretJob {
                action,
                source: JobSource::PassAgent {
                    ctx: ctx.clone(),
                    entry: entry.clone(),
                },
                settings: request.settings,
            })
        }
        Err(SecretError::NeedPassphrase) => Outcome::Prompt(Purpose::GpgPassphrase, None),
        Err(SecretError::BadPassphrase) if request.credential.is_some() => {
            Outcome::Prompt(Purpose::GpgPassphrase, Some(SecretError::BadPassphrase))
        }
        Err(error) => Outcome::Failed(error),
    }
}

fn done(result: Result<(), SecretError>) -> Outcome {
    match result {
        Ok(()) => Outcome::Done,
        Err(error) => Outcome::Failed(error),
    }
}

fn run_bitwarden(runner: &dyn Runner, request: &Request) -> Outcome {
    if request.op.kind == OpKind::Lock {
        return done(bitwarden::lock(runner));
    }
    // Every other rbw command unlocks on its own, popping rbw's pinentry
    // under a misleading progress row; check first and say so instead.
    if request.unlock_first {
        if let Err(error) = bitwarden::unlock(runner) {
            return Outcome::Failed(error);
        }
    } else {
        match bitwarden::is_unlocked(runner) {
            Ok(true) => {}
            Ok(false) => return Outcome::NeedsUnlock,
            Err(error) => return Outcome::Failed(error),
        }
    }
    if runner.cancelled() {
        return Outcome::Failed(SecretError::Cancelled);
    }
    let result = match &request.op.kind {
        OpKind::Load => bitwarden::load_items(runner).map(Outcome::Loaded),
        OpKind::Sync => bitwarden::sync(runner).map(|()| Outcome::Done),
        OpKind::Entry(action, id) => bitwarden::fetch(runner, id, *action)
            .map(|payload| Outcome::Ready(SecretJob::fetched(*action, payload, request.settings))),
        OpKind::Lock => unreachable!("handled above"),
    };
    result.unwrap_or_else(Outcome::Failed)
}

#[derive(Debug, Clone)]
enum JobSource {
    Fetched(Secret),
    /// Decrypt after Raffi closed, with gpg-agent's own pinentry.
    PassAgent {
        ctx: PassContext,
        entry: String,
    },
}

/// A secret action run after Raffi closed. Its `Debug` shows no secret.
#[derive(Debug, Clone)]
pub(super) struct SecretJob {
    action: EntryAction,
    source: JobSource,
    settings: JobSettings,
}

impl SecretJob {
    pub fn fetched(action: EntryAction, secret: Secret, settings: JobSettings) -> Self {
        Self {
            action,
            source: JobSource::Fetched(secret),
            settings,
        }
    }

    pub fn run(&self) -> Result<(), String> {
        self.run_with(&SystemRunner {
            cancel: Cancel::default(),
        })
        .map_err(|error| error.message())
    }

    fn run_with(&self, runner: &dyn Runner) -> Result<(), SecretError> {
        let secret = match &self.source {
            JobSource::Fetched(secret) => secret.clone(),
            JobSource::PassAgent { ctx, entry } => {
                pass::fetch(runner, ctx, entry, self.action, Pinentry::Agent)?
            }
        };
        match self.action {
            EntryAction::Copy(_) => copy_secret(
                &ClipboardTools::detect()?,
                &secret,
                self.settings.clipboard_timeout,
            ),
            EntryAction::Type => {
                let tool = TypeTool::detect()?;
                std::thread::sleep(self.settings.type_delay);
                type_secret(runner, tool, &secret)
            }
        }
    }
}

/// The masked prompt for the GPG passphrase.
#[derive(Debug)]
pub(super) struct PromptState {
    pub purpose: Purpose,
    /// The operation to resume once unlocked.
    pub op: Op,
    pub buffer: Secret,
    pub error: Option<String>,
}

/// Secret operations of one Raffi run. At most one runs at a time; its
/// children are killed when it is dropped, and when Raffi exits.
#[derive(Debug, Default)]
pub(super) struct VaultState {
    /// Bumped by every start and abort, so stale results are dropped.
    pub generation: u64,
    pub cancel: Cancel,
    pub running: Option<Op>,
    pub prompt: Option<PromptState>,
    /// The running operation first waits for rbw's pinentry.
    pub unlocking: bool,
    /// The Bitwarden notes search in flight, apart from the operation:
    /// its generation, its children, and the filter it is for.
    pub search_generation: u64,
    pub search_cancel: Cancel,
    pub search_query: Option<String>,
    /// When each entry was last copied or typed, by row identity, and where
    /// that is saved; `None` keeps it in memory only.
    pub usage: HashMap<String, MruEntry>,
    pub usage_path: Option<PathBuf>,
}

impl VaultState {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    /// Start `op`; its result must carry the returned generation.
    pub fn begin(&mut self, op: Op) -> (u64, Cancel) {
        self.cancel.cancel();
        self.cancel = Cancel::default();
        self.generation = self.generation.wrapping_add(1);
        self.running = Some(op);
        self.unlocking = false;
        (self.generation, self.cancel.clone())
    }

    /// Drop whatever runs, killing its children, and close the prompt.
    pub fn abort(&mut self) {
        self.cancel.cancel();
        self.generation = self.generation.wrapping_add(1);
        self.running = None;
        self.prompt = None;
        self.unlocking = false;
    }

    /// Take the operation a result belongs to, if it is still wanted.
    pub fn finish(&mut self, generation: u64) -> Option<Op> {
        if generation != self.generation {
            return None;
        }
        self.running.take()
    }

    /// Start a notes search for `query`, dropping the one in flight; its
    /// result must carry the returned generation.
    pub fn begin_search(&mut self, query: String) -> (u64, Cancel) {
        self.stop_search();
        self.search_query = Some(query);
        (self.search_generation, self.search_cancel.clone())
    }

    /// Drop the notes search in flight, killing its children.
    pub fn stop_search(&mut self) {
        if self.search_query.take().is_some() {
            self.search_cancel.cancel();
            self.search_cancel = Cancel::default();
            self.search_generation = self.search_generation.wrapping_add(1);
        }
    }

    /// The filter a search result is for, if it is still wanted.
    pub fn finish_search(&mut self, generation: u64) -> Option<String> {
        if generation != self.search_generation {
            return None;
        }
        let query = self.search_query.take();
        self.search_generation = self.search_generation.wrapping_add(1);
        query
    }

    /// The text of the row replacing the list while prompting or working.
    pub fn overlay(&self) -> Option<String> {
        if let Some(prompt) = &self.prompt {
            if self.busy() {
                return Some(match prompt.purpose {
                    Purpose::GpgPassphrase => "Decrypting...".to_string(),
                });
            }
            return Some(match &prompt.error {
                Some(error) => format!("{error}, try again (Escape cancels)"),
                None => format!(
                    "Enter the {} and press Enter (Escape cancels)",
                    prompt.purpose.placeholder()
                ),
            });
        }
        if self.unlocking && self.busy() {
            return Some("Unlocking with rbw: answer the pinentry window (Escape cancels)".into());
        }
        self.running.as_ref()?.progress().map(str::to_string)
    }
}

impl Drop for VaultState {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.search_cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretBytes;
    use crate::PassPinentry;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::super::secret_io::{Cmd, Output};

    struct FakeRunner {
        outputs: Mutex<Vec<(i32, &'static str, &'static str)>>,
        seen: Mutex<Vec<Cmd>>,
        cancel: Cancel,
    }

    impl FakeRunner {
        fn new(outputs: &[(i32, &'static str, &'static str)]) -> Self {
            Self {
                outputs: Mutex::new(outputs.iter().rev().copied().collect()),
                seen: Mutex::new(Vec::new()),
                cancel: Cancel::default(),
            }
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
            self.seen.lock().unwrap().push(cmd.clone());
            let (code, out, err) = self
                .outputs
                .lock()
                .unwrap()
                .pop()
                .ok_or(SecretError::Tool("unexpected command".into()))?;
            Ok(Output {
                code: Some(code),
                stdout: SecretBytes::new(out.as_bytes().to_vec()),
                stderr: SecretBytes::new(err.as_bytes().to_vec()),
            })
        }

        fn cancelled(&self) -> bool {
            self.cancel.is_cancelled()
        }
    }

    const NEED: &str = "[GNUPG:] ERROR pkdecrypt_failed 67108949\n";
    const BAD: &str = "[GNUPG:] ERROR pkdecrypt_failed 67108875\n";
    const OKAY: &str = "[GNUPG:] DECRYPTION_OKAY\n";

    fn pass_request(
        dir: &std::path::Path,
        pinentry: PassPinentry,
        credential: Option<&str>,
    ) -> Request {
        Request {
            op: Op {
                addon: VaultAddon::Pass,
                kind: OpKind::Entry(EntryAction::Copy(Field::Password), "site".into()),
            },
            backend: Backend::Pass(PassContext {
                store_dir: dir.to_path_buf(),
                pinentry,
                username_fields: vec!["login".into()],
                gpg: "gpg".into(),
            }),
            credential: credential.map(|text| Secret::new(text.into())),
            unlock_first: false,
            settings: JobSettings {
                clipboard_timeout: 45,
                type_delay: Duration::ZERO,
            },
        }
    }

    fn store() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raffi-vault-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("site.gpg"), b"").unwrap();
        dir
    }

    #[test]
    fn test_log_name_leaves_out_the_entry() {
        let op = Op {
            addon: VaultAddon::Pass,
            kind: OpKind::Entry(EntryAction::Copy(Field::Otp), "bank/alice".into()),
        };
        assert_eq!(op.log_name(), "Pass Copy(Otp)");
    }

    #[test]
    fn test_pass_prompts_retries_then_fetches() {
        let dir = store();
        let runner = FakeRunner::new(&[(2, "", NEED)]);
        let report = run(&runner, pass_request(&dir, PassPinentry::Raffi, None));
        assert!(matches!(
            report.outcome,
            Outcome::Prompt(Purpose::GpgPassphrase, None)
        ));

        let runner = FakeRunner::new(&[(2, "", BAD)]);
        let report = run(
            &runner,
            pass_request(&dir, PassPinentry::Raffi, Some("wrong")),
        );
        assert!(matches!(
            report.outcome,
            Outcome::Prompt(Purpose::GpgPassphrase, Some(SecretError::BadPassphrase))
        ));

        let runner = FakeRunner::new(&[(0, "pw\n", OKAY)]);
        let report = run(
            &runner,
            pass_request(&dir, PassPinentry::Raffi, Some("right")),
        );
        let Outcome::Ready(job) = report.outcome else {
            panic!("expected a job");
        };
        assert!(!format!("{job:?}").contains("pw"));
        match job.source {
            JobSource::Fetched(secret) => assert_eq!(secret.expose(), "pw"),
            _ => panic!("expected a fetched secret"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_pass_agent_mode_defers_the_decrypt() {
        let dir = store();
        let runner = FakeRunner::new(&[(2, "", NEED)]);
        let report = run(&runner, pass_request(&dir, PassPinentry::Agent, None));
        let Outcome::Ready(job) = report.outcome else {
            panic!("expected a job");
        };
        assert!(matches!(job.source, JobSource::PassAgent { .. }));

        // After close, the decrypt asks gpg-agent; a failure types nothing.
        let runner = FakeRunner::new(&[(2, "", "[GNUPG:] ERROR pkdecrypt_failed 83886179\n")]);
        assert_eq!(job.run_with(&runner).unwrap_err(), SecretError::Cancelled);
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0]
            .args
            .windows(2)
            .any(|pair| pair == ["--pinentry-mode", "ask"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_cancelled_run_reports_cancelled() {
        let dir = store();
        let runner = FakeRunner::new(&[(0, "pw\n", OKAY)]);
        runner.cancel.cancel();
        let report = run(&runner, pass_request(&dir, PassPinentry::Raffi, None));
        assert!(matches!(
            report.outcome,
            Outcome::Failed(SecretError::Cancelled)
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_bitwarden_locked_asks_for_unlock_then_resumes() {
        let request = |unlock_first| Request {
            op: Op {
                addon: VaultAddon::Bitwarden,
                kind: OpKind::Entry(EntryAction::Copy(Field::Username), "id".into()),
            },
            backend: Backend::Bitwarden,
            credential: None,
            unlock_first,
            settings: JobSettings {
                clipboard_timeout: 0,
                type_delay: Duration::ZERO,
            },
        };
        // Locked: nothing else runs, so rbw's pinentry does not pop up
        // under the wrong progress row.
        let runner = FakeRunner::new(&[(1, "", "agent is locked")]);
        assert!(matches!(
            run(&runner, request(false)).outcome,
            Outcome::NeedsUnlock
        ));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);

        // The second run unlocks, then resumes the original fetch.
        let item = r#"{"id":"id","data":{"username":"me","password":null,"totp":null}}"#;
        let runner = FakeRunner::new(&[(0, "", ""), (0, item, "")]);
        assert!(matches!(
            run(&runner, request(true)).outcome,
            Outcome::Ready(_)
        ));
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen[0].args, ["unlock"]);
        assert_eq!(seen[1].args, ["get", "--raw", "id"]);
        drop(seen);

        // A cancelled pinentry stops there.
        let runner = FakeRunner::new(&[(1, "", "pinentry cancelled")]);
        assert!(matches!(
            run(&runner, request(true)).outcome,
            Outcome::Failed(SecretError::Tool(_))
        ));
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_state_drops_stale_results_and_kills_on_abort() {
        let mut state = VaultState::default();
        let op = Op {
            addon: VaultAddon::Pass,
            kind: OpKind::Load,
        };
        let (generation, cancel) = state.begin(op.clone());
        state.abort();
        assert!(cancel.is_cancelled());
        assert_eq!(state.finish(generation), None);

        let (generation, _) = state.begin(op.clone());
        assert_eq!(state.finish(generation), Some(op));
        assert!(!state.busy());
    }
}
