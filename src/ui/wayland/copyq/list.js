var tabName = useTab(str(arguments[1]));
var start = parseInt(str(arguments[2])), limit = parseInt(str(arguments[3]));
var terms = str(arguments[4]).toLowerCase().split(" ").filter(function (t) { return t.length > 0; });
var phrase = terms.join(" ");
var previewChars = parseInt(str(arguments[5]));
var onlyKind = str(arguments[6]);
var minChars = parseInt(str(arguments[7])) || 0;
// A positive byte budget emits each kept item's complete text as its excerpt,
// so Rust can match exclusion patterns against it, and ends the page before
// its items' output would exceed the budget. An item too large on its own is
// sent with a plain excerpt and flagged, for Rust to fetch its text alone.
var fullTextBudget = parseInt(str(arguments[8])) || 0;
var used = 0, overflow = false;
// Upper bound of the bytes `emit` prints for `value`: UTF-8 takes at most
// three bytes per UTF-16 unit of its JSON.
function outputBytes(value) { return 3 * JSON.stringify(value).length + 1; }
// A whole text that is one URL: any scheme followed by "://", or one of the
// schemes written without it, then no whitespace, control character or
// angle bracket. Built without backslashes, see prelude.js.
var URL_SCHEME = new RegExp("^([a-z][a-z0-9+.-]*://|(mailto|tel|magnet|urn|sms|geo):)", "i");
function isUrl(text) {
  text = text.trim();
  var scheme = URL_SCHEME.exec(text);
  if (!scheme || scheme[0].length == text.length) return false;
  for (var i = 0; i < text.length; i++) {
    var c = text.charAt(i), code = text.charCodeAt(i);
    if (code < 32 || (code >= 127 && code < 160) || c.trim() == "" || c == "<" || c == ">") return false;
  }
  return true;
}
function tierOf(text) {
  if (!terms.length) return 0;
  var lower = text.toLowerCase();
  for (var i = 0; i < terms.length; i++) if (lower.indexOf(terms[i]) < 0) return -1;
  var at = lower.indexOf(phrase);
  if (at < 0) return 2;
  return lower.trim().indexOf(phrase) == 0 ? 0 : 1;
}
var total = count();
var head = total ? fingerprint(getItem(0)) : "";
var end = Math.min(total, start + limit);
var items = [];
for (var row = start; row < end; row++) {
  var item = getItem(row);
  var keys = formatsOf(item);
  var kind = kindOf(keys);
  var isText = kind == "text" || kind == "html";
  if (onlyKind == "url") {
    if (!isText) continue;
  } else if (onlyKind && kind != onlyKind) continue;
  var text = textOf(item);
  if (onlyKind == "url" && !isUrl(text)) continue;
  if (isText && text.trim().length < minChars) continue;
  var tier = tierOf(text);
  if (tier < 0) continue;
  var entry = { r: row, id: fingerprint(item), f: keys, k: kind, n: text.length, p: fullTextBudget ? text : text.substring(0, previewChars), t: tier, c: copyTimeOf(item), s: sourceAppOf(item) };
  if (fullTextBudget) {
    var cost = outputBytes(entry);
    if (cost > fullTextBudget) {
      entry.p = text.substring(0, previewChars);
      entry.x = true;
      cost = outputBytes(entry);
    }
    // Too large even for an empty page: skip it, so the scan still advances.
    if (cost > fullTextBudget) continue;
    if (used + cost > fullTextBudget) { overflow = true; break; }
    used += cost;
  }
  items.push(entry);
}
var stable = count() == total && (total ? fingerprint(getItem(0)) : "") == head;
emit({ tab: tabName, total: total, head: head, next: overflow ? row : end, stable: stable, items: items });
