useTab(str(arguments[1]));
var row = findRow(str(arguments[2]), parseInt(str(arguments[3])));
if (row < 0) { emit({ ok: false, error: "item is no longer in the clipboard history" }); }
else {
  var item = getItem(row);
  var keys = formatsOf(item).filter(function (k) { return k.indexOf("application/x-copyq-") != 0; });
  var want = {};
  for (var i = 0; i < keys.length; i++) want[keys[i]] = str(sha256sum(item[keys[i]]));
  select(row);
  var deadline = Date.now() + parseInt(str(arguments[4]));
  var missing = "";
  while (true) {
    missing = "";
    for (var k = 0; k < keys.length && !missing; k++)
      if (str(sha256sum(clipboard(keys[k]))) != want[keys[k]]) missing = keys[k];
    if (!missing || Date.now() > deadline) break;
    sleep(50);
  }
  if (missing) emit({ ok: false, error: "the clipboard did not take the item (" + missing + " differs)" });
  else emit({ ok: true, row: row });
}
