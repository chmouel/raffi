#!/usr/bin/env python3
"""Raffi filter for Google People API contacts."""

import argparse
import base64
import concurrent.futures
import fcntl
import hashlib
import http.client
import json
import os
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.error
import urllib.request
import webbrowser
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

API = "https://people.googleapis.com/v1/people/me/connections"
TOKEN_URL = "https://oauth2.googleapis.com/token"
SCOPE = "https://www.googleapis.com/auth/contacts.readonly"
AUTHORIZATION_TIMEOUT_SECONDS = 180
AVATAR_CACHE_TIMEOUT_SECONDS = 30
PASS_ENTRY = os.environ.get("GOOGLE_CONTACTS_PASS_ENTRY", "google/raffi-contacts")


def run_pass(*args, input_text=None):
    return subprocess.run(["pass", *args], input=input_text, text=True,
                          capture_output=True, check=True).stdout


def cache_dir():
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "raffi/google-contacts"


def photo_url(person):
    return next((photo["url"] for photo in person.get("photos", [])
                 if photo.get("url") and not photo.get("default")), None)


def avatar_path(root, url):
    return root / "avatars" / (hashlib.sha256(url.encode()).hexdigest() + ".jpg")


def cache_avatars(entry):
    root = cache_dir() / hashlib.sha256(entry.encode()).hexdigest()
    with (root / "avatars.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        people = json.loads((root / "contacts.json").read_text())["contacts"]
        avatars = root / "avatars"
        avatars.mkdir(mode=0o700, exist_ok=True)
        # Expired workers can leave unpublished files, and the lock excludes live writers
        for temp in avatars.glob("avatar-*.tmp"):
            temp.unlink(missing_ok=True)

        def download(url):
            path = avatar_path(root, url)
            if path.exists():
                return
            temp = None
            try:
                with urllib.request.urlopen(url, timeout=5) as response:
                    length = response.getheader("Content-Length")
                    image = response.read(2_000_001)
                if not image or len(image) > 2_000_000:
                    return
                if length is not None and len(image) != int(length):
                    return
                with tempfile.NamedTemporaryFile(dir=path.parent, prefix="avatar-", suffix=".tmp",
                                                 delete=False) as output:
                    temp = Path(output.name)
                    output.write(image)
                temp.replace(path)
            except (OSError, ValueError, http.client.HTTPException):
                pass
            finally:
                if temp is not None:
                    temp.unlink(missing_ok=True)

        urls = {url for person in people if (url := photo_url(person))}
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
            list(executor.map(download, urls))


def request_json(url, token=None, data=None):
    headers = {"User-Agent": "raffi-google-contacts/1"}
    if token:
        headers["Authorization"] = "Bearer " + token
    if data is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
        data = urllib.parse.urlencode(data).encode()
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=15) as response:
        return json.load(response)


def save_credentials(entry, credentials):
    run_pass("insert", "--multiline", "--force", "--", entry,
             input_text=json.dumps(credentials, indent=2) + "\n")
    saved = json.loads(run_pass("show", "--", entry))
    if saved != credentials:
        raise RuntimeError(f"Could not verify credentials saved to pass entry {entry}")


def authorize(entry):
    client = json.loads(run_pass("show", entry))
    client = client.get("installed", client)
    client_id, client_secret = client["client_id"], client.get("client_secret", "")
    verifier = secrets.token_urlsafe(64)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
    state = secrets.token_urlsafe(24)
    class Callback(BaseHTTPRequestHandler):
        code = None
        error = None
        def handle(self):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            self.connection.settimeout(remaining)

            def expire():
                try:
                    self.connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

            # Socket timeouts alone restart on each read, allowing slow requests to outlive the deadline
            timer = threading.Timer(remaining, expire)
            timer.daemon = True
            timer.start()
            try:
                super().handle()
            except OSError:
                if time.monotonic() < deadline:
                    raise
            finally:
                timer.cancel()
                timer.join()

        def do_GET(self):
            args = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            if args.get("state", [None])[0] != state:
                self.send_error(400)
                return
            Callback.error = args.get("error", [None])[0]
            Callback.code = args.get("code", [None])[0]
            if not Callback.error and not Callback.code:
                Callback.error = "missing authorization code"
            self.send_response(400 if Callback.error else 200)
            self.end_headers()
            self.wfile.write(b"Authorization failed. Return to the terminal." if Callback.error else
                             b"Google authorization received. Return to the terminal to check whether credentials were saved.")
        def log_message(self, *_):
            pass
    server = HTTPServer(("127.0.0.1", 0), Callback)
    redirect = f"http://127.0.0.1:{server.server_port}"
    params = {"client_id": client_id, "redirect_uri": redirect, "response_type": "code",
              "scope": SCOPE, "state": state, "code_challenge": challenge,
              "code_challenge_method": "S256", "access_type": "offline", "prompt": "consent"}
    try:
        deadline = time.monotonic() + AUTHORIZATION_TIMEOUT_SECONDS
        webbrowser.open("https://accounts.google.com/o/oauth2/v2/auth?" + urllib.parse.urlencode(params))
        while Callback.code is None and Callback.error is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Google Contacts authorization timed out")
            server.timeout = remaining
            server.handle_request()
        if time.monotonic() >= deadline:
            raise TimeoutError("Google Contacts authorization timed out")
        if Callback.error:
            raise RuntimeError(f"Google Contacts authorization failed: {Callback.error}")
    finally:
        server.server_close()
    tokens = request_json(TOKEN_URL, data={"client_id": client_id, "client_secret": client_secret,
        "code": Callback.code, "code_verifier": verifier, "redirect_uri": redirect,
        "grant_type": "authorization_code"})
    credentials = {"client_id": client_id, "client_secret": client_secret,
                   "refresh_token": tokens["refresh_token"]}
    save_credentials(entry, credentials)
    print(f"Saved Google Contacts credentials to pass entry {entry}")


def access_token(credentials):
    return request_json(TOKEN_URL, data={"client_id": credentials["client_id"],
        "client_secret": credentials.get("client_secret", ""),
        "refresh_token": credentials["refresh_token"], "grant_type": "refresh_token"})["access_token"]


def fetch_contacts(token):
    contacts, page = [], None
    fields = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,birthdays,events,urls,biographies,relations,userDefined,photos"
    while True:
        params = {"personFields": fields, "pageSize": 1000, "sources": "READ_SOURCE_TYPE_CONTACT"}
        if page:
            params["pageToken"] = page
        result = request_json(API + "?" + urllib.parse.urlencode(params), token)
        contacts.extend(result.get("connections", []))
        page = result.get("nextPageToken")
        if not page:
            return contacts


def values(person, key, label):
    return [(f"{label} ({entry.get('type', 'other')})", entry.get(key, ""))
            for entry in person.get(key + ("s" if key not in ("addresses", "organizations", "birthdays", "events", "urls", "relations", "userDefined") else ""), [])]


def fields_for(person):
    fields = []
    for key, label, prop in (("emailAddresses", "Email", "value"), ("phoneNumbers", "Phone", "value"),
        ("addresses", "Address", "formattedValue"), ("organizations", "Organization", "name"),
        ("birthdays", "Birthday", "date"), ("events", "Event", "date"), ("urls", "URL", "value"),
        ("relations", "Relation", "person"), ("userDefined", "Custom", "value")):
        for entry in person.get(key, []):
            val = entry.get(prop)
            if isinstance(val, dict):
                val = "-".join(str(val.get(k)) for k in ("year", "month", "day") if val.get(k))
            if val:
                fields.append({"title": f"{label}: {val}", "arg": str(val), "subtitle": entry.get("type", "")})
    for entry in person.get("biographies", []):
        if entry.get("value"):
            fields.append({"title": "Note: " + entry["value"], "arg": entry["value"]})
    return fields


def load_data(force=False, entry=PASS_ENTRY):
    root = cache_dir() / hashlib.sha256(entry.encode()).hexdigest()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = root / "contacts.json"
    try:
        saved = json.loads(path.read_text())
        if not force and time.time() - saved["fetched"] < 300:
            return saved["contacts"], root, None
    except (OSError, ValueError, KeyError):
        saved = None
    try:
        credentials = json.loads(run_pass("show", "--", entry))
        contacts = fetch_contacts(access_token(credentials))
        temp = None
        try:
            with tempfile.NamedTemporaryFile(mode="w", dir=root, prefix="contacts-", suffix=".tmp",
                                             delete=False) as output:
                temp = Path(output.name)
                json.dump({"fetched": time.time(), "contacts": contacts}, output)
            temp.replace(path)
        finally:
            if temp is not None:
                temp.unlink(missing_ok=True)
        return contacts, root, None
    except Exception as error:
        detail = type(error).__name__
        if isinstance(error, urllib.error.HTTPError):
            detail = f"Google HTTP {error.code}"
            try:
                payload = json.load(error)
                reason = payload.get("error")
                if isinstance(reason, dict):
                    detail += ": " + reason.get("message", reason.get("status", "Request failed"))
                elif isinstance(reason, str):
                    detail += ": " + reason
            except (ValueError, AttributeError):
                pass
        if saved:
            return saved["contacts"], root, f"Refresh failed; showing cached contacts ({detail})"
        return [], root, f"Google Contacts unavailable ({detail}). Pass entry: {entry}"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--authorize", action="store_true", help="Authorize Google Contacts and save credentials to pass")
    parser.add_argument("--pass-entry", default=PASS_ENTRY, help="Pass entry (default: GOOGLE_CONTACTS_PASS_ENTRY or google/raffi-contacts)")
    parser.add_argument("--cache-avatars", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("query", nargs="?", default="", help="Contact search text")
    args = parser.parse_args(argv)
    if args.authorize:
        authorize(args.pass_entry)
        return
    if args.cache_avatars:
        # Executor shutdown waits for running reads, so expiry must terminate this dedicated process
        timer = threading.Timer(AVATAR_CACHE_TIMEOUT_SECONDS, os._exit, args=(0,))
        timer.daemon = True
        timer.start()
        try:
            cache_avatars(args.pass_entry)
        finally:
            timer.cancel()
            timer.join()
        return
    contacts, root, warning = load_data(os.environ.get("RAFFI_REFRESH") == "1", args.pass_entry)
    needle = args.query.casefold()
    items = []
    missing_avatars = False
    for person in contacts:
        name = next((n.get("displayName") for n in person.get("names", []) if n.get("displayName")), "Unnamed contact")
        searchable = json.dumps(person, ensure_ascii=False).casefold()
        if needle and needle not in name.casefold() and needle not in searchable:
            continue
        photo = photo_url(person)
        icon = None
        if photo:
            avatar = avatar_path(root, photo)
            if avatar.exists():
                icon = str(avatar)
            else:
                missing_avatars = True
        emails = ", ".join(e.get("value", "") for e in person.get("emailAddresses", []) if e.get("value"))
        metadata = [["Name", name]]
        for key, label, prop in (("nicknames", "Nickname", "value"), ("emailAddresses", "Email", "value"),
            ("phoneNumbers", "Phone", "value"), ("addresses", "Address", "formattedValue"),
            ("organizations", "Organization", "name"), ("birthdays", "Birthday", "date"),
            ("events", "Event", "date"), ("urls", "URL", "value"), ("biographies", "Note", "value"),
            ("relations", "Relation", "person"), ("userDefined", "Custom", "value")):
            for entry in person.get(key, []):
                value = entry.get(prop)
                if isinstance(value, dict):
                    value = "-".join(str(value.get(k)) for k in ("year", "month", "day") if value.get(k))
                if value:
                    metadata.append([label, str(value)])
        items.append({"title": name, "subtitle": emails or "Google contact", "arg": name,
                      "icon": {"path": icon} if icon else None, "children": fields_for(person),
                      "preview": {"title": name, "subtitle": emails, "metadata": [["Name", name]] +
                          metadata[1:],
                          "image": icon}})
    if warning:
        items.insert(0, {"title": warning, "valid": False})
    print(json.dumps({"items": items}))
    if missing_avatars:
        try:
            subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--cache-avatars",
                              "--pass-entry", args.pass_entry], stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
        except OSError:
            pass


if __name__ == "__main__":
    main()
