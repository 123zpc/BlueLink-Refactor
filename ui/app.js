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
  categories: [],
  catEditId: null,
  stagedImage: null,
  transferMap: {},
  quoteFrom: null,
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
  row.dataset.kind = m.kind;
  const wrap = document.createElement("div");
  wrap.className = "msg";
  const bubble = document.createElement("div");
  bubble.className = "bubble" + (m.kind === "file" ? " file-card" : "");
  if (m.reply_to && m.kind === "text" && !opts.ai) {
    const q = document.createElement("div");
    q.className = "quote-block";
    q.textContent = "引用加载中…";
    q.addEventListener("click", () => jumpToQuoted(m));
    bubble.appendChild(q);
    fillQuote(q, m);
  }
  if (m.kind === "file") {
    bubble.innerHTML = `<div class="file-card-inner"><span class="file-ico">${iconUse("i-file", "ico ico-20")}</span><span class="file-main"><span class="file-name">${esc(m.body)}</span><span class="file-meta">文件</span></span></div><div class="progress hidden"><div></div></div><div class="progress-meta hidden"></div><button class="progress-cancel hidden">取消</button><button class="transfer-retry hidden">重试</button>`;
    state.lastFileBubble = bubble;
    bubble.addEventListener("click", (e) => {
      if (e.target.closest("button")) return;
      openFileMenu(m.id, e.clientX, e.clientY);
    });
    bubble.querySelector(".progress-cancel").addEventListener("click", async () => {
      const tid = state.transferMap[m.id];
      if (tid) await invoke("cancel_transfer", { transferId: tid });
    });
    bubble.querySelector(".transfer-retry").addEventListener("click", async () => {
      try {
        await invoke("resend_message", { messageId: m.id });
        row.remove();
        state.renderedChat.delete(m.id);
      } catch (err) { alert("重发失败：" + err); }
    });
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
    const tspan = document.createElement("span");
    tspan.textContent = m.body;
    bubble.appendChild(tspan);
  }
  const meta = document.createElement("div");
  meta.className = "msg-meta";
  const tagsSpan = document.createElement("span");
  tagsSpan.className = "msg-tags";
  meta.appendChild(tagsSpan);
  const status = document.createElement("span");
  status.className = "msg-status";
  applyStatus(status, m.id, m.status);
  const time = document.createElement("span");
  time.textContent = fmtTime(m.created_at);
  const actions = document.createElement("span");
  actions.className = "msg-actions";
  const btns = [["彩签", (e) => openCatPop(m.id, e.currentTarget)], ["收藏", () => toggleMark(m.id, "favorite")], ["置顶", () => toggleMark(m.id, "pin")]];
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
  refreshMessageCategory(m.id, tagsSpan);
  return row;
}

/** 消息状态展示：失败时标红可点重试（文本原地重发；文件重发会生成新气泡并移除旧的）。 */
function applyStatus(el, mid, status) {
  el.textContent = STATUS_CN[status] || "";
  el.classList.toggle("failed", status === "failed");
  el.title = status === "failed" ? "发送失败，点击重试" : "";
  el.onclick = status === "failed" ? async () => {
    try {
      const rowEl = document.querySelector(`[data-mid="${mid}"]`);
      await invoke("resend_message", { messageId: mid });
      if (rowEl && rowEl.dataset.kind === "file") {
        rowEl.remove();
        state.renderedChat.delete(mid);
      }
    } catch (err) { alert("重试失败：" + err); }
  } : null;
}

// ---------- 引用 ----------
const quoteCache = new Map();
async function fillQuote(el, m) {
  try {
    let orig = quoteCache.get(m.reply_to);
    if (!orig) {
      orig = await invoke("get_message", { messageId: m.reply_to });
      if (orig) quoteCache.set(m.reply_to, orig);
    }
    if (!orig) { el.textContent = "原消息已删除"; return; }
    const who = orig.sender === "me" ? "我" : orig.sender === "ai" ? "AI" : "对方";
    el.textContent = `${who}：${orig.body.slice(0, 80)}`;
  } catch (_) { el.textContent = "引用消息"; }
}

/** 点引用块跳到原消息，并亮出「回到回复」按钮一键跳回。 */
function jumpToQuoted(m) {
  state.quoteFrom = m.id;
  locateMessage(m.conversation_id, m.reply_to);
  const btn = $("back-to-reply");
  btn.classList.remove("hidden");
  btn.onclick = () => {
    btn.classList.add("hidden");
    if (state.quoteFrom) locateMessage(m.conversation_id, state.quoteFrom);
    state.quoteFrom = null;
  };
}

// 彩签分类：颜色即分类，序号 0–8 与存储层 categories.color 对应
const CAT_COLORS = ["#6B7280", "#E11D48", "#EA580C", "#CA8A04", "#059669", "#0891B2", "#2563EB", "#7C3AED", "#DB2777"];
const CAT_DEFAULT_NAMES = ["灰色", "红色", "橙色", "黄色", "绿色", "青色", "蓝色", "紫色", "粉色"];
function catName(color) {
  const c = state.categories.find((x) => x.color === color);
  return c ? c.name : CAT_DEFAULT_NAMES[color] || "分类";
}
async function loadCategories() {
  try { state.categories = await invoke("list_categories"); } catch (_) { state.categories = []; }
  return state.categories;
}

/** 刷新一条消息的彩签小片：备注优先展示，否则显示分类名；点小片改分类，右键给分类改名。 */
async function refreshMessageCategory(mid, span) {
  try {
    const mc = await invoke("get_message_category", { messageId: mid });
    span.innerHTML = "";
    if (!mc) return;
    const hex = CAT_COLORS[mc.color] || CAT_COLORS[0];
    const chip = document.createElement("span");
    chip.className = "tag-chip";
    chip.textContent = mc.note || catName(mc.color);
    chip.style.borderColor = hex;
    chip.style.color = hex;
    chip.title = `分类：${catName(mc.color)}（点击修改，右键改名）`;
    chip.addEventListener("click", (e) => openCatPop(mid, e.currentTarget));
    chip.addEventListener("contextmenu", (e) => { e.preventDefault(); openRenamePop(mc.color, e.currentTarget); });
    span.appendChild(chip);
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
  if (view === "tags") loadCategoryList();
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
  await refreshRibbon();
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
  if (t.length > 1000) return { name: "长文.txt", kind: "long" };
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
// 粘贴格式优先级（重要）：Excel 等复制时剪贴板同时带文字与图片两种格式，
// 必须优先取文字——粘进来就是文字，不超限直接按文字发送，绝不能变成图片；
// 将来做图片粘贴时，图片分支也只处理剪贴板里没有文字的纯图片（如截图）。
$("input").addEventListener("paste", () => setTimeout(updateDocBar, 0));
// 粘贴图片：仅当剪贴板里没有文字时才走图片分支（带文字的粘贴永远优先文字，见上方注释）。
$("input").addEventListener("paste", async (e) => {
  const cd = e.clipboardData;
  if (!cd) return;
  const text = cd.getData("text/plain");
  if (text && text.trim()) return;
  const item = Array.from(cd.items || []).find((i) => i.kind === "file" && i.type.startsWith("image/"));
  if (!item) return;
  e.preventDefault();
  const file = item.getAsFile();
  if (!file) return;
  try {
    const buf = await file.arrayBuffer();
    const bytes = new Uint8Array(buf);
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    const ext = (file.type.split("/")[1] || "png").replace("jpeg", "jpg");
    const path = await invoke("stage_pasted_image", { dataBase64: btoa(bin), ext });
    state.stagedImage = { path, size: file.size };
    $("attach-text").textContent = `粘贴的图片.${ext} · ${fmtSize(file.size)}（点发送时一并发给对方）`;
    $("attach-bar").classList.remove("hidden");
  } catch (err) { alert("粘贴图片失败：" + err); }
});
$("attach-remove").addEventListener("click", () => {
  state.stagedImage = null;
  $("attach-bar").classList.add("hidden");
});

async function sendCurrent() {
  const raw = $("input").value;
  const text = raw.trim();
  if (!text && !state.stagedImage) return;
  // 暂存的粘贴图片先发（文件气泡由后端事件回推，与选文件发送一致）
  if (state.stagedImage) {
    const img = state.stagedImage;
    state.stagedImage = null;
    $("attach-bar").classList.add("hidden");
    if (!state.connected) {
      alert("尚未连接设备，图片未发送");
    } else {
      invoke("send_file_path", { path: img.path }).catch((e) => alert("图片发送失败：" + e));
    }
    if (!text) {
      $("input").value = "";
      docDismissed = false;
      updateDocBar();
      return;
    }
  }
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

// ---------- 彩签总览 ----------
async function loadCategoryList() {
  await loadCategories();
  const list = $("tag-list");
  list.innerHTML = "";
  for (const c of state.categories) {
    const item = document.createElement("div");
    item.className = "list-item";
    item.innerHTML = `<div class="li-title"><span><span class="tag-dot" style="background:${CAT_COLORS[c.color]}"></span>${esc(c.name)}</span><span class="muted">${c.count}</span></div>`;
    item.addEventListener("click", () => openCategoryDrawer(c.color));
    list.appendChild(item);
  }
}

async function openCategoryDrawer(color) {
  const items = await invoke("list_messages_by_category", { color });
  $("drawer-title").textContent = `分类：${catName(color)}（${items.length}）`;
  const list = $("drawer-list");
  list.innerHTML = "";
  if (!items.length) {
    list.innerHTML = `<div class="list-item"><div class="li-sub">这个分类下还没有消息。在任意消息上点「彩签」即可归类。</div></div>`;
  }
  for (const it of items) {
    const m = it.message;
    const item = document.createElement("div");
    item.className = "list-item";
    item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(it.note || m.body.slice(0, 80))}</div>`;
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

// ---------- 彩签分类选择层 ----------
async function openCatPop(mid, anchor) {
  state.catEditId = mid;
  await loadCategories();
  const mc = await invoke("get_message_category", { messageId: mid }).catch(() => null);
  $("cat-note").value = mc && mc.note ? mc.note : "";
  renderCatGrid(mc ? mc.color : -1);
  const pop = $("cat-pop");
  pop.classList.remove("hidden");
  const r = anchor && anchor.getBoundingClientRect ? anchor.getBoundingClientRect() : null;
  if (r) {
    pop.style.top = Math.min(window.innerHeight - 330, r.bottom + 6) + "px";
    pop.style.left = Math.max(8, Math.min(window.innerWidth - 316, r.left - 120)) + "px";
  } else {
    pop.style.top = "80px";
    pop.style.left = "300px";
  }
}
function renderCatGrid(selected) {
  const grid = $("cat-grid");
  grid.innerHTML = "";
  for (const c of state.categories) {
    const b = document.createElement("button");
    b.className = "cat-swatch" + (c.color === selected ? " sel" : "");
    b.innerHTML = `<span class="cs-dot" style="background:${CAT_COLORS[c.color]}"></span><span class="cs-name">${esc(c.name)}</span>`;
    b.addEventListener("click", async () => {
      await invoke("set_message_category", {
        messageId: state.catEditId,
        color: c.color,
        note: $("cat-note").value.trim() || null,
      });
      $("cat-pop").classList.add("hidden");
      refreshAllCategoryUI(state.catEditId);
    });
    grid.appendChild(b);
  }
}
$("cat-clear").addEventListener("click", async () => {
  await invoke("set_message_category", { messageId: state.catEditId, color: null, note: null });
  $("cat-pop").classList.add("hidden");
  refreshAllCategoryUI(state.catEditId);
});
$("cat-cancel").addEventListener("click", () => $("cat-pop").classList.add("hidden"));
document.addEventListener("click", (e) => {
  const pop = $("cat-pop");
  if (pop && !pop.classList.contains("hidden") && !pop.contains(e.target)) pop.classList.add("hidden");
});
/** 归类变化后统一刷新：该消息小片、彩签条、总览（在该视图时）。 */
function refreshAllCategoryUI(mid) {
  if (mid) {
    const span = document.querySelector(`[data-mid="${mid}"] .msg-tags`);
    if (span) refreshMessageCategory(mid, span);
  }
  refreshRibbon();
  if (state.view === "tags") loadCategoryList();
}

// ---------- 文件菜单 ----------
async function openFileMenu(mid, x, y) {
  const menu = $("file-menu");
  let info = null;
  try { info = await invoke("file_info", { messageId: mid }); } catch (_) { /* 忽略 */ }
  const exists = !!(info && info.exists && info.path);
  menu.innerHTML = "";
  const items = [
    ["打开", exists, async () => { await invoke("open_file", { path: info.path }); }, ""],
    ["打开文件位置", exists, async () => { await invoke("reveal_file", { path: info.path }); }, ""],
    ["复制文件", exists, async () => {
      try {
        const resp = await fetch("file://" + encodeURI(info.path));
        const blob = await resp.blob();
        await navigator.clipboard.write([new ClipboardItem({ [blob.type || "application/octet-stream"]: blob })]);
        showToast("文件已复制到剪贴板");
      } catch (_) {
        await navigator.clipboard.writeText(info.path);
        showToast("此系统不支持复制文件本身，已复制文件路径");
      }
    }, ""],
    ["另存为…", exists, async () => {
      const dst = await window.__TAURI__.dialog.save({ defaultPath: info.name });
      if (dst) {
        await invoke("copy_file_as", { src: info.path, dst });
        showToast("已另存为");
      }
    }, ""],
    ["删除记录", true, async () => {
      await invoke("delete_message", { messageId: mid });
      const row = document.querySelector(`[data-mid="${mid}"]`);
      if (row) {
        row.remove();
        state.renderedChat.delete(mid);
        state.renderedAi.delete(mid);
      }
      showToast("记录已删除（文件本体保留）");
    }, "fm-danger"],
  ];
  for (const [label, enabled, fn, cls] of items) {
    const b = document.createElement("button");
    b.textContent = label;
    if (cls) b.className = cls;
    b.disabled = !enabled;
    b.addEventListener("click", async () => {
      menu.classList.add("hidden");
      try { await fn(); } catch (e) { alert("操作失败：" + e); }
    });
    menu.appendChild(b);
  }
  if (!exists) {
    const hint = document.createElement("div");
    hint.className = "li-sub";
    hint.style.padding = "4px 10px";
    hint.textContent = "文件尚未接收完成或已被移动";
    menu.appendChild(hint);
  }
  menu.style.left = Math.min(x, window.innerWidth - 190) + "px";
  menu.style.top = Math.min(y, window.innerHeight - 240) + "px";
  menu.classList.remove("hidden");
}
document.addEventListener("click", (e) => {
  const menu = $("file-menu");
  if (menu && !menu.classList.contains("hidden") && !menu.contains(e.target)) menu.classList.add("hidden");
});

// ---------- 聊天页右上角彩签（分类） ----------
// 平时半透明贴边；悬停某个彩签弹出该分类在本会话的条目列表（备注优先），点条目定位；
// 点彩签本体连点轮换定位；右键彩签给分类改名（改名全类同步）。
state.ribbonMsgs = {};   // 颜色 → 本会话内该分类条目（时间正序）
state.ribbonCycle = {};  // 颜色 → 已定位次数（连点轮换）
async function refreshRibbon() {
  const ribbon = $("tag-ribbon");
  if (!ribbon) return;
  ribbon.innerHTML = "";
  state.ribbonMsgs = {};
  if (!state.peerConvId) return;
  await loadCategories();
  for (const c of state.categories) {
    if (!c.count) continue;
    let items = [];
    try { items = await invoke("list_messages_by_category", { color: c.color }); } catch (_) { continue; }
    const mine = items.filter((it) => it.message.conversation_id === state.peerConvId);
    if (!mine.length) continue;
    state.ribbonMsgs[c.color] = mine;
    const tab = document.createElement("div");
    tab.className = "ribbon-tab";
    tab.style.borderLeft = `3px solid ${CAT_COLORS[c.color]}`;
    const name = document.createElement("span");
    name.className = "r-name";
    name.textContent = c.name;
    const count = document.createElement("span");
    count.className = "r-count";
    count.textContent = mine.length;
    tab.append(name, count);
    tab.title = `「${c.name}」：点击轮换定位，悬停看列表，右键改名`;
    tab.addEventListener("click", () => {
      const list = state.ribbonMsgs[c.color] || [];
      if (!list.length) return;
      const n = (state.ribbonCycle[c.color] || 0) + 1;
      state.ribbonCycle[c.color] = n;
      const it = list[list.length - 1 - ((n - 1) % list.length)];
      locateMessage(state.peerConvId, it.message.id);
    });
    tab.addEventListener("mouseenter", () => showRibbonList(c.color, tab));
    tab.addEventListener("mouseleave", () => scheduleHideRibbonList());
    tab.addEventListener("contextmenu", (e) => { e.preventDefault(); openRenamePop(c.color, tab); });
    ribbon.appendChild(tab);
  }
}

let ribbonHideTimer = null;
function scheduleHideRibbonList() {
  ribbonHideTimer = setTimeout(() => $("ribbon-list").classList.add("hidden"), 250);
}
function showRibbonList(color, tab) {
  clearTimeout(ribbonHideTimer);
  const box = $("ribbon-list");
  const items = state.ribbonMsgs[color] || [];
  box.innerHTML = "";
  const head = document.createElement("div");
  head.className = "rl-head";
  head.innerHTML = `<span class="cs-dot" style="background:${CAT_COLORS[color]}"></span><b>${esc(catName(color))}</b><span class="muted">${items.length} 条</span>`;
  box.appendChild(head);
  for (const it of [...items].reverse()) {
    const d = document.createElement("div");
    d.className = "rl-item";
    d.innerHTML = `<div class="rl-note">${esc(it.note || it.message.body.slice(0, 60))}</div><div class="rl-sub">${fmtDay(it.message.created_at)} ${fmtTime(it.message.created_at)}${it.note ? " · " + esc(it.message.body.slice(0, 40)) : ""}</div>`;
    d.addEventListener("click", () => {
      box.classList.add("hidden");
      locateMessage(it.message.conversation_id, it.message.id);
    });
    box.appendChild(d);
  }
  const r = tab.getBoundingClientRect();
  const main = $("main").getBoundingClientRect();
  box.style.top = Math.max(64, r.top - main.top) + "px";
  box.classList.remove("hidden");
}
$("ribbon-list").addEventListener("mouseenter", () => clearTimeout(ribbonHideTimer));
$("ribbon-list").addEventListener("mouseleave", () => scheduleHideRibbonList());

// ---------- 分类改名层 ----------
let renameColor = null;
function openRenamePop(color, anchor) {
  renameColor = color;
  $("rename-input").value = catName(color);
  const pop = $("rename-pop");
  pop.classList.remove("hidden");
  const r = anchor && anchor.getBoundingClientRect ? anchor.getBoundingClientRect() : null;
  if (r) {
    pop.style.top = Math.min(window.innerHeight - 180, r.bottom + 6) + "px";
    pop.style.left = Math.max(8, Math.min(window.innerWidth - 276, r.left - 60)) + "px";
  }
  $("rename-input").focus();
  $("rename-input").select();
}
$("rename-cancel").addEventListener("click", () => $("rename-pop").classList.add("hidden"));
$("rename-save").addEventListener("click", async () => {
  const name = $("rename-input").value.trim();
  if (!name) return;
  try {
    await invoke("rename_category", { color: renameColor, name });
    $("rename-pop").classList.add("hidden");
    await loadCategories();
    document.querySelectorAll(".msg-row").forEach((r) => {
      const span = r.querySelector(".msg-tags");
      if (span && r.dataset.mid) refreshMessageCategory(r.dataset.mid, span);
    });
    refreshRibbon();
    if (state.view === "tags") loadCategoryList();
  } catch (e) { alert("改名失败：" + e); }
});
document.addEventListener("click", (e) => {
  const pop = $("rename-pop");
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
  updateAiState();
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
$("btn-local-api").addEventListener("click", () => {
  switchView("settings");
  setTimeout(() => $("local-api-section").scrollIntoView({ behavior: "smooth" }), 60);
});
/** AI 页副标题：如实说明当前走哪条路（本机直连 / 经对端转发 / 都不可用）。 */
async function updateAiState() {
  try {
    const cfg = await invoke("get_ai_config");
    const ready = cfg.has_key && cfg.base_url && cfg.model;
    $("ai-state").textContent = ready
      ? "使用本机配置的 AI 接口"
      : state.connected
        ? `本机未配置 AI · 经${state.peerName || "对方设备"}转发回答`
        : "本机未配置 AI，也未连接设备";
  } catch (_) { /* 忽略 */ }
}
$("btn-ai-send").addEventListener("click", sendAi);
$("ai-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendAi(); }
});
$("btn-ai-stop").addEventListener("click", () => invoke("ai_stop"));

// ---------- 通用确认框与轻提示 ----------
function showConfirm(text, okLabel = "确定") {
  return new Promise((resolve) => {
    $("modal-text").textContent = text;
    $("modal-ok").textContent = okLabel;
    $("modal").classList.remove("hidden");
    const done = (v) => {
      $("modal").classList.add("hidden");
      $("modal-ok").onclick = null;
      $("modal-cancel").onclick = null;
      resolve(v);
    };
    $("modal-ok").onclick = () => done(true);
    $("modal-cancel").onclick = () => done(false);
  });
}
let toastTimer = null;
function showToast(text) {
  const t = $("toast");
  t.textContent = text;
  t.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), 2600);
}

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
  $("ai-temp").value = cfg.temperature ?? 0.7;
  $("ai-temp-val").textContent = Number($("ai-temp").value).toFixed(1);
  $("ai-max-tokens").value = cfg.max_tokens ?? "";
  $("ai-prompt").value = cfg.system_prompt || "";
  $("ai-stream").checked = cfg.stream !== false;
  await loadGatewaySettings();
  await loadStorageStats();
  updateAiState();
}
async function loadGatewaySettings() {
  try {
    const g = await invoke("get_gateway_settings");
    $("proxy-allow-toggle").checked = !!g.proxy_allow;
    $("proxy-stats").textContent = `${g.proxy_count} 次 · 约 ${g.proxy_tokens} tokens`;
    $("local-api-toggle").checked = !!g.local_enabled;
    $("local-api-addr").textContent = g.local_addr;
    $("local-api-status").textContent = g.local_enabled
      ? (g.local_running ? "运行中" : "启动失败（端口可能被占用）")
      : "未开启";
    $("local-api-key").placeholder = g.local_has_key ? "已设置（留空不修改）" : "可留空";
    $("btn-local-api-clear").classList.toggle("hidden", !g.local_has_key);
  } catch (_) { /* 忽略 */ }
}
async function loadStorageStats() {
  try {
    const s = await invoke("storage_stats");
    $("store-chat").textContent = `${fmtSize(s.chat_bytes)} · ${s.chat_count} 条`;
    $("store-files").textContent = `${fmtSize(s.files_bytes)} · ${s.files_count} 个`;
    $("store-cache").textContent = fmtSize(s.cache_bytes);
  } catch (_) { /* 忽略 */ }
}
$("ai-temp").addEventListener("input", () => {
  $("ai-temp-val").textContent = Number($("ai-temp").value).toFixed(1);
});
$("btn-ai-save").addEventListener("click", async () => {
  const maxTok = parseInt($("ai-max-tokens").value, 10);
  await invoke("set_ai_config", {
    baseUrl: $("ai-base").value.trim(),
    apiKey: $("ai-key").value,
    model: $("ai-model").value.trim(),
    stream: $("ai-stream").checked,
    temperature: Number($("ai-temp").value),
    maxTokens: Number.isFinite(maxTok) && maxTok > 0 ? maxTok : null,
    systemPrompt: $("ai-prompt").value,
  });
  $("ai-key").value = "";
  $("ai-test-result").textContent = "已保存";
  updateAiState();
});
$("btn-ai-test").addEventListener("click", async () => {
  $("ai-test-result").textContent = "测试中…";
  try {
    $("ai-test-result").textContent = await invoke("test_ai_connection");
  } catch (e) { $("ai-test-result").textContent = String(e); }
});
$("proxy-allow-toggle").addEventListener("change", async (e) => {
  await invoke("set_proxy_allow", { on: e.target.checked });
  showToast(e.target.checked ? "已允许连接设备借用本机 AI" : "已关闭 AI 转发");
  loadGatewaySettings();
});
$("local-api-toggle").addEventListener("change", async (e) => {
  await invoke("set_local_api", { enabled: e.target.checked, key: null });
  loadGatewaySettings();
});
$("btn-copy-api-addr").addEventListener("click", () => {
  navigator.clipboard
    .writeText($("local-api-addr").textContent)
    .then(() => showToast("接口地址已复制"));
});
$("btn-local-api-save").addEventListener("click", async () => {
  const v = $("local-api-key").value;
  if (!v) {
    $("local-api-result").textContent = "密钥为空，未修改（清除请用清除按钮）";
    return;
  }
  await invoke("set_local_api", { enabled: $("local-api-toggle").checked, key: v });
  $("local-api-key").value = "";
  $("local-api-result").textContent = "密钥已保存";
  loadGatewaySettings();
});
$("btn-local-api-clear").addEventListener("click", async () => {
  await invoke("set_local_api", { enabled: $("local-api-toggle").checked, key: "" });
  $("local-api-result").textContent = "密钥已清除";
  loadGatewaySettings();
});
$("btn-clean-cache").addEventListener("click", async () => {
  const freed = await invoke("clean_cache");
  $("store-result").textContent =
    freed > 0 ? `已清理缓存，释放 ${fmtSize(freed)}` : "没有可清理的缓存";
  loadStorageStats();
});
$("btn-clear-history").addEventListener("click", async () => {
  const ok = await showConfirm(
    "确定要清空全部聊天记录吗？消息、彩签归属、收藏与置顶都会一并清除，此操作不可恢复。",
    "清空"
  );
  if (!ok) return;
  await invoke("clear_chat_history");
  $("store-result").textContent = "聊天记录已清空";
  state.renderedChat.clear();
  $("messages").innerHTML = "";
  refreshRibbon();
  loadStorageStats();
  showToast("聊天记录已清空");
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
    for (const c of await invoke("list_categories"))
      items.push({ label: `分类：${c.name}（${c.count} 条）`, run: () => { switchView("tags"); openCategoryDrawer(c.color); } });
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
    updateAiState();
  });
  await listen("message_new", (e) => {
    const m = e.payload;
    if (m.conversation_id === state.aiConvId) appendMessage($("ai-messages"), m, state.renderedAi, { ai: true });
    else { appendMessage($("messages"), m, state.renderedChat, {}); rebuildDateRail(); }
  });
  await listen("message_status", (e) => {
    const row = document.querySelector(`[data-mid="${e.payload.id}"] .msg-status`);
    if (row) applyStatus(row, e.payload.id, e.payload.status);
  });
  await listen("peer_typing", (e) => {
    $("typing-hint").classList.toggle("hidden", !e.payload);
  });
  await listen("transfer_progress", (e) => {
    const { transfer_id, msg_id, received, total } = e.payload;
    if (msg_id && transfer_id) state.transferMap[msg_id] = transfer_id;
    let b = null;
    if (msg_id) {
      const row = document.querySelector(`[data-mid="${msg_id}"]`);
      if (row) b = row.querySelector(".bubble");
    }
    if (!b) b = state.lastFileBubble;
    if (!b) return;
    const bar = b.querySelector(".progress");
    const fill = b.querySelector(".progress > div");
    if (bar && fill) {
      bar.classList.remove("hidden");
      const pct = total ? Math.floor((received / total) * 100) : 0;
      fill.style.width = pct + "%";
      const meta = b.querySelector(".progress-meta");
      if (meta) {
        meta.classList.remove("hidden");
        meta.textContent = `${pct}% · ${fmtSize(received)} / ${fmtSize(total)}`;
      }
      const cancel = b.querySelector(".progress-cancel");
      if (cancel && received < total) cancel.classList.remove("hidden");
    }
  });
  await listen("transfer_state", (e) => {
    const { transfer_id, state: tstate } = e.payload;
    const mid = Object.keys(state.transferMap).find((k) => state.transferMap[k] === transfer_id);
    let b = null;
    if (mid) {
      const row = document.querySelector(`[data-mid="${mid}"]`);
      if (row) b = row.querySelector(".bubble");
    }
    if (!b) b = state.lastFileBubble;
    if (!b) return;
    const bar = b.querySelector(".progress");
    const meta = b.querySelector(".progress-meta");
    const cancel = b.querySelector(".progress-cancel");
    const retry = b.querySelector(".transfer-retry");
    if (cancel) cancel.classList.add("hidden");
    if (tstate === "done") {
      if (meta) meta.textContent = "已完成";
      setTimeout(() => {
        if (bar) bar.classList.add("hidden");
        if (meta) meta.classList.add("hidden");
      }, 900);
    } else if (tstate === "cancelled") {
      if (meta) { meta.classList.remove("hidden"); meta.textContent = "已取消"; }
      if (retry) retry.classList.remove("hidden");
    } else if (tstate === "verify_failed") {
      if (meta) { meta.classList.remove("hidden"); meta.textContent = "校验失败，文件不完整"; }
      if (retry) retry.classList.remove("hidden");
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
  // 版本号以应用包内版本为准（tauri.conf.json），不在页面里写死，避免与安装包版本脱节
  if (window.__TAURI__.app && window.__TAURI__.app.getVersion) {
    window.__TAURI__.app.getVersion().then((v) => { $("set-version").textContent = v; }).catch(() => {});
  }
  await loadCategories();
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
