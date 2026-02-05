use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    process::Command,
};

use anyhow::{Context, Result};
use gumdrop::Options;
use serde::Deserialize;
use serde_yaml::Value;

pub mod ui;

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

/// UI type selection
#[derive(Debug, Clone, PartialEq)]
pub enum UIType {
    Fuzzel,
    #[cfg(feature = "wayland")]
    Native,
}

impl std::str::FromStr for UIType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "fuzzel" => Ok(UIType::Fuzzel),
            #[cfg(feature = "wayland")]
            "native" | "wayland" | "iced" => Ok(UIType::Native),
            #[cfg(not(feature = "wayland"))]
            "native" | "wayland" | "iced" => Err(
                "Native UI is not available. Build with the 'wayland' feature to enable it."
                    .to_string(),
            ),
            _ => {
                #[cfg(feature = "wayland")]
                {
                    Err(format!(
                        "Invalid UI type: {}. Valid options are: fuzzel, native",
                        s
                    ))
                }
                #[cfg(not(feature = "wayland"))]
                {
                    Err(format!("Invalid UI type: {}. Valid options are: fuzzel", s))
                }
            }
        }
    }
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
    #[options(help = "UI type to use: fuzzel, native (default: fuzzel)", short = "u")]
    pub ui_type: Option<String>,
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

/// Extract icon size from path (e.g., "/usr/share/icons/Papirus/48x48/apps/icon.svg" -> 48).
/// Returns 0 if size cannot be determined.
fn extract_icon_size(path: &std::path::Path) -> u32 {
    for component in path.components() {
        if let std::path::Component::Normal(s) = component {
            if let Some(s_str) = s.to_str() {
                // Match patterns like "48x48", "64x64", "scalable"
                if s_str == "scalable" {
                    return 512; // Treat scalable as large
                }
                if let Some((w, _)) = s_str.split_once('x') {
                    if let Ok(size) = w.parse::<u32>() {
                        return size;
                    }
                }
            }
        }
    }
    0
}

/// Get the icon mapping from system directories.
/// Prefers larger icons (48x48+) since raffi renders at 48x48.
fn get_icon_map() -> Result<HashMap<String, String>> {
    let mut icon_map: HashMap<String, String> = HashMap::new();
    let mut icon_sizes: HashMap<String, u32> = HashMap::new();
    let mut data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or("/usr/local/share/:/usr/share/".to_string());
    let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    write!(&mut data_dirs, ":{data_home}")?;

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
                        let icon_name = fname.rsplit_once('.').unwrap().0.to_string();
                        let icon_path = entry.path().to_string_lossy().to_string();
                        let new_size = extract_icon_size(entry.path());
                        let current_size = icon_sizes.get(&icon_name).copied().unwrap_or(0);

                        // Prefer icons >= 48px, or larger than current
                        if new_size >= current_size || (new_size >= 48 && current_size < 48) {
                            icon_map.insert(icon_name.clone(), icon_path);
                            icon_sizes.insert(icon_name, new_size);
                        }
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

/// Save the icon map to a cache file.
fn save_to_cache_file(map: &HashMap<String, String>) -> Result<()> {
    let cache_dir = format!(
        "{}/raffi",
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

/// Clear the icon cache file to force regeneration.
pub fn clear_icon_cache() -> Result<()> {
    let cache_path = format!(
        "{}/raffi/icon.cache",
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()))
    );
    if Path::new(&cache_path).exists() {
        fs::remove_file(&cache_path).context("Failed to remove icon cache file")?;
    }
    Ok(())
}

/// Read the icon map from the cache file or generate it if it doesn't exist.
pub fn read_icon_map() -> Result<HashMap<String, String>> {
    let cache_path = format!(
        "{}/raffi/icon.cache",
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

    if args.refresh_cache {
        clear_icon_cache()?;
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

    // Determine UI type
    let ui_type = if let Some(ref ui_type_str) = args.ui_type {
        ui_type_str
            .parse::<UIType>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else if find_binary("fuzzel") {
        UIType::Fuzzel
    } else {
        #[cfg(feature = "wayland")]
        {
            UIType::Native
        }
        #[cfg(not(feature = "wayland"))]
        {
            return Err(anyhow::anyhow!(
                "No UI backend available. Install 'fuzzel' or build with the 'wayland' feature."
            ));
        }
    };

    // Get the appropriate UI implementation
    let ui = ui::get_ui(ui_type);
    let chosen = ui
        .show(&rafficonfigs, args.no_icons)
        .context("Failed to show UI")?;

    let chosen_name = chosen.trim();
    if chosen_name.is_empty() {
        std::process::exit(0);
    }
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
            ui_type: None,
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
            ui_type: None,
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
