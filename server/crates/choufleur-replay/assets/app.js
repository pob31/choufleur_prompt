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

  return { fmt, slug, get, post, confirmButton, swatchRow, PALETTE, esc };
})();
