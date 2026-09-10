//! The pages and the bridge (plugins spec §18.5), under the daemon's
//! mount: every link is built from `X-Balerix-Forwarded-Prefix`. The
//! index polls `agents.json`; the terminal page runs the vendored
//! xterm.js against `agents/{id}/ws`, which relays to the daemon's attach.

use std::sync::{Arc, LazyLock};

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use balerix_api::{PluginAction, ResizeFrame, TextFrame};
use balerix_plugin_sdk::SdkError;
use balerix_plugin_sdk::metrics::IntGauge;
use sha2::{Digest, Sha256};

use crate::plugin::Shared;
use crate::review::{MAX_MESSAGE_BYTES, ReviewBody, render_message, validate};
use crate::state::{AgentRow, now};

pub const PREFIX_HEADER: &str = "x-balerix-forwarded-prefix";

const XTERM_JS: &[u8] = include_bytes!("../assets/xterm.js");
const XTERM_CSS: &[u8] = include_bytes!("../assets/xterm.css");
const ADDON_FIT_JS: &[u8] = include_bytes!("../assets/addon-fit.js");
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// The first twelve hex digits of the sha256 over the three vendored
/// files, in every asset path: a vendor bump is a new URL, so `IMMUTABLE`
/// can never hand a browser that visited before a stale bundle.
pub static ASSET_DIGEST: LazyLock<String> = LazyLock::new(|| {
    let mut h = Sha256::new();
    h.update(XTERM_JS);
    h.update(XTERM_CSS);
    h.update(ADDON_FIT_JS);
    hex::encode(h.finalize())[..12].to_string()
});
/// How often the index re-fetches `agents.json`, in milliseconds.
const INDEX_POLL_MS: u32 = 2000;

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/agents.json", get(agents_json))
        .route("/agents/{fleet}/{crew}/{agent}", get(terminal))
        .route("/agents/{fleet}/{crew}/{agent}/ws", get(bridge_route))
        .route(
            "/agents/{fleet}/{crew}/{agent}/events.json",
            get(events_json),
        )
        .route(
            "/agents/{fleet}/{crew}/{agent}/review",
            get(review_page).post(post_review),
        )
        .route("/agents/{fleet}/{crew}/{agent}/diff.json", get(diff_json))
        .route("/agents/{fleet}/{crew}/{agent}/file", get(file_text))
        .route("/assets/{digest}/{file}", get(asset))
        .with_state(shared)
}

/// The mount the daemon put us under, without a trailing slash; empty
/// when called directly (tests, curl). The proxy overwrites the header
/// and direct access needs the plugin's bearer, so a value that is not
/// a path could only come from a caller who already holds the token —
/// it is still not put into a page.
fn prefix(headers: &HeaderMap) -> String {
    headers
        .get(PREFIX_HEADER)
        .and_then(|v| v.to_str().ok())
        // one leading slash, not two (or a backslash): `//host` is a
        // scheme-relative URL to a browser, and it would land in a `src`
        .filter(|s| s.starts_with('/') && !s[1..].starts_with(['/', '\\']))
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_default()
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn phase_label(row: &AgentRow) -> String {
    serde_json::to_value(row.phase)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The index: rows rendered server-side, then refreshed by a small script
/// that rebuilds them from `agents.json` with `textContent` — nothing a
/// message says can become markup.
pub fn index_html(prefix: &str, rows: &[AgentRow]) -> String {
    let mut body = String::new();
    for r in rows {
        body.push_str(&format!(
            "<tr><td><a href=\"{p}/agents/{id}\">{id}</a> <a class=\"review\" href=\"{p}/agents/{id}/review\">review</a></td><td>{phase}</td><td>{msg}</td></tr>\n",
            p = html_escape(prefix),
            id = html_escape(&r.id),
            phase = html_escape(&phase_label(r)),
            msg = html_escape(&r.message),
        ));
    }
    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>balerix</title>
<style>body{{font:14px system-ui,sans-serif;margin:2rem}}table{{border-collapse:collapse}}td,th{{padding:.3rem .8rem;text-align:left;border-bottom:1px solid #ddd}}</style>
</head><body>
<h1>balerix agents</h1>
<table><thead><tr><th>agent</th><th>phase</th><th>message</th></tr></thead>
<tbody id="rows">
{body}</tbody></table>
<p id="empty" hidden>no agents enabled for web</p>
<script>
const prefix = "{p}";
async function refresh() {{
  try {{
    const rows = await (await fetch("{p}/agents.json")).json();
    const tbody = document.getElementById("rows");
    tbody.replaceChildren();
    for (const r of rows) {{
      const tr = document.createElement("tr");
      const a = document.createElement("a");
      a.href = prefix + "/agents/" + r.id;
      a.textContent = r.id;
      const c1 = document.createElement("td"); c1.appendChild(a);
      const rv = document.createElement("a");
      rv.className = "review";
      rv.href = prefix + "/agents/" + r.id + "/review";
      rv.textContent = "review";
      c1.append(" ", rv);
      const c2 = document.createElement("td"); c2.textContent = r.phase;
      const c3 = document.createElement("td"); c3.textContent = r.message;
      tr.append(c1, c2, c3);
      tbody.appendChild(tr);
    }}
    document.getElementById("empty").hidden = rows.length > 0;
  }} catch (e) {{ console.warn("refresh failed", e); }}
}}
document.getElementById("empty").hidden = document.querySelectorAll("#rows tr").length > 0;
setInterval(refresh, {poll});
</script>
</body></html>
"##,
        p = html_escape(prefix),
        poll = INDEX_POLL_MS,
    )
}

/// The terminal page: xterm.js on a full-window div, the bridge socket,
/// resizes on fit and on window resize, a line when the socket closes.
/// A resize is only sent with both dimensions non-zero: the fit addon
/// reports zeroes for a hidden container, and the daemon ignores such a
/// frame anyway (plugins spec §18.8), so it is not worth sending.
pub fn terminal_html(prefix: &str, id: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{id}</title>
<link rel="stylesheet" href="{p}/assets/{d}/xterm.css">
<style>html,body{{height:100%;margin:0;background:#000}}#t{{height:100%}}</style>
</head><body><div id="t"></div>
<script src="{p}/assets/{d}/xterm.js"></script>
<script src="{p}/assets/{d}/addon-fit.js"></script>
<script>
const term = new Terminal({{ cursorBlink: true, fontSize: 14 }});
const fit = new FitAddon.FitAddon();
term.loadAddon(fit);
term.open(document.getElementById("t"));
fit.fit();
const scheme = location.protocol === "https:" ? "wss://" : "ws://";
const ws = new WebSocket(scheme + location.host + "{p}/agents/{id}/ws");
ws.binaryType = "arraybuffer";
const resize = () => {{
  if (ws.readyState === WebSocket.OPEN && term.cols > 0 && term.rows > 0) {{
    ws.send(JSON.stringify({{ resize: {{ cols: term.cols, rows: term.rows }} }}));
  }}
}};
ws.onopen = resize;
ws.onmessage = (e) => term.write(new Uint8Array(e.data));
ws.onclose = (e) => term.write("\r\n[disconnected" + (e.reason ? ": " + e.reason : "") + "]\r\n");
const enc = new TextEncoder();
term.onData((d) => {{ if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d)); }});
window.addEventListener("resize", () => {{ fit.fit(); resize(); }});
term.focus();
</script>
</body></html>
"#,
        p = html_escape(prefix),
        id = html_escape(id),
        d = ASSET_DIGEST.as_str(),
    )
}

/// A JSON string literal safe inside `<script>`: `serde_json` escapes
/// quotes, backslashes and control characters; `<`, `>` and `&` are
/// escaped here so no value can close the element.
fn js_string(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// The review page (Spec C §4.3): the diff with gutter comments on the
/// left, the activity column on the right, the draft in `localStorage`.
/// Everything the script renders goes through `textContent`; the id and
/// prefix reach the script as JSON string literals.
pub fn review_html(prefix: &str, id: &str) -> String {
    let p_json = js_string(prefix);
    let id_json = js_string(id);
    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>review {id}</title>
<style>
body{{font:13px system-ui,sans-serif;margin:0;display:flex;flex-direction:column;height:100vh}}
header{{padding:.5rem 1rem;border-bottom:1px solid #ddd;display:flex;gap:1rem;align-items:center}}
main{{flex:1;display:flex;min-height:0}}
#diff{{flex:1;overflow:auto;padding:0 1rem 7rem;position:relative}}
#side{{width:26rem;border-left:1px solid #ddd;display:flex;flex-direction:column;min-height:0}}
#side.collapsed{{width:2.2rem}}
#side.collapsed #events,#side.collapsed #side-title{{display:none}}
#side-head{{display:flex;align-items:center;padding:.3rem;border-bottom:1px solid #eee}}
#events{{flex:1;overflow:auto;font:12px ui-monospace,monospace;padding:.4rem}}
.ev{{padding:.2rem .3rem;border-bottom:1px solid #eee;cursor:pointer}}
.ev .t{{color:#888;margin-right:.4rem}}
.ev .n{{color:#57606a;margin-right:.4rem}}
.ev pre{{white-space:pre-wrap;margin:.2rem 0 0;color:#555;max-height:16em;overflow:auto}}
.ev.divider{{background:#fff6d5;font-weight:600}}
.file{{margin:1rem 0;border:1px solid #ddd;border-radius:4px}}
.file h3{{margin:0;padding:.4rem .6rem;background:#f6f8fa;font-size:13px;font-weight:600;display:flex;gap:.6rem;align-items:center}}
.badge{{font-weight:normal;color:#b35900}}
.file h3 a{{font-weight:normal;margin-left:auto}}
table.hunk{{border-collapse:collapse;width:100%;font:12px ui-monospace,monospace}}
table.hunk td{{padding:0 .4rem;white-space:pre;vertical-align:top}}
td.ln{{color:#999;text-align:right;width:3em;user-select:none;cursor:pointer}}
td.ln:hover{{background:#dbe9ff}}
tr.add td.code{{background:#e6ffec}} tr.del td.code{{background:#ffebe9}} tr.hdr td{{background:#f1f8ff;color:#57606a}}
tr.comment td{{background:#fff8c5;white-space:normal;padding:.4rem .6rem}}
tr.comment textarea{{width:100%;min-height:4em;box-sizing:border-box}}
#stale{{border:1px solid #f0c36d;background:#fff8e1;padding:.5rem 1rem;margin:1rem 0}}
footer{{position:fixed;bottom:0;left:0;right:0;border-top:1px solid #ddd;background:#fff;padding:.5rem 1rem;display:flex;gap:1rem;align-items:flex-start}}
footer textarea{{flex:1;min-height:3.5em}}
#banner{{margin-top:.3rem}}
#age{{color:#57606a;padding:0 .3rem;border-radius:3px;transition:background 1.2s}}
#age.flash{{background:#fff3b0;transition:none}}
</style></head>
<body>
<header><a href="{p}/">agents</a> <strong>{id}</strong> <a href="{p}/agents/{id}">terminal</a> <span id="meta"></span> <span id="age"></span></header>
<main>
<div id="diff"><p id="loading">loading diff...</p></div>
<aside id="side"><div id="side-head"><button id="collapse" title="collapse or expand the activity column">&#8677;</button><span id="side-title" style="margin-left:.5rem">activity &middot; <span id="phase"></span> <span id="unread"></span></span></div><div id="events"><p id="no-events">no events yet; the daemon delivers no history</p></div></aside>
</main>
<footer><textarea id="summary" placeholder="Overall summary (optional)"></textarea><div><div id="count">0 comments</div><button id="reload">Reload diff</button> <button id="send">Send review</button><div id="banner"></div></div></footer>
<script>
const prefix = {p_json};
const id = {id_json};
const key = "balerix-review/" + id;
let diff = null;
let pending = null;
let rendered = null;
let latestFp = null;
let pendingDiff = null;
let lastApplied = 0;
let renderedAt = null;
const APPLY_MIN_MS = 3000;
let draft = {{ comments: [], summary: "", collapsed: false }};
try {{ const s = localStorage.getItem(key); if (s) draft = Object.assign(draft, JSON.parse(s)); }} catch (e) {{}}
const transient = (k, v) => (k === "editing" || k === "typing") ? undefined : v;
function save() {{ try {{ localStorage.setItem(key, JSON.stringify(draft, transient)); }} catch (e) {{}} }}
function el(tag, cls, text) {{ const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }}
const anchorOf = (c) => c.path + " " + c.side + " " + c.line + " " + c.text;

function parsePatch(patch) {{
  const hunks = []; let h = null; let o = 0, n = 0;
  for (const raw of patch.split("\n")) {{
    const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (m) {{ o = +m[1]; n = +m[2]; h = {{ header: raw, lines: [] }}; hunks.push(h); continue; }}
    if (!h) continue;
    if (raw.startsWith("+")) h.lines.push({{ kind: "add", side: "new", line: n++, text: raw }});
    else if (raw.startsWith("-")) h.lines.push({{ kind: "del", side: "old", line: o++, text: raw }});
    else if (raw.startsWith(" ")) h.lines.push({{ kind: "ctx", side: "new", line: n++, oldLine: o++, text: raw }});
    else if (raw.startsWith("\\")) h.lines.push({{ kind: "meta", text: raw }});
  }}
  return hunks;
}}

// Spec D PD-4: a comment keeps its exact anchor; otherwise the lines of the
// same path and side with the same text are collected — exactly one hit
// re-anchors the comment to that line number, none or several make it
// stale. Returns the stale comments; re-anchored ones are updated in place.
function anchorComments(diff, comments) {{
  const exact = new Set(); const byText = new Map();
  for (const f of diff.files) for (const h of parsePatch(f.patch)) for (const l of h.lines) {{
    if (l.kind === "meta") continue;
    exact.add(f.path + " " + l.side + " " + l.line + " " + l.text);
    const k = f.path + " " + l.side + " " + l.text;
    const a = byText.get(k); if (a) a.push(l.line); else byText.set(k, [l.line]);
  }}
  const stale = [];
  for (const c of comments) {{
    if (exact.has(anchorOf(c))) continue;
    const hits = byText.get(c.path + " " + c.side + " " + c.text) || [];
    if (hits.length === 1) c.line = hits[0]; else stale.push(c);
  }}
  return stale;
}}

function commentRow(c, editing) {{
  const tr = el("tr", "comment"); const td = el("td"); td.colSpan = 3;
  if (editing) {{
    const ta = el("textarea"); ta.value = c.typing !== undefined ? c.typing : (c.body || "");
    ta.oninput = () => {{ c.typing = ta.value; }};
    const ok = el("button", "", "Save comment"); const no = el("button", "", "Cancel");
    ok.onclick = () => {{ if (!ta.value.trim()) return; c.body = ta.value; delete c.editing; delete c.typing; if (!draft.comments.includes(c)) draft.comments.push(c); pending = null; save(); render(); maybeApply(); }};
    no.onclick = () => {{ delete c.editing; delete c.typing; pending = null; render(); maybeApply(); }};
    td.append(ta, ok, " ", no);
    setTimeout(() => ta.focus(), 0);
  }} else {{
    const body = el("div", "", c.body); body.style.whiteSpace = "pre-wrap";
    const edit = el("button", "", "Edit"); const del = el("button", "", "Delete");
    edit.onclick = () => {{ c.editing = true; render(); }};
    del.onclick = () => {{ draft.comments = draft.comments.filter((x) => x !== c); save(); render(); }};
    td.append(body, edit, " ", del);
  }}
  tr.appendChild(td); return tr;
}}

function sameLine(c, f, l) {{ return c.path === f.path && c.side === l.side && c.line === l.line && c.text === l.text; }}
function boxOpen() {{ return pending !== null || draft.comments.some((c) => c.editing); }}

function renderFile(f) {{
  const box = el("div", "file"); box.dataset.path = f.path;
  const h3 = el("h3");
  h3.append(el("span", "", f.status), el("span", "", f.old_path ? f.old_path + " -> " + f.path : f.path));
  if (f.uncommitted) h3.appendChild(el("span", "badge", "uncommitted"));
  if (f.binary) h3.appendChild(el("span", "badge", "binary"));
  if (f.truncated) h3.appendChild(el("span", "badge", "patch truncated"));
  if (f.status !== "deleted") {{ const a = el("a", "", "view file"); a.href = prefix + "/agents/" + id + "/file?path=" + encodeURIComponent(f.path); a.target = "_blank"; h3.appendChild(a); }}
  box.appendChild(h3);
  if (f.binary) return box;
  const table = el("table", "hunk");
  for (const h of parsePatch(f.patch)) {{
    const hdr = el("tr", "hdr"); const td = el("td", "", h.header); td.colSpan = 3; hdr.appendChild(td); table.appendChild(hdr);
    for (const l of h.lines) {{
      const tr = el("tr", l.kind);
      const oldNo = l.kind === "del" ? l.line : l.kind === "ctx" ? l.oldLine : "";
      const newNo = l.kind === "add" || l.kind === "ctx" ? l.line : "";
      const c1 = el("td", "ln", String(oldNo)); const c2 = el("td", "ln", String(newNo)); const c3 = el("td", "code", l.text);
      if (l.kind !== "meta") {{
        const start = () => {{ pending = {{ path: f.path, side: l.side, line: l.line, text: l.text, body: "" }}; render(); }};
        c1.onclick = start; c2.onclick = start;
      }}
      tr.append(c1, c2, c3); table.appendChild(tr);
      if (l.kind === "meta") continue;
      for (const c of draft.comments.filter((c) => sameLine(c, f, l))) table.appendChild(commentRow(c, !!c.editing));
      if (pending && sameLine(pending, f, l)) table.appendChild(commentRow(pending, true));
    }}
  }}
  box.appendChild(table); return box;
}}

function render() {{
  const root = document.getElementById("diff"); root.replaceChildren();
  if (!diff) {{ root.appendChild(el("p", "", "loading diff...")); return; }}
  document.getElementById("meta").textContent = "against " + diff.base_ref + " at " + diff.head.slice(0, 7) + (diff.truncated ? " (file list truncated)" : "");
  const stale = anchorComments(diff, draft.comments);
  for (const c of stale) {{ delete c.editing; delete c.typing; }}
  save();
  if (stale.length) {{
    const box = el("div"); box.id = "stale"; box.appendChild(el("strong", "", "no longer in the diff (still sent):"));
    for (const c of stale) {{ const row = el("div", "", c.path + " line " + c.line + " (" + c.side + "): " + c.body + " "); const del = el("button", "", "Delete"); del.onclick = () => {{ draft.comments = draft.comments.filter((x) => x !== c); save(); render(); }}; row.appendChild(del); box.appendChild(row); }}
    root.appendChild(box);
  }}
  if (!diff.files.length) root.appendChild(el("p", "", "no changes against " + diff.base_ref));
  for (const f of diff.files) root.appendChild(renderFile(f));
  document.getElementById("count").textContent = draft.comments.length + " comment" + (draft.comments.length === 1 ? "" : "s");
  const summary = document.getElementById("summary");
  if (summary.value !== draft.summary) summary.value = draft.summary;
}}

// Keep the reader's place across a re-render: the file whose header was at
// or above the top of the viewport is scrolled back to the top when it is
// still there; otherwise the raw offset is restored.
function renderKeepingScroll() {{
  const root = document.getElementById("diff");
  const top = root.scrollTop;
  const files = Array.from(root.querySelectorAll(".file"));
  const above = files.filter((f) => f.offsetTop <= top);
  const topPath = above.length ? above[above.length - 1].dataset.path : null;
  render();
  const again = topPath === null ? null : Array.from(root.querySelectorAll(".file")).find((f) => f.dataset.path === topPath);
  root.scrollTop = again ? again.offsetTop : top;
}}

function fmtAge(ms) {{
  const s = Math.floor(ms / 1000);
  if (s < 1) return "just now";
  if (s < 60) return s + " s ago";
  return Math.floor(s / 60) + " min ago";
}}
function tickAge() {{
  const age = document.getElementById("age");
  age.textContent = renderedAt === null ? "" : "· updated " + fmtAge(Date.now() - renderedAt);
}}
function flashAge() {{
  const age = document.getElementById("age");
  age.classList.add("flash");
  setTimeout(() => age.classList.remove("flash"), 200);
}}

function applyDiff(d, fp) {{
  diff = d; rendered = fp; pendingDiff = null; pending = null;
  lastApplied = Date.now(); renderedAt = lastApplied;
  renderKeepingScroll(); tickAge(); flashAge();
}}
// Spec D PD-3: a waiting diff is applied when no comment box is open and
// at least APPLY_MIN_MS have passed since the last apply; the next poll or
// the next Save/Cancel tries again otherwise.
function maybeApply() {{
  if (pendingDiff && !boxOpen() && Date.now() - lastApplied >= APPLY_MIN_MS) applyDiff(pendingDiff.diff, pendingDiff.fp);
}}

async function fetchDiff() {{
  const r = await fetch(prefix + "/agents/" + id + "/diff.json");
  if (!r.ok) throw new Error(await r.text());
  return await r.json();
}}
// Manual "Reload diff": unconditional, and the way a daemon refusal reaches
// the banner.
async function loadDiff() {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  try {{ applyDiff(await fetchDiff(), latestFp); }}
  catch (e) {{ banner.style.color = "#b00"; banner.textContent = "diff: " + e.message; }}
}}

async function send() {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  const comments = draft.comments.map((c) => ({{ path: c.path, side: c.side, line: c.line, text: c.text, body: c.body }}));
  const body = {{ head: diff ? diff.head : "", base_ref: diff ? diff.base_ref : "", summary: draft.summary, comments }};
  try {{
    const r = await fetch(prefix + "/agents/" + id + "/review", {{ method: "POST", headers: {{ "content-type": "application/json" }}, body: JSON.stringify(body) }});
    if (r.ok) {{ draft.comments = []; draft.summary = ""; save(); render(); banner.style.color = "#080"; banner.textContent = "review sent"; }}
    else {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + (await r.text()); }}
  }} catch (e) {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + e; }}
}}

let lastSeq = 0, unread = 0, fetching = false;
const side = document.getElementById("side");
function renderEvent(e) {{
  const row = el("div", "ev" + (e.name === "review_sent" ? " divider" : ""));
  row.append(el("span", "t", new Date(e.at * 1000).toLocaleTimeString()), el("span", "n", e.name), el("span", "s", e.summary));
  const pre = el("pre", "", JSON.stringify(e.payload, null, 2) + (e.payload_truncated ? "\n(truncated)" : "")); pre.hidden = true;
  row.appendChild(pre); row.onclick = () => {{ pre.hidden = !pre.hidden; }};
  return row;
}}
// One poll drives both columns (Spec D PD-2): the events, and the
// worktree's version — a new fingerprint fetches the diff once and parks
// it until maybeApply lets it through.
async function pollEvents() {{
  try {{
    const r = await (await fetch(prefix + "/agents/" + id + "/events.json?after=" + lastSeq)).json();
    document.getElementById("phase").textContent = r.phase;
    const box = document.getElementById("events");
    const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
    for (const e of r.events) {{ box.appendChild(renderEvent(e)); lastSeq = e.seq; if (side.classList.contains("collapsed")) unread++; }}
    if (lastSeq > 0) document.getElementById("no-events").hidden = true;
    document.getElementById("unread").textContent = unread ? "(" + unread + " new)" : "";
    if (r.events.length && atBottom) box.scrollTop = box.scrollHeight;
    const w = r.workspace;
    if (w) {{
      latestFp = w.fingerprint;
      if (w.fingerprint !== rendered && !(pendingDiff && pendingDiff.fp === w.fingerprint) && !fetching) {{
        fetching = true;
        try {{
          pendingDiff = {{ diff: await fetchDiff(), fp: w.fingerprint }};
          const banner = document.getElementById("banner");
          if (banner.textContent.startsWith("diff: ")) banner.textContent = "";
        }}
        catch (e) {{
          if (diff === null) {{ const banner = document.getElementById("banner"); banner.style.color = "#b00"; banner.textContent = "diff: " + e.message; }}
          else console.warn("diff", e);
        }}
        finally {{ fetching = false; }}
      }}
    }} else if (diff === null && !fetching) {{
      // no version: surface the daemon's reason once
      fetching = true; try {{ await loadDiff(); }} finally {{ fetching = false; }}
    }}
    maybeApply();
  }} catch (e) {{ console.warn("events", e); }}
}}

function applyCollapse() {{ side.classList.toggle("collapsed", !!draft.collapsed); if (!draft.collapsed) {{ unread = 0; document.getElementById("unread").textContent = ""; }} }}
document.getElementById("collapse").onclick = () => {{ draft.collapsed = !draft.collapsed; save(); applyCollapse(); }};
document.getElementById("reload").onclick = loadDiff;
document.getElementById("send").onclick = send;
document.getElementById("summary").oninput = (ev) => {{ draft.summary = ev.target.value; save(); }};
applyCollapse();
render();
pollEvents();
setInterval(pollEvents, {poll});
setInterval(tickAge, 1000);
</script>
</body></html>
"##,
        p = html_escape(prefix),
        id = html_escape(id),
        poll = INDEX_POLL_MS,
    )
}

async fn index(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Html<String> {
    Html(index_html(&prefix(&headers), &shared.cache.rows()))
}

async fn agents_json(State(shared): State<Arc<Shared>>) -> Json<Vec<AgentRow>> {
    Json(shared.cache.rows())
}

/// A daemon refusal crosses to the browser with its status and text; a
/// transport failure is a 502.
fn sdk_error(e: SdkError) -> Response {
    match e {
        SdkError::Status { status, message } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        )
            .into_response(),
        other => (StatusCode::BAD_GATEWAY, other.to_string()).into_response(),
    }
}

/// The agent id for an enabled agent, else the 404 to answer.
///
/// `Response` is large (it carries an extensions map); every caller
/// returns the `Err` straight back out rather than matching on it, so
/// boxing would only add an allocation on the hot path for no benefit.
#[allow(clippy::result_large_err)]
fn enabled_id(
    shared: &Shared,
    (fleet, crew, agent): (String, String, String),
) -> Result<String, Response> {
    let id = format!("{fleet}/{crew}/{agent}");
    if shared.cache.is_enabled(&id) {
        Ok(id)
    } else {
        Err((StatusCode::NOT_FOUND, "no such agent").into_response())
    }
}

async fn terminal(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, String)>,
) -> Response {
    match enabled_id(&shared, path) {
        Ok(id) => Html(terminal_html(&prefix(&headers), &id)).into_response(),
        Err(r) => r,
    }
}

async fn review_page(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Path(path): Path<(String, String, String)>,
) -> Response {
    match enabled_id(&shared, path) {
        Ok(id) => Html(review_html(&prefix(&headers), &id)).into_response(),
        Err(r) => r,
    }
}

/// The submission (Spec C §4.4): validate, render one message, send it
/// as a `send_text` with submit, and append the `review_sent` divider.
/// A refused action is 502 with the daemon's text and leaves the draft
/// to the page.
async fn post_review(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    body: Result<Json<ReviewBody>, JsonRejection>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Json(review) = match body {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    if let Err(reason) = validate(&review) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    let message = render_message(&review);
    // `validate` bounds every field, but `path` and `text` multiply by the
    // comment count, so the rendered message needs its own cap.
    if message.len() > MAX_MESSAGE_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            format!("message: longer than {MAX_MESSAGE_BYTES} bytes"),
        )
            .into_response();
    }
    let n = review.comments.len();
    let action = PluginAction::SendText {
        text: message.clone(),
        submit: true,
    };
    match shared.host.action(&id, &action).await {
        Ok(()) => {
            shared.reviews_total.with_label_values(&["sent"]).inc();
            shared.review_comments_total.inc_by(n as u64);
            shared.cache.push_event(
                &id,
                now(),
                "review_sent",
                format!("review sent ({n} comment{})", if n == 1 { "" } else { "s" }),
                serde_json::json!({ "message": message, "comments": n }),
            );
            Json(serde_json::json!({})).into_response()
        }
        Err(e) => {
            shared.reviews_total.with_label_values(&["failed"]).inc();
            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
    }
}

async fn diff_json(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match shared.host.workspace_diff(&id).await {
        Ok(diff) => {
            shared.diff_refreshes_total.inc();
            Json(diff).into_response()
        }
        Err(e) => sdk_error(e),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileQuery {
    path: String,
}

async fn file_text(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    Query(q): Query<FileQuery>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match shared.host.workspace_file(&id, &q.path).await {
        Ok(Some(bytes)) => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "no such path").into_response(),
        Err(e) => sdk_error(e),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct AfterQuery {
    after: u64,
}

/// The activity column's poll: entries after `after`, the phase, and the
/// worktree's version (Spec D §3.1) — `null` when the daemon refuses or
/// fails, so the column keeps flowing.
async fn events_json(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    q: Result<Query<AfterQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let after = match q {
        Ok(Query(q)) => q.after,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let mut events = shared.cache.events_after(&id, after);
    match shared.host.workspace_version(&id).await {
        Ok(v) => events.workspace = Some(v),
        Err(e) => {
            tracing::debug!(agent = %id, "workspace version: {e}");
            shared.version_failures_total.inc();
        }
    }
    Json(events).into_response()
}

async fn bridge_route(
    State(shared): State<Arc<Shared>>,
    Path(path): Path<(String, String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let id = match enabled_id(&shared, path) {
        Ok(id) => id,
        Err(r) => return r,
    };
    ws.on_upgrade(move |socket| bridge(socket, shared, id))
}

async fn asset(Path((digest, file)): Path<(String, String)>) -> Response {
    if digest != *ASSET_DIGEST {
        return (StatusCode::NOT_FOUND, "no such asset bundle").into_response();
    }
    let (kind, bytes) = match file.as_str() {
        "xterm.js" => ("text/javascript; charset=utf-8", XTERM_JS),
        "xterm.css" => ("text/css; charset=utf-8", XTERM_CSS),
        "addon-fit.js" => ("text/javascript; charset=utf-8", ADDON_FIT_JS),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, IMMUTABLE),
        ],
        Bytes::from_static(bytes),
    )
        .into_response()
}

/// A close frame's reason fits in the control frame: 123 bytes at most,
/// cut at a character boundary (a longer one makes the browser fail the
/// connection with 1002 and show nothing).
fn close_reason(reason: &str) -> String {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: close_reason(reason).into(),
        })))
        .await;
}

/// Holds `terminals_open` up for as long as it lives, so a bridge future
/// dropped mid-way (server shutdown, an aborted task) still lets the
/// gauge fall.
struct OpenTerminal(IntGauge);

impl OpenTerminal {
    fn new(gauge: &IntGauge) -> Self {
        gauge.inc();
        Self(gauge.clone())
    }
}

impl Drop for OpenTerminal {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// A close code that may be sent in a close frame: the daemon's own
/// (1000, 1003, 1011) cross as they are; 1005, 1006 and 1015 are reserved
/// for a socket that ended without one and become 1011.
fn forwardable(code: u16) -> u16 {
    match code {
        1004..=1006 | 1015 => 1011,
        1000..=1013 | 3000..=4999 => code,
        _ => 1011,
    }
}

/// One browser tab ↔ one daemon attach: binary both ways, the browser's
/// resize text frames forwarded as resizes, everything else ignored.
/// Ends when either side closes; the daemon's close code and reason
/// reach the browser, so the page can say why.
pub async fn bridge(mut browser: WebSocket, shared: Arc<Shared>, agent: String) {
    let attach = match shared.host.attach(&agent).await {
        Ok(a) => a,
        Err(e) => {
            close(&mut browser, 1011, &format!("attach: {e}")).await;
            return;
        }
    };
    let (mut rd, mut wr) = attach.split();
    let _open = OpenTerminal::new(&shared.terminals_open);
    shared.terminals_total.inc();
    loop {
        tokio::select! {
            frame = rd.read() => match frame {
                Some(bytes) => {
                    if browser.send(Message::Binary(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                None => {
                    let (code, reason) = match rd.close_reason() {
                        Some(why) if !why.reason.is_empty() => (forwardable(why.code), why.reason.clone()),
                        Some(why) => (forwardable(why.code), "the terminal closed".to_string()),
                        None => (1000, "the terminal closed".to_string()),
                    };
                    close(&mut browser, code, &reason).await;
                    break;
                }
            },
            msg = browser.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if wr.write(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let TextFrame::Resize(f) = ResizeFrame::parse(text.as_str())
                        && wr.resize(f.resize.cols, f.resize.rows).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    wr.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use balerix_api::AgentPhase;

    #[test]
    fn pages_escape_what_they_render_and_link_through_the_prefix() {
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
        let rows = vec![AgentRow {
            id: "f/c/a".into(),
            phase: AgentPhase::Dead,
            message: "<script>alert(1)</script>".into(),
        }];
        let html = index_html("/v1/plugins/web", &rows);
        assert!(html.contains(r#"<a href="/v1/plugins/web/agents/f/c/a">f/c/a</a>"#));
        assert!(html.contains("<td>dead</td>"));
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>alert"));
        assert!(html.contains(r#"fetch("/v1/plugins/web/agents.json")"#));
        let html = index_html("", &[]);
        assert!(
            html.contains(r#"fetch("/agents.json")"#),
            "no prefix: relative to the root"
        );
        let page = terminal_html("/v1/plugins/web", "f/c/a");
        let d = ASSET_DIGEST.as_str();
        assert_eq!(d.len(), 12);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()), "{d}");
        assert!(page.contains(&format!(r#"src="/v1/plugins/web/assets/{d}/xterm.js""#)));
        assert!(page.contains(r#""/v1/plugins/web/agents/f/c/a/ws""#));
        let page = terminal_html("/p", "<x>");
        assert!(page.contains("&lt;x&gt;") && !page.contains("<x>"));
    }

    /// The proxy overwrites the header, and direct access needs the
    /// plugin's bearer; the check is defence in depth at the boundary.
    #[test]
    fn a_forwarded_prefix_is_a_path_or_nothing() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(PREFIX_HEADER, v.parse().unwrap());
            prefix(&h)
        };
        assert_eq!(with("/v1/plugins/web"), "/v1/plugins/web");
        assert_eq!(with("/v1/plugins/web/"), "/v1/plugins/web");
        assert_eq!(with("http://evil.example"), "", "not a path");
        assert_eq!(with("//evil.example"), "", "scheme-relative, not a path");
        assert_eq!(with("/\\evil.example"), "");
        assert_eq!(with("javascript:alert(1)"), "");
        assert_eq!(with(""), "");
        assert_eq!(prefix(&HeaderMap::new()), "");
    }

    #[test]
    fn the_open_terminals_gauge_falls_with_the_guard() {
        let gauge = balerix_plugin_sdk::metrics::IntGauge::new("open", "open").unwrap();
        let a = OpenTerminal::new(&gauge);
        let b = OpenTerminal::new(&gauge);
        assert_eq!(gauge.get(), 2);
        drop(a);
        assert_eq!(gauge.get(), 1);
        drop(b);
        assert_eq!(gauge.get(), 0);
    }

    #[test]
    fn a_close_reason_fits_the_control_frame() {
        assert_eq!(close_reason("short"), "short");
        let long = "é".repeat(100); // 200 bytes
        let cut = close_reason(&long);
        assert!(cut.len() <= 123 && cut.chars().all(|c| c == 'é'), "{cut:?}");
        assert_eq!(cut.len(), 122, "cut at a character boundary");
    }

    #[test]
    fn only_sendable_close_codes_are_forwarded() {
        assert_eq!(forwardable(1000), 1000);
        assert_eq!(forwardable(1003), 1003);
        assert_eq!(forwardable(1011), 1011);
        assert_eq!(forwardable(4000), 4000);
        // reserved: never on the wire
        assert_eq!(forwardable(1004), 1011);
        assert_eq!(forwardable(1005), 1011);
        assert_eq!(forwardable(1006), 1011);
        assert_eq!(forwardable(1015), 1011);
        assert_eq!(forwardable(2000), 1011);
    }

    #[test]
    fn the_review_page_links_its_routes_through_the_prefix_and_escapes_the_id() {
        let page = review_html("/v1/plugins/web", "f/c/a");
        assert!(page.contains(r#"const prefix = "/v1/plugins/web""#));
        assert!(page.contains(r#"const id = "f/c/a""#));
        assert!(page.contains("/diff.json"), "fetches the diff");
        assert!(page.contains("/events.json?after="), "polls the column");
        assert!(
            page.contains(r#"href="/v1/plugins/web/agents/f/c/a""#),
            "back to the terminal"
        );
        assert!(page.contains(r#"id="collapse""#));
        assert!(page.contains("balerix-review/"), "the draft key");
        assert!(
            page.contains("function anchorComments("),
            "re-anchoring is a standalone function"
        );
        assert!(page.contains(r#"id="age""#), "the indicator");
        assert!(
            page.contains("const APPLY_MIN_MS = 3000;"),
            "the apply rate limit"
        );
        assert!(page.contains("r.workspace"), "the poll reads the version");
        assert!(
            !page.contains("loadDiff();\npollEvents();"),
            "the first diff comes from the first poll"
        );
        let page = review_html("/p", "<x>&");
        assert!(page.contains("&lt;x&gt;&amp;") && !page.contains("<x>"));
        assert!(
            page.contains("const id = \"\\u003cx\\u003e\\u0026\""),
            "the id reaches the script as a JSON literal with <, > and & escaped: {page}"
        );
        let rows = vec![AgentRow {
            id: "f/c/a".into(),
            phase: AgentPhase::Ready,
            message: String::new(),
        }];
        let index = index_html("/v1/plugins/web", &rows);
        assert!(
            index.contains(
                r#"<a class="review" href="/v1/plugins/web/agents/f/c/a/review">review</a>"#
            ),
            "{index}"
        );
        assert!(
            index.contains(r#"prefix + "/agents/" + r.id + "/review""#),
            "the refresh script too"
        );
    }
}
