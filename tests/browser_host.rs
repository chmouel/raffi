use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use raffi::browser_host::{list_browsers, read_frame, send_request, write_frame, Request};
use serde_json::{json, Value};

fn send(stdin: &mut impl Write, value: Value) {
    write_frame(stdin, &serde_json::to_vec(&value).unwrap()).unwrap();
}

fn receive(stdout: &mut impl Read) -> Value {
    serde_json::from_slice(&read_frame(stdout).unwrap().unwrap()).unwrap()
}

fn run_host(args: &[&str]) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let runtime = std::env::temp_dir().join(format!("raffi-host-it-{unique}"));
    std::fs::create_dir_all(&runtime).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_raffi"))
        .args(args)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    send(
        &mut stdin,
        json!({"id": 0, "method": "browser/init",
               "data": {"id": "x", "name": "UA", "engine": "chromium"}}),
    );
    assert_eq!(receive(&mut stdout), json!({"id": 0, "result": {}}));
    send(
        &mut stdin,
        json!({"method": "browser/tabs-changed",
               "data": [{"id": 3, "title": "Raffi", "url": "https://github.com/chmouel/raffi"}]}),
    );

    let dir = runtime.join("raffi/browsers");
    let socket: PathBuf = dir.join(format!("{}.sock", child.id()));
    let deadline = Instant::now() + Duration::from_secs(5);
    let browsers = loop {
        let browsers = list_browsers(&dir);
        if browsers
            .first()
            .is_some_and(|(_, listing)| !listing.tabs.is_empty())
            || Instant::now() > deadline
        {
            break browsers;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(browsers.len(), 1);
    assert_eq!(browsers[0].0, socket);
    assert_eq!(browsers[0].1.tabs[0].title, "Raffi");

    send_request(&socket, &Request::Focus { tab_id: 3 }).unwrap();
    assert_eq!(
        receive(&mut stdout),
        json!({"method": "browser/focus-tab", "data": {"tabId": 3}})
    );

    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert!(!socket.exists(), "the host removes its socket on exit");
    let _ = std::fs::remove_dir_all(runtime);
}

#[test]
fn chrome_style_invocation_runs_the_native_host() {
    run_host(&[
        "chrome-extension://kcmipingpfbohfjckomimmahknoddnke/",
        "--parent-window=0",
    ]);
}

#[test]
fn firefox_style_invocation_runs_the_native_host() {
    run_host(&[
        "/home/user/.mozilla/native-messaging-hosts/com.vicinae.vicinae.json",
        "firefox@vicinae.com",
    ]);
}
