// The mint's admin UI. The session is an HttpOnly cookie this script never
// sees; every call below rides on it. Everything shown is built with DOM
// methods and textContent, never innerHTML, so nothing the mint returns can
// inject markup. Loaded as a module: strict, and run once the page is parsed.

const $ = (selector) => document.querySelector(selector);

/** h('div', {class: 'x', onclick: fn}, 'text', child) */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key === 'class') el.className = value;
    else if (value === true) el.setAttribute(key, '');
    else el.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

class SignedOut extends Error {}

async function api(path, body) {
  const options = {credentials: 'same-origin', headers: {}};
  if (body !== undefined) {
    options.method = 'POST';
    options.headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  const res = await fetch(path, options);
  if (res.status === 401) {
    showLogin();
    throw new SignedOut('signed out');
  }
  const text = await res.text();
  if (!res.ok) throw new Error(text || res.statusText);
  return text ? JSON.parse(text) : {};
}

// ---- formatting ----

const nf = new Intl.NumberFormat();
function sat(msat) {
  if (msat === undefined || msat === null) return '–';
  const whole = Math.floor(msat / 1000);
  const rest = msat % 1000;
  return `${nf.format(whole)}${rest ? `.${String(rest).padStart(3, '0')}` : ''} sat`;
}
const satFromSat = (s) => (s === undefined || s === null ? '–' : `${nf.format(s)} sat`);
const short = (s, n = 10) => (s && s.length > 2 * n + 1 ? `${s.slice(0, n)}…${s.slice(-n)}` : s);

function toast(message, kind = 'ok') {
  const el = $('#toast');
  el.textContent = message;
  el.className = `toast ${kind}`;
  el.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (el.hidden = true), kind === 'error' ? 8000 : 3500);
}

function failed(err) {
  if (err instanceof SignedOut) return;
  toast(err.message || String(err), 'error');
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast('Copied');
  } catch {
    toast('Copy failed - select and copy by hand', 'error');
  }
}

const copyable = (text, label) =>
  h(
    'span',
    {class: 'copyable'},
    h('code', {title: text}, label ?? text),
    h('button', {class: 'icon', title: 'Copy', 'aria-label': 'Copy', onclick: () => copy(text)}, '⧉'),
  );

const qr = (data) => h('img', {class: 'qr', alt: 'QR code', src: `qr?data=${encodeURIComponent(data)}`});

const card = (title, ...body) => h('section', {class: 'card'}, h('h2', {}, title), ...body);

const kv = (rows) =>
  h(
    'dl',
    {class: 'kv'},
    rows.filter(Boolean).flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]),
  );

const stat = (label, value, hint) =>
  h(
    'div',
    {class: 'stat'},
    h('span', {class: 'label'}, label),
    h('span', {class: 'value'}, value),
    hint ? h('span', {class: 'hint'}, hint) : null,
  );

/** A button that asks to be clicked twice before it acts. */
function confirmButton(label, action, cls = 'danger') {
  const btn = h('button', {class: cls}, label);
  let armed = false;
  btn.addEventListener('click', async () => {
    if (!armed) {
      armed = true;
      btn.textContent = 'Click again to confirm';
      setTimeout(() => {
        armed = false;
        btn.textContent = label;
      }, 4000);
      return;
    }
    armed = false;
    btn.disabled = true;
    try {
      await action();
    } catch (err) {
      failed(err);
    } finally {
      btn.disabled = false;
      btn.textContent = label;
    }
  });
  return btn;
}

/** A form whose submit handler gets its values; errors become toasts. */
function form(fields, submitLabel, onSubmit, {danger = false} = {}) {
  const inputs = {};
  const el = h(
    'form',
    {class: 'form'},
    fields.map((f) => {
      const id = `f-${f.name}-${Math.random().toString(36).slice(2, 8)}`;
      const input =
        f.type === 'textarea'
          ? h('textarea', {id, name: f.name, rows: 3, placeholder: f.placeholder, required: f.required, spellcheck: 'false'})
          : h('input', {
              id,
              name: f.name,
              type: f.type || 'text',
              placeholder: f.placeholder,
              required: f.required,
              min: f.min,
              step: f.step,
              spellcheck: 'false',
              autocomplete: 'off',
            });
      inputs[f.name] = input;
      if (f.type === 'checkbox') return h('label', {class: 'check', for: id}, input, f.label);
      return h(
        'div',
        {class: 'field'},
        h('label', {for: id}, f.label),
        input,
        f.help ? h('small', {class: 'muted'}, f.help) : null,
      );
    }),
    h('button', {type: 'submit', class: danger ? 'danger' : 'primary'}, submitLabel),
  );
  el.addEventListener('submit', async (e) => {
    e.preventDefault();
    const button = el.querySelector('button[type=submit]');
    button.disabled = true;
    const values = Object.fromEntries(
      Object.entries(inputs).map(([k, i]) => [k, i.type === 'checkbox' ? i.checked : i.value.trim()]),
    );
    try {
      await onSubmit(values, el);
    } catch (err) {
      failed(err);
    } finally {
      button.disabled = false;
    }
  });
  return el;
}

function nodeDown(err) {
  return h('p', {class: 'warn'}, 'The Lightning node is not available: ', err.message || String(err));
}

// ---- views ----

async function overview() {
  const info = await api('info');
  let balance = null,
    channels = null,
    nodeError = null;
  try {
    [balance, channels] = await Promise.all([api('node/balance'), api('node/channels')]);
  } catch (err) {
    if (err instanceof SignedOut) throw err;
    nodeError = err;
  }
  const st = info.stats;
  const outbound = balance?.lightning.outbound_msat ?? 0;
  const coverage = st.outstanding_msat > 0 ? Math.floor((outbound / st.outstanding_msat) * 100) : null;

  return [
    h(
      'div',
      {class: 'grid'},
      card(
        'Mint',
        kv([
          ['Lightning', h('span', {class: info.lightning === 'ready' ? 'ok' : 'warn'}, info.lightning)],
          ['Address', copyable(info.lightning_address)],
          ['LNURL', copyable(info.lnurl, short(info.lnurl, 12))],
          ['Base URL', info.base_url],
          info.onion_url ? ['Onion', info.onion_url] : null,
          ['Node id', info.mint_pubkey ? copyable(info.mint_pubkey, short(info.mint_pubkey)) : '–'],
          [
            'Fees',
            info.base_fee_msat || info.fee_percent_ppm ? `${sat(info.base_fee_msat)} + ${info.fee_percent_ppm} ppm` : 'none',
          ],
          ['Mintable', `${sat(info.min_sendable_msat)} – ${sat(info.max_sendable_msat)}`],
          info.sunset_mint ? ['Sunset', h('span', {class: 'warn'}, 'minting and splitting refused')] : null,
        ]),
      ),
      card(
        'Notes',
        h(
          'div',
          {class: 'stats'},
          stat('Outstanding', sat(st.outstanding_msat), `${nf.format(st.outstanding_notes)} notes`),
          stat('Pending melts', nf.format(st.pending_notes)),
          stat('Spent', nf.format(st.spent_notes)),
          stat('Unpaid mints', nf.format(st.unpaid_mints)),
          stat('Usernames', nf.format(st.usernames)),
        ),
      ),
      card(
        'Liquidity',
        nodeError
          ? nodeDown(nodeError)
          : [
              h(
                'div',
                {class: 'stats'},
                stat(
                  'Can pay out',
                  sat(outbound),
                  coverage === null ? 'no notes outstanding' : `${coverage}% of outstanding notes`,
                ),
                stat('Can receive', sat(balance.lightning.inbound_msat)),
                stat(
                  'On-chain',
                  satFromSat(balance.onchain.confirmed_sat),
                  balance.onchain.unconfirmed_sat ? `+ ${satFromSat(balance.onchain.unconfirmed_sat)} unconfirmed` : null,
                ),
                stat('Channels', `${channels.filter((c) => c.usable).length} usable`, `${channels.length} total`),
              ),
              coverage !== null && coverage < 100
                ? h(
                    'p',
                    {class: 'warn'},
                    'Outbound liquidity is below what the notes are worth: not every holder could melt at once.',
                  )
                : null,
            ],
      ),
    ),
  ];
}

function liquidityBar(c) {
  const total = c.outbound_msat + c.inbound_msat || 1;
  const out = Math.round((c.outbound_msat / total) * 100);
  const bar = h(
    'div',
    {class: 'bar', title: `out ${sat(c.outbound_msat)} / in ${sat(c.inbound_msat)}`},
    h('span', {class: 'out'}),
    h('span', {class: 'in'}),
  );
  bar.firstChild.style.width = `${out}%`;
  bar.lastChild.style.width = `${100 - out}%`;
  return bar;
}

async function channelsView() {
  let channels, peers;
  try {
    [channels, peers] = await Promise.all([api('node/channels'), api('node/peers')]);
  } catch (err) {
    if (err instanceof SignedOut) throw err;
    return [card('Channels', nodeDown(err))];
  }
  const rows = channels.map((c) =>
    h(
      'tr',
      {},
      h('td', {}, copyable(c.peer, short(c.peer, 8))),
      h('td', {class: 'num'}, satFromSat(c.value_sat)),
      h('td', {}, liquidityBar(c), h('small', {class: 'muted'}, `${sat(c.outbound_msat)} out · ${sat(c.inbound_msat)} in`)),
      h(
        'td',
        {},
        h(
          'span',
          {class: c.usable ? 'pill ok' : c.ready ? 'pill warn' : 'pill'},
          c.usable ? 'usable' : c.ready ? 'ready' : 'pending',
        ),
        c.public ? h('span', {class: 'pill'}, 'public') : null,
      ),
      h(
        'td',
        {class: 'actions'},
        confirmButton(
          'Close',
          async () => {
            await api('node/channels/close', {channel_id: c.channel_id, force: false});
            toast('Closing channel');
            render();
          },
          'ghost',
        ),
        confirmButton('Force close', async () => {
          await api('node/channels/close', {channel_id: c.channel_id, force: true});
          toast('Force-closing channel');
          render();
        }),
      ),
    ),
  );
  return [
    card(
      'Channels',
      channels.length
        ? h(
            'table',
            {},
            h(
              'thead',
              {},
              h(
                'tr',
                {},
                ['Peer', 'Capacity', 'Liquidity', 'State', ''].map((t) => h('th', {}, t)),
              ),
            ),
            h('tbody', {}, rows),
          )
        : h('p', {class: 'muted'}, 'No channels yet. Fund the wallet, then open one.'),
    ),
    h(
      'div',
      {class: 'grid'},
      card(
        'Open a channel',
        form(
          [
            {
              name: 'peer',
              label: 'Peer',
              placeholder: 'pubkey@host:9735',
              required: true,
              help: "A connected peer's pubkey, or pubkey@host:port to connect first.",
            },
            {name: 'amount_sat', label: 'Amount (sat)', type: 'number', min: 20000, required: true},
            {name: 'public', label: 'Announce the channel (public)', type: 'checkbox'},
          ],
          'Open channel',
          async (v, el) => {
            const res = await api('node/channels', {peer: v.peer, amount_sat: Number(v.amount_sat), public: v.public});
            toast(`Opening channel ${short(res.channel_id)}`);
            el.reset();
            render();
          },
        ),
      ),
      card(
        'Peers',
        peers.length
          ? h(
              'ul',
              {class: 'list'},
              peers.map((p) => h('li', {}, copyable(p, short(p, 12)))),
            )
          : h('p', {class: 'muted'}, 'Not connected to any peer.'),
        form([{name: 'peer', label: 'Connect', placeholder: 'pubkey@host:9735', required: true}], 'Connect', async (v, el) => {
          await api('node/peers', {peer: v.peer});
          toast('Connected');
          el.reset();
          render();
        }),
      ),
    ),
  ];
}

/** Poll `check` every 2 s until it returns a final state, while `el` is shown. */
function poll(el, check) {
  const tick = async () => {
    if (!el.isConnected) return;
    try {
      if (await check()) return;
    } catch (err) {
      if (err instanceof SignedOut) return;
    }
    setTimeout(tick, 2000);
  };
  setTimeout(tick, 2000);
}

function paymentsView() {
  const receiveOut = h('div', {class: 'result'});
  const payOut = h('div', {class: 'result'});
  return [
    h(
      'div',
      {class: 'grid'},
      card(
        'Receive',
        h('p', {class: 'muted'}, 'An invoice paying the node itself, crediting no note: to take in liquidity or test a route.'),
        form(
          [
            {name: 'amount', label: 'Amount (sat)', type: 'number', min: 1, required: true},
            {name: 'description', label: 'Description', placeholder: 'optional'},
          ],
          'Create invoice',
          async (v) => {
            const inv = await api('node/invoice', {amount_msat: Number(v.amount) * 1000, description: v.description});
            const status = h('span', {class: 'pill warn'}, 'waiting for payment');
            receiveOut.replaceChildren(
              qr(`lightning:${inv.bolt11}`),
              copyable(inv.bolt11, short(inv.bolt11, 16)),
              h('p', {}, status),
            );
            poll(receiveOut, async () => {
              const s = await api(`node/invoice/${inv.payment_hash}`);
              if (s.paid) {
                status.textContent = 'paid';
                status.className = 'pill ok';
                toast('Invoice paid');
              }
              return s.paid;
            });
          },
        ),
        receiveOut,
      ),
      card(
        'Pay',
        h('p', {class: 'muted'}, "Pay a BOLT-11 invoice from the node's own liquidity."),
        form(
          [
            {name: 'bolt11', label: 'Invoice', type: 'textarea', required: true, placeholder: 'lnbc…'},
            {
              name: 'max_fee_sat',
              label: 'Max routing fee (sat)',
              type: 'number',
              min: 0,
              help: 'Default: the mint fee, at least 0.5% or 5 sat.',
            },
          ],
          'Pay',
          async (v, el) => {
            const body = {bolt11: v.bolt11};
            if (v.max_fee_sat !== '') body.max_fee_msat = Number(v.max_fee_sat) * 1000;
            const res = await api('node/pay', body);
            const status = h('span', {class: 'pill warn'}, 'sending');
            payOut.replaceChildren(h('p', {}, 'Payment ', copyable(res.payment_hash, short(res.payment_hash)), ' ', status));
            el.reset();
            poll(payOut, async () => {
              const s = await api(`node/payment/${res.payment_hash}`);
              const done = s.status === 'complete' || s.status === 'absent';
              status.textContent = s.status === 'complete' ? 'paid' : s.status === 'absent' ? 'failed' : 'sending';
              status.className = `pill ${s.status === 'complete' ? 'ok' : s.status === 'absent' ? 'bad' : 'warn'}`;
              return done;
            });
          },
        ),
        payOut,
      ),
    ),
  ];
}

async function walletView() {
  let balance;
  try {
    balance = await api('node/balance');
  } catch (err) {
    if (err instanceof SignedOut) throw err;
    return [card('Wallet', nodeDown(err))];
  }
  const addressOut = h('div', {class: 'result'});
  return [
    h(
      'div',
      {class: 'grid'},
      card(
        'On-chain wallet',
        h(
          'div',
          {class: 'stats'},
          stat('Confirmed', satFromSat(balance.onchain.confirmed_sat)),
          stat('Unconfirmed', satFromSat(balance.onchain.unconfirmed_sat)),
          stat('Claimable on close', satFromSat(balance.lightning.claimable_on_close_sat)),
        ),
        h('p', {class: 'muted'}, 'Keep a few confirmed coins here: closing an anchor channel pays its fee from them.'),
        h(
          'button',
          {
            class: 'primary',
            onclick: async () => {
              try {
                const {address} = await api('node/address', {});
                addressOut.replaceChildren(qr(`bitcoin:${address}`), copyable(address));
              } catch (err) {
                failed(err);
              }
            },
          },
          'New receive address',
        ),
        addressOut,
      ),
      card(
        'Send on-chain',
        form(
          [
            {name: 'address', label: 'Address', required: true, placeholder: 'bc1…'},
            {
              name: 'amount_sat',
              label: 'Amount (sat)',
              type: 'number',
              min: 1,
              help: 'Leave empty with "everything" ticked to sweep the wallet.',
            },
            {name: 'all', label: 'Send everything', type: 'checkbox'},
          ],
          'Send',
          async (v, el) => {
            if (!v.all && !v.amount_sat) throw new Error('Give an amount, or tick "Send everything".');
            const body = {address: v.address};
            if (!v.all) body.amount_sat = Number(v.amount_sat);
            const {txid} = await api('node/send', body);
            toast(`Sent: ${short(txid)}`);
            el.reset();
          },
          {danger: true},
        ),
      ),
    ),
  ];
}

async function notesView() {
  const [{pending_melts: pending}, {users}] = await Promise.all([api('pending'), api('users')]);
  const lookupOut = h('div', {class: 'result'});
  const pendingRows = Object.entries(pending);
  return [
    h(
      'div',
      {class: 'grid'},
      card(
        'Pending melts',
        pendingRows.length
          ? h(
              'table',
              {},
              h('thead', {}, h('tr', {}, h('th', {}, 'Payment hash'), h('th', {}, 'Notes'))),
              h(
                'tbody',
                {},
                pendingRows.map(([hash, notes]) =>
                  h('tr', {}, h('td', {}, copyable(hash, short(hash))), h('td', {}, notes.length)),
                ),
              ),
            )
          : h('p', {class: 'muted'}, 'None. Every melt has settled or been released.'),
        h(
          'button',
          {
            class: 'ghost',
            onclick: async () => {
              try {
                const report = await api('reconcile', {});
                const n = Object.keys(report).length;
                toast(n ? `Reconciled: ${Object.values(report).join(', ')}` : 'Nothing to reconcile');
                render();
              } catch (err) {
                failed(err);
              }
            },
          },
          'Reconcile now',
        ),
      ),
      card(
        'Look up a note',
        form([{name: 'note', label: "cp1… or a bearer note's hash", required: true}], 'Look up', async (v) => {
          const n = await api(`note/${encodeURIComponent(v.note)}`);
          lookupOut.replaceChildren(
            kv([
              [
                'Status',
                h('span', {class: n.status === 'outstanding' ? 'ok' : n.status === 'unknown' ? 'muted' : 'warn'}, n.status),
              ],
              ['Note id', copyable(n.note_id, short(n.note_id))],
              n.amount_msat !== undefined ? ['Value', sat(n.amount_msat)] : null,
              n.locked_at ? ['Credited', new Date(n.locked_at * 1000).toLocaleString()] : null,
              n.unpaid_mint ? ['Unpaid mint', copyable(n.unpaid_mint, short(n.unpaid_mint))] : null,
            ]),
          );
        }),
        lookupOut,
      ),
    ),
    card(
      'Lightning Address usernames',
      users.length
        ? h(
            'table',
            {},
            h(
              'thead',
              {},
              h(
                'tr',
                {},
                ['Username', 'cx1', 'Next index'].map((t) => h('th', {}, t)),
              ),
            ),
            h(
              'tbody',
              {},
              users.map((u) =>
                h(
                  'tr',
                  {},
                  h('td', {}, u.username),
                  h('td', {}, u.cx1 ? copyable(u.cx1, short(u.cx1, 12)) : '–'),
                  h('td', {class: 'num'}, u.next_index),
                ),
              ),
            ),
          )
        : h('p', {class: 'muted'}, 'No usernames registered.'),
    ),
  ];
}

// ---- shell ----

const views = {overview, channels: channelsView, payments: paymentsView, wallet: walletView, notes: notesView};
let current = 'overview';
let refreshTimer = null;

async function render() {
  clearTimeout(refreshTimer);
  for (const t of document.querySelectorAll('[role=tab]')) {
    t.setAttribute('aria-selected', String(t.dataset.tab === current));
  }
  const view = $('#view');
  try {
    const content = await views[current]();
    view.replaceChildren(...content);
  } catch (err) {
    if (err instanceof SignedOut) return;
    view.replaceChildren(card('Error', h('p', {class: 'error'}, err.message || String(err))));
  }
  updateHeader();
  // the overview keeps itself current; forms elsewhere would lose input
  if (current === 'overview') refreshTimer = setTimeout(render, 15000);
}

async function updateHeader() {
  try {
    const info = await api('info');
    $('#title').textContent = info.lightning_address;
    $('#subtitle').textContent = info.lightning === 'ready' ? '' : info.lightning;
    $('#status-dot').className = `dot ${info.lightning === 'ready' ? 'ok' : 'warn'}`;
  } catch {
    /* the view already shows what went wrong */
  }
}

function showLogin() {
  clearTimeout(refreshTimer);
  $('#app').hidden = true;
  $('#login').hidden = false;
  $('#token').focus();
}

function showApp() {
  $('#login').hidden = true;
  $('#app').hidden = false;
  render();
}

function selectTab(tab) {
  if (!views[tab]) tab = 'overview';
  current = tab;
  if (location.hash !== `#${tab}`) history.replaceState(null, '', `#${tab}`);
  render();
}

document.addEventListener('DOMContentLoaded', async () => {
  $('#login-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const error = $('#login-error');
    error.hidden = true;
    const res = await fetch('login', {
      method: 'POST',
      credentials: 'same-origin',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({token: $('#token').value}),
    });
    $('#token').value = '';
    if (res.ok) return showApp();
    error.textContent = res.status === 401 ? 'Wrong token.' : `Sign-in failed (${res.status}).`;
    error.hidden = false;
  });
  $('#logout').addEventListener('click', async () => {
    await fetch('logout', {method: 'POST', credentials: 'same-origin'});
    showLogin();
  });
  for (const t of document.querySelectorAll('[role=tab]')) {
    t.addEventListener('click', () => selectTab(t.dataset.tab));
  }
  window.addEventListener('hashchange', () => selectTab(location.hash.slice(1)));
  current = views[location.hash.slice(1)] ? location.hash.slice(1) : 'overview';

  // already signed in?
  const res = await fetch('info', {credentials: 'same-origin'});
  if (res.ok) showApp();
  else showLogin();
});
