import importlib.util
import io
import json
import os
import re
import sqlite3
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
import zipfile
import zlib
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "file-preview.py"
spec = importlib.util.spec_from_file_location("file_preview", SCRIPT)
fp = importlib.util.module_from_spec(spec)
sys.modules["file_preview"] = fp
spec.loader.exec_module(fp)

PREVIEW_KEYS = {
    "title",
    "subtitle",
    "badge",
    "icon",
    "text",
    "image",
    "sections",
    "metadata",
}
SGR = re.compile(r"\x1b\[[0-9;]*m")
PANE = fp.Pane(columns=70, lines=10, width=400, height=300)


def plain(text: str) -> str:
    return SGR.sub("", text)


def png_bytes(width: int, height: int) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\0" + b"\xff\x00\x00" * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


class Stdout:
    """sys.stdout stand-in: main() writes UTF-8 bytes to stdout.buffer."""

    def __init__(self):
        self.buffer = io.BytesIO()

    def flush(self):
        pass


def metadata(preview: dict) -> dict:
    return dict(preview.get("metadata", []))


class PreviewTestCase(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        cache = patch.object(fp, "CACHE_DIR", self.dir / "cache")
        cache.start()
        self.addCleanup(cache.stop)

    def write(self, name: str, data: bytes | str) -> Path:
        path = self.dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(data, str):
            path.write_text(data)
        else:
            path.write_bytes(data)
        return path

    def main(self, path: Path) -> tuple[int, str]:
        stdout = Stdout()
        env = {"RAFFI_PREVIEW_COLUMNS": "70", "RAFFI_PREVIEW_LINES": "10"}
        with patch.dict(os.environ, env), patch.object(sys, "stdout", stdout):
            code = fp.main([str(path)])
        return code, stdout.buffer.getvalue().decode()


class DispatchTests(unittest.TestCase):
    def test_extension_and_multi_extension(self):
        cases = {
            "photo.JPG": fp.image_preview,
            "scan.heic": fp.image_preview,
            "backup.tar.gz": fp.tar_preview,
            "backup.tgz": fp.tar_preview,
            "dump.json.gz": fp.compressed_preview,
            "notes.txt.xz": fp.compressed_preview,
            "report.pdf": fp.pdf_preview,
            "main.rs": fp.text_preview,
            "README.md": fp.markdown_preview,
            "data.jsonl": fp.json_preview,
            "pkg.deb": fp.deb_preview,
            "app.ts": fp.ts_preview,
        }
        for name, handler in cases.items():
            with self.subTest(name=name):
                self.assertIs(fp.handler_for_name(name), handler)
        self.assertIsNone(fp.handler_for_name("Makefile"))
        self.assertIsNone(fp.handler_for_name("archive.unknownext"))

    def test_mime_prefixes(self):
        self.assertIs(fp.handler_for_mime("image/x-portable-pixmap"), fp.image_preview)
        self.assertIs(fp.handler_for_mime("text/x-c"), fp.text_preview)
        self.assertIs(fp.handler_for_mime("application/pdf"), fp.pdf_preview)
        self.assertIsNone(fp.handler_for_mime("application/octet-stream"))


class ContractTests(PreviewTestCase):
    def assert_contract(self, raw: str) -> dict:
        preview = json.loads(raw)
        self.assertIsInstance(preview, dict)
        self.assertNotIn("items", preview)
        self.assertLessEqual(set(preview), PREVIEW_KEYS)
        for row in preview.get("metadata", []):
            self.assertIsInstance(row, list)
            self.assertEqual(len(row), 2)
            self.assertTrue(all(isinstance(v, str) for v in row), row)
        for section in preview.get("sections", []):
            self.assertIn(section["type"], ("text", "image", "file"))
        return preview

    def test_main_outputs_valid_preview_objects(self):
        with zipfile.ZipFile(self.dir / "a.zip", "w") as z:
            z.writestr("hello.txt", "hello")
        db = sqlite3.connect(self.dir / "a.sqlite")
        db.execute("CREATE TABLE t (x)")
        db.commit()
        db.close()
        paths = [
            self.write("tiny.png", png_bytes(3, 2)),
            self.write("data.csv", "a,b\n1,2\n"),
            self.write("data.json", '{"a": [1, true, null]}'),
            self.write("blob.bin", bytes(range(256))),
            self.write("main.py", "print('hi')\n"),
            self.dir / "a.zip",
            self.dir / "a.sqlite",
            self.dir / "cache",
        ]
        (self.dir / "cache").mkdir(exist_ok=True)
        with patch("shutil.which", return_value=None):
            for path in paths:
                with self.subTest(path=path.name):
                    code, out = self.main(path)
                    self.assertEqual(code, 0)
                    self.assert_contract(out)

    def test_native_png_reports_dimensions(self):
        path = self.write("tiny.png", png_bytes(3, 2))
        code, out = self.main(path)
        preview = self.assert_contract(out)
        self.assertEqual(code, 0)
        self.assertEqual(preview["image"], str(path))
        self.assertEqual(preview["badge"], "Image")
        self.assertEqual(metadata(preview)["Dimensions"], "3 × 2")

    def test_emit_keeps_only_sgr_escapes_and_known_keys(self):
        stdout = Stdout()
        text = (
            "\x1b[1mbold\x1b[0m \x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\ \x1b[2Jdone"
        )
        with patch.object(sys, "stdout", stdout):
            self.assertEqual(fp.emit({"badge": "X", "text": text, "extra": 1}), 0)
        preview = json.loads(stdout.buffer.getvalue())
        self.assertEqual(
            preview, {"badge": "X", "text": "\x1b[1mbold\x1b[0m link done"}
        )

    def test_empty_preview_means_no_preview(self):
        self.assertEqual(fp.emit(None), 1)
        self.assertEqual(fp.emit({"badge": "X"}), 1)
        self.assertEqual(fp.emit("  \n"), 1)


class FileSystemTests(PreviewTestCase):
    def test_missing_path_exits_1(self):
        err = io.StringIO()
        with redirect_stderr(err):
            self.assertEqual(fp.main([str(self.dir / "nope")]), 1)
        self.assertIn("nope", err.getvalue())

    def test_empty_file_exits_1(self):
        self.assertEqual(self.main(self.write("empty", b""))[0], 1)

    @unittest.skipUnless(hasattr(os, "mkfifo"), "needs FIFOs")
    def test_fifo_is_never_opened(self):
        fifo = self.dir / "pipe"
        os.mkfifo(fifo)
        start = time.monotonic()
        # Opening a FIFO with no writer blocks: a hang here means it was opened.
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), str(fifo)],
            capture_output=True,
            timeout=5,
            check=False,
        )
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(proc.stdout, b"")
        self.assertLess(time.monotonic() - start, 3)

    def test_directory_tree_without_tools(self):
        for name in ("src/main.rs", "src/lib.rs", ".git/HEAD", "README.md"):
            self.write(f"repo/{name}", "x")
        with patch("shutil.which", return_value=None):
            preview = fp.directory_preview(self.dir / "repo", PANE)
        lines = plain(preview["text"]).splitlines()
        self.assertEqual(lines[:3], ["├── src", "│   ├── lib.rs", "│   └── main.rs"])
        self.assertEqual(lines[-1], "└── README.md")
        self.assertNotIn(".git", preview["text"])
        self.assertEqual(metadata(preview)["Items"], "2 folders, 1 file")


class TextTests(PreviewTestCase):
    def test_long_text_is_capped_with_marker(self):
        path = self.write("log.txt", "".join(f"line {n}\n" for n in range(500)))
        with patch("shutil.which", return_value=None):
            preview = fp.text_preview(path, PANE)
        lines = plain(preview["text"]).splitlines()
        self.assertEqual(len(lines), PANE.max_lines + 1)
        self.assertEqual(lines[-1], "… 300 more lines")
        self.assertEqual(metadata(preview)["Lines"], "500")

    def test_csv_columns_are_aligned(self):
        path = self.write(
            "people.csv", "name,age,city\nAda,36,London\nLinus,7,Helsinki\n"
        )
        preview = fp.csv_preview(path, PANE)
        lines = plain(preview["text"]).splitlines()
        self.assertEqual(
            lines,
            [
                "name  │ age │ city",
                "──────┼─────┼─────────",
                "Ada   │  36 │ London",
                "Linus │   7 │ Helsinki",
            ],
        )
        self.assertEqual(metadata(preview)["Rows"], "2")
        self.assertEqual(metadata(preview)["Columns"], "3")

    def test_builtin_json_colorizer(self):
        line = '  "key": "value", "n": -1.5e3, "ok": true, "none": null'
        colored = fp.colorize_json_line(line)
        self.assertEqual(plain(colored), line)
        self.assertIn(fp.sgr('"key"', fp.KEY_COLOR) + ":", colored)
        self.assertIn(fp.sgr('"value"', fp.STRING_COLOR), colored)
        self.assertIn(fp.sgr("-1.5e3", fp.NUMBER_COLOR), colored)
        self.assertIn(fp.sgr("true", fp.LITERAL_COLOR), colored)

    def test_json_is_pretty_printed_without_tools(self):
        path = self.write("data.json", '{"name": "raffi", "tags": [1, 2]}')
        with patch("shutil.which", return_value=None):
            preview = fp.json_preview(path, PANE)
        expected = json.dumps({"name": "raffi", "tags": [1, 2]}, indent=2)
        self.assertEqual(plain(preview["text"]), expected)
        self.assertEqual(metadata(preview)["Structure"], "Object, 2 keys")

    def test_invalid_json_falls_back_to_raw_text(self):
        path = self.write("broken.json", '{"name": ')
        with patch("shutil.which", return_value=None):
            preview = fp.json_preview(path, PANE)
        self.assertEqual(preview["badge"], "JSON")
        self.assertIn('{"name":', plain(preview["text"]))
        self.assertEqual(metadata(preview)["Lines"], "1")

    def test_typescript_is_not_mistaken_for_video(self):
        path = self.write("app.ts", "export const x: number = 1;\n")
        with patch("shutil.which", return_value=None):
            preview = fp.ts_preview(path, PANE)
        self.assertEqual(preview["badge"], "TypeScript")

    def test_gzipped_text_is_decompressed(self):
        import gzip

        path = self.write("notes.md.gz", gzip.compress(b"# Title\n\nBody\n"))
        with patch("shutil.which", return_value=None):
            preview = fp.compressed_preview(path, PANE)
        self.assertIn("# Title", plain(preview["text"]))
        self.assertEqual(metadata(preview)["Compression"], "gzip")


class BinaryTests(PreviewTestCase):
    def test_hexdump_fallback(self):
        path = self.write("blob.bin", b"\x7fELF\x00\x01" + bytes(range(200, 256)))
        with patch("shutil.which", return_value=None):
            preview = fp.text_preview(path, PANE)
        self.assertEqual(preview["badge"], "Binary")
        section = preview["sections"][-1]
        self.assertEqual(section["heading"], "First 62 bytes")
        first = plain(section["content"]).splitlines()[0]
        self.assertEqual(first, "00000000  7f 45 4c 46 00 01 c8 c9  •ELF⋄•××")

    def test_hexdump_squeezes_repeated_rows(self):
        dump = plain(fp.hexdump(bytes(64) + b"end", 80)).splitlines()
        self.assertEqual(dump[1], "*")
        self.assertTrue(dump[-1].startswith("00000040  65 6e 64"))


class ArchiveTests(PreviewTestCase):
    def test_zip_listing_with_stdlib(self):
        path = self.dir / "bundle.zip"
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
            z.writestr("docs/", "")
            z.writestr("docs/readme.txt", "a" * 3000)
        with patch("shutil.which", return_value=None):
            preview = fp.zip_preview(path, PANE)
        self.assertEqual(
            plain(preview["text"]).splitlines(),
            ["        docs/", "  2.9K  docs/readme.txt"],
        )
        meta = metadata(preview)
        self.assertEqual(meta["Entries"], "1 file, 1 folder")
        self.assertEqual(meta["Uncompressed"], "2.9 KiB")
        self.assertIn("% of original", meta["Compressed"])

    def test_tar_gz_listing_with_stdlib(self):
        path = self.dir / "release.tar.gz"
        with tarfile.open(path, "w:gz") as tf:
            folder = tarfile.TarInfo("./bin")
            folder.type = tarfile.DIRTYPE
            tf.addfile(folder)
            data = b"#!/bin/sh\n"
            info = tarfile.TarInfo("./bin/tool")
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))
            link = tarfile.TarInfo("./bin/alias")
            link.type, link.linkname = tarfile.SYMTYPE, "tool"
            tf.addfile(link)
        with patch("shutil.which", return_value=None):
            preview = fp.handler_for_name(path.name)(path, PANE)
        self.assertEqual(
            plain(preview["text"]).splitlines(),
            ["        bin/", "    10  bin/tool", "        bin/alias → tool"],
        )
        self.assertEqual(metadata(preview)["Entries"], "2 files, 1 folder")


class MediaTests(PreviewTestCase):
    def test_cache_key_changes_with_mtime_and_width(self):
        path = self.write("a.pdf", b"%PDF-1.4")
        key = fp.cache_key(path, 400)
        self.assertEqual(key, fp.cache_key(path, 400))
        self.assertNotEqual(key, fp.cache_key(path, 800))
        st = path.stat()
        os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))
        self.assertNotEqual(key, fp.cache_key(path, 400))

    def test_cached_image_is_reused(self):
        path = self.write("video.mp4", b"data")
        calls = []

        def make(out):
            calls.append(out)
            out.write_bytes(png_bytes(1, 1))
            return True

        first = fp.cached_image(path, PANE, ".jpg", make)
        second = fp.cached_image(path, PANE, ".jpg", make)
        self.assertEqual(first, second)
        # The tool wrote a PNG to a .jpg name: the cache keeps the real type.
        self.assertTrue(first.endswith(".png"))
        self.assertEqual(len(calls), 1)

    def test_slow_previews_are_cached(self):
        path = self.write("huge.tar.xz", b"data")
        calls = []

        def slow_handler(path, pane):
            calls.append(path)
            return {"badge": "Archive", "text": "listing"}

        with (
            patch.object(fp, "SLOW_PREVIEW", -1),
            patch.object(fp, "choose_handler", return_value=slow_handler),
        ):
            first = self.main(path)
            second = self.main(path)
        self.assertEqual(first, second)
        self.assertEqual(len(calls), 1)

    def test_pdf_preview_with_poppler(self):
        path = self.write("report.pdf", b"%PDF-1.7")

        def fake_run(cmd, timeout=2.0, input=None, env=None):
            if cmd[0] == "pdfinfo":
                return "Title:          Quarterly\nPages:          3\nPage size:      595 x 842 pts (A4)\n"
            if cmd[0] == "pdftoppm":
                Path(cmd[-1] + ".jpg").write_bytes(b"\xff\xd8\xff\xe0 fake jpeg")
                return ""
            if cmd[0] == "pdftotext":
                return "Quarterly report\n\n\n\nRevenue up\n"
            return None

        with patch.object(fp, "run", side_effect=fake_run):
            preview = fp.pdf_preview(path, PANE)
        self.assertEqual(preview["badge"], "PDF")
        image, text = preview["sections"]
        self.assertEqual(image["type"], "image")
        self.assertTrue(Path(image["path"]).is_file())
        self.assertEqual(Path(image["path"]).parent, fp.CACHE_DIR)
        self.assertEqual(
            text,
            {
                "type": "text",
                "heading": "Page 1 text",
                "content": "Quarterly report\n\nRevenue up",
            },
        )
        meta = metadata(preview)
        self.assertEqual(meta["Pages"], "3")
        self.assertEqual(meta["Title"], "Quarterly")
        self.assertEqual(meta["Page size"], "210 × 297 mm (A4)")

    def test_epub_cover_and_metadata_with_stdlib(self):
        path = self.dir / "book.epub"
        opf = """<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" xmlns:dc="http://purl.org/dc/elements/1.1/" version="3.0">
  <metadata>
    <dc:title>Dune</dc:title><dc:creator>Frank Herbert</dc:creator>
    <dc:language>en</dc:language><dc:description>&lt;p&gt;Spice.&lt;/p&gt;</dc:description>
  </metadata>
  <manifest><item id="c" href="images/cover.png" media-type="image/png" properties="cover-image"/></manifest>
</package>"""
        container = """<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"""
        with zipfile.ZipFile(path, "w") as z:
            z.writestr("mimetype", "application/epub+zip")
            z.writestr("META-INF/container.xml", container)
            z.writestr("OEBPS/content.opf", opf)
            z.writestr("OEBPS/images/cover.png", png_bytes(2, 3))
        with patch("shutil.which", return_value=None):
            preview = fp.ebook_preview(path, PANE)
        image, description = preview["sections"]
        self.assertEqual(Path(image["path"]).read_bytes(), png_bytes(2, 3))
        self.assertEqual(description["content"], "Spice.")
        meta = metadata(preview)
        self.assertEqual(
            (meta["Title"], meta["Author"], meta["Language"]),
            ("Dune", "Frank Herbert", "English"),
        )

    def test_sqlite_schema_with_stdlib(self):
        path = self.dir / "app.db"
        db = sqlite3.connect(path)
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        db.executemany("INSERT INTO users (name) VALUES (?)", [("a",), ("b",), ("c",)])
        db.execute("CREATE VIEW names AS SELECT name FROM users")
        db.commit()
        db.close()
        with patch("shutil.which", return_value=None):
            preview = fp.handler_for_name(path.name)(path, PANE)
        self.assertEqual(preview["badge"], "SQLite")
        self.assertEqual(
            plain(preview["text"]).splitlines(),
            ["users  3 rows", "  id INTEGER PK, name TEXT", "", "Views", "  names"],
        )
        self.assertEqual(metadata(preview), {"Tables": "1", "Views": "1"})


if __name__ == "__main__":
    unittest.main()
