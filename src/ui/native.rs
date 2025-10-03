use std::{borrow::Cow, collections::HashMap, sync::Arc};

use anyhow::Result;
use skim::prelude::*;

use super::UI;
use crate::{read_icon_map, RaffiConfig};

/// Native UI implementation using skim
pub struct NativeUI;

impl UI for NativeUI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
        run_native_ui(configs, no_icons)
    }
}

/// Custom item structure for skim that holds description and optional icon
#[derive(Debug, Clone)]
struct RaffiItem {
    description: String,
    icon_path: Option<String>,
}

impl SkimItem for RaffiItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.description)
    }

    fn display<'a>(&'a self, _context: DisplayContext<'a>) -> AnsiString<'a> {
        if self.icon_path.is_some() {
            // Display with icon indicator
            AnsiString::parse(&format!("🎯 {}", self.description))
        } else {
            AnsiString::parse(&self.description)
        }
    }
}

/// Run the native UI with the provided configurations and return the selected item.
fn run_native_ui(rafficonfigs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let icon_map = if no_icons {
        HashMap::new()
    } else {
        read_icon_map().unwrap_or_default()
    };

    let items: Vec<Arc<dyn SkimItem>> = rafficonfigs
        .iter()
        .map(|mc| {
            let description = mc
                .description
                .clone()
                .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));

            let icon_path = if !no_icons {
                let icon = mc
                    .icon
                    .clone()
                    .unwrap_or_else(|| mc.binary.clone().unwrap_or_else(|| "unknown".to_string()));
                icon_map.get(&icon).cloned()
            } else {
                None
            };

            Arc::new(RaffiItem {
                description,
                icon_path,
            }) as Arc<dyn SkimItem>
        })
        .collect();

    let options = SkimOptionsBuilder::default()
        .height("50%".to_string())
        .multi(false)
        .reverse(true)
        .prompt("❯ ".to_string())
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build skim options: {}", e))?;

    let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();
    for item in items {
        let _ = tx.send(item);
    }
    drop(tx);

    let output = Skim::run_with(&options, Some(rx))
        .ok_or_else(|| anyhow::anyhow!("Failed to run native UI"))?;

    if output.is_abort {
        anyhow::bail!("Selection aborted");
    }

    if let Some(selected) = output.selected_items.first() {
        Ok(selected.text().to_string())
    } else {
        anyhow::bail!("No item selected")
    }
}

