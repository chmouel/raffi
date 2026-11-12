// Shared helpers for the CopyQ clipboard scripts. CopyQ expands escape
// sequences in a script passed on its command line, so these scripts must not
// contain backslashes (a unit test enforces it). Values arrive raw after `--`
// as arguments[1..].
var NL = String.fromCharCode(10);
function fingerprint(item) {
  var keys = Object.keys(item).sort();
  var parts = [];
  for (var i = 0; i < keys.length; i++) parts.push(keys[i] + "=" + str(sha256sum(item[keys[i]])));
  return str(sha256sum(parts.join(NL)));
}
function formatsOf(item) { return Object.keys(item).sort(); }
function kindOf(keys) {
  for (var i = 0; i < keys.length; i++) if (keys[i].indexOf("image/") == 0) return "image";
  if (keys.indexOf("text/uri-list") >= 0) return "files";
  if (keys.indexOf("text/plain") >= 0) return "text";
  if (keys.indexOf("text/html") >= 0) return "html";
  return "other";
}
var STYLE = new RegExp("<(style|script)[^>]*>[^<]*</(style|script)>", "gi");
function formatText(item, mime) { return item[mime] === undefined ? "" : str(item[mime]); }
// Plain text, else the file list, else the HTML with its tags removed. Empty
// formats are skipped, as the raffi backend drops them before reading text.
function textOf(item) {
  var text = formatText(item, "text/plain") || formatText(item, "text/uri-list");
  if (text) return text;
  return formatText(item, "text/html").replace(STYLE, " ").replace(/<[^>]*>/g, " ")
    .replace(/&nbsp;/g, " ").replace(/&lt;/g, "<").replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"').replace(/&amp;/g, "&");
}
// Copy time stored by CopyQ's "Show Copy Time" command, either as its own
// format or, in newer versions of the command, as a tag.
var COPY_TIME = new RegExp("^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$");
function copyTimeOf(item) {
  var time = item["application/x-copyq-user-copy-time"];
  if (time !== undefined && COPY_TIME.test(str(time).trim())) return str(time).trim();
  var tags = item["application/x-copyq-tags"];
  if (tags === undefined) return "";
  var parts = str(tags).split(",");
  for (var i = parts.length - 1; i >= 0; i--) if (COPY_TIME.test(parts[i].trim())) return parts[i].trim();
  return "";
}
// App that had focus when the item was copied, stored by the "Store Source
// App" command from the clipboard history docs. Only a label, so bounded.
function sourceAppOf(item) {
  var app = item["application/x-copyq-user-source-app"];
  return app === undefined ? "" : str(app).trim().substring(0, 256);
}
function useTab(name) {
  if (!name) {
    name = str(config("clipboard_tab")).split(NL).join("");
    if (!name) { var tabs = tab(); name = tabs.length ? str(tabs[0]) : ""; }
  }
  if (!name) throw new Error("CopyQ has no clipboard tab");
  tab(name);
  return name;
}
function findRow(id, hint) {
  var n = count();
  if (hint >= 0 && hint < n && fingerprint(getItem(hint)) == id) return hint;
  for (var i = 0; i < n; i++) if (fingerprint(getItem(i)) == id) return i;
  return -1;
}
function emit(value) { print(JSON.stringify(value) + NL); }
