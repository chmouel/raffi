use std::io::Cursor;

use anyhow::Result;
use skim::{
    prelude::{SkimItemReader, SkimOptionsBuilder},
    Skim,
};

use super::UI;
use crate::RaffiConfig;

/// TUI implementation using skim
pub struct TuiUI;

impl UI for TuiUI {
    fn show(&self, configs: &[RaffiConfig], _no_icons: bool) -> Result<String> {
        let input = make_skim_input(configs);
        run_skim_with_input(input)
    }
}

/// Create the input for skim based on the Raffi configurations.
fn make_skim_input(rafficonfigs: &[RaffiConfig]) -> String {
    let mut ret = String::new();
    for mc in rafficonfigs {
        let description = mc
            .description
            .clone()
            .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
        ret.push_str(&format!("{}\n", description));
    }
    ret
}

/// Run the skim command with the provided input and return its output.
fn run_skim_with_input(input: String) -> Result<String> {
    let options = SkimOptionsBuilder::default()
        .height("50%".to_string())
        .multi(false)
        .color(Some("dark".to_string()))
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;

    let item_reader = SkimItemReader::default();
    let items = item_reader.of_bufread(Cursor::new(input));

    let selected_items = Skim::run_with(&options, Some(items))
        .map(|out| out.selected_items)
        .unwrap_or_default();

    if let Some(item) = selected_items.first() {
        Ok(item.output().to_string())
    } else {
        Ok(String::new())
    }
}
