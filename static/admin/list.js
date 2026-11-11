/* Списки админки: поповеры фильтров и колонок, клавиши. Только надстройка над серверными ссылками:
   без этого файла списки работают (чипы, сортировка, страницы — обычные hx-get ссылки).
   Все обработчики document ставятся один раз и переживают замену #list-body. */
(function () {
  'use strict';
  if (window.__listJs) return;
  window.__listJs = true;
  document.documentElement.classList.add('list-js');

  var $ = function (sel, root) { return (root || document).querySelector(sel); };
  var $$ = function (sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); };
  var esc = function (s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  };

  /* ---------- Спека и адреса ---------- */

  function spec() {
    var el = $('#list-spec');
    try { return el ? JSON.parse(el.textContent) : null; } catch (e) { return null; }
  }
  function listPage() { return !!$('#list-body') && !!$('#list-spec'); }
  function filterDef(code) {
    var s = spec();
    return s && s.filters.filter(function (f) { return f.code === code; })[0];
  }

  // Канонический адрес текущего состояния: сервер отдаёт его в hx-get тела списка
  function baseHref() {
    var b = $('#list-body');
    return (b && b.getAttribute('hx-get')) || location.pathname + location.search;
  }

  // Новый адрес: правка параметров, сброс page; при открытом виде пустое состояние помечается edited=1
  function buildUrl(mutate) {
    var u = new URL(baseHref(), location.origin);
    var p = u.searchParams;
    mutate(p);
    p.delete('page');
    p.delete('edited');
    if (p.has('view')) {
      var blank = true;
      p.forEach(function (_, k) { if (k !== 'view' && k !== 'per') blank = false; });
      if (blank) p.set('edited', '1');
    }
    var qs = p.toString();
    return u.pathname + (qs ? '?' + qs : '');
  }

  // Применить: фрагмент в #list-body, адрес — после успешной замены
  function go(url, keepPop) {
    if (!window.htmx) { location.href = url; return null; }
    if (!keepPop) closePop();
    closeDetails();
    var before = location.pathname + location.search;
    var old = document.getElementById('list-body');
    return window.htmx.ajax('GET', url, { target: '#list-body', swap: 'outerHTML' }).then(function () {
      // Запрос не удался (не 2xx, нет замены): адрес и история остаются прежними
      if (old && old.isConnected) return;
      var after = location.pathname + location.search;
      // состояние {htmx:true}: так htmx узнаёт записи истории и при «назад/вперёд» подгружает страницу по адресу
      history.replaceState({ htmx: true }, '', before);
      if (after !== before) {
        // сервер заменил адрес (HX-Replace-Url): прежняя запись остаётся с прежним адресом, нормализованный — новая
        history.pushState({ htmx: true }, '', after);
      } else if (url !== before) {
        history.pushState({ htmx: true }, '', url);
      }
    }, function () { /* сбой запроса: ничего не менять */ });
  }

  function currentFilter(code) {
    var v = new URL(baseHref(), location.origin).searchParams.get('f.' + code);
    if (v === null) return null;
    var i = v.indexOf(':');
    return { op: i < 0 ? v : v.slice(0, i), raw: i < 0 ? '' : v.slice(i + 1) };
  }

  function currentCols() {
    var s = spec();
    var v = new URL(baseHref(), location.origin).searchParams.get('cols');
    if (v) return v.split(',').filter(Boolean);
    return s.columns.filter(function (c) { return c.default; }).map(function (c) { return c.code; });
  }

  /* ---------- Поповер ---------- */

  var pop = null;

  function closePop() {
    if (pop) { pop.remove(); pop = null; }
  }

  function openPop(anchor, html, cls) {
    closePop();
    pop = document.createElement('div');
    pop.className = 'pop' + (cls ? ' ' + cls : '');
    pop.setAttribute('role', 'dialog');
    pop.innerHTML = html;
    document.body.appendChild(pop);
    placePop(anchor);
    return pop;
  }

  function placePop(anchor) {
    if (!pop) return;
    var r = anchor.getBoundingClientRect();
    var w = pop.offsetWidth, h = pop.offsetHeight;
    var top = r.bottom + 6;
    if (top + h > window.innerHeight - 8) top = Math.max(8, window.innerHeight - h - 8);
    pop.style.top = top + 'px';
    pop.style.left = Math.max(8, Math.min(r.left, window.innerWidth - w - 8)) + 'px';
  }

  function closeDetails(except) {
    $$('details.view-menu[open], details[data-view-save-as][open], details.confirm[open]').forEach(function (d) {
      if (d !== except && !(except && d.contains(except))) d.open = false;
    });
  }
  function detailsOpen() {
    return !!$('details.view-menu[open], details[data-view-save-as][open], details.confirm[open]');
  }

  /* ---------- Фильтры ---------- */

  var OPS = {
    text: [['has', 'содержит'], ['eq', 'равно'], ['empty', 'пусто'], ['notempty', 'не пусто']],
    number: [['eq', '='], ['ne', '≠'], ['gt', '>'], ['lt', '<'], ['between', 'между'], ['empty', 'пусто']],
    date: [['today', 'сегодня'], ['d7', 'за 7 дней'], ['d30', 'за 30 дней'], ['range', 'период']],
    enum: [['in', 'любой из'], ['nin', 'ни один из']],
    link: [['eq', 'равно'], ['empty', 'пусто'], ['notempty', 'не пусто']],
    file: [['empty', 'пусто'], ['notempty', 'не пусто']]
  };
  OPS.money = OPS.number;
  var NO_VALUE = ['empty', 'notempty', 'today', 'd7', 'd30'];
  var KIND = { text: 'строка', number: 'число', money: 'число', enum: 'список', bool: 'да/нет', date: 'дата', link: 'связь', file: 'файл' };

  function pickerHtml(q) {
    var s = spec(), g = null, out = '';
    var list = s.filters.filter(function (f) { return f.label.toLowerCase().indexOf(q.toLowerCase()) >= 0; });
    list.forEach(function (f) {
      if (f.group !== g) { g = f.group; out += '<div class="sec">' + esc(g) + '</div>'; }
      out += '<button type="button" class="item" data-pick="' + esc(f.code) + '">' + esc(f.label) +
        '<span class="kind">' + esc(KIND[f.ty] || '') + '</span></button>';
    });
    return out || '<div class="item muted">Нет такого поля</div>';
  }

  // Применённые условия из чипов над таблицей: их видно и можно снять прямо в диалоге.
  function appliedFilters() {
    return $$('.list-bar .chip').filter(function (chip) {
      var main = $('[data-chip]', chip);
      return main && main.dataset.chip;
    }).map(function (chip) {
      var main = $('[data-chip]', chip);
      var lbl = $('.lbl', main), op = $('.op', main), val = $('.val', main);
      return {
        code: main.dataset.chip,
        label: lbl ? lbl.textContent : main.dataset.chip,
        text: [op ? op.textContent : '', val ? val.textContent : ''].join(' ').trim(),
        url: ($('.chip-x', chip) || {}).getAttribute ? $('.chip-x', chip).getAttribute('href') : ''
      };
    });
  }

  function appliedHtml() {
    var list = appliedFilters();
    if (!list.length) return '';
    return '<div class="sec">Применённые условия</div>' + list.map(function (f) {
      return '<div class="applied"><button type="button" class="item grow" data-edit-chip="' + esc(f.code) + '">' +
        esc(f.label) + (f.text ? '<span class="kind">' + esc(f.text) + '</span>' : '') +
        '</button><a class="chip-x" href="' + esc(f.url) + '" aria-label="Убрать условие">✕</a></div>';
    }).join('') +
      '<div class="row"><button type="button" class="btn ghost sm" data-add-condition>+ Добавить условие</button></div>';
  }

  function pickerForm(anchor, withApplied) {
    var p = openPop(anchor, (withApplied ? appliedHtml() : '') +
      '<div class="row"><input type="text" id="fq" placeholder="Поле…" autocomplete="off" aria-label="Поле"></div>' +
      '<div class="list" id="fl">' + pickerHtml('') + '</div>');
    var q = $('#fq', p);
    if (q) q.focus();
    if (q) q.addEventListener('input', function () { $('#fl', p).innerHTML = pickerHtml(q.value); });
    p.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); var b = $('[data-pick]', p); if (b) b.click(); }
    });
    p.addEventListener('click', function (e) {
      var b = e.target.closest('[data-pick]');
      if (b) { filterEditor(anchor, b.dataset.pick); return; }
      var edit = e.target.closest('[data-edit-chip]');
      if (edit) filterEditor(anchor, edit.dataset.editChip);
    });
  }

  function appliedPanel(anchor) {
    var p = openPop(anchor, appliedHtml());
    p.addEventListener('click', function (e) {
      var edit = e.target.closest('[data-edit-chip]');
      if (edit) filterEditor(anchor, edit.dataset.editChip);
    });
  }

  function filterPicker(anchor) {
    if (appliedFilters().length) appliedPanel(anchor);
    else pickerForm(anchor, false);
  }

  function filterEditor(anchor, code) {
    var def = filterDef(code);
    if (!def) return;
    var cur = currentFilter(code);
    var ty = def.ty;
    var ops = OPS[ty] || [];
    var op = ty === 'bool' ? 'eq' : (cur && ops.some(function (o) { return o[0] === cur.op; }) ? cur.op : ops[0][0]);
    var raw = cur && cur.op === op ? cur.raw : '';
    var many = ty === 'enum' ? (raw ? raw.split(',') : []) : [];
    var pair = (op === 'between' || op === 'range') ? raw.split(',') : ['', ''];
    var single = (op === 'between' || op === 'range' || ty === 'enum') ? '' : raw;

    var p = openPop(anchor, '');
    function remember() {
      if (ty === 'enum') many = $$('.list input:checked', p).map(function (x) { return x.value; });
      if ($('#v', p)) single = $('#v', p).value;
      if ($('#v0', p)) pair = [$('#v0', p).value, $('#v1', p).value];
    }
    function body() {
      var h = '<div class="title">' + esc(def.label) + '</div>';
      if (ty === 'bool') {
        var on = cur && cur.raw;
        return h + '<div class="ops"><button type="button" data-bool="Y" class="' + (on === 'Y' ? 'on' : '') + '">да</button>' +
          '<button type="button" data-bool="N" class="' + (on === 'N' ? 'on' : '') + '">нет</button></div>';
      }
      h += '<div class="ops">' + ops.map(function (o) {
        return '<button type="button" data-op="' + o[0] + '" class="' + (o[0] === op ? 'on' : '') + '">' + esc(o[1]) + '</button>';
      }).join('') + '</div>';
      if (NO_VALUE.indexOf(op) >= 0) return h;
      if (ty === 'enum') {
        return h + '<div class="row"><input type="text" id="eq" placeholder="Найти вариант…" autocomplete="off" aria-label="Найти вариант"></div><div class="list">' +
          def.options.map(function (o) {
            return '<label class="item"><input type="checkbox" value="' + esc(o.value) + '"' + (many.indexOf(o.value) >= 0 ? ' checked' : '') + '>' + esc(o.label) + '</label>';
          }).join('') + '</div>';
      }
      if (op === 'between' || op === 'range') {
        var t = op === 'range' ? 'date' : 'number';
        return h + '<div class="row"><input type="' + t + '" id="v0" value="' + esc(pair[0] || '') + '" aria-label="От">' +
          '<span class="muted">—</span><input type="' + t + '" id="v1" value="' + esc(pair[1] || '') + '" aria-label="До"></div>';
      }
      var inpType = (ty === 'number' || ty === 'money' || ty === 'link') ? 'number' : 'text';
      return h + '<div class="row"><input type="' + inpType + '"' + (inpType === 'number' ? ' step="any"' : '') + ' id="v" value="' + esc(single) + '" placeholder="Значение" autocomplete="off" aria-label="Значение"></div>';
    }
    function draw() {
      p.innerHTML = body() + (ty === 'bool' ? (cur ? '<div class="actions"><button type="button" class="btn ghost sm" data-rm>Убрать</button></div>' : '') :
        '<div class="actions">' + (cur ? '<button type="button" class="btn ghost sm" data-rm>Убрать</button>' : '') +
        '<button type="button" class="btn primary sm" data-apply>Применить</button></div>');
      var first = $('input[type=text], input[type=number], input[type=date]', p);
      if (first) { first.focus(); if (first.select && first.type !== 'date' && first.type !== 'number') first.select(); }
      var eq = $('#eq', p);
      if (eq) eq.addEventListener('input', function () {
        $$('.list label', p).forEach(function (l) { l.hidden = l.textContent.toLowerCase().indexOf(eq.value.toLowerCase()) < 0; });
      });
      placePop(anchor);
    }
    var hintText = '';
    function value() {
      remember();
      hintText = 'Введите значение';
      if (NO_VALUE.indexOf(op) >= 0) return op;
      if (ty === 'enum') { hintText = 'Выберите хотя бы один вариант'; return many.length ? op + ':' + many.join(',') : null; }
      if (op === 'between' || op === 'range') {
        var a = pair[0], b = pair[1];
        if (op === 'between' ? (a === '' || b === '') : (!a && !b)) { hintText = 'Заполните оба значения'; if (op === 'range') hintText = 'Укажите дату от или до'; return null; }
        if (a !== '' && b !== '' && (op === 'between' ? parseFloat(a) > parseFloat(b) : a > b)) { hintText = 'Начало больше конца'; return null; }
        return op + ':' + a + ',' + b;
      }
      return single.trim() !== '' ? op + ':' + single.trim() : null;
    }
    function apply(v) {
      if (v === null) {
        var hint = $('.pop-hint', p);
        if (!hint) { hint = document.createElement('div'); hint.className = 'pop-hint'; hint.setAttribute('role', 'alert'); var act = $('.actions', p); p.insertBefore(hint, act); }
        hint.textContent = hintText;
        var inp = $('input[type=text], input[type=number], input[type=date]', p); if (inp) inp.focus();
        placePop(anchor);
        return;
      }
      go(buildUrl(function (q) { q.set('f.' + code, v); }));
    }
    draw();
    p.addEventListener('click', function (e) {
      var o = e.target.closest('[data-op]');
      if (o) {
        remember();
        op = o.dataset.op;
        if (op === 'between' || op === 'range') pair = ['', ''];
        draw();
        return;
      }
      var b = e.target.closest('[data-bool]');
      if (b) { apply('eq:' + b.dataset.bool); return; }
      if (e.target.closest('[data-apply]')) apply(value());
      if (e.target.closest('[data-rm]')) go(buildUrl(function (q) { q.delete('f.' + code); }));
    });
    p.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && e.target.tagName === 'INPUT' && e.target.type !== 'checkbox' && ty !== 'bool') { e.preventDefault(); apply(value()); }
    });
  }

  /* ---------- Колонки ---------- */

  function colsPicker(anchor) {
    var s = spec();
    var defaults = s.columns.filter(function (c) { return c.default; }).map(function (c) { return c.code; });
    function info(code) { return s.columns.filter(function (c) { return c.code === code; })[0]; }
    function html() {
      var cur = currentCols();
      var ordered = cur.concat(s.columns.map(function (c) { return c.code; }).filter(function (c) { return cur.indexOf(c) < 0; }));
      return '<div class="title">Колонки</div><div class="list">' + ordered.map(function (code) {
        var c = info(code);
        if (!c) return '';
        return '<label class="item" draggable="true" data-col="' + esc(code) + '"><span class="drag" aria-hidden="true">⋮⋮</span>' +
          '<input type="checkbox"' + (cur.indexOf(code) >= 0 ? ' checked' : '') + (c.required ? ' disabled' : '') + '>' + esc(c.title) + '</label>';
      }).join('') + '</div><div class="actions"><button type="button" class="btn ghost sm" data-defcols>По умолчанию</button></div>';
    }
    var p = openPop(anchor, html(), 'pop-cols');
    var drag = null;
    function sync() {
      var cols = $$('[data-col]', p).filter(function (l) { return $('input', l).checked; }).map(function (l) { return l.dataset.col; });
      if (!cols.length) return;
      go(buildUrl(function (q) {
        if (cols.join() === defaults.join()) q.delete('cols'); else q.set('cols', cols.join(','));
      }), true);
    }
    p.addEventListener('change', sync);
    p.addEventListener('click', function (e) {
      if (!e.target.closest('[data-defcols]')) return;
      go(buildUrl(function (q) { q.delete('cols'); }), true);
      var list = $('.list', p);
      var rest = s.columns.map(function (c) { return c.code; }).filter(function (c) { return defaults.indexOf(c) < 0; });
      list.innerHTML = defaults.concat(rest).map(function (code) {
        var c = info(code);
        return '<label class="item" draggable="true" data-col="' + esc(code) + '"><span class="drag" aria-hidden="true">⋮⋮</span>' +
          '<input type="checkbox"' + (defaults.indexOf(code) >= 0 ? ' checked' : '') + (c.required ? ' disabled' : '') + '>' + esc(c.title) + '</label>';
      }).join('');
    });
    p.addEventListener('dragstart', function (e) {
      drag = e.target.closest && e.target.closest('[data-col]');
      if (drag && e.dataTransfer) { e.dataTransfer.effectAllowed = 'move'; e.dataTransfer.setData('text/plain', drag.dataset.col); }
    });
    p.addEventListener('dragover', function (e) {
      e.preventDefault();
      var t = e.target.closest && e.target.closest('[data-col]');
      if (t && drag && t !== drag) {
        var r = t.getBoundingClientRect();
        t.parentNode.insertBefore(drag, e.clientY < r.top + r.height / 2 ? t : t.nextSibling);
      }
    });
    p.addEventListener('drop', function (e) { e.preventDefault(); drag = null; sync(); });
    // перетаскивание отменено (Esc, бросили вне списка): порядок возвращается к текущему из адреса
    p.addEventListener('dragend', function () {
      if (!drag) return;
      drag = null;
      var list = $('.list', p);
      if (list) list.innerHTML = $('.list', document.createRange().createContextualFragment(html())).innerHTML;
    });
  }

  /* ---------- События мыши ---------- */

  document.addEventListener('click', function (e) {
    var t = e.target;
    // цель могла исчезнуть из DOM при перерисовке поповера (клик по пункту) — это не клик снаружи
    if (!t.closest || !t.isConnected) return;
    if (pop && !pop.contains(t) && !t.closest('#list-add-filter, [data-chip], #list-cols, #bulk-bar')) closePop();
    closeDetails(t);
    if (!listPage()) return;
    var add = t.closest('#list-add-filter');
    if (add) { filterPicker(add); return; }
    if (t.closest('[data-add-condition]')) {
      var anchor = $('#list-add-filter');
      if (anchor) pickerForm(anchor, false);
      return;
    }
    var chip = t.closest('[data-chip]');
    if (chip) { filterEditor(chip, chip.dataset.chip); return; }
    var cols = t.closest('#list-cols');
    if (cols) { if (pop && pop.classList.contains('pop-cols')) closePop(); else colsPicker(cols); return; }
  });

  /* ---------- Выбор строк и массовые действия ---------- */

  var sel = new Set();      // id отмеченных строк текущей страницы
  var lastId = null;        // последняя отмеченная строка: начало диапазона
  var allFilter = false;    // выбраны все записи по фильтру
  var busy = false;         // массовое действие выполняется
  var barSig = '';          // размер выбора и охват, под которые нарисована панель

  function bulkSpec() {
    var s = spec();
    return s && s.bulk_actions && s.bulk_actions.length ? s : null;
  }
  function bulkRows() { return $$('#list-body tbody tr[data-id]'); }
  function bodyData(k) { var b = $('#list-body'); return b ? b.dataset[k] : undefined; }
  function totalCount() { return parseInt(bodyData('total'), 10) || 0; }
  function fmt(n) { return Number(n).toLocaleString('ru-RU'); }
  function plural(n, one, few, many) {
    var m = n % 100, d = n % 10;
    if (m > 10 && m < 20) return many;
    return d === 1 ? one : (d >= 2 && d <= 4 ? few : many);
  }
  function selCount() { return allFilter ? totalCount() : sel.size; }

  function rowBox(row) { return $('input[data-bulk]', row); }

  function setRow(row, on) {
    var box = rowBox(row);
    if (!box) return;
    box.checked = on;
    row.classList.toggle('sel', on);
    if (on) sel.add(row.dataset.id); else { sel.delete(row.dataset.id); allFilter = false; }
  }

  // Диапазон от прежней отмеченной строки до этой: всем состояние `on`
  function setRange(fromId, row, on) {
    var rows = bulkRows();
    var a = -1, b = rows.indexOf(row);
    rows.forEach(function (r, k) { if (r.dataset.id === fromId) a = k; });
    if (a < 0 || b < 0) { setRow(row, on); return; }
    for (var k = Math.min(a, b); k <= Math.max(a, b); k++) setRow(rows[k], on);
  }

  function syncBulk() {
    var rows = bulkRows();
    var present = {};
    rows.forEach(function (r) { present[r.dataset.id] = true; });
    sel.forEach(function (id) { if (!present[id]) sel.delete(id); });
    rows.forEach(function (r) {
      var on = sel.has(r.dataset.id), box = rowBox(r);
      if (box) box.checked = on;
      r.classList.toggle('sel', on);
    });
    var head = $('#list-body input[data-bulk-all]');
    if (head) {
      head.checked = rows.length > 0 && sel.size === rows.length;
      head.indeterminate = sel.size > 0 && sel.size < rows.length;
    }
    drawBar();
  }

  function clearSel() {
    sel.clear();
    lastId = null;
    allFilter = false;
    syncBulk();
  }

  function drawBar() {
    var bar = $('#bulk-bar');
    // изменился объём выбора: открытое подтверждение говорило о прежнем
    var sig = sel.size + ':' + allFilter;
    if (sig !== barSig && pop && pop.classList.contains('pop-bulk') && !busy) closePop();
    barSig = sig;
    document.body.classList.toggle('has-bulk-bar', sel.size > 0);
    if (!sel.size) {
      if (bar) bar.remove();
      if (pop && pop.classList.contains('pop-bulk')) closePop();
      return;
    }
    if (!bar) {
      bar = document.createElement('div');
      bar.id = 'bulk-bar';
      bar.className = 'bulk-bar';
      bar.setAttribute('role', 'region');
      bar.setAttribute('aria-label', 'Массовые действия');
      document.body.appendChild(bar);
    }
    var rows = bulkRows(), total = totalCount();
    var h = '<span class="bulk-n">' + (allFilter ? 'Выбраны все ' + fmt(total) + ' по фильтру' : 'Выбрано: ' + sel.size) + '</span>';
    if (allFilter) {
      h += '<button type="button" class="btn ghost sm" data-bulk-page>Только эта страница</button>';
    } else if (rows.length > 0 && sel.size === rows.length && total > rows.length) {
      h += bodyData('capped') === '1'
        ? '<span class="muted">Больше 10 000 — уточните фильтр</span>'
        : '<button type="button" class="btn ghost sm" data-bulk-filter>Выбрать все ' + fmt(total) + ' по фильтру</button>';
    }
    h += '<button type="button" class="btn sm" id="bulk-actions-btn" aria-haspopup="dialog"' + (busy ? ' disabled' : '') + '>' +
      (busy ? 'Выполняется…' : 'Действия ▾') + '</button>' +
      '<button type="button" class="btn ghost sm" data-bulk-clear aria-label="Снять выбор" title="Снять выбор (Esc)"' + (busy ? ' disabled' : '') + '>✕</button>';
    bar.innerHTML = h;
  }

  // Поповер над панелью: снизу ему не хватит места
  function popAbove(anchor) {
    if (!pop) return;
    var r = anchor.getBoundingClientRect();
    pop.style.top = Math.max(8, r.top - pop.offsetHeight - 6) + 'px';
  }

  function actionsMenu(anchor) {
    var s = bulkSpec();
    if (!s) return;
    var p = openPop(anchor, '<div class="list">' + s.bulk_actions.map(function (a) {
      return '<button type="button" class="item" data-act="' + esc(a.code) + '">' + esc(a.label) + '</button>';
    }).join('') + '</div>', 'pop-bulk');
    popAbove(anchor);
    p.addEventListener('click', function (e) {
      var b = e.target.closest('[data-act]');
      if (!b) return;
      var a = s.bulk_actions.filter(function (x) { return x.code === b.dataset.act; })[0];
      if (a) startAction(a, anchor);
    });
  }

  function startAction(a, anchor) {
    if (allFilter || a.confirm !== 'never' || a.param.kind !== 'none') confirmPop(a, anchor);
    else { closePop(); submitBulk(a, '', false, null, snapshot()); }
  }

  // Охват и число в момент показа подтверждения: действие не может отличаться от прочитанного в нём
  function snapshot() {
    var body = $('#list-body');
    return { all: allFilter, ids: Array.from(sel), n: selCount(), query: body ? body.dataset.query || '' : '' };
  }

  function confirmPop(a, anchor) {
    var snap = snapshot();
    var n = snap.n, all = snap.all, danger = a.confirm === 'danger';
    var what = all ? 'все ' + fmt(n) + ' по фильтру' : fmt(n) + ' ' + plural(n, 'запись', 'записи', 'записей');
    var h = '<div class="title">' + esc(danger ? a.label + ' ' + what + '? Это нельзя отменить' : a.label) + '</div>';
    if (!danger) h += '<div class="pop-note muted">' + esc(all ? 'Будет затронуто: все ' + fmt(n) + ' по фильтру' : 'Выбрано: ' + n) + '</div>';
    if (a.param.kind === 'choice') {
      h += '<div class="row"><select data-param aria-label="' + esc(a.label) + '"><option value="">Выберите…</option>' +
        a.param.options.map(function (o) { return '<option value="' + esc(o.value) + '">' + esc(o.label) + '</option>'; }).join('') + '</select></div>';
    } else if (a.param.kind === 'reason') {
      h += '<div class="row"><input type="text" data-param maxlength="500" placeholder="Причина (необязательно)" autocomplete="off" aria-label="Причина"></div>';
    }
    if (a.notify) h += '<label class="item"><input type="checkbox" data-notify checked>Уведомить покупателей</label>';
    h += '<div class="actions"><button type="button" class="btn ghost sm" data-cancel>Отмена</button>' +
      '<button type="button" class="btn sm ' + (danger ? 'danger' : 'primary') + '" data-go>' +
      esc(danger ? a.label + ' ' + fmt(n) : 'Применить') + '</button></div>';
    var p = openPop(anchor, h, 'pop-bulk');
    popAbove(anchor);
    var go = $('[data-go]', p), param = $('[data-param]', p);
    if (param) param.focus(); else go.focus();
    function apply() {
      if (busy) return;
      var v = param ? param.value : '';
      if (a.param.kind === 'choice' && !v) {
        if (!$('.pop-hint', p)) {
          var hint = document.createElement('div');
          hint.className = 'pop-hint';
          hint.setAttribute('role', 'alert');
          hint.textContent = 'Выберите значение';
          p.insertBefore(hint, $('.actions', p));
          popAbove(anchor);
        }
        param.focus();
        return;
      }
      var nt = $('[data-notify]', p);
      submitBulk(a, v.trim(), !!(nt && nt.checked), go, snap);
    }
    p.addEventListener('click', function (e) {
      if (e.target.closest('[data-cancel]')) closePop();
      else if (e.target.closest('[data-go]')) apply();
    });
    p.addEventListener('keydown', function (e) {
      if (e.key === 'Enter' && e.target.tagName === 'INPUT' && e.target.type === 'text') { e.preventDefault(); apply(); }
    });
  }

  function submitBulk(a, param, notify, btn, snap) {
    var s = bulkSpec(), body = $('#list-body');
    if (!s || !body || busy || !window.htmx || !snap || !snap.n) return;
    // выбор мог остаться от другого списка: действуем только над строками, отмеченными в этом
    var ids = snap.ids;
    if (!snap.all) {
      var checked = {};
      bulkRows().forEach(function (r) { var b = rowBox(r); if (b && b.checked) checked[r.dataset.id] = true; });
      ids = snap.ids.filter(function (id) { return checked[id]; });
      if (!ids.length) { clearSel(); return; }
    } else if ((body.dataset.query || '') !== snap.query) {
      clearSel();
      return;
    }
    busy = true;
    var label = btn && btn.textContent;
    if (btn) { btn.disabled = true; btn.textContent = 'Выполняется…'; }
    drawBar();
    var values = {
      list_key: s.list_key, path: s.path, query: snap.query,
      action: a.code, param: param, notify: notify ? '1' : '0',
      scope: snap.all ? 'filter' : 'ids', ids: snap.all ? '' : ids.join(','),
      // число, показанное в подтверждении: сервер откажет, если по фильтру теперь другое
      expect: snap.all ? String(snap.n) : ''
    };
    function done() {
      // список заменён — выбор уже сброшен; иначе (4xx, сбой сети) выбор и подтверждение остаются
      if (!body.isConnected) return;
      busy = false;
      if (btn && btn.isConnected) { btn.disabled = false; btn.textContent = label; }
      drawBar();
    }
    window.htmx.ajax('POST', '/admin/lists/bulk', { target: '#list-body', swap: 'outerHTML', values: values }).then(done, done);
  }

  // Клик по ячейке выбора: не открывает строку и быстрый просмотр
  document.addEventListener('click', function (e) {
    var cell = e.target.closest && e.target.closest('td.sel, th.sel');
    if (!cell || !$('#list-body') || !$('#list-body').contains(cell)) return;
    if (pop) closePop();
    var box = $('input[type=checkbox]', cell);
    if (!box || busy) { e.preventDefault(); return; }
    if (e.target !== box) box.checked = !box.checked;
    if (box.hasAttribute('data-bulk-all')) {
      bulkRows().forEach(function (r) { setRow(r, box.checked); });
      lastId = null;
    } else {
      var row = cell.closest('tr');
      if (e.shiftKey && lastId !== null) setRange(lastId, row, box.checked); else setRow(row, box.checked);
      lastId = row.dataset.id;
    }
    syncBulk();
    // фокус на чекбоксе отключил бы клавиши списка (X, J/K, Esc)
    box.blur();
  }, true);
  document.addEventListener('mousedown', function (e) {
    if (e.shiftKey && e.target.closest && e.target.closest('td.sel')) e.preventDefault();
  }, true);

  document.addEventListener('click', function (e) {
    var t = e.target;
    if (!t.closest || !t.isConnected || !t.closest('#bulk-bar')) return;
    if (t.closest('[data-bulk-filter]')) { allFilter = true; drawBar(); }
    else if (t.closest('[data-bulk-page]')) { allFilter = false; drawBar(); }
    else if (t.closest('[data-bulk-clear]')) clearSel();
    else {
      var b = t.closest('#bulk-actions-btn');
      if (b && !busy) { if (pop && pop.classList.contains('pop-bulk')) closePop(); else actionsMenu(b); }
    }
  });

  // X / Shift+X: отметить текущую строку / диапазон от последней отмеченной
  function toggleCur(range) {
    var row = curRow();
    var box = row && rowBox(row);
    if (!box || busy) return;
    var on = !box.checked;
    if (range && lastId !== null) {
      var from = bulkRows().filter(function (r) { return r.dataset.id === lastId; })[0];
      on = from && rowBox(from) ? rowBox(from).checked : true;
      setRange(lastId, row, on);
    } else {
      setRow(row, on);
    }
    lastId = row.dataset.id;
    syncBulk();
  }

  /* ---------- Правка ячейки ---------- */

  var editing = null;    // {cell, editor, original, title, minWidth, kind, phase: edit|saving}
  var pending = null;    // {next} — ждём замену #list-body после сохранения
  var failStatus = 0;    // код 4xx текущего сохранения (ставится в htmx:responseError)
  var viewKeep = null;   // {y, cur} — позиция и текущая строка до замены списка
  var firstTimer = null; // отложенный переход по ссылке первой ячейки
  var firstAnchor = null;
  var firstRowId = null;
  var firstPass = false; // программный клик после задержки не перехватывается
  var boolAt = 0;        // защита от двойного переключения «да/нет» двойным кликом

  function editDef(code) {
    var s = spec();
    return s && s.edit_columns ? s.edit_columns.filter(function (c) { return c.code === code; })[0] : null;
  }

  function colTitle(code) {
    var s = spec();
    var c = s && s.columns.filter(function (x) { return x.code === code; })[0];
    return (c && c.title) || code;
  }

  // Правку не начинаем при открытом поповере/диалоге и во время массового действия.
  function canEdit() {
    return listPage() && !busy && !pop && !$('dialog[open]');
  }

  function inListBody(el) {
    var b = $('#list-body');
    return !!b && !!el && b.contains(el);
  }

  function cancelFirst() {
    if (firstTimer) { window.clearTimeout(firstTimer); firstTimer = null; }
    firstAnchor = null;
    firstRowId = null;
  }

  /* Позиция страницы и текущая строка: после замены #list-body их нужно вернуть */

  function captureView() {
    var row = curRow();
    return { y: window.scrollY || 0, cur: row ? row.dataset.id : null };
  }

  function restoreView(keep) {
    if (keep.cur) {
      var row = rowsOf().filter(function (r) { return r.dataset.id === keep.cur; })[0];
      if (row) row.classList.add('cur');
    }
    window.scrollTo(0, keep.y);
  }

  /* ---------- Редактор ---------- */

  function setFieldState(td, field, saving) {
    td.classList.toggle('saving', saving);
    if (saving) td.setAttribute('aria-busy', 'true'); else td.removeAttribute('aria-busy');
    if (!field) return;
    field.disabled = saving;
    var wrap = field.parentNode;
    var spin = wrap ? $('.cell-spin', wrap) : null;
    if (saving && !spin && wrap) {
      spin = document.createElement('span');
      spin.className = 'cell-spin';
      spin.setAttribute('aria-hidden', 'true');
      wrap.appendChild(spin);
    } else if (!saving && spin) {
      spin.remove();
    }
  }

  function openEditor(td) {
    if (!td || !canEdit() || editing || td.classList.contains('cell-editing')) return;
    if (!inListBody(td)) return;
    var def = editDef(td.dataset.edit);
    if (!def) return;
    var kind = td.dataset.kind || def.kind || 'text';
    if (kind === 'bool') return;
    cancelFirst();
    var raw = td.dataset.value || '';
    var wrap = document.createElement('div');
    wrap.className = 'cell-edit';
    var field;
    if (kind === 'list') {
      field = document.createElement('select');
      var found = false;
      (def.options || []).forEach(function (o) {
        var opt = document.createElement('option');
        opt.value = o.value;
        opt.textContent = o.label;
        field.appendChild(opt);
        if (o.value === raw) found = true;
      });
      // вариант удалён из справочника, но значение ещё в записи — показываем как есть
      if (raw !== '' && !found) {
        var own = document.createElement('option');
        own.value = raw;
        own.textContent = raw;
        field.insertBefore(own, field.firstChild);
        found = true;
      }
      if (!def.required) {
        var empty = document.createElement('option');
        empty.value = '';
        empty.textContent = '—';
        field.insertBefore(empty, field.firstChild);
        if (raw === '') found = true;
      }
      // обязательный список без значения: не подставляем первый вариант молча
      if (!found) {
        var ph = document.createElement('option');
        ph.value = '';
        ph.textContent = 'Выберите…';
        ph.disabled = true;
        field.insertBefore(ph, field.firstChild);
      }
      field.value = found ? raw : '';
    } else {
      field = document.createElement('input');
      // число — текстом: сервер принимает и запятую («1,5»)
      field.type = kind === 'date' ? 'date' : 'text';
      if (kind === 'number') field.inputMode = 'decimal';
      field.value = raw;
    }
    field.className = 'cell-input';
    field.setAttribute('aria-label', colTitle(td.dataset.edit));
    field.setAttribute('autocomplete', 'off');
    wrap.appendChild(field);

    editing = {
      cell: td,
      editor: field,
      // исходные узлы: при отмене возвращаем их же, не теряя обработчики htmx на ссылке
      original: Array.prototype.slice.call(td.childNodes),
      title: td.getAttribute('title'),
      minWidth: td.style.minWidth,
      kind: kind,
      phase: 'edit'
    };
    // колонка не должна прыгать при замене текста редактором
    td.style.minWidth = td.offsetWidth + 'px';
    td.removeAttribute('title');
    td.textContent = '';
    td.appendChild(wrap);
    td.classList.add('cell-editing');
    field.addEventListener('keydown', onEditorKey);
    field.addEventListener('blur', function () { onEditorBlur(field); });
    field.focus();
    if (kind === 'text' || kind === 'number') {
      try { field.select(); } catch (e) { /* выделение не критично */ }
    }
  }

  function onEditorBlur(field) {
    // blur мог прийти от уже убранного из DOM поля, когда открыт следующий редактор
    if (!editing || editing.phase !== 'edit' || editing.editor !== field) return;
    saveEdit(null);
  }

  function nextCell(td, back) {
    var cells = $$('#list-body td[data-edit]').filter(function (c) { return c.dataset.kind !== 'bool'; });
    var i = cells.indexOf(td);
    if (i < 0) return null;
    var next = cells[i + (back ? -1 : 1)];
    if (!next) return null;
    var row = next.closest('tr');
    return row ? { id: row.dataset.id, code: next.dataset.edit } : null;
  }

  function onEditorKey(e) {
    if (!editing || editing.phase !== 'edit' || e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Enter') {
      e.preventDefault();
      e.stopPropagation();
      saveEdit(null);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      closeEdit();
    } else if (e.key === 'Tab') {
      e.preventDefault();
      e.stopPropagation();
      saveEdit(nextCell(editing.cell, e.shiftKey));
    }
  }

  function closeEdit() {
    if (!editing) return;
    var td = editing.cell;
    var original = editing.original;
    var title = editing.title;
    var minWidth = editing.minWidth;
    // сначала снимаем состояние: blur от удаления поля не должен запустить сохранение
    editing = null;
    pending = null;
    failStatus = 0;
    if (!td || !td.isConnected) return;
    if (original) {
      while (td.firstChild) td.removeChild(td.firstChild);
      original.forEach(function (n) { td.appendChild(n); });
    }
    td.classList.remove('cell-editing', 'saving');
    td.removeAttribute('aria-busy');
    td.style.minWidth = minWidth;
    if (title !== null) td.setAttribute('title', title);
  }

  // 4xx без замены списка: 422 — правим дальше, 409/404 — список устарел
  function resumeEdit() {
    pending = null;
    viewKeep = null;
    failStatus = 0;
    if (!editing) return;
    var td = editing.cell;
    if (!td || !td.isConnected) { editing = null; return; }
    if (!editing.editor) { // «да/нет»: редактора нет, просто снимаем блокировку
      setFieldState(td, null, false);
      editing = null;
      return;
    }
    editing.phase = 'edit';
    setFieldState(td, editing.editor, false);
    editing.editor.focus();
  }

  function saveEdit(next) {
    if (!editing || editing.phase !== 'edit') return;
    var td = editing.cell;
    if (!td || !td.isConnected || !inListBody(td)) { closeEdit(); return; }
    var value = editing.editor ? String(editing.editor.value) : '';
    if (value === (td.dataset.value || '')) {
      // значение не изменилось — запрос не нужен, но Tab всё равно ведёт дальше
      closeEdit();
      if (next) openCell(next);
      return;
    }
    editing.phase = 'saving';
    viewKeep = captureView();
    pending = { next: next || null };
    failStatus = 0;
    setFieldState(td, editing.editor, true);
    postCell(td, value);
  }

  function postCell(td, value) {
    var s = spec(), body = $('#list-body'), row = td.closest('tr');
    if (!s || !body || !row || !window.htmx) { resumeEdit(); return; }
    var values = {
      list_key: s.list_key || '',
      path: s.path || '',
      // текущий запрос — из тела списка: он обновляется при каждой замене
      query: body.dataset.query || '',
      row: row.dataset.id || '',
      column: td.dataset.edit || '',
      value: value,
      version: row.dataset.version || ''
    };
    window.htmx.ajax('POST', '/admin/lists/cell', {
      target: '#list-body', swap: 'outerHTML', values: values
    }).then(afterSave, afterSaveFail);
  }

  function afterSave() {
    // замену уже разобрал htmx:afterSwap
    if (!pending) return;
    var status = failStatus;
    pending = null;
    failStatus = 0;
    if (status === 409 || status === 404) {
      closeEdit();
      if (window.htmx) window.htmx.trigger(document.body, 'list:refresh');
    } else {
      resumeEdit();
    }
  }

  function afterSaveFail() {
    pending = null;
    resumeEdit();
  }

  function openCell(ref) {
    if (!ref) return;
    var row = rowsOf().filter(function (r) { return r.dataset.id === ref.id; })[0];
    if (!row) return;
    var td = $$('td[data-edit]', row).filter(function (c) { return c.dataset.edit === ref.code; })[0];
    if (!td) return;
    openEditor(td);
    if (td.scrollIntoView) td.scrollIntoView({ block: 'nearest' });
  }

  function toggleBool(td) {
    var def = editDef(td.dataset.edit);
    if (!def || def.kind !== 'bool') return;
    var raw = (td.dataset.value || '').toLowerCase();
    var value = raw === 'true' ? 'false' : 'true';
    editing = { cell: td, editor: null, original: null, title: null, minWidth: '', kind: 'bool', phase: 'saving' };
    viewKeep = captureView();
    pending = { next: null };
    failStatus = 0;
    setFieldState(td, null, true);
    postCell(td, value);
  }

  /* ---------- События правки ---------- */

  // Ошибку запроса запоминаем до промиса: htmx.ajax резолвится и на 4xx
  document.addEventListener('htmx:responseError', function (e) {
    if (!editing || editing.phase !== 'saving') return;
    var d = e.detail || {};
    var path = (d.pathInfo && d.pathInfo.requestPath) || (d.xhr && d.xhr.responseURL) || '';
    if (String(path).indexOf('/admin/lists/cell') < 0) return;
    failStatus = (d.xhr && d.xhr.status) || 0;
  });

  // После замены списка возвращаем позицию и текущую строку; Tab открывает следующую ячейку
  document.addEventListener('htmx:afterSwap', function (e) {
    var t = e.detail && e.detail.target;
    if (!t || t.id !== 'list-body' || t.isConnected) return;
    if (viewKeep) { restoreView(viewKeep); viewKeep = null; }
    var p = pending;
    pending = null;
    failStatus = 0;
    // ячейка редактора осталась в старом DOM — состояние не должно зависать
    if (editing && (!editing.cell || !editing.cell.isConnected)) editing = null;
    if (!p) return;
    editing = null;
    if (p.next) openCell(p.next);
  });

  // Любая замена могла унести ячейку редактора (переход по истории, замена body): состояние не зависает
  document.addEventListener('htmx:afterSwap', function () {
    if (editing && (!editing.cell || !editing.cell.isConnected)) editing = null;
  });

  // Перехват в capture: ссылку htmx-boost обрабатывает на самом <a>, до всплытия
  document.addEventListener('click', function (e) {
    var t = e.target;
    if (!t || !t.closest || !t.isConnected) return;
    var td = t.closest('td[data-edit]');
    if (!td || !inListBody(td)) return;
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;

    if (td.dataset.kind === 'bool') {
      if (e.detail === 0) return; // клавиатура/программный клик: работает обычная ссылка
      e.preventDefault();
      e.stopPropagation();
      closeDetails(td);
      if (!canEdit()) { if (pop) closePop(); return; }
      if (editing || Date.now() - boolAt < 300) return; // второй клик двойного — не переключаем
      boolAt = Date.now();
      toggleBool(td);
      return;
    }

    var a = t.closest('td[data-first] a[href]');
    if (!a) return;
    if (firstPass || e.detail === 0) return; // программный клик после задержки идёт как обычно
    e.preventDefault();
    e.stopPropagation();
    if (pop) closePop();
    closeDetails(a);
    // второй клик двойного отменяет переход: дальше сработает dblclick и откроет редактор
    if (firstTimer && firstAnchor === a) { cancelFirst(); return; }
    // одиночный клик открывает карточку с задержкой: двойной успевает отменить переход
    cancelFirst();
    var row = a.closest('tr');
    firstAnchor = a;
    firstRowId = row ? row.dataset.id : null;
    firstTimer = window.setTimeout(function () {
      firstTimer = null;
      var anchor = firstAnchor;
      var rowId = firstRowId;
      firstAnchor = null;
      firstRowId = null;
      // список мог перерисоваться (например, уход фокуса сохранил ячейку) — ищем ссылку той же строки
      if (!anchor || !anchor.isConnected) {
        if (!rowId) return;
        var fresh = rowsOf().filter(function (r) { return r.dataset.id === rowId; })[0];
        anchor = fresh ? $('td[data-first] a[href]', fresh) : null;
      }
      if (!anchor || !anchor.isConnected) return;
      firstPass = true;
      try { anchor.click(); } finally { firstPass = false; }
    }, 200);
  }, true);

  document.addEventListener('dblclick', function (e) {
    var td = e.target.closest && e.target.closest('td[data-edit]');
    if (!td || !inListBody(td) || td.dataset.kind === 'bool') return;
    if (!canEdit() || editing) return;
    e.preventDefault();
    openEditor(td);
  });

  /* ---------- Клавиши ---------- */

  function typing(el) {
    return !!el && !!el.closest && !!el.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]');
  }
  function peekIsOpen() { var p = $('#peek'); return !!p && !p.hidden; }
  function rowsOf() { return $$('#list-body tbody tr[data-id]'); }

  function curIndex(rows) {
    var i = -1;
    rows.forEach(function (r, k) { if (i < 0 && r.classList.contains('cur')) i = k; });
    if (i < 0) rows.forEach(function (r, k) { if (i < 0 && r.classList.contains('peeked')) i = k; });
    return i;
  }

  function moveCur(delta) {
    var rows = rowsOf();
    if (!rows.length) return;
    var i = curIndex(rows);
    var next = i < 0 ? (delta > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, i + delta));
    rows.forEach(function (r) { r.classList.remove('cur'); });
    var row = rows[next];
    row.classList.add('cur');
    row.scrollIntoView({ block: 'nearest' });
    if (peekIsOpen() && row.dataset.peek) {
      if (window.getSelection) window.getSelection().removeAllRanges();
      row.click();
    }
  }

  function curRow() {
    var rows = rowsOf();
    var i = curIndex(rows);
    return i < 0 ? null : rows[i];
  }

  function openRow(row) {
    var a = $('td a[href]', row);
    if (a) a.click(); else if (row.dataset.open) location.href = row.dataset.open;
  }

  function searchTarget() {
    var s = spec();
    var def = s.filters.filter(function (f) { return f.code === 'name' && f.ty === 'text'; })[0] ||
      s.filters.filter(function (f) { return f.ty === 'text'; })[0];
    return def ? def.code : null;
  }

  function pageLink(label) {
    var a = $$('#list-body .pager a').filter(function (x) { return x.textContent.trim() === label; })[0];
    if (a) a.click();
  }

  document.addEventListener('keydown', function (e) {
    if (!listPage()) return;
    if (e.key === 'Escape') {
      if (pop) { e.preventDefault(); e.stopImmediatePropagation(); closePop(); }
      else if (detailsOpen() && !$('dialog[open]')) { e.stopImmediatePropagation(); closeDetails(); }
      else if (sel.size && !busy && !$('dialog[open]') && !typing(e.target)) { e.stopImmediatePropagation(); clearSel(); }
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey || e.isComposing) return;
    if (typing(e.target) || $('dialog[open]') || pop || detailsOpen()) return;

    var onAction = e.target.closest && e.target.closest('a, button, summary');

    if (e.code === 'KeyX' && bulkSpec() && $('#list-body input[data-bulk]')) {
      e.preventDefault(); e.stopImmediatePropagation();
      if (!e.repeat) toggleCur(e.shiftKey);
      return;
    }
    if (e.shiftKey) {
      // Shift допустим только для { }
      if (e.code === 'BracketLeft' || e.code === 'BracketRight') {
        e.preventDefault();
        e.stopImmediatePropagation();
        if (!e.repeat) pageLink(e.code === 'BracketLeft' ? '‹' : '›');
      }
      return;
    }
    if (e.code === 'KeyJ' || e.code === 'KeyK') {
      e.preventDefault(); e.stopImmediatePropagation();
      moveCur(e.code === 'KeyJ' ? 1 : -1);
    } else if (e.key === 'Enter' && !onAction) {
      var r = curRow();
      if (r) { e.preventDefault(); openRow(r); }
    } else if (e.code === 'Space' && !onAction) {
      var row = curRow();
      if (row && row.dataset.peek) {
        e.preventDefault();
        if (row.classList.contains('peeked') && peekIsOpen()) {
          var x = $('#peek [data-peek-close]');
          if (x) x.click();
        } else {
          if (window.getSelection) window.getSelection().removeAllRanges();
          row.click();
        }
      }
    } else if (e.code === 'KeyF') {
      e.preventDefault(); e.stopImmediatePropagation();
      var add = $('#list-add-filter');
      if (add) filterPicker(add);
    } else if (e.code === 'Slash') {
      var code = searchTarget();
      var anchor = (code && $('[data-chip="' + code + '"]')) || $('#list-add-filter');
      if (anchor) {
        e.preventDefault(); e.stopImmediatePropagation();
        if (code) filterEditor(anchor, code); else filterPicker(anchor);
      }
    }
  }, true);

  // Выбор живёт только на странице списка: подмена всего body (переход hx-boost, возврат по истории) его сбрасывает
  function resetBulk() {
    busy = false;
    if (pop && pop.classList.contains('pop-bulk')) closePop();
    sel.clear();
    lastId = null;
    allFilter = false;
    barSig = '';
    var bar = $('#bulk-bar');
    if (bar) bar.remove();
    document.body.classList.remove('has-bulk-bar');
    syncBulk();
  }
  document.addEventListener('htmx:afterSwap', function (e) {
    var t = e.detail && e.detail.target;
    if (!t || t.id === 'list-body' || !t.querySelector) return;
    if (t === document.body || t === document.documentElement || t.querySelector('#list-body')) resetBulk();
  });
  document.addEventListener('htmx:historyRestore', resetBulk);
  // снимок страницы для истории без панели и отметок строк
  document.addEventListener('htmx:beforeHistorySave', function () {
    if (!sel.size) return;
    var bar = $('#bulk-bar');
    if (bar) bar.remove();
    document.body.classList.remove('has-bulk-bar');
    if (pop && pop.classList.contains('pop-bulk')) closePop();
    bulkRows().forEach(function (r) { r.classList.remove('sel'); });
    // страница осталась на месте (снимок без перехода) — вернуть панель
    setTimeout(syncBulk, 0);
  });

  // после замены списка текущая строка и открытые «⋯» сбрасываются сами (новый DOM); поповер закрываем
  document.addEventListener('htmx:afterSwap', function (e) {
    var t = e.detail && e.detail.target;
    // HX-Reswap: none (отказ сервера) оставляет прежний список: поповер и выбор тоже остаются
    if (!t || t.id !== 'list-body' || t.isConnected) return;
    if (pop && !pop.classList.contains('pop-cols')) closePop();
    // любая замена списка сбрасывает выбор: строки и итоги уже другие
    busy = false;
    clearSel();
  });
})();
