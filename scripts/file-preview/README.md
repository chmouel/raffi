# File Preview Script

This script draws the rich previews in the Raffi file browser: PDF pages, video thumbnails, album and ebook covers, archive listings, highlighted source code and tables, with metadata rows such as page count, duration or image dimensions.

Raffi carries a copy of the script inside its binary and runs it with `python3` when `preview_command` is unset, so you have nothing to install.

## Requirements

- Python 3, standard library only.
- Optionally, the tools listed below. The script looks for each one at run time and falls back to Python code or to Raffi's built-in preview when it is missing.

## Customising

To change the script, fetch a copy, edit it and point `preview_command` at it. Raffi upgrades will not update your copy.

```bash
mkdir -p ~/.config/raffi/scripts
curl -fsSL -o ~/.config/raffi/scripts/file-preview.py \
  https://raw.githubusercontent.com/chmouel/raffi/main/scripts/file-preview/file-preview.py
chmod +x ~/.config/raffi/scripts/file-preview.py
```

```yaml
addons:
  file_browser:
    preview_command: "~/.config/raffi/scripts/file-preview.py"
```

Set `preview_command: none` to turn rich previews off.

## Supported Files

| Files | Tool used | Without it |
|-------|-----------|------------|
| Folders | `eza --tree` or `lsd --tree` | Python tree, two levels deep |
| PNG, JPEG, GIF, WebP, BMP, ICO, SVG | Raffi draws them; `exiftool` adds camera rows | Dimensions and EXIF read from the file header |
| HEIC, AVIF, JXL, TIFF, PSD, RAW photos | `vipsthumbnail` or ImageMagick | Raffi's built-in preview |
| PDF | `pdftoppm`, `pdfinfo`, `pdftotext` | Text only, or Raffi's built-in preview |
| Video | `ffmpegthumbnailer` or `ffmpeg`, plus `ffprobe` | `mediainfo` text, or Raffi's built-in preview |
| Audio | `ffprobe` tags, `ffmpeg` for the embedded cover | `mediainfo` text, or Raffi's built-in preview |
| EPUB, CBZ | Python | |
| MOBI, AZW3, FB2 | `ebook-meta` (calibre) | Raffi's built-in preview |
| Markdown | `glow` or `gum format` | Highlighted source |
| JSON, JSON Lines, GeoJSON | `jq` | Python pretty printer with colours |
| CSV, TSV | Python | |
| HTML | `w3m`, `lynx` or `html2text` | Highlighted source |
| XML | `xmllint --format` | Highlighted source |
| tar, zip, jar, whl, apk | Python | |
| tar.zst, 7z, rar, iso, cpio | `7z`, `bsdtar` or `unrar` | Raffi's built-in preview |
| deb, rpm | Python reads the package header; `bsdtar` lists rpm files | |
| gz, bz2, xz, zst | Python decompresses the start of the file | |
| docx, odt, pptx, xlsx | `pandoc` for docx and odt | Python text extraction |
| SQLite databases | Python | |
| Fonts | `fc-scan`, ImageMagick for the sample | Raffi's built-in preview |
| Source code and text | `bat` | Plain text with line numbers |
| Other binary files | `file`, `hexyl` | Python hex dump |

Optional tools and their usual package names: `bat`, `eza` or `lsd`, `glow` or `gum`, `jq`, `poppler` or `poppler-utils` (pdftoppm, pdfinfo, pdftotext), `ffmpeg`, `ffmpegthumbnailer`, `mediainfo`, `calibre` (ebook-meta), `imagemagick`, `libvips` or `libvips-tools` (vipsthumbnail), `perl-image-exiftool` or `libimage-exiftool-perl` (exiftool), `w3m`, `libxml2` or `libxml2-utils` (xmllint), `7zip` or `p7zip`, `libarchive` or `libarchive-tools` (bsdtar), `unrar`, `pandoc`, `fontconfig` (fc-scan), `hexyl` and `file`.

## Output

The script answers in one of two ways, as Raffi expects from a preview command:

- A JSON preview object with a `badge`, ANSI coloured `text`, an `image`, `sections` (image and text blocks with headings) and `metadata` rows. Raffi adds the path, size, modification time and permissions rows itself.
- No output and exit status 1 when the file is a FIFO, socket or device, when it is empty, or when no preview could be made. Raffi then shows its built-in preview.

The script caps text at four screens (at least 200 lines) and gives every tool call a timeout, so it answers within Raffi's 3 second limit.

## Colours

Previews follow the Raffi theme. Raffi draws the 16 basic ANSI colours with the theme's palette and passes the theme to the script in `RAFFI_THEME` and `RAFFI_THEME_MODE`. The script then:

- picks a matching bat theme: `Catppuccin Mocha`, `Catppuccin Latte`, `Nord`, `Dracula` or `gruvbox-dark`. Themes bat does not ship (`dark`, `light`, `tokyo-night`, `rose-pine`, `kanagawa`) use bat's `ansi` theme, which takes its colours from the Raffi palette. An older bat without the matching theme also gets `ansi`.
- renders Markdown with the `dracula` or `tokyo-night` glow/gum style for those themes, and `light` or `dark` for the others.
- ignores your `BAT_THEME`, bat config theme, `LS_COLORS`, `LSCOLORS` and `EZA_COLORS`, so a terminal colour scheme does not leak into the launcher.

To keep a bat theme of your own inside Raffi, export `FILE_PREVIEW_BAT_THEME` in the environment Raffi starts from, or set it in a custom preview command:

```yaml
preview_command: "FILE_PREVIEW_BAT_THEME=Coldark-Dark ~/.config/raffi/scripts/file-preview.py"
```

Run from a terminal, without `RAFFI_THEME`, the script leaves your tool settings alone.

## Cache

The script keeps thumbnails, extracted covers and any preview that took more than a quarter of a second to build in `$XDG_CACHE_HOME/raffi/file-preview` (`~/.cache/raffi/file-preview` by default). The cache key includes the file path, modification time, size, pane size and theme, so editing a file gives it a new entry. Past 200 MiB or 2000 files, the script deletes the oldest entries.

To clear it, for example after installing a new tool:

```bash
rm -rf ~/.cache/raffi/file-preview
```

## Testing Manually

```bash
# Preview a file as Raffi would, with a 70x40 cell pane
RAFFI_PREVIEW=1 RAFFI_PREVIEW_COLUMNS=70 RAFFI_PREVIEW_LINES=40 \
  RAFFI_PREVIEW_WIDTH=700 RAFFI_PREVIEW_HEIGHT=800 \
  ./file-preview.py ~/Documents/report.pdf | python3 -m json.tool

# Show the ANSI text of a preview in the terminal
./file-preview.py file-preview.py | python3 -c 'import json, sys; print(json.load(sys.stdin)["text"])'

# Log tool calls, timeouts and cache hits to stderr
DEBUG=1 ./file-preview.py ~/Videos/clip.mp4

# Unit tests
python3 -m unittest -v test_file_preview
```

Without the `RAFFI_PREVIEW_*` variables the script uses the terminal size and an 800x600 pixel pane.
