use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    process::{Command, Stdio},
};

use serde::Deserialize;
use serde_yaml::Value;

#[derive(Deserialize)]
struct MounchConfig {
    binary: String,
    args: Option<Vec<String>>,
    icon: Option<String>,
    description: Option<String>,
}
#[derive(Deserialize)]
struct Config {
    #[serde(flatten)]
    toplevel: HashMap<String, Value>,
}

fn get_icon_path() -> HashMap<String, String> {
    // create a hasmap of icon names and paths
    let mut icon_map = HashMap::new();
    for entry in walkdir::WalkDir::new("/usr/share/icons")
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let fname = entry
            .file_name()
            .to_str()
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_string();
        if entry.path().to_str().unwrap().ends_with("png")
            || entry.path().to_str().unwrap().ends_with("svg")
        {
            icon_map.insert(fname, entry.path().to_str().unwrap().to_string());
        }
    }
    let home = std::env::var("HOME").unwrap();
    for entry in walkdir::WalkDir::new(home + "/.local/share/icons")
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let fname = entry
            .file_name()
            .to_str()
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_string();
        if entry.path().to_str().unwrap().ends_with("png")
            || entry.path().to_str().unwrap().ends_with("svg")
        {
            icon_map.insert(fname, entry.path().to_str().unwrap().to_string());
        }
    }
    icon_map
}

// read configuration from a yaml file as argument
fn read_config(filename: &str) -> Vec<MounchConfig> {
    let file = File::open(filename).expect("file not found");
    let config: Config = serde_yaml::from_reader(file).expect("cannot parse yaml");
    let mut mounchconfigs: Vec<MounchConfig> = Vec::new();
    for (_, value) in config.toplevel {
        if value.is_mapping() {
            mounchconfigs.push(serde_yaml::from_value(value).unwrap());
        }
    }
    mounchconfigs
}

fn run_rofi_with_input(input: String) -> String {
    let home = std::env::var("HOME").unwrap();
    let xdg_cache_home = std::env::var("XDG_CACHE_HOME").unwrap_or(format!("{home}/.cache"));
    let mut child = Command::new("wofi")
        .arg(format!(
            "-d -G -I -k {xdg_cache_home}/raffi --alow-images --allow-markup -W500 -H500 -i"
        ))
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cannot launch wofi command");

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().expect("failed to read output");
    String::from_utf8(output.stdout).unwrap()
}

fn make_wofi_input(mounchconfigs: &Vec<MounchConfig>) -> String {
    let mut ret = String::new();
    let icon_map = get_icon_path();
    for mc in mounchconfigs {
        let s = mc.description.clone().unwrap_or(mc.binary.clone());
        let icon = mc.icon.clone().unwrap_or("default".to_string());
        let icon_path = icon_map
            .get(&icon)
            .unwrap_or(&"default".to_string())
            .to_string();
        ret.push_str(&format!("img:{icon_path}:text:{s}\n",));
    }
    ret
}

fn main() {
    let home = std::env::var("HOME").unwrap();
    let mounchconfigs = read_config(&(home + "/.config/mounch/mounch.yaml"));
    let inputs = make_wofi_input(&mounchconfigs);
    let ret = run_rofi_with_input(inputs);
    let chosen = ret.split(':').last().unwrap().trim();
    // match chosen in mounchconfigs
    for mc in mounchconfigs {
        if mc.description.unwrap_or(mc.binary.clone()) == chosen {
            let mut child = Command::new(mc.binary)
                .args(mc.args.unwrap_or(vec![]))
                .spawn()
                .expect("cannot launch binary");
            child.wait().expect("cannot wait for child");
        }
    }
}
