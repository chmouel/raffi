use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::UI;
use crate::{read_icon_map, RaffiConfig};

/// Fuzzel-based UI implementation
pub struct FuzzelUI;

impl UI for FuzzelUI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
        let input = make_fuzzel_input(configs, no_icons)?;
        run_fuzzel_with_input(&input)
    }
}

/// Create the input for fuzzel based on the Raffi configurations.
fn make_fuzzel_input(rafficonfigs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let mut ret = String::new();

    if no_icons {
        for mc in rafficonfigs {
            let description = mc
                .description
                .clone()
                .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
            ret.push_str(&format!("{description}\n"));
        }
        return Ok(ret);
    }

    // Load icon map from cache (optimized at ~5ms for 16k icons)
    let icon_map = read_icon_map().unwrap_or_default();

    for mc in rafficonfigs {
        let description = mc
            .description
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        let icon = mc
            .icon
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        let icon_path = icon_map
            .get(&icon)
            .unwrap_or(&"default".to_string())
            .to_string();
        ret.push_str(&format!("{description}\0icon\x1f{icon_path}\n"));
    }
    Ok(ret)
}

/// Run the fuzzel command with the provided input and return its output.
fn run_fuzzel_with_input(input: &str) -> Result<String> {
    let cache_file = super::get_mru_cache_path().context("Failed to get MRU cache path")?;
    let mut child = Command::new("fuzzel")
        .args(["-d", "--counter", "--cache", cache_file.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot launch fuzzel command")?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(input.as_bytes())
            .context("Failed to write to stdin")?;
    }

    let output = child.wait_with_output().context("failed to read output")?;
    String::from_utf8(output.stdout).context("Invalid UTF-8 in output")
}
