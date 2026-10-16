# Web Search Script Filter

A Raffi script filter that provides live DuckDuckGo autocomplete suggestions while opening results in any configured search engine.

## Requirements

- Python 3 (stdlib only — no pip dependencies)
- `xdg-open` or equivalent for opening URLs

## How It Works

The script calls the DuckDuckGo free autocomplete API (`duckduckgo.com/ac/`) to fetch suggestions for the typed query. Each suggestion is displayed as a Raffi item whose `arg` is the full search URL for the configured engine. The first item is always the raw query itself, so you can always search without waiting for suggestions.

With the preview pane open (`Ctrl+P`), the selected suggestion is looked up in the DuckDuckGo Instant Answer API (`api.duckduckgo.com`). When there is an answer, the preview shows its heading, summary, source link, and image. Otherwise it shows the URL that Enter opens. Raffi asks for this by running the script again with `RAFFI_PREVIEW=1` and the suggestion in `RAFFI_TITLE`. Add `dynamic_preview: false` to an entry to turn it off.

Multiple Raffi entries (one per search engine) call the same script, each passing a different engine name as the first argument. Raffi appends the user's query as the final argument automatically.

## Installation

```bash
mkdir -p ~/.config/raffi/scripts/search
cp scripts/search/search.py ~/.config/raffi/scripts/search/search.py
chmod +x ~/.config/raffi/scripts/search/search.py
```

## Configuration

Add entries to your `~/.config/raffi/raffi.yaml` under `addons.script_filters`:

```yaml
addons:
  script_filters:
    - name: "Search DuckDuckGo"
      keyword: "ddg"
      command: "python3"
      args: ["~/.config/raffi/scripts/search/search.py", "duckduckgo"]
      icon: "web-browser"
      action: "xdg-open {value}"
      secondary_action: "copy"

    - name: "Search Google"
      keyword: "g"
      command: "python3"
      args: ["~/.config/raffi/scripts/search/search.py", "google"]
      icon: "google"
      action: "xdg-open {value}"
      secondary_action: "copy"
```

### Available Engines

| Engine name  | URL opened                          |
|--------------|-------------------------------------|
| `google`     | google.com/search                   |
| `duckduckgo` | duckduckgo.com                      |
| `bing`       | bing.com/search                     |
| `brave`      | search.brave.com/search             |

## Adding a Custom Engine

Edit the `ENGINES` dict at the top of `search.py`:

```python
ENGINES = {
    # ... existing engines ...
    "myddg": "https://my.custom-engine.com/search?q={query}",
}
```

Then add a Raffi entry using `"myddg"` as the engine argument.

## Keyboard Shortcuts

- **Enter**: Open search URL in browser
- **Ctrl+Enter**: Copy URL to clipboard (with `secondary_action: "copy"`)

## Testing Manually

```bash
# With suggestions
python3 search.py google "rust programming"

# Empty query — returns empty items list
python3 search.py duckduckgo ""

# Unknown engine — exits with error JSON
python3 search.py badengine "test"

# Validate JSON output
python3 search.py google "rust async" | python3 -m json.tool

# Preview for one suggestion, as Raffi asks for it
RAFFI_PREVIEW=1 RAFFI_TITLE="rust programming" python3 search.py google | python3 -m json.tool

# Unit tests
python3 -m unittest test_search
```
