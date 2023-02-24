use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    process::{Command, Stdio},
};

use gumdrop::Options;
use serde::Deserialize;
use serde_yaml::Value;

#[derive(Deserialize)]
struct RaffiConfig {
    binary: String,
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
    #[options(help = "config file location ")]
    configfile: Option<String>,
    #[options(help = "print command to stdout, do not run it")]
    print_only: bool,
}

fn get_icon_map() -> HashMap<String, String> {
    // create a hasmap of icon names and paths
    let mut icon_map = HashMap::new();
    let iconhome = std::env::var("XDG_DATA_HOME")
        .unwrap_or(format!("{}/.local/share", std::env::var("HOME").unwrap()))
        + "/icons";
    let icondirs = vec!["/usr/share/icons", "/usr/share/pixmaps", iconhome.as_str()];
    for dir in icondirs {
        for entry in walkdir::WalkDir::new(dir)
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
    }
    icon_map
}

// read configuration from a yaml according to the if conditions
fn read_config(filename: &str) -> Vec<RaffiConfig> {
    let file =
        File::open(filename).unwrap_or_else(|_| panic!("cannot open config file {filename}"));
    let config: Config = serde_yaml::from_reader(file).expect("cannot parse yaml");
    let mut rafficonfigs: Vec<RaffiConfig> = Vec::new();
    for (_, value) in config.toplevel {
        if value.is_mapping() {
            let mc: RaffiConfig = serde_yaml::from_value(value).unwrap();
            if let Some(disabled) = mc.disabled {
                if disabled {
                    continue;
                }
            }
            if let Some(ifenveq) = mc.ifenveq.clone() {
                if ifenveq.len() != 2 {
                    continue;
                }
                if std::env::var(&ifenveq[0]).unwrap_or("".to_string()) != ifenveq[1] {
                    continue;
                }
            }
            if let Some(ifenvset) = mc.ifenvset.clone() {
                if std::env::var(&ifenvset).is_ok() {
                    continue;
                }
            }

            if let Some(ifenvnotset) = mc.ifenvnotset.clone() {
                if std::env::var(&ifenvnotset).is_err() {
                    continue;
                }
            }

            if let Some(ifexist) = mc.ifexist.clone() {
                // first check if that binary exist
                if !std::path::Path::new(&ifexist).exists() {
                    let paths = std::env::var("PATH").unwrap();
                    let mut found = false;
                    for path in paths.split(':') {
                        if std::path::Path::new(&(path.to_string() + "/" + &ifexist)).exists() {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        continue;
                    }
                }
            }
            rafficonfigs.push(mc);
        }
    }
    rafficonfigs
}

fn run_wofi_with_input(input: String) -> String {
    let home = std::env::var("HOME").unwrap();
    let xdg_cache_home = std::env::var("XDG_CACHE_HOME").unwrap_or(format!("{home}/.cache"));
    let mut child = Command::new("wofi")
        .args([
            "-d",
            "-G",
            "-I",
            "-k",
            format!("{xdg_cache_home}/raffi/mru.cache").as_str(),
            "--allow-markup",
            "-W500",
            "-H500",
            "-i",
            "-p",
            "Raffi",
        ])
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

fn make_wofi_input(rafficonfigs: &Vec<RaffiConfig>) -> String {
    let mut ret = String::new();
    let icon_map = get_icon_map();
    for mc in rafficonfigs {
        let s = mc.description.clone().unwrap_or(mc.binary.clone());
        let icon = mc.icon.clone().unwrap_or(mc.binary.clone());
        let mut icon_path = icon_map
            .get(&icon)
            .unwrap_or(&"default".to_string())
            .to_string();
        if std::path::Path::new(&icon).exists() {
            icon_path = icon;
        }
        ret.push_str(&format!("img:{icon_path}:text:{s}\n",));
    }
    ret
}

fn main() {
    let args = Args::parse_args_default_or_exit();

    let home = std::env::var("HOME").unwrap();
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or(format!("{home}/.config"));
    let configfile = args
        .configfile
        .unwrap_or(xdg_config_home + "/raffi/raffi.yaml");
    let rafficonfigs = read_config(configfile.as_str());
    let inputs = make_wofi_input(&rafficonfigs);
    let ret = run_wofi_with_input(inputs);
    let chosen = ret.split(':').last().unwrap().trim();
    for mc in rafficonfigs {
        if mc.description.unwrap_or(mc.binary.clone()) == chosen {
            if args.print_only {
                // print the command to stdout with args
                println!("{} {}", mc.binary, mc.args.unwrap_or(vec![]).join(" "));
                return;
            }
            let mut child = Command::new(mc.binary)
                .args(mc.args.unwrap_or(vec![]))
                .spawn()
                .expect("cannot launch binary");
            child.wait().expect("cannot wait for child");
        }
    }
}
