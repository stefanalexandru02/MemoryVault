// MemoryVault viewer. No dependencies, no innerHTML: every node is built with textContent, because memory
// text comes from tool output and must never be interpreted as markup.
'use strict';

(() => {
  // ---------- small helpers ----------
  const $app = document.getElementById('app');

  function h(tag, props, ...children) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(props || {})) {
      if (v === undefined || v === null || v === false) continue;
      if (k === 'class') el.className = v;
      else if (k === 'text') el.textContent = v;
      else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (k === 'style') Object.assign(el.style, v); // CSSOM, not the style attribute: the CSP forbids inline styles
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, v);
    }
    for (const c of children.flat()) {
      if (c === undefined || c === null || c === false) continue;
      el.append(c.nodeType ? c : document.createTextNode(String(c)));
    }
    return el;
  }

  const store = {
    get(key) { try { return localStorage.getItem(key) ?? sessionStorage.getItem(key); } catch { return null; } },
    set(key, val, persistent) {
      try {
        if (persistent) { localStorage.setItem(key, val); sessionStorage.removeItem(key); }
        else { sessionStorage.setItem(key, val); localStorage.removeItem(key); }
      } catch { /* storage blocked: the session just will not survive a reload */ }
    },
    del(key) { try { localStorage.removeItem(key); sessionStorage.removeItem(key); } catch { /* ignore */ } },
  };

  const MIN = 60e3, HOUR = 60 * MIN, DAY = 24 * HOUR;
  function ago(iso) {
    const t = Date.parse(iso);
    if (Number.isNaN(t)) return '';
    const d = Date.now() - t;
    if (d < MIN) return 'just now';
    if (d < HOUR) return `${Math.floor(d / MIN)}m ago`;
    if (d < DAY) return `${Math.floor(d / HOUR)}h ago`;
    if (d < 30 * DAY) return `${Math.floor(d / DAY)}d ago`;
    return new Date(t).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
  }
  const fullTime = (iso) => (iso ? new Date(iso).toLocaleString() : '');
  function dayLabel(iso) {
    const d = new Date(iso);
    const start = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
    const diff = Math.round((start(new Date()) - start(d)) / DAY);
    if (diff === 0) return 'Today';
    if (diff === 1) return 'Yesterday';
    return d.toLocaleDateString(undefined, { weekday: 'short', year: 'numeric', month: 'short', day: 'numeric' });
  }
  const shortId = (id) => (id ? String(id).slice(0, 8) : '');
  const projectName = (key) => (key.split('/').filter(Boolean).slice(-1)[0] || key);
  const scopeLabel = (scope) => {
    if (!scope) return '';
    if (scope.kind === 'global') return 'global';
    if (scope.kind === 'project') return projectName(scope.key);
    return `${scope.kind}:${scope.key}`;
  };

  // ---------- API ----------
  const state = {
    token: null,
    tab: 'memory',
    admin: false,
    health: null,
    projects: new Map(), // key -> count seen
    filters: { q: '', tables: new Set(['observations', 'summaries', 'facts', 'procedures', 'documents']), project: '', type: '', period: '30d', asOf: '' },
    rows: [],
    offset: 0,
    hasMore: false,
    loading: false,
    selected: null, // {id, table}
    live: true,
    injection: null, // 'full' | 'lean' | 'smart': the user's default for what a session starts with; null when unknown
  };
  const PAGE = 50;

  class ApiError extends Error {
    constructor(status, message, code) { super(message); this.status = status; this.code = code; }
  }

  async function api(path, { method = 'GET', body } = {}) {
    const headers = { Authorization: `Bearer ${state.token}` };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    let res;
    try {
      res = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
    } catch {
      throw new ApiError(0, 'Cannot reach the server.');
    }
    if (res.status === 401) {
      signOut('That token was not accepted. It may be revoked or mistyped.');
      throw new ApiError(401, 'unauthorized');
    }
    const text = await res.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch { /* not JSON */ }
    if (!res.ok) {
      const e = data && data.error;
      throw new ApiError(res.status, (e && e.message) || text || `HTTP ${res.status}`, e && e.code);
    }
    return data;
  }

  // ---------- toasts ----------
  const toasts = h('div', { class: 'toasts', role: 'status' });
  document.body.append(toasts);
  function toast(message, bad = false) {
    const t = h('div', { class: `toast${bad ? ' bad' : ''}`, text: message });
    toasts.append(t);
    setTimeout(() => t.remove(), bad ? 6000 : 3200);
  }

  // ---------- theme ----------
  function applyTheme() {
    const t = store.get('mv.theme');
    if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
  }
  function toggleTheme() {
    const dark = document.documentElement.dataset.theme === 'dark' ||
      (!document.documentElement.dataset.theme && matchMedia('(prefers-color-scheme: dark)').matches);
    store.set('mv.theme', dark ? 'light' : 'dark', true);
    applyTheme();
  }
  applyTheme();

  // ---------- sign in / out ----------
  function renderSignIn(message = '') {
    stopTimers();
    state.token = null;
    const input = h('input', { class: 'input', type: 'password', autocomplete: 'off', spellcheck: 'false', placeholder: 'mem_…', id: 'token', required: true });
    const remember = h('input', { type: 'checkbox', id: 'remember' });
    const err = h('div', { class: 'error', role: 'alert', text: message });
    const form = h('form', {
      onsubmit: async (ev) => {
        ev.preventDefault();
        const tok = input.value.trim();
        if (!/^mem_[A-Za-z0-9]+_[A-Za-z0-9]+$/.test(tok)) { err.textContent = 'That does not look like a MemoryVault token (mem_…_…).'; return; }
        state.token = tok;
        err.textContent = '';
        btn.disabled = true;
        try {
          await api('/v1/me');
          store.set('mv.token', tok, remember.checked);
          await boot();
        } catch (e) {
          state.token = null;
          if (e.status !== 401) err.textContent = e.message;
        } finally { btn.disabled = false; }
      },
    },
    h('label', { class: 'field', for: 'token' }, h('span', { text: 'Token' }), input),
    h('label', { class: 'check', for: 'remember' }, remember, 'Remember on this device'),
    err);
    const btn = h('button', { class: 'btn primary', type: 'submit' }, 'Open the vault');
    form.append(btn);
    $app.replaceChildren(h('div', { class: 'signin' }, h('div', { class: 'signin-card' },
      h('h1', { class: 'brand' }, h('i'), 'MemoryVault'),
      h('p', { class: 'muted', text: 'Browse, search and forget what your agents remember. Paste a device token; it is sent only to this server.' }),
      form)));
    input.focus();
  }

  function signOut(message) {
    store.del('mv.token');
    renderSignIn(message || '');
  }

  // ---------- timers ----------
  let timers = [];
  function stopTimers() { timers.forEach(clearInterval); timers = []; }

  // ---------- shell ----------
  let $main, $health, $tabs;

  async function boot() {
    state.projects.clear();
    state.rows = [];
    state.selected = null;
    state.me = await api('/v1/me');
    state.admin = (state.me.scopes || []).includes('admin');
    try { state.injection = (await api('/v1/me/settings')).injection_mode; } catch { state.injection = null; }
    renderShell();
    refreshHealth();
    stopTimers();
    timers.push(setInterval(refreshHealth, 30000));
    timers.push(setInterval(() => { if (state.tab === 'memory' && state.live && !document.hidden) pollNew(); }, 20000));
    route();
  }

  function renderShell() {
    $tabs = h('nav', { class: 'tabs', 'aria-label': 'Sections' });
    $health = h('span', { class: 'pill', title: 'Server health' }, h('i', { class: 'dot' }), 'checking');
    const who = h('span', { class: 'pill', title: `Signed in as a ${state.me.token_kind} token` }, `${state.me.device_id}${state.admin ? ' · admin' : ''}`);
    const tabs = [['memory', 'Memories'], ['work', 'Work state']];
    if (state.admin) tabs.push(['admin', 'Admin']);
    for (const [id, label] of tabs) {
      $tabs.append(h('button', { class: 'tab', dataset: { tab: id }, onclick: () => { location.hash = `#/${id}`; } }, label));
    }
    $main = h('main', { class: 'main' });
    $app.replaceChildren(h('div', { class: 'shell' },
      h('header', { class: 'topbar' },
        h('div', { class: 'brand' }, h('i'), 'MemoryVault'),
        $tabs,
        h('div', { class: 'spacer' }),
        $health, who, injectionToggle(),
        h('button', { class: 'btn small', onclick: toggleTheme, title: 'Switch light and dark' }, 'Theme'),
        h('button', { class: 'btn small', onclick: () => signOut(''), title: 'Forget this token in this browser' }, 'Sign out')),
      $main));
  }

  // The user's default injection mode (PUT /v1/me/settings). A MEM_INJECTION in a client's ~/.memory/config
  // and a mode in the request take precedence over it.
  const INJECTION_MODES = [['full', 'Full context'], ['lean', 'Lean context'], ['smart', 'Smart context']];
  function injectionToggle() {
    if (!state.injection) return null;
    const sel = h('select', {
      class: 'input small', 'aria-label': 'Session start context',
      title: 'What a new session starts with: the full memory block, or only the profile, open work and a rubric for the search tools. A client set to MEM_INJECTION overrides this.',
      onchange: async (e) => {
        const mode = e.target.value;
        try {
          await api('/v1/me/settings', { method: 'PUT', body: { injection_mode: mode } });
          state.injection = mode;
          toast(`New sessions start with the ${mode} context`);
        } catch (err) {
          e.target.value = state.injection;
          if (err.status !== 401) toast(err.message, true);
        }
      },
    }, INJECTION_MODES.map(([value, label]) => h('option', { value, text: label })));
    sel.value = state.injection;
    return sel;
  }

  async function refreshHealth() {
    if (!$health) return;
    let level = 'bad', text = 'unreachable', title = '';
    try {
      const res = await fetch('/health');
      const body = await res.json();
      level = body.status === 'ok' ? 'ok' : body.status === 'degraded' ? 'warn' : 'bad';
      text = body.status === 'degraded' ? 'degraded: embeddings down' : body.status;
      title = `v${body.version}  db ${body.db ? 'up' : 'down'}  embed ${body.tei.embed ? 'up' : 'down'}  rerank ${body.tei.rerank ? 'up' : 'down'}`;
    } catch { /* keep unreachable */ }
    $health.replaceChildren(h('i', { class: `dot ${level}` }), text);
    $health.title = title;
  }

  // ---------- routing ----------
  function route() {
    const hash = location.hash.replace(/^#\/?/, '');
    const [head, arg] = hash.split('/');
    const tab = ['work', 'admin'].includes(head) ? head : 'memory';
    if (tab === 'admin' && !state.admin) { location.hash = '#/memory'; return; }
    state.tab = tab;
    for (const b of $tabs.children) b.setAttribute('aria-current', b.dataset.tab === tab ? 'page' : 'false');
    if (tab === 'memory') {
      renderMemory();
      if (head === 'm' && arg) openMemory(arg, null, { fromHash: true });
    } else if (tab === 'work') renderWork();
    else renderAdmin();
  }
  window.addEventListener('hashchange', () => { if (state.token) route(); });

  // ---------- memory view ----------
  const TIERS = [['observations', 'Observations'], ['summaries', 'Summaries'], ['facts', 'Facts'], ['procedures', 'Procedures'], ['documents', 'Documents']];
  const OBS_TYPES = ['bugfix', 'feature', 'refactor', 'change', 'discovery', 'decision', 'security_alert', 'security_note'];
  const TYPE_FILTERS = [...OBS_TYPES, 'document']; // 'document' narrows the search to ingested docs
  const PERIODS = [['24h', 'Last 24 hours'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['90d', 'Last 90 days'], ['all', 'All time']];
  let $list, $detail, $panes, $memory, $count, searchTimer;

  // The picked day (YYYY-MM-DD, local) counts up to its last millisecond.
  function asOfTime(day) {
    const [y, m, d] = day.split('-').map(Number);
    return new Date(y, m - 1, d + 1).getTime() - 1;
  }

  function sinceFor(period, base) {
    if (period === 'all') return '2000-01-01T00:00:00Z';
    const n = parseInt(period, 10);
    const unit = period.endsWith('h') ? HOUR : DAY;
    return new Date(base - n * unit).toISOString();
  }

  function searchParams(offset) {
    const f = state.filters;
    const p = new URLSearchParams();
    if (f.q.trim()) p.set('q', f.q.trim());
    p.set('tables', [...f.tables].join(','));
    if (f.project === '__global') p.set('scopes', 'global');
    else if (f.project) p.set('scopes', `project:${f.project}`);
    if (f.type) p.set('types', f.type);
    const base = f.asOf ? asOfTime(f.asOf) : Date.now();
    p.set('since', sinceFor(f.period, base));
    if (f.asOf) p.set('as_of', new Date(base).toISOString());
    p.set('limit', String(PAGE));
    p.set('offset', String(offset));
    return p;
  }

  function noteProjects(rows) {
    for (const r of rows) {
      if (r.scope && r.scope.kind === 'project') state.projects.set(r.scope.key, (state.projects.get(r.scope.key) || 0) + 1);
    }
  }

  function renderMemory() {
    const f = state.filters;
    const search = h('input', {
      class: 'input', type: 'search', id: 'q', placeholder: 'Search memories  ( / )', value: f.q, autocomplete: 'off',
      oninput: (e) => { f.q = e.target.value; clearTimeout(searchTimer); searchTimer = setTimeout(() => load(true), 300); },
    });
    const chips = h('div', { class: 'chips', role: 'group', 'aria-label': 'Kinds of memory' },
      TIERS.map(([id, label]) => h('button', {
        class: 'chip-toggle', type: 'button', 'aria-pressed': String(f.tables.has(id)),
        onclick: (e) => {
          if (f.tables.has(id)) { if (f.tables.size > 1) f.tables.delete(id); } else f.tables.add(id);
          e.currentTarget.setAttribute('aria-pressed', String(f.tables.has(id)));
          for (const b of chips.children) b.setAttribute('aria-pressed', String(f.tables.has(b.dataset.id)));
          load(true);
        }, dataset: { id },
      }, label)));
    const projectSel = h('select', { class: 'input', id: 'project', onchange: (e) => { f.project = e.target.value; load(true); } });
    const typeSel = h('select', {
      class: 'input', id: 'type', onchange: (e) => { f.type = e.target.value; load(true); },
    }, h('option', { value: '', text: 'Any type' }), TYPE_FILTERS.map((t) => h('option', { value: t, text: t.replace('_', ' ') })));
    typeSel.value = f.type;
    const periodSel = h('select', { class: 'input', id: 'period', onchange: (e) => { f.period = e.target.value; load(true); } },
      PERIODS.map(([v, l]) => h('option', { value: v, text: l })));
    periodSel.value = f.period;
    const today = new Date();
    const asOfInput = h('input', {
      class: 'input', type: 'date', id: 'asof', value: f.asOf, 'aria-label': 'Show memory as it was on this day',
      max: `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`,
      onchange: (e) => { f.asOf = e.target.value; nowBtn.hidden = !f.asOf; load(true); },
    });
    const nowBtn = h('button', {
      class: 'mini', type: 'button', text: 'Now', hidden: !f.asOf, title: 'Back to the present',
      onclick: () => { f.asOf = ''; asOfInput.value = ''; nowBtn.hidden = true; load(true); },
    });
    const live = h('input', { type: 'checkbox', id: 'live', onchange: (e) => { state.live = e.target.checked; } });
    live.checked = state.live;
    $count = h('span', { class: 'muted' });

    $list = h('div', { class: 'list', role: 'listbox', 'aria-label': 'Memories', tabindex: '-1' });
    $detail = h('section', { class: 'detail', 'aria-label': 'Memory detail' });
    $panes = h('div', { class: 'panes' }, $list, $detail);
    $memory = h('div', { class: 'memory' },
      h('div', { class: 'filters' },
        h('label', { class: 'field search' }, h('span', { text: 'Search' }), search),
        h('div', { class: 'field' }, h('span', { text: 'Kinds' }), chips),
        h('label', { class: 'field' }, h('span', { text: 'Project' }), projectSel),
        h('label', { class: 'field' }, h('span', { text: 'Type' }), typeSel),
        h('label', { class: 'field' }, h('span', { text: 'Period' }), periodSel),
        h('div', { class: 'field' }, h('span', { text: 'As of' }), h('div', { class: 'asof' }, asOfInput, nowBtn)),
        h('label', { class: 'check' }, live, 'Live'),
        $count),
      $panes);
    $main.replaceChildren($memory);
    syncProjectSelect();
    renderDetailEmpty();
    load(true);
  }

  function syncProjectSelect() {
    const sel = document.getElementById('project');
    if (!sel) return;
    const keep = state.filters.project;
    const opts = [h('option', { value: '', text: 'All projects' }), h('option', { value: '__global', text: 'Global only' })];
    const keys = [...state.projects.keys()].sort((a, b) => projectName(a).localeCompare(projectName(b)));
    for (const k of keys) opts.push(h('option', { value: k, text: k }));
    if (keep && keep !== '__global' && !state.projects.has(keep)) opts.push(h('option', { value: keep, text: keep }));
    sel.replaceChildren(...opts);
    sel.value = keep;
  }

  async function load(reset) {
    if (state.loading && !reset) return;
    const f = state.filters;
    if (reset) { state.offset = 0; state.rows = []; }
    state.loading = true;
    const myQuery = JSON.stringify([f.q, [...f.tables], f.project, f.type, f.period, f.asOf]);
    state.lastQuery = myQuery;
    renderList({ loading: true });
    try {
      const res = await api(`/v1/search?${searchParams(state.offset)}`);
      if (state.lastQuery !== myQuery) return; // a newer search replaced this one
      const rows = res.rows || [];
      noteProjects(rows);
      state.rows = reset ? rows : state.rows.concat(rows);
      state.offset += rows.length;
      state.hasMore = rows.length === PAGE;
      syncProjectSelect();
      renderList({});
    } catch (e) {
      if (e.status !== 401) renderList({ error: e.message });
    } finally { state.loading = false; }
  }

  // Live updates: fetch the newest page and add rows that are not shown yet.
  async function pollNew() {
    const f = state.filters;
    if (f.q.trim() || f.asOf || state.loading || !$list) return;
    try {
      const res = await api(`/v1/search?${searchParams(0)}`);
      const have = new Set(state.rows.map((r) => r.id));
      const fresh = (res.rows || []).filter((r) => !have.has(r.id));
      if (!fresh.length) return;
      noteProjects(fresh);
      fresh.forEach((r) => { r._fresh = true; });
      state.rows = fresh.concat(state.rows);
      state.offset += fresh.length;
      syncProjectSelect();
      renderList({});
      setTimeout(() => state.rows.forEach((r) => { delete r._fresh; }), 2500);
    } catch { /* a missed poll is fine */ }
  }

  function tagFor(row) {
    const type = row.table === 'facts' ? 'fact' : row.table === 'procedures' ? 'procedure' : row.table === 'summaries' ? 'summary' : row.table === 'documents' ? 'document' : (row.type || 'change');
    const label = row.table === 'facts' ? 'fact' : row.table === 'procedures' ? 'procedure' : row.table === 'summaries' ? `${row.type || 'summary'}` : row.table === 'documents' ? 'document' : (row.type || 'note').replace('_', ' ');
    return h('span', { class: `tag t-${type}`, text: label });
  }

  function renderList({ loading, error } = {}) {
    if (!$list) return;
    const rows = state.rows;
    const nodes = [];
    if (error) nodes.push(h('div', { class: 'empty error', role: 'alert', text: error }));
    else if (!rows.length && loading) nodes.push(h('div', { class: 'empty', text: 'Loading…' }));
    else if (!rows.length) nodes.push(h('div', { class: 'empty', text: state.filters.q.trim() ? 'Nothing matches that search.' : state.filters.asOf ? 'Nothing was in memory in this period as of that day.' : 'No memories in this period yet.' }));
    let lastDay = '';
    const ranked = state.filters.q.trim() !== '';
    for (const r of rows) {
      if (!ranked) {
        const d = dayLabel(r.created_at);
        if (d !== lastDay) { nodes.push(h('div', { class: 'day', text: d })); lastDay = d; }
      }
      const sel = state.selected && state.selected.id === r.id;
      nodes.push(h('button', {
        class: `row${r._fresh ? ' fresh' : ''}`, role: 'option', 'aria-selected': String(!!sel), dataset: { id: r.id },
        onclick: () => openMemory(r.id, r.table),
      },
      tagFor(r),
      h('div', {},
        h('div', { class: 'title', text: r.title || '(untitled)' }),
        h('div', { class: 'meta' },
          h('span', { text: scopeLabel(r.scope) }),
          h('span', { title: fullTime(r.created_at), text: ago(r.created_at) }),
          r.token_estimate ? h('span', { text: `~${r.token_estimate} tok` }) : null,
          ranked && typeof r.score === 'number' ? h('span', { text: `score ${r.score.toFixed(2)}` }) : null))));
    }
    if (rows.length && state.hasMore) {
      nodes.push(h('div', { class: 'more' }, h('button', { class: 'btn', onclick: () => load(false), disabled: state.loading }, state.loading ? 'Loading…' : 'Load more')));
    }
    const top = $list.scrollTop;
    $list.replaceChildren(...nodes);
    $list.scrollTop = top;
    if ($count) $count.textContent = rows.length ? `${rows.length}${state.hasMore ? '+' : ''} shown` : '';
  }

  // ---------- detail ----------
  function renderDetailEmpty() {
    $detail.replaceChildren(h('div', { class: 'empty', text: 'Select a memory to read it, see where it came from, or forget it.' }));
    $memory.classList.remove('has-detail');
  }

  function closeDetail() {
    state.selected = null;
    if (location.hash.startsWith('#/m/')) history.replaceState(null, '', '#/memory');
    renderDetailEmpty();
    renderList({});
  }

  async function openMemory(id, table, opts = {}) {
    state.selected = { id, table };
    if (!opts.fromHash) history.replaceState(null, '', `#/m/${id}`);
    renderList({});
    $memory.classList.add('has-detail');
    $detail.replaceChildren(h('div', { class: 'empty', text: 'Loading…' }));
    $detail.scrollTop = 0;
    try {
      // display: a person reads this body, so it is not counted as served to or recalled by a model.
      const res = await api('/v1/memories/get', { method: 'POST', body: { ids: [id], display: true } });
      if (!state.selected || state.selected.id !== id) return;
      const item = (res.items || [])[0];
      if (!item) { $detail.replaceChildren(h('div', { class: 'empty', text: 'That memory is gone, or you cannot see it.' })); return; }
      state.selected.table = item.table;
      renderItem(item, (res.feedback || {})[item.id]);
    } catch (e) {
      if (e.status !== 401) $detail.replaceChildren(h('div', { class: 'empty error', role: 'alert', text: e.message }));
    }
  }

  const section = (title, ...body) => h('div', { class: 'section' }, h('h3', { text: title }), ...body);
  const bullets = (items) => h('ul', { class: 'bullets' }, items.map((t) => h('li', { text: t })));
  const prose = (text) => h('div', { class: 'prose', text });
  const kv = (pairs) => h('div', { class: 'kv' }, pairs.filter(([, v]) => v !== undefined && v !== null && v !== '').map(([k, v]) => h('span', {}, `${k} `, h('b', { text: String(v) }))));
  const chipsLine = (items, onclick) => h('div', { class: 'chips-line' }, items.map((t) => (onclick ? h('button', { class: 'mini', type: 'button', onclick: () => onclick(t), text: t }) : h('span', { class: 'mini', text: t }))));
  const link = (id, label) => h('button', { class: 'mini', type: 'button', onclick: () => openMemory(id, null), title: id, text: label || shortId(id) });

  function filterByFile(path) {
    state.filters.q = path;
    const q = document.getElementById('q');
    if (q) q.value = path;
    load(true);
  }

  function renderItem(item, feedback) {
    const body = [];
    let head;
    if (item.table === 'observations') {
      head = h('div', { class: 'detail-head' },
        h('div', {}, tagFor({ table: 'observations', type: item.type })),
        h('h2', { text: item.title }),
        item.subtitle ? h('div', { class: 'sub', text: item.subtitle }) : null,
        kv([['project', scopeLabel(item.scope)], ['saved', fullTime(item.created_at)], ['importance', item.importance.toFixed(2)],
          ['sensitivity', item.sensitivity !== 'normal' ? item.sensitivity : ''], ['~tokens', item.token_estimate || ''],
          ['archived', item.archived_at ? fullTime(item.archived_at) : '']]));
      if (item.narrative) body.push(section('What happened', prose(item.narrative)));
      if (item.facts && item.facts.length) body.push(section('Facts', bullets(item.facts)));
      if (item.files && item.files.length) body.push(section('Files', chipsLine(item.files, filterByFile)));
      if (item.concepts && item.concepts.length) body.push(section('Concepts', chipsLine(item.concepts)));
      if (item.entities && item.entities.length) body.push(section('Entities', chipsLine(item.entities)));
      if (item.superseded_by) body.push(section('Superseded by', link(item.superseded_by)));
    } else if (item.table === 'summaries') {
      head = h('div', { class: 'detail-head' },
        h('div', {}, h('span', { class: 'tag t-summary', text: `${item.grain} summary` })),
        h('h2', { text: item.request || `${item.grain} summary` }),
        kv([['project', scopeLabel(item.scope)], ['saved', fullTime(item.created_at)],
          ['period', item.period_start ? `${fullTime(item.period_start)} to ${fullTime(item.period_end)}` : ''], ['~tokens', item.token_estimate || '']]));
      for (const [key, label] of [['investigated', 'Investigated'], ['learned', 'Learned'], ['completed', 'Completed'], ['next_steps', 'Next steps'], ['notes', 'Notes']]) {
        if (item[key]) body.push(section(label, prose(item[key])));
      }
    } else if (item.table === 'facts') {
      const pct = Math.round(item.confidence * 100);
      head = h('div', { class: 'detail-head' },
        h('div', {}, h('span', { class: 'tag t-fact', text: 'fact' }), ' ', h('span', { class: 'mini', text: item.status }),
          item.conflict ? h('span', { class: 'mini warn', text: 'conflict' }) : null,
          item.caution ? h('span', { class: 'mini warn', text: item.caution }) : null,
          item.pinned ? h('span', { class: 'mini', text: ' pinned' }) : null),
        h('h2', { class: 'fact-line' }, `${item.subject.canonical_name} `, h('code', { text: item.predicate }), ' = ', item.value_text),
        kv([['scope', scopeLabel(item.scope)], ['first seen', fullTime(item.first_seen_at)], ['last confirmed', fullTime(item.last_confirmed_at)],
          ['confirmed', `${item.confirmations}×`], ['importance', item.importance.toFixed(2)]]));
      body.push(section('Confidence', h('div', {}, h('span', { class: 'bar' }, h('i', { style: { width: `${pct}%` } })), ` ${pct}%`)));
      if (item.valid_from || item.valid_to) body.push(section('Valid', kv([['from', fullTime(item.valid_from)], ['until', fullTime(item.valid_to)]])));
      if (item.supersedes_id) body.push(section('Replaced', h('div', {}, 'This value replaced an older one: ', link(item.supersedes_id))));
      if (item.superseded_by) body.push(section('Replaced by', link(item.superseded_by)));
    } else if (item.table === 'procedures') {
      head = h('div', { class: 'detail-head' },
        h('div', {}, h('span', { class: 'tag t-procedure', text: 'procedure' }), ' ', h('span', { class: 'mini', text: `v${item.version} ${item.status}` })),
        h('h2', { text: item.name }),
        kv([['task class', item.task_class], ['scope', scopeLabel(item.scope)], ['saved', fullTime(item.created_at)],
          ['worked', `${item.success_count}×`], ['failed', `${item.failure_count}×`], ['last used', item.last_used_at ? fullTime(item.last_used_at) : '']]));
      if (item.trigger_text) body.push(section('When to use it', prose(item.trigger_text)));
      body.push(section('Steps', h('div', { class: 'pre', text: item.body_md })));
    } else if (item.table === 'documents') {
      const source = item.heading_path && item.heading_path.length ? `${item.source_uri}#${item.heading_path.join(' > ')}` : item.source_uri;
      head = h('div', { class: 'detail-head' },
        h('div', {}, tagFor({ table: 'documents' }), item.retired_at ? h('span', { class: 'mini warn', text: 'retired' }) : null),
        h('h2', { text: item.title }),
        kv([['source', source], ['scope', scopeLabel(item.scope)], ['ingested', fullTime(item.created_at)], ['chunk', item.chunk_index + 1],
          ['~tokens', item.token_estimate || ''], ['retired', item.retired_at ? fullTime(item.retired_at) : '']]));
      body.push(section('Text', h('div', { class: 'pre', text: item.content })));
    }

    if (item.table !== 'summaries' && item.table !== 'documents') body.push(feedbackSection(item, feedback || {}));

    const around = h('div', { class: 'section', hidden: true });
    const versions = item.table === 'facts' || item.table === 'procedures'
      ? h('div', { class: 'section' }, h('h3', { text: 'History' }), h('div', { class: 'muted', text: 'Loading…' })) : null;
    const why = h('div', { class: 'section' }, h('h3', { text: 'Where this came from' }), h('div', { class: 'muted', text: 'Loading…' }));
    const canTimeline = item.table === 'observations' || item.table === 'summaries';
    const actions = h('div', { class: 'actions' },
      h('button', { class: 'btn small back', onclick: closeDetail }, '← Back'),
      canTimeline ? h('button', { class: 'btn small', onclick: () => showTimeline(item.id, around) }, 'What was around it') : null,
      h('button', { class: 'btn small', onclick: () => { navigator.clipboard?.writeText(item.id).then(() => toast('Id copied'), () => toast(item.id)); } }, 'Copy id'),
      h('button', { class: 'btn small danger', onclick: () => confirmForget(item) }, 'Forget…'));
    const idLine = h('div', { class: 'muted', title: item.id }, h('code', { text: item.id }));
    $detail.replaceChildren(h('div', { class: 'detail-inner' }, actions, head, ...body, versions, around, why, idLine));
    if (versions) loadHistory(item.id, versions);
    loadExplain(item.id, why);
  }

  // helpful / wrong / outdated on a memory, with how many were given. A verdict counts once per user; a
  // fact with enough negative ones is queued for the consolidation planner, which retracts it (undoable).
  const VERDICTS = [['helpful', 'Helpful'], ['wrong', 'Wrong'], ['outdated', 'Outdated']];
  function feedbackSection(item, counts) {
    const note = h('input', { class: 'input', type: 'text', maxlength: '500', placeholder: 'Optional note, kept with the verdict', 'aria-label': 'Feedback note' });
    async function send(verdict) {
      try {
        const res = await api(`/v1/memories/${item.id}/feedback`, { method: 'POST', body: { verdict, note: note.value.trim() || undefined } });
        toast(res.applied ? (res.queued ? 'Noted. It is queued for review.' : 'Noted.') : `You already marked this ${verdict}.`);
        if (res.applied && state.selected && state.selected.id === item.id) openMemory(item.id, item.table, { fromHash: true });
      } catch (e) { if (e.status !== 401) toast(e.message, true); }
    }
    return h('div', { class: 'section' }, h('h3', { text: 'Was this right?' }),
      h('div', { class: 'actions' }, VERDICTS.map(([verdict, label]) =>
        h('button', { class: `btn small verdict ${verdict}`, type: 'button', onclick: () => send(verdict) },
          label, h('span', { class: 'count', text: String(counts[verdict] || 0) })))),
      note);
  }

  // The chain of versions a fact or procedure went through, oldest first; the one on screen is marked.
  async function loadHistory(id, box) {
    const title = h('h3', { text: 'History' });
    try {
      const res = await api(`/v1/memories/${id}/history`);
      if (!state.selected || state.selected.id !== id) return;
      const entries = res.entries || [];
      if (entries.length < 2) { box.replaceChildren(title, h('div', { class: 'muted', text: 'This is the only version so far.' })); return; }
      box.replaceChildren(title, h('ol', { class: 'history' }, entries.map((e) => h('li', { class: e.current ? 'current' : '', 'aria-current': e.current ? 'true' : false },
        e.current ? h('div', { class: 'hist-text', text: e.text })
          : h('button', { class: 'hist-link', type: 'button', onclick: () => openMemory(e.id, e.table), text: e.text }),
        kv([['status', e.status], ['from', e.valid_from ? fullTime(e.valid_from) : ''], ['until', e.valid_to ? fullTime(e.valid_to) : (e.status === 'active' ? 'now' : '')],
          ['ended by', e.ended_by], ['why', e.reason], ['model', e.model]])))));
    } catch (e) {
      if (e.status !== 401) box.replaceChildren(title, h('div', { class: 'muted', text: e.message }));
    }
  }

  async function showTimeline(id, box) {
    box.hidden = false;
    box.replaceChildren(h('h3', { text: 'What was around it' }), h('div', { class: 'muted', text: 'Loading…' }));
    try {
      const res = await api(`/v1/timeline?anchor_id=${id}&before=5&after=5`);
      const rows = res.rows || [];
      box.replaceChildren(h('h3', { text: 'What was around it' }),
        rows.length ? h('div', { class: 'card' }, rows.map((r) => h('div', { class: `meta${r.is_anchor ? ' anchor' : ''}` },
          h('span', { class: 'muted', text: fullTime(r.created_at) }), ' ', tagFor(r), ' ',
          r.is_anchor ? h('span', { text: r.title }) : h('button', { class: 'mini', type: 'button', onclick: () => openMemory(r.id, r.table), text: r.title })))) : h('div', { class: 'muted', text: 'Nothing nearby.' }));
    } catch (e) { box.replaceChildren(h('h3', { text: 'What was around it' }), h('div', { class: 'error', text: e.message })); }
  }

  const jsonPairs = (obj) => Object.entries(obj || {}).map(([k, v]) => [k, typeof v === 'object' ? JSON.stringify(v) : v]);

  async function loadExplain(id, box) {
    try {
      const ex = await api(`/v1/memories/${id}/explain`);
      if (!state.selected || state.selected.id !== id) return;
      const parts = [h('h3', { text: 'Where this came from' })];
      const prov = (p) => {
        if (!p) return null;
        const s = p.session;
        return h('div', { class: 'card' },
          s ? kv([['session', shortId(s.id)], ['tool', s.harness], ['project', s.project_key], ['device', s.device_id || s.hostname], ['started', fullTime(s.started_at)]]) : h('div', { class: 'muted', text: 'No session recorded (written directly).' }),
          p.event_seqs && p.event_seqs.length ? kv([['events', p.event_seqs.length > 12 ? `${p.event_seqs.slice(0, 12).join(', ')} …` : p.event_seqs.join(', ')]]) : null,
          p.extracted_by && Object.keys(p.extracted_by).length ? kv(jsonPairs(p.extracted_by)) : null);
      };
      if (ex.provenance) parts.push(prov(ex.provenance));
      if (ex.evidence && ex.evidence.length) {
        parts.push(h('div', { class: 'muted', text: `Rests on ${ex.evidence.length} observation${ex.evidence.length === 1 ? '' : 's'}:` }));
        for (const ev of ex.evidence) parts.push(h('div', { class: 'card' },
          h('div', {}, tagFor({ table: 'observations', type: ev.type }), ' ', link(ev.id, ev.title), ' ', h('span', { class: 'muted', text: ago(ev.created_at) })), prov(ev.provenance)));
      }
      if (!ex.provenance && !(ex.evidence && ex.evidence.length)) parts.push(h('div', { class: 'muted', text: 'No source observations are linked to this one.' }));
      if (ex.audit && ex.audit.length) {
        parts.push(h('h3', { text: 'Audit log' }));
        parts.push(h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['When', 'Who', 'What', 'Why'].map((c) => h('th', { text: c })))),
          h('tbody', {}, ex.audit.map((a) => h('tr', {}, h('td', { text: fullTime(a.created_at) }), h('td', { text: a.actor }), h('td', { text: a.action }), h('td', { text: [a.reason, a.model].filter(Boolean).join(' · ') })))))));
      }
      box.replaceChildren(...parts);
    } catch (e) {
      if (e.status !== 401) box.replaceChildren(h('h3', { text: 'Where this came from' }), h('div', { class: 'muted', text: e.status === 404 ? 'No provenance recorded.' : e.message }));
    }
  }

  // ---------- forget ----------
  function confirmForget(item) {
    const label = item.table === 'facts' ? `${item.predicate} = ${item.value_text}` : item.table === 'procedures' ? item.name : item.title || item.request || item.id;
    const reason = h('input', { class: 'input', type: 'text', maxlength: '500', placeholder: 'Optional, kept in the audit log', id: 'reason' });
    const go = h('button', { class: 'btn danger solid', type: 'submit', value: 'go' }, 'Forget it');
    const dlg = h('dialog', { 'aria-labelledby': 'forget-title' },
      h('form', { method: 'dialog' },
        h('h3', { id: 'forget-title', text: 'Forget this memory?' }),
        h('div', { class: 'pre', text: label }),
        h('p', { class: 'muted', text: item.table === 'facts' ? 'The fact is retracted: agents stop using it, and the history stays in the audit log.' : item.table === 'procedures' ? 'The procedure is retired.' : item.table === 'documents' ? 'This chunk of the document is retired: it no longer shows up in searches. `mem-hook ingest --force` on the file brings it back.' : 'The observation is archived: it no longer shows up in searches or context. The viewer cannot undo this.' }),
        h('label', { class: 'field', for: 'reason' }, h('span', { text: 'Why' }), reason),
        h('menu', {}, h('button', { class: 'btn', type: 'submit', value: 'cancel' }, 'Cancel'), go)));
    document.body.append(dlg);
    dlg.addEventListener('close', async () => {
      const why = reason.value.trim();
      const confirmed = dlg.returnValue === 'go';
      dlg.remove();
      if (!confirmed) return;
      try {
        const q = why ? `?reason=${encodeURIComponent(why)}` : '';
        const res = await api(`/v1/memories/${item.id}${q}`, { method: 'DELETE' });
        toast(`Forgotten (${res.action})`);
        state.rows = state.rows.filter((r) => r.id !== item.id);
        closeDetail();
      } catch (e) { if (e.status !== 401) toast(e.message, true); }
    });
    dlg.showModal();
    reason.focus();
  }

  // ---------- work state ----------
  let workScope = '';
  async function renderWork() {
    const sel = h('select', { class: 'input', id: 'wscope', onchange: (e) => { workScope = e.target.value; fill(); } },
      h('option', { value: '', text: 'Default scopes (global and this device)' }), h('option', { value: 'global', text: 'Global' }),
      [...state.projects.keys()].sort().map((k) => h('option', { value: `project:${k}`, text: k })));
    sel.value = workScope;
    const out = h('div', { class: 'page-inner' });
    $main.replaceChildren(h('div', { class: 'page' }, h('div', { class: 'page-inner' },
      h('h2', { text: 'Work state' }),
      h('p', { class: 'muted', text: 'Open lists agents keep per project, like todo or handoff. Later writes win; they are shown at the start of every session until closed.' }),
      h('div', { class: 'toolbar' }, h('label', { class: 'field' }, h('span', { text: 'Scope' }), sel)), out,
      statsPanel())));
    async function fill() {
      out.replaceChildren(h('div', { class: 'muted', text: 'Loading…' }));
      try {
        const res = await api(`/v1/work-state${workScope ? `?scope=${encodeURIComponent(workScope)}` : ''}`);
        const lists = res.lists || [];
        if (!lists.length) { out.replaceChildren(h('div', { class: 'empty', text: 'No open work lists here.' })); return; }
        out.replaceChildren(...lists.map((l) => h('div', { class: 'card' },
          h('h3', { text: l.list_name }),
          h('div', { class: 'muted', text: [l.scope ? scopeLabel(l.scope) : '', l.updated_at ? `updated ${ago(l.updated_at)}` : ''].filter(Boolean).join(' · ') }),
          h('table', { class: 't' }, h('tbody', {}, Object.entries(l.fields || {}).map(([k, v]) => h('tr', {}, h('td', {}, h('b', { text: k })), h('td', { text: typeof v === 'string' ? v : JSON.stringify(v) }))))))));
      } catch (e) { if (e.status !== 401) out.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    fill();
  }

  // Tokens served per session start (by injection mode), recall and memory get, and the work recalled
  // (GET /v1/stats/recall).
  const CALL_LABELS = { context: 'Session start', smart: 'First prompt', recall: 'Recall', get: 'Memory get' };
  function statsPanel() {
    const days = h('select', { class: 'input', 'aria-label': 'Window', onchange: () => load() },
      [1, 7, 30, 90].map((d) => h('option', { value: String(d), text: d === 1 ? 'Last day' : `Last ${d} days` })));
    days.value = '7';
    const all = state.admin ? h('input', { type: 'checkbox', onchange: () => load() }) : null;
    const out = h('div', {});
    async function load() {
      out.replaceChildren(h('div', { class: 'muted', text: 'Loading…' }));
      try {
        const stats = await api(`/v1/stats/recall?days=${days.value}${all && all.checked ? '&all=true' : ''}`);
        if (!stats.modes.length) { out.replaceChildren(h('div', { class: 'empty', text: 'No context or recall calls in this window.' })); return; }
        const num = (v) => h('td', { class: 'num', text: Math.round(v).toLocaleString() });
        out.replaceChildren(h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['Call', 'Mode', 'Calls', 'Avg tokens', 'p50', 'p95', 'Avg memories'].map((c) => h('th', { text: c })))),
          h('tbody', {}, stats.modes.map((m) => h('tr', {},
            h('td', { text: CALL_LABELS[m.mode] || m.mode }), h('td', { text: m.injection || '–' }),
            num(m.calls), num(m.avg_tokens), num(m.p50_tokens), num(m.p95_tokens), h('td', { class: 'num', text: m.avg_hits.toFixed(1) })))))),
          workRecalled(stats.work));
      } catch (e) { if (e.status !== 401) out.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    // Not a saving: the session work behind the memories whose full body reached the model, which assumes the
    // model would otherwise have redone it.
    function workRecalled(w) {
      const missing = w.memories_without_work_estimate ? `, ${w.memories_without_work_estimate.toLocaleString()} of them without an estimate` : '';
      const ratio = w.ratio == null ? '' : ` (${w.ratio.toFixed(1)}×)`;
      return h('p', { text: `Work recalled: ~${w.work_tokens_recalled.toLocaleString()} tokens of session work behind ${w.memories_recalled.toLocaleString()} memories${missing}, against ${w.tokens_served.toLocaleString()} tokens served${ratio}. An upper bound: it assumes the model would otherwise have redone that work.` });
    }
    load();
    return h('div', { class: 'page-inner' },
      h('h2', { text: 'Tokens served' }),
      h('p', { class: 'muted', text: 'Estimated tokens per call. The full context is a budgeted block with an index of recent observations and sessions (about 400 to 550 tokens); the lean context is the profile, open work and a short rubric (about 250) and leaves the rest to the search tools; the smart context starts lean and adds at most 250 tokens of memories on the first prompt, only when the prompt matches some (first-prompt calls count only those that added something). Change the default in the header.' }),
      h('div', { class: 'toolbar' }, h('label', { class: 'field' }, h('span', { text: 'Window' }), days),
        all ? h('label', { class: 'field' }, h('span', { text: 'All users' }), all) : null),
      out);
  }

  // ---------- admin ----------
  async function renderAdmin() {
    const own = (state.token.split('_')[1] || '');
    const health = h('div', {}), tokens = h('div', {}), runs = h('div', {}), packets = h('div', {}), jobs = h('div', {});
    $main.replaceChildren(h('div', { class: 'page' }, h('div', { class: 'page-inner' },
      h('div', { class: 'head-row' }, h('h2', { text: 'Health' }), h('button', { class: 'btn small', onclick: () => loadDoctor() }, 'Check again')), health,
      h('h2', { text: 'Tokens' }), tokens,
      h('h2', { text: 'Consolidation runs' }),
      h('p', { class: 'muted', text: 'Planner runs, newest first, with what started them: the nightly pass, the end of a session, enough new observations in a scope, or an operator. Runs waiting in the queue come first.' }), runs,
      h('h2', { text: 'Failed work packets' }), packets, h('h2', { text: 'Failed background jobs' }), jobs)));

    // Server diagnosis (GET /v1/admin/doctor). detail and fix are server text: textContent only, the fix in <code>.
    async function loadDoctor() {
      health.replaceChildren(h('div', { class: 'muted', text: 'Checking…' }));
      try {
        const { checks } = await api('/v1/admin/doctor');
        const level = { ok: 'ok', warn: 'warn', fail: 'bad' };
        health.replaceChildren(h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['Check', 'Status', 'Detail'].map((c) => h('th', { text: c })))),
          h('tbody', {}, checks.map((c) => h('tr', {},
            h('td', { text: c.name }),
            h('td', {}, h('i', { class: `dot ${level[c.status] || 'bad'}` }), ` ${c.status}`),
            h('td', {}, c.detail, c.fix ? h('div', { class: 'muted' }, 'Fix: ', h('code', { text: c.fix })) : null)))))));
      } catch (e) { if (e.status !== 401) health.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    async function loadTokens() {
      try {
        const list = await api('/v1/admin/tokens');
        tokens.replaceChildren(h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['Token', 'Kind', 'Created', 'Last used', 'Expires', ''].map((c) => h('th', { text: c })))),
          h('tbody', {}, list.map((t) => h('tr', {},
            h('td', {}, h('code', { text: `mem_${t.prefix}_…` }), t.prefix === own ? h('span', { class: 'mini', text: ' this session' }) : null, t.revoked_at ? h('span', { class: 'mini', text: ' revoked' }) : null),
            h('td', { text: t.kind }), h('td', { title: fullTime(t.created_at), text: ago(t.created_at) }),
            h('td', { text: t.last_used_at ? ago(t.last_used_at) : 'never' }), h('td', { text: t.expires_at ? fullTime(t.expires_at) : 'never' }),
            h('td', {}, t.revoked_at ? null : h('button', { class: 'btn small danger', onclick: () => revoke(t, t.prefix === own) }, 'Revoke'))))))));
      } catch (e) { if (e.status !== 401) tokens.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    async function revoke(t, isOwn) {
      if (!confirm(isOwn ? 'This is the token you are signed in with. Revoke it and sign out?' : `Revoke token mem_${t.prefix}_…? The device using it loses access.`)) return;
      try { await api(`/v1/admin/tokens/${t.id}`, { method: 'DELETE' }); toast('Token revoked'); if (isOwn) signOut('You revoked your own token.'); else loadTokens(); } catch (e) { if (e.status !== 401) toast(e.message, true); }
    }
    // GET /v1/admin/consolidation (finished runs) and the queued `plan` jobs, each with its trigger.
    async function loadRuns() {
      try {
        const [done, queued] = await Promise.all([api('/v1/admin/consolidation?limit=20'), api('/v1/admin/jobs?type=plan&status=queued&limit=20')]);
        const counts = (o) => Object.entries(o || {}).map(([k, v]) => `${k} ${v}`).join(', ') || '–';
        const trigger = (t) => h('span', { class: 'pill', text: t.replace('_', ' ') });
        const rows = [
          ...queued.map((j) => h('tr', {}, h('td', { title: fullTime(j.run_after), text: `queued for ${fullTime(j.run_after)}` }), h('td', {}, trigger(j.trigger || 'manual')), h('td', { text: j.scope || '' }), h('td', { text: '–' }), h('td', { text: '–' }))),
          ...done.map((r) => h('tr', {}, h('td', { title: fullTime(r.created_at), text: ago(r.created_at) }), h('td', {}, trigger(r.trigger)), h('td', { text: r.scope || 'all scopes' }), h('td', { text: counts(r.packets) }), h('td', { text: counts(r.actions) }))),
        ];
        runs.replaceChildren(rows.length ? h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['When', 'Trigger', 'Scope', 'Packets', 'Actions'].map((c) => h('th', { text: c })))),
          h('tbody', {}, rows))) : h('div', { class: 'muted', text: 'None yet.' }));
      } catch (e) { if (e.status !== 401) runs.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    async function loadPackets() {
      try {
        const list = await api('/v1/admin/packets?status=failed&limit=20');
        packets.replaceChildren(list.length ? h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['Kind', 'Attempts', 'Created', 'Error', ''].map((c) => h('th', { text: c })))),
          h('tbody', {}, list.map((p) => h('tr', {}, h('td', { text: p.kind }), h('td', { text: `${p.attempts}/${p.max_attempts}` }), h('td', { text: ago(p.created_at) }),
            h('td', { text: p.last_error || '' }),
            h('td', {}, h('button', { class: 'btn small', onclick: async () => { try { await api(`/v1/admin/packets/${p.id}/retry`, { method: 'POST', body: {} }); toast('Queued again'); loadPackets(); } catch (e) { if (e.status !== 401) toast(e.message, true); } } }, 'Retry'))))))) : h('div', { class: 'muted', text: 'None. Extraction and consolidation are keeping up.' }));
      } catch (e) { if (e.status !== 401) packets.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    async function loadJobs() {
      try {
        const list = await api('/v1/admin/jobs?status=failed&limit=20');
        jobs.replaceChildren(list.length ? h('div', { class: 'scroll-x' }, h('table', { class: 't' },
          h('thead', {}, h('tr', {}, ['Type', 'Attempts', 'Created', 'Error'].map((c) => h('th', { text: c })))),
          h('tbody', {}, list.map((j) => h('tr', {}, h('td', { text: j.trigger ? `${j.type} (${j.trigger})` : j.type }), h('td', { text: j.attempts }), h('td', { text: ago(j.created_at) }), h('td', { text: j.last_error || '' })))))) : h('div', { class: 'muted', text: 'None.' }));
      } catch (e) { if (e.status !== 401) jobs.replaceChildren(h('div', { class: 'error', text: e.message })); }
    }
    loadDoctor(); loadTokens(); loadRuns(); loadPackets(); loadJobs();
  }

  // ---------- keyboard ----------
  document.addEventListener('keydown', (e) => {
    if (!state.token || state.tab !== 'memory') return;
    const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement && document.activeElement.tagName);
    if (e.key === '/' && !typing && !e.metaKey && !e.ctrlKey) { e.preventDefault(); document.getElementById('q')?.focus(); return; }
    if (e.key === 'Escape' && !typing && state.selected && !document.querySelector('dialog[open]')) { closeDetail(); return; }
    if ((e.key === 'j' || e.key === 'k') && !typing && !document.querySelector('dialog[open]')) {
      const rows = [...document.querySelectorAll('.row')];
      if (!rows.length) return;
      const i = rows.findIndex((r) => r.getAttribute('aria-selected') === 'true');
      const next = rows[Math.max(0, Math.min(rows.length - 1, i + (e.key === 'j' ? 1 : -1)))];
      next.click();
      next.scrollIntoView({ block: 'nearest' });
    }
  });

  // ---------- start ----------
  const saved = store.get('mv.token');
  if (saved) {
    state.token = saved;
    boot().catch(() => renderSignIn());
  } else renderSignIn();
})();
