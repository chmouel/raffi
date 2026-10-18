use anyhow::Result;
use gumdrop::Options;
use raffi::{run, Args};

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if raffi::browser_host::is_native_host_invocation(&argv) {
        std::process::exit(raffi::browser_host::run_native_host(&argv));
    }
    let args: Args = Args::parse_args_default_or_exit();
    run(args)
}
