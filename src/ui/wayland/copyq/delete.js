useTab(str(arguments[1]));
var row = findRow(str(arguments[2]), parseInt(str(arguments[3])));
if (row < 0) { emit({ ok: false, error: "item is no longer in the clipboard history" }); }
else { remove(row); emit({ ok: true }); }
