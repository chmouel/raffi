
import base64
import io
import json
import os
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

import search

ANSWER = {
    "Heading": "Python (programming language)",
    "AbstractText": "Python is a programming language.",
    "AbstractSource": "Wikipedia",
    "AbstractURL": "https://en.wikipedia.org/wiki/Python",
    "Image": "/i/python.png",
}


def fake_fetch(url, timeout):
    if url.startswith("https://api.duckduckgo.com/"):
        return json.dumps(ANSWER).encode()
    if url == "https://duckduckgo.com/i/python.png":
        return b"PNGDATA"
    raise AssertionError(url)


class SearchPreviewTests(unittest.TestCase):
    def test_preview_uses_instant_answer_and_image(self):
        with patch.object(search, "fetch", side_effect=fake_fetch):
            preview = search.build_preview("google", "python", "https://g/?q=python")

        self.assertEqual(preview["title"], "Python (programming language)")
        self.assertEqual(preview["badge"], "Google")
        self.assertEqual(
            preview["sections"],
            [
                {"type": "image", "base64": base64.b64encode(b"PNGDATA").decode()},
                {"type": "text", "content": "Python is a programming language."},
            ],
        )
        self.assertEqual(
            preview["metadata"],
            [
                ["Opens", "https://g/?q=python"],
                ["Source", "https://en.wikipedia.org/wiki/Python"],
            ],
        )

    def test_preview_falls_back_when_lookup_fails(self):
        with patch.object(search, "fetch", side_effect=TimeoutError("slow")):
            preview = search.build_preview("bing", "nothing", "https://b/?q=nothing")

        self.assertEqual(preview["title"], "nothing")
        self.assertIn("No instant answer", preview["text"])
        self.assertEqual(preview["metadata"], [["Opens", "https://b/?q=nothing"]])

    def test_main_prints_preview_when_raffi_asks(self):
        stdout = io.StringIO()
        env = {"RAFFI_PREVIEW": "1", "RAFFI_TITLE": "python", "RAFFI_VALUE": ""}
        with (
            patch.dict(os.environ, env),
            patch.object(search.sys, "argv", ["search.py", "duckduckgo", "py"]),
            patch.object(search, "fetch", side_effect=fake_fetch),
            redirect_stdout(stdout),
        ):
            search.main()

        payload = json.loads(stdout.getvalue())
        self.assertNotIn("items", payload)
        self.assertEqual(
            payload["metadata"][0], ["Opens", search.build_url("duckduckgo", "python")]
        )


if __name__ == "__main__":
    unittest.main()
