use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    fmt::Write as _,
};

use anyhow::{Context, Result};
use gumdrop::Options;
use serde::Deserialize;
use serde_yaml::Value;

/// Represents the configuration for each Raffi entry.
#[derive(Deserialize, Debug, PartialEq, Clone, Default)]
pub struct RaffiConfig {
    pub binary: Option<String>,
    pub args: Option<Vec<String>>,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub ifenveq: Option<Vec<String>>,
    pub ifenvset: Option<String>,
    pub ifenvnotset: Option<String>,
    pub ifexist: Option<String>,
    pub disabled: Option<bool>,
    pub script: Option<String>,
}

/// Represents the top-level configuration structure.
#[derive(Deserialize)]
struct Config {
    #[serde(flatten)]
    toplevel: HashMap<String, Value>,
}

/// Command-line arguments structure.
#[derive(Debug, Options, Clone)]
pub struct Args {
    #[options(help = "print help message")]
    pub help: bool,
    #[options(help = "print version")]
    pub version: bool,
    #[options(help = "config file location")]
    pub configfile: Option<String>,
    #[options(help = "print command to stdout, do not run it")]
    pub print_only: bool,
    #[options(help = "refresh cache")]
    pub refresh_cache: bool,
    #[options(help = "do not show icons", short = "I")]
    pub no_icons: bool,
    #[options(
        help = "default shell when using scripts",
        default = "bash",
        short = "P"
    )]
    pub default_script_shell: String,
}

/// A trait for checking environment variables.
pub trait EnvProvider {
    fn var(&self, key: &str) -> Result<String, std::env::VarError>;
}

/// The default environment provider.
pub struct DefaultEnvProvider;

impl EnvProvider for DefaultEnvProvider {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        std::env::var(key)
    }
}

/// A trait for checking if a binary exists.
pub trait BinaryChecker {
    fn exists(&self, binary: &str) -> bool;
}

/// The default binary checker.
pub struct DefaultBinaryChecker;

impl BinaryChecker for DefaultBinaryChecker {
    fn exists(&self, binary: &str) -> bool {
        find_binary(binary)
    }
}

/// A trait for providing an icon map.
pub trait IconMapProvider {
    fn get_icon_map(&self) -> Result<HashMap<String, String>>;
}

/// The default icon map provider.
pub struct DefaultIconMapProvider;

impl IconMapProvider for DefaultIconMapProvider {
    fn get_icon_map(&self) -> Result<HashMap<String, String>> {
        read_icon_map()
    }
}

/// Get the icon mapping from system directories.
fn get_icon_map() -> Result<HashMap<String, String>> {
    let mut icon_map = HashMap::new();
    let mut data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or("/usr/local/share/:/usr/share/".to_string());
    let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let _ = write!(&mut data_dirs, ":{data_home}");

    for datadir in std::env::split_paths(&data_dirs) {
        for subdir in &["icons", "pixmaps"] {
            let mut dir = datadir.clone();
            dir.push(subdir);
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
    }
    Ok(icon_map)
}

/// Read the configuration file and return a list of RaffiConfig.
pub fn read_config(filename: &str, args: &Args) -> Result<Vec<RaffiConfig>> {
    let file = File::open(filename).context(format!("cannot open config file {filename}"))?;
    read_config_from_reader(file, args)
}

pub fn read_config_from_reader<R: Read>(reader: R, args: &Args) -> Result<Vec<RaffiConfig>> {
    let config: Config = serde_yaml::from_reader(reader).context("cannot parse config")?;
    let mut rafficonfigs = Vec::new();

    for value in config.toplevel.values() {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value.clone())
                .context("cannot parse config entry".to_string())?;
            if mc.disabled.unwrap_or(false)
                || !is_valid_config(&mut mc, args, &DefaultEnvProvider, &DefaultBinaryChecker)
            {
                continue;
            }
            rafficonfigs.push(mc);
        }
    }
    Ok(rafficonfigs)
}

/// Validate the RaffiConfig based on various conditions.
fn is_valid_config(
    mc: &mut RaffiConfig,
    args: &Args,
    env_provider: &impl EnvProvider,
    binary_checker: &impl BinaryChecker,
) -> bool {
    if let Some(_script) = &mc.script {
        if !binary_checker.exists(mc.binary.as_deref().unwrap_or(&args.default_script_shell)) {
            return false;
        }
    } else if let Some(binary) = &mc.binary {
        if !binary_checker.exists(binary) {
            return false;
        }
    } else if let Some(description) = &mc.description {
        mc.binary = Some(description.clone());
    } else {
        return false;
    }

    mc.ifenveq
        .as_ref()
        .is_none_or(|eq| eq.len() == 2 && env_provider.var(&eq[0]).unwrap_or_default() == eq[1])
        && mc
            .ifenvset
            .as_ref()
            .is_none_or(|var| env_provider.var(var).is_ok())
        && mc
            .ifenvnotset
            .as_ref()
            .is_none_or(|var| env_provider.var(var).is_err())
        && mc
            .ifexist
            .as_ref()
            .is_none_or(|exist| binary_checker.exists(exist))
}

/// Check if a binary exists in the PATH.
fn find_binary(binary: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|path| Path::new(&format!("{path}/{binary}")).exists())
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

    let cache_file_path = format!("{cache_dir}/icon.cache");
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
pub fn make_fuzzel_input(
    rafficonfigs: &[RaffiConfig],
    no_icons: bool,
    icon_map_provider: &impl IconMapProvider,
) -> Result<String> {
    let icon_map = if no_icons {
        HashMap::new()
    } else {
        icon_map_provider.get_icon_map()?
    };
    let mut ret = String::new();

    for mc in rafficonfigs {
        let description = mc
            .description
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        if no_icons {
            ret.push_str(&format!("{description}\n"));
        } else {
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
    }
    Ok(ret)
}

/// Execute the chosen command or script.
pub fn execute_chosen_command(mc: &RaffiConfig, args: &Args, interpreter: &str) -> Result<()> {
    // make interepreter with mc.binary and mc.args on the same line
    let interpreter_with_args = mc.args.as_ref().map_or(interpreter.to_string(), |args| {
        format!("{} {}", interpreter, args.join(" "))
    });

    if args.print_only {
        if let Some(script) = &mc.script {
            println!("#!/usr/bin/env -S {interpreter_with_args}\n{script}");
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
        let mut command = Command::new(interpreter);
        command.arg("-c").arg(script);
        if let Some(args) = &mc.args {
            command.arg(interpreter);
            command.args(args);
        }
        command.spawn().context("cannot launch script")?;
    } else {
        Command::new(mc.binary.as_deref().context("Binary not found")?)
            .args(mc.args.as_deref().unwrap_or(&[]))
            .spawn()
            .context("cannot launch command")?;
    }
    Ok(())
}

pub fn run(args: Args) -> Result<()> {
    if args.version {
        println!("raffi version 0.1.0");
        return Ok(());
    }

    let default_config_path = format!(
        "{}/.config/raffi/raffi.yaml",
        std::env::var("HOME").unwrap_or_default()
    );
    let configfile = args.configfile.as_deref().unwrap_or(&default_config_path);

    let rafficonfigs = read_config(configfile, &args).context("Failed to read config")?;

    if rafficonfigs.is_empty() {
        eprintln!("No valid configurations found in {configfile}");
        std::process::exit(1);
    }

    let input = make_fuzzel_input(&rafficonfigs, args.no_icons, &DefaultIconMapProvider)
        .context("Failed to make fuzzel input")?;

    let chosen = run_fuzzel_with_input(&input).context("Failed to run fuzzel")?;

    let chosen_name = chosen.trim();
    let mc = rafficonfigs
        .iter()
        .find(|mc| {
            mc.description.as_deref() == Some(chosen_name)
                || mc.binary.as_deref() == Some(chosen_name)
        })
        .context("No matching configuration found")?;

    let interpreter = if mc.script.is_some() {
        mc.binary.as_deref().unwrap_or(&args.default_script_shell)
    } else {
        // Not used for binary commands
        ""
    };
    execute_chosen_command(mc, &args, interpreter).context("Failed to execute command")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_read_config_from_reader() {
        let yaml_config = r#"
        firefox:
          binary: firefox
          description: "Firefox browser"
        hello_script:
          script: "echo hello"
          description: "Hello script"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: "bash".to_string(),
        };
        let configs = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(configs.len(), 2);

        let expected_configs = vec![
            RaffiConfig {
                binary: Some("firefox".to_string()),
                description: Some("Firefox browser".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Hello script".to_string()),
                script: Some("echo hello".to_string()),
                ..Default::default()
            },
        ];

        for expected_config in &expected_configs {
            assert!(configs.contains(expected_config));
        }
    }

    struct MockIconMapProvider {
        icon_map: HashMap<String, String>,
    }

    impl IconMapProvider for MockIconMapProvider {
        fn get_icon_map(&self) -> Result<HashMap<String, String>> {
            Ok(self.icon_map.clone())
        }
    }

    #[test]
    fn test_make_fuzzel_input() {
        let configs = vec![
            RaffiConfig {
                binary: Some("firefox".to_string()),
                description: Some("Firefox browser".to_string()),
                icon: Some("firefox".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                script: Some("echo hello".to_string()),
                description: Some("Hello script".to_string()),
                icon: Some("script".to_string()),
                ..Default::default()
            },
        ];
        let icon_map_provider = MockIconMapProvider {
            icon_map: {
                let mut map = HashMap::new();
                map.insert("firefox".to_string(), "/path/to/firefox.png".to_string());
                map.insert("script".to_string(), "/path/to/script.png".to_string());
                map
            },
        };
        let input = make_fuzzel_input(&configs, false, &icon_map_provider).unwrap();
        assert!(input.contains("Firefox browser\0icon\x1f/path/to/firefox.png"));
        assert!(input.contains("Hello script\0icon\x1f/path/to/script.png"));
    }

    struct MockEnvProvider {
        vars: HashMap<String, String>,
    }

    impl EnvProvider for MockEnvProvider {
        fn var(&self, key: &str) -> Result<String, std::env::VarError> {
            self.vars
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    struct MockBinaryChecker {
        binaries: Vec<String>,
    }

    impl BinaryChecker for MockBinaryChecker {
        fn exists(&self, binary: &str) -> bool {
            self.binaries.contains(&binary.to_string())
        }
    }

    #[test]
    fn test_is_valid_config() {
        let mut config = RaffiConfig {
            binary: Some("test-binary".to_string()),
            description: Some("Test Description".to_string()),
            script: None,
            args: None,
            icon: None,
            ifenveq: Some(vec!["TEST_VAR".to_string(), "true".to_string()]),
            ifenvset: Some("ANOTHER_VAR".to_string()),
            ifenvnotset: Some("MISSING_VAR".to_string()),
            ifexist: Some("another-binary".to_string()),
            disabled: None,
        };
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: "bash".to_string(),
        };
        let env_provider = MockEnvProvider {
            vars: {
                let mut vars = HashMap::new();
                vars.insert("TEST_VAR".to_string(), "true".to_string());
                vars.insert("ANOTHER_VAR".to_string(), "some_value".to_string());
                vars
            },
        };
        let binary_checker = MockBinaryChecker {
            binaries: vec!["test-binary".to_string(), "another-binary".to_string()],
        };

        assert!(is_valid_config(
            &mut config,
            &args,
            &env_provider,
            &binary_checker
        ));
    }
}
