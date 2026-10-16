useTab(str(arguments[1]));
var row = findRow(str(arguments[2]), parseInt(str(arguments[3])));
if (row < 0) { emit({ ok: false, error: "item is no longer in the clipboard history" }); }
else {
  var item = getItem(row);
  var keys = formatsOf(item);
  var maxText = parseInt(str(arguments[4])), maxImage = parseInt(str(arguments[5]));
  var sizes = [];
  for (var i = 0; i < keys.length; i++) sizes.push([keys[i], item[keys[i]].length]);
  var image = "";
  if (item["image/png"] !== undefined) image = "image/png";
  else for (var j = 0; j < keys.length && !image; j++) if (keys[j].indexOf("image/") == 0) image = keys[j];
  var text = textOf(item);
  var send = image && item[image].length <= maxImage;
  emit({ ok: true, row: row, f: keys, k: kindOf(keys), sizes: sizes, n: text.length, text: text.substring(0, maxText), image: image, sent: send ? item[image].length : -1 });
  if (send) print(item[image]);
}
