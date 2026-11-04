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

  function filterPicker(anchor) {
    var p = openPop(anchor, '<div class="row"><input type="text" id="fq" placeholder="Поле…" autocomplete="off" aria-label="Поле"></div>' +
      '<div class="list" id="fl">' + pickerHtml('') + '</div>');
    var q = $('#fq', p);
    q.focus();
    q.addEventListener('input', function () { $('#fl', p).innerHTML = pickerHtml(q.value); });
    q.addEventListener('keydown', function (e) {
      if (e.key === 'Enter') { e.preventDefault(); var b = $('[data-pick]', p); if (b) b.click(); }
    });
    p.addEventListener('click', function (e) {
      var b = e.target.closest('[data-pick]');
      if (b) filterEditor(anchor, b.dataset.pick);
    });
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
    if (pop && !pop.contains(t) && !t.closest('#list-add-filter, [data-chip], #list-cols')) closePop();
    closeDetails(t);
    if (!listPage()) return;
    var add = t.closest('#list-add-filter');
    if (add) { filterPicker(add); return; }
    var chip = t.closest('[data-chip]');
    if (chip) { filterEditor(chip, chip.dataset.chip); return; }
    var cols = t.closest('#list-cols');
    if (cols) { if (pop && pop.classList.contains('pop-cols')) closePop(); else colsPicker(cols); return; }
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
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey || e.isComposing) return;
    if (typing(e.target) || $('dialog[open]') || pop || detailsOpen()) return;

    var onAction = e.target.closest && e.target.closest('a, button, summary');

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

  // после замены списка текущая строка и открытые «⋯» сбрасываются сами (новый DOM); поповер закрываем
  document.addEventListener('htmx:afterSwap', function (e) {
    var t = e.detail && e.detail.target;
    if (t && t.id === 'list-body' && pop && !pop.classList.contains('pop-cols')) closePop();
  });
})();
