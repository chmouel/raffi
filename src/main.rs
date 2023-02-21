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
#[derive(Deserialize, Debug)]
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
    // add ~/.local/share/icons
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
        // check if extension finishes by png or svg
        if entry.path().to_str().unwrap().ends_with("png")
            || entry.path().to_str().unwrap().ends_with("svg")
        {
            icon_map.insert(fname, entry.path().to_str().unwrap().to_string());
        }
    }
    icon_map
}

// read configuration from a yaml file as argument
fn read_config(filename: &str) -> String {
    let file = File::open(filename).expect("file not found");
    let config: Config = serde_yaml::from_reader(file).expect("cannot parse yaml");
    let icon_map = get_icon_path();
    let mut ret = String::new();
    for (key, value) in config.toplevel {
        if value.is_mapping() {
            let mc: MounchConfig = serde_yaml::from_value(value).unwrap();
            let s = mc.description.unwrap_or(key);
            let icon = mc.icon.unwrap_or("default".to_string());
            let icon_path = icon_map
                .get(&icon)
                .unwrap_or(&"default".to_string())
                .to_string();
            ret.push_str(&format!("img:{icon_path}:text:{s}\n"));
        }
    }
    ret
}

fn run_rofi_with_input(input: String) -> String {
    let mut child = Command::new("wofi")
        .arg("-d -G -I --alow-images --allow-markup -W500 -H500 -i")
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("cannot launch wofi command");

    // Write to stdin
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input.as_bytes()).unwrap();
    }

    // Read stdout
    let output = child.wait_with_output().expect("failed to read output");
    // String::from_utf8_lossy(output.stdout).to_string()
    String::from_utf8(output.stdout).unwrap()
}

fn main() {
    let home = std::env::var("HOME").unwrap();
    let inputs = read_config(&(home + "/.config/mounch/mounch.yaml"));
    let ret = run_rofi_with_input(inputs);
    let chosen = "{:?}", ret.split(':').last().unwrap().trim();
}
