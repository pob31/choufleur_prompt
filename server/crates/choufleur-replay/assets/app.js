// Helpers shared by every page.
//
// A plain script, not a module, so a page can use it without a build step and without
// `type="module"` changing how its own inline script is scoped. Everything is hung on
// one `App` object rather than loose globals.

const App = (() => {
  /** Seconds as `h:mm:ss`. */
  const fmt = (s) => {
    const t = Math.max(0, Math.floor(s));
    const h = Math.floor(t / 3600);
    const m = String(Math.floor((t % 3600) / 60)).padStart(h ? 2 : 1, '0');
    return `${h ? h + ':' : ''}${m}:${String(t % 60).padStart(2, '0')}`;
  };

  /** Accents stripped, spaces hyphenated — for ids, never for display. */
  const slug = (s) =>
    s.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase()
      .replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');

  // Absolute paths, always.
  //
  // The operator page fetches `'script.json'` with no leading slash, which resolves
  // against the current path. That is correct at `/` and wrong the moment a page is
  // served from anywhere else, and it fails by fetching the wrong thing rather than by
  // failing — the worst way for it to be wrong.
  const url = (path) => (path.startsWith('/') ? path : '/' + path);

  async function get(path) {
    const r = await fetch(url(path));
    if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
    return r.json();
  }

  /** POST JSON, and treat the server's error text as the message. */
  async function post(path, body) {
    const r = await fetch(url(path), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body ?? {}),
    });
    const text = await r.text();
    if (!r.ok) throw new Error(text || `${r.status}`);
    return text ? JSON.parse(text) : null;
  }

  // A destructive button that asks once.
  //
  // Replaces two copies in the operator page that had already drifted: the cue delete
  // stays armed for ever, the line delete disarms after four seconds. Four seconds is
  // the better of the two — an armed button left across a scene break is a delete
  // waiting for an accidental click — so that is the one kept.
  //
  // The operator page's line-delete version also calls `focus()` on the textarea behind
  // it, because WebKit does not focus a clicked `<button>` and the editor commits on
  // `focusout`, which would tear down the button before its second click. That is a
  // quirk of living inside an editor; a standalone button does not need it, and the
  // `onAfter` hook is here for the case that does.
  function confirmButton(el, { label, confirm, onConfirm, ms = 4000, onAfter }) {
    let armed = false;
    let timer = null;
    const rest = () => {
      armed = false;
      clearTimeout(timer);
      el.textContent = label;
      el.classList.remove('armed');
    };
    el.textContent = label;
    el.onmousedown = (e) => e.preventDefault();
    el.onclick = (e) => {
      e.stopPropagation();
      if (!armed) {
        armed = true;
        el.textContent = confirm;
        el.classList.add('armed');
        timer = setTimeout(rest, ms);
        onAfter?.();
        return;
      }
      rest();
      onConfirm();
    };
    return rest;
  }

  // Ten colours chosen to stay apart from each other at three pixels wide in the dark.
  const PALETTE = [
    '#ffd479', '#7fb2d9', '#8fd18f', '#e8918c', '#b48ee8',
    '#e8c98e', '#8ee8dc', '#e88ec4', '#a0a6b0', '#c9e88e',
  ];

  /** A row of colour buttons. `onPick` gets the new value. */
  function swatchRow(value, onPick) {
    const row = document.createElement('span');
    row.className = 'swatches';
    for (const hex of PALETTE) {
      const b = document.createElement('button');
      b.type = 'button';
      b.style.background = hex;
      b.classList.toggle('on', hex === value);
      b.onclick = (e) => {
        e.preventDefault();
        for (const other of row.children) other.classList.remove('on');
        b.classList.add('on');
        onPick(hex);
      };
      row.appendChild(b);
    }
    return row;
  }

  /** Escape text destined for innerHTML. Prefer textContent; this is for the rest. */
  const esc = (s) =>
    String(s).replace(/[&<>"']/g, (c) =>
      ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

  // `{ uiPort }` when this page is inside the desktop app, otherwise null.
  //
  // The same pages are served to the app's window and to every operator's tablet, and
  // a few things are true in only one of them: the app has one window rather than
  // tabs, and it knows the library server's port even when the page being shown
  // belongs to a show server on another one. Set by the shell as the window's first
  // script, so it is already there before any page runs — including after navigating
  // to a different port.
  const shell = window.__CHOUFLEUR_SHELL__ ?? null;

  // Copy, on a screen that may have no clipboard.
  //
  // `navigator.clipboard` is [SecureContext] and every operator reaches these pages over
  // plain http, so on exactly the devices that most need to copy an address — to escape
  // a scanner's in-app browser and open it somewhere real — the modern call is not there
  // at all. The deprecated one still works everywhere, and a deprecated call that works
  // beats a standard one that is absent.
  //
  // Which path is taken is decided *synchronously*, and that is the whole of why this
  // exists. `execCommand` is only permitted inside the user gesture that reached it, so
  // awaiting a rejected `writeText` and falling back afterwards would fall back into a
  // gesture that has already ended — the same silent failure, one layer further in.
  //
  // Returns whether it worked, because a button that says "copied" when nothing was is
  // worse than one that admits it.
  async function copyText(text) {
    if (navigator.clipboard?.writeText) {
      try { await navigator.clipboard.writeText(text); return true; } catch { return false; }
    }
    const ta = document.createElement('textarea');
    ta.value = text;
    // On the page but invisible: `display: none` has no selection to copy. iOS will not
    // select a plain readonly textarea by script either, hence `contentEditable` — which
    // costs nothing anywhere else.
    ta.setAttribute('readonly', '');
    ta.contentEditable = 'true';
    ta.style.cssText = 'position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;';
    document.body.appendChild(ta);
    try {
      const range = document.createRange();
      range.selectNodeContents(ta);
      const sel = getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      ta.setSelectionRange(0, text.length);
      return document.execCommand('copy');
    } catch {
      return false;
    } finally {
      ta.remove();
    }
  }

  return { fmt, slug, get, post, confirmButton, swatchRow, PALETTE, esc, shell, copyText };
})();
