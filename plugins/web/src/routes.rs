//! The pages and the bridge (plugins spec §18.5), under the daemon's
//! mount: every link is built from `X-Balerix-Forwarded-Prefix`. The
//! index polls `agents.json`; the terminal page runs the vendored
//! xterm.js against `agents/{id}/ws`, which relays to the daemon's attach.

use std::collections::HashMap;
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
use balerix_plugin_sdk::metrics::{IntCounter, IntGauge};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

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
.ev .x{{white-space:pre-wrap;margin:.2rem 0 0;font:12px system-ui,sans-serif;max-height:24em;overflow:auto}}
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
.file p.notice{{margin:0;padding:.4rem .6rem;color:#57606a;white-space:pre-wrap}}
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

// A file's patch as hunks of numbered lines. A `diff --git ` line ends the
// hunk before it: a typechange is two sections under one header, and the
// second's `--- /dev/null` and `+++ b/x` are not lines of the first (#226).
// A hunk's line counts say where it ends, so an empty line inside one is a
// blank context line whose space git dropped (`diff.suppressBlankEmpty`).
function parsePatch(patch) {{
  const hunks = []; let h = null; let o = 0, n = 0, oLeft = 0, nLeft = 0;
  const rows = patch.split("\n");
  // what follows the last newline is not a line, even inside a hunk a
  // truncated patch cut short
  if (patch.endsWith("\n")) rows.pop();
  for (const raw of rows) {{
    if (raw.startsWith("diff --git ")) {{ h = null; continue; }}
    const m = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(raw);
    if (m) {{
      o = +m[1]; n = +m[3]; oLeft = m[2] === undefined ? 1 : +m[2]; nLeft = m[4] === undefined ? 1 : +m[4];
      h = {{ header: raw, lines: [] }}; hunks.push(h); continue;
    }}
    if (!h) continue;
    if (raw.startsWith("+")) {{ h.lines.push({{ kind: "add", side: "new", line: n++, text: raw }}); nLeft--; }}
    else if (raw.startsWith("-")) {{ h.lines.push({{ kind: "del", side: "old", line: o++, text: raw }}); oLeft--; }}
    else if (raw.startsWith(" ") || (raw === "" && oLeft > 0 && nLeft > 0)) {{
      h.lines.push({{ kind: "ctx", side: "new", line: n++, oldLine: o++, text: raw || " " }}); oLeft--; nLeft--;
    }}
    else if (raw.startsWith("\\")) h.lines.push({{ kind: "meta", text: raw }});
  }}
  return hunks;
}}

// Spec D PD-4: a comment keeps its exact anchor; otherwise the lines of the
// same path and side with the same text are collected — exactly one hit
// re-anchors the comment to that line number, none or several make it
// stale. Returns the stale comments; re-anchored ones are updated in place.
function anchorComments(workspaceDiff, comments) {{
  const exact = new Set(); const byText = new Map();
  for (const f of workspaceDiff.files) for (const h of parsePatch(f.patch)) for (const l of h.lines) {{
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

// A patch with no hunk shows what it says instead. Only the daemon's own
// notice for a path that is not a regular file (#220) has nothing to view,
// so only it loses the "view file" link; an empty patch (an empty file, or
// one line past the patch cap) and git's hunkless one (a mode change, a pure
// rename: its extended header lines) keep it (#226).
const SKIPPED_NOT_A_FILE = "not a regular file, skipped\n";
function renderFile(f) {{
  const box = el("div", "file"); box.dataset.path = f.path;
  const h3 = el("h3");
  h3.append(el("span", "", f.status), el("span", "", f.old_path ? f.old_path + " -> " + f.path : f.path));
  if (f.uncommitted) h3.appendChild(el("span", "badge", "uncommitted"));
  if (f.binary) h3.appendChild(el("span", "badge", "binary"));
  if (f.truncated) h3.appendChild(el("span", "badge", "patch truncated"));
  const hunks = f.binary ? [] : parsePatch(f.patch);
  if (f.status !== "deleted" && f.patch !== SKIPPED_NOT_A_FILE) {{ const a = el("a", "", "view file"); a.href = prefix + "/agents/" + id + "/file?path=" + encodeURIComponent(f.path); a.target = "_blank"; h3.appendChild(a); }}
  box.appendChild(h3);
  if (f.binary) return box;
  if (!hunks.length) {{
    const lines = f.patch.split("\n").filter((l) => l && !/^(diff --git |index |--- |\+\+\+ )/.test(l));
    if (lines.length) box.appendChild(el("p", "notice", lines.join("\n")));
    return box;
  }}
  const table = el("table", "hunk");
  for (const h of hunks) {{
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
// or above the top of the viewport is put back where it was, the same
// distance into it (top - its old offsetTop), when it is still there;
// otherwise the raw offset is restored.
function renderKeepingScroll() {{
  const root = document.getElementById("diff");
  const top = root.scrollTop;
  const files = Array.from(root.querySelectorAll(".file"));
  const above = files.filter((f) => f.offsetTop <= top);
  const anchor = above.length ? above[above.length - 1] : null;
  const topPath = anchor ? anchor.dataset.path : null;
  const into = anchor ? top - anchor.offsetTop : 0;
  render();
  const again = topPath === null ? null : Array.from(root.querySelectorAll(".file")).find((f) => f.dataset.path === topPath);
  root.scrollTop = again ? again.offsetTop + into : top;
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
// The worktree's fingerprint now, for a diff about to be fetched: read
// before the fetch, so a change after it makes the next poll fetch again
// rather than be missed. `after` is past every sequence number, so the
// column's events are left to the poll. null when there is no version.
async function currentFp() {{
  try {{
    const r = await (await fetch(prefix + "/agents/" + id + "/events.json?after=" + Number.MAX_SAFE_INTEGER)).json();
    return r.workspace ? r.workspace.fingerprint : null;
  }} catch (e) {{ return null; }}
}}
// Manual "Reload diff": unconditional, and the way a daemon refusal reaches
// the banner. Stamps the fingerprint the fetch corresponds to: `fp` when
// the caller just read it, else `currentFp()`.
async function loadDiff(fp) {{
  const banner = document.getElementById("banner"); banner.textContent = "";
  try {{
    if (fp === undefined) fp = await currentFp();
    applyDiff(await fetchDiff(), fp);
  }}
  catch (e) {{ banner.style.color = "#b00"; banner.textContent = "diff: " + e.message; }}
}}

// One review at a time: the button is off while a POST is out, so a second
// click cannot submit it twice (#225). A comment whose edit box is open is
// not sent: it stays in the draft for the next review, with its edit. What
// was sent is what success clears: a comment saved or edited while the POST
// was out stays, and so does a summary changed meanwhile.
let sending = false;
async function send() {{
  if (sending) return;
  const banner = document.getElementById("banner"); banner.textContent = "";
  const sent = draft.comments.filter((c) => !c.editing).map((c) => [c, c.body]);
  const open = draft.comments.length - sent.length;
  const summary = draft.summary;
  const comments = sent.map(([c]) => ({{ path: c.path, side: c.side, line: c.line, text: c.text, body: c.body }}));
  const body = {{ head: diff ? diff.head : "", base_ref: diff ? diff.base_ref : "", summary, comments }};
  const button = document.getElementById("send");
  sending = true; button.disabled = true;
  try {{
    const r = await fetch(prefix + "/agents/" + id + "/review", {{ method: "POST", headers: {{ "content-type": "application/json" }}, body: JSON.stringify(body) }});
    if (r.ok) {{
      const done = new Set(sent.filter(([c, b]) => c.body === b).map(([c]) => c));
      draft.comments = draft.comments.filter((c) => !done.has(c));
      if (draft.summary === summary) draft.summary = "";
      save(); render(); banner.style.color = "#080";
      banner.textContent = "review sent" + (open ? " (" + open + " comment" + (open === 1 ? "" : "s") + " being edited kept for the next one)" : "");
    }}
    else {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + (await r.text()); }}
  }} catch (e) {{ banner.style.color = "#b00"; banner.textContent = "send failed: " + e; }}
  finally {{ sending = false; button.disabled = false; }}
}}

let unread = 0, fetching = false;
const side = document.getElementById("side");
function renderEvent(e) {{
  const row = el("div", "ev" + (e.name === "review_sent" ? " divider" : ""));
  // A turn's text replaces its summary, which is only its first line or "turn ended".
  const hasText = typeof e.text === "string";
  row.append(el("span", "t", new Date(e.at * 1000).toLocaleTimeString()), el("span", "n", e.name));
  if (!hasText) row.appendChild(el("span", "s", e.summary));
  if (e.payload_truncated || e.text_truncated) row.appendChild(el("span", "badge", " · cut"));
  if (hasText) row.appendChild(el("div", "x", e.text));
  const pre = el("pre", "", JSON.stringify(e.payload, null, 2) + (e.payload_truncated ? "\n(truncated)" : "")); pre.hidden = true;
  row.appendChild(pre); row.onclick = () => {{ pre.hidden = !pre.hidden; }};
  return row;
}}
async function fetchEvents(after) {{
  return await (await fetch(prefix + "/agents/" + id + "/events.json?after=" + after)).json();
}}
// The activity column's half of a poll: new events and the phase. Returns
// the poll's body (its `workspace` is the version, or null when the daemon
// refused, failed or was slower than the plugin's deadline), or null when
// the poll itself failed or another was still out (#225: one at a time, as
// the interval does not wait for a slow one). A changed `epoch` is a log
// made again, its `seq` restarted (a re-activation, a plugin restart): the
// column is cleared and read again from the start.
let lastSeq = 0, epoch = null, polling = false;
async function pollEvents() {{
  if (polling) return null;
  polling = true;
  try {{
    let r = await fetchEvents(lastSeq);
    const box = document.getElementById("events");
    if (epoch !== null && r.epoch !== epoch) {{
      lastSeq = 0; unread = 0;
      const none = document.getElementById("no-events"); none.hidden = false;
      box.replaceChildren(none);
      r = await fetchEvents(0);
    }}
    epoch = r.epoch;
    document.getElementById("phase").textContent = r.phase;
    const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
    const fresh = r.events.filter((e) => e.seq > lastSeq);
    for (const e of fresh) {{ box.appendChild(renderEvent(e)); lastSeq = e.seq; if (side.classList.contains("collapsed")) unread++; }}
    if (lastSeq > 0) document.getElementById("no-events").hidden = true;
    document.getElementById("unread").textContent = unread ? "(" + unread + " new)" : "";
    if (fresh.length && atBottom) box.scrollTop = box.scrollHeight;
    return r;
  }} catch (e) {{ console.warn("events", e); return null; }}
  finally {{ polling = false; }}
}}
// The diff column's half: a new fingerprint fetches the diff once and parks
// it until maybeApply lets it through — unless a reload applied that
// fingerprint while the fetch was out. No version before any diff is shown
// surfaces the daemon's reason once, through loadDiff.
async function refreshDiff(w) {{
  if (fetching) return;
  if (w) {{
    const fp = w.fingerprint;
    if (fp === rendered || (pendingDiff && pendingDiff.fp === fp)) return;
    fetching = true;
    try {{
      const d = await fetchDiff();
      if (fp !== rendered) pendingDiff = {{ diff: d, fp }};
      const banner = document.getElementById("banner");
      if (banner.textContent.startsWith("diff: ")) banner.textContent = "";
    }}
    catch (e) {{
      if (diff === null) {{ const banner = document.getElementById("banner"); banner.style.color = "#b00"; banner.textContent = "diff: " + e.message; }}
      else console.warn("diff", e);
    }}
    finally {{ fetching = false; }}
  }} else if (diff === null) {{
    fetching = true; try {{ await loadDiff(null); }} finally {{ fetching = false; }}
  }}
}}
// One poll drives both columns (Spec D PD-2), each through its own half.
async function poll() {{
  const r = await pollEvents();
  if (r) await refreshDiff(r.workspace);
  maybeApply();
}}

function applyCollapse() {{ side.classList.toggle("collapsed", !!draft.collapsed); if (!draft.collapsed) {{ unread = 0; document.getElementById("unread").textContent = ""; }} }}
document.getElementById("collapse").onclick = () => {{ draft.collapsed = !draft.collapsed; save(); applyCollapse(); }};
document.getElementById("reload").onclick = () => loadDiff();
document.getElementById("send").onclick = send;
document.getElementById("summary").oninput = (ev) => {{ draft.summary = ev.target.value; save(); }};
applyCollapse();
render();
poll();
setInterval(poll, {poll});
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
    events.workspace = bounded_version(
        &id,
        shared_version(&shared, &id),
        VERSION_DEADLINE,
        &shared.version_failures_total,
    )
    .await;
    Json(events).into_response()
}

/// How long a poll waits for the worktree's version before answering
/// without it (#18): the column keeps flowing when the daemon is slow, not
/// only when it fails. Under the page's 2 s poll.
const VERSION_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1500);

/// What a daemon `version` call answered; `None` until it has.
type VersionAnswer = Option<Result<balerix_api::WorkspaceVersion, SdkError>>;

/// The daemon `version` call in flight for each agent (#185). A poll
/// that times out drops only its wait, never the call, so without this
/// a version slower than the page's 2 s poll would have one call per
/// poll running on the daemon at once. The entry goes when its call ends.
#[derive(Default)]
pub struct VersionFlights(std::sync::Mutex<HashMap<String, watch::Receiver<VersionAnswer>>>);

impl VersionFlights {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, watch::Receiver<VersionAnswer>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Removes an agent's flight when its call task ends, also by panic or
/// abort, so the next poll starts a fresh call.
struct Landing {
    shared: Arc<Shared>,
    id: String,
}

impl Drop for Landing {
    fn drop(&mut self) {
        self.shared.versions.lock().remove(&self.id);
    }
}

/// `id`'s worktree version through the call in flight for it, starting
/// one when there is none. The call runs in its own task: a caller that
/// stops waiting (`bounded_version`'s deadline) leaves it running for
/// the polls after it.
fn shared_version(
    shared: &Arc<Shared>,
    id: &str,
) -> impl std::future::Future<Output = Result<balerix_api::WorkspaceVersion, SdkError>> + use<> {
    let mut rx = {
        let mut flights = shared.versions.lock();
        match flights.get(id) {
            Some(rx) => rx.clone(),
            None => {
                let (tx, rx) = watch::channel(None);
                flights.insert(id.to_string(), rx.clone());
                let landing = Landing {
                    shared: shared.clone(),
                    id: id.to_string(),
                };
                tokio::spawn(async move {
                    let answer = landing.shared.host.workspace_version(&landing.id).await;
                    // a poll between these two lines reads this answer
                    tx.send_replace(Some(answer));
                    drop(landing);
                });
                rx
            }
        }
    };
    async move {
        match rx.wait_for(Option::is_some).await.as_deref() {
            Ok(Some(answer)) => answer.clone(),
            // the task ended without an answer: it panicked
            _ => Err(SdkError::Transport(
                "workspace version call ended without an answer".into(),
            )),
        }
    }
}

/// `version`, or `None` when it fails or outlives `deadline`; a timeout
/// counts in `version_failures_total` like any failure but the routine
/// no-workspace 404 (`is_version_failure`).
async fn bounded_version<F>(
    id: &str,
    version: F,
    deadline: std::time::Duration,
    failures: &IntCounter,
) -> Option<balerix_api::WorkspaceVersion>
where
    F: std::future::Future<Output = Result<balerix_api::WorkspaceVersion, SdkError>>,
{
    match tokio::time::timeout(deadline, version).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::debug!(agent = %id, "workspace version: {e}");
            if is_version_failure(&e) {
                failures.inc();
            }
            None
        }
        Err(_) => {
            tracing::debug!(agent = %id, "workspace version: no answer within {deadline:?}");
            failures.inc();
            None
        }
    }
}

/// Whether a `workspace_version` error counts in `version_failures_total`
/// (Spec D §3.3): everything but the daemon's routine 404 `no workspace
/// for agent …`, which an open page sees on every poll until `up` has made
/// the worktree (#17).
fn is_version_failure(e: &SdkError) -> bool {
    !matches!(
        e,
        SdkError::Status { status: 404, message } if message.starts_with("no workspace for agent")
    )
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
/// reach the browser, so the page can say why. An attach that fails,
/// the SDK's 10 s handshake deadline included (#227), closes the browser
/// with 1011 before anything is counted open.
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

    /// Only the daemon's routine "no worktree yet" answer is quiet; every
    /// other refusal and every transport failure counts (#17).
    #[test]
    fn only_the_no_workspace_404_is_not_a_version_failure() {
        let status = |status, message: &str| SdkError::Status {
            status,
            message: message.into(),
        };
        assert!(!is_version_failure(&status(
            404,
            "no workspace for agent f/c/a"
        )));
        assert!(is_version_failure(&status(404, "no such agent f/c/a")));
        assert!(is_version_failure(&status(
            500,
            "no workspace for agent f/c/a"
        )));
        assert!(is_version_failure(&status(403, "forbidden")));
        assert!(is_version_failure(&SdkError::Transport("refused".into())));
    }

    /// #18: a version slower than the deadline is answered as `null` and
    /// counted; a quick one passes through; the routine 404 is not counted.
    #[tokio::test]
    async fn a_slow_version_is_dropped_and_counted() {
        let failures = IntCounter::new("f", "failures").unwrap();
        let short = std::time::Duration::from_millis(50);
        let never = std::future::pending::<Result<balerix_api::WorkspaceVersion, SdkError>>();
        assert_eq!(bounded_version("a", never, short, &failures).await, None);
        assert_eq!(failures.get(), 1);
        let v = balerix_api::WorkspaceVersion::new("h", "f");
        let quick = std::future::ready(Ok(v.clone()));
        assert_eq!(bounded_version("a", quick, short, &failures).await, Some(v));
        let routine = std::future::ready(Err(SdkError::Status {
            status: 404,
            message: "no workspace for agent f/c/a".into(),
        }));
        assert_eq!(bounded_version("a", routine, short, &failures).await, None);
        assert_eq!(failures.get(), 1);
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

    /// Returns `true` if the test may run. Otherwise prints a skip reason,
    /// or panics when `BALERIX_REQUIRE_TOOLS=1` (CI never skips) — the
    /// pattern of `balerix-runtime`'s integration tests.
    fn require_or_skip(name: &str, present: bool) -> bool {
        if present {
            return true;
        }
        if std::env::var_os("BALERIX_REQUIRE_TOOLS").is_some_and(|v| v == "1") {
            panic!("{name} is required (BALERIX_REQUIRE_TOOLS=1) but not available");
        }
        eprintln!("skip: {name} not available");
        false
    }

    /// Runs `script` under `node` on stdin and asserts it exits 0 having
    /// printed `marker`; returns at once when node is missing and the test
    /// may skip (`require_or_skip`).
    fn run_under_node(script: &str, marker: &str) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let node = Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !require_or_skip("node", node) {
            return;
        }
        let mut child = Command::new("node")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("node starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(script.as_bytes())
            .expect("the script reaches node");
        let out = child.wait_with_output().expect("node finishes");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains(marker),
            "node failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The review page's whole script, for `run_under_node`.
    fn page_script() -> String {
        let page = review_html("", "f/c/a");
        let start = page.find("<script>\n").expect("the script") + "<script>\n".len();
        let end = page.rfind("</script>").expect("its end");
        page[start..end].to_string()
    }

    /// A stub DOM for `page_script`: elements keep their children, and
    /// `.file` elements stack 100 px apart. `fetch` is the test's own,
    /// defined after this and before the page's script (its load polls).
    const DOM_STUBS: &str = r#"
const assert = require("node:assert");
class El {
  constructor(tag) { this.tag = tag; this.children = []; this.dataset = {}; this.style = {}; this.className = ""; this.textContent = ""; this.value = ""; this.hidden = false; this.disabled = false; this.scrollTop = 0; this.scrollHeight = 0; this.clientHeight = 0; this.classList = { add() {}, remove() {}, toggle() {}, contains() { return false; } }; }
  appendChild(c) { this.children.push(c); return c; }
  append(...cs) { for (const c of cs) this.children.push(c); }
  replaceChildren(...cs) { this.children = cs; }
  // layout: the files stack 100 px apart, in order
  querySelectorAll(sel) { assert.strictEqual(sel, ".file"); const out = this.children.filter((c) => c instanceof El && c.className === "file"); out.forEach((f, i) => { f.offsetTop = i * 100; }); return out; }
}
const els = {};
globalThis.document = { getElementById: (k) => (els[k] = els[k] || new El(k)), createElement: (t) => new El(t) };
globalThis.localStorage = { getItem: () => null, setItem: () => {} };
globalThis.setInterval = () => 0;
globalThis.setTimeout = () => 0;
const settle = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
const json = (v) => ({ ok: true, json: async () => v, text: async () => JSON.stringify(v) });
// every text under an element, depth first
const texts = (e, out = []) => { if (e.textContent) out.push(e.textContent); for (const c of e.children || []) if (c instanceof El) texts(c, out); return out; };
"#;

    /// Spec D PD-4 run as the page runs it (#20): the script is sliced out
    /// of `review_html` itself — `anchorOf`, `parsePatch` and
    /// `anchorComments` — and the four cases run under `node`, fed on stdin.
    #[test]
    fn anchor_comments_keeps_reanchors_or_stales_under_node() {
        let page = review_html("", "f/c/a");
        let start = page.find("const anchorOf").expect("anchorOf in the page");
        let func = page
            .find("function anchorComments(")
            .expect("anchorComments in the page");
        let end = func + page[func..].find("\n}\n").expect("its closing brace") + 3;
        let cases = r#"
const assert = require("node:assert");
// new: 1 " keep", 2 "+moved", 3 "+twin", 4 "+twin"; old: 1 " keep", 2 "-gone"
const patch = "@@ -1,2 +1,4 @@\n keep\n-gone\n+moved\n+twin\n+twin\n";
const d = { files: [{ path: "a.rs", patch }] };
const exact = { path: "a.rs", side: "new", line: 1, text: " keep" };
const unique = { path: "a.rs", side: "new", line: 9, text: "+moved" };
const several = { path: "a.rs", side: "new", line: 9, text: "+twin" };
const none = { path: "a.rs", side: "new", line: 2, text: "+vanished" };
const stale = anchorComments(d, [exact, unique, several, none]);
assert.strictEqual(exact.line, 1, "the exact anchor keeps");
assert.strictEqual(unique.line, 2, "exactly one same path/side/text line re-anchors");
assert.deepStrictEqual(stale, [several, none], "several or none: stale");
assert.strictEqual(several.line, 9, "a stale comment keeps its line");
console.log("anchor cases ok");
"#;
        let script = format!("{}\n{cases}", &page[start..end]);
        run_under_node(&script, "anchor cases ok");
    }

    /// The review page's whole script under `node` with a stub DOM and a
    /// scripted `fetch` (#20): the scroll keeps the reader's offset into the
    /// top file; "Reload diff" stamps the fingerprint read before its fetch;
    /// a poll's fetch that a reload overtook parks nothing.
    #[test]
    fn the_review_page_keeps_its_place_and_its_fingerprints_under_node() {
        let fetches = r#"
// fetch: events.json answers `version` (null until set); diff.json waits on `diffs`
let version = null; const diffs = []; const fetched = [];
globalThis.fetch = async (url) => {
  fetched.push(url);
  if (url.includes("/events.json")) return json({ phase: "ready", events: [], workspace: version && { head: "h", fingerprint: version } });
  return await new Promise((resolve) => diffs.push((d) => resolve(json(d))));
};
const file = (path) => ({ path, status: "modified", patch: "@@ -1 +1 @@\n-a\n+b\n" });
const diffOf = (...paths) => ({ base_ref: "origin/main", head: "h", truncated: false, files: paths.map(file) });
"#;
        let cases = r#"
(async () => {
  // the first poll finds no version: one diff fetch, through loadDiff
  await settle();
  diffs.shift()(diffOf("a", "b", "c"));
  await settle();
  assert.strictEqual(diff.files.length, 3);
  assert.strictEqual(rendered, null);

  // scroll: 30 px into b (offsetTop 100); a file lands above it
  const root = document.getElementById("diff");
  root.scrollTop = 130;
  applyDiff(diffOf("0", "a", "b", "c"), "x");
  assert.strictEqual(root.scrollTop, 230, "the same 30 px into b, now at 200");

  // "Reload diff" stamps the fingerprint read just before its fetch
  version = "f1";
  document.getElementById("reload").onclick({ type: "click" });
  await settle();
  assert.ok(fetched[fetched.length - 2].endsWith("events.json?after=" + Number.MAX_SAFE_INTEGER), fetched.join("\n"));
  diffs.shift()(diffOf("a"));
  await settle();
  assert.strictEqual(rendered, "f1", "the fingerprint the fetch corresponds to");

  // a poll's fetch for f2 that a reload overtakes parks nothing
  version = "f2";
  const polling = poll();
  await settle();
  assert.strictEqual(diffs.length, 1, "the poll's fetch is out");
  const reload = loadDiff();
  await settle();
  diffs.pop()(diffOf("a", "b"));
  await reload;
  assert.strictEqual(rendered, "f2");
  diffs.shift()(diffOf("a", "b"));
  await polling;
  assert.strictEqual(pendingDiff, null, "no second render of what is on screen");
  console.log("page cases ok");
})().catch((e) => { console.error(e); process.exit(1); });
"#;
        let script = format!("{DOM_STUBS}\n{fetches}\n{}\n{cases}", page_script());
        run_under_node(&script, "page cases ok");
    }

    /// #225 under `node`: overlapping polls render each event once; a
    /// changed epoch clears the column and reads it again from seq 0; Send
    /// is one POST at a time and success clears only what it sent.
    #[test]
    fn the_review_page_polls_once_at_a_time_and_sends_once_under_node() {
        let fetches = r#"
// events.json and the POST wait on `out`, answered by the test; diff.json is empty at once
const out = []; const posts = []; const fetched = [];
globalThis.fetch = async (url, opts) => {
  fetched.push(url);
  if (url.includes("/diff.json")) return json({ base_ref: "origin/main", head: "h", truncated: false, files: [] });
  if (opts && opts.method === "POST") posts.push(JSON.parse(opts.body));
  // `out.shift()(v)` answers 200 with `v`; `.raw(response)` answers that; `.fail(e)` throws
  return await new Promise((resolve, reject) => { const f = (v) => resolve(json(v)); f.raw = resolve; f.fail = reject; out.push(f); });
};
const ev = (seq, summary) => ({ seq, at: seq, name: "Notification", summary: summary || "n" + seq, payload: {} });
const rows = () => document.getElementById("events").children.filter((c) => c.className && c.className.startsWith("ev")).map((r) => texts(r).join(" "));
"#;
        let cases = r#"
(async () => {
  await settle();
  assert.strictEqual(out.length, 1, "the load's poll is out");
  // the interval ticks while it is out: no second request
  assert.strictEqual(await pollEvents(), null, "skipped while one is in flight");
  const tick = poll(); await settle();
  assert.strictEqual(out.length, 1, fetched.join("\n"));
  out.shift()({ epoch: "A", phase: "ready", workspace: null, events: [ev(1), ev(2)] });
  await tick; await settle();
  assert.deepStrictEqual(rows().length, 2);

  // an event at or below lastSeq is dropped, whatever the server sends
  const p = poll(); await settle();
  assert.ok(fetched[fetched.length - 1].endsWith("events.json?after=2"), fetched.join("\n"));
  out.shift()({ epoch: "A", phase: "ready", workspace: null, events: [ev(2), ev(3)] });
  await p;
  assert.strictEqual(rows().length, 3, rows().join("\n"));
  assert.strictEqual(lastSeq, 3);

  // a re-activation: the log is new and its seq starts again below 3
  const q = poll(); await settle();
  out.shift()({ epoch: "B", phase: "ready", workspace: null, events: [] });
  await settle();
  assert.ok(fetched[fetched.length - 1].endsWith("events.json?after=0"), "read again from the start: " + fetched.join("\n"));
  out.shift()({ epoch: "B", phase: "ready", workspace: null, events: [ev(1, "fresh")] });
  await q;
  assert.deepStrictEqual(rows().length, 1, rows().join("\n"));
  assert.ok(rows()[0].includes("fresh"), rows()[0]);
  assert.strictEqual(lastSeq, 1);
  assert.strictEqual(document.getElementById("no-events").hidden, true);

  // Send twice before the first answers: one POST, the button off meanwhile
  const button = document.getElementById("send");
  const c1 = { path: "a.rs", side: "new", line: 1, text: "+x", body: "one" };
  const c2 = { path: "a.rs", side: "new", line: 2, text: "+y", body: "two" };
  draft.comments = [c1, c2]; draft.summary = "please fix";
  const s1 = send(); const s2 = send(); await settle();
  assert.strictEqual(posts.length, 1, "one review");
  assert.strictEqual(button.disabled, true);
  // meanwhile: a comment saved, one edited, the summary changed
  const c3 = { path: "a.rs", side: "new", line: 3, text: "+z", body: "three" };
  draft.comments.push(c3); c2.body = "two, edited";
  draft.summary = "please fix, and test";
  out.shift()({});
  await s1; await s2;
  assert.strictEqual(button.disabled, false);
  assert.deepStrictEqual(posts[0].comments.map((c) => c.body), ["one", "two"]);
  assert.deepStrictEqual(draft.comments, [c2, c3], "only what was sent, unchanged, is cleared");
  assert.strictEqual(draft.summary, "please fix, and test", "a changed summary stays");

  // an unchanged summary is cleared; a failed send keeps everything
  draft.comments = [c3]; draft.summary = "again";
  const s3 = send(); await settle();
  out.shift()({});
  await s3;
  assert.deepStrictEqual(draft.comments, []);
  assert.strictEqual(draft.summary, "");

  // a comment whose edit box is open is not sent, and is kept with its edit
  // (on lines of the diff: a stale comment's box is closed by render)
  diff = { base_ref: "origin/main", head: "h", truncated: false, files: [{ path: "a.rs", status: "modified", patch: "@@ -3,0 +4,2 @@\n+w\n+v\n" }] };
  const banner = document.getElementById("banner");
  const c4 = { path: "a.rs", side: "new", line: 4, text: "+w", body: "four", editing: true, typing: "four, better" };
  const c5 = { path: "a.rs", side: "new", line: 5, text: "+v", body: "five" };
  draft.comments = [c4, c5];
  posts.length = 0;
  const s4 = send(); await settle();
  assert.deepStrictEqual(posts[0].comments.map((c) => c.body), ["five"], "the open one stays out");
  out.shift()({});
  await s4;
  assert.deepStrictEqual(draft.comments, [c4]);
  assert.strictEqual(c4.typing, "four, better");
  assert.ok(banner.textContent.startsWith("review sent (1 comment being edited kept"), banner.textContent);

  // failures keep the whole draft and give the button back
  delete c4.editing; delete c4.typing;
  draft.comments = [c4, c5]; draft.summary = "keep";
  const s5 = send(); await settle();
  assert.strictEqual(button.disabled, true);
  out.shift().raw({ ok: false, json: async () => ({}), text: async () => "f/c/a: no window" });
  await s5;
  assert.strictEqual(button.disabled, false);
  assert.strictEqual(banner.textContent, "send failed: f/c/a: no window");
  assert.deepStrictEqual(draft.comments, [c4, c5]);
  assert.strictEqual(draft.summary, "keep");
  const s6 = send(); await settle();
  out.shift().fail(new Error("network down"));
  await s6;
  assert.strictEqual(button.disabled, false);
  assert.strictEqual(banner.textContent, "send failed: Error: network down");
  assert.deepStrictEqual(draft.comments, [c4, c5]);
  assert.strictEqual(draft.summary, "keep");
  assert.strictEqual(posts.length, 3);
  console.log("race cases ok");
})().catch((e) => { console.error(e); process.exit(1); });
"#;
        let script = format!("{DOM_STUBS}\n{fetches}\n{}\n{cases}", page_script());
        run_under_node(&script, "race cases ok");
    }

    /// #226 under `node`, on patches the daemon sends: a typechange's two
    /// sections (git's own output, `x` from a file to a symlink) are two
    /// hunks with no header lines in them; a blank context line printed
    /// empty still counts; a hunkless patch is a notice, with "view file"
    /// only when git wrote it.
    #[test]
    fn the_review_page_parses_typechanges_blank_lines_and_notices_under_node() {
        let fetches = r#"globalThis.fetch = () => new Promise(() => {});"#;
        let cases = r#"
const lines = (p) => parsePatch(p).map((h) => h.lines.map((l) => [l.kind, l.kind === "ctx" ? l.oldLine : l.side === "old" ? l.line : "", l.side === "new" ? l.line : "", l.text]));
const typechange = "diff --git a/x b/x\ndeleted file mode 100644\nindex ce01362..0000000\n--- a/x\n+++ /dev/null\n@@ -1 +0,0 @@\n-hello\ndiff --git a/x b/x\nnew file mode 120000\nindex 0000000..1de5659\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+target\n\\ No newline at end of file\n";
assert.deepStrictEqual(lines(typechange), [
  [["del", 1, "", "-hello"]],
  [["add", "", 1, "+target"], ["meta", "", "", "\\ No newline at end of file"]],
]);

// diff.suppressBlankEmpty=true: the blank line 2 arrives as "", not " "
const blank = "diff --git a/f b/f\nindex d32b46e..6c17d57 100644\n--- a/f\n+++ b/f\n@@ -1,5 +1,5 @@\n a\n\n b\n-c\n+C\n d\n";
assert.deepStrictEqual(lines(blank), [[
  ["ctx", 1, 1, " a"], ["ctx", 2, 2, " "], ["ctx", 3, 3, " b"],
  ["del", 4, "", "-c"], ["add", "", 4, "+C"], ["ctx", 5, 5, " d"],
]]);
assert.deepStrictEqual(lines(blank.replace("\n\n", "\n \n")), lines(blank), "the same as git's default");
// two hunks: the second's counts start afresh, the trailing "" is nothing
const two = "@@ -1,2 +1,2 @@\n a\n\n@@ -9 +9 @@\n-z\n+Z\n";
assert.deepStrictEqual(lines(two), [[["ctx", 1, 1, " a"], ["ctx", 2, 2, " "]], [["del", 9, "", "-z"], ["add", "", 9, "+Z"]]]);

// a patch cut mid-hunk (`truncated`) ends in a newline: nothing after it is a line
assert.deepStrictEqual(lines("@@ -1,5 +1,5 @@\n a\n b\n"), [[["ctx", 1, 1, " a"], ["ctx", 2, 2, " b"]]]);
assert.deepStrictEqual(lines("@@ -1,5 +1,5 @@\n a\n\n"), [[["ctx", 1, 1, " a"], ["ctx", 2, 2, " "]]], "a blank line before the cut still counts");

const file = (patch, status, truncated) => ({ path: "p", status: status || "added", uncommitted: true, binary: false, truncated: !!truncated, patch });
const shown = (f) => texts(renderFile(f));
const skipped = shown(file("not a regular file, skipped\n"));
assert.ok(skipped.includes("not a regular file, skipped"), skipped.join("|"));
assert.ok(!skipped.includes("view file"), "nothing to view: " + skipped.join("|"));
const mode = shown(file("diff --git a/p b/p\nold mode 100644\nnew mode 100755\n", "modified"));
assert.ok(mode.includes("old mode 100644\nnew mode 100755"), mode.join("|"));
assert.ok(mode.includes("view file"), "git's hunkless patch keeps the link: " + mode.join("|"));
// an empty patch: an empty file, or one long line past the patch cap
for (const f of [file(""), file("", "modified", true)]) {
  const box = renderFile(f);
  assert.ok(texts(box).includes("view file"), "the only way to see it: " + texts(box).join("|"));
  assert.ok(!box.children.some((c) => c.className === "notice"), "no empty notice");
}
const hunk = shown(file(blank, "modified"));
assert.ok(hunk.includes("view file") && hunk.includes("+C"), hunk.join("|"));
console.log("patch cases ok");
"#;
        let script = format!("{DOM_STUBS}\n{fetches}\n{}\n{cases}", page_script());
        run_under_node(&script, "patch cases ok");
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
            page.contains(r#"const hasText = typeof e.text === "string";"#)
                && page.contains(r#"if (!hasText) row.appendChild(el("span", "s", e.summary));"#),
            "a row with text shows the text, not the summary's first line again"
        );
        assert!(
            page.contains(r#"el("div", "x", e.text)"#),
            "a turn's text shows on its row, through textContent (#42)"
        );
        assert!(
            page.contains("e.payload_truncated || e.text_truncated"),
            "a cut entry is marked on its row (#42)"
        );
        assert!(
            !page.contains("loadDiff();\npoll();"),
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
