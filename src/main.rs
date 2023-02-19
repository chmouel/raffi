use std::{
    io::Write,
    process::{Command, Stdio},
};

fn run_command_with_input(input: &[String]) -> String {
    let mut child = Command::new("wofi")
        .arg("-d -G -I --alow-images --allow-markup -W500 -H500 -i")
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .expect("cannot launch wofi command");

    // Write to stdin
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input.join("\n").as_bytes()).unwrap();
    }

    // Read stdout
    let output = child.wait_with_output().expect("failed to read output");
    // String::from_utf8_lossy(output.stdout).to_string()
    String::from_utf8(output.stdout).unwrap()
}

fn main() {
    let input = vec![
        String::from("one"),
        String::from("two"),
        String::from("three"),
    ];
    let output = run_command_with_input(&input);
    print!("{output}")
}
