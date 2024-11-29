use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use gumdrop::Options;
use serde::Deserialize;
use serde_yaml::Value;

/// Represents the configuration for each Raffi entry.
#[derive(Deserialize)]
struct RaffiConfig {
    binary: Option<String>,
    args: Option<Vec<String>>,
    icon: Option<String>,
    description: Option<String>,
    ifenveq: Option<Vec<String>>,
    ifenvset: Option<String>,
    ifenvnotset: Option<String>,
    ifexist: Option<String>,
    disabled: Option<bool>,
}

/// Represents the top-level configuration structure.
#[derive(Deserialize)]
struct Config {
    #[serde(flatten)]
    toplevel: HashMap<String, Value>,
}

/// Command-line arguments structure.
#[derive(Debug, Options)]
struct Args {
    #[options(help = "print help message")]
    help: bool,
    #[options(help = "print version")]
    version: bool,
    #[options(help = "config file location")]
    configfile: Option<String>,
    #[options(help = "print command to stdout, do not run it")]
    print_only: bool,
    #[options(help = "refresh cache")]
    refresh_cache: bool,
    #[options(help = "do not show icons", short = "I")]
    no_icons: bool,
}

/// Get the icon mapping from system directories.
fn get_icon_map() -> Result<HashMap<String, String>> {
    let mut icon_map = HashMap::new();
    let iconhome = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()))
        + "/icons";

    let icon_dirs = vec!["/usr/share/icons", "/usr/share/pixmaps", &iconhome];

    for dir in icon_dirs {
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let fname = entry.file_name().to_string_lossy().to_string();
            if let Some(ext) = entry.path().extension().and_then(|s| s.to_str()) {
                if ext == "png" || ext == "svg" {
                    icon_map.insert(
                        fname.split('.').next().unwrap().to_string(),
                        entry.path().to_string_lossy().to_string(),
                    );
                }
            }
        }
    }
    Ok(icon_map)
}

/// Read the configuration file and return a list of RaffiConfig.
fn read_config(filename: &str) -> Result<Vec<RaffiConfig>> {
    let file = File::open(filename).context(format!("cannot open config file {}", filename))?;
    let config: Config =
        serde_yaml::from_reader(file).context(format!("cannot parse config file {}", filename))?;
    let mut rafficonfigs = Vec::new();

    for value in config.toplevel.values() {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value.clone())
                .context("cannot parse config entry".to_string())?;
            if mc.disabled.unwrap_or(false) || !is_valid_config(&mut mc) {
                continue;
            }
            rafficonfigs.push(mc);
        }
    }
    Ok(rafficonfigs)
}

/// Validate the RaffiConfig based on various conditions.
fn is_valid_config(mc: &mut RaffiConfig) -> bool {
    if let Some(binary) = &mc.binary {
        if !find_binary(binary) {
            return false;
        }
    } else if let Some(description) = &mc.description {
        mc.binary = Some(description.clone());
    } else {
        return false;
    }

    mc.ifenveq.as_ref().map_or(true, |eq| {
        eq.len() == 2 && std::env::var(&eq[0]).unwrap_or_default() == eq[1]
    }) && mc
        .ifenvset
        .as_ref()
        .map_or(true, |var| std::env::var(var).is_ok())
        && mc
            .ifenvnotset
            .as_ref()
            .map_or(true, |var| std::env::var(var).is_err())
        && mc.ifexist.as_ref().map_or(true, |exist| find_binary(exist))
}

/// Check if a binary exists in the PATH.
fn find_binary(binary: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|path| Path::new(&format!("{}/{}", path, binary)).exists())
}

/// Run the fuzzel command with the provided input and return its output.
fn run_fuzzel_with_input(input: &str) -> Result<String> {
    let mut child = Command::new("fuzzel")
        .args(["-d", "--no-sort", "--counter"])
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

/// Save the icon map to a cache file.
fn save_to_cache_file(map: &HashMap<String, String>) -> Result<()> {
    let cache_dir = format!(
        "{}/.cache/raffi",
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()))
    );

    fs::create_dir_all(&cache_dir).context("Failed to create cache directory")?;

    let cache_file_path = format!("{}/icon.cache", cache_dir);
    let mut cache_file = File::create(&cache_file_path).context("Failed to create cache file")?;
    cache_file
        .write_all(
            serde_json::to_string(map)
                .context("Failed to serialize icon map")?
                .as_bytes(),
        )
        .context("Failed to write to cache file")?;
    Ok(())
}

/// Read the icon map from the cache file or generate it if it doesn't exist.
fn read_icon_map() -> Result<HashMap<String, String>> {
    let cache_path = format!(
        "{}/.cache/raffi/icon.cache",
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()))
    );

    if !Path::new(&cache_path).exists() {
        let icon_map = get_icon_map()?;
        save_to_cache_file(&icon_map)?;
        return Ok(icon_map);
    }

    let mut cache_file = File::open(&cache_path).context("Failed to open cache file")?;
    let mut contents = String::new();
    cache_file
        .read_to_string(&mut contents)
        .context("Failed to read cache file")?;
    serde_json::from_str(&contents).context("Failed to deserialize cache file")
}

/// Create the input for fuzzel based on the Raffi configurations.
fn make_fuzzel_input(rafficonfigs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let icon_map = if no_icons {
        HashMap::new()
    } else {
        read_icon_map()?
    };
    let mut ret = String::new();

    for mc in rafficonfigs {
        let description = mc
            .description
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        if no_icons {
            ret.push_str(&format!("{}\n", description));
        } else {
            let icon = mc
                .icon
                .clone()
                .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
            let icon_path = icon_map
                .get(&icon)
                .unwrap_or(&"default".to_string())
                .to_string();
            ret.push_str(&format!("{}\0icon\x1f{}\n", description, icon_path));
        }
    }
    Ok(ret)
}

/// Main function to execute the program logic.
fn main() -> Result<()> {
    let args = Args::parse_args_default_or_exit();
    let configfile = args.configfile.unwrap_or_else(|| {
        format!(
            "{}/raffi/raffi.yaml",
            std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!(
                "{}/.config",
                std::env::var("HOME").unwrap_or_default()
            ))
        )
    });

    if args.refresh_cache {
        let icon_map = get_icon_map()?;
        save_to_cache_file(&icon_map)?;
    }

    let rafficonfigs = read_config(&configfile)?;
    let inputs = make_fuzzel_input(&rafficonfigs, args.no_icons)?;
    let ret = run_fuzzel_with_input(&inputs)?;
    let chosen = ret
        .split(':')
        .last()
        .context("Failed to split input")?
        .trim();

    for mc in rafficonfigs {
        let description = mc
            .description
            .as_deref()
            .unwrap_or_else(|| mc.binary.as_deref().unwrap_or("unknown"));
        if description == chosen {
            if args.print_only {
                println!(
                    "{} {}",
                    mc.binary.as_deref().context("Binary not found")?,
                    mc.args.as_deref().unwrap_or(&[]).join(" ")
                );
                return Ok(());
            }
            let mut child = Command::new(mc.binary.as_deref().context("Binary not found")?)
                .args(mc.args.as_deref().unwrap_or(&[]))
                .spawn()
                .context("cannot launch binary")?;
            child.wait().context("cannot wait for child")?;
        }
    }
    Ok(())
}
