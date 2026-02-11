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

/// Configuration for the currency addon
#[derive(Deserialize, Debug, Clone)]
pub struct CurrencyAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub currencies: Option<Vec<String>>,
    #[serde(default)]
    pub default_currency: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
}

impl Default for CurrencyAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            currencies: None,
            default_currency: None,
            trigger: None,
        }
    }
}

/// Configuration for the calculator addon
#[derive(Deserialize, Debug, Clone)]
pub struct CalculatorAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CalculatorAddonConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Configuration for the file browser addon
#[derive(Deserialize, Debug, Clone)]
pub struct FileBrowserAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub show_hidden: Option<bool>,
}

impl Default for FileBrowserAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_hidden: None,
        }
    }
}

/// Configuration for a script filter addon
#[derive(Deserialize, Debug, Clone)]
pub struct ScriptFilterConfig {
    pub name: String,
    pub command: String,
    pub keyword: String,
    pub icon: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub action: Option<String>,
    pub secondary_action: Option<String>,
}

/// Configuration for a web search addon
#[derive(Deserialize, Debug, Clone)]
pub struct WebSearchConfig {
    pub name: String,
    pub keyword: String,
    pub url: String,
    pub icon: Option<String>,
}

/// Container for all addon configurations
#[derive(Deserialize, Debug, Clone, Default)]
pub struct AddonsConfig {
    #[serde(default)]
    pub currency: CurrencyAddonConfig,
    #[serde(default)]
    pub calculator: CalculatorAddonConfig,
    #[serde(default)]
    pub file_browser: FileBrowserAddonConfig,
    #[serde(default)]
    pub script_filters: Vec<ScriptFilterConfig>,
    #[serde(default)]
    pub web_searches: Vec<WebSearchConfig>,
}

fn default_true() -> bool {
    true
}

/// Per-colour overrides for the native UI theme.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ThemeColorsConfig {
    pub bg_base: Option<String>,
    pub bg_input: Option<String>,
    pub accent: Option<String>,
    pub accent_hover: Option<String>,
    pub text_main: Option<String>,
    pub text_muted: Option<String>,
    pub selection_bg: Option<String>,
    pub border: Option<String>,
}

/// General configuration for persistent defaults
#[derive(Deserialize, Debug, Clone, Default)]
pub struct GeneralConfig {
    #[serde(default)]
    pub ui_type: Option<String>,
    #[serde(default)]
    pub default_script_shell: Option<String>,
    #[serde(default)]
    pub no_icons: Option<bool>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub theme_colors: Option<ThemeColorsConfig>,
}

/// Complete parsed configuration
pub struct ParsedConfig {
    pub general: GeneralConfig,
    pub addons: AddonsConfig,
    pub entries: Vec<RaffiConfig>,
}

/// Represents the top-level configuration structure.
#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    general: GeneralConfig,
    #[serde(default)]
    addons: AddonsConfig,
    #[serde(flatten)]
    entries: HashMap<String, Value>,
}

/// UI type selection
#[derive(Debug, Clone, PartialEq)]
pub enum UIType {
    Fuzzel,
    #[cfg(feature = "wayland")]
    Native,
}

/// Theme mode selection
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl std::str::FromStr for ThemeMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dark" => Ok(ThemeMode::Dark),
            "light" => Ok(ThemeMode::Light),
            _ => Err(format!(
                "Invalid theme: {}. Valid options are: dark, light",
                s
            )),
        }
    }
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
    #[options(help = "default shell when using scripts", short = "P")]
    pub default_script_shell: Option<String>,
    #[options(help = "UI type to use: fuzzel, native (default: fuzzel)", short = "u")]
    pub ui_type: Option<String>,
    #[options(help = "initial search query (native mode only)", short = "i")]
    pub initial_query: Option<String>,
    #[options(help = "theme: dark, light (default: dark)", short = "t")]
    pub theme: Option<String>,
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

/// Expand `~/` prefix to the user's HOME directory.
pub(crate) fn expand_tilde(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), stripped)
    } else {
        s.to_string()
    }
}

/// Expand `${VAR}` references to their environment variable values.
/// Unknown or unset variables expand to an empty string.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let var_name: String = chars.by_ref().take_while(|&c| c != '}').collect();
            result.push_str(&std::env::var(&var_name).unwrap_or_default());
        } else {
            result.push(c);
        }
    }
    result
}

/// Expand both `~/` and `${VAR}` in a config value.
pub(crate) fn expand_config_value(s: &str) -> String {
    expand_env_vars(&expand_tilde(s))
}

/// Read the configuration file and return a ParsedConfig.
pub fn read_config(filename: &str, args: &Args) -> Result<ParsedConfig> {
    let file = File::open(filename).context(format!("cannot open config file {filename}"))?;
    read_config_from_reader(file, args)
}

pub fn read_config_from_reader<R: Read>(reader: R, args: &Args) -> Result<ParsedConfig> {
    let config: Config = serde_yaml::from_reader(reader).context("cannot parse config")?;
    let mut rafficonfigs = Vec::new();

    for value in config.entries.values() {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value.clone())
                .context("cannot parse config entry".to_string())?;
            mc.binary = mc.binary.map(|s| expand_config_value(&s));
            mc.icon = mc.icon.map(|s| expand_config_value(&s));
            mc.ifexist = mc.ifexist.map(|s| expand_config_value(&s));
            mc.args = mc
                .args
                .map(|v| v.into_iter().map(|s| expand_config_value(&s)).collect());
            if mc.disabled.unwrap_or(false)
                || !is_valid_config(&mut mc, args, &DefaultEnvProvider, &DefaultBinaryChecker)
            {
                continue;
            }
            rafficonfigs.push(mc);
        }
    }

    let mut addons = config.addons;
    for sf in &mut addons.script_filters {
        sf.command = expand_config_value(&sf.command);
        sf.icon = sf.icon.as_ref().map(|s| expand_config_value(s));
        sf.action = sf.action.as_ref().map(|s| expand_config_value(s));
        sf.secondary_action = sf.secondary_action.as_ref().map(|s| expand_config_value(s));
    }
    for ws in &mut addons.web_searches {
        ws.url = expand_config_value(&ws.url);
        ws.icon = ws.icon.as_ref().map(|s| expand_config_value(s));
    }

    Ok(ParsedConfig {
        general: config.general,
        addons,
        entries: rafficonfigs,
    })
}

/// Validate the RaffiConfig based on various conditions.
fn is_valid_config(
    mc: &mut RaffiConfig,
    args: &Args,
    env_provider: &impl EnvProvider,
    binary_checker: &impl BinaryChecker,
) -> bool {
    if let Some(_script) = &mc.script {
        if !binary_checker.exists(
            mc.binary
                .as_deref()
                .unwrap_or(args.default_script_shell.as_deref().unwrap_or("bash")),
        ) {
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

/// Percent-encode a query string for use in URLs.
/// Encodes all characters except unreserved ones (A-Z, a-z, 0-9, '-', '.', '_', '~').
pub fn url_encode_query(query: &str) -> String {
    let mut encoded = String::with_capacity(query.len() * 3);
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                write!(encoded, "%{:02X}", byte).unwrap();
            }
        }
    }
    encoded
}

/// Build a web search URL by replacing `{query}` in the template with the percent-encoded query,
/// then open it with `xdg-open`.
pub fn execute_web_search_url(url_template: &str, query: &str) -> Result<()> {
    let encoded = url_encode_query(query);
    let url = url_template.replace("{query}", &encoded);
    Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .context("cannot open web search URL")?;
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

    let parsed_config = read_config(configfile, &args).context("Failed to read config")?;

    if parsed_config.entries.is_empty() {
        eprintln!("No valid configurations found in {configfile}");
        std::process::exit(1);
    }

    // Merge general config: CLI flags override config values
    let general = &parsed_config.general;
    let no_icons = args.no_icons || general.no_icons.unwrap_or(false);
    let ui_type_str = args.ui_type.as_ref().or(general.ui_type.as_ref());
    let default_script_shell = args
        .default_script_shell
        .as_deref()
        .or(general.default_script_shell.as_deref())
        .unwrap_or("bash")
        .to_string();

    // Determine theme
    let theme_str = args.theme.as_ref().or(general.theme.as_ref());
    let theme = if let Some(theme_str) = theme_str {
        theme_str
            .parse::<ThemeMode>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        ThemeMode::Dark
    };

    // Determine UI type
    let ui_type = if let Some(ui_type_str) = ui_type_str {
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
        .show(
            &parsed_config.entries,
            &parsed_config.addons,
            no_icons,
            args.initial_query.as_deref(),
            &theme,
            parsed_config.general.theme_colors.as_ref(),
        )
        .context("Failed to show UI")?;

    let chosen_name = chosen.trim();
    if chosen_name.is_empty() {
        std::process::exit(0);
    }
    let mc = parsed_config
        .entries
        .iter()
        .find(|mc| {
            mc.description.as_deref() == Some(chosen_name)
                || mc.binary.as_deref() == Some(chosen_name)
        })
        .context("No matching configuration found")?;

    let interpreter = if mc.script.is_some() {
        mc.binary.as_deref().unwrap_or(&default_script_shell)
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
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(parsed_config.entries.len(), 2);

        // Addons should default to enabled
        assert!(parsed_config.addons.currency.enabled);
        assert!(parsed_config.addons.calculator.enabled);

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
            assert!(parsed_config.entries.contains(expected_config));
        }
    }

    #[test]
    fn test_addons_config_parsing() {
        let yaml_config = r#"
        addons:
          currency:
            enabled: true
            currencies: ["USD", "EUR", "GBP"]
          calculator:
            enabled: false
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.addons.currency.enabled);
        assert!(!parsed_config.addons.calculator.enabled);
        assert_eq!(
            parsed_config.addons.currency.currencies,
            Some(vec![
                "USD".to_string(),
                "EUR".to_string(),
                "GBP".to_string()
            ])
        );
        assert_eq!(parsed_config.entries.len(), 1);
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
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
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

    #[test]
    fn test_script_filter_action_parsing() {
        let yaml_config = r#"
        addons:
          script_filters:
            - name: "Bookmarks"
              keyword: "bm"
              command: "my-bookmark-script"
              args: ["-j"]
              action: "wl-copy {value}"
              secondary_action: "xdg-open {value}"
            - name: "Timezones"
              keyword: "tz"
              command: "batz"
              args: ["-j"]
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.script_filters.len(), 2);

        let bm = &parsed_config.addons.script_filters[0];
        assert_eq!(bm.name, "Bookmarks");
        assert_eq!(bm.action, Some("wl-copy {value}".to_string()));
        assert_eq!(bm.secondary_action, Some("xdg-open {value}".to_string()));

        let tz = &parsed_config.addons.script_filters[1];
        assert_eq!(tz.name, "Timezones");
        assert_eq!(tz.action, None);
        assert_eq!(tz.secondary_action, None);
    }

    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), format!("{home}/foo/bar"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_expand_env_vars() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_env_vars("${HOME}/foo"), format!("{home}/foo"));
    }

    #[test]
    fn test_expand_env_vars_unknown() {
        assert_eq!(expand_env_vars("${NONEXISTENT_VAR_12345}/foo"), "/foo");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        let home = std::env::var("HOME").unwrap();
        let user = std::env::var("USER").unwrap_or_default();
        assert_eq!(
            expand_env_vars("${HOME}/stuff/${USER}/data"),
            format!("{home}/stuff/{user}/data")
        );
    }

    #[test]
    fn test_expand_env_vars_no_vars() {
        assert_eq!(expand_env_vars("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_env_vars("plain text"), "plain text");
    }

    #[test]
    fn test_expand_config_value_combined() {
        let home = std::env::var("HOME").unwrap();
        let user = std::env::var("USER").unwrap_or_default();
        assert_eq!(
            expand_config_value("~/foo/${USER}/bar"),
            format!("{home}/foo/{user}/bar")
        );
    }

    #[test]
    fn test_config_expands_env_vars_in_fields() {
        let home = std::env::var("HOME").unwrap();
        let yaml_config = r#"
        myapp:
          binary: "${HOME}/bin/myapp"
          description: "My App"
          args: ["${HOME}/Downloads/file.txt", "--verbose"]
          icon: "${HOME}/icons/myapp.png"
          ifexist: "${HOME}/bin/myapp"
        "#;
        let reader = Cursor::new(yaml_config);
        let config: super::Config = serde_yaml::from_reader(reader).expect("cannot parse config");
        let mut rafficonfigs = Vec::new();
        for value in config.entries.values() {
            if value.is_mapping() {
                let mut mc: RaffiConfig = serde_yaml::from_value(value.clone()).unwrap();
                mc.binary = mc.binary.map(|s| expand_config_value(&s));
                mc.icon = mc.icon.map(|s| expand_config_value(&s));
                mc.ifexist = mc.ifexist.map(|s| expand_config_value(&s));
                mc.args = mc
                    .args
                    .map(|v| v.into_iter().map(|s| expand_config_value(&s)).collect());
                rafficonfigs.push(mc);
            }
        }

        assert_eq!(rafficonfigs.len(), 1);
        let mc = &rafficonfigs[0];
        assert_eq!(mc.binary, Some(format!("{home}/bin/myapp")));
        assert_eq!(mc.icon, Some(format!("{home}/icons/myapp.png")));
        assert_eq!(mc.ifexist, Some(format!("{home}/bin/myapp")));
        assert_eq!(
            mc.args,
            Some(vec![
                format!("{home}/Downloads/file.txt"),
                "--verbose".to_string()
            ])
        );
    }

    #[test]
    fn test_config_expands_tilde_in_fields() {
        let home = std::env::var("HOME").unwrap();
        let yaml_config = r#"
        myapp:
          binary: "~/bin/myapp"
          description: "My App"
          args: ["~/Downloads/file.txt", "--verbose"]
          icon: "~/icons/myapp.png"
          ifexist: "~/bin/myapp"
        "#;
        let reader = Cursor::new(yaml_config);
        // Parse and expand manually to avoid is_valid_config filtering out non-existent paths
        let config: super::Config = serde_yaml::from_reader(reader).expect("cannot parse config");
        let mut rafficonfigs = Vec::new();
        for value in config.entries.values() {
            if value.is_mapping() {
                let mut mc: RaffiConfig = serde_yaml::from_value(value.clone()).unwrap();
                mc.binary = mc.binary.map(|s| expand_tilde(&s));
                mc.icon = mc.icon.map(|s| expand_tilde(&s));
                mc.ifexist = mc.ifexist.map(|s| expand_tilde(&s));
                mc.args = mc
                    .args
                    .map(|v| v.into_iter().map(|s| expand_tilde(&s)).collect());
                rafficonfigs.push(mc);
            }
        }

        assert_eq!(rafficonfigs.len(), 1);
        let mc = &rafficonfigs[0];
        assert_eq!(mc.binary, Some(format!("{home}/bin/myapp")));
        assert_eq!(mc.icon, Some(format!("{home}/icons/myapp.png")));
        assert_eq!(mc.ifexist, Some(format!("{home}/bin/myapp")));
        assert_eq!(
            mc.args,
            Some(vec![
                format!("{home}/Downloads/file.txt"),
                "--verbose".to_string()
            ])
        );
    }

    #[test]
    fn test_general_config_parsing() {
        let yaml_config = r#"
        general:
          ui_type: native
          default_script_shell: zsh
          no_icons: true
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.general.ui_type, Some("native".to_string()));
        assert_eq!(
            parsed_config.general.default_script_shell,
            Some("zsh".to_string())
        );
        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_config_without_general_section() {
        let yaml_config = r#"
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.general.ui_type.is_none());
        assert!(parsed_config.general.default_script_shell.is_none());
        assert!(parsed_config.general.no_icons.is_none());
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_partial_general_config() {
        let yaml_config = r#"
        general:
          no_icons: true
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.general.ui_type.is_none());
        assert!(parsed_config.general.default_script_shell.is_none());
        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_general_config_theme_parsing() {
        let yaml_config = r#"
        general:
          theme: light
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(parsed_config.general.theme, Some("light".to_string()));
    }

    #[test]
    fn test_theme_mode_from_str() {
        assert_eq!("dark".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("Dark".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("DARK".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("light".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert_eq!("Light".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert_eq!("LIGHT".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert!("invalid".parse::<ThemeMode>().is_err());
    }

    #[test]
    fn test_url_encode_query() {
        assert_eq!(url_encode_query("hello"), "hello");
        assert_eq!(url_encode_query("hello world"), "hello%20world");
        assert_eq!(url_encode_query("rust traits"), "rust%20traits");
        assert_eq!(url_encode_query("a+b"), "a%2Bb");
        assert_eq!(url_encode_query("foo&bar=baz"), "foo%26bar%3Dbaz");
        assert_eq!(url_encode_query(""), "");
        assert_eq!(url_encode_query("A-Z_0.9~"), "A-Z_0.9~");
    }

    #[test]
    fn test_web_search_config_parsing() {
        let yaml_config = r#"
        addons:
          web_searches:
            - name: "Google"
              keyword: "g"
              url: "https://google.com/search?q={query}"
              icon: "google"
            - name: "DuckDuckGo"
              keyword: "ddg"
              url: "https://duckduckgo.com/?q={query}"
        firefox:
          binary: firefox
          description: "Firefox browser"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            initial_query: None,
            theme: None,
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.web_searches.len(), 2);

        let google = &parsed_config.addons.web_searches[0];
        assert_eq!(google.name, "Google");
        assert_eq!(google.keyword, "g");
        assert_eq!(google.url, "https://google.com/search?q={query}");
        assert_eq!(google.icon, Some("google".to_string()));

        let ddg = &parsed_config.addons.web_searches[1];
        assert_eq!(ddg.name, "DuckDuckGo");
        assert_eq!(ddg.keyword, "ddg");
        assert_eq!(ddg.url, "https://duckduckgo.com/?q={query}");
        assert!(ddg.icon.is_none());
    }
}
