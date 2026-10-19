// Конструктор дерева условий и действий правила скидки (как CSaleCondTree Битрикса).
// Дерево {CLASS_ID, DATA, CHILDREN} живёт в скрытых полях conditions/actions и переписывается
// после каждой правки; классы, подписи и допустимую вложенность присылает сервер
// (#discount-tree-data). Проверяет дерево сервер при сохранении — ошибки приходят по пути узла.
(function () {
  "use strict";

  var dataInput = document.getElementById("discount-tree-data");
  if (!dataInput) return;
  var D = JSON.parse(dataInput.value);
  var classes = {};
  D.classes.forEach(function (c) { classes[c.id] = c; });
  var names = D.names || {};
  var asked = { items: {}, sections: {} };

  function el(tag, attrs, text) {
    var e = document.createElement(tag);
    Object.keys(attrs || {}).forEach(function (k) { e.setAttribute(k, attrs[k]); });
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function asList(v) {
    if (Array.isArray(v)) return v;
    return v === undefined || v === null || v === "" ? [] : [v];
  }

  function clone(v) { return JSON.parse(JSON.stringify(v)); }

  // Ключ с точкой — вложенный объект DATA (sum_period_data.discount_sum_period_type)
  function getKey(data, key) {
    var parts = key.split(".");
    var o = data;
    for (var i = 0; i < parts.length; i++) {
      if (o === null || typeof o !== "object") return undefined;
      o = o[parts[i]];
    }
    return o;
  }

  function setKey(data, key, v) {
    var parts = key.split(".");
    var o = data;
    for (var i = 0; i < parts.length - 1; i++) {
      if (o[parts[i]] === null || typeof o[parts[i]] !== "object" || Array.isArray(o[parts[i]])) o[parts[i]] = {};
      o = o[parts[i]];
    }
    if (v === undefined) delete o[parts[parts.length - 1]];
    else o[parts[parts.length - 1]] = v;
  }

  function newNode(classId) {
    var data = {};
    classes[classId].fields.forEach(function (f) {
      if (f.kind !== "json" && f.default !== null) setKey(data, f.key, clone(f.default));
    });
    return { CLASS_ID: classId, DATA: data, CHILDREN: [] };
  }

  // Даты — по Москве (UTC+3, без перехода на летнее время), хранятся unix-секундами
  var MSK = 3 * 3600;
  function pad(n) { return (n < 10 ? "0" : "") + n; }
  function toInput(ts) {
    var n = Number(ts);
    if (ts === "" || ts === null || ts === undefined || !isFinite(n) || n === 0) return "";
    var d = new Date((n + MSK) * 1000);
    return d.getUTCFullYear() + "-" + pad(d.getUTCMonth() + 1) + "-" + pad(d.getUTCDate()) +
      "T" + pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes());
  }
  function fromInput(s) {
    var m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(s);
    if (!m) return undefined;
    return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5]) / 1000 - MSK;
  }

  document.querySelectorAll(".discount-tree").forEach(mountTree);

  function mountTree(box) {
    var tree = box.getAttribute("data-tree");
    var input = box.parentNode.querySelector('input[name="' + tree + '"]');
    var errors = (D.errors && D.errors[tree]) || {};
    var root;
    try {
      root = JSON.parse(input.value);
    } catch (e) {
      rawEditor(box, input);
      return;
    }

    function sync() { input.value = JSON.stringify(root); }
    function redraw() { box.replaceChildren(renderNode(root, "0", null, 0)); sync(); }
    // После смены структуры пути узлов другие — ошибки сервера больше не к месту
    function restructure() { errors = {}; redraw(); }

    function renderNode(node, path, parent, index) {
      var isRoot = parent === null;
      var def = classes[node.CLASS_ID];
      var wrap = el("div", { "class": "dt-node" + (errors[path] ? " has-error" : "") });
      var head = el("div", { "class": "dt-head" });
      var title = isRoot
        ? (tree === "actions" ? "Выполнить действия:" : "Условия:")
        : (def ? def.label : "Неподдерживаемый элемент «" + node.CLASS_ID + "»");
      head.appendChild(el("strong", {}, title));
      if (def && !(isRoot && tree === "actions")) {
        node.DATA = node.DATA || {};
        def.fields.forEach(function (f) { control(head, node, f); });
      }
      if (!isRoot) {
        var del = el("button", { type: "button", "class": "link danger" }, "Удалить");
        del.addEventListener("click", function () {
          parent.CHILDREN.splice(index, 1);
          restructure();
        });
        head.appendChild(del);
      }
      wrap.appendChild(head);
      (errors[path] || []).forEach(function (m) {
        wrap.appendChild(el("div", { "class": "dt-error" }, path + ": " + m));
      });
      if (!def) {
        if (!isRoot) wrap.appendChild(el("div", { "class": "muted small" },
          "С ним правило не применяется; удалите элемент, чтобы правило заработало."));
        return wrap;
      }
      var allowed = D.children[tree][isRoot ? "#root" : node.CLASS_ID];
      if (allowed) {
        node.CHILDREN = node.CHILDREN || [];
        var list = el("div", { "class": "dt-children" });
        node.CHILDREN.forEach(function (child, i) {
          list.appendChild(renderNode(child, path + "." + i, node, i));
        });
        wrap.appendChild(list);
        wrap.appendChild(addSelect(node, allowed, tree === "actions" && isRoot));
      }
      return wrap;
    }

    function addSelect(node, allowed, isAction) {
      var sel = el("select", { "class": "dt-add" });
      sel.appendChild(el("option", { value: "" }, isAction ? "Добавить действие…" : "Добавить условие…"));
      var groups = {};
      allowed.forEach(function (id) {
        var def = classes[id];
        if (!def) return;
        if (!groups[def.group]) {
          groups[def.group] = el("optgroup", { label: def.group });
          sel.appendChild(groups[def.group]);
        }
        groups[def.group].appendChild(el("option", { value: id }, def.label));
      });
      sel.addEventListener("change", function () {
        if (!sel.value) return;
        node.CHILDREN.push(newNode(sel.value));
        restructure();
      });
      return sel;
    }

    function control(head, node, f) {
      if (f.kind === "const") return;
      if (f.label) head.appendChild(el("span", { "class": "muted" }, f.label));
      var data = node.DATA;
      var c;
      if (f.kind === "choice" || f.kind === "multi") {
        var multi = f.kind === "multi";
        c = el("select", multi ? { multiple: "", size: String(Math.min(Math.max(f.options.length, 2), 6)) } : {});
        var current = (multi ? asList(getKey(data, f.key)) : [getKey(data, f.key)]).map(String);
        var seen = {};
        f.options.forEach(function (o) {
          var opt = el("option", { value: o[0] }, o[1]);
          opt.selected = current.indexOf(o[0]) >= 0;
          seen[o[0]] = true;
          c.appendChild(opt);
        });
        // Значение, которого нет в списке (удалено или пришло из Битрикса), не теряем
        current.forEach(function (v) {
          if (v === "undefined" || seen[v]) return;
          var opt = el("option", { value: v }, v + " (нет в списке)");
          opt.selected = true;
          c.appendChild(opt);
        });
        on(c, function () {
          if (!multi) { setKey(data, f.key, c.value); return; }
          setKey(data, f.key, Array.prototype.filter.call(c.options, function (o) { return o.selected; })
            .map(function (o) { return /^\d+$/.test(o.value) ? Number(o.value) : o.value; }));
        });
      } else if (f.kind === "number" || f.kind === "text") {
        c = el("input", { type: "text", "class": "dt-" + f.kind });
        var cur = getKey(data, f.key);
        c.value = cur === undefined || cur === null ? "" : String(cur);
        on(c, function () {
          var v = c.value.trim();
          setKey(data, f.key, f.kind === "number" && v !== "" && isFinite(v) ? Number(v) : c.value);
        });
      } else if (f.kind === "datetime") {
        c = el("input", { type: "datetime-local", "class": "dt-datetime", title: "по Москве" });
        c.value = toInput(getKey(data, f.key));
        on(c, function () { setKey(data, f.key, fromInput(c.value)); });
      } else if (f.kind === "ranges") {
        rangesControl(head, data, f);
        return;
      } else if (f.kind === "ids") {
        c = el("input", { type: "text", "class": "dt-ids", placeholder: f.single ? "id" : "id через запятую" });
        c.value = asList(getKey(data, f.key)).join(", ");
        var hint = el("span", { "class": "dt-hint" });
        var showHint = function () {
          hint.textContent = asList(getKey(data, f.key)).map(function (id) {
            var n = (names[f.lookup] || {})[id];
            if (n) return n;
            return asked[f.lookup][id] ? id + " — не найдено" : "";
          }).filter(Boolean).join("; ");
        };
        on(c, function () {
          var ids = c.value.split(/[\s,;]+/).filter(Boolean).map(function (s) {
            return /^\d+$/.test(s) ? Number(s) : s;
          });
          setKey(data, f.key, f.single ? (ids.length ? ids[0] : "") : ids);
        });
        c.addEventListener("change", function () { lookup(f.lookup, asList(getKey(data, f.key)), showHint); });
        head.appendChild(c);
        head.appendChild(hint);
        lookup(f.lookup, asList(getKey(data, f.key)), showHint);
        return;
      } else if (f.kind === "json") {
        c = el("textarea", { rows: "2", "class": "dt-json" });
        c.value = JSON.stringify(data);
        c.addEventListener("input", function () {
          try {
            node.DATA = JSON.parse(c.value);
            c.classList.remove("invalid");
            sync();
          } catch (e) {
            c.classList.add("invalid");
          }
        });
        head.appendChild(c);
        return;
      }
      head.appendChild(c);
    }

    // Пороги [{sum, value, type}]: строки добавляются и удаляются
    function rangesControl(head, data, f) {
      var box = el("div", { "class": "dt-ranges" });
      var rows = function () {
        var v = getKey(data, f.key);
        if (!Array.isArray(v)) { v = []; setKey(data, f.key, v); }
        return v;
      };
      var num = function (s) {
        var t = s.trim().replace(",", ".");
        return t !== "" && isFinite(t) ? Number(t) : s;
      };
      function draw() {
        box.replaceChildren();
        rows().forEach(function (r, i) {
          var line = el("div", { "class": "dt-range" });
          line.appendChild(el("span", { "class": "muted" }, "от"));
          var sum = el("input", { type: "text", "class": "dt-number" });
          sum.value = r.sum === undefined ? "" : String(r.sum);
          on(sum, function () { r.sum = num(sum.value); });
          var value = el("input", { type: "text", "class": "dt-number" });
          value.value = r.value === undefined ? "" : String(r.value);
          on(value, function () { r.value = num(value.value); });
          var type = el("select", {});
          [["P", "%"], ["F", "руб. на каждый товар"]].forEach(function (o) {
            var opt = el("option", { value: o[0] }, o[1]);
            opt.selected = r.type === o[0];
            type.appendChild(opt);
          });
          on(type, function () { r.type = type.value; });
          var del = el("button", { type: "button", "class": "link danger" }, "×");
          del.addEventListener("click", function () { rows().splice(i, 1); draw(); sync(); });
          [sum, el("span", { "class": "muted" }, "скидка"), value, type, del].forEach(function (x) { line.appendChild(x); });
          box.appendChild(line);
        });
        var add = el("button", { type: "button", "class": "link" }, "Добавить порог");
        add.addEventListener("click", function () {
          rows().push({ sum: 0, value: 0, type: "P" });
          draw();
          sync();
        });
        box.appendChild(add);
      }
      draw();
      head.appendChild(box);
    }

    function on(c, apply) {
      var handler = function () { apply(); sync(); };
      c.addEventListener("input", handler);
      c.addEventListener("change", handler);
    }

    redraw();
  }

  // Названия записей и разделов по id — подсказка у поля
  function lookup(kind, ids, done) {
    var map = names[kind] = names[kind] || {};
    var missing = ids.filter(function (id) {
      return typeof id === "number" && !map[id] && !asked[kind][id];
    });
    if (!missing.length) { done(); return; }
    fetch("/admin/discounts/names?" + kind + "=" + missing.join(","), { credentials: "same-origin" })
      .then(function (r) { return r.json(); })
      .then(function (res) {
        Object.assign(map, res[kind] || {});
        missing.forEach(function (id) { asked[kind][id] = true; });
        done();
      })
      .catch(done);
  }

  // Дерево не разобралось — правим JSON как есть
  function rawEditor(box, input) {
    var area = el("textarea", { rows: "6", "class": "dt-json" });
    area.value = input.value;
    area.addEventListener("input", function () { input.value = area.value; });
    box.replaceChildren(el("p", { "class": "dt-error" }, "Дерево не разобрано — исправьте JSON."), area);
  }
})();
