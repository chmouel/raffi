//! The Wayland compositor Raffi runs in, and starting programs through it.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(2);

/// The compositor did not answer a launch request in time, so it may still
/// start the program.
#[derive(Debug)]
pub struct NoAnswer(&'static str);

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} did not answer", self.0)
    }
}

impl std::error::Error for NoAnswer {}

/// The compositor Raffi runs in, for window switching, launching and log out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Compositor {
    Hyprland,
    Niri,
    Sway,
}

/// Environment variables naming each compositor's IPC socket, in detection order.
const SOCKETS: [(&str, Compositor); 3] = [
    ("HYPRLAND_INSTANCE_SIGNATURE", Compositor::Hyprland),
    ("NIRI_SOCKET", Compositor::Niri),
    ("SWAYSOCK", Compositor::Sway),
];

impl Compositor {
    /// The running compositor instance, which window ids are only unique
    /// within: niri and sway number windows from 1 every session. `var`
    /// reads an environment variable.
    pub fn instance(var: impl Fn(&str) -> Option<String>) -> Option<String> {
        SOCKETS
            .into_iter()
            .find_map(|(name, _)| var(name).filter(|value| !value.is_empty()))
    }

    /// Detect the compositor from the environment. `var` reads a variable.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        SOCKETS
            .into_iter()
            .find(|(name, _)| var(name).is_some_and(|value| !value.is_empty()))
            .map(|(_, compositor)| compositor)
    }

    pub fn current() -> Option<Self> {
        Self::detect(|name| std::env::var(name).ok())
    }

    /// The program Raffi talks to the compositor with.
    pub fn program(self) -> &'static str {
        match self {
            Self::Hyprland => "hyprctl",
            Self::Niri => "niri",
            Self::Sway => "swaymsg",
        }
    }

    /// The commands that ask the compositor to start a program, tried in
    /// order until one is accepted. `argv` is the program and its arguments,
    /// `shell` the same as a shell command line.
    pub fn launch_commands(self, argv: &[String], shell: &str) -> Vec<Vec<String>> {
        let own = |args: &[&str]| -> Vec<String> {
            std::iter::once(self.program())
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect()
        };
        match self {
            // Lua configs (Hyprland 0.55+) only take `eval`; hyprlang ones
            // only take `dispatch exec`.
            Self::Hyprland => vec![
                own(&[
                    "eval",
                    &format!("hl.dispatch(hl.dsp.exec_cmd({}))", lua_quote(shell)),
                ]),
                own(&["dispatch", "exec", shell]),
            ],
            Self::Niri => {
                let mut command = own(&["msg", "action", "spawn", "--"]);
                command.extend(argv.iter().cloned());
                vec![command]
            }
            Self::Sway if sway_keeps(shell) => vec![own(&["exec", "--", shell])],
            Self::Sway => Vec::new(),
        }
    }

    /// Start a program through the compositor, so it belongs to the session
    /// rather than to Raffi and follows the compositor's rules.
    pub fn launch(self, argv: &[String], shell: &str) -> Result<()> {
        let commands = self.launch_commands(argv, shell);
        let Some(last) = commands.len().checked_sub(1) else {
            bail!("{} would change this command line", self.program());
        };
        for (index, command) in commands.iter().enumerate() {
            let (success, reply) = run_bounded(self.program(), command)?;
            if self == Self::Hyprland && index < last && lua_unavailable(&reply) {
                crate::debug_log!("launch: hyprctl eval unavailable; using dispatch exec");
                continue;
            }
            return launch_status(self, success, &reply);
        }
        bail!("{} cannot start programs", self.program())
    }
}

/// Whether sway's `exec` runs `shell` as written. Sway splits commands on
/// `;` and `,`, strips quotes from a quoted command, replaces `$variables`
/// and joins lines, so a command line using any of them is started
/// directly instead.
fn sway_keeps(shell: &str) -> bool {
    !shell.starts_with(['\'', '"']) && !shell.contains([';', ',', '"', '\\', '$', '\n', '\r'])
}

/// Whether the compositor accepted a launch request.
fn launch_status(compositor: Compositor, success: bool, reply: &str) -> Result<()> {
    let reply = reply.trim();
    let accepted = match compositor {
        Compositor::Hyprland => reply == "ok",
        // niri prints nothing; swaymsg prints a JSON list of results and
        // exits non-zero when a command failed.
        Compositor::Niri | Compositor::Sway => success,
    };
    if accepted {
        Ok(())
    } else if reply.is_empty() {
        bail!("{} failed", compositor.program())
    } else {
        bail!("{} refused to start it: {reply}", compositor.program())
    }
}

/// Whether an `hyprctl eval` reply means Hyprland cannot run Lua: versions
/// before 0.55 do not know `eval`, and hyprlang configs reject it.
pub fn lua_unavailable(reply: &str) -> bool {
    let reply = reply.trim();
    reply == "unknown request" || reply.starts_with("eval is only supported with the lua config")
}

/// A Lua string literal for `value`. Control characters use decimal escapes
/// so the expression stays on one line.
pub fn lua_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            c if c.is_ascii_control() => quoted.push_str(&format!("\\{:03}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Run `argv` and return whether it succeeded with its standard output and
/// error. A compositor that does not answer in time is an error, so the
/// caller does not start the program a second time.
fn run_bounded(program: &'static str, argv: &[String]) -> Result<(bool, String)> {
    let mut child = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {program}"))?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(64 * 1024).read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NoAnswer(program).into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut reply = stdout.join().unwrap_or_default();
    reply.push_str(&stderr.join().unwrap_or_default());
    Ok((status.success(), reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_launch_commands() {
        let argv = vec!["env".to_string(), "--".into(), "foo bar".into()];
        let shell = "env -- 'foo bar' \"x\"";
        assert_eq!(
            Compositor::Hyprland.launch_commands(&argv, shell),
            vec![
                vec![
                    "hyprctl".to_string(),
                    "eval".into(),
                    r#"hl.dispatch(hl.dsp.exec_cmd("env -- 'foo bar' \"x\""))"#.into(),
                ],
                vec![
                    "hyprctl".to_string(),
                    "dispatch".into(),
                    "exec".into(),
                    shell.into()
                ],
            ]
        );
        assert_eq!(
            Compositor::Niri.launch_commands(&argv, shell),
            vec![vec![
                "niri", "msg", "action", "spawn", "--", "env", "--", "foo bar"
            ]]
        );
        let quoted = "env -- 'foo bar'";
        assert_eq!(
            Compositor::Sway.launch_commands(&argv, quoted),
            vec![vec!["swaymsg", "exec", "--", quoted]]
        );
        // Sway's parser would change these, so Raffi starts them itself.
        for shell in [
            shell,
            "a; b",
            "a, b",
            "echo $HOME",
            "a \\; b",
            "x\ny",
            "'a b'",
        ] {
            assert!(
                Compositor::Sway.launch_commands(&argv, shell).is_empty(),
                "{shell:?}"
            );
        }
    }

    #[test]
    fn test_launch_status() {
        assert!(launch_status(Compositor::Hyprland, true, "ok\n").is_ok());
        let error = launch_status(Compositor::Hyprland, true, "error: bad").unwrap_err();
        assert!(error.to_string().contains("bad"), "{error}");
        assert!(launch_status(Compositor::Niri, true, "").is_ok());
        assert!(launch_status(Compositor::Sway, false, "[{\"success\": false}]").is_err());
    }
}
