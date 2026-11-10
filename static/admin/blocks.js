// Редактор поля «Блоки»: карточки блоков поверх textarea с JSON.
// Без JS остаётся textarea — её видно, пока скрипт не добавил класс blocks-source.
(function () {
  "use strict";

  var LIMIT = 200;
  var TYPES = [
    ["heading", "Заголовок"],
    ["text", "Текст"],
    ["image", "Картинка"],
    ["gallery", "Галерея"],
    ["quote", "Цитата"],
    ["table", "Таблица"],
    ["divider", "Разделитель"],
    ["html", "HTML"],
  ];

  function parse(raw) {
    if (!raw || !raw.trim()) return { blocks: [], ok: true };
    try {
      var value = JSON.parse(raw);
      if (!Array.isArray(value)) return { blocks: [], ok: false };
      return { blocks: value, ok: true };
    } catch (error) {
      return { blocks: [], ok: false };
    }
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function init(area) {
    if (area.dataset.blocksReady === "1") return;
    area.dataset.blocksReady = "1";
    var collection = area.dataset.collection;
    var parsed = parse(area.value);
    var blocks = parsed.blocks;
    var root = el("div", "blocks");
    var status = el("div", "blocks-status");
    area.after(root);
    area.after(status);
    var rawToggle = el("button", "blocks-btn", "JSON");
    rawToggle.type = "button";
    rawToggle.title = "Показать исходный JSON";
    root.before(rawToggle);
    if (parsed.ok) area.classList.add("blocks-source");
    var dragFrom = null;

    function save() {
      area.value = JSON.stringify(blocks);
    }

    function hideRaw() {
      area.classList.add("blocks-source");
      root.hidden = false;
      rawToggle.textContent = "JSON";
    }

    function showRaw() {
      area.classList.remove("blocks-source");
      root.hidden = true;
      rawToggle.textContent = "Блоки";
      area.focus();
    }

    rawToggle.addEventListener("click", function () {
      if (area.classList.contains("blocks-source")) showRaw();
      else hideRaw();
    });

    function render() {
      root.textContent = "";
      blocks.forEach(function (block, index) {
        root.appendChild(card(block, index));
      });
      root.appendChild(bar());
      if (blocks.length >= LIMIT) {
        status.textContent = "Достигнут предел: не больше " + LIMIT + " блоков.";
      }
      save();
    }

    function field(label, value, onChange, multiline) {
      var wrap = el("label", "blocks-field");
      wrap.appendChild(el("span", "blocks-label", label));
      var input = el(multiline ? "textarea" : "input");
      if (multiline) input.rows = 3;
      input.value = value || "";
      input.addEventListener("input", function () {
        onChange(input.value);
        save();
      });
      wrap.appendChild(input);
      return wrap;
    }

    function upload(onPath) {
      var wrap = el("div", "blocks-field");
      wrap.appendChild(el("span", "blocks-label", "Загрузить картинку"));
      var file = el("input");
      file.type = "file";
      file.accept = "image/*";
      file.addEventListener("change", function () {
        if (!file.files || !file.files[0]) return;
        var form = new FormData();
        form.append("file", file.files[0]);
        fetch("/admin/blocks/upload?collection=" + encodeURIComponent(collection), {
          method: "POST",
          body: form,
        })
          .then(function (response) {
            return response.json();
          })
          .then(function (data) {
            if (data && data.data && data.data.path) {
              onPath(data.data.path);
              render();
            } else {
              alert("Не удалось загрузить картинку");
            }
          })
          .catch(function () {
            alert("Не удалось загрузить картинку");
          });
      });
      wrap.appendChild(file);
      return wrap;
    }

    function card(block, index) {
      var node = el("div", "blocks-card");
      node.draggable = true;
      node.addEventListener("dragstart", function () {
        dragFrom = index;
        node.classList.add("dragging");
      });
      node.addEventListener("dragend", function () {
        node.classList.remove("dragging");
      });
      node.addEventListener("dragover", function (event) {
        event.preventDefault();
      });
      node.addEventListener("drop", function (event) {
        event.preventDefault();
        if (dragFrom === null || dragFrom === index) return;
        var moved = blocks.splice(dragFrom, 1)[0];
        blocks.splice(index, 0, moved);
        dragFrom = null;
        render();
      });

      var head = el("div", "blocks-head");
      head.appendChild(el("span", "blocks-grip", "⋮⋮"));
      var known = TYPES.filter(function (type) {
        return type[0] === block.type;
      })[0];
      head.appendChild(el("strong", null, known ? known[1] : "Неизвестный блок: " + block.type));
      var up = el("button", "blocks-btn", "↑");
      up.type = "button";
      up.addEventListener("click", function () {
        if (index === 0) return;
        blocks.splice(index - 1, 0, blocks.splice(index, 1)[0]);
        render();
      });
      var down = el("button", "blocks-btn", "↓");
      down.type = "button";
      down.addEventListener("click", function () {
        if (index === blocks.length - 1) return;
        blocks.splice(index + 1, 0, blocks.splice(index, 1)[0]);
        render();
      });
      var copy = el("button", "blocks-btn", "⧉");
      copy.type = "button";
      copy.addEventListener("click", function () {
        blocks.splice(index + 1, 0, JSON.parse(JSON.stringify(block)));
        render();
      });
      var remove = el("button", "blocks-btn danger", "✕");
      remove.type = "button";
      remove.addEventListener("click", function () {
        blocks.splice(index, 1);
        render();
      });
      [up, down, copy, remove].forEach(function (button) {
        head.appendChild(button);
      });
      node.appendChild(head);

      var body = el("div", "blocks-body");
      if (!known) {
        body.appendChild(
          el("p", "blocks-note", "Блок неизвестного типа не редактируется — удалите его или исправьте JSON.")
        );
      } else if (block.type === "heading") {
        body.appendChild(
          field("Заголовок", block.heading, function (value) {
            block.heading = value;
          })
        );
        body.appendChild(
          field("Уровень (2, 3 или 4)", block.level || 2, function (value) {
            block.level = Number(value) || 2;
          })
        );
      } else if (block.type === "text") {
        body.appendChild(
          field("Текст (абзацы — пустой строкой)", block.text, function (value) {
            block.text = value;
          }, true)
        );
      } else if (block.type === "image") {
        body.appendChild(upload(function (path) {
          block.image = path;
        }));
        body.appendChild(
          field("Путь", block.image, function (value) {
            block.image = value;
          })
        );
        body.appendChild(
          field("Alt", block.alt, function (value) {
            block.alt = value;
          })
        );
        body.appendChild(
          field("Подпись", block.caption, function (value) {
            block.caption = value;
          })
        );
        body.appendChild(
          field("Ссылка", block.link, function (value) {
            block.link = value;
          })
        );
      } else if (block.type === "gallery") {
        body.appendChild(upload(function (path) {
          block.images = (block.images || []).concat([{ image: path }]);
        }));
        var existing = block.images || [];
        var paths = existing
          .map(function (image) {
            return image.image || "";
          })
          .join("\n");
        body.appendChild(
          field("Пути картинок, по одному в строке", paths, function (value) {
            block.images = value
              .split("\n")
              .map(function (line, position) {
                return line.trim() ? { src: line.trim(), base: existing[position] } : null;
              })
              .filter(Boolean)
              .map(function (item) {
                var base = item.base || {};
                var image = { image: item.src };
                if (base.alt) image.alt = base.alt;
                if (base.caption) image.caption = base.caption;
                return image;
              });
          }, true)
        );
      } else if (block.type === "quote") {
        body.appendChild(
          field("Цитата", block.quote, function (value) {
            block.quote = value;
          }, true)
        );
        body.appendChild(
          field("Автор", block.author, function (value) {
            block.author = value;
          })
        );
      } else if (block.type === "table") {
        body.appendChild(
          field("Строки — JSON: массив массивов", JSON.stringify(block.rows || [], null, 2), function (value) {
            try {
              var rows = JSON.parse(value);
              if (Array.isArray(rows)) block.rows = rows;
            } catch (error) {
              /* оставляем прежние строки, пока JSON не станет валидным */
            }
          }, true)
        );
      } else if (block.type === "html") {
        body.appendChild(
          field("HTML (очищается при сохранении)", block.html, function (value) {
            block.html = value;
          }, true)
        );
      }
      node.appendChild(body);
      return node;
    }

    function bar() {
      var node = el("div", "blocks-add");
      var select = el("select");
      var empty = el("option", null, "Добавить блок…");
      empty.value = "";
      select.appendChild(empty);
      TYPES.forEach(function (type) {
        var option = el("option", null, type[1]);
        option.value = type[0];
        select.appendChild(option);
      });
      select.disabled = blocks.length >= LIMIT;
      select.addEventListener("change", function () {
        if (!select.value) return;
        var block = { type: select.value };
        if (select.value === "heading") block.level = 2;
        blocks.push(block);
        render();
      });
      node.appendChild(select);
      return node;
    }

    render();
  }

  function start(rootNode) {
    var scope = rootNode && rootNode.querySelectorAll ? rootNode : document;
    var areas = [];
    if (scope.matches && scope.matches("textarea.blocks-input")) areas.push(scope);
    scope.querySelectorAll("textarea.blocks-input").forEach(function (area) {
      areas.push(area);
    });
    areas.forEach(init);
  }

  document.addEventListener("htmx:load", function (event) {
    start(event.target);
  });
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      start(document);
    });
  } else {
    start(document);
  }
})();
