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

const AVATAR_COLORS = ["#2563EB", "#0E7490", "#047857", "#B45309", "#BE123C", "#475569"];
function avatarColor(key) {
  let h = 0;
  for (const ch of String(key || "?")) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return AVATAR_COLORS[h % AVATAR_COLORS.length];
}
function iconUse(name, cls = "ico ico-16") {
  return `<svg class="${cls}"><use href="#${name}"/></svg>`;
}
/** 头像：AI 用机器人图标，本机用“我”，对端用名称首字 + 按名称派生的底色。 */
function buildAvatarEl(m, opts = {}) {
  const av = document.createElement("div");
  if (opts.ai && m.sender === "ai") {
    av.className = "avatar avatar-32 avatar-ai";
    av.innerHTML = iconUse("i-bot", "ico ico-18");
  } else if (m.sender === "me") {
    av.className = "avatar avatar-32 avatar-me";
    av.textContent = "我";
  } else {
    const name = state.peerName || "对方";
    av.className = "avatar avatar-32";
    av.style.background = avatarColor(name);
    av.textContent = name.slice(0, 1);
  }
  return av;
}

/** 构建一条消息的 DOM。opts.ai 表示 AI 会话样式。 */
function buildMessageEl(m, opts = {}) {
  const row = document.createElement("div");
  row.className = "msg-row" + (m.sender === "me" ? " me" : "");
  row.dataset.mid = m.id;
  row.dataset.sender = m.sender;
  row.appendChild(buildAvatarEl(m, opts));
  const wrap = document.createElement("div");
  wrap.className = "msg";
  const bubble = document.createElement("div");
  bubble.className = "bubble" + (m.kind === "file" ? " file-card" : "");
  if (m.kind === "file") {
    bubble.innerHTML = `<div class="file-card-inner"><span class="file-ico">${iconUse("i-file", "ico ico-20")}</span><span class="file-main"><span class="file-name">${esc(m.body)}</span><span class="file-meta">文件</span></span></div><div class="progress hidden"><div></div></div>`;
    state.lastFileBubble = bubble;
  } else if (opts.ai && m.sender === "ai") {
    bubble.innerHTML = md(m.body);
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
  // 消息分组：同日、同发送者、间隔 < 5 分钟时收起头像并压缩间距
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

// ---------- 会话列表 ----------
async function loadConversations() {
  const convs = await invoke("list_conversations");
  const list = $("conv-list");
  list.innerHTML = "";
  for (const c of convs) {
    if (c.kind === "peer") state.peerConvId = c.id;
    const item = document.createElement("div");
    item.className = "list-item conv-item";
    const avatar = c.kind === "ai"
      ? `<span class="avatar avatar-40 avatar-ai">${iconUse("i-bot", "ico ico-20")}</span>`
      : `<span class="avatar avatar-40" style="background:${avatarColor(c.title)}">${esc(c.title.slice(0, 1))}</span>`;
    item.innerHTML = `${avatar}<span class="li-main"><span class="li-title"><span>${esc(c.title)}</span></span><span class="li-sub">${c.kind === "ai" ? "AI 会话" : "设备会话"}</span></span>`;
    item.addEventListener("click", () => switchView(c.kind === "ai" ? "ai" : "chat"));
    list.appendChild(item);
  }
}

// ---------- 聊天 ----------
async function loadChatMessages() {
  if (!state.peerConvId) return;
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

async function sendCurrent() {
  const text = $("input").value.trim();
  if (!text) return;
  $("input").value = "";
  try {
    const m = await invoke("send_text", { text, replyTo: state.replyTo });
    appendMessage($("messages"), m, state.renderedChat, {});
    state.replyTo = null;
    $("reply-bar").classList.add("hidden");
    invoke("save_draft", { conversationId: state.peerConvId, text: "" }).catch(() => {});
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
    item.innerHTML = `<div class="li-title"><span><span class="tag-dot"></span>${esc(t.name)}</span><span class="muted">${t.count}</span></div>`;
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
    const pav = $("peer-avatar");
    if (state.peerName) {
      pav.textContent = state.peerName.slice(0, 1);
      pav.style.background = avatarColor(state.peerName);
    }
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
      row.innerHTML = `<div class="avatar avatar-32 avatar-ai">${iconUse("i-bot", "ico ico-18")}</div><div class="msg"><div class="bubble"></div></div>`;
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
  await loadConversations();
  if (state.identity.connected) {
    state.connected = true;
    switchView("chat");
    loadChatMessages();
  } else {
    switchView("chat");
  }
})();
