import concurrent.futures
import importlib.util
import http.client
import io
import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler
from pathlib import Path
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("contacts", Path(__file__).with_name("contacts.py"))
contacts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contacts)


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.env = patch.dict(os.environ, {"XDG_CACHE_HOME": self.directory.name})
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_entries_have_separate_contacts_and_photo_roots(self):
        with patch.object(contacts, "run_pass", return_value="{}"), \
             patch.object(contacts, "access_token", return_value="token"), \
             patch.object(contacts, "fetch_contacts", return_value=[{"name": "Personal"}]):
            personal, personal_root, warning = contacts.load_data(entry="personal")
        self.assertIsNone(warning)
        with patch.object(contacts, "run_pass", side_effect=RuntimeError("offline")):
            work, work_root, warning = contacts.load_data(entry="work")
            cached, cached_root, cached_warning = contacts.load_data(entry="personal")
        self.assertEqual(work, [])
        self.assertIsNotNone(warning)
        self.assertNotEqual(personal_root, work_root)
        self.assertEqual(cached_root, personal_root)
        self.assertEqual(cached, personal)
        self.assertIsNone(cached_warning)
        self.assertEqual(stat.S_IMODE((personal_root / "contacts.json").stat().st_mode), 0o600)

    def test_concurrent_writers_use_private_unique_temporary_files(self):
        barrier = threading.Barrier(2)
        original = tempfile.NamedTemporaryFile
        paths = []

        def temporary_file(*args, **kwargs):
            file = original(*args, **kwargs)
            paths.append(Path(file.name))
            self.assertEqual(stat.S_IMODE(os.stat(file.name).st_mode), 0o600)
            barrier.wait(timeout=5)
            return file

        with patch.object(contacts, "run_pass", return_value="{}"), \
             patch.object(contacts, "access_token", return_value="token"), \
             patch.object(contacts, "fetch_contacts", return_value=[{"name": "Alice"}]), \
             patch.object(contacts.tempfile, "NamedTemporaryFile", side_effect=temporary_file):
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(lambda _: contacts.load_data(force=True, entry="personal"), range(2)))
        self.assertTrue(all(result[2] is None for result in results))
        self.assertEqual(len(set(paths)), 2)
        self.assertTrue(all(not path.exists() for path in paths))
        saved = json.loads((results[0][1] / "contacts.json").read_text())
        self.assertEqual(saved["contacts"], [{"name": "Alice"}])

    def test_failed_replace_cleans_temporary_file_and_keeps_complete_cache(self):
        with patch.object(contacts, "run_pass", return_value="{}"), \
             patch.object(contacts, "access_token", return_value="token"), \
             patch.object(contacts, "fetch_contacts", return_value=[{"name": "Alice"}]):
            saved, root, _ = contacts.load_data(entry="personal")
            with patch.object(Path, "replace", side_effect=OSError("failed")):
                result, _, warning = contacts.load_data(force=True, entry="personal")
        self.assertEqual(result, saved)
        self.assertIsNotNone(warning)
        self.assertEqual(list(root.glob("*.tmp")), [])
        self.assertEqual(json.loads((root / "contacts.json").read_text())["contacts"], saved)

    def test_missing_avatar_does_not_delay_contact_results(self):
        person = {"names": [{"displayName": "Alice"}],
                  "photos": [{"url": "https://example.com/alice.jpg"}]}
        output = io.StringIO()
        with patch.object(contacts, "load_data", return_value=([person], Path(self.directory.name), None)), \
             patch.object(contacts.urllib.request, "urlopen") as fetch, \
             patch.object(contacts.subprocess, "Popen") as worker, redirect_stdout(output):
            contacts.main(["--pass-entry", "personal"])
        fetch.assert_not_called()
        worker.assert_called_once()
        self.assertEqual(worker.call_args.args[0][-2:], ["--pass-entry", "personal"])
        item = json.loads(output.getvalue())["items"][0]
        self.assertEqual(item["title"], "Alice")
        self.assertIsNone(item["icon"])

    def test_avatar_worker_spawn_failure_preserves_contact_results(self):
        person = {"names": [{"displayName": "Alice"}],
                  "photos": [{"url": "https://example.com/alice.jpg"}]}
        output = io.StringIO()
        with patch.object(contacts, "load_data", return_value=([person], Path(self.directory.name), None)), \
             patch.object(contacts.subprocess, "Popen", side_effect=OSError("cannot spawn")), \
             redirect_stdout(output):
            contacts.main([])
        self.assertEqual(json.loads(output.getvalue())["items"][0]["title"], "Alice")

    def seed_avatar_contacts(self, url):
        root = contacts.cache_dir() / contacts.hashlib.sha256(b"personal").hexdigest()
        root.mkdir(parents=True, exist_ok=True)
        (root / "contacts.json").write_text(json.dumps({"contacts": [{"photos": [{"url": url}]}]}))
        return root

    def test_truncated_avatar_is_not_published_and_can_be_retried(self):
        url = "https://example.com/alice.jpg"
        root = self.seed_avatar_contacts(url)
        avatars = root / "avatars"
        avatars.mkdir()
        stale = avatars / "avatar-aborted.tmp"
        stale.write_bytes(b"unfinished")

        class ResponseSocket:
            def __init__(self, body):
                self.body = body

            def makefile(self, *args):
                return io.BytesIO(self.body)

        for name, body, expected in (
                ("truncated length", b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc", None),
                ("truncated chunk", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab", None),
                ("empty", b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", None),
                ("oversized", b"HTTP/1.1 200 OK\r\nContent-Length: 2000001\r\n\r\n" + b"x" * 2_000_001, None),
                ("complete", b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc", b"abc")):
            with self.subTest(response=name):
                response = http.client.HTTPResponse(ResponseSocket(body))
                response.begin()
                with patch.object(contacts.urllib.request, "urlopen", return_value=response):
                    contacts.cache_avatars("personal")
                self.assertFalse(stale.exists())
                avatar = contacts.avatar_path(root, url)
                if expected is None:
                    self.assertFalse(avatar.exists())
                    self.assertEqual(list((root / "avatars").iterdir()), [])
                else:
                    self.assertEqual(avatar.read_bytes(), expected)
                    self.assertEqual(stat.S_IMODE(avatar.stat().st_mode), 0o600)

    def test_avatar_worker_deadline_terminates_reads_and_releases_lock(self):
        for mode in ("headers", "body", "trickle"):
            with self.subTest(mode=mode):
                requested, stop = threading.Event(), threading.Event()

                class SlowResponse(BaseHTTPRequestHandler):
                    def do_GET(self):
                        requested.set()
                        if mode == "headers":
                            stop.wait(timeout=5)
                            return
                        self.send_response(200)
                        self.send_header("Content-Length", "100000")
                        self.end_headers()
                        if mode == "body":
                            stop.wait(timeout=5)
                            return
                        while not stop.wait(0.02):
                            try:
                                self.wfile.write(b"x")
                                self.wfile.flush()
                            except OSError:
                                return

                    def log_message(self, *_):
                        pass

                server = contacts.HTTPServer(("127.0.0.1", 0), SlowResponse)
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                url = f"http://127.0.0.1:{server.server_port}/photo"
                root = self.seed_avatar_contacts(url)
                script = (f"import runpy; module = runpy.run_path({str(Path(contacts.__file__).resolve())!r}); "
                          "module['main'].__globals__['AVATAR_CACHE_TIMEOUT_SECONDS'] = 0.4; "
                          "module['main'](['--cache-avatars', '--pass-entry', 'personal'])")
                worker = subprocess.Popen([sys.executable, "-B", "-c", script],
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                try:
                    self.assertTrue(requested.wait(timeout=3), "Avatar worker never requested photo")
                    with (root / "avatars.lock").open("a") as lock:
                        with self.assertRaises(BlockingIOError):
                            contacts.fcntl.flock(lock, contacts.fcntl.LOCK_EX | contacts.fcntl.LOCK_NB)
                        stdout, stderr = worker.communicate(timeout=3)
                        self.assertEqual(worker.returncode, 0, stderr.decode())
                        self.assertEqual(stdout, b"")
                        contacts.fcntl.flock(lock, contacts.fcntl.LOCK_EX | contacts.fcntl.LOCK_NB)
                    self.assertFalse(contacts.avatar_path(root, url).exists())
                finally:
                    if worker.poll() is None:
                        worker.kill()
                    worker.communicate(timeout=3)
                    stop.set()
                    server.shutdown()
                    server.server_close()
                    thread.join(timeout=3)

    def test_cached_avatar_is_used_without_starting_worker(self):
        root = Path(self.directory.name)
        url = "https://example.com/alice.jpg"
        avatar = contacts.avatar_path(root, url)
        avatar.parent.mkdir()
        avatar.write_bytes(b"image")
        person = {"photos": [{"url": url}]}
        output = io.StringIO()
        with patch.object(contacts, "load_data", return_value=([person], root, None)), \
             patch.object(contacts.subprocess, "Popen") as worker, redirect_stdout(output):
            contacts.main([])
        worker.assert_not_called()
        self.assertEqual(json.loads(output.getvalue())["items"][0]["icon"], {"path": str(avatar)})


class AuthorizationTests(unittest.TestCase):
    def run_authorization(self, callback_error=None, clock=None):
        server = Mock(server_port=12345)
        handler_class = None
        state = None
        response = None

        def make_server(address, handler):
            nonlocal handler_class
            self.assertEqual(address, ("127.0.0.1", 0))
            handler_class = handler
            return server

        def open_browser(url):
            nonlocal state
            state = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)["state"][0]

        def handle_request():
            nonlocal response
            handler = handler_class.__new__(handler_class)
            handler.path = "/?" + urllib.parse.urlencode({"state": state, "error": callback_error})
            handler.send_response = Mock()
            handler.end_headers = Mock()
            handler.wfile = io.BytesIO()
            handler.do_GET()
            response = handler

        server.handle_request.side_effect = handle_request
        with patch.object(contacts, "HTTPServer", side_effect=make_server), \
             patch.object(contacts.webbrowser, "open", side_effect=open_browser), \
             patch.object(contacts, "run_pass", return_value='{"client_id":"client"}'), \
             patch.object(contacts, "request_json") as request, \
             patch.object(contacts, "save_credentials") as save, \
             patch.object(contacts.time, "monotonic", side_effect=clock or [0, 1, 2]):
            with self.assertRaisesRegex(RuntimeError if callback_error else TimeoutError,
                                        callback_error or "timed out"):
                contacts.authorize("test-entry")
            request.assert_not_called()
            save.assert_not_called()
        server.server_close.assert_called_once()
        return response, server

    def test_denied_consent_is_terminal_and_reports_failure(self):
        response, server = self.run_authorization(callback_error="access_denied")
        response.send_response.assert_called_once_with(400)
        self.assertIn(b"Authorization failed", response.wfile.getvalue())
        server.handle_request.assert_called_once()

    def test_missing_callback_has_overall_deadline(self):
        _, server = self.run_authorization(clock=[0, 181])
        server.handle_request.assert_not_called()

    def test_accepted_connections_cannot_outlive_authorization_deadline(self):
        for initial_request, trickle in ((b"", False), (b"GET / HTTP/1.1\r\nHost: ", False),
                                         (b"GET / HTTP/1.1\r\nHost: ", True)):
            with self.subTest(initial_request=initial_request, trickle=trickle):
                clients, servers, errors, senders = [], [], [], []
                stop = threading.Event()
                real_server = contacts.HTTPServer

                def make_server(address, handler):
                    server = real_server(address, handler)
                    servers.append(server)
                    return server

                def send_bytes(client):
                    while not stop.wait(0.01):
                        try:
                            client.sendall(b"x")
                        except OSError:
                            return

                def open_browser(url):
                    redirect = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)["redirect_uri"][0]
                    port = urllib.parse.urlparse(redirect).port
                    client = socket.create_connection(("127.0.0.1", port), timeout=1)
                    clients.append(client)
                    client.sendall(initial_request)
                    if trickle:
                        sender = threading.Thread(target=send_bytes, args=(client,), daemon=True)
                        senders.append(sender)
                        sender.start()

                def authorize():
                    try:
                        contacts.authorize("test-entry")
                    except Exception as error:
                        errors.append(error)

                with patch.object(contacts, "HTTPServer", side_effect=make_server), \
                     patch.object(contacts, "AUTHORIZATION_TIMEOUT_SECONDS", 0.2), \
                     patch.object(contacts.webbrowser, "open", side_effect=open_browser), \
                     patch.object(contacts, "run_pass", return_value='{"client_id":"client"}'), \
                     patch.object(contacts, "request_json") as request, \
                     patch.object(contacts, "save_credentials") as save:
                    worker = threading.Thread(target=authorize, daemon=True)
                    worker.start()
                    try:
                        worker.join(timeout=3)
                        self.assertFalse(worker.is_alive(), "Accepted connection exceeded OAuth deadline")
                        self.assertEqual(len(errors), 1)
                        self.assertIsInstance(errors[0], TimeoutError)
                        self.assertEqual(servers[0].socket.fileno(), -1)
                        request.assert_not_called()
                        save.assert_not_called()
                    finally:
                        stop.set()
                        for client in clients:
                            client.close()
                        for sender in senders:
                            sender.join(timeout=3)
                        worker.join(timeout=3)

    def test_request_timeout_does_not_restart_overall_deadline(self):
        server = Mock(server_port=12345)
        with patch.object(contacts, "HTTPServer", return_value=server), \
             patch.object(contacts.webbrowser, "open"), \
             patch.object(contacts, "run_pass", return_value='{"client_id":"client"}'), \
             patch.object(contacts.time, "monotonic", side_effect=[0, 1, 181]):
            with self.assertRaises(TimeoutError):
                contacts.authorize("test-entry")
        server.handle_request.assert_called_once()
        self.assertEqual(server.timeout, 179)
        server.server_close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
