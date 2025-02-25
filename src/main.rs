use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
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
    script: Option<String>,
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
    #[options(
        help = "default shell when using scripts",
        default = "bash",
        short = "P"
    )]
    default_script_shell: String,
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
fn read_config(filename: &str, args: &Args) -> Result<Vec<RaffiConfig>> {
    let file = File::open(filename).context(format!("cannot open config file {}", filename))?;
    let config: Config =
        serde_yaml::from_reader(file).context(format!("cannot parse config file {}", filename))?;
    let mut rafficonfigs = Vec::new();

    for value in config.toplevel.values() {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value.clone())
                .context("cannot parse config entry".to_string())?;
            if mc.disabled.unwrap_or(false) || !is_valid_config(&mut mc, args) {
                continue;
            }
            rafficonfigs.push(mc);
        }
    }
    Ok(rafficonfigs)
}

/// Validate the RaffiConfig based on various conditions.
fn is_valid_config(mc: &mut RaffiConfig, args: &Args) -> bool {
    if let Some(_script) = &mc.script {
        if !find_binary(mc.binary.as_deref().unwrap_or(&args.default_script_shell)) {
            return false;
        }
    } else if let Some(binary) = &mc.binary {
        if !find_binary(binary) {
            return false;
        }
    } else if let Some(description) = &mc.description {
        mc.binary = Some(description.clone());
    } else {
        return false;
    }

    mc.ifenveq
        .as_ref()
        .is_none_or(|eq| eq.len() == 2 && std::env::var(&eq[0]).unwrap_or_default() == eq[1])
        && mc
            .ifenvset
            .as_ref()
            .is_none_or(|var| std::env::var(var).is_ok())
        && mc
            .ifenvnotset
            .as_ref()
            .is_none_or(|var| std::env::var(var).is_err())
        && mc.ifexist.as_ref().is_none_or(|exist| find_binary(exist))
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
    let cache_file = format!(
        "{}/.cache/raffi/mru.cache",
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_default().to_string())
    );
    if let Some(parent) = Path::new(&cache_file).parent() {
        fs::create_dir_all(parent).context("Failed to create cache directory for fuzzel")?;
    }
    let mut child = Command::new("fuzzel")
        .args(["-d", "--counter", "--cache", &cache_file])
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

/// Execute the chosen command or script.
fn execute_chosen_command(mc: &RaffiConfig, args: &Args, interpreter: &str) -> Result<()> {
    // make interepreter with mc.binary and mc.args on the same line
    let interpreter_with_args = mc.args.as_ref().map_or(interpreter.to_string(), |args| {
        format!("{} {}", interpreter, args.join(" "))
    });

    if args.print_only {
        if let Some(script) = &mc.script {
            println!("#!/usr/bin/env -S {}\n{}", interpreter_with_args, script);
        } else {
            println!(
                "{} {}",
                mc.binary.as_deref().context("Binary not found")?,
                mc.args.as_deref().unwrap_or(&[]).join(" ")
            );
        }
        return Ok(());
    }
    if let Some(script) = &mc.script {
        let mut temp_script =
            tempfile::NamedTempFile::new().context("Failed to create temp script file")?;
        writeln!(
            temp_script,
            "#!/usr/bin/env -S {}\n{}",
            interpreter_with_args, script
        )
        .context("Failed to write to temp script file")?;

        // set the script file to be executable
        let mut permissions = temp_script
            .as_file()
            .metadata()
            .context("Failed to get metadata of temp script file")?
            .permissions();
        permissions.set_mode(0o755);
        temp_script
            .as_file()
            .set_permissions(permissions)
            .context("Failed to set permissions of temp script file")?;
        temp_script
            .flush()
            .context("Failed to flush temp script file")?;
        let temp_script_path = temp_script
            .path()
            .to_str()
            .context("Failed to get temp script path")?
            .to_string();
        temp_script
            .persist(&temp_script_path)
            .context("Failed to persist temp script file")?;

        let mut command = Command::new(&temp_script_path);
        let mut child = command.spawn().context("cannot launch script")?;
        child.wait().context("cannot wait for child")?;
        // remove the temp script file
        fs::remove_file(temp_script_path.clone()).context("Failed to remove temp script file")?;
    } else {
        let mut command = Command::new(mc.binary.as_deref().context("Binary not found")?);
        if let Some(binary_args) = &mc.args {
            command.args(binary_args);
        }
        let mut child = command.spawn().context("cannot launch binary")?;
        child.wait().context("cannot wait for child")?;
    }
    Ok(())
}

/// Main function to execute the program logic.
fn main() -> Result<()> {
    let args = Args::parse_args_default_or_exit();
    let configfile = args.configfile.clone().unwrap_or_else(|| {
        format!(
            "{}/raffi/raffi.yaml",
            std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!(
                "{}/.config",
                std::env::var("HOME").unwrap_or_default()
            ))
        )
    });

    if args.refresh_cache {
        refresh_icon_cache()?;
    }

    let rafficonfigs = read_config(&configfile, &args)?;
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
            let interpreter = mc
                .binary
                .clone()
                .unwrap_or_else(|| args.default_script_shell.clone());
            execute_chosen_command(&mc, &args, &interpreter)?;
        }
    }
    Ok(())
}

/// Refresh the icon cache.
fn refresh_icon_cache() -> Result<()> {
    let icon_map = get_icon_map()?;
    save_to_cache_file(&icon_map)?;
    Ok(())
}
