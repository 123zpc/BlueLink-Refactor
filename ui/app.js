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
  connectTransport: null, // 本次连接用的传输（自发起时记下，已连接卡副行用；被动连入时为 null）
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

/* ---------- 标志双环数据流特效：左环=本机，右环=链接的机器 ----------
   本机发出数据 → 左环脉冲；对方数据进来 → 右环脉冲；文件传输进行中 → 对应环持续脉冲直到结束。 */
const ringState = { me: { count: 0, until: 0, timer: null }, peer: { count: 0, until: 0, timer: null } };
function ringApply() {
  const logo = $("nav-logo");
  if (!logo) return;
  for (const side of ["me", "peer"]) {
    const on = ringState[side].count > 0 || Date.now() < ringState[side].until;
    logo.classList.toggle(side === "me" ? "flow-me" : "flow-peer", on);
  }
}
function ringPulse(side) {
  const s = ringState[side];
  if (!s) return;
  s.until = Date.now() + 1500;
  ringApply();
  clearTimeout(s.timer);
  s.timer = setTimeout(ringApply, 1550);
}
function ringSet(side, on) {
  const s = ringState[side];
  if (!s) return;
  s.count = Math.max(0, s.count + (on ? 1 : -1));
  ringApply();
}
function ringClearAll() {
  ringState.me.count = 0;
  ringState.peer.count = 0;
  ringApply();
}

/* ---------- 输入框自适应高度：随内容向上长高，到上限（168px，约 7 行）后框内滚动，滚动条已美化 ---------- */
const COMPOSER_MAX_H = 168;
const COMPOSER_MIN_H = 35; // 一行：行高 21 + 上下内边距 14
function autoGrow(el) {
  if (!el) return;
  // 元素所在视图隐藏时没有布局，scrollHeight 为 0，量出来会把输入框压成一条缝；
  // 隐藏时跳过，等视图切回来再量（switchView 末尾会补调）。
  if (el.offsetParent === null) return;
  el.style.height = "auto";
  el.style.height = Math.min(Math.max(el.scrollHeight, COMPOSER_MIN_H), COMPOSER_MAX_H) + "px";
}
function wireAutoGrow(id) {
  const el = $(id);
  if (!el) return;
  el.addEventListener("input", () => autoGrow(el));
  autoGrow(el);
}
wireAutoGrow("input");
wireAutoGrow("ai-input");
window.addEventListener("resize", () => { autoGrow($("input")); autoGrow($("ai-input")); });
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
      } catch (err) { showToast("重发失败：" + err); }
    });
  } else if (m.sender === "ai") {
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
  if (!opts.ai) btns.push(["引用", () => setReply(m)], ["问AI", () => askAiAbout(m)]);
  if (opts.ai && m.sender === "ai") btns.push(["复制", () => navigator.clipboard.writeText(m.body)], ["重新生成", () => regenerateAi()]);
  for (const [label, fn] of btns) {
    const b = document.createElement("button");
    b.textContent = label;
    b.addEventListener("click", fn);
    actions.appendChild(b);
  }
  meta.appendChild(status); meta.appendChild(time); meta.appendChild(actions);
  // 聊天里的 AI 回答（@AI 提问产生）加一个「AI 回答」标记，与对方消息区分
  if (!opts.ai && m.sender === "ai") {
    const badge = document.createElement("div");
    badge.className = "ai-badge";
    badge.textContent = "AI 回答";
    wrap.appendChild(badge);
  }
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
    } catch (err) { showToast("重试失败：" + err); }
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

/** 刷新一条消息的彩签小片：小片只显示分类名，备注不在小片上常驻，
 * 鼠标悬浮小片时才以提示形式出现（用户 2026-11-18 裁定）；点小片改分类，右键给分类改名。 */
async function refreshMessageCategory(mid, span) {
  try {
    const mc = await invoke("get_message_category", { messageId: mid });
    span.innerHTML = "";
    if (!mc) return;
    const hex = CAT_COLORS[mc.color] || CAT_COLORS[0];
    const chip = document.createElement("span");
    chip.className = "tag-chip";
    chip.textContent = catName(mc.color);
    chip.style.borderColor = hex;
    chip.style.color = hex;
    chip.title = mc.note
      ? `备注：${mc.note}（点击修改分类，右键给分类改名）`
      : `分类：${catName(mc.color)}（点击修改，右键改名）`;
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

/** 唯一的定位链路：切会话→加载消息→滚动到目标→高亮闪烁两次。
 *  colorHex 可选：从某个彩签分类发起的定位（点彩签、悬停列表、分类总览）传该分类颜色，
 *  脉冲高亮就用分类色；引用/搜索/收藏/置顶发起的定位不传，仍用默认蓝色。 */
async function locateMessage(conversationId, messageId, colorHex) {
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
    if (colorHex) el.style.setProperty("--flash-ring", colorHex);
    el.classList.add("flash");
    setTimeout(() => { el.classList.remove("flash"); el.style.removeProperty("--flash-ring"); }, 1300);
  }
  closeDrawer();
}

// ---------- 视图切换 ----------
function switchView(view) {
  state.view = view;
  document.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.view === view));
  $("chat-view").classList.toggle("hidden", !(view === "chat" && state.connected));
  $("connect-view").classList.toggle("hidden", !(view === "chat" && !state.connected));
  $("ai-view").classList.toggle("hidden", view !== "ai");
  $("tags-view").classList.toggle("hidden", view !== "tags");
  $("fav-view").classList.toggle("hidden", view !== "favorites");
  $("settings-view").classList.toggle("hidden", view !== "settings");
  applyAside();
  syncDrawer();
  // 视图刚变可见时补量一次输入框高度（隐藏期间 autoGrow 会跳过）
  autoGrow($("input"));
  autoGrow($("ai-input"));
  if (view === "tags") loadCategoryList();
  if (view === "favorites") loadFavList();
  if (view === "ai") loadAiMessages();
  if (view === "settings") loadSettings();
}

document.querySelectorAll(".nav-item").forEach((b) =>
  b.addEventListener("click", () => switchView(b.dataset.view)));

// ---------- 会话列表收起/展开（列表只属于聊天页；收起即整列消失，不留残条） ----------
// 默认收起（用户裁定，两次确认）：每次启动程序都以收起状态进入，不沿用上次退出时的展开状态；
// 本次运行内用户展开/收起的选择照常生效，重启后回到收起。
// 状态只许由悬浮开关改：页面切换永不自动弹开（v2.4.1 修——旧版导航点「聊天」时
// 会强制把收起状态改成展开，用户反馈一切换菜单列表就自己弹开）。
// 开关形态（v2.4.1 用户定版）：唯一的展开/收起入口是骑在左导航/列表与内容区分界线上的
// 圆形悬浮按钮（原页头 » 展开按钮与列表头 « 收起按钮已收敛删除），仅聊天页出现。
let asideBootDone = false;
function applyAside() {
  if (!asideBootDone) {
    asideBootDone = true;
    localStorage.setItem("bl-aside", "collapsed");
  }
  const collapsed = localStorage.getItem("bl-aside") === "collapsed";
  $("aside").classList.toggle("hidden", state.view !== "chat" || collapsed);
  // 悬浮开关定位：收起时贴导航右缘、展开时贴列表右缘（按实测宽：导航宽 + 列表实宽，
  // 列表隐藏时实宽为 0，自然落在导航右缘）；图标随状态翻转。
  const fb = $("btn-aside-float");
  const onChat = state.view === "chat";
  fb.classList.toggle("hidden", !onChat);
  if (onChat) {
    fb.style.left = ($("nav").offsetWidth + $("aside").offsetWidth) + "px";
    fb.querySelector(".float-ico-open").classList.toggle("hidden", !collapsed);
    fb.querySelector(".float-ico-close").classList.toggle("hidden", collapsed);
    fb.title = collapsed ? "展开会话列表" : "收起会话列表";
  }
}
$("btn-aside-float").addEventListener("click", () => {
  const collapsed = localStorage.getItem("bl-aside") === "collapsed";
  localStorage.setItem("bl-aside", collapsed ? "open" : "collapsed");
  applyAside();
});

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
/* 分页懒加载（2026-11-18 用户问性能后补）：打开会话只渲染最近 CHAT_PAGE 条，
   向上滚到接近顶部时再按页往前补，DOM 数量始终跟着「实际看过多少」走，
   消息再多也不会一打开就把成千上万个节点全铺出来。定位（搜索/彩签/引用）
   仍走 locateMessage 的整段加载兜底。 */
const CHAT_PAGE = 100;
async function loadChatMessages() {
  if (!state.peerConvId) return;
  await refreshRibbon();
  const msgs = await invoke("list_messages", { conversationId: state.peerConvId, limit: CHAT_PAGE });
  const container = $("messages");
  container.innerHTML = "";
  state.renderedChat.clear();
  state.chatHasMore = msgs.length >= CHAT_PAGE;
  state.chatLoadingOlder = false;
  for (const m of msgs) appendMessage(container, m, state.renderedChat, {});
  // 恢复草稿
  const convs = await invoke("list_conversations");
  const conv = convs.find((c) => c.id === state.peerConvId);
  if (conv && conv.draft) { $("input").value = conv.draft; autoGrow($("input")); }
  refreshPinBar();
  rebuildDateRail();
}

/** 向上翻页：取当前最旧一条之前的一页，拼到顶部并保持阅读位置不动。 */
async function loadOlderChat() {
  const container = $("messages");
  if (!state.peerConvId || !state.chatHasMore || state.chatLoadingOlder) return;
  const first = container.querySelector(".msg-row");
  if (!first) return;
  state.chatLoadingOlder = true;
  try {
    const before = Number(first.dataset.ts || 0);
    const older = await invoke("list_messages", { conversationId: state.peerConvId, before, limit: CHAT_PAGE });
    const fresh = older.filter((m) => !state.renderedChat.has(m.id));
    if (!fresh.length) { state.chatHasMore = false; return; }
    if (older.length < CHAT_PAGE) state.chatHasMore = false;
    // 先在临时容器里按正常顺序渲染（复用日期分隔/分组逻辑），再处理与现有第一条的接缝
    const tmp = document.createElement("div");
    for (const m of fresh) appendMessage(tmp, m, state.renderedChat, {});
    const nextRow = container.querySelector(".msg-row");
    if (nextRow) {
      const lastNew = tmp.querySelectorAll(".msg-row");
      const lastNewRow = lastNew.length ? lastNew[lastNew.length - 1] : null;
      const prevSep = nextRow.previousElementSibling;
      if (lastNewRow && fmtDay(Number(lastNewRow.dataset.ts)) === fmtDay(Number(nextRow.dataset.ts))) {
        // 同一天：现有第一条上方那条日期分隔成了多余的（新批次里已有同日分隔或同属一天）
        if (prevSep && prevSep.classList.contains("date-sep")) prevSep.remove();
        if (nextRow.dataset.sender === lastNewRow.dataset.sender && Number(nextRow.dataset.ts) - Number(lastNewRow.dataset.ts) < 5 * 60 * 1000) {
          nextRow.classList.add("grouped");
        }
      }
    }
    const h0 = container.scrollHeight;
    const top0 = container.scrollTop;
    container.prepend(...tmp.childNodes);
    container.scrollTop = top0 + (container.scrollHeight - h0);
    rebuildDateRail();
  } catch (_) {
    /* 翻页失败不打断阅读，下次滚到顶再试 */
  } finally {
    state.chatLoadingOlder = false;
  }
}

/* 聊天区滚动：接近顶部自动翻页；离底部够远时亮出「回到底部」。 */
$("messages").addEventListener("scroll", () => {
  const c = $("messages");
  if (c.scrollTop < 120) loadOlderChat();
  const dist = c.scrollHeight - c.scrollTop - c.clientHeight;
  $("btn-bottom").classList.toggle("hidden", dist <= 400);
});
$("btn-bottom").addEventListener("click", () => {
  const c = $("messages");
  c.scrollTo({ top: c.scrollHeight, behavior: "smooth" });
});

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
  } catch (err) { showToast("粘贴图片失败：" + err); }
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
      showToast("尚未连接设备，图片未发送");
    } else {
      invoke("send_file_path", { path: img.path }).catch((e) => showToast("图片发送失败：" + e));
    }
    if (!text) {
      $("input").value = ""; autoGrow($("input"));
      docDismissed = false;
      updateDocBar();
      return;
    }
  }
  // @AI 路由：这条消息是问 AI 的，不发给对方；问答经事件回推落到当前聊天里
  if (pendingAiRoute && text) {
    if (chatAiBusy) { showToast("AI 正在回答上一个问题，请稍候"); return; }
    const src = pendingAiRoute;
    pendingAiRoute = null;
    $("ai-route-bar").classList.add("hidden");
    $("input").value = ""; autoGrow($("input"));
    docDismissed = false;
    updateDocBar();
    invoke("save_draft", { conversationId: state.peerConvId, text: "" }).catch(() => {});
    chatAiBusy = true;
    try {
      await invoke("chat_ask_ai", { conversationId: state.peerConvId, source: src, text });
    } catch (e) {
      showToast("问AI 失败：" + e);
    } finally { chatAiBusy = false; }
    return;
  }
  const asDoc = docSuggestion;
  const replyTo = state.replyTo;
  $("input").value = ""; autoGrow($("input"));
  docDismissed = false;
  updateDocBar();
  state.replyTo = null;
  $("reply-bar").classList.add("hidden");
  invoke("save_draft", { conversationId: state.peerConvId, text: "" }).catch(() => {});
  try {
    if (asDoc) {
      // 转文档发送：文件气泡由后端事件回推，与选文件发送一致
      await invoke("send_document", { name: asDoc.name, content: raw });
      ringPulse("me");
    } else {
      const m = await invoke("send_text", { text, replyTo });
      ringPulse("me");
      appendMessage($("messages"), m, state.renderedChat, {});
    }
  } catch (e) {
    showToast("发送失败：" + e);
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
    invoke("send_file_path", { path }).catch((e) => showToast("文件发送失败：" + e));
  } catch (e) { showToast("选择文件失败：" + e); }
});

$("btn-disconnect").addEventListener("click", async () => { await invoke("disconnect"); });
$("btn-connected-disconnect").addEventListener("click", async () => { await invoke("disconnect"); });
$("btn-goto-chat").addEventListener("click", () => switchView("chat"));

// ---------- 连接（蓝牙） ----------
// 传输选择（仅开发者模式可用，存设置键 bt_transport）：
// "classic" 经典蓝牙 / "ble" BLE / "simpleble" SimpleBLE（第三套实现，实验）。
// 普通模式连接页不出现选择，固定走经典蓝牙（用户 2026-11-18 定版）；
// 非 Windows 没有经典通道（classic_available 为假）时一律回落 BLE。
// SimpleBLE 未编译进当前版本时，后端会如实回落经典蓝牙并在状态文案里写明。
let btTransport = "classic"; // 开发者模式里选定的传输（启动时从设置读回）
let btClassicAvailable = false;
let btStateLoaded = false;

function syncTransportSeg() {
  $("seg-classic").classList.toggle("active", btTransport === "classic");
  $("seg-ble").classList.toggle("active", btTransport === "ble");
  $("seg-simpleble").classList.toggle("active", btTransport === "simpleble");
  $("seg-classic").disabled = !btClassicAvailable;
}

/** 本次扫描/连接实际用的传输：开发者模式选了 SimpleBLE 就直传（后端负责未编译回落）；
 * 经典不可用时恒 BLE；普通模式恒经典；开发者模式用所选。 */
function effectiveTransport() {
  if (state.devMode && btTransport === "simpleble") return "simpleble";
  if (!btClassicAvailable) return "ble";
  return state.devMode ? btTransport : "classic";
}

async function switchTransport(t) {
  if (t === btTransport) { syncTransportSeg(); return; }
  btTransport = t;
  syncTransportSeg();
  invoke("set_bt_transport", { transport: t }).catch(() => {});
  refreshBtState();
  // 正在扫描时切换通道：停掉旧扫描、用新通道重扫
  if (scanning) {
    await invoke("bt_stop_scan").catch(() => {});
    try {
      await invoke("bt_start_scan", { transport: effectiveTransport() });
    } catch (e) {
      scanning = false;
      setScanningUI();
      $("connect-error").textContent = String(e);
    }
  }
}
$("seg-classic").addEventListener("click", () => switchTransport("classic"));
$("seg-ble").addEventListener("click", () => switchTransport("ble"));
$("seg-simpleble").addEventListener("click", () => switchTransport("simpleble"));

/** 传输的中文名（设备行副行、已连接卡、上次连接卡共用）。 */
function transportName(t) {
  return t === "classic" ? "经典蓝牙" : t === "simpleble" ? "SimpleBLE" : t === "lan" ? "局域网" : "BLE";
}

let lastBtState = null;

/** 顶部状态胶囊：已连接 > 扫描中 > 生效传输的服务状态（短文案，详状看右侧状态卡）。 */
function updateCapsule() {
  const dot = $("capsule-dot");
  const txt = $("capsule-text");
  if (state.connected) {
    dot.className = "status-dot on";
    txt.textContent = `已连接 ${state.peerName || ""}`.trim();
    return;
  }
  if (scanning) {
    dot.className = "status-dot on";
    txt.textContent = "扫描中…";
    return;
  }
  const s = lastBtState;
  if (!s) {
    dot.className = "status-dot";
    txt.textContent = "正在检测蓝牙…";
    return;
  }
  const t = effectiveTransport();
  if (t === "classic") {
    if (s.classic_serving) { dot.className = "status-dot on"; txt.textContent = "经典蓝牙已开启 · 服务运行中"; }
    else if (!s.classic_available) { dot.className = "status-dot off"; txt.textContent = "经典蓝牙不可用"; }
    else if (String(s.classic_message).includes("服务启动失败") || String(s.classic_message).includes("服务未能启动")) { dot.className = "status-dot warn"; txt.textContent = "经典蓝牙服务启动失败"; }
    else { dot.className = "status-dot warn"; txt.textContent = "经典蓝牙已开启 · 服务未启动"; }
  } else if (t === "simpleble") {
    if (s.simpleble_serving) { dot.className = "status-dot on"; txt.textContent = "SimpleBLE 已开启 · 服务运行中"; }
    else if (!s.simpleble_compiled) { dot.className = "status-dot warn"; txt.textContent = "SimpleBLE 未编译 · 已回落经典蓝牙"; }
    else if (!s.simpleble_available) { dot.className = "status-dot off"; txt.textContent = "未检测到蓝牙"; }
    else { dot.className = "status-dot warn"; txt.textContent = "SimpleBLE 已开启 · 广播未启动"; }
  } else {
    if (s.advertising) { dot.className = "status-dot on"; txt.textContent = "蓝牙已开启 · 可被发现"; }
    else if (!s.available) { dot.className = "status-dot off"; txt.textContent = "未检测到蓝牙"; }
    else { dot.className = "status-dot warn"; txt.textContent = "蓝牙已开启 · 广播未启动"; }
  }
}

/** 右侧「连接状态」卡：每个传输一行（圆点+人话文案），异常行橙色并附说明小字。
 * 原型常态两行（经典 + BLE）；开发者模式下多一行 SimpleBLE。 */
function renderStatusCard(s) {
  const lines = [];
  const line = (dot, text, warn = false, note = "") =>
    `<div class="status-line${warn ? " warn" : ""}"><span class="status-dot ${dot}"></span><span>${esc(text)}</span></div>${note ? `<p class="status-note">${esc(note)}</p>` : ""}`;
  // 经典蓝牙行
  if (s.classic_serving && String(s.classic_message).includes("可被附近设备发现")) {
    lines.push(line("on", "经典蓝牙：已开启，服务运行中"));
  } else if (s.classic_serving) {
    lines.push(line("warn", "经典蓝牙：服务已启动，但 SDP 注册失败，对方可能找不到本机", true));
  } else if (String(s.classic_message).includes("服务启动失败") || String(s.classic_message).includes("服务未能启动")) {
    // 服务启动失败的人话说明：端口占用（WSA 10048）是最常见的一种，给出可操作解释。
    // v2.3.1 后端对 10048 已直接给出人话整句（「经典蓝牙服务未能启动：端口被其他程序占用…」），
    // 优先取其冒号后的说明原文（与后端文案保持一致）；后端未人话时按同一句式兜底。
    const raw = String(s.classic_message);
    const note = raw.includes("端口被其他程序占用")
      ? raw.slice(raw.indexOf("端口被其他程序占用"))
      : raw.includes("10048")
        ? "端口被其他程序占用（多半是另一个 BlueLink 或老版本正在运行），关掉它再重启本应用；你主动连接别人不受影响"
        : raw.replace(/^经典蓝牙已开启，但服务启动失败：?/, "").replace(/^经典蓝牙服务未能启动：?/, "");
    lines.push(line("warn", "经典蓝牙：服务启动失败", true, note));
  } else if (!s.classic_available) {
    // 后端文案本身已带「经典蓝牙」前缀时（如「经典蓝牙仅支持 Windows」）不重复加前缀
    lines.push(line("off", String(s.classic_message).startsWith("经典蓝牙") ? s.classic_message : `经典蓝牙：${s.classic_message}`));
  } else {
    lines.push(line("warn", "经典蓝牙：已开启，但服务未启动", true));
  }
  // BLE 行
  if (s.advertising) {
    lines.push(line("on", "BLE：已开启"));
  } else if (!s.available) {
    lines.push(line("off", "BLE：未检测到蓝牙适配器或蓝牙未开启"));
  } else {
    lines.push(line("warn", `BLE：${String(s.message).replace(/^蓝牙/, "")}`, true));
  }
  // SimpleBLE 行（仅开发者模式）
  if (state.devMode) {
    if (s.simpleble_serving) lines.push(line("on", "SimpleBLE：已开启"));
    else if (!s.simpleble_compiled) lines.push(line("warn", "SimpleBLE：未编译进本版本，已回落经典蓝牙", true));
    else if (!s.simpleble_available) lines.push(line("off", "SimpleBLE：未检测到蓝牙适配器或蓝牙未开启"));
    else lines.push(line("warn", "SimpleBLE：广播未启动", true, s.simpleble_message));
  }
  $("status-lines").innerHTML = lines.join("");
}

async function refreshBtState() {
  try {
    const s = await invoke("bt_state");
    lastBtState = s;
    btClassicAvailable = !!s.classic_available;
    btStateLoaded = true;
    syncTransportSeg();
    renderStatusCard(s);
    updateCapsule();
    // 生效传输不可用时禁用扫描按钮（点了也必然失败，不如一开始就明示）
    const t = effectiveTransport();
    $("btn-scan").disabled = (t === "classic" && !s.classic_available) || (t === "ble" && !s.available);
    if (t === "ble" && !s.available) {
      $("bt-empty").textContent = "未检测到可用的蓝牙：请确认设备有蓝牙并已在系统中开启。";
    }
  } catch (_) { btStateLoaded = true; }
}

let scanning = false;
let hasScanned = false; // 本次运行扫过至少一轮（决定空列表显示「没有发现设备」还是初始提示）
let lastDevices = []; // 最近一次 bt_devices 快照（选中行重渲染、上次连接徽标刷新用）

/** 扫描按钮与胶囊随 scanning 状态同步（全页统一入口，别再各处改按钮文案）。 */
function setScanningUI() {
  $("btn-scan").textContent = scanning ? "停止" : "扫描";
  updateCapsule();
  updateDevicePanelState();
}

/** 左面板空态区：有设备时全收；扫描中给提示；扫过且为空给大空态；初始给一行提示。 */
function updateDevicePanelState() {
  const emptyState = $("bt-empty-state");
  const hint = $("bt-empty");
  if (lastDevices.length > 0) {
    emptyState.classList.add("hidden");
    if (!lastBtState || effectiveTransport() !== "ble" || lastBtState.available) hint.textContent = "";
    return;
  }
  if (scanning) {
    emptyState.classList.add("hidden");
    hint.textContent = "正在扫描附近的设备…";
  } else if (hasScanned) {
    emptyState.classList.remove("hidden");
    hint.textContent = "";
  } else {
    emptyState.classList.add("hidden");
    if (!lastBtState || effectiveTransport() !== "ble" || lastBtState.available) {
      hint.textContent = "点「扫描」查找附近打开了 BlueLink 的设备。";
    }
  }
}

async function toggleScan() {
  $("connect-error").textContent = "";
  try {
    if (scanning) {
      await invoke("bt_stop_scan");
      scanning = false;
      hasScanned = true;
      setScanningUI();
    } else {
      if (!btStateLoaded) await refreshBtState();
      await invoke("bt_start_scan", { transport: effectiveTransport() });
      scanning = true;
      hasScanned = true;
      setScanningUI();
    }
  } catch (e) { $("connect-error").textContent = String(e); }
}
$("btn-scan").addEventListener("click", toggleScan);
$("btn-rescan").addEventListener("click", toggleScan);

/** 信号格数图标：按 RSSI 分 4 格（原型裁定，不用 dBm 文字）。 */
function sigBars(rssi) {
  const n = rssi >= -55 ? 4 : rssi >= -65 ? 3 : rssi >= -75 ? 2 : rssi >= -85 ? 1 : 0;
  let bars = "";
  for (let i = 1; i <= 4; i++) bars += `<i class="${i <= n ? "on" : ""}"></i>`;
  return `<span class="sig" title="信号 ${rssi} dBm">${bars}</span>`;
}

/** 渲染扫描到的设备列表（bt_devices 事件推送快照）。
 * 行结构（定版原型）：蓝牙图标 / 设备名（+上次连接或已连接徽标）/ 副行「传输 · 已配对」/
 * 信号格（仅有 RSSI 时渲染，经典扫描没有就不画、不造假）/ 连接或断开按钮。
 * 扫描中在列表尾部挂骨架行，表示还在找。 */
function renderBtDevices(devices) {
  lastDevices = devices;
  if (scanning) hasScanned = true;
  const list = $("bt-device-list");
  list.innerHTML = "";
  $("device-count").textContent = `${devices.length} 台`;
  for (const d of devices) {
    const isConnectedPeer = state.connected && state.peerName && d.name === state.peerName;
    const isLastPeer = !isConnectedPeer && state.lastPeer && state.lastPeer.deviceId === d.id;
    const row = document.createElement("div");
    row.className = "bt-device" + (state.btDevice === d.id ? " selected" : "");
    row.innerHTML = `<span class="bt-device-ico">${iconUse("i-bt", "ico ico-20")}</span>
      <span class="bt-device-main"><span class="bt-device-name"><span class="nm">${esc(d.name)}</span>${
        isConnectedPeer ? `<span class="peer-badge connected">已连接</span>`
          : isLastPeer ? `<span class="peer-badge">上次连接</span>` : ""
      }</span>
      <span class="bt-device-sub">${transportName(effectiveTransport())}${d.paired ? " · 已配对" : ""}</span></span>
      ${d.rssi != null ? sigBars(d.rssi) : ""}
      ${isConnectedPeer
        ? `<button class="btn small-btn danger-outline">断开</button>`
        : `<button class="btn small-btn primary">连接</button>`}`;
    const btn = row.querySelector("button");
    if (isConnectedPeer) {
      btn.addEventListener("click", async () => { await invoke("disconnect"); });
    } else {
      btn.addEventListener("click", () => openCodeCard(d));
    }
    list.appendChild(row);
  }
  if (scanning) {
    const skelCount = devices.length ? 1 : 2;
    for (let i = 0; i < skelCount; i++) {
      const skel = document.createElement("div");
      skel.className = "bt-skeleton";
      skel.innerHTML = `<span class="skel skel-ico"></span><span style="flex:1"><span class="skel skel-line" style="display:block;width:45%;margin-bottom:8px"></span><span class="skel skel-line" style="display:block;width:28%"></span></span>`;
      list.appendChild(skel);
    }
  }
  updateDevicePanelState();
  // 「上次连接」一键重连（蓝牙）：后台扫描就是为它跑的，名单里出现目标设备就自动发起连接
  if (pendingRemembered) {
    const hit = devices.find((d) => d.id === pendingRemembered.deviceId);
    if (hit) {
      const p = pendingRemembered;
      pendingRemembered = null;
      clearTimeout(rememberedTimer);
      state.btDevice = hit.id;
      state.btDeviceName = hit.name;
      state.connectTransport = p.transport;
      $("last-peer-status").textContent = `已找到「${hit.name}」，正在连接…`;
      invoke("bt_connect", { deviceId: p.deviceId, code: p.code, transport: p.transport })
        .then(() => saveLastPeer({ transport: p.transport, deviceId: p.deviceId, code: p.code, name: hit.name }))
        .catch((e) => { $("last-peer-status").textContent = ""; $("connect-error").textContent = String(e); });
      if (scanning) { invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
    }
  }
}

// ---------- 上次连接（一键重连） ----------
// 记住上次成功连接的设备（后端设置表存 JSON），连接页渲染成一行，点「连接」直接重连：
// 局域网用存下的地址+连接码直连；蓝牙在后台自动扫描、找到目标设备后自动连接，
// 用户不用打开扫描列表等、也不用再输连接码（用户 2026-11-18 要求）。
let pendingRemembered = null;
let rememberedTimer = null;

async function saveLastPeer(p) {
  try { await invoke("set_last_peer", { json: JSON.stringify({ ...p, ts: Date.now() }) }); } catch (_) { /* 保存失败不影响连接本身 */ }
  loadLastPeer();
}

/** 步骤卡两种形态（定版原型）：有上次连接记录时是简版三步；
 * 没有记录时换成详细指南版（教对方怎么连本机），提示行也随之换。 */
function updateStepsCard(hasLastPeer) {
  $("steps-short").classList.toggle("hidden", !hasLastPeer);
  $("steps-guide").classList.toggle("hidden", hasLastPeer);
  $("steps-tip").textContent = hasLastPeer
    ? "提示：对方连不上时，先确认两边蓝牙都已开启"
    : "提示：不要把连接码发给陌生人";
}

async function loadLastPeer() {
  const box = $("last-peer");
  try {
    const raw = await invoke("get_last_peer");
    const p = raw ? JSON.parse(raw) : null;
    if (!p || !p.name) { box.classList.add("hidden"); state.lastPeer = null; updateStepsCard(false); renderBtDevices(lastDevices); return; }
    // 局域网与 SimpleBLE 记录只在开发者模式下可用（与各自的入口一致），关了开发者模式就不显示
    if ((p.transport === "lan" || p.transport === "simpleble") && !state.devMode) { box.classList.add("hidden"); updateStepsCard(false); return; }
    state.lastPeer = p;
    $("last-peer-name").textContent = p.name;
    const when = p.ts ? (() => { const d = new Date(p.ts); const z = (n) => String(n).padStart(2, "0"); return `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())} ${z(d.getHours())}:${z(d.getMinutes())}`; })() : "";
    $("last-peer-sub").textContent = `${transportName(p.transport)}${when ? " · " + when : ""}`;
    $("last-peer-status").textContent = "";
    box.classList.remove("hidden");
    updateStepsCard(true);
    renderBtDevices(lastDevices);
  } catch (_) { box.classList.add("hidden"); }
}

$("last-peer-clear").addEventListener("click", async () => {
  await invoke("set_last_peer", { json: "" }).catch(() => {});
  state.lastPeer = null;
  $("last-peer").classList.add("hidden");
  updateStepsCard(false);
  renderBtDevices(lastDevices);
});

$("last-peer-connect").addEventListener("click", async () => {
  const p = state.lastPeer;
  if (!p) return;
  $("connect-error").textContent = "";
  if (p.transport === "lan") {
    state.connectTransport = "lan";
    $("last-peer-status").textContent = `正在连接「${p.name}」…`;
    try {
      const name = await invoke("connect_to", { addr: p.addr, code: p.code });
      saveLastPeer({ ...p, name: name || p.name });
    } catch (e) {
      $("last-peer-status").textContent = "";
      $("connect-error").textContent = String(e);
    }
  } else {
    // 按记录里的通道重连：经典记录走经典扫描（Inquiry 含已记住设备），
    // BLE 记录走 BLE 扫描，SimpleBLE 记录走 SimpleBLE 扫描（未编译时后端回落）
    const t = p.transport === "classic" ? "classic" : p.transport === "simpleble" ? "simpleble" : "ble";
    btTransport = t;
    syncTransportSeg();
    refreshBtState();
    $("last-peer-status").textContent = `正在查找「${p.name}」，找到后自动连接…`;
    pendingRemembered = { deviceId: p.deviceId, code: p.code, transport: t };
    clearTimeout(rememberedTimer);
    rememberedTimer = setTimeout(() => {
      if (pendingRemembered) {
        pendingRemembered = null;
        $("last-peer-status").textContent = "未找到该设备，请确认对方已开机并打开 BlueLink 后再试。";
        if (scanning) { invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
      }
    }, 25000);
    try {
      await invoke("bt_start_scan", { transport: t });
      scanning = true;
      hasScanned = true;
      setScanningUI();
    } catch (e) {
      pendingRemembered = null;
      clearTimeout(rememberedTimer);
      $("last-peer-status").textContent = "";
      $("connect-error").textContent = String(e);
    }
  }
});

// ---------- 开发者模式（局域网测试通道 + 传输切换，正式版默认隐藏） ----------
function applyDevMode(on) {
  state.devMode = on;
  // 开发者卡（传输切换 + 局域网入口）只在开发者模式出现在右栏顶部
  $("dev-card").classList.toggle("hidden", !on);
  if (!on) {
    $("dev-lan-section").classList.add("hidden");
    $("lan-toggle-label").textContent = "进入 ›";
  }
  const row = $("dev-mode-row");
  if (row) row.classList.toggle("hidden", !on);
  // 状态卡的 SimpleBLE 行、传输选择随开发者模式重新计算
  refreshBtState();
  // 上次连接卡的可见性与开发者模式相关（局域网/SimpleBLE 记录），一并刷新
  loadLastPeer();
  // 开关开发者模式时停掉进行中的扫描，避免新旧传输混杂
  if (scanning) {
    invoke("bt_stop_scan").catch(() => {});
    scanning = false;
    setScanningUI();
  }
}
// 局域网测试入口行：点「进入」展开局域网连接区（定版原型开发者卡形态）
$("btn-lan-toggle").addEventListener("click", () => {
  const nowHidden = $("dev-lan-section").classList.toggle("hidden");
  $("lan-toggle-label").textContent = nowHidden ? "进入 ›" : "收起 ›";
});
// 开发者模式开关：设置页连点版本号三次（用户 2026-11-18 定版入口，替代原开关）
let versionClicks = 0;
let versionClickTimer = null;
$("set-version").addEventListener("click", async () => {
  versionClicks += 1;
  clearTimeout(versionClickTimer);
  versionClickTimer = setTimeout(() => { versionClicks = 0; }, 900);
  if (versionClicks < 3) return;
  versionClicks = 0;
  clearTimeout(versionClickTimer);
  const on = !state.devMode;
  try {
    await invoke("set_dev_mode", { on });
    applyDevMode(on);
    showToast(on ? "开发者模式已开启" : "开发者模式已关闭");
  } catch (e) { showToast("切换开发者模式失败：" + e); }
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
  state.connectTransport = "lan";
  try {
    const name = await invoke("connect_to", { addr, code });
    saveLastPeer({ transport: "lan", addr, code, name: name || "对方设备" });
  } catch (e) {
    $("connect-error").textContent = String(e);
    $("connect-status").textContent = "";
  }
});

// ---------- 输码卡（右栏第一张卡：点设备后替换本机设备卡） ----------
let connecting = false;

/** 打开输码卡：高亮选中行，本机设备卡让位，输入框聚焦。 */
function openCodeCard(d) {
  state.btDevice = d.id;
  state.btDeviceName = d.name;
  renderBtDevices(lastDevices);
  $("code-card-title").textContent = `连接到 ${d.name}`;
  setCodeError("");
  $("bt-code-card").classList.remove("hidden");
  $("my-card").classList.add("hidden");
  $("bt-peer-code").focus();
}

/** 关闭输码卡：回到本机设备卡，清除选中高亮与错误。 */
function closeCodeCard() {
  $("bt-code-card").classList.add("hidden");
  if (!state.connected) $("my-card").classList.remove("hidden");
  state.btDevice = null;
  state.btDeviceName = null;
  setCodeError("");
  renderBtDevices(lastDevices);
}

/** 输码卡内的错误（红字 + 输入框红描边）；连接码不对时用原型定版文案。 */
function setCodeError(msg) {
  const e = $("code-error");
  e.textContent = msg || "";
  e.classList.toggle("hidden", !msg);
  $("bt-peer-code").classList.toggle("input-error", !!msg);
}
function codeErrorText(err) {
  const raw = String(err);
  if (raw.includes("连接码不正确")) return "连接码不对，或对方已离线，请核对后重试";
  return raw;
}

/** 连接中态：按钮转圈禁用、取消与输入禁用（原型连接中画板）。 */
function setConnectingUI(on) {
  connecting = on;
  const btn = $("btn-bt-connect");
  btn.disabled = on;
  btn.innerHTML = on ? `<span class="spinner"></span>连接中…` : "连接";
  $("btn-bt-cancel").disabled = on;
  $("bt-peer-code").disabled = on;
}

$("btn-bt-cancel").addEventListener("click", () => { if (!connecting) closeCodeCard(); });
$("bt-peer-code").addEventListener("input", () => setCodeError(""));
$("bt-peer-code").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); $("btn-bt-connect").click(); }
});

$("btn-bt-connect").addEventListener("click", async () => {
  if (connecting) return;
  const code = $("bt-peer-code").value.trim();
  if (!state.btDevice) { setCodeError("请先在列表中选择一台设备"); return; }
  if (!code) { setCodeError("请输入对方的连接码"); return; }
  setCodeError("");
  $("connect-error").textContent = "";
  if (!btStateLoaded) await refreshBtState();
  const t = effectiveTransport();
  state.connectTransport = t;
  setConnectingUI(true);
  try {
    await invoke("bt_connect", { deviceId: state.btDevice, code, transport: t });
    saveLastPeer({ transport: t, deviceId: state.btDevice, code, name: state.btDeviceName || "对方设备" });
    if (scanning) { await invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
    setConnectingUI(false);
    closeCodeCard();
  } catch (e) {
    setConnectingUI(false);
    setCodeError(codeErrorText(e));
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
    item.addEventListener("contextmenu", (e) => { e.preventDefault(); openRenamePop(c.color, item); });
    list.appendChild(item);
  }
}

// 抽屉只属于彩签页：开关状态（drawerOpen + 选中分类 drawerColor）跨页保持，但可见性
// 跟随当前页——离开彩签页随页隐藏、切回时原样恢复，绝不漏到其他页面（v2.4.1 修：
// 此前抽屉挂在顶层且切页不处理，开着切到 AI/设置页会一直盖在那些页面上）。
let drawerOpen = false;
function syncDrawer() {
  $("drawer").classList.toggle("hidden", !(drawerOpen && state.view === "tags"));
}
async function openCategoryDrawer(color) {
  const items = await invoke("list_messages_by_category", { color });
  drawerColor = color;
  resetCatRenameEdit();
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
    item.addEventListener("click", () => locateMessage(m.conversation_id, m.id, CAT_COLORS[color]));
    list.appendChild(item);
  }
  drawerOpen = true;
  syncDrawer();
}
function closeDrawer() { drawerOpen = false; syncDrawer(); resetCatRenameEdit(); }
$("drawer-close").addEventListener("click", closeDrawer);

async function toggleMark(mid, kind) {
  try {
    const on = await invoke("toggle_mark", { messageId: mid, kind });
    if (kind === "pin") refreshPinBar();
    if (kind === "favorite" && state.view === "favorites") loadFavList();
    return on;
  } catch (e) { showToast("操作失败：" + e); }
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
        try {
          await navigator.clipboard.writeText(info.path);
          showToast("此系统不支持复制文件本身，已复制文件路径");
        } catch (_) {
          // WebKitGTK 等环境剪贴板整体不可用时，给中文提示，不把英文 DOM 异常抛给用户
          showToast("复制失败：当前系统剪贴板不可用");
        }
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
      try { await fn(); } catch (e) { showToast("操作失败：" + e); }
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
      locateMessage(state.peerConvId, it.message.id, CAT_COLORS[c.color]);
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
    // 条目只显示一行（CSS 省略号）；悬停条目时用提示显示完整备注（无备注则显示完整正文）——用户 2026-11-18 裁定
    d.title = it.note || it.message.body;
    d.addEventListener("click", () => {
      box.classList.add("hidden");
      locateMessage(it.message.conversation_id, it.message.id, CAT_COLORS[color]);
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
    await refreshAfterCategoryRename();
  } catch (e) { showToast("改名失败：" + e); }
});
// 改名成功后的统一刷新：分类缓存、聊天里的小片、彩签悬浮列表、彩签页列表、打开中的详情标题。
async function refreshAfterCategoryRename() {
  await loadCategories();
  document.querySelectorAll(".msg-row").forEach((r) => {
    const span = r.querySelector(".msg-tags");
    if (span && r.dataset.mid) refreshMessageCategory(r.dataset.mid, span);
  });
  refreshRibbon();
  if (state.view === "tags") loadCategoryList();
  if (drawerColor !== null && drawerOpen) {
    const items = await invoke("list_messages_by_category", { color: drawerColor });
    $("drawer-title").textContent = `分类：${catName(drawerColor)}（${items.length}）`;
  }
}

// ---------- 彩签页详情抽屉内联改名 ----------
let drawerColor = null;
function resetCatRenameEdit() {
  $("cat-rename-edit").classList.add("hidden");
  $("drawer-title").classList.remove("hidden");
  $("btn-cat-rename").classList.remove("hidden");
}
$("btn-cat-rename").addEventListener("click", () => {
  if (drawerColor === null) return;
  $("cat-rename-input").value = catName(drawerColor);
  $("drawer-title").classList.add("hidden");
  $("btn-cat-rename").classList.add("hidden");
  $("cat-rename-edit").classList.remove("hidden");
  $("cat-rename-input").focus();
  $("cat-rename-input").select();
});
async function saveCatRename() {
  const name = $("cat-rename-input").value.trim();
  if (!name) { showToast("分类名不能为空"); $("cat-rename-input").focus(); return; }
  try {
    await invoke("rename_category", { color: drawerColor, name });
    resetCatRenameEdit();
    await refreshAfterCategoryRename();
  } catch (e) { showToast("改名失败：" + e); }
}
$("cat-rename-save").addEventListener("click", saveCatRename);
$("cat-rename-cancel").addEventListener("click", resetCatRenameEdit);
$("cat-rename-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") saveCatRename();
  else if (e.key === "Escape") resetCatRenameEdit();
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
  $("ai-input").value = ""; autoGrow($("ai-input"));
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  try {
    await invoke("ai_send", { text });
  } catch (e) {
    showToast("AI 提问失败：" + e);
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
  } catch (e) { showToast("重新生成失败：" + e); }
  state.aiGenerating = false;
  state.streamEl = null;
  $("btn-ai-stop").classList.add("hidden");
}
$("btn-local-api").addEventListener("click", () => {
  switchView("settings");
  setTimeout(() => $("local-api-section").scrollIntoView({ behavior: "smooth" }), 60);
});
/** AI 页副标题：如实说明默认 AI 当前走哪条路（本机 / 对方设备 / 自动）。 */
async function updateAiState() {
  try {
    const cfg = await invoke("get_ai_config");
    const ready = !!(cfg.has_key && cfg.base_url && cfg.model);
    state.aiLocalReady = ready;
    state.aiLocalModel = cfg.model || "";
    const src = await invoke("get_ai_source").catch(() => "auto");
    state.aiSource = src;
    const sel = $("ai-source");
    if (sel && document.activeElement !== sel) sel.value = src;
    const peerLabel = state.peerName || "对方设备";
    $("ai-state").textContent =
      src === "local"
        ? ready ? `默认AI：本机AI（${cfg.model}）` : "默认AI：本机AI（尚未配置）"
        : src === "peer"
          ? state.connected ? `默认AI：${peerLabel} 的 AI（经对方转发）` : "默认AI：对方设备 AI（当前未连接设备）"
          : ready
            ? "默认AI：自动（使用本机配置的 AI 接口）"
            : state.connected
              ? `默认AI：自动（本机未配置 · 经${peerLabel}转发回答）`
              : "默认AI：自动（本机未配置 AI，也未连接设备）";
  } catch (_) { /* 忽略 */ }
}
$("ai-source").addEventListener("change", async (e) => {
  await invoke("set_ai_source", { source: e.target.value }).catch(() => {});
  updateAiState();
});

// ---------- 聊天框 @AI ----------
// 输入 @ 弹出 AI 选项（本机AI / 对方设备AI / 默认AI），选定后这条消息不发给对方，
// 而是作为问题发给所选 AI，回答以「AI 回答」气泡落在当前聊天里（仅本机保存）。
let pendingAiRoute = null;
let chatAiBusy = false;
let aiPickBusy = false;

function aiRouteName(key) {
  if (key === "local") return "本机AI";
  if (key === "peer") return `${state.peerName || "对方设备"}的AI`;
  return "默认AI";
}

async function openAiPick() {
  if (aiPickBusy) return;
  aiPickBusy = true;
  try {
    await updateAiState();
    const box = $("ai-pick");
    const peerLabel = state.peerName || "对方设备";
    const items = [
      { key: "local", title: "本机AI", sub: state.aiLocalReady ? (state.aiLocalModel || "本机配置") : "未配置" },
      { key: "peer", title: `${peerLabel}的AI`, sub: state.connected ? "连接设备" : "未连接" },
      { key: "default", title: "默认AI", sub: "按 AI 页的默认AI 设置" },
    ];
    box.innerHTML = "";
    for (const it of items) {
      const row = document.createElement("div");
      row.className = "ai-pick-item";
      row.innerHTML = `<span class="ai-pick-title">@${esc(it.title)}</span><span class="muted small">${esc(it.sub)}</span>`;
      row.addEventListener("click", () => chooseAiRoute(it.key));
      box.appendChild(row);
    }
    box.classList.remove("hidden");
  } finally { aiPickBusy = false; }
}
function closeAiPick() { $("ai-pick").classList.add("hidden"); }
function chooseAiRoute(key) {
  const el = $("input");
  el.value = el.value.replace(/@$/, "");
  pendingAiRoute = key;
  closeAiPick();
  const bar = $("ai-route-bar");
  bar.innerHTML = `<span>提问给：${esc(aiRouteName(key))}（这条不发给对方）</span><button class="btn small-btn">取消</button>`;
  bar.querySelector("button").addEventListener("click", () => {
    pendingAiRoute = null;
    bar.classList.add("hidden");
    el.focus();
  });
  bar.classList.remove("hidden");
  el.focus();
}
$("input").addEventListener("input", () => {
  const v = $("input").value;
  if (/(^|\s)@$/.test(v)) openAiPick(); else closeAiPick();
});
$("input").addEventListener("keydown", (e) => {
  if ($("ai-pick").classList.contains("hidden")) return;
  if (e.key === "Enter") {
    e.preventDefault();
    e.stopPropagation();
    const first = $("ai-pick").querySelector(".ai-pick-item");
    if (first) first.click();
  }
  if (e.key === "Escape") closeAiPick();
}, true);

/** 聊天页「问AI」：把这条消息作为上下文发给默认 AI，跳到 AI 页看回答并可继续追问。 */
async function askAiAbout(m) {
  if (state.aiGenerating) { showToast("AI 正在生成中，请稍候"); return; }
  switchView("ai");
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  try {
    await invoke("ai_ask_message", { conversationId: m.conversation_id, messageId: m.id });
  } catch (e) {
    showToast("问AI 失败：" + e);
  } finally {
    state.aiGenerating = false;
    state.streamEl = null;
    $("btn-ai-stop").classList.add("hidden");
    loadAiMessages();
  }
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
  // 身份信息每次进设置页都刷新：下载目录等字段可能已被改动，缓存会显示旧值
  try { state.identity = await invoke("get_identity"); } catch (_) { /* 保留缓存兜底 */ }
  const id = state.identity;
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
    $("store-dir").textContent = s.download_dir || "-";
    $("store-dir").title = s.download_dir || "";
    $("store-log").textContent = s.log_dir || "-";
    $("store-log").title = s.log_dir || "";
  } catch (_) { /* 忽略 */ }
}
$("store-dir-change").addEventListener("click", async () => {
  try {
    const path = await window.__TAURI__.dialog.open({ multiple: false, directory: true });
    if (!path) return;
    const eff = await invoke("set_download_dir", { path });
    showToast("保存目录已改为：" + eff);
    loadStorageStats();
  } catch (e) { showToast("更改保存目录失败：" + e); }
});
$("store-dir-reset").addEventListener("click", async () => {
  try {
    const eff = await invoke("set_download_dir", { path: null });
    showToast("保存目录已恢复默认：" + eff);
    loadStorageStats();
  } catch (e) { showToast("恢复默认失败：" + e); }
});
$("store-log-open").addEventListener("click", async () => {
  try { await invoke("open_log_dir"); }
  catch (e) { showToast("打开日志目录失败：" + e); }
});
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
  if (!state.peerConvId) { showToast("还没有可导出的会话"); return; }
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
$("btn-export").addEventListener("click", () => exportCurrentConv().catch((e) => showToast("导出失败：" + e)));

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
    $("nav-logo").classList.toggle("linked", state.connected);
    if (!state.connected) ringClearAll();
    state.peerName = e.payload.peer_name;
    $("peer-name").textContent = state.peerName || "未连接";
    $("peer-state").textContent = state.connected ? "已连接" : "离线";
    $("connect-status").textContent = state.connected ? "" : $("connect-status").textContent;
    // 连接页的已连接卡与胶囊：连上时已连接卡替换本机设备卡，断开时恢复
    if (state.connected) {
      $("connected-name").textContent = state.peerName || "对方设备";
      $("connected-sub").textContent = state.connectTransport ? transportName(state.connectTransport) : "";
      $("connected-card").classList.remove("hidden");
      $("my-card").classList.add("hidden");
      $("bt-code-card").classList.add("hidden");
    } else {
      state.connectTransport = null;
      $("connected-card").classList.add("hidden");
      $("my-card").classList.remove("hidden");
    }
    updateCapsule();
    if (lastDevices.length) renderBtDevices(lastDevices);
    await loadConversations();
    if (state.connected) { switchView("chat"); loadChatMessages(); }
    else { loadLastPeer(); switchView(state.view); }
    updateAiState();
  });
  await listen("message_new", (e) => {
    const m = e.payload;
    ringPulse(m.sender === "me" ? "me" : "peer");
    if (m.conversation_id === state.aiConvId) appendMessage($("ai-messages"), m, state.renderedAi, { ai: true });
    else { appendMessage($("messages"), m, state.renderedChat, {}); rebuildDateRail(); }
  });
  await listen("message_status", (e) => {
    // 回执可能早于本地回显气泡渲染到达：找不到元素时稍后重试一次，避免状态标签停在「已发送」
    const apply = () => {
      const row = document.querySelector(`[data-mid="${e.payload.id}"] .msg-status`);
      if (row) applyStatus(row, e.payload.id, e.payload.status);
      return !!row;
    };
    if (!apply()) setTimeout(apply, 400);
  });
  await listen("peer_typing", (e) => {
    $("typing-hint").classList.toggle("hidden", !e.payload);
    if (e.payload) ringPulse("peer");
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
      // 传输进行中：对应环持续特效（我发出的看左环，对方发来的看右环），一个气泡只计一次
      if (received < total && !b.dataset.flowing) {
        const side = b.closest(".msg-row")?.dataset.sender === "me" ? "me" : "peer";
        b.dataset.flowing = side;
        ringSet(side, true);
      }
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
    // 传输结束（完成/取消/校验失败）：停掉对应环的持续特效
    if (b.dataset.flowing) { ringSet(b.dataset.flowing, false); delete b.dataset.flowing; }
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
  await listen("ai_error", (e) => showToast(e.payload.error));
}

// ---------- 主题 ----------
function applyTheme(mode) {
  if (mode === "system") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", mode);
  document.querySelectorAll("#theme-seg button").forEach((b) =>
    b.classList.toggle("active", b.dataset.themeSet === mode));
  // 原生窗口标题栏跟随主题（深色时不再是一条白杠）
  try {
    const w = window.__TAURI__.window.getCurrentWindow();
    w.setTheme(mode === "system" ? null : mode).catch(() => {});
  } catch (_) { /* 忽略 */ }
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
  // 屏蔽 WebView 默认右键菜单（浏览器式的 Back/Reload/Inspect 与本应用无关）；
  // 输入框内保留默认菜单，方便粘贴等编辑操作。
  document.addEventListener("contextmenu", (e) => {
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    e.preventDefault();
  });
  await loadCategories();
  state.identity = await invoke("get_identity");
  $("my-code").textContent = state.identity.code_display;
  $("my-name").textContent = state.identity.name;
  $("guide-my-name").textContent = state.identity.name;
  refreshBtState();
  invoke("get_dev_mode").then(applyDevMode).catch(() => {});
  invoke("get_bt_transport").then((t) => { btTransport = t; syncTransportSeg(); }).catch(() => {});
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
        if (!state.connected) { showToast("尚未连接设备，无法发送文件"); return; }
        for (const path of p.paths || []) {
          invoke("send_file_path", { path }).catch((err) => showToast("文件发送失败：" + err));
        }
      } else {
        $("drop-overlay").classList.add("hidden");
      }
    });
  } catch (e) { console.error("拖拽监听注册失败", e); }
  await loadConversations();
  loadLastPeer();
  if (state.identity.connected) {
    state.connected = true;
    switchView("chat");
    loadChatMessages();
  } else {
    switchView("chat");
  }
})();
