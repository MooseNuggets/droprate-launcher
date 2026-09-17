/* DropRate launcher UI.
 *
 * Deliberately thin. Every decision that matters — whether a game may launch,
 * where an archive may write, what the device token is — lives in Rust. This
 * file renders what it is told and forwards clicks. It has never seen the device
 * token and cannot ask for it.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const view = $("view");

const esc = (s) =>
  String(s ?? "").replace(/[<>&"']/g, (c) =>
    ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&#39;" }[c]));

const shortWallet = (w) =>
  !w ? "" : w.length > 12 ? `${w.slice(0, 4)}…${w.slice(-4)}` : w;

const bytes = (n) => {
  if (!n) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0, v = Number(n);
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
};

const money = (cents) => (cents == null ? "—" : `$${(cents / 100).toFixed(2)}`);

let toastTimer = null;
function toast(message, isError = false) {
  document.querySelector(".toast")?.remove();
  const el = document.createElement("div");
  el.className = "toast" + (isError ? " err" : "");
  el.textContent = message;
  document.body.appendChild(el);
  clearTimeout(toastTimer);
  // Errors from the server are written for people and are worth reading twice.
  toastTimer = setTimeout(() => el.remove(), isError ? 7000 : 3200);
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

let games = [];                       // last rendered library
let storeGames = [];                  // last rendered catalog
let VIEW = "library";                 // "library" | "store"
let pollTimer = null;
const installing = new Map();         // product_id -> {phase, percent, received, total}
const awaiting = new Set();           // ids whose purchase we're watching land

// ---------------------------------------------------------------------------
// pairing
// ---------------------------------------------------------------------------

async function showPairing() {
  stopPolling();
  view.innerHTML = `
    <div class="centre"><div class="card">
      <h1>Pair this computer</h1>
      <p>Approve it from your browser, where your wallet already lives. Your keys never come near this app.</p>
      <div class="code" id="code">····&nbsp;····</div>
      <p class="hint" id="hint">Getting a code…</p>
      <ul class="grants">
        <li><span class="y">✓</span><span>See the games you own and install them</span></li>
        <li><span class="n">✕</span><span>Spend, sell or transfer anything</span></li>
        <li><span class="n">✕</span><span>Touch your wallet's keys</span></li>
      </ul>
      <button class="btn" id="open" disabled>Open the pairing page</button>
    </div></div>`;

  let started;
  try {
    started = await invoke("pair_start");
  } catch (e) {
    $("hint").textContent = String(e);
    $("hint").style.color = "var(--bad)";
    return;
  }

  $("code").textContent = started.code;
  // Name the exact page rather than just the site. If the button below fails to
  // reach a browser for any reason, this line is the whole fallback — it has to
  // be enough on its own to finish pairing by hand.
  $("hint").textContent = "Enter this code at droprate.xyz/pair.html — or click below.";
  const open = $("open");
  open.disabled = false;
  open.addEventListener("click", async () => {
    try {
      // Takes no URL — Rust opens the pairing page it was issued.
      await invoke("open_pair_page");
      toast("Opened in your browser — approve it there.");
    } catch {
      toast("Couldn't open your browser. Go to droprate.xyz/pair.html and enter the code.", true);
    }
  });

  startPolling();
}

function startPolling() {
  stopPolling();
  // 2s: fast enough to feel instant after approval, slow enough not to hammer
  // a serverless endpoint while someone finds their wallet.
  pollTimer = setInterval(async () => {
    let res;
    try {
      res = await invoke("pair_poll");
    } catch {
      return; // transient; keep waiting
    }
    if (res.state === "paired") {
      stopPolling();
      toast("Paired. Loading your library…");
      await boot();
    } else if (["expired", "revoked"].includes(res.state)) {
      stopPolling();
      const hint = $("hint");
      if (hint) {
        hint.textContent = "That code expired. Restart the app for a new one.";
        hint.style.color = "var(--bad)";
      }
    }
  }, 2000);
}

function stopPolling() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
}

// ---------------------------------------------------------------------------
// library
// ---------------------------------------------------------------------------

function cardFor(g) {
  const prog = installing.get(g.product_id);
  const initials = g.title.split(/\s+/).map((w) => w[0]).join("").slice(0, 2).toUpperCase();

  const cover = g.image
    ? `<img src="${esc(g.image)}" alt="" onerror="this.remove()">`
    : `<span class="ini">${esc(initials)}</span>`;

  let actions;
  if (prog) {
    const known = prog.percent >= 0;
    actions = `
      <div style="width:100%">
        <div class="bar ${known ? "" : "indeterminate"}"><i style="width:${known ? prog.percent.toFixed(1) : 0}%"></i></div>
        <div class="phase">${esc(labelFor(prog))}</div>
      </div>`;
  } else if (!g.owned) {
    actions = `<button class="ghost" data-act="uninstall" data-id="${g.product_id}" style="flex:1">Delete files</button>`;
  } else if (!g.installed) {
    actions = g.installable
      ? `<button class="btn" data-act="install" data-id="${g.product_id}">Install</button>`
      : `<button class="ghost" disabled style="flex:1">No build for this PC</button>`;
  } else {
    const play = g.playable
      ? `<button class="btn" data-act="play" data-id="${g.product_id}">Play</button>`
      : `<button class="btn" disabled>Play</button>`;
    const update = g.update_available
      ? `<button class="ghost" data-act="install" data-id="${g.product_id}">Update</button>`
      : `<button class="ghost" data-act="folder" data-id="${g.product_id}">Folder</button>`;
    actions = play + update;
  }

  const sub = [
    g.copy_number ? `Copy #${g.copy_number}` : null,
    g.installed ? `v${g.installed_version}` : null,
  ].filter(Boolean).join(" · ");

  return `
    <div class="game ${g.owned ? "" : "sold"}">
      <div class="cover">${cover}</div>
      <div class="body">
        <div class="title">${esc(g.title)}</div>
        ${sub ? `<div class="sub">${esc(sub)}</div>` : ""}
        ${g.blocked_reason && !prog ? `<div class="reason">${esc(g.blocked_reason)}</div>` : ""}
        <div class="acts">${actions}</div>
      </div>
    </div>`;
}

function labelFor(p) {
  if (p.phase === "downloading") {
    return p.total > 0
      ? `${bytes(p.received)} of ${bytes(p.total)}`
      : `${bytes(p.received)} downloaded`;
  }
  if (p.phase === "verifying") return "Checking the download…";
  if (p.phase === "extracting") return "Installing…";
  return "Done";
}

function renderLibrary() {
  if (!games.length) {
    view.innerHTML = `
      <div class="empty">
        <h2>No games yet</h2>
        <p>Anything you buy on droprate.xyz shows up here.</p>
      </div>`;
    return;
  }
  view.innerHTML = `<div class="grid">${games.map(cardFor).join("")}</div>`;
}

// ---------------------------------------------------------------------------
// store — the website's storefront, condensed into the app
// ---------------------------------------------------------------------------

let sFilter = "all";
let sQuery = "";

const S_FILTERS = [
  ["all", "All"], ["web", "Plays in browser"], ["native", "Launcher"],
  ["finite", "Limited run"], ["infinite", "Open edition"],
];
const isLimited = (g) => /left|of\b|limited/i.test(g.supply || "") && !/open/i.test(g.supply || "");
function sMatch(g) {
  if (sFilter === "web" && g.runtime !== "web") return false;
  if (sFilter === "native" && g.runtime === "web") return false;
  if (sFilter === "finite" && !isLimited(g)) return false;
  if (sFilter === "infinite" && isLimited(g)) return false;
  if (sQuery) {
    const hay = [g.title, g.tagline, g.studio, (g.genres || []).join(" ")].join(" ").toLowerCase();
    if (!sQuery.toLowerCase().split(/\s+/).filter(Boolean).every((t) => hay.includes(t))) return false;
  }
  return true;
}
const initialsOf = (t) => (t || "?").split(/\s+/).map((w) => w[0] || "").join("").slice(0, 3).toUpperCase();

function storeAction(g) {
  if (awaiting.has(g.product_id)) return `<div class="waiting">Waiting for your purchase…</div>`;
  if (g.owned) return `<button class="ghost" data-act="tolibrary" data-id="${g.product_id}" style="flex:1">In your library</button>`;
  if (g.sold_out) return `<button class="ghost" disabled style="flex:1">Sold out</button>`;
  return `<button class="btn" data-act="buy" data-id="${g.product_id}">Buy — ${money(g.price_cents)}</button>`;
}

function storeCardFor(g) {
  const cover = g.image
    ? `<img src="${esc(g.image)}" alt="" onerror="this.remove()">`
    : `<span class="ini">${esc(initialsOf(g.title))}</span>`;
  const rt = g.runtime === "web" ? "Plays in browser" : "Launcher";
  const own = g.owned
    ? `<span class="badge own-badge have">&#10003; Owned</span>`
    : `<span class="badge own-badge">&#128273; Own it</span>`;
  const ed = g.sold_out
    ? `<span class="ed gone">Sold out</span>`
    : isLimited(g)
      ? `<span class="ed lim">${esc(g.supply)}</span>`
      : `<span class="ed open">Open edition</span>`;
  return `
    <div class="scard">
      <div class="cover">${cover}<span class="badge rt-badge">${rt}</span>${own}${ed}</div>
      <div class="cbody">
        <div class="ctitle">${esc(g.title)}</div>
        ${g.tagline ? `<div class="ctag">${esc(g.tagline)}</div>` : ""}
        <div class="cmeta">
          <span class="cprice gold-text">${money(g.price_cents)}</span>
          ${g.studio ? `<span class="cstudio">${esc(g.studio)}${g.trusted ? ' <span class="tick">&#10003;</span>' : ""}</span>` : ""}
        </div>
        <div class="acts">${storeAction(g)}</div>
      </div>
    </div>`;
}

function spotCardFor(g) {
  const rt = g.runtime === "web" ? "Plays in your browser" : "Runs in this app";
  const meta = isLimited(g)
    ? `<span class="spot-meta"><b>${esc(g.supply)}</b> · ${rt}</span>`
    : `<span class="spot-meta">Open edition · <b>always available</b> · ${rt}</span>`;
  const cta = g.owned ? "In your library" : g.sold_out ? "Sold out" : "Buy on droprate.xyz →";
  const act = g.owned ? "tolibrary" : g.sold_out ? "" : "buy";
  return `
    <div class="spot" ${act ? `data-act="${act}" data-id="${g.product_id}"` : ""}>
      <div class="spot-bg">${g.image ? `<img src="${esc(g.image)}" alt="">` : ""}</div>
      <div class="spot-rib">FEATURED</div>
      ${g.image ? "" : `<div class="spot-mono">${esc(initialsOf(g.title))}</div>`}
      <div class="spot-in">
        <div class="spot-tag">&#128273; Yours to keep</div>
        <h3>${esc(g.title)}</h3>
        ${g.tagline ? `<div class="tl">${esc(g.tagline)}</div>` : ""}
        <div class="spot-row"><span class="spot-price gold-text">${money(g.price_cents)}</span>${meta}</div>
        <div class="spot-row"><span class="spot-cta">${cta}</span></div>
      </div>
    </div>`;
}

function storeShell() {
  return `
    <div class="swrap">
      <section class="shero">
        <div>
          <div class="kick"><span class="dot"></span>The DropRate game store</div>
          <h1>You bought it.<br><span class="gold-text">You own it.</span></h1>
          <p class="sub">A game you buy here is <b>actually yours</b> — not a rental that disappears if an account gets closed. Play it, and when you're done, <b>sell it like a disc</b>. Checkout opens in your browser so your wallet keys never touch this app.</p>
          <div class="trust">
            <div><div class="t">Yours to keep</div><div class="l">Not a rental</div></div>
            <div><div class="t">Devs keep 95%</div><div class="l">Flat 5% fee</div></div>
            <div><div class="t">One click to play</div><div class="l">Installs itself</div></div>
          </div>
        </div>
        <div id="spot"></div>
      </section>
      <div class="sec-label"><h2>Now on the shelf</h2><span class="ln"></span><span class="note" id="scount"></span></div>
      <div class="tools">
        <div class="search"><span class="mag">&#8981;</span><input id="sq" type="search" placeholder="Search games, studios, genres…" autocomplete="off" spellcheck="false" value="${esc(sQuery)}"></div>
      </div>
      <div class="filters" id="sfilters"></div>
      <div class="sgrid" id="sgrid"></div>
    </div>`;
}

function renderStore() {
  view.classList.add("storefront");
  if (!storeGames.length) {
    view.innerHTML = `
      <div class="empty">
        <h2>Nothing on the shelf yet</h2>
        <p>Games show up here as developers publish them.</p>
      </div>`;
    return;
  }
  // Build the shell once; re-render only the moving parts so typing in the
  // search box doesn't lose focus and the spotlight image doesn't reload.
  if (!view.querySelector("#sgrid")) {
    view.innerHTML = storeShell();
    view.querySelector("#sq").addEventListener("input", (e) => { sQuery = e.target.value.trim(); paintStoreGrid(); });
    view.querySelector("#sfilters").addEventListener("click", (e) => {
      const b = e.target.closest("[data-f]"); if (!b) return;
      sFilter = b.dataset.f; paintStoreGrid();
    });
  }
  const spot = view.querySelector("#spot");
  const feat = storeGames.find((g) => !g.sold_out) || storeGames[0];
  spot.innerHTML = storeGames.length > 1 ? spotCardFor(feat) : "";
  paintStoreGrid();
}

function paintStoreGrid() {
  const counts = Object.fromEntries(S_FILTERS.map(([k]) => [k, 0]));
  for (const g of storeGames) for (const [k] of S_FILTERS) { const save = sFilter; sFilter = k; const q = sQuery; sQuery = ""; if (sMatch(g)) counts[k]++; sFilter = save; sQuery = q; }
  view.querySelector("#sfilters").innerHTML = S_FILTERS
    .filter(([k]) => k === "all" || counts[k] > 0)
    .map(([k, l]) => `<button class="chipf ${k === sFilter ? "on" : ""}" data-f="${k}">${l} <span class="n">${counts[k]}</span></button>`).join("");
  const list = storeGames.filter(sMatch);
  const filtered = sFilter !== "all" || !!sQuery;
  view.querySelector("#scount").textContent = `${list.length} title${list.length === 1 ? "" : "s"}${filtered ? " · filtered" : ""}`;
  view.querySelector("#sgrid").innerHTML = list.length
    ? list.map(storeCardFor).join("")
    : `<div class="empty" style="grid-column:1/-1"><h2>Nothing matches</h2><p>Try another filter or search.</p></div>`;
}

async function showStore() {
  if (!storeGames.length) {
    view.innerHTML = `<div class="empty"><h2>Loading the store…</h2></div>`;
  }
  try {
    storeGames = await invoke("browse_store");
  } catch (err) {
    view.innerHTML = `
      <div class="empty">
        <h2>Couldn't reach the store</h2>
        <p>${esc(String(err))}</p>
      </div>`;
    return;
  }
  if (VIEW === "store") renderStore();
}

/* Buying happens in the browser, so nothing tells us when it finishes. Poll the
   catalog for a few minutes and pick the copy up the moment it lands, rather
   than making someone hunt for a refresh button after paying. */
async function watchForPurchase(productId, title) {
  awaiting.add(productId);
  if (VIEW === "store") renderStore();

  for (let i = 0; i < 30 && awaiting.has(productId); i++) {
    await new Promise((r) => setTimeout(r, 6000));
    if (!awaiting.has(productId)) return;

    let fresh;
    try { fresh = await invoke("browse_store"); } catch { continue; }
    storeGames = fresh;

    const g = fresh.find((x) => x.product_id === productId);
    if (g && g.owned) {
      awaiting.delete(productId);
      toast(`${title} is yours — it's in your library.`);
      try { games = await invoke("refresh_library"); } catch { /* keep what we have */ }
      if (VIEW === "store") renderStore(); else renderLibrary();
      return;
    }
    if (VIEW === "store") renderStore();
  }

  // Gave up watching. Not an error — they may simply not have finished.
  awaiting.delete(productId);
  if (VIEW === "store") renderStore();
}

function setView(next) {
  VIEW = next;
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("is-on", t.dataset.view === next);
  }
  if (next === "store") { view.innerHTML = ""; showStore(); }
  else { view.classList.remove("storefront"); renderLibrary(); }
}

// Re-render only the card that changed. Rebuilding the whole grid on every
// progress tick restarts image loads and makes a long download flicker.
function repaintCard(productId) {
  const g = games.find((x) => x.product_id === productId);
  if (!g) return;
  const cards = view.querySelectorAll(".game");
  const idx = games.indexOf(g);
  const el = cards[idx];
  if (!el) return renderLibrary();
  const tmp = document.createElement("div");
  tmp.innerHTML = cardFor(g);
  el.replaceWith(tmp.firstElementChild);
}

view.addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-act]");
  if (!btn) return;
  const id = Number(btn.dataset.id);

  // ---- store actions ----
  if (btn.dataset.act === "buy") {
    const g = storeGames.find((x) => x.product_id === id);
    if (!g) return;
    try {
      await invoke("open_store_page", { productId: id });
      toast("Opened in your browser — finish the purchase there.");
      watchForPurchase(id, g.title);
    } catch (err) { toast(String(err), true); }
    return;
  }
  if (btn.dataset.act === "tolibrary") return setView("library");

  // ---- library actions ----
  const game = games.find((g) => g.product_id === id);
  if (!game) return;

  if (btn.dataset.act === "install") return doInstall(game);
  if (btn.dataset.act === "play") return doPlay(game);
  if (btn.dataset.act === "folder") {
    try { await invoke("open_install_folder", { productId: id }); }
    catch (err) { toast(String(err), true); }
    return;
  }
  if (btn.dataset.act === "uninstall") {
    try {
      await invoke("uninstall_game", { productId: id });
      toast(`Removed ${game.title} from this PC.`);
      await refresh();
    } catch (err) { toast(String(err), true); }
  }
});

async function doInstall(game) {
  installing.set(game.product_id, { phase: "downloading", received: 0, total: 0, percent: -1 });
  repaintCard(game.product_id);
  try {
    await invoke("install_game", { productId: game.product_id, title: game.title });
    toast(`${game.title} is ready.`);
  } catch (err) {
    toast(String(err), true);
  } finally {
    installing.delete(game.product_id);
    await refresh();
  }
}

async function doPlay(game) {
  try {
    await invoke("launch_game", { productId: game.product_id });
    toast(`Launching ${game.title}…`);
  } catch (err) {
    // This is where the resale gate surfaces. The message comes from Rust and
    // is already written for a person, so show it as-is.
    toast(String(err), true);
    await refresh();
  }
}

listen("install-progress", (event) => {
  const p = event.payload;
  if (p.phase === "done") return;             // the install call resolves right after
  installing.set(p.product_id, p);
  repaintCard(p.product_id);
});

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

async function refresh(showErrors = true) {
  try {
    games = await invoke("refresh_library");
  } catch (err) {
    // Offline: fall back to what's on disk so installed games still play.
    const status = await invoke("get_status");
    games = status.games;
    if (showErrors) toast(`${err}`, true);
  }
  if (VIEW === "library") renderLibrary();
}

async function boot() {
  const status = await invoke("get_status");

  const paired = status.paired;
  $("who").hidden = !paired;
  $("tabs").hidden = !paired;
  $("refresh").hidden = !paired;
  $("devportal").hidden = !paired;
  $("signout").hidden = !paired;

  if (!paired) return showPairing();

  $("who").textContent = shortWallet(status.wallet);
  games = status.games;               // paint from disk immediately
  setView(VIEW);
  await refresh(false);               // then reconcile with the server
}

$("tabs").addEventListener("click", (e) => {
  const b = e.target.closest("[data-view]");
  if (b) setView(b.dataset.view);
});

$("refresh").addEventListener("click", async () => {
  const b = $("refresh");
  b.disabled = true;
  if (VIEW === "store") { storeGames = []; view.innerHTML = ""; await showStore(); }
  else await refresh();
  b.disabled = false;
});

/* Studios manage their games in one place — the web portal. Same login, same
   dashboard as the website, so there is nothing to keep in sync here. */
$("devportal").addEventListener("click", async () => {
  try { await invoke("open_dev_portal"); toast("Opened the Developer Portal in your browser."); }
  catch (err) { toast(String(err), true); }
});

$("signout").addEventListener("click", async () => {
  try {
    await invoke("sign_out");
    games = [];
    toast("Signed out. Your installed files were left alone.");
    await boot();
  } catch (err) { toast(String(err), true); }
});

boot();
