// BlueLink 前端逻辑：调用 Tauri 命令、订阅后端事件、渲染三栏界面。
// 定位链路全局只有一条：locateMessage（会话切换→加载→滚动→高亮），标签/收藏/搜索/置顶共用。
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const state = {
  identity: null,
  connected: false,
  peerName: null,
  view: "chat",
  peerConvId: null,
  aiConvId: "ai-default",
  replyTo: null,
  renderedChat: new Set(),
  renderedAi: new Set(),
  aiGenerating: false,
  streamEl: null,
  tagEditId: null,
  tagEditList: [],
  lastFileBubble: null,
  btDevice: null,
  btDeviceName: null,
  devMode: false,
};

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

/** 极简 Markdown：代码块、行内代码、加粗、换行（AI 回复用）。 */
function md(s) {
  let h = esc(s);
  h = h.replace(/```([\s\S]*?)```/g, (_, code) => `<pre>${code}</pre>`);
  h = h.replace(/`([^`\n]+)`/g, "<code>$1</code>");
  h = h.replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>");
  return h;
}

function fmtTime(ts) {
  const d = new Date(ts);
  return d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" });
}
function fmtDay(ts) {
  const d = new Date(ts);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  return sameDay ? "今天" : d.toLocaleDateString("zh-CN", { month: "long", day: "numeric" });
}
function fmtSize(n) {
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MB";
  return (n / 1024 / 1024 / 1024).toFixed(2) + " GB";
}
const STATUS_CN = { sending: "发送中", sent: "已发送", delivered: "已送达", received: "", failed: "发送失败" };

function iconUse(name, cls = "ico ico-16") {
  return `<svg class="${cls}"><use href="#${name}"/></svg>`;
}
/** 构建一条消息的 DOM（无头像，靠左右对齐与底色区分双方）。opts.ai 表示 AI 会话样式。 */
function buildMessageEl(m, opts = {}) {
  const row = document.createElement("div");
  row.className = "msg-row" + (m.sender === "me" ? " me" : "");
  row.dataset.mid = m.id;
  row.dataset.sender = m.sender;
  const wrap = document.createElement("div");
  wrap.className = "msg";
  const bubble = document.createElement("div");
  bubble.className = "bubble" + (m.kind === "file" ? " file-card" : "");
  if (m.kind === "file") {
    bubble.innerHTML = `<div class="file-card-inner"><span class="file-ico">${iconUse("i-file", "ico ico-20")}</span><span class="file-main"><span class="file-name">${esc(m.body)}</span><span class="file-meta">文件</span></span></div><div class="progress hidden"><div></div></div>`;
    state.lastFileBubble = bubble;
  } else if (opts.ai && m.sender === "ai") {
    bubble.innerHTML = md(m.body);
  } else if (m.body.length > 500 || m.body.split("\n").length > 10) {
    // 长文本默认折叠，避免单个气泡刷屏，需要时点开全文
    const textDiv = document.createElement("div");
    textDiv.className = "msg-text collapsed";
    textDiv.textContent = m.body;
    const tg = document.createElement("button");
    tg.className = "bubble-toggle";
    tg.textContent = "展开全文";
    tg.addEventListener("click", () => {
      const collapsed = textDiv.classList.toggle("collapsed");
      tg.textContent = collapsed ? "展开全文" : "收起";
    });
    bubble.appendChild(textDiv);
    bubble.appendChild(tg);
  } else {
    bubble.textContent = m.body;
  }
  const meta = document.createElement("div");
  meta.className = "msg-meta";
  const tagsSpan = document.createElement("span");
  tagsSpan.className = "msg-tags";
  meta.appendChild(tagsSpan);
  const status = document.createElement("span");
  status.className = "msg-status";
  status.textContent = STATUS_CN[m.status] || "";
  const time = document.createElement("span");
  time.textContent = fmtTime(m.created_at);
  const actions = document.createElement("span");
  actions.className = "msg-actions";
  const btns = [["标签", () => openTagEditor(m.id)], ["收藏", () => toggleMark(m.id, "favorite")], ["置顶", () => toggleMark(m.id, "pin")]];
  if (!opts.ai) btns.push(["引用", () => setReply(m)]);
  if (opts.ai && m.sender === "ai") btns.push(["复制", () => navigator.clipboard.writeText(m.body)], ["重新生成", () => regenerateAi()]);
  for (const [label, fn] of btns) {
    const b = document.createElement("button");
    b.textContent = label;
    b.addEventListener("click", fn);
    actions.appendChild(b);
  }
  meta.appendChild(status); meta.appendChild(time); meta.appendChild(actions);
  wrap.appendChild(bubble); wrap.appendChild(meta);
  row.appendChild(wrap);
  refreshMessageTags(m.id, tagsSpan);
  return row;
}

async function refreshMessageTags(mid, span) {
  try {
    const tags = await invoke("get_message_tags", { messageId: mid });
    span.innerHTML = "";
    for (const t of tags) {
      const chip = document.createElement("span");
      chip.className = "tag-chip";
      chip.textContent = t;
      const chipHex = (state.tagColorMap || {})[t.toLowerCase()];
      if (chipHex) { chip.style.borderColor = chipHex; chip.style.color = chipHex; }
      chip.addEventListener("click", () => openTagDrawer(t));
      span.appendChild(chip);
    }
  } catch (_) { /* 忽略 */ }
}

function appendMessage(container, m, renderedSet, opts) {
  if (renderedSet.has(m.id)) return;
  // 日期分隔
  const rows = container.querySelectorAll(".msg-row");
  const last = rows.length ? rows[rows.length - 1] : null;
  const lastTs = last ? Number(last.dataset.ts || 0) : 0;
  const newDay = !lastTs || fmtDay(lastTs) !== fmtDay(m.created_at);
  if (newDay) {
    const sep = document.createElement("div");
    sep.className = "date-sep";
    sep.innerHTML = `<span>${fmtDay(m.created_at)}</span>`;
    container.appendChild(sep);
  }
  const el = buildMessageEl(m, opts);
  el.dataset.ts = m.created_at;
  // 消息分组：同日、同发送者、间隔 < 5 分钟时压缩间距
  if (!newDay && last && last.dataset.sender === m.sender && m.created_at - lastTs < 5 * 60 * 1000) {
    el.classList.add("grouped");
  }
  container.appendChild(el);
  renderedSet.add(m.id);
  container.scrollTop = container.scrollHeight;
}

/** 唯一的定位链路：切会话→加载消息→滚动到目标→高亮闪烁两次。 */
async function locateMessage(conversationId, messageId) {
  const isAi = conversationId === state.aiConvId || conversationId.startsWith("ai");
  switchView(isAi ? "ai" : "chat");
  const container = isAi ? $("ai-messages") : $("messages");
  const set = isAi ? state.renderedAi : state.renderedChat;
  if (!set.has(messageId)) {
    const msgs = await invoke("list_messages", { conversationId, limit: 2000 });
    container.innerHTML = "";
    set.clear();
    for (const m of msgs) appendMessage(container, m, set, { ai: isAi });
  }
  const el = container.querySelector(`[data-mid="${messageId}"]`);
  if (el) {
    el.scrollIntoView({ block: "center" });
    el.classList.add("flash");
    setTimeout(() => el.classList.remove("flash"), 1300);
  }
  closeDrawer();
}

// ---------- 视图切换 ----------
function switchView(view) {
  state.view = view;
  document.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  $("conv-list").classList.toggle("hidden", view !== "chat");
  $("tag-list").classList.toggle("hidden", view !== "tags");
  $("fav-list").classList.toggle("hidden", view !== "favorites");
  $("chat-view").classList.toggle("hidden", !(view === "chat" && state.connected));
  $("connect-view").classList.toggle("hidden", !(view === "chat" && !state.connected));
  $("ai-view").classList.toggle("hidden", view !== "ai");
  $("settings-view").classList.toggle("hidden", view !== "settings");
  $("placeholder-view").classList.toggle("hidden", !(view === "tags" || view === "favorites"));
  if (view === "tags") loadTagList();
  if (view === "favorites") loadFavList();
  if (view === "ai") loadAiMessages();
  if (view === "settings") loadSettings();
}

document.querySelectorAll(".nav-item").forEach((b) =>
  b.addEventListener("click", () => switchView(b.dataset.view)));

// ---------- 中间列表收起/展开（长期只连同一台设备时可收起，状态记忆） ----------
function applyAsideCollapsed() {
  const collapsed = localStorage.getItem("bl-aside") === "collapsed";
  $("aside").classList.toggle("hidden", collapsed);
  $("btn-expand-aside").classList.toggle("hidden", !collapsed);
}
$("btn-collapse-aside").addEventListener("click", () => {
  localStorage.setItem("bl-aside", "collapsed");
  applyAsideCollapsed();
});
$("btn-expand-aside").addEventListener("click", () => {
  localStorage.setItem("bl-aside", "open");
  applyAsideCollapsed();
});
applyAsideCollapsed();

// ---------- 会话列表 ----------
async function loadConversations() {
  const convs = await invoke("list_conversations");
  const list = $("conv-list");
  list.innerHTML = "";
  for (const c of convs) {
    if (c.kind === "peer") state.peerConvId = c.id;
    const item = document.createElement("div");
    item.className = "list-item conv-item";
    item.innerHTML = `<span class="li-main"><span class="li-title"><span>${esc(c.title)}</span></span><span class="li-sub">${c.kind === "ai" ? "AI 会话" : "设备会话"}</span></span>`;
    item.addEventListener("click", () => switchView(c.kind === "ai" ? "ai" : "chat"));
    list.appendChild(item);
  }
}

// ---------- 聊天 ----------
async function loadChatMessages() {
  if (!state.peerConvId) return;
  await refreshTagRibbon();
  const msgs = await invoke("list_messages", { conversationId: state.peerConvId, limit: 200 });
  const container = $("messages");
  container.innerHTML = "";
  state.renderedChat.clear();
  for (const m of msgs) appendMessage(container, m, state.renderedChat, {});
  // 恢复草稿
  const convs = await invoke("list_conversations");
  const conv = convs.find((c) => c.id === state.peerConvId);
  if (conv && conv.draft) $("input").value = conv.draft;
  refreshPinBar();
  rebuildDateRail();
}

/** 日期锚点栏：列出当前会话出现过的日期，点击滚到当天第一条消息。 */
function rebuildDateRail() {
  const rail = $("date-rail");
  if (!rail) return;
  const seen = new Map();
  for (const row of document.querySelectorAll("#messages .msg-row")) {
    const ts = Number(row.dataset.ts || 0);
    if (!ts) continue;
    const key = new Date(ts).toDateString();
    if (!seen.has(key)) seen.set(key, { ts, mid: row.dataset.mid });
  }
  rail.innerHTML = "";
  if (seen.size < 2) return;
  for (const { ts, mid } of seen.values()) {
    const b = document.createElement("button");
    b.textContent = fmtDay(ts);
    b.addEventListener("click", () => {
      const el = document.querySelector(`#messages [data-mid="${mid}"]`);
      if (el) el.scrollIntoView({ block: "center", behavior: "smooth" });
    });
    rail.appendChild(b);
  }
}

async function refreshPinBar() {
  try {
    const pins = await invoke("list_marked", { kind: "pin" });
    const mine = pins.filter((m) => m.conversation_id === state.peerConvId);
    const bar = $("pin-bar");
    if (!mine.length) { bar.classList.add("hidden"); return; }
    bar.classList.remove("hidden");
    bar.innerHTML = `${iconUse("i-pin", "ico ico-14")}<span>置顶：${esc(mine[0].body.slice(0, 40))}${mine.length > 1 ? `（共 ${mine.length} 条）` : ""}</span>`;
    bar.onclick = () => locateMessage(mine[0].conversation_id, mine[0].id);
  } catch (_) { /* 忽略 */ }
}

function setReply(m) {
  state.replyTo = m.id;
  const bar = $("reply-bar");
  bar.classList.remove("hidden");
  bar.innerHTML = `<span>引用：${esc(m.body.slice(0, 50))}</span><button class="btn small-btn" id="reply-cancel">取消</button>`;
  $("reply-cancel").addEventListener("click", () => {
    state.replyTo = null;
    bar.classList.add("hidden");
  });
  $("input").focus();
}

// ---------- 代码/长文本识别：自动转成对应后缀的文档发送 ----------
let docSuggestion = null; // {name, kind: "code" | "long"}
let docDismissed = false; // 用户对当前内容选择了「仍按文本发送」

const FENCE_EXT = { rust: "rs", rs: "rs", python: "py", py: "py", javascript: "js", js: "js", typescript: "ts", ts: "ts", go: "go", java: "java", c: "c", cpp: "cpp", "c++": "cpp", json: "json", html: "html", css: "css", markdown: "md", md: "md", shell: "sh", bash: "sh", sh: "sh", text: "txt", txt: "txt" };
const CODE_SIGNALS = [
  { re: /^\s*(pub\s+)?(async\s+)?fn\s+\w+\s*\(|^\s*use\s+[\w:]+;|\w+\s*->\s*[\w&]+\s*\{?\s*$/, name: "代码片段.rs" },
  { re: /^\s*def\s+\w+\s*\(|^\s*(import|from)\s+[\w.]+|^\s*class\s+\w+.*:\s*$/, name: "代码片段.py" },
  { re: /^\s*(function|const|let|var)\s+\w+|=>\s*\{|console\.log\(/, name: "代码片段.js" },
  { re: /^\s*package\s+[\w.]+;|^\s*public\s+(final\s+)?(class|static)/, name: "代码片段.java" },
  { re: /^#include\s*[<"]|^\s*int\s+main\s*\(/, name: "代码片段.c" },
  { re: /^\s*package\s+main\b|^\s*func\s+\w+\s*\(/, name: "代码片段.go" },
];

/** 识别输入内容是否应转文档：围栏代码块 / 代码特征 / 超长纯文本。返回 {name, kind} 或 null。 */
function detectDocument(text) {
  const t = text.trim();
  if (t.length < 60) return null;
  const fence = t.match(/^```([a-zA-Z0-9+#-]*)\s*\n/);
  if (fence) {
    const ext = FENCE_EXT[(fence[1] || "txt").toLowerCase()] || "txt";
    return { name: "代码片段." + ext, kind: "code" };
  }
  const lines = t.split("\n");
  if (lines.length >= 4) {
    let codeLines = 0;
    let firstName = null;
    for (const line of lines) {
      for (const s of CODE_SIGNALS) {
        if (s.re.test(line)) { codeLines++; if (!firstName) firstName = s.name; break; }
      }
    }
    const braceLines = lines.filter((l) => /[{};]\s*$/.test(l.trim())).length;
    if (codeLines >= 2 || (codeLines >= 1 && braceLines >= 3)) {
      return { name: firstName || "代码片段.txt", kind: "code" };
    }
  }
  if (t.length > 1000) return { name: "长文.md", kind: "long" };
  return null;
}

function updateDocBar() {
  docSuggestion = docDismissed ? null : detectDocument($("input").value);
  if (docSuggestion) {
    $("doc-bar-text").textContent = docSuggestion.kind === "code"
      ? `检测到代码，将作为文件发送：${docSuggestion.name}`
      : `内容较长，将作为文档发送：${docSuggestion.name}`;
    $("doc-bar").classList.remove("hidden");
  } else {
    $("doc-bar").classList.add("hidden");
  }
}
$("doc-bar-cancel").addEventListener("click", () => { docDismissed = true; updateDocBar(); });
$("input").addEventListener("paste", () => setTimeout(updateDocBar, 0));

async function sendCurrent() {
  const raw = $("input").value;
  const text = raw.trim();
  if (!text) return;
  const asDoc = docSuggestion;
  const replyTo = state.replyTo;
  $("input").value = "";
  docDismissed = false;
  updateDocBar();
  state.replyTo = null;
  $("reply-bar").classList.add("hidden");
  invoke("save_draft", { conversationId: state.peerConvId, text: "" }).catch(() => {});
  try {
    if (asDoc) {
      // 转文档发送：文件气泡由后端事件回推，与选文件发送一致
      await invoke("send_document", { name: asDoc.name, content: raw });
    } else {
      const m = await invoke("send_text", { text, replyTo });
      appendMessage($("messages"), m, state.renderedChat, {});
    }
  } catch (e) {
    alert("发送失败：" + e);
  }
}

$("btn-send").addEventListener("click", sendCurrent);
$("input").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendCurrent(); }
});
let typingTimer = null, draftTimer = null;
$("input").addEventListener("input", () => {
  updateDocBar();
  invoke("send_typing", { typing: true }).catch(() => {});
  clearTimeout(typingTimer);
  typingTimer = setTimeout(() => invoke("send_typing", { typing: false }).catch(() => {}), 1500);
  clearTimeout(draftTimer);
  draftTimer = setTimeout(() => {
    if (state.peerConvId) invoke("save_draft", { conversationId: state.peerConvId, text: $("input").value }).catch(() => {});
  }, 400);
});

$("btn-file").addEventListener("click", async () => {
  try {
    const path = await window.__TAURI__.dialog.open({ multiple: false, directory: false });
    if (!path) return;
    invoke("send_file_path", { path }).catch((e) => alert("文件发送失败：" + e));
  } catch (e) { alert("选择文件失败：" + e); }
});

$("btn-disconnect").addEventListener("click", async () => { await invoke("disconnect"); });

// ---------- 连接（蓝牙） ----------
async function refreshBtState() {
  try {
    const s = await invoke("bt_state");
    $("bt-status-text").textContent = s.message;
    $("bt-status-dot").className = "status-dot" + (s.advertising ? " on" : s.available ? " warn" : " off");
    if (!s.available) {
      $("btn-scan").disabled = true;
      $("bt-empty").textContent = "未检测到可用的蓝牙：请确认设备有蓝牙并已在系统中开启。";
    }
  } catch (_) { /* 忽略 */ }
}

let scanning = false;
$("btn-scan").addEventListener("click", async () => {
  $("connect-error").textContent = "";
  try {
    if (scanning) {
      await invoke("bt_stop_scan");
      scanning = false;
      $("btn-scan").innerHTML = `${iconUse("i-search", "ico ico-14")}扫描`;
      $("bt-empty").textContent = "点「扫描」查找附近打开了 BlueLink 的设备。";
    } else {
      await invoke("bt_start_scan");
      scanning = true;
      $("btn-scan").innerHTML = "停止";
      $("bt-empty").textContent = "正在扫描附近的设备…";
    }
  } catch (e) { $("connect-error").textContent = String(e); }
});

/** 渲染扫描到的设备列表（bt_devices 事件推送快照）。 */
function renderBtDevices(devices) {
  const list = $("bt-device-list");
  list.innerHTML = "";
  if (devices.length) $("bt-empty").textContent = "";
  else if (scanning) $("bt-empty").textContent = "正在扫描附近的设备…";
  for (const d of devices) {
    const row = document.createElement("div");
    row.className = "bt-device" + (state.btDevice === d.id ? " selected" : "");
    row.innerHTML = `<span class="bt-device-ico">${iconUse("i-bt", "ico ico-18")}</span>
      <span class="bt-device-main"><span class="bt-device-name">${esc(d.name)}</span>
      <span class="bt-device-sub">${d.rssi != null ? `信号 ${d.rssi} dBm` : "蓝牙设备"}</span></span>
      <button class="btn small-btn">连接</button>`;
    row.querySelector("button").addEventListener("click", () => {
      state.btDevice = d.id;
      state.btDeviceName = d.name;
      renderBtDevices(devices);
      $("bt-code-row").classList.remove("hidden");
      $("bt-peer-code").focus();
      $("connect-status").textContent = `已选择「${d.name}」，输入对方屏幕上的连接码后点连接。`;
    });
    list.appendChild(row);
  }
}

// ---------- 开发者模式（局域网测试通道，正式版默认隐藏） ----------
function applyDevMode(on) {
  state.devMode = on;
  $("dev-lan-section").classList.toggle("hidden", !on);
  const t = $("dev-mode-toggle");
  if (t) t.checked = on;
}
$("dev-mode-toggle").addEventListener("change", async (e) => {
  await invoke("set_dev_mode", { on: e.target.checked });
  applyDevMode(e.target.checked);
});
$("btn-listen").addEventListener("click", async () => {
  $("connect-error").textContent = "";
  try {
    const addr = await invoke("start_listening", { port: 7800 });
    $("connect-status").textContent = `正在等待局域网连接（${addr}）…`;
  } catch (e) { $("connect-error").textContent = String(e); }
});
$("btn-connect").addEventListener("click", async () => {
  $("connect-error").textContent = "";
  const addr = $("peer-addr").value.trim();
  const code = $("peer-code").value.trim();
  if (!addr || !code) { $("connect-error").textContent = "请填写对方地址和连接码"; return; }
  $("connect-status").textContent = "正在连接…";
  try {
    await invoke("connect_to", { addr, code });
  } catch (e) {
    $("connect-error").textContent = String(e);
    $("connect-status").textContent = "";
  }
});

$("btn-bt-connect").addEventListener("click", async () => {
  $("connect-error").textContent = "";
  const code = $("bt-peer-code").value.trim();
  if (!state.btDevice) { $("connect-error").textContent = "请先在列表中选择一台设备"; return; }
  if (!code) { $("connect-error").textContent = "请输入对方的连接码"; return; }
  $("connect-status").textContent = `正在通过蓝牙连接「${state.btDeviceName || "对方设备"}」…`;
  try {
    await invoke("bt_connect", { deviceId: state.btDevice, code });
    if (scanning) { await invoke("bt_stop_scan").catch(() => {}); scanning = false; }
  } catch (e) {
    $("connect-error").textContent = String(e);
    $("connect-status").textContent = "";
  }
});

// ---------- 标签 ----------
async function loadTagList() {
  const tags = await invoke("list_tags");
  const list = $("tag-list");
  list.innerHTML = "";
  if (!tags.length) {
    list.innerHTML = `<div class="list-item"><div class="li-sub">还没有标签。在任意消息上点“标签”即可添加。</div></div>`;
    return;
  }
  for (const t of tags) {
    const item = document.createElement("div");
    item.className = "list-item";
    item.innerHTML = `<div class="li-title"><span><span class="tag-dot" style="background:${tagColorHex(t.color) || "var(--muted)"}"></span>${esc(t.name)}</span><span class="muted">${t.count}</span></div>`;
    item.addEventListener("click", () => openTagDrawer(t.name));
    list.appendChild(item);
  }
}

async function openTagDrawer(tagName) {
  const msgs = await invoke("list_messages_by_tag", { tag: tagName });
  $("drawer-title").textContent = `标签：${tagName}（${msgs.length}）`;
  const list = $("drawer-list");
  list.innerHTML = "";
  for (const m of msgs) {
    const item = document.createElement("div");
    item.className = "list-item";
    item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(m.body.slice(0, 80))}</div>`;
    item.addEventListener("click", () => locateMessage(m.conversation_id, m.id));
    list.appendChild(item);
  }
  $("drawer").classList.remove("hidden");
}
function closeDrawer() { $("drawer").classList.add("hidden"); }
$("drawer-close").addEventListener("click", closeDrawer);

async function toggleMark(mid, kind) {
  try {
    const on = await invoke("toggle_mark", { messageId: mid, kind });
    if (kind === "pin") refreshPinBar();
    if (kind === "favorite" && state.view === "favorites") loadFavList();
    return on;
  } catch (e) { alert("操作失败：" + e); }
}

async function loadFavList() {
  const msgs = await invoke("list_marked", { kind: "favorite" });
  const list = $("fav-list");
  list.innerHTML = "";
  if (!msgs.length) {
    list.innerHTML = `<div class="list-item"><div class="li-sub">还没有收藏。在任意消息上点“收藏”即可。</div></div>`;
    return;
  }
  for (const m of msgs) {
    const item = document.createElement("div");
    item.className = "list-item";
    item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(m.body.slice(0, 80))}</div>`;
    item.addEventListener("click", () => locateMessage(m.conversation_id, m.id));
    list.appendChild(item);
  }
}

// ---------- 标签编辑浮层 ----------
async function openTagEditor(mid) {
  state.tagEditId = mid;
  state.tagEditList = await invoke("get_message_tags", { messageId: mid });
  renderTagChips();
  const pop = $("tag-pop");
  pop.classList.remove("hidden");
  pop.style.top = "80px";
  pop.style.right = "380px";
  $("tag-input").value = "";
  $("tag-input").focus();
}
function renderTagChips() {
  const box = $("tag-chips");
  box.innerHTML = "";
  state.tagEditList.forEach((t, i) => {
    const chip = document.createElement("span");
    chip.className = "tag-chip";
    chip.innerHTML = `${esc(t)} <button>×</button>`;
    chip.querySelector("button").addEventListener("click", () => {
      state.tagEditList.splice(i, 1);
      renderTagChips();
    });
    box.appendChild(chip);
  });
}
$("tag-input").addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  const v = $("tag-input").value.trim();
  if (!v) return;
  if (state.tagEditList.length >= 10) { alert("一条消息最多 10 个标签"); return; }
  if (!state.tagEditList.some((t) => t.toLowerCase() === v.toLowerCase())) state.tagEditList.push(v);
  $("tag-input").value = "";
  renderTagChips();
});
$("tag-cancel").addEventListener("click", () => $("tag-pop").classList.add("hidden"));
$("tag-save").addEventListener("click", async () => {
  await invoke("set_message_tags", { messageId: state.tagEditId, tags: state.tagEditList });
  $("tag-pop").classList.add("hidden");
  // 刷新该消息的标签展示与标签列表
  const row = document.querySelector(`[data-mid="${state.tagEditId}"] .msg-tags`);
  if (row) refreshMessageTags(state.tagEditId, row);
  if (state.view === "tags") loadTagList();
  refreshTagRibbon();
});

// ---------- 聊天页右上角标签彩签 ----------
// 调色板序号与存储层 tags.color 对应：0 默认，其余为彩签颜色。
const TAG_PALETTE = [null, "#E11D48", "#EA580C", "#CA8A04", "#059669", "#0891B2", "#2563EB", "#7C3AED", "#DB2777"];
function tagColorHex(idx) { return TAG_PALETTE[idx || 0] || null; }

state.tagColorMap = {};  // 标签名（小写）→ 颜色 hex 或 null，供消息内标签小签着色
state.ribbonMsgs = {};   // 标签名 → 本会话内消息 id 数组（时间正序）
state.ribbonCycle = {};  // 标签名 → 已定位次数（连点轮换）

/** 刷新彩签：只显示当前设备会话里出现过的标签；平时半透明不挡聊天，悬浮时凸显；点签定位，连点往前轮换。 */
async function refreshTagRibbon() {
  const ribbon = $("tag-ribbon");
  if (!ribbon) return;
  ribbon.innerHTML = "";
  state.ribbonMsgs = {};
  if (!state.peerConvId) return;
  let tags = [];
  try { tags = await invoke("list_tags"); } catch (_) { return; }
  state.tagColorMap = {};
  for (const t of tags) state.tagColorMap[t.name.toLowerCase()] = tagColorHex(t.color);
  for (const t of tags) {
    if (!t.count) continue;
    let msgs = [];
    try { msgs = await invoke("list_messages_by_tag", { tag: t.name }); } catch (_) { continue; }
    const ids = msgs.filter((m) => m.conversation_id === state.peerConvId).map((m) => m.id);
    if (!ids.length) continue;
    state.ribbonMsgs[t.name] = ids;
    const hex = tagColorHex(t.color);
    const tab = document.createElement("div");
    tab.className = "ribbon-tab";
    if (hex) tab.style.borderLeft = `3px solid ${hex}`;
    const dot = document.createElement("button");
    dot.className = "r-dot";
    dot.title = "设置标签颜色";
    dot.style.background = hex || "var(--muted)";
    dot.addEventListener("click", (e) => { e.stopPropagation(); openTagColorPop(t, tab); });
    const name = document.createElement("span");
    name.className = "r-name";
    name.textContent = t.name;
    const count = document.createElement("span");
    count.className = "r-count";
    count.textContent = ids.length;
    tab.append(dot, name, count);
    tab.title = `定位到「${t.name}」标记的消息（连点往前轮换）`;
    tab.addEventListener("click", () => {
      const list = state.ribbonMsgs[t.name] || [];
      if (!list.length) return;
      const n = (state.ribbonCycle[t.name] || 0) + 1;
      state.ribbonCycle[t.name] = n;
      const id = list[list.length - 1 - ((n - 1) % list.length)];
      locateMessage(state.peerConvId, id);
    });
    ribbon.appendChild(tab);
  }
}

/** 标签颜色选择弹层：默认 + 8 色，点选即保存，并刷新彩签与已渲染消息的标签着色。 */
function openTagColorPop(tag, anchor) {
  const pop = $("tag-color-pop");
  pop.innerHTML = "";
  const title = document.createElement("div");
  title.className = "pop-title";
  title.textContent = `「${tag.name}」的颜色`;
  pop.appendChild(title);
  const row = document.createElement("div");
  row.className = "swatches";
  TAG_PALETTE.forEach((hex, idx) => {
    const b = document.createElement("button");
    b.className = "swatch" + (idx === (tag.color || 0) ? " sel" : "") + (idx === 0 ? " default" : "");
    if (hex) b.style.background = hex; else b.textContent = "默";
    b.title = idx === 0 ? "默认" : hex;
    b.addEventListener("click", async () => {
      try { await invoke("set_tag_color", { tag: tag.name, color: idx }); }
      catch (e) { alert("设置颜色失败：" + e); }
      pop.classList.add("hidden");
      refreshTagRibbon();
      document.querySelectorAll(".msg-row").forEach((r) => {
        const span = r.querySelector(".msg-tags");
        if (span && r.dataset.mid) refreshMessageTags(r.dataset.mid, span);
      });
    });
    row.appendChild(b);
  });
  pop.appendChild(row);
  const view = $("chat-view").getBoundingClientRect();
  const r = anchor.getBoundingClientRect();
  pop.style.top = Math.max(64, r.top - view.top) + "px";
  pop.classList.remove("hidden");
}
// 点到弹层之外时关闭
document.addEventListener("click", (e) => {
  const pop = $("tag-color-pop");
  if (pop && !pop.classList.contains("hidden") && !pop.contains(e.target)) pop.classList.add("hidden");
});

// ---------- 搜索 ----------
let searchTimer = null;
$("search-input").addEventListener("input", () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(async () => {
    const q = $("search-input").value.trim();
    const box = $("search-results");
    if (!q) { box.classList.add("hidden"); box.innerHTML = ""; return; }
    const msgs = await invoke("search_messages", { query: q });
    box.innerHTML = "";
    box.classList.remove("hidden");
    if (!msgs.length) box.innerHTML = `<div class="list-item"><div class="li-sub">没有找到包含“${esc(q)}”的消息</div></div>`;
    for (const m of msgs.slice(0, 50)) {
      const item = document.createElement("div");
      item.className = "list-item";
      item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(m.body.slice(0, 80))}</div>`;
      item.addEventListener("click", () => locateMessage(m.conversation_id, m.id));
      box.appendChild(item);
    }
  }, 300);
});

// ---------- AI ----------
async function loadAiMessages() {
  const msgs = await invoke("list_messages", { conversationId: state.aiConvId, limit: 200 });
  const container = $("ai-messages");
  container.innerHTML = "";
  state.renderedAi.clear();
  for (const m of msgs) appendMessage(container, m, state.renderedAi, { ai: true });
}

async function sendAi() {
  const text = $("ai-input").value.trim();
  if (!text || state.aiGenerating) return;
  $("ai-input").value = "";
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  try {
    await invoke("ai_send", { text });
  } catch (e) {
    alert("AI 提问失败：" + e);
  } finally {
    state.aiGenerating = false;
    state.streamEl = null;
    $("btn-ai-stop").classList.add("hidden");
  }
}
async function regenerateAi() {
  if (state.aiGenerating) return;
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  try {
    await invoke("ai_regenerate");
    await loadAiMessages();
  } catch (e) { alert("重新生成失败：" + e); }
  state.aiGenerating = false;
  state.streamEl = null;
  $("btn-ai-stop").classList.add("hidden");
}
$("btn-ai-send").addEventListener("click", sendAi);
$("ai-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendAi(); }
});
$("btn-ai-stop").addEventListener("click", () => invoke("ai_stop"));

// ---------- 设置 ----------
async function loadSettings() {
  const id = state.identity || (await invoke("get_identity"));
  $("set-name").textContent = id.name;
  $("set-code").textContent = id.code_display;
  $("set-dl").textContent = id.download_dir;
  const cfg = await invoke("get_ai_config");
  $("ai-base").value = cfg.base_url || "";
  $("ai-model").value = cfg.model || "";
  $("ai-key").placeholder = cfg.has_key ? "已配置（留空不修改）" : "sk-…";
}
$("btn-ai-save").addEventListener("click", async () => {
  await invoke("set_ai_config", {
    baseUrl: $("ai-base").value.trim(),
    apiKey: $("ai-key").value,
    model: $("ai-model").value.trim(),
  });
  $("ai-key").value = "";
  $("ai-test-result").textContent = "已保存";
});
$("btn-ai-test").addEventListener("click", async () => {
  $("ai-test-result").textContent = "测试中…";
  try {
    $("ai-test-result").textContent = await invoke("test_ai_connection");
  } catch (e) { $("ai-test-result").textContent = String(e); }
});


// ---------- 导出 ----------
/** 导出当前设备会话为 Markdown 文本文件（路径由保存对话框选定）。 */
async function exportCurrentConv() {
  if (!state.peerConvId) { alert("还没有可导出的会话"); return; }
  const msgs = await invoke("list_messages", { conversationId: state.peerConvId, limit: 1000 });
  const lines = ["# BlueLink 聊天记录", "", `导出时间：${new Date().toLocaleString("zh-CN")}`, ""];
  for (const m of msgs) {
    const who = m.sender === "me" ? "我" : "对方";
    lines.push(`- [${fmtDay(m.created_at)} ${fmtTime(m.created_at)}] ${who}：${m.body}`);
  }
  const path = await window.__TAURI__.dialog.save({ defaultPath: "bluelink-chat.md", filters: [{ name: "Markdown", extensions: ["md"] }] });
  if (!path) return;
  await invoke("save_text_file", { path, content: lines.join("\n") });
}
$("btn-export").addEventListener("click", () => exportCurrentConv().catch((e) => alert("导出失败：" + e)));

// ---------- 命令面板（Ctrl+K） ----------
const paletteState = { items: [], sel: 0 };
async function refreshPalette(q) {
  const items = [];
  for (const [v, label] of [["chat", "聊天"], ["ai", "AI 对话"], ["tags", "标签总览"], ["favorites", "收藏"], ["settings", "设置"]])
    items.push({ label: `打开：${label}`, run: () => switchView(v) });
  if (state.connected) items.push({ label: "断开连接", run: () => $("btn-disconnect").click() });
  if (state.peerConvId) items.push({ label: "导出当前会话", run: () => exportCurrentConv() });
  try {
    for (const t of await invoke("list_tags"))
      items.push({ label: `标签：${t.name}（${t.count} 条）`, run: () => { switchView("tags"); openTagDrawer(t.name); } });
  } catch (_) { /* 忽略 */ }
  paletteState.items = items.filter((it) => !q || it.label.includes(q));
  paletteState.sel = 0;
  renderPalette();
}
function renderPalette() {
  const list = $("palette-list");
  list.innerHTML = "";
  paletteState.items.forEach((it, i) => {
    const d = document.createElement("div");
    d.className = "palette-item" + (i === paletteState.sel ? " sel" : "");
    d.textContent = it.label;
    d.addEventListener("click", () => { closePalette(); it.run(); });
    list.appendChild(d);
  });
}
function openPalette() {
  $("palette").classList.remove("hidden");
  $("palette-input").value = "";
  refreshPalette("");
  $("palette-input").focus();
}
function closePalette() { $("palette").classList.add("hidden"); }
$("palette-input").addEventListener("input", (e) => refreshPalette(e.target.value.trim()));
$("palette-input").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); paletteState.sel = Math.min(paletteState.sel + 1, paletteState.items.length - 1); renderPalette(); }
  else if (e.key === "ArrowUp") { e.preventDefault(); paletteState.sel = Math.max(paletteState.sel - 1, 0); renderPalette(); }
  else if (e.key === "Enter") { const it = paletteState.items[paletteState.sel]; if (it) { closePalette(); it.run(); } }
  else if (e.key === "Escape") closePalette();
});
$("palette").addEventListener("click", (e) => { if (e.target === $("palette")) closePalette(); });
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openPalette(); }
});

// ---------- 后端事件 ----------
async function wireEvents() {
  await listen("bt_devices", (e) => renderBtDevices(e.payload || []));
  await listen("connection_state", async (e) => {
    state.connected = e.payload.connected;
    state.peerName = e.payload.peer_name;
    $("peer-name").textContent = state.peerName || "未连接";
    $("peer-state").textContent = state.connected ? "已连接" : "离线";
    $("connect-status").textContent = state.connected ? "" : $("connect-status").textContent;
    await loadConversations();
    if (state.connected) { switchView("chat"); loadChatMessages(); }
    else switchView(state.view);
  });
  await listen("message_new", (e) => {
    const m = e.payload;
    if (m.conversation_id === state.aiConvId) appendMessage($("ai-messages"), m, state.renderedAi, { ai: true });
    else { appendMessage($("messages"), m, state.renderedChat, {}); rebuildDateRail(); }
  });
  await listen("message_status", (e) => {
    const row = document.querySelector(`[data-mid="${e.payload.id}"] .msg-status`);
    if (row) row.textContent = STATUS_CN[e.payload.status] || "";
  });
  await listen("peer_typing", (e) => {
    $("typing-hint").classList.toggle("hidden", !e.payload);
  });
  await listen("transfer_progress", (e) => {
    let b = null;
    if (e.payload.msg_id) {
      const row = document.querySelector(`[data-mid="${e.payload.msg_id}"]`);
      if (row) b = row.querySelector(".bubble");
    }
    if (!b) b = state.lastFileBubble;
    if (!b) return;
    const bar = b.querySelector(".progress");
    const fill = b.querySelector(".progress > div");
    if (bar && fill) {
      bar.classList.remove("hidden");
      fill.style.width = e.payload.total ? (e.payload.received / e.payload.total) * 100 + "%" : "0";
    }
  });
  await listen("transfer_state", (e) => {
    const b = state.lastFileBubble;
    if (b && e.payload.state === "done") {
      const bar = b.querySelector(".progress");
      if (bar) setTimeout(() => bar.classList.add("hidden"), 600);
    }
  });
  await listen("ai_chunk", (e) => {
    const container = $("ai-messages");
    if (!state.streamEl) {
      const row = document.createElement("div");
      row.className = "msg-row";
      row.innerHTML = `<div class="msg"><div class="bubble"></div></div>`;
      container.appendChild(row);
      state.streamEl = row.querySelector(".bubble");
    }
    state.streamEl.textContent += e.payload.delta;
    container.scrollTop = container.scrollHeight;
  });
  await listen("ai_done", (e) => {
    if (state.streamEl) { state.streamEl = null; }
    loadAiMessages();
  });
  await listen("ai_error", (e) => alert("AI 错误：" + e.payload.error));
}

// ---------- 主题 ----------
function applyTheme(mode) {
  if (mode === "system") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", mode);
  document.querySelectorAll("#theme-seg button").forEach((b) =>
    b.classList.toggle("active", b.dataset.themeSet === mode));
}
document.querySelectorAll("#theme-seg button").forEach((b) =>
  b.addEventListener("click", () => {
    localStorage.setItem("bl-theme", b.dataset.themeSet);
    applyTheme(b.dataset.themeSet);
  }));

// ---------- 启动 ----------
(async function init() {
  applyTheme(localStorage.getItem("bl-theme") || "system");
  state.identity = await invoke("get_identity");
  $("my-code").textContent = state.identity.code_display;
  $("my-name").textContent = state.identity.name;
  refreshBtState();
  invoke("get_dev_mode").then(applyDevMode).catch(() => {});
  // 事件订阅失败不能拖垮其余启动流程（v2.0.0 曾因缺权限配置导致实时事件全灭）。
  try { await wireEvents(); } catch (e) { console.error("事件订阅失败", e); }
  // 拖拽文件到窗口直接发送（需已连接）；拖放提示层只做视觉提示，不拦截事件。
  try {
    await window.__TAURI__.webview.getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload || {};
      if (p.type === "enter" || p.type === "over") {
        $("drop-overlay").classList.remove("hidden");
      } else if (p.type === "drop") {
        $("drop-overlay").classList.add("hidden");
        if (!state.connected) { alert("尚未连接设备，无法发送文件"); return; }
        for (const path of p.paths || []) {
          invoke("send_file_path", { path }).catch((err) => alert("文件发送失败：" + err));
        }
      } else {
        $("drop-overlay").classList.add("hidden");
      }
    });
  } catch (e) { console.error("拖拽监听注册失败", e); }
  await loadConversations();
  if (state.identity.connected) {
    state.connected = true;
    switchView("chat");
    loadChatMessages();
  } else {
    switchView("chat");
  }
})();
