// Редактор поля «Блоки»: карточки блоков поверх textarea с JSON.
// Без JS остаётся textarea — её видно, пока скрипт не добавил класс blocks-source.
(function () {
  "use strict";

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
    try {
      var value = JSON.parse(raw || "[]");
      return Array.isArray(value) ? value : [];
    } catch (error) {
      return [];
    }
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function init(area) {
    var collection = area.dataset.collection;
    var blocks = parse(area.value);
    var root = el("div", "blocks");
    area.after(root);
    area.classList.add("blocks-source");
    var dragFrom = null;

    function save() {
      area.value = JSON.stringify(blocks);
    }

    function render() {
      root.textContent = "";
      blocks.forEach(function (block, index) {
        root.appendChild(card(block, index));
      });
      root.appendChild(bar());
      save();
    }

    function textField(label, value, onChange, multiline) {
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

    function selectField(label, value, options, onChange) {
      var wrap = el("label", "blocks-field");
      wrap.appendChild(el("span", "blocks-label", label));
      var select = el("select");
      options.forEach(function (option) {
        var item = el("option", null, option[1]);
        item.value = option[0];
        if (String(option[0]) === String(value)) item.selected = true;
        select.appendChild(item);
      });
      select.addEventListener("change", function () {
        onChange(select.value);
        save();
      });
      wrap.appendChild(select);
      return wrap;
    }

    function uploadField(block) {
      var wrap = el("div", "blocks-field");
      wrap.appendChild(el("span", "blocks-label", "Картинка"));
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
              block.image = data.data.path;
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
      var title = TYPES.filter(function (type) {
        return type[0] === block.type;
      })[0];
      head.appendChild(el("strong", null, title ? title[1] : block.type));

      var up = el("button", "blocks-btn", "↑");
      up.type = "button";
      up.title = "Выше";
      up.addEventListener("click", function () {
        if (index === 0) return;
        blocks.splice(index - 1, 0, blocks.splice(index, 1)[0]);
        render();
      });
      var down = el("button", "blocks-btn", "↓");
      down.type = "button";
      down.title = "Ниже";
      down.addEventListener("click", function () {
        if (index === blocks.length - 1) return;
        blocks.splice(index + 1, 0, blocks.splice(index, 1)[0]);
        render();
      });
      var copy = el("button", "blocks-btn", "⧉");
      copy.type = "button";
      copy.title = "Дублировать";
      copy.addEventListener("click", function () {
        blocks.splice(index + 1, 0, JSON.parse(JSON.stringify(block)));
        render();
      });
      var remove = el("button", "blocks-btn danger", "✕");
      remove.type = "button";
      remove.title = "Удалить";
      remove.addEventListener("click", function () {
        blocks.splice(index, 1);
        render();
      });
      [up, down, copy, remove].forEach(function (button) {
        head.appendChild(button);
      });
      node.appendChild(head);

      var body = el("div", "blocks-body");
      if (block.type === "heading") {
        body.appendChild(
          textField("Заголовок", block.heading, function (value) {
            block.heading = value;
          })
        );
        body.appendChild(
          selectField(
            "Уровень",
            block.level || 2,
            [["2", "H2"], ["3", "H3"], ["4", "H4"]],
            function (value) {
              block.level = Number(value);
            }
          )
        );
      } else if (block.type === "text") {
        body.appendChild(
          textField("Текст (абзацы — пустой строкой)", block.text, function (value) {
            block.text = value;
          }, true)
        );
      } else if (block.type === "image") {
        body.appendChild(uploadField(block));
        body.appendChild(
          textField("Путь", block.image, function (value) {
            block.image = value;
          })
        );
        body.appendChild(
          textField("Alt", block.alt, function (value) {
            block.alt = value;
          })
        );
        body.appendChild(
          textField("Подпись", block.caption, function (value) {
            block.caption = value;
          })
        );
        body.appendChild(
          textField("Ссылка", block.link, function (value) {
            block.link = value;
          })
        );
      } else if (block.type === "gallery") {
        var paths = (block.images || [])
          .map(function (image) {
            return image.image || "";
          })
          .join("\n");
        body.appendChild(
          textField("Пути картинок, по одному в строке", paths, function (value) {
            block.images = value
              .split("\n")
              .map(function (line) {
                return line.trim();
              })
              .filter(Boolean)
              .map(function (src) {
                return { image: src };
              });
          }, true)
        );
      } else if (block.type === "quote") {
        body.appendChild(
          textField("Цитата", block.quote, function (value) {
            block.quote = value;
          }, true)
        );
        body.appendChild(
          textField("Автор", block.author, function (value) {
            block.author = value;
          })
        );
      } else if (block.type === "table") {
        var rows = (block.rows || [])
          .map(function (row) {
            return row.join(" | ");
          })
          .join("\n");
        body.appendChild(
          textField("Строки, ячейки через |", rows, function (value) {
            block.rows = value
              .split("\n")
              .filter(function (line) {
                return line.trim() !== "";
              })
              .map(function (line) {
                return line.split("|").map(function (cell) {
                  return cell.trim();
                });
              });
          }, true)
        );
      } else if (block.type === "html") {
        body.appendChild(
          textField("HTML (очищается)", block.html, function (value) {
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

  function start() {
    document.querySelectorAll("textarea.blocks-input").forEach(init);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
