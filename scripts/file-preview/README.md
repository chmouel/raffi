# File Preview Script

A preview command for the Raffi file browser. It shows PDF pages, video thumbnails, album covers, ebook covers, archive listings, highlighted source code, tables and more in the preview pane, with a few metadata rows such as page count, duration or image dimensions.

## Requirements

- Python 3 (stdlib only, no pip dependencies)
- Any of the optional tools listed below. Each one is detected at run time, and the script falls back to a Python implementation or to Raffi's built-in preview when it is missing.

## Installation

```bash
mkdir -p ~/.config/raffi/scripts/file-preview
cp scripts/file-preview/file-preview.py ~/.config/raffi/scripts/file-preview/
chmod +x ~/.config/raffi/scripts/file-preview/file-preview.py
```

## Configuration

```yaml
addons:
  file_browser:
    preview_command: "~/.config/raffi/scripts/file-preview/file-preview.py"
```

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

Syntax highlighting uses your `bat` configuration, including `BAT_THEME`.

Optional tools and their usual package names: `bat`, `eza` or `lsd`, `glow` or `gum`, `jq`, `poppler` or `poppler-utils` (pdftoppm, pdfinfo, pdftotext), `ffmpeg`, `ffmpegthumbnailer`, `mediainfo`, `calibre` (ebook-meta), `imagemagick`, `libvips` or `libvips-tools` (vipsthumbnail), `perl-image-exiftool` or `libimage-exiftool-perl` (exiftool), `w3m`, `libxml2` or `libxml2-utils` (xmllint), `7zip` or `p7zip`, `libarchive` or `libarchive-tools` (bsdtar), `unrar`, `pandoc`, `fontconfig` (fc-scan), `hexyl` and `file`.

## Output

The script answers in one of two ways, as Raffi expects from a preview command:

- A JSON preview object with a `badge`, ANSI coloured `text`, an `image`, `sections` (image and text blocks with headings) and `metadata` rows. Raffi adds the path, size, modification time and permissions rows itself.
- No output and exit status 1 when the file is a FIFO, socket or device, when it is empty, or when no preview could be made. Raffi then shows its built-in preview.

Text is capped at four screens (at least 200 lines), and every tool call has a timeout so the script answers well within Raffi's 3 second limit.

## Cache

Thumbnails, extracted covers and previews that took more than a quarter of a second to build are stored in `$XDG_CACHE_HOME/raffi/file-preview` (`~/.cache/raffi/file-preview` by default). Entries are keyed on the file path, modification time, size and pane size, so editing a file creates a new entry. The oldest entries are removed once the cache grows past 200 MiB or 2000 files.

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
