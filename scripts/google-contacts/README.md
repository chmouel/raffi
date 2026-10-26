# Google Contacts filter

Browse saved Google contacts from Raffi. Enter opens a contact's available
fields, Enter copies the selected field, and Escape returns to the prior list.
Search matches names and contact data.

## Setup

Enable the People API in a Google Cloud project, configure its OAuth consent
screen, and create a Desktop app OAuth client. Store the downloaded client
JSON using `pass insert -m google/raffi-contacts < client_secret.json`.
Install the script and authorize once:

```sh
mkdir -p ~/.config/raffi/scripts/google-contacts
cp scripts/google-contacts/contacts.py ~/.config/raffi/scripts/google-contacts/
chmod +x ~/.config/raffi/scripts/google-contacts/contacts.py
~/.config/raffi/scripts/google-contacts/contacts.py --authorize --pass-entry google/raffi-contacts
```

Google may require test users to be added to the consent screen. Refresh
tokens issued while an OAuth app is in testing can expire after seven days;
rerun authorization if Google rejects the saved token. Credentials and the
refresh token remain in `pass`. The script requests only
`contacts.readonly`.

The default pass entry is `google/raffi-contacts`. Set
`GOOGLE_CONTACTS_PASS_ENTRY` in the script filter's `env` to change it, or
use `args: ["--pass-entry", "google/agenda-credentials"]`. The same flag
works when running the script directly. Use `--help` for command options.
Authorization verifies the saved entry before reporting success. Denied consent
reports failure; authorization without a callback times out after three minutes.
Contact data and photo files are cached with private permissions under
`$XDG_CACHE_HOME/raffi/google-contacts/<entry-hash>/`, separately for each pass
entry. Contact data is reused for five minutes. Ctrl+R forces a refresh and
returns from contact fields to the root contact list. Failed refreshes leave
the last complete contact cache for that entry available.
Missing photos download in the background, with up to eight requests at a
time. Each download worker stops after 30 seconds, releasing its cache lock;
unfetched photos are retried on later runs. Empty, oversized, or detectably
incomplete responses are not cached. Results immediately use available cached
photos; run the filter again to see newly downloaded photos. Failure to start
the optional worker does not prevent contact results. Google placeholder
photos are skipped.

Example configuration is in the script-filter feature documentation.
