/* Клиент админки: палитра, горячие клавиши, панель сохранения, уведомления, меню.
   Без сборки. Глобальные обработчики вешаются на document один раз и переживают
   hx-boost-переходы (меняется содержимое body); постраничная инициализация идёт в init(). */
(function () {
  'use strict';

  var $ = function (sel, root) { return (root || document).querySelector(sel); };
  var $$ = function (sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); };

  // localStorage может быть недоступен (приватный режим, запрет) — любое обращение в try/catch
  function store(key, value) {
    try {
      if (value === undefined) return window.localStorage.getItem(key);
      window.localStorage.setItem(key, value);
    } catch (e) { /* не страшно: просто не запомним */ }
    return null;
  }

  /* ---------- Уведомления ---------- */

  var TOAST_MS = 4000;

  function dismissToast(el) {
    if (!el.parentNode) return;
    el.classList.add('is-leaving');
    window.setTimeout(function () { if (el.parentNode) el.parentNode.removeChild(el); }, 200);
  }

  // Общее поведение для созданных клиентом и отрисованных сервером уведомлений.
  function armToast(el) {
    if (el.dataset.armed) return;
    el.dataset.armed = '1';
    if (el.dataset.kind === 'error') {
      if (!$('.toast-x', el)) {
        var x = document.createElement('button');
        x.type = 'button';
        x.className = 'toast-x';
        x.setAttribute('aria-label', 'Закрыть');
        x.textContent = '×';
        x.addEventListener('click', function () { dismissToast(el); });
        el.appendChild(x);
      }
    } else {
      window.setTimeout(function () { dismissToast(el); }, TOAST_MS);
    }
  }

  function toast(text, kind) {
    var box = $('#toasts');
    if (!box || !text) return;
    var el = document.createElement('div');
    el.className = 'toast';
    el.dataset.kind = kind === 'error' ? 'error' : 'ok';
    el.setAttribute('role', el.dataset.kind === 'error' ? 'alert' : 'status');
    el.textContent = text; // textContent: текст ответа сервера не должен стать разметкой
    box.appendChild(el);
    armToast(el);
  }

  // HX-Trigger: {"toast": {"text": "...", "kind": "ok"}} → событие toast; htmx 2 кладёт
  // не-объектное значение в detail.value.
  document.addEventListener('toast', function (e) {
    var d = e.detail || {};
    if (typeof d.value === 'string') toast(d.value, 'ok');
    else if (d.value && typeof d.value === 'object') toast(d.value.text, d.value.kind);
    else toast(d.text, d.kind);
  });

  document.addEventListener('htmx:responseError', function (e) {
    var xhr = e.detail && e.detail.xhr;
    var text = ((xhr && xhr.responseText) || '').trim().slice(0, 300);
    toast(text || 'Ошибка запроса' + (xhr && xhr.status ? ' (' + xhr.status + ')' : ''), 'error');
  });
  document.addEventListener('htmx:sendError', function () {
    toast('Нет связи с сервером', 'error');
  });

  /* ---------- Панель сохранения ---------- */

  var dirtyForm = null;

  function savebar() { return $('#savebar'); }

  function setDirty(form) {
    dirtyForm = form;
    var bar = savebar();
    if (bar) bar.hidden = false;
  }

  function clearDirty() {
    dirtyForm = null;
    var bar = savebar();
    if (bar) bar.hidden = true;
  }

  // Форма могла уйти из DOM после перехода — тогда состояние сбрасываем.
  function isDirty() {
    if (dirtyForm && !dirtyForm.isConnected) clearDirty();
    return !!dirtyForm;
  }

  function onFormEdit(e) {
    var form = e.target.closest && e.target.closest('form[data-dirty-guard]');
    if (form && form !== dirtyForm) setDirty(form);
  }
  document.addEventListener('input', onFormEdit);
  document.addEventListener('change', onFormEdit);

  document.addEventListener('click', function (e) {
    if (!isDirty()) return;
    if (e.target.closest('[data-savebar-reset]')) {
      dirtyForm.reset();
      clearDirty();
    } else if (e.target.closest('[data-savebar-save]')) {
      dirtyForm.requestSubmit();
    }
  });

  // Своя отправка формы — не повод предупреждать об уходе.
  var submitting = false;
  document.addEventListener('submit', function (e) {
    if (dirtyForm && e.target === dirtyForm) submitting = true;
  });

  // сохранение не удалось (ошибка ответа или сети): страница остаётся, защита от ухода возвращается
  document.addEventListener('htmx:afterRequest', function (e) {
    var d = e.detail || {};
    if (dirtyForm && d.elt === dirtyForm && !d.successful) submitting = false;
  });

  window.addEventListener('beforeunload', function (e) {
    if (isDirty() && !submitting) {
      e.preventDefault();
      e.returnValue = '';
    }
  });

  // Переход по hx-boost при грязной форме, а также запрос, ответ которого заменит область
  // с этой формой (вкладки карточки). Отправка самой формы (сохранение) — не уход.
  document.addEventListener('htmx:beforeRequest', function (e) {
    var d = e.detail || {};
    if (!isDirty()) return;
    var boosted = d.boosted || (d.requestConfig && d.requestConfig.boosted);
    var replaces = d.target && d.target.contains && d.target.contains(dirtyForm);
    if (!boosted && !replaces) return;
    if (d.elt === dirtyForm || (d.elt && dirtyForm.contains(d.elt))) return;
    if (!window.confirm('Есть несохранённые изменения. Уйти?')) e.preventDefault();
  });

  /* ---------- Диалоги и палитра ---------- */

  function paletteEl() { return $('#palette'); }
  function paletteOpen() { var p = paletteEl(); return !!(p && p.open); }

  function openPalette() {
    var p = paletteEl(), q = $('#palette-q');
    if (!p || !q) return;
    if (!p.open) p.showModal();
    q.value = '';
    q.focus();
    // пустой запрос — подсказки по умолчанию, каждый раз свежие
    if (window.htmx) window.htmx.ajax('GET', '/admin/palette', { target: '#palette-results', swap: 'innerHTML', values: { q: '' } });
  }

  function items() { return $$('#palette-results .pal-item'); }

  function select(idx) {
    var list = items();
    if (!list.length) return;
    idx = Math.max(0, Math.min(list.length - 1, idx));
    list.forEach(function (it, i) {
      it.classList.toggle('is-selected', i === idx);
      if (i === idx) it.scrollIntoView({ block: 'nearest' });
    });
  }

  function selectedIndex() {
    var list = items();
    for (var i = 0; i < list.length; i++) if (list[i].classList.contains('is-selected')) return i;
    return -1;
  }

  // после подгрузки результатов выбран первый
  document.addEventListener('htmx:afterSwap', function (e) {
    if (e.detail && e.detail.target && e.detail.target.id === 'palette-results') select(0);
  });

  document.addEventListener('click', function (e) {
    if (e.target.closest('[data-palette-open]')) {
      e.preventDefault();
      openPalette();
      return;
    }
    // клик по фону: у dialog target — сам диалог
    if (e.target.tagName === 'DIALOG' && e.target.open) {
      var r = e.target.getBoundingClientRect();
      if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) e.target.close();
    }
    // переход из палитры закрывает её (hx-boost подменит страницу, а диалог остался бы открытым)
    var link = e.target.closest('#palette-results .pal-item');
    if (link && paletteEl().open) paletteEl().close();
  });

  document.addEventListener('mousemove', function (e) {
    var it = e.target.closest && e.target.closest('#palette-results .pal-item');
    if (it && !it.classList.contains('is-selected')) select(items().indexOf(it));
  });

  function paletteKeys(e) {
    if (e.key === 'ArrowDown') { e.preventDefault(); select(selectedIndex() + 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); select(selectedIndex() - 1); }
    else if (e.key === 'Enter') {
      var list = items(), i = selectedIndex();
      var it = list[i >= 0 ? i : 0];
      if (it) { e.preventDefault(); it.click(); }
    }
  }

  /* ---------- Горячие клавиши ---------- */

  var gPending = null; // таймер ожидания буквы после g

  // Раскладка может быть русской: сравниваем и event.key, и физическую клавишу.
  function keyIs(e, latin, code) {
    return e.key.toLowerCase() === latin || e.code === code;
  }

  function typing(t) {
    if (!t || !t.closest) return false;
    return !!t.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])');
  }

  function submitVisibleBar() {
    var bar = savebar();
    if (bar && !bar.hidden && isDirty()) { dirtyForm.requestSubmit(); return true; }
    return false;
  }

  var CODE_TO_LATIN = { KeyD: 'd', KeyO: 'o', KeyC: 'c', KeyU: 'u', KeyG: 'g' };

  function gotoTarget(e) {
    var letter = e.key.length === 1 && /[a-z]/i.test(e.key) ? e.key.toLowerCase() : CODE_TO_LATIN[e.code];
    return letter ? $('a[data-goto="' + letter + '"]') : null;
  }

  document.addEventListener('keydown', function (e) {
    var mod = e.ctrlKey || e.metaKey;

    // Ctrl/Cmd+K работает всегда
    if (mod && !e.altKey && keyIs(e, 'k', 'KeyK')) {
      e.preventDefault();
      if (paletteOpen()) paletteEl().close(); else openPalette();
      return;
    }

    if (paletteOpen()) {
      if (e.key === 'Escape') return; // dialog закроется сам
      paletteKeys(e);
      return;
    }

    if (mod && !e.altKey && keyIs(e, 's', 'KeyS')) {
      if (submitVisibleBar()) e.preventDefault();
      return;
    }

    if (e.altKey || mod || e.isComposing) return;
    // открытое окно «Горячие клавиши» тоже перехватывает клавиатуру
    if (typing(e.target) || ($('#hotkeys') && $('#hotkeys').open)) return;

    if (gPending) {
      window.clearTimeout(gPending);
      gPending = null;
      var a = gotoTarget(e);
      if (a) { e.preventDefault(); a.click(); }
      return;
    }

    if (e.key === '?' || (e.shiftKey && e.code === 'Slash')) {
      var h = $('#hotkeys');
      if (h) { e.preventDefault(); h.showModal(); }
    } else if (keyIs(e, 'g', 'KeyG') && !e.shiftKey) {
      gPending = window.setTimeout(function () { gPending = null; }, 1000);
    } else if (keyIs(e, 'c', 'KeyC')) {
      var c = $('[data-hotkey=c]');
      if (c) { e.preventDefault(); c.click(); }
    } else if (e.key === '/' || e.code === 'Slash') {
      var s = $('[data-hotkey="/"]');
      if (s) { e.preventDefault(); s.focus(); }
    }
  });

  /* ---------- Меню ---------- */

  function initNav() {
    $$('details[data-nav-group]').forEach(function (d) {
      var saved = store('admin.nav.' + d.dataset.navGroup);
      // группу с текущей страницей не прячем, чтобы не потерять «где я»
      if (saved === '0' && !$('[aria-current]', d)) d.open = false;
      else if (saved === '1') d.open = true;
    });
  }

  // toggle не всплывает — ловим в фазе перехвата
  document.addEventListener('toggle', function (e) {
    var d = e.target;
    if (d.matches && d.matches('details[data-nav-group]')) store('admin.nav.' + d.dataset.navGroup, d.open ? '1' : '0');
  }, true);

  document.addEventListener('click', function (e) {
    if (e.target.closest('[data-nav-toggle]')) {
      document.body.classList.toggle('nav-open');
    } else if (document.body.classList.contains('nav-open') &&
               (e.target.closest('.sidebar a') || !e.target.closest('.sidebar'))) {
      document.body.classList.remove('nav-open');
    }
  });

  /* ---------- Штрихкоды в документе склада ---------- */

  function bcLines(text) {
    return text.split('\n').map(function (c) { return c.trim(); }).filter(Boolean);
  }

  // Счётчик «N/M»; у поштучного товара красный, пока N ≠ M
  function bcRefresh(row) {
    var area = $('textarea.bc-text', row);
    var amount = $('input[name="line_amount[]"]', row);
    var out = $('.bc-count', row);
    if (!area || !out) return;
    var n = bcLines(area.value).length;
    var m = amount ? parseFloat(String(amount.value).replace(',', '.')) : NaN;
    out.textContent = n + '/' + (isNaN(m) ? 0 : m);
    out.classList.toggle('bc-bad', row.dataset.multi === '1' && n !== m);
  }

  document.addEventListener('input', function (e) {
    var row = e.target.closest && e.target.closest('tr.line');
    if (row && e.target.matches('textarea.bc-text, input[name="line_amount[]"]')) bcRefresh(row);
  });

  document.addEventListener('click', function (e) {
    var btn = e.target.closest('[data-bc-toggle]');
    if (!btn) return;
    var area = $('textarea.bc-text', btn.closest('tr.line'));
    if (area) area.hidden = !area.hidden;
  });

  function bcScan(input) {
    var code = input.value.trim();
    if (!code) return;
    var table = $('#lines');
    var tpl = $('#line-template');
    fetch('/admin/shop/store-docs/scan?code=' + encodeURIComponent(code), { credentials: 'same-origin' })
      .then(function (r) {
        if (r.status === 404) { toast('Штрихкод не найден', 'error'); return null; }
        if (!r.ok) throw new Error(String(r.status));
        return r.json();
      })
      .then(function (hit) {
        if (!hit || !table) return;
        var row = $$('tr.line', table).filter(function (tr) {
          var id = $('input[name="line_item[]"]', tr);
          return id && id.value === String(hit.item_id);
        })[0];
        var amount;
        if (row) {
          amount = $('input[name="line_amount[]"]', row);
          amount.value = (parseFloat(String(amount.value).replace(',', '.')) || 0) + 1;
        } else {
          if (!tpl) return;
          row = tpl.content.firstElementChild.cloneNode(true);
          $('input[name="line_item[]"]', row).value = hit.item_id;
          $('.name', row).textContent = hit.name + ' (#' + hit.item_id + ')';
          $('tbody', table).appendChild(row);
        }
        if (hit.multi) {
          row.dataset.multi = '1';
          var area = $('textarea.bc-text', row);
          if (bcLines(area.value).indexOf(code) < 0) area.value = bcLines(area.value).concat(code).join('\n');
        }
        bcRefresh(row);
        if (dirtyForm !== table.closest('form')) setDirty(table.closest('form'));
      })
      .catch(function () { toast('Не удалось найти штрихкод', 'error'); })
      .then(function () { input.value = ''; input.focus(); });
  }

  document.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && e.target.id === 'doc-scan') {
      e.preventDefault();
      bcScan(e.target);
    }
  });

  /* ---------- Инициализация страницы ---------- */

  function init() {
    $$('#lines tr.line').forEach(bcRefresh);
    $$('#toasts .toast').forEach(armToast);
    initNav();
    if (!isDirty()) clearDirty();
    submitting = false;
  }

  // htmx:load приходит и на первой загрузке, и для каждого подменённого фрагмента; init идемпотентен
  document.addEventListener('htmx:load', init);
  document.addEventListener('htmx:afterSettle', function () { document.body.classList.remove('nav-open'); });
  init();
})();
