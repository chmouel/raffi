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

#[derive(Deserialize)]
struct Config {
    #[serde(flatten)]
    toplevel: HashMap<String, Value>,
}

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
}

fn get_icon_map() -> Result<HashMap<String, String>> {
    let mut icon_map = HashMap::new();
    let iconhome = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.local/share",
            std::env::var("HOME").unwrap_or_else(|_| {
                eprintln!("HOME not set");
                String::new()
            })
        )
    }) + "/icons";
    let icondirs = vec!["/usr/share/icons", "/usr/share/pixmaps", &iconhome];
    for dir in icondirs {
        for entry in walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let fname = entry
                .file_name()
                .to_str()
                .context("Invalid file name")?
                .split('.')
                .next()
                .context("Invalid file name")?
                .to_string();
            if entry
                .path()
                .to_str()
                .context("Invalid path")?
                .ends_with("png")
                || entry
                    .path()
                    .to_str()
                    .context("Invalid path")?
                    .ends_with("svg")
            {
                icon_map.insert(
                    fname,
                    entry.path().to_str().context("Invalid path")?.to_string(),
                );
            }
        }
    }
    Ok(icon_map)
}

fn read_config(filename: &str) -> Result<Vec<RaffiConfig>> {
    let file = File::open(filename).context(format!("cannot open config file {}", filename))?;
    let config: Config =
        serde_yaml::from_reader(file).context(format!("cannot parse config file {}", filename))?;
    let mut rafficonfigs = Vec::new();
    for (_, value) in config.toplevel {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value)
                .context(format!("cannot parse config file {}", filename))?;
            if mc.disabled.unwrap_or(false) {
                continue;
            }

            if let Some(binary) = mc.binary.clone() {
                if !find_binary(&binary) {
                    continue;
                }
            } else if let Some(description) = mc.description.clone() {
                mc.binary = Some(description);
            } else {
                continue;
            }

            if let Some(ifenveq) = mc.ifenveq.clone() {
                if ifenveq.len() != 2
                    || std::env::var(&ifenveq[0]).unwrap_or_default() != ifenveq[1]
                {
                    continue;
                }
            }
            if let Some(ifenvset) = mc.ifenvset.clone() {
                if std::env::var(&ifenvset).is_err() {
                    continue;
                }
            }

            if let Some(ifenvnotset) = mc.ifenvnotset.clone() {
                if std::env::var(&ifenvnotset).is_ok() {
                    continue;
                }
            }

            if let Some(ifexist) = mc.ifexist.clone() {
                if !find_binary(&ifexist) {
                    continue;
                }
            }
            rafficonfigs.push(mc);
        }
    }
    Ok(rafficonfigs)
}

fn find_binary(binary: &str) -> bool {
    std::env::var("PATH")
        .unwrap()
        .split(':')
        .any(|path| Path::new(&(path.to_string() + "/" + binary)).exists())
}

fn run_fuzzel_with_input(input: &str) -> Result<String> {
    let mut child = Command::new("fuzzel")
        .args(["-d", "--no-sort", "--counter"])
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot launch raffi command")?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(input.as_bytes())
            .context("Failed to write to stdin")?;
    }
    let output = child.wait_with_output().context("failed to read output")?;
    String::from_utf8(output.stdout).context("Invalid UTF-8 in output")
}

fn save_to_cache_file(map: &HashMap<String, String>) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let xdg_cache_home =
        std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));
    let cache_dir = format!("{}/raffi", xdg_cache_home);

    // Create the cache directory if it does not exist
    fs::create_dir_all(&cache_dir).context("Failed to create cache directory")?;

    let mut cache_file =
        File::create(format!("{}/icon.cache", cache_dir)).context("Failed to create cache file")?;
    cache_file
        .write_all(
            serde_json::to_string(&map)
                .context("Failed to serialize icon map")?
                .as_bytes(),
        )
        .context("Failed to write to cache file")?;
    Ok(())
}

fn read_icon_map() -> Result<HashMap<String, String>> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let xdg_cache_home =
        std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));
    let cache_path = format!("{xdg_cache_home}/raffi/icon.cache");

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

fn make_fuzzel_input(rafficonfigs: &[RaffiConfig]) -> Result<String> {
    let mut ret = String::new();
    let icon_map = read_icon_map()?;

    for mc in rafficonfigs {
        let s = mc
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
        ret.push_str(&format!("{s}\0icon\x1f{icon_path}\n"));
    }
    Ok(ret)
}

fn main() -> Result<()> {
    let args = Args::parse_args_default_or_exit();

    let home = std::env::var("HOME").context("HOME not set")?;
    let xdg_config_home =
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{}/.config", home));

    let configfile = args
        .configfile
        .unwrap_or_else(|| format!("{}/raffi/raffi.yaml", xdg_config_home));
    if args.refresh_cache {
        let icon_map = get_icon_map()?;
        save_to_cache_file(&icon_map)?;
    }
    let rafficonfigs = read_config(&configfile)?;
    let inputs = make_fuzzel_input(&rafficonfigs)?;
    let ret = run_fuzzel_with_input(&inputs)?;
    let chosen = ret
        .split(':')
        .last()
        .context("Failed to split input")?
        .trim();
    for mc in rafficonfigs {
        if mc
            .description
            .as_deref()
            .unwrap_or_else(|| mc.binary.as_deref().unwrap())
            == chosen
        {
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
