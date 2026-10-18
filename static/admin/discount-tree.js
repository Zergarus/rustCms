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

  function newNode(classId) {
    var data = {};
    classes[classId].fields.forEach(function (f) {
      if (f.kind !== "json") data[f.key] = clone(f.default);
    });
    return { CLASS_ID: classId, DATA: data, CHILDREN: [] };
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
      if (f.label) head.appendChild(el("span", { "class": "muted" }, f.label));
      var data = node.DATA;
      var c;
      if (f.kind === "choice" || f.kind === "multi") {
        var multi = f.kind === "multi";
        c = el("select", multi ? { multiple: "", size: String(Math.min(Math.max(f.options.length, 2), 6)) } : {});
        var current = (multi ? asList(data[f.key]) : [data[f.key]]).map(String);
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
          if (!multi) { data[f.key] = c.value; return; }
          data[f.key] = Array.prototype.filter.call(c.options, function (o) { return o.selected; })
            .map(function (o) { return /^\d+$/.test(o.value) ? Number(o.value) : o.value; });
        });
      } else if (f.kind === "number" || f.kind === "text") {
        c = el("input", { type: "text", "class": "dt-" + f.kind });
        c.value = data[f.key] === undefined ? "" : String(data[f.key]);
        on(c, function () {
          var v = c.value.trim();
          data[f.key] = f.kind === "number" && v !== "" && isFinite(v) ? Number(v) : c.value;
        });
      } else if (f.kind === "ids") {
        c = el("input", { type: "text", "class": "dt-ids", placeholder: "id через запятую" });
        c.value = asList(data[f.key]).join(", ");
        var hint = el("span", { "class": "dt-hint" });
        var showHint = function () {
          hint.textContent = asList(data[f.key]).map(function (id) {
            var n = (names[f.lookup] || {})[id];
            if (n) return n;
            return asked[f.lookup][id] ? id + " — не найдено" : "";
          }).filter(Boolean).join("; ");
        };
        on(c, function () {
          data[f.key] = c.value.split(/[\s,;]+/).filter(Boolean).map(function (s) {
            return /^\d+$/.test(s) ? Number(s) : s;
          });
        });
        c.addEventListener("change", function () { lookup(f.lookup, asList(data[f.key]), showHint); });
        head.appendChild(c);
        head.appendChild(hint);
        lookup(f.lookup, asList(data[f.key]), showHint);
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
