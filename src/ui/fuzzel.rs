use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::{UISettings, UI};
use crate::{
    read_desktop_icon_map, read_icon_map, resolve_launcher_icon_path, AddonsConfig, RaffiConfig,
};

/// Fuzzel-based UI implementation
pub struct FuzzelUI;

impl UI for FuzzelUI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        _addons: &AddonsConfig,
        settings: &UISettings,
    ) -> Result<String> {
        let input = make_fuzzel_input(configs, settings.no_icons)?;
        run_fuzzel_with_input(&input)
    }
}

/// Create the input for fuzzel based on the Raffi configurations.
fn make_fuzzel_input(rafficonfigs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    if no_icons {
        return Ok(render_fuzzel_input(rafficonfigs, None));
    }

    // Load icon maps once for the entire menu build.
    let icon_map = read_icon_map().unwrap_or_default();
    let desktop_icon_map = read_desktop_icon_map();

    Ok(render_fuzzel_input(
        rafficonfigs,
        Some((&icon_map, &desktop_icon_map)),
    ))
}

type IconMaps<'a> = Option<(
    &'a std::collections::HashMap<String, String>,
    &'a std::collections::HashMap<String, String>,
)>;

fn render_fuzzel_input(rafficonfigs: &[RaffiConfig], icon_maps: IconMaps) -> String {
    let mut ret = String::new();

    let Some((icon_map, desktop_icon_map)) = icon_maps else {
        for mc in rafficonfigs {
            let description = mc
                .description
                .clone()
                .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
            ret.push_str(&format!("{description}\n"));
        }
        return ret;
    };

    for mc in rafficonfigs {
        let description = mc
            .description
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        let icon_path =
            resolve_launcher_icon_path(mc, icon_map, desktop_icon_map).unwrap_or_default();
        ret.push_str(&format!("{description}\0icon\x1f{icon_path}\n"));
    }
    ret
}

/// Run the fuzzel command with the provided input and return its output.
fn run_fuzzel_with_input(input: &str) -> Result<String> {
    let cache_file = super::get_mru_cache_path().context("Failed to get MRU cache path")?;
    let cache_file = cache_file
        .to_str()
        .context("MRU cache path is not valid UTF-8")?;
    let mut child = Command::new("fuzzel")
        .args(["-d", "--counter", "--cache", cache_file])
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::render_fuzzel_input;
    use crate::RaffiConfig;

    #[test]
    fn test_make_fuzzel_input_uses_resolved_fallback_icon() {
        let configs = vec![RaffiConfig {
            binary: Some("jumpapp".to_string()),
            args: Some(vec!["-X".to_string(), "firefox".to_string()]),
            description: Some("Firefox".to_string()),
            ..Default::default()
        }];
        let icon_map = HashMap::from([("firefox".to_string(), "/icons/firefox.svg".to_string())]);
        let desktop_icon_map = HashMap::from([("firefox".to_string(), "firefox".to_string())]);

        let input = render_fuzzel_input(&configs, Some((&icon_map, &desktop_icon_map)));

        assert_eq!(input, "Firefox\0icon\x1f/icons/firefox.svg\n");
    }

    #[test]
    fn test_make_fuzzel_input_without_icons() {
        let configs = vec![RaffiConfig {
            binary: Some("firefox".to_string()),
            description: Some("Firefox".to_string()),
            ..Default::default()
        }];

        let input = render_fuzzel_input(&configs, None);

        assert_eq!(input, "Firefox\n");
    }
}
