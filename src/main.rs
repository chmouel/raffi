use anyhow::Result;
use gumdrop::Options;
use raffi::{run, Args};

fn main() -> Result<()> {
    let args: Args = Args::parse_args_default_or_exit();
    run(args)
}
