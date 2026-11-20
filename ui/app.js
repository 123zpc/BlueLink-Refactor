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
  pendingFiles: [],
  transferMap: {},
  quoteFrom: null,
  favIds: new Set(),
  pins: [],
  pinIds: new Set(),
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
  bubble.className = "bubble" + (m.kind === "file" ? " file-card" : "") + (m.kind === "imagegroup" ? " group-card" : "");
  if (m.reply_to && m.kind === "text" && !opts.ai) {
    const q = document.createElement("div");
    q.className = "quote-block";
    q.textContent = "引用加载中…";
    q.addEventListener("click", () => jumpToQuoted(m));
    bubble.appendChild(q);
    fillQuote(q, m);
  }
  if (m.kind === "imagegroup") {
    buildGroupBubble(bubble, m, row);
  } else if (m.kind === "file") {
    bubble.innerHTML = `<div class="file-card-inner"><span class="file-ico">${iconUse("i-file", "ico ico-20")}</span><span class="file-main"><span class="file-name">${esc(m.body)}</span><span class="file-meta">文件</span></span></div><div class="progress hidden"><div></div></div><div class="progress-meta hidden"></div><button class="progress-cancel hidden">取消</button><button class="transfer-retry hidden">重试</button>`;
    state.lastFileBubble = bubble;
    bubble.addEventListener("click", (e) => {
      if (e.target.closest("button")) return;
      openFileSmart(m, e.clientX, e.clientY);
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
  // 图片组的正文是清单 JSON，引用/问AI 对它无意义，不给这两个入口
  if (!opts.ai && m.kind !== "imagegroup") btns.push(["引用", () => setReply(m)], ["问AI", () => askAiAbout(m)]);
  if (opts.ai && m.sender === "ai") btns.push(["复制", () => navigator.clipboard.writeText(m.body)], ["重新生成", () => regenerateAi()]);
  for (const [label, fn] of btns) {
    const b = document.createElement("button");
    b.textContent = label;
    if (label === "收藏") { b.classList.add("fav-btn"); paintFavBtn(b, state.favIds.has(m.id)); }
    if (label === "置顶") { b.classList.add("pin-btn"); paintPinBtn(b, state.pinIds.has(m.id)); }
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
  row.addEventListener("contextmenu", (e) => { e.preventDefault(); openMsgMenu(m, e.clientX, e.clientY); });
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
    el.textContent = orig.kind === "imagegroup"
      ? `${who}：[图片组]`
      : `${who}：${orig.body.slice(0, 80)}`;
  } catch (_) { el.textContent = "引用消息"; }
}

/** 「回到底部」与「回到回复」互斥（v2.5.1）：待返回状态（quoteFrom）存在时
 * 回到回复优先，回到底部不显示（两钮同角上下叠，用户实测撞车）；待返回
 * 清除后回到底部按原规矩（离底 >400px）恢复。两钮各自原行为不动。 */
function syncBottomBtn() {
  const c = $("messages");
  const dist = c.scrollHeight - c.scrollTop - c.clientHeight;
  $("btn-bottom").classList.toggle("hidden", dist <= 400 || !!state.quoteFrom);
}

/** 点引用块跳到原消息，并亮出「回到回复」按钮一键跳回。 */
function jumpToQuoted(m) {
  state.quoteFrom = m.id;
  locateMessage(m.conversation_id, m.reply_to);
  const btn = $("back-to-reply");
  btn.classList.remove("hidden");
  syncBottomBtn();
  btn.onclick = () => {
    btn.classList.add("hidden");
    if (state.quoteFrom) locateMessage(m.conversation_id, state.quoteFrom);
    state.quoteFrom = null;
    syncBottomBtn();
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
    chip.addEventListener("contextmenu", (e) => { e.preventDefault(); e.stopPropagation(); openRenamePop(mc.color, e.currentTarget); });
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
  el.dataset.seq = m.seq || 0;
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
  closePinList();
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
  await refreshFavIds();
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
    // 游标只认本机入账序号 seq（v2.5.1）：旧版按 created_at 严格小于翻页，
    // 与游标同毫秒的消息会被永久吞掉，且双机时钟差下墙钟序本身就是乱序。
    const beforeSeq = Number(first.dataset.seq || 0);
    const older = await invoke("list_messages", { conversationId: state.peerConvId, before: beforeSeq > 0 ? beforeSeq : null, limit: CHAT_PAGE });
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
  syncBottomBtn();
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

/** 置顶钮的已置顶态绘制（与收藏同口径：文案 + 强调色，CSS .marked）。 */
function paintPinBtn(b, on) {
  b.classList.toggle("marked", !!on);
  b.textContent = on ? "已置顶" : "置顶";
}

/** 同步全部置顶 id 集（按钮态/菜单文案据此画，不按会话过滤）。 */
async function refreshPinIds() {
  try {
    const pins = await invoke("list_marked", { kind: "pin" });
    state.pinIds = new Set(pins.map((m) => m.id));
    return pins;
  } catch (_) { return null; }
}

async function refreshPinBar() {
  try {
    const pins = await refreshPinIds();
    if (!pins) return;
    const mine = pins.filter((m) => m.conversation_id === state.peerConvId);
    state.pins = mine;
    const bar = $("pin-bar");
    if (!mine.length) { bar.classList.add("hidden"); closePinList(); return; }
    bar.classList.remove("hidden");
    // 1 条：保持现行文案与点击直跳；≥2 条：文案提示可多选，点击先弹列表
    bar.innerHTML = mine.length === 1
      ? `${iconUse("i-pin", "ico ico-14")}<span>置顶：${esc(mine[0].body.slice(0, 40))}</span>`
      : `${iconUse("i-pin", "ico ico-14")}<span>置顶消息 · 共 ${mine.length} 条（点击选择）</span>`;
    bar.onclick = () => {
      if (state.pins.length === 1) locateMessage(state.pins[0].conversation_id, state.pins[0].id);
      else togglePinList();
    };
  } catch (_) { /* 忽略 */ }
}

// ---------- 置顶消息列表（≥2 条时点置顶栏弹出，点哪条跳哪条） ----------
let pinListOpen = false;
function togglePinList() { pinListOpen ? closePinList() : openPinList(); }
function openPinList() {
  const list = $("pin-list");
  const bar = $("pin-bar");
  list.innerHTML = "";
  const head = document.createElement("div");
  head.className = "rl-head";
  head.innerHTML = `<b>置顶消息 · 共 ${state.pins.length} 条</b>`;
  list.appendChild(head);
  for (const m of state.pins) {
    const item = document.createElement("div");
    item.className = "rl-item";
    const main = m.kind === "file" ? `[文件] ${m.body}` : m.body;
    item.innerHTML = `<div class="rl-note">${esc(main)}</div><div class="rl-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div>`;
    item.title = main;
    // 回跳复用 locateMessage：目标不在已加载首屏时它会整段加载兜底定位
    item.addEventListener("click", () => { closePinList(); locateMessage(m.conversation_id, m.id); });
    list.appendChild(item);
  }
  list.style.top = (bar.offsetTop + bar.offsetHeight + 4) + "px";
  list.classList.remove("hidden");
  pinListOpen = true;
}
function closePinList() {
  pinListOpen = false;
  const list = $("pin-list");
  if (list) list.classList.add("hidden");
}
document.addEventListener("mousedown", (e) => {
  if (!pinListOpen) return;
  const list = $("pin-list");
  const bar = $("pin-bar");
  if (!list.contains(e.target) && !bar.contains(e.target)) closePinList();
});
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closePinList(); });

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

// ---------- 待发文件队列（多文件批量发送，用户 2026-11-20 定版） ----------
// 附件按钮多选、输入框粘贴文件、拖放文件三个入口都汇入这条队列；
// 点发送时按顺序逐个传（每个文件一条文件消息、各自进度），单个失败
// 提示并跳过、不堵住后面。发送前可在附件条逐个移除、也可一键清空。
/** 待发队列里的图片数（按扩展名判定，与查看器/图片组共用同一口径）。 */
function pendingImageCount() {
  return state.pendingFiles.filter((f) => isImageExt(fileExt(f.name))).length;
}

function renderAttachBar() {
  const bar = $("attach-bar");
  const list = $("attach-list");
  list.innerHTML = "";
  for (const f of state.pendingFiles) {
    const chip = document.createElement("span");
    chip.className = "attach-chip";
    const label = document.createElement("span");
    label.className = "attach-name";
    label.textContent = `${f.name} · ${fmtSize(f.size)}`;
    label.title = f.path;
    const x = document.createElement("button");
    x.className = "attach-x";
    x.textContent = "×";
    x.title = "移除";
    x.addEventListener("click", () => {
      state.pendingFiles = state.pendingFiles.filter((q) => q.path !== f.path);
      renderAttachBar();
    });
    chip.appendChild(label);
    chip.appendChild(x);
    list.appendChild(chip);
  }
  // 图片组开关：队列里图片超过 3 张才出现（定版口径），默认开启；
  // 用户手动改过就尊重其选择，直到队列清空才复位默认。
  const grow = $("attach-group-row");
  const nImg = pendingImageCount();
  if (!state.pendingFiles.length) state.attachGroupTouched = false;
  if (nImg > 3) {
    if (grow.classList.contains("hidden") && !state.attachGroupTouched) {
      $("attach-group-toggle").checked = true;
    }
    $("attach-group-label").textContent = `把 ${nImg} 张图片合并为一组发送`;
    grow.classList.remove("hidden");
  } else {
    grow.classList.add("hidden");
  }
  bar.classList.toggle("hidden", state.pendingFiles.length === 0);
}
$("attach-group-toggle").addEventListener("change", () => { state.attachGroupTouched = true; });
$("attach-clear").addEventListener("click", () => {
  state.pendingFiles = [];
  renderAttachBar();
});

/** 把一批路径逐个校验入队（不存在/不是文件 → 提示并跳过，不连累其余）；
 * 同路径重复添加自动去重。返回实际入队数量。 */
async function stageFilePaths(paths) {
  let added = 0;
  for (const path of paths || []) {
    if (!path || state.pendingFiles.some((q) => q.path === path)) continue;
    try {
      const info = await invoke("file_attach_info", { path });
      state.pendingFiles.push({ path, name: info.name, size: info.size });
      added++;
    } catch (err) {
      const base = String(path).split(/[\\/]/).pop() || path;
      showToast(`无法添加「${base}」：${err}`);
    }
  }
  if (added) renderAttachBar();
  return added;
}

/** 粘贴的图片位图落盘入队（截图粘贴的老路径，行为与旧版一致）。 */
async function stagePastedImageFile(file) {
  try {
    const buf = await file.arrayBuffer();
    const bytes = new Uint8Array(buf);
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    const ext = (file.type.split("/")[1] || "png").replace("jpeg", "jpg");
    const path = await invoke("stage_pasted_image", { dataBase64: btoa(bin), ext });
    state.pendingFiles.push({ path, name: `粘贴的图片.${ext}`, size: file.size });
    renderAttachBar();
  } catch (err) { showToast("粘贴图片失败：" + err); }
}

// 粘贴分流（定版规则，2026-11-18 + 2026-11-20 合并）：
// ①剪贴板带普通文字 → 文字优先，永远按文字粘（Excel 等场景），不处理；
// ②带文件对象 → 文件优先：向系统剪贴板要真实路径（文件管理器里复制
//   的文件），逐个入待发队列，绝不把文件路径当文字粘进输入框；
// ③没有文件对象路径但有一张图片位图 → 截图粘贴，落盘入队（旧规则）；
// ④其余（平台读不到文件列表/虚拟文件）→ 诚实提示，不粘任何东西。
// 注意：文件对象在各 WebView 的 DataTransfer 里暴露程度不一（WebKitGTK
// 对 uri-list 剪贴板甚至既不给 text 也不给 files），故分流的权威来源是
// 后端读到的系统剪贴板文件列表；同步先 preventDefault 掐掉默认粘贴
// （防 uri 文本被塞进输入框），再异步定性。文字若长得纯像路径列表，
// 也先拦下核对：剪贴板真有文件对象才按文件走，否则按文字粘回。
function insertTextAtCursor(text) {
  const ta = $("input");
  const s = ta.selectionStart ?? ta.value.length;
  const e2 = ta.selectionEnd ?? ta.value.length;
  ta.value = ta.value.slice(0, s) + text + ta.value.slice(e2);
  ta.selectionStart = ta.selectionEnd = s + text.length;
  ta.dispatchEvent(new Event("input", { bubbles: true }));
  autoGrow(ta);
}
$("input").addEventListener("paste", async (e) => {
  const cd = e.clipboardData;
  if (!cd) return;
  const text = cd.getData("text/plain") || "";
  const lines = text.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  const pathListOnly = lines.length > 0
    && lines.every((l) => /^([a-zA-Z]:[\\/]|\\\\|\/)/.test(l));
  if (text.trim() && !pathListOnly) return; // 普通文字：原样粘贴
  const fileItems = Array.from(cd.items || []).filter((i) => i.kind === "file");
  // 无文字（或纯路径样文字）一律先同步拦下默认粘贴，再异步定性：
  // DataTransfer 对文件对象的暴露各引擎不一（WebKitGTK 对 uri-list
  // 剪贴板甚至什么都不给），权威来源是后端读到的系统剪贴板。
  e.preventDefault();
  let paths = null;
  try { paths = await invoke("clipboard_file_list"); } catch (_) { paths = null; }
  if (paths && paths.length) { await stageFilePaths(paths); return; }
  if (pathListOnly) { insertTextAtCursor(text); return; } // 只是像路径的文字
  const img = fileItems.find((i) => i.type.startsWith("image/"));
  if (img) {
    const f = img.getAsFile();
    if (f) { await stagePastedImageFile(f); return; }
  }
  if (fileItems.length || (cd.files && cd.files.length)) {
    showToast(paths === null ? "此系统暂未支持粘贴文件，请用附件按钮选择" : "未能读取粘贴的文件，已忽略");
  }
});

async function sendCurrent() {
  const raw = $("input").value;
  const text = raw.trim();
  if (!text && !state.pendingFiles.length) return;
  // 待发文件队列先逐个发（文件气泡由后端事件回推，与选文件发送一致；
  // 每个文件一条消息、各自进度；单个失败提示并跳过、不堵住后面）
  if (state.pendingFiles.length) {
    const queue = state.pendingFiles;
    if (!state.connected) {
      // 未连接时保留队列（连上后点发送即可），只提示不清空
      showToast("尚未连接设备，文件未发送");
    } else {
      // 图片组（v2.6.0 定版）：队列里图片超过 3 张且合并开关开着时，
      // 图片子集整体走一组发送（保持相对顺序，在第一张图片的位置
      // 整组发出）；其余文件仍逐个发。开关状态要在清队列前读。
      const groupOn = pendingImageCount() > 3 && $("attach-group-toggle").checked;
      state.pendingFiles = [];
      renderAttachBar();
      let groupSent = false;
      for (const f of queue) {
        if (groupOn && isImageExt(fileExt(f.name))) {
          if (groupSent) continue;
          groupSent = true;
          const paths = queue.filter((q) => isImageExt(fileExt(q.name))).map((q) => q.path);
          try { await invoke("send_image_group", { paths }); }
          catch (e) { showToast(`图片组发送失败：${e}`); }
          continue;
        }
        try { await invoke("send_file_path", { path: f.path }); }
        catch (e) { showToast(`文件「${f.name}」发送失败：${e}`); }
      }
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
    if (!(await aiReadyOrRemind())) return;
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
    // 多选入待发队列（与粘贴/拖放同一条队列，点发送键统一逐个发）
    const picked = await window.__TAURI__.dialog.open({ multiple: true, directory: false });
    if (!picked) return;
    await stageFilePaths(Array.isArray(picked) ? picked : [picked]);
    $("input").focus();
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
    else if (String(s.simpleble_message).includes("启动失败")) { dot.className = "status-dot warn"; txt.textContent = "SimpleBLE 启动失败 · 已回落经典蓝牙"; }
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
    else if (String(s.simpleble_message).includes("启动失败")) lines.push(line("warn", "SimpleBLE：启动失败，已回落经典蓝牙", true, s.simpleble_message));
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
let lastRenderSig = "";
function renderBtDevices(devices) {
  lastDevices = devices;
  if (scanning) hasScanned = true;
  // 「上次连接」自动连检查必须每次快照都跑（不许被下面的渲染节流跳过：
  // 目标已在列表里时，快照可能与上次完全相同）
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
      invoke("bt_connect", { deviceId: p.deviceId, code: p.code, transport: p.transport, name: hit.name || "", rssi: hit.rssi ?? null })
        .then(() => saveLastPeer({ transport: p.transport, deviceId: p.deviceId, code: p.code, name: hit.name }))
        .catch((e) => { $("last-peer-status").textContent = ""; $("connect-error").textContent = String(e); });
      if (scanning) { invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
    }
  }
  // 渲染节流：快照与上次实质相同（设备/顺序/信号格/选中/连接徽标都没变）就
  // 不重建 DOM——后端每秒推一帧，RSSI 数值小抖若每次都重建，列表会闪、
  // 行上的按钮焦点也会丢。信号格按格数档位进签名，格数变了照样刷新。
  const sig = devices.map((d) => `${d.id}|${d.name}|${d.paired ? 1 : 0}|${d.rssi == null ? "-" : Math.round(d.rssi / 10)}`).join(";")
    + `#${state.btDevice || ""}|${state.connected ? 1 : 0}|${state.peerName || ""}|${state.lastPeer ? state.lastPeer.deviceId : ""}|${scanning ? 1 : 0}|${effectiveTransport()}`;
  if (sig === lastRenderSig) return;
  lastRenderSig = sig;
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
// 版本号手势（用户 2026-11-20 定版，覆盖先前分开规格）：统一一个点击计数
// 处理——连点三下同时触发两件事：撒花彩蛋 + 开发者模式开关，二者合一；
// 双击什么都不发生。版本号不可选中（CSS user-select:none + 此处拦
// selectstart/dblclick），手势全程不碰剪贴板（旧版双击会选中文本被系统复制）。
let versionClicks = 0;
let versionClickTimer = null;
$("set-version").addEventListener("selectstart", (e) => e.preventDefault());
$("set-version").addEventListener("dblclick", (e) => e.preventDefault());
$("set-version").addEventListener("click", async () => {
  versionClicks += 1;
  clearTimeout(versionClickTimer);
  versionClickTimer = setTimeout(() => { versionClicks = 0; }, 900);
  if (versionClicks < 3) return;
  versionClicks = 0;
  clearTimeout(versionClickTimer);
  showVersionEgg();
  const on = !state.devMode;
  try {
    await invoke("set_dev_mode", { on });
    applyDevMode(on);
    showToast(on ? "开发者模式已开启" : "开发者模式已关闭");
  } catch (e) { showToast("切换开发者模式失败：" + e); }
});

/** 三击彩蛋：以版本号为中心喷一波彩带彩花（canvas 轻量实现），配色用彩签
 * 9 色 + 品牌蓝；标志扣合作引子（小字层动画）。指针穿透不挡操作；重触发
 * 先清旧场再重播一波；两三秒飘落散尽自清。 */
let eggAnim = null;
function showVersionEgg() {
  const egg = $("ver-egg");
  const cv = $("egg-canvas");
  const ctx = cv.getContext("2d");
  if (eggAnim) cancelAnimationFrame(eggAnim);
  egg.classList.remove("hidden");
  const cap = $("egg-caption");
  cap.classList.remove("play");
  void cap.offsetWidth;
  cap.classList.add("play");
  cv.width = window.innerWidth;
  cv.height = window.innerHeight;
  const v = $("set-version").getBoundingClientRect();
  const ox = v.left + v.width / 2;
  const oy = v.top + v.height / 2;
  const colors = [...CAT_COLORS, "#2563EB"];
  const parts = [];
  for (let i = 0; i < 170; i++) {
    const ang = Math.random() * Math.PI * 2;
    const sp = 4 + Math.random() * 9.5;
    parts.push({
      x: ox, y: oy,
      vx: Math.cos(ang) * sp, vy: Math.sin(ang) * sp - 4.5,
      w: 5 + Math.random() * 6, h: 3 + Math.random() * 5,
      rot: Math.random() * Math.PI, vr: (Math.random() - 0.5) * 0.32,
      color: colors[i % colors.length],
      ribbon: Math.random() < 0.35,
      ph: Math.random() * 6.28,
    });
  }
  const t0 = performance.now();
  const tick = (t) => {
    const el = t - t0;
    ctx.clearRect(0, 0, cv.width, cv.height);
    let alive = false;
    for (const pt of parts) {
      pt.vy += 0.16;
      pt.vx *= 0.99;
      pt.vy *= 0.995;
      pt.x += pt.vx + Math.sin(el / 160 + pt.ph) * 0.6;
      pt.y += pt.vy;
      pt.rot += pt.vr;
      const fade = el < 2000 ? 1 : Math.max(0, 1 - (el - 2000) / 800);
      if (pt.y < cv.height + 30 && fade > 0) {
        alive = true;
        ctx.save();
        ctx.globalAlpha = fade;
        ctx.translate(pt.x, pt.y);
        ctx.rotate(pt.rot);
        ctx.fillStyle = pt.color;
        if (pt.ribbon) ctx.fillRect(-pt.w, -1.2, pt.w * 2, 2.4);
        else ctx.fillRect(-pt.w / 2, -pt.h / 2, pt.w, pt.h);
        ctx.restore();
      }
    }
    if (alive && el < 2900) eggAnim = requestAnimationFrame(tick);
    else {
      eggAnim = null;
      ctx.clearRect(0, 0, cv.width, cv.height);
      egg.classList.add("hidden");
    }
  };
  eggAnim = requestAnimationFrame(tick);
}
$("btn-listen").addEventListener("click", async () => {
  $("connect-error").textContent = "";
  try {
    const addr = await invoke("start_listening", { port: 7800 });
    $("connect-status").textContent = `正在等待局域网连接（${addr}）…`;
  } catch (e) { $("connect-error").textContent = String(e); }
});
let lanConnecting = false;
$("btn-connect").addEventListener("click", async () => {
  if (lanConnecting) return;
  $("connect-error").textContent = "";
  const addr = $("peer-addr").value.trim();
  const code = $("peer-code").value.trim();
  if (!addr || !code) { $("connect-error").textContent = "请填写对方地址和连接码"; return; }
  lanConnecting = true;
  const my = ++connectAttempt;
  $("connect-status").textContent = "正在连接…";
  $("btn-connect").disabled = true;
  $("btn-lan-cancel").classList.remove("hidden");
  state.connectTransport = "lan";
  try {
    const name = await invoke("connect_to", { addr, code });
    if (my !== connectAttempt) return; // 已取消：后端会自行收尾迟到的连接
    saveLastPeer({ transport: "lan", addr, code, name: name || "对方设备" });
  } catch (e) {
    if (my !== connectAttempt) return; // 取消引发的失败不弹错
    $("connect-error").textContent = String(e);
    $("connect-status").textContent = "";
  } finally {
    if (my === connectAttempt) {
      lanConnecting = false;
      $("btn-connect").disabled = false;
      $("btn-lan-cancel").classList.add("hidden");
    }
  }
});
$("btn-lan-cancel").addEventListener("click", () => {
  if (!lanConnecting) return;
  // 立即收尾不等底层：作废尝试号 + 通知后端作废令牌，界面当场恢复
  connectAttempt++;
  lanConnecting = false;
  $("btn-connect").disabled = false;
  $("btn-lan-cancel").classList.add("hidden");
  $("connect-status").textContent = "";
  invoke("cancel_connect").catch(() => {});
});

// ---------- 输码卡（右栏第一张卡：点设备后替换本机设备卡） ----------
let connecting = false;

/** 打开输码卡：高亮选中行，输码卡排在本机设备卡正上方，输入框聚焦。
 * 本机设备卡保持可见（用户 2026-11-19 裁定）：输码/连接中/失败时也要能
 * 一眼看到自己的连接码，不许被挤出第一屏、更不许藏掉。 */
function openCodeCard(d) {
  state.btDevice = d.id;
  state.btDeviceName = d.name;
  renderBtDevices(lastDevices);
  $("code-card-title").textContent = `连接到 ${d.name}`;
  setCodeError("");
  $("bt-code-card").classList.remove("hidden");
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

/** 连接尝试号（前端侧）：每次发起连接 +1，点取消再 +1 作废；
 * invoke 的迟到结果回来时先对号，作废的那次不许再动 UI（后端另有令牌
 * 保证迟到连接会被自行断开、不进会话、不写上次连接记录）。
 * 与局域网连接共用一个计数（同一时刻只会有一笔连接在飞）。 */
let connectAttempt = 0;

/** 连接中态：连接按钮转圈禁用、输入禁用；取消按钮必须保持可用——
 * 旧版把取消一并禁用，连接干等时点取消毫无反应（用户真机实证的缺陷）。 */
function setConnectingUI(on) {
  connecting = on;
  const btn = $("btn-bt-connect");
  btn.disabled = on;
  btn.innerHTML = on ? `<span class="spinner"></span>连接中…` : "连接";
  $("bt-peer-code").disabled = on;
}

$("btn-bt-cancel").addEventListener("click", () => {
  if (connecting) {
    // 立即退回浏览态不等底层返回：作废尝试号 + 后端令牌，关卡复位
    connectAttempt++;
    setConnectingUI(false);
    closeCodeCard();
    invoke("cancel_connect").catch(() => {});
  } else {
    closeCodeCard();
  }
});
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
  const my = ++connectAttempt;
  setConnectingUI(true);
  try {
    await invoke("bt_connect", { deviceId: state.btDevice, code, transport: t, name: state.btDeviceName || "", rssi: (lastDevices.find((d) => d.id === state.btDevice) || {}).rssi ?? null });
    if (my !== connectAttempt) return; // 已取消：迟到的成功由后端自行断开
    saveLastPeer({ transport: t, deviceId: state.btDevice, code, name: state.btDeviceName || "对方设备" });
    if (scanning) { await invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
    setConnectingUI(false);
    closeCodeCard();
  } catch (e) {
    if (my !== connectAttempt) return; // 取消引发的失败不弹错（卡片已关）
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

/** 收藏钮的已收藏态绘制（收藏/已收藏 + 强调色，CSS .marked）。 */
function paintFavBtn(b, on) {
  b.classList.toggle("marked", !!on);
  b.textContent = on ? "已收藏" : "收藏";
}

/** 刷新已收藏 id 集：消息渲染时据此画收藏钮状态（与落库数据一致）。 */
async function refreshFavIds() {
  try {
    const favs = await invoke("list_marked", { kind: "favorite" });
    state.favIds = new Set(favs.map((m) => m.id));
  } catch (_) { /* 拿不到就按未收藏画，点了以后端返回为准 */ }
}

async function toggleMark(mid, kind) {
  try {
    const on = await invoke("toggle_mark", { messageId: mid, kind });
    if (kind === "pin") {
      // 补反馈（v2.5.2，与收藏同口径）：旧版置顶落库没问题、置顶栏也会
      // 刷新，但没 toast、按钮与菜单文案纹丝不动，用户感觉「跟没起效
      // 一样」（2026-11-20 实报）。现在触发点立刻变已置顶态 + toast，
      // 取消同理复原；置顶栏随后端数据刷新（await 到位再返回）。
      if (on) state.pinIds.add(mid); else state.pinIds.delete(mid);
      document.querySelectorAll(`[data-mid="${mid}"] .pin-btn`).forEach((b) => paintPinBtn(b, on));
      showToast(on ? "已置顶" : "已取消置顶");
      await refreshPinBar();
    }
    if (kind === "favorite") {
      // 补反馈（v2.5.1）：旧版收藏落库没问题但界面纹丝不动，看起来像没用。
      // 现在触发点立刻变已收藏态 + toast，取消同理复原。
      if (on) state.favIds.add(mid); else state.favIds.delete(mid);
      document.querySelectorAll(`[data-mid="${mid}"] .fav-btn`).forEach((b) => paintFavBtn(b, on));
      showToast(on ? "已收藏" : "已取消收藏");
      if (state.view === "favorites") loadFavList();
    }
    return on;
  } catch (e) { showToast("操作失败：" + e); }
}

async function loadFavList() {
  const list = $("fav-list");
  let msgs;
  try {
    msgs = await invoke("list_marked", { kind: "favorite" });
  } catch (e) {
    list.innerHTML = `<div class="list-item"><div class="li-sub">收藏加载失败：${esc(String(e))}</div></div>`;
    return;
  }
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

/** 复制文字到系统剪贴板（与 AI 回答「复制」同一条 navigator.clipboard 路径），
 * 成败都给 toast，不静默。 */
async function copyText(text) {
  try { await navigator.clipboard.writeText(text); showToast("已复制"); }
  catch (_) { showToast("复制失败：当前系统剪贴板不可用"); }
}

/** 消息右键菜单（v2.5.1 补齐）：文字消息给「复制」；文件消息（含图片——
 * 都是落盘文件）复用文件菜单，「打开文件位置」「复制文件」都在里面；
 * 其余项与悬停动作一致（收藏/置顶/引用/问AI）。菜单容器与文件菜单共用
 * #file-menu（点外关闭逻辑现成）。 */
function openMsgMenu(m, x, y) {
  if (m.kind === "file") { openFileMenu(m.id, x, y); return; }
  if (m.kind === "imagegroup") { openGroupMenu(m, x, y); return; }
  if (m.kind === "system") return;
  const menu = $("file-menu");
  menu.innerHTML = "";
  const items = [
    ["复制", () => copyText(m.body)],
    ["收藏", () => toggleMark(m.id, "favorite")],
    [state.pinIds.has(m.id) ? "取消置顶" : "置顶", () => toggleMark(m.id, "pin")],
    ["引用", () => setReply(m)],
    ["问AI", () => askAiAbout(m)],
  ];
  for (const [label, fn] of items) {
    const b = document.createElement("button");
    b.textContent = label;
    b.addEventListener("click", async () => {
      menu.classList.add("hidden");
      try { await fn(); } catch (e) { showToast("操作失败：" + e); }
    });
    menu.appendChild(b);
  }
  menu.style.left = Math.min(x, window.innerWidth - 190) + "px";
  menu.style.top = Math.min(y, window.innerHeight - 240) + "px";
  menu.classList.remove("hidden");
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
      // 首选后端原生实现：Windows 写 CF_HDROP 文件本体（等同资源管理器
      // Ctrl+C，用户 2026-11-20 要求，Windows 上不再回退路径）；后端
      // 报 UNSUPPORTED 的平台（macOS/Linux 现状）维持旧有回退链。
      try {
        await invoke("copy_file_to_clipboard", { path: info.path });
        showToast("已复制文件");
        return;
      } catch (e) {
        if (!String(e).startsWith("UNSUPPORTED")) {
          showToast("复制失败：" + e);
          return;
        }
      }
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
    hint.textContent = "文件已不在原位置（可能尚未接收完成或已被移动）";
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
  await refreshFavIds();
  await refreshPinIds();
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
  if (!(await aiReadyOrRemind())) return;
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
  if (!(await aiReadyOrRemind())) return;
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
/** 带一个动作按钮的 toast（如未配置 AI 时的「去配置」直达设置页）。 */
function showToastAction(text, label, fn) {
  const t = $("toast");
  t.innerHTML = "";
  const span = document.createElement("span");
  span.textContent = text;
  const b = document.createElement("button");
  b.className = "toast-action";
  b.textContent = label;
  b.addEventListener("click", () => { t.classList.add("hidden"); fn(); });
  t.appendChild(span);
  t.appendChild(b);
  t.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), 4500);
}

/** AI 未配置拦截（v2.5.1）：点任一 AI 入口先判定，不许静默、不许发出去再失败。
 * - 默认来源 = 对方设备：走对方配置，本机未配置不拦；
 * - 自动模式且已连接：可经对方转发，不拦（对方未开代理等由后端错误提示）；
 * - 其余未配置情形：立刻提醒「还没有配置 AI」并给「去配置」直达设置页。 */
async function aiReadyOrRemind() {
  try {
    const src = await invoke("get_ai_source");
    if (src === "peer") return true;
    const cfg = await invoke("get_ai_config");
    if (cfg && cfg.configured) return true;
    if (src === "auto" && state.connected) return true;
    showToastAction("还没有配置 AI", "去配置", () => switchView("settings"));
    return false;
  } catch (_) { return true; /* 判定本身失败不误拦，交给后端报错 */ }
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
  await loadClipboardSync();
  loadNotifySetting();
  // 存储统计不在开页主链上等：先画缓存值/占位，统计回来再回填（v2.5.1 修
  // 设置页开页卡顿——后端统计已挪阻塞线程，前端也不再串行 await 它）
  loadStorageStats();
  updateAiState();
}
async function loadNotifySetting() {
  try { $("notify-toggle").checked = await invoke("get_sys_notify"); } catch (_) { /* 忽略 */ }
}
$("notify-toggle").addEventListener("change", async () => {
  try { await invoke("set_sys_notify", { on: $("notify-toggle").checked }); }
  catch (e) { showToast("设置失败：" + e); }
});
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
function clipStatusText(v) {
  if (!v || !v.local) return "未开启（双方都开启后才会同步）";
  if (!v.connected) return "已开启，等待连接";
  if (!v.peer) return "对方未开启";
  return "已同步";
}
async function loadClipboardSync() {
  try {
    const v = await invoke("get_clipboard_sync");
    $("clip-sync-toggle").checked = !!v.local;
    $("clip-sync-status").textContent = clipStatusText(v);
  } catch (_) { /* 忽略 */ }
}
let lastStorageStats = null; // 上次统计结果：进设置页先画它，不让用户对着旧占位干等
async function loadStorageStats() {
  if (lastStorageStats) paintStorageStats(lastStorageStats);
  else {
    $("store-chat").textContent = "计算中…";
    $("store-files").textContent = "计算中…";
    $("store-cache").textContent = "计算中…";
  }
  try {
    const s = await invoke("storage_stats");
    lastStorageStats = s;
    paintStorageStats(s);
  } catch (_) { /* 忽略 */ }
}
function paintStorageStats(s) {
  // 都是固定行内的 textContent 回填，不新增/删除节点，不引发布局跳动
  $("store-chat").textContent = `${fmtSize(s.chat_bytes)} · ${s.chat_count} 条`;
  $("store-files").textContent = `${fmtSize(s.files_bytes)} · ${s.files_count} 个`;
  $("store-cache").textContent = fmtSize(s.cache_bytes);
  $("store-dir").textContent = s.download_dir || "-";
  $("store-dir").title = s.download_dir || "";
  $("store-log").textContent = s.log_dir || "-";
  $("store-log").title = s.log_dir || "";
}
$("store-log-change").addEventListener("click", async () => {
  try {
    const path = await window.__TAURI__.dialog.open({ multiple: false, directory: true });
    if (!path) return;
    const eff = await invoke("set_log_dir", { path });
    showToast("日志目录已改为：" + eff);
    lastStorageStats = null;
    loadStorageStats();
  } catch (e) { showToast("更改日志目录失败：" + e); }
});
$("store-log-reset").addEventListener("click", async () => {
  try {
    const eff = await invoke("set_log_dir", { path: null });
    showToast("日志目录已恢复默认：" + eff);
    lastStorageStats = null;
    loadStorageStats();
  } catch (e) { showToast("恢复默认失败：" + e); }
});
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
$("clip-sync-toggle").addEventListener("change", async (e) => {
  await invoke("set_clipboard_sync", { on: e.target.checked });
  showToast(e.target.checked ? "剪贴板同步已开启（对方也开启后生效）" : "剪贴板同步已关闭");
  loadClipboardSync();
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
  await listen("notification_activated", (e) => {
    // 点系统通知（Windows WinRT 激活回调）回窗：后端已把窗口带回前台，
    // 这里只负责打开对应会话（设备会话回聊天页，AI 会话回 AI 页）。
    const cid = e.payload && e.payload.conversation_id;
    if (cid && (cid === state.aiConvId || cid.startsWith("ai"))) { switchView("ai"); return; }
    switchView("chat");
    if (state.connected) loadChatMessages();
  });
  await listen("message_updated", (e) => {
    // 已有消息内容变化（图片组接收端逐张并入清单）：按 id 重建该行，
    // 位置与已渲染集合不变；不在当前列表里就忽略（下次加载自然带出）。
    const m = e.payload;
    if (!m || m.conversation_id === state.aiConvId) return;
    registerGroupMembers(m);
    delete state.groupFiles[m.id];
    const row = document.querySelector(`#messages [data-mid="${m.id}"]`);
    if (row) {
      const fresh = buildMessageEl(m, {});
      fresh.dataset.ts = row.dataset.ts;
      fresh.dataset.seq = row.dataset.seq;
      if (row.classList.contains("grouped")) fresh.classList.add("grouped");
      row.replaceWith(fresh);
    }
  });
  await listen("message_status", (e) => {
    // 回执可能早于本地回显气泡渲染到达：找不到元素时稍后重试一次，避免状态标签停在「已发送」
    const apply = () => {
      const row = document.querySelector(`[data-mid="${e.payload.id}"] .msg-status`);
      if (row) applyStatus(row, e.payload.id, e.payload.status);
      // 图片组：组消息本身的状态是最终口径——成员 done 事件可能早于气泡渲染被丢，
      // 这里对账收起进度 UI，避免进度条永远停在「正在校验…」
      if (e.payload.status === "delivered" || e.payload.status === "failed") {
        const grow = document.querySelector(`[data-mid="${e.payload.id}"]`);
        if (grow && grow.dataset.kind === "imagegroup") {
          grow.querySelectorAll(".progress, .progress-meta").forEach((el) => el.classList.add("hidden"));
        }
      }
      return !!row;
    };
    if (!apply()) setTimeout(apply, 400);
  });
  await listen("peer_typing", (e) => {
    $("typing-hint").classList.toggle("hidden", !e.payload);
    if (e.payload) ringPulse("peer");
  });
  await listen("clipboard_state", (e) => {
    const v = e.payload || {};
    $("clip-sync-toggle").checked = !!v.local;
    $("clip-sync-status").textContent = clipStatusText(v);
  });
  await listen("clipboard_synced", (e) => {
    const kind = e.payload && e.payload.kind === "image" ? "图片" : "文字";
    $("clip-sync-status").textContent = `已同步（刚刚收到${kind}）`;
  });
  await listen("transfer_progress", (e) => {
    const { transfer_id, msg_id, received, total } = e.payload;
    if (msg_id && transfer_id) state.transferMap[msg_id] = transfer_id;
    // 图片组成员的进度按组聚合显示在组气泡上（成员 msg_id → 组 id）
    const gid = msg_id ? (state.groupMemberOf || {})[msg_id] : null;
    if (gid) { updateGroupProgress(gid, msg_id, received, total); return; }
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
    // 图片组成员的状态同样按组聚合（完成时顺带刷新组气泡缩略图）
    const gid = mid ? (state.groupMemberOf || {})[mid] : null;
    if (gid) { updateGroupTransferState(gid, mid, tstate); return; }
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
      }, 400);
    } else if (tstate === "verifying") {
      // 片已发完/收完、等校验结果：明说在等什么，不在 100% 干挂着
      if (bar) bar.classList.remove("hidden");
      if (meta) {
        meta.classList.remove("hidden");
        const mine = b.closest(".msg-row")?.dataset.sender === "me";
        meta.textContent = mine ? "已发送，等待对方校验…" : "已收完，正在校验…";
      }
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
  syncWindowTheme(mode);
}
// 原生窗口标题栏跟随主题（深色时不再是一条白杠）：走后端命令
// set_window_theme（Window::set_theme）。失败静默，不阻塞主题切换；
// 非 Tauri 环境（纯浏览器打开页面）直接跳过。
function syncWindowTheme(mode) {
  try {
    const p = invoke("set_window_theme", { theme: mode });
    if (p && p.catch) p.catch(() => {});
  } catch (_) { /* 忽略 */ }
}
// 「跟随系统」时系统深浅翻转 → 再同步一次标题栏
try {
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  const onSysThemeChange = () => {
    if ((localStorage.getItem("bl-theme") || "system") === "system") syncWindowTheme("system");
  };
  if (mq.addEventListener) mq.addEventListener("change", onSysThemeChange);
  else if (mq.addListener) mq.addListener(onSysThemeChange);
} catch (_) { /* 忽略 */ }
document.querySelectorAll("#theme-seg button").forEach((b) =>
  b.addEventListener("click", () => {
    localStorage.setItem("bl-theme", b.dataset.themeSet);
    applyTheme(b.dataset.themeSet);
  }));

// ---------- 软件更新（v2.5.2）：仅手动触发，平时绝不自动联网检查 ----------
// 流程：点「检查更新」→ 查 GitHub 已发布版 latest.json（草稿不会被推送）→
// 已是最新给明确提示；发现新版弹窗（版本号+说明节选）→「立即更新」下载
// （进度条；下载中可取消——插件下载无法中途掐断，取消=放弃，下载完不安装）→
// 下载完成再确认 → 安装并自动重启。签名校验由 updater 插件按公钥执行。
let updHandle = null;   // check() 返回的 Update 对象
let updPhase = "idle";  // idle | found | downloading | downloaded | installing
let updAbandoned = false;

function updSetStatus(text) { $("update-status").textContent = text; }
function friendlyUpdError(e) {
  const s = String((e && e.message) || e || "");
  if (/404|not found/i.test(s)) return "暂无可用的更新信息";
  if (/network|error sending request|dns|connect|timed out/i.test(s)) return "网络不可用";
  return s.slice(0, 80) || "未知错误";
}

$("btn-check-update").addEventListener("click", async () => {
  const btn = $("btn-check-update");
  if (btn.disabled) return;
  btn.disabled = true;
  updSetStatus("正在检查更新…");
  try {
    const updater = window.__TAURI__.updater;
    if (!updater || !updater.check) throw new Error("更新组件不可用");
    const update = await updater.check();
    if (!update) {
      updSetStatus("已是最新版本");
      showToast("已是最新版本");
      return;
    }
    updHandle = update;
    updPhase = "found";
    updAbandoned = false;
    $("update-title").textContent = `发现新版本 v${update.version}`;
    const notes = (update.body || "").trim();
    $("update-notes").textContent = notes ? notes.slice(0, 600) : "（此版本未附更新说明）";
    $("update-progress-wrap").classList.add("hidden");
    $("update-action").textContent = "立即更新";
    $("update-action").disabled = false;
    $("update-cancel").textContent = "以后再说";
    $("update-cancel").disabled = false;
    $("update-modal").classList.remove("hidden");
    updSetStatus(`发现新版本 v${update.version}`);
  } catch (e) {
    updSetStatus("检查失败，请稍后再试");
    showToast("检查更新失败：" + friendlyUpdError(e));
  } finally {
    btn.disabled = false;
  }
});

$("update-cancel").addEventListener("click", () => {
  if (updPhase === "downloading") { updAbandoned = true; updSetStatus("已取消更新"); }
  if (updPhase === "found") updSetStatus("仅手动检查，不会自动联网");
  $("update-modal").classList.add("hidden");
});

$("update-action").addEventListener("click", async () => {
  if (!updHandle) return;
  if (updPhase === "found") {
    updPhase = "downloading";
    updAbandoned = false;
    $("update-action").disabled = true;
    $("update-cancel").textContent = "取消";
    $("update-progress-wrap").classList.remove("hidden");
    const bar = $("update-progress-bar");
    const text = $("update-progress-text");
    bar.style.width = "0%";
    text.textContent = "正在下载…";
    let total = 0;
    let got = 0;
    try {
      await updHandle.download((ev) => {
        if (ev.event === "Started") { total = ev.data.contentLength || 0; }
        else if (ev.event === "Progress") {
          got += ev.data.chunkLength || 0;
          if (total > 0) {
            const pct = Math.min(100, Math.round((got / total) * 100));
            bar.style.width = pct + "%";
            text.textContent = `正在下载… ${pct}%`;
          } else {
            text.textContent = `正在下载… ${Math.round(got / 1024)} KB`;
          }
        }
      });
    } catch (e) {
      updPhase = "found";
      $("update-action").disabled = false;
      $("update-progress-wrap").classList.add("hidden");
      showToast("下载失败：" + friendlyUpdError(e));
      return;
    }
    if (updAbandoned) { updPhase = "idle"; updHandle = null; return; }
    updPhase = "downloaded";
    $("update-title").textContent = "下载完成";
    $("update-notes").textContent = "新版本已下载完成。点「安装并重启」将自动完成安装并重新打开 BlueLink。";
    $("update-progress-wrap").classList.add("hidden");
    $("update-action").textContent = "安装并重启";
    $("update-action").disabled = false;
    $("update-cancel").textContent = "以后再说";
    updSetStatus("新版本已下载，待安装");
  } else if (updPhase === "downloaded") {
    updPhase = "installing";
    $("update-action").disabled = true;
    $("update-cancel").disabled = true;
    $("update-action").textContent = "正在安装…";
    try {
      await updHandle.install();
      await invoke("restart_app");
    } catch (e) {
      updPhase = "downloaded";
      $("update-action").disabled = false;
      $("update-cancel").disabled = false;
      $("update-action").textContent = "安装并重启";
      showToast("安装失败：" + friendlyUpdError(e));
    }
  }
});

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
  // 拖拽文件到窗口：入待发队列（与附件按钮/粘贴同一条队列，点发送键
  // 统一逐个发；旧版是拖放即直发，2026-11-20 批量队列定版后统一口径）。
  // 拖放提示层只做视觉提示，不拦截事件。
  try {
    await window.__TAURI__.webview.getCurrentWebview().onDragDropEvent(async (e) => {
      const p = e.payload || {};
      if (p.type === "enter" || p.type === "over") {
        $("drop-overlay").classList.remove("hidden");
      } else if (p.type === "drop") {
        $("drop-overlay").classList.add("hidden");
        const added = await stageFilePaths(p.paths || []);
        if (added) showToast(`已添加 ${added} 个文件，点发送键发送`);
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


/* ================= 内置查看器 + 图片组（v2.6.0） =================
 * 查看器形态：页内全屏覆盖层（不用独立窗口）——没有第二个 WebView
 * 的冷启动与二次脚本加载，点击即出画面（秒开红线）；状态全在内存
 * 里，关闭即释放。文本解码走 WebView 自带 TextDecoder（UTF-8 严格
 * 校验失败回退 GBK），图片/媒体走 asset 协议直读本地文件，后端零
 * 新增依赖。 */

// ---------- 文件类型口径（查看器分流/图片组共用） ----------
const IMAGE_EXTS = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "avif", "ico"];
const TEXT_EXTS = new Set(["txt", "md", "log", "json", "xml", "csv", "tsv", "ini", "cfg", "conf", "yaml", "yml", "toml", "properties", "env", "java", "js", "mjs", "ts", "jsx", "tsx", "rs", "py", "go", "c", "h", "cpp", "hpp", "cc", "cs", "php", "rb", "sh", "bash", "zsh", "sql", "html", "htm", "css", "scss", "vue", "kt", "swift", "pl", "r", "lua", "diff", "patch", "dockerfile", "gitignore", "gradle", "cmake"]);
const VIDEO_EXTS = ["mp4", "webm", "mkv", "mov", "m4v", "avi"];
const AUDIO_EXTS = ["mp3", "wav", "ogg", "flac", "m4a", "aac", "opus"];
const OFFICE_EXTS = ["doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf"];
function fileExt(name) {
  const s = String(name || "");
  const i = s.lastIndexOf(".");
  return i >= 0 ? s.slice(i + 1).toLowerCase() : "";
}
function isImageExt(e) { return IMAGE_EXTS.includes(e); }

// ---------- asset 协议直读（图片/媒体） ----------
const assetUrlCache = new Map();
async function assetUrl(path) {
  if (!assetUrlCache.has(path)) {
    assetUrlCache.set(path, (async () => {
      await invoke("allow_asset_file", { path });
      return window.__TAURI__.core.convertFileSrc(path);
    })());
  }
  return assetUrlCache.get(path);
}

// ---------- 图片组 ----------
state.groupMemberOf = {};  // 成员 msg_id → 组消息 id
state.groupFiles = {};     // 组消息 id → group_files 结果缓存
state.groupProgress = {};  // 组消息 id → { members: Map, states: Map }

function parseManifest(body) {
  try {
    const j = JSON.parse(body);
    return Array.isArray(j.items) ? j.items : [];
  } catch (_) { return []; }
}
function registerGroupMembers(m) {
  if (!m || m.kind !== "imagegroup") return;
  for (const it of parseManifest(m.body)) state.groupMemberOf[it.msg_id] = m.id;
}
async function groupFilesOf(gid) {
  if (!state.groupFiles[gid]) {
    try { state.groupFiles[gid] = await invoke("group_files", { messageId: gid }); }
    catch (_) { state.groupFiles[gid] = []; }
  }
  return state.groupFiles[gid];
}

/** 组气泡：堆叠卡片（仿 iOS 短信图片组）——主图 + 露边的底层卡 +
 * 「共 N 张」角标 + 气泡上左右翻卡；点主图进查看器大图翻看。 */
function buildGroupBubble(bubble, m, row) {
  const items = parseManifest(m.body);
  registerGroupMembers(m);
  bubble._gcount = items.length;
  bubble.innerHTML = `
    <div class="gstack">
      <div class="gstack-card c2"></div>
      <div class="gstack-card c1"></div>
      <div class="gstack-main">
        <img class="gstack-img hidden" alt="" draggable="false">
        <div class="gstack-ph">${iconUse("i-file", "ico ico-28")}<span class="gstack-ph-name"></span></div>
      </div>
      <span class="gstack-count">共 ${items.length} 张</span>
      <button class="gstack-nav prev" title="上一张">‹</button>
      <button class="gstack-nav next" title="下一张">›</button>
      <span class="gstack-idx">1 / ${items.length}</span>
    </div>
    <div class="progress hidden"><div></div></div>
    <div class="progress-meta hidden"></div>
    <button class="progress-cancel hidden">取消</button>
    <button class="transfer-retry hidden">重试</button>`;
  state.lastFileBubble = bubble;
  bubble._gidx = 0;
  const showCard = async () => {
    const files = await groupFilesOf(m.id);
    if (!items.length) return;
    const i = Math.max(0, Math.min(bubble._gidx, items.length - 1));
    bubble._gidx = i;
    bubble.querySelector(".gstack-idx").textContent = `${i + 1} / ${items.length}`;
    const img = bubble.querySelector(".gstack-img");
    const ph = bubble.querySelector(".gstack-ph");
    const f = files[i];
    if (f && f.exists && f.path) {
      try {
        img.src = await assetUrl(f.path);
        img.classList.remove("hidden");
        ph.classList.add("hidden");
        return;
      } catch (_) { /* 落到占位 */ }
    }
    img.classList.add("hidden");
    ph.classList.remove("hidden");
    ph.querySelector(".gstack-ph-name").textContent = items[i] ? items[i].name : "";
  };
  bubble._showGroupCard = showCard;
  showCard();
  const step = (d) => {
    if (items.length < 2) return;
    bubble._gidx = (bubble._gidx + d + items.length) % items.length;
    showCard();
  };
  bubble.querySelector(".gstack-nav.prev").addEventListener("click", (e) => { e.stopPropagation(); step(-1); });
  bubble.querySelector(".gstack-nav.next").addEventListener("click", (e) => { e.stopPropagation(); step(1); });
  bubble.addEventListener("click", async (e) => {
    if (e.target.closest("button")) return;
    const files = await groupFilesOf(m.id);
    const view = items.map((it, i) => ({
      name: it.name,
      path: files[i] && files[i].exists ? files[i].path : null,
    }));
    openImageViewer(view, bubble._gidx);
  });
  bubble.querySelector(".progress-cancel").addEventListener("click", async () => {
    for (const it of items) {
      const tid = state.transferMap[it.msg_id];
      if (tid) { await invoke("cancel_transfer", { transferId: tid }); return; }
    }
  });
  bubble.querySelector(".transfer-retry").addEventListener("click", async () => {
    try {
      await invoke("resend_message", { messageId: m.id });
      row.remove();
      state.renderedChat.delete(m.id);
    } catch (err) { showToast("重发失败：" + err); }
  });
}

function groupProg(gid) {
  if (!state.groupProgress[gid]) state.groupProgress[gid] = { members: new Map(), states: new Map() };
  return state.groupProgress[gid];
}
function groupBubbleOf(gid) {
  const row = document.querySelector(`[data-mid="${gid}"]`);
  return row ? row.querySelector(".bubble") : null;
}
/** 组进度聚合：成员各自的已传字节汇总成一条组进度。 */
function updateGroupProgress(gid, memberMsgId, received, total) {
  const gp = groupProg(gid);
  gp.members.set(memberMsgId, { received, total });
  const b = groupBubbleOf(gid);
  if (!b) return;
  let r = 0, t = 0;
  for (const v of gp.members.values()) { r += v.received; t += v.total; }
  const bar = b.querySelector(".progress");
  const fill = b.querySelector(".progress > div");
  const meta = b.querySelector(".progress-meta");
  if (bar && fill) {
    bar.classList.remove("hidden");
    const pct = t ? Math.floor((r / t) * 100) : 0;
    fill.style.width = pct + "%";
    if (meta) {
      meta.classList.remove("hidden");
      meta.textContent = `${pct}% · ${fmtSize(r)} / ${fmtSize(t)}`;
    }
  }
  const cancel = b.querySelector(".progress-cancel");
  if (cancel && r < t) cancel.classList.remove("hidden");
}
function updateGroupTransferState(gid, memberMsgId, tstate) {
  const gp = groupProg(gid);
  gp.states.set(memberMsgId, tstate);
  const b = groupBubbleOf(gid);
  if (!b) return;
  const meta = b.querySelector(".progress-meta");
  const bar = b.querySelector(".progress");
  const cancel = b.querySelector(".progress-cancel");
  const retry = b.querySelector(".transfer-retry");
  if (cancel) cancel.classList.add("hidden");
  if (tstate === "done") {
    if (b._showGroupCard) b._showGroupCard();
    const total = b._gcount || gp.states.size;
    let done = 0;
    for (const s of gp.states.values()) if (s === "done") done++;
    if (done >= total) {
      if (meta) { meta.classList.remove("hidden"); meta.textContent = "已完成"; }
      setTimeout(() => {
        if (bar) bar.classList.add("hidden");
        if (meta) meta.classList.add("hidden");
      }, 400);
    } else if (meta) {
      meta.classList.remove("hidden");
      meta.textContent = `已完成 ${done} / ${total} 张`;
    }
  } else if (tstate === "verifying") {
    if (meta) { meta.classList.remove("hidden"); meta.textContent = "正在校验…"; }
  } else if (tstate === "verify_failed") {
    if (meta) { meta.classList.remove("hidden"); meta.textContent = "有图片校验失败，文件不完整"; }
    if (retry) retry.classList.remove("hidden");
  } else if (tstate === "cancelled") {
    if (meta) { meta.classList.remove("hidden"); meta.textContent = "已取消"; }
    if (retry) retry.classList.remove("hidden");
  }
}

/** 图片组右键菜单：查看、保存全部（按序 1、2、3…落到所选文件夹）、
 * 收藏/置顶/删除记录。容器与文件菜单共用 #file-menu。 */
function openGroupMenu(m, x, y) {
  const menu = $("file-menu");
  const items = parseManifest(m.body);
  menu.innerHTML = "";
  const entries = [
    ["查看图片", async () => {
      const files = await groupFilesOf(m.id);
      openImageViewer(items.map((it, i) => ({
        name: it.name,
        path: files[i] && files[i].exists ? files[i].path : null,
      })), 0);
    }],
    ["保存全部图片…", async () => {
      const dir = await window.__TAURI__.dialog.open({ directory: true, multiple: false });
      if (!dir) return;
      const [ok, total] = await invoke("save_group_images", { messageId: m.id, dir });
      showToast(`已保存 ${ok} / ${total} 张图片到所选文件夹`);
    }],
    ["收藏", () => toggleMark(m.id, "favorite")],
    [state.pinIds.has(m.id) ? "取消置顶" : "置顶", () => toggleMark(m.id, "pin")],
    ["删除记录", async () => {
      await invoke("delete_message", { messageId: m.id });
      const row = document.querySelector(`[data-mid="${m.id}"]`);
      if (row) {
        row.remove();
        state.renderedChat.delete(m.id);
        state.renderedAi.delete(m.id);
      }
      showToast("记录已删除（文件本体保留）");
    }],
  ];
  for (const [label, fn] of entries) {
    const b = document.createElement("button");
    b.textContent = label;
    if (label === "删除记录") b.className = "fm-danger";
    b.addEventListener("click", async () => {
      menu.classList.add("hidden");
      try { await fn(); } catch (e) { showToast("操作失败：" + e); }
    });
    menu.appendChild(b);
  }
  menu.style.left = Math.min(x, window.innerWidth - 190) + "px";
  menu.style.top = Math.min(y, window.innerHeight - 240) + "px";
  menu.classList.remove("hidden");
}

/** 文件气泡点击分流（v2.6.0）：文本/代码→内置文本查看器，图片→
 * 图片查看器，音视频→内置播放，PDF/Office→系统程序打开（本轮不做
 * 内置），其余与文件不存在时维持原文件菜单。 */
async function openFileSmart(m, x, y) {
  let info = null;
  try { info = await invoke("file_info", { messageId: m.id }); } catch (_) { /* 忽略 */ }
  if (!info || !info.exists || !info.path) { openFileMenu(m.id, x, y); return; }
  const ext = fileExt(info.name || m.body);
  if (TEXT_EXTS.has(ext)) { openTextViewer(info); return; }
  if (isImageExt(ext)) { openImageViewer([{ name: info.name, path: info.path }], 0); return; }
  if (VIDEO_EXTS.includes(ext) || AUDIO_EXTS.includes(ext)) { openMediaViewer(info, ext); return; }
  if (ext === "pdf" || OFFICE_EXTS.includes(ext)) {
    try { await invoke("open_file", { path: info.path }); }
    catch (e) { showToast("打开失败：" + e); }
    return;
  }
  openFileMenu(m.id, x, y);
}

// ---------- 查看器外壳 ----------
let viewerMode = null; // "text" | "image" | "media"
function showViewer(mode, title, sub) {
  viewerMode = mode;
  $("viewer").classList.remove("hidden");
  $("viewer-title").textContent = title || "";
  $("viewer-sub").textContent = sub || "";
  $("viewer-text").classList.toggle("hidden", mode !== "text");
  $("viewer-image").classList.toggle("hidden", mode !== "image");
  $("viewer-media").classList.toggle("hidden", mode !== "media");
  $("viewer-tools").classList.toggle("hidden", mode !== "text");
}
function closeViewer() {
  $("viewer").classList.add("hidden");
  // 关闭即释放：停播并摘掉媒体/图片源，文本模型整体丢弃
  for (const id of ["viewer-video", "viewer-audio"]) {
    const el = $(id);
    try { el.pause(); el.removeAttribute("src"); el.load(); } catch (_) { /* 忽略 */ }
  }
  $("viewer-img").removeAttribute("src");
  tvReset();
  IV.items = [];
  viewerMode = null;
}
$("viewer-close").addEventListener("click", closeViewer);
document.addEventListener("keydown", (e) => {
  if ($("viewer").classList.contains("hidden")) return;
  if (e.key === "Escape") {
    if (viewerMode === "text" && !$("viewer-findbar").classList.contains("hidden")) {
      closeFindBar();
      return;
    }
    closeViewer();
    return;
  }
  if (viewerMode === "image") {
    if (e.key === "ArrowLeft") ivStep(-1);
    else if (e.key === "ArrowRight") ivStep(1);
  } else if (viewerMode === "text") {
    tvKeys(e);
  }
});

// ---------- 图片查看器 ----------
const IV = { items: [], idx: 0, scale: 1, rot: 0, tx: 0, ty: 0, fitScale: 1, natural: { w: 0, h: 0 } };
async function openImageViewer(items, idx) {
  IV.items = items || [];
  IV.idx = Math.max(0, Math.min(idx || 0, IV.items.length - 1));
  showViewer(
    "image",
    IV.items.length > 1 ? `图片组 · 共 ${IV.items.length} 张` : (IV.items[0] ? IV.items[0].name : "图片"),
    ""
  );
  await ivShow();
}
async function ivShow() {
  const it = IV.items[IV.idx];
  const img = $("viewer-img");
  $("vi-count").textContent = IV.items.length > 1 ? `${IV.idx + 1} / ${IV.items.length}` : "";
  $("viewer-img-prev").classList.toggle("hidden", IV.items.length <= 1);
  $("viewer-img-next").classList.toggle("hidden", IV.items.length <= 1);
  $("viewer-sub").textContent = it ? it.name : "";
  $("viewer-title").textContent = IV.items.length > 1 ? `图片组 · 共 ${IV.items.length} 张` : (it ? it.name : "图片");
  IV.rot = 0; IV.tx = 0; IV.ty = 0; IV.scale = 1;
  img.removeAttribute("src");
  if (!it || !it.path) { $("vi-zoom").textContent = "文件未就绪（尚未接收完成或已被移动）"; return; }
  $("vi-zoom").textContent = "";
  try { img.src = await assetUrl(it.path); }
  catch (_) { $("vi-zoom").textContent = "图片加载失败"; }
}
function ivFit() {
  const stage = $("viewer-img-stage");
  const { w, h } = IV.natural;
  if (!w || !h || !stage.clientWidth) return;
  const swap = IV.rot % 180 !== 0;
  const ew = swap ? h : w, eh = swap ? w : h;
  const sw = Math.max(50, stage.clientWidth - 24), sh = Math.max(50, stage.clientHeight - 24);
  IV.fitScale = ew <= sw && eh <= sh ? 1 : Math.min(sw / ew, sh / eh);
  IV.scale = IV.fitScale;
  IV.tx = 0; IV.ty = 0;
  ivApply();
}
function ivApply() {
  $("viewer-img").style.transform =
    `translate(${IV.tx}px, ${IV.ty}px) rotate(${IV.rot}deg) scale(${IV.scale})`;
  $("vi-zoom").textContent = `${Math.round(IV.scale * 100)}%`;
}
function ivStep(d) {
  if (IV.items.length < 2) return;
  IV.idx = (IV.idx + d + IV.items.length) % IV.items.length;
  ivShow();
}
$("viewer-img").addEventListener("load", () => {
  IV.natural = { w: $("viewer-img").naturalWidth, h: $("viewer-img").naturalHeight };
  ivFit();
});
$("vi-fit").addEventListener("click", () => ivFit());
$("vi-actual").addEventListener("click", () => { IV.scale = 1; IV.tx = 0; IV.ty = 0; ivApply(); });
$("vi-rotl").addEventListener("click", () => { IV.rot = (IV.rot + 270) % 360; ivFit(); });
$("vi-rotr").addEventListener("click", () => { IV.rot = (IV.rot + 90) % 360; ivFit(); });
$("viewer-img-prev").addEventListener("click", () => ivStep(-1));
$("viewer-img-next").addEventListener("click", () => ivStep(1));
{
  const stage = $("viewer-img-stage");
  stage.addEventListener("wheel", (e) => {
    if (viewerMode !== "image") return;
    e.preventDefault();
    IV.scale = Math.max(0.05, Math.min(20, IV.scale * (e.deltaY < 0 ? 1.12 : 0.89)));
    ivApply();
  }, { passive: false });
  stage.addEventListener("dblclick", () => {
    if (Math.abs(IV.scale - IV.fitScale) < 0.01) {
      IV.scale = 1; IV.tx = 0; IV.ty = 0; ivApply();
    } else ivFit();
  });
  let drag = null;
  stage.addEventListener("pointerdown", (e) => {
    if (e.target.closest("button")) return;
    drag = { x: e.clientX, y: e.clientY, tx: IV.tx, ty: IV.ty };
    stage.setPointerCapture(e.pointerId);
  });
  stage.addEventListener("pointermove", (e) => {
    if (!drag) return;
    IV.tx = drag.tx + (e.clientX - drag.x);
    IV.ty = drag.ty + (e.clientY - drag.y);
    ivApply();
  });
  stage.addEventListener("pointerup", () => { drag = null; });
  stage.addEventListener("pointercancel", () => { drag = null; });
}

// ---------- 音视频（WebView 自带播放，播不了给系统打开兜底） ----------
let mediaPath = null;
async function openMediaViewer(info, ext) {
  mediaPath = info.path;
  showViewer("media", info.name, fmtSize(info.size));
  const isVideo = VIDEO_EXTS.includes(ext);
  const vel = $("viewer-video"), ael = $("viewer-audio");
  vel.classList.toggle("hidden", !isVideo);
  ael.classList.toggle("hidden", isVideo);
  $("viewer-media-err").classList.add("hidden");
  try {
    const el = isVideo ? vel : ael;
    el.src = await assetUrl(info.path);
    el.play().catch(() => { /* 自动播放被拦时用户可手动点播放 */ });
  } catch (_) {
    $("viewer-media-err").classList.remove("hidden");
  }
}
for (const id of ["viewer-video", "viewer-audio"]) {
  $(id).addEventListener("error", () => {
    if (viewerMode === "media") $("viewer-media-err").classList.remove("hidden");
  });
}
$("viewer-media-open").addEventListener("click", async () => {
  if (mediaPath) {
    try { await invoke("open_file", { path: mediaPath }); }
    catch (e) { showToast("打开失败：" + e); }
  }
});

// ---------- 文本/代码查看器 ----------
// 红线落实：①秒开——先把原文第一屏以纯文本渲染出来，高亮随后补；
// ②大文件不卡——虚拟滚动只渲染可视行（±10 行缓冲）；③选择/复制
// 走数据层——内存持全文与行偏移表，选区是 {行,列} 模型，与渲染了
// 多少行无关，Ctrl+A 全选、Ctrl+C 复制选区都按模型算；④高亮——
// 小文件（≤4000 行）打开即全量算完，大文件只算可视区及滚动预取，
// 结果只缓存视区附近（上限约 6000 行），不全量缓存。
const TV = {
  name: "", origText: "", text: "", lines: [], starts: [],
  encoding: "", lang: "text", ext: "",
  sel: null, hl: new Map(), stateAt: null, chW: 8,
  findQ: "", findHits: [], findPos: -1, transformed: null,
};
const TV_LINE_H = 20;
const TV_FULL_HL = 4000;
const TV_FONT = '13px ui-monospace, "Cascadia Mono", Consolas, Menlo, monospace';

function tvReset() {
  TV.text = ""; TV.origText = ""; TV.lines = []; TV.starts = [];
  TV.sel = null; TV.hl.clear(); TV.stateAt = null;
  TV.findQ = ""; TV.findHits = []; TV.findPos = -1; TV.transformed = null;
  $("viewer-lines").innerHTML = "";
  $("viewer-gutter").innerHTML = "";
  $("viewer-banner").classList.add("hidden");
  $("viewer-findbar").classList.add("hidden");
}

function base64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}
/** 编码识别：BOM 优先；UTF-8 严格解码成功即 UTF-8，否则 GBK。 */
function decodeTextBytes(bytes) {
  if (bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
    return { text: new TextDecoder("utf-8").decode(bytes.subarray(3)), enc: "UTF-8" };
  }
  if (bytes.length >= 2 && bytes[0] === 0xff && bytes[1] === 0xfe) {
    return { text: new TextDecoder("utf-16le").decode(bytes), enc: "UTF-16" };
  }
  if (bytes.length >= 2 && bytes[0] === 0xfe && bytes[1] === 0xff) {
    return { text: new TextDecoder("utf-16be").decode(bytes), enc: "UTF-16" };
  }
  try {
    return { text: new TextDecoder("utf-8", { fatal: true }).decode(bytes), enc: "UTF-8" };
  } catch (_) {
    return { text: new TextDecoder("gbk").decode(bytes), enc: "GBK" };
  }
}

async function openTextViewer(info) {
  showViewer("text", info.name, "加载中…");
  tvReset();
  TV.name = info.name;
  let data;
  try {
    data = await invoke("read_text_file", { path: info.path });
  } catch (e) {
    const msg = String(e);
    closeViewer();
    if (msg.startsWith("TOO_LARGE")) {
      showToast("文件超过 10 MB，改用系统程序打开");
      try { await invoke("open_file", { path: info.path }); } catch (_) { /* 忽略 */ }
    } else {
      showToast("打开失败：" + msg);
    }
    return;
  }
  const dec = decodeTextBytes(base64ToBytes(data.base64));
  TV.origText = dec.text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
  TV.encoding = dec.enc;
  TV.ext = fileExt(info.name);
  TV.lang = langOfExt(TV.ext);
  $("viewer-sub").textContent = `${fmtSize(data.size)} · ${dec.enc}`;
  tvSetText(TV.origText);
}

function tvSetText(text) {
  TV.text = text;
  TV.lines = text.split("\n");
  TV.starts = new Array(TV.lines.length);
  let off = 0;
  for (let i = 0; i < TV.lines.length; i++) {
    TV.starts[i] = off;
    off += TV.lines[i].length + 1;
  }
  TV.sel = null;
  TV.hl.clear();
  TV.stateAt = TV.lines.length <= TV_FULL_HL ? new Array(TV.lines.length + 1) : null;
  TV.findHits = []; TV.findPos = -1;
  $("viewer-scroll").scrollTop = 0;
  tvMeasureCh();
  tvRender(); // 第一屏先出原文（无高亮），不等排版
  if (TV.lines.length <= TV_FULL_HL) {
    tvHighlightRange(0, TV.lines.length);
    tvRender();
  } else {
    tvEnsureHlVisible();
  }
  tvStatus();
}

let tvCanvas = null;
function tvMeasureCh() {
  if (!tvCanvas) tvCanvas = document.createElement("canvas");
  const ctx = tvCanvas.getContext("2d");
  ctx.font = TV_FONT;
  TV.chW = ctx.measureText("0").width || 7.8;
}

function tvRender() {
  const sc = $("viewer-scroll");
  if (!TV.lines.length || !sc.clientHeight) return;
  const total = TV.lines.length;
  $("viewer-spacer").style.height = total * TV_LINE_H + "px";
  $("viewer-gutter").style.height = total * TV_LINE_H + "px";
  const first = Math.max(0, Math.floor(sc.scrollTop / TV_LINE_H) - 10);
  const visCount = Math.ceil(sc.clientHeight / TV_LINE_H) + 20;
  const last = Math.min(total, first + visCount);
  const linesEl = $("viewer-lines"), gutEl = $("viewer-gutter");
  linesEl.style.paddingTop = first * TV_LINE_H + "px";
  gutEl.style.paddingTop = first * TV_LINE_H + "px";
  let lh = "", gh = "";
  for (let i = first; i < last; i++) {
    gh += `<div class="tv-num">${i + 1}</div>`;
    lh += `<div class="tv-line">${tvLineHtml(i)}</div>`;
  }
  linesEl.innerHTML = lh;
  gutEl.innerHTML = gh;
}

/** 一行的 HTML：语法色（缓存或纯文本）+ 选区/查找命中的矩形衬底
 * （等宽字体按列定位，只加衬底不改布局，高亮不跳动）。 */
function tvLineHtml(i) {
  const rects = [];
  const sel = normSel();
  if (sel && i >= sel.a.l && i <= sel.f.l) {
    const from = i === sel.a.l ? sel.a.c : 0;
    const to = i === sel.f.l ? sel.f.c : TV.lines[i].length;
    if (to > from) {
      rects.push(`<span class="tv-rect sel" style="left:${from * TV.chW}px;width:${(to - from) * TV.chW}px"></span>`);
    }
  }
  if (TV.findQ) {
    const line = TV.lines[i];
    let at = line.indexOf(TV.findQ);
    while (at >= 0) {
      rects.push(`<span class="tv-rect hit" style="left:${at * TV.chW}px;width:${TV.findQ.length * TV.chW}px"></span>`);
      at = line.indexOf(TV.findQ, at + 1);
    }
  }
  const body = TV.hl.get(i) ?? esc(TV.lines[i]);
  return rects.join("") + `<span class="tv-code">${body}</span>`;
}

let tvScrollRaf = 0;
$("viewer-scroll").addEventListener("scroll", () => {
  if (tvScrollRaf) return;
  tvScrollRaf = requestAnimationFrame(() => {
    tvScrollRaf = 0;
    tvRender();
    tvEnsureHlVisible();
  });
});
/** 大文件：只给可视区（上下各扩一屏）补高亮，其余滚动到了再算。 */
function tvEnsureHlVisible() {
  if (TV.lines.length <= TV_FULL_HL) return;
  const sc = $("viewer-scroll");
  const first = Math.max(0, Math.floor(sc.scrollTop / TV_LINE_H) - 40);
  const last = Math.min(
    TV.lines.length,
    first + Math.ceil(sc.clientHeight / TV_LINE_H) + 80
  );
  let need = false;
  for (let i = first; i < last; i++) {
    if (!TV.hl.has(i)) { need = true; break; }
  }
  if (need) {
    tvHighlightRange(first, last);
    tvRender();
  }
}

/** 把 [from, to) 行逐行着色进缓存。小文件全量时经 stateAt 精确接续
 * 跨行状态（块注释/模板串）；大文件分片时每片从默认状态起算，片头
 * 个别行可能短暂色差——轻量取舍，不影响内容与布局。 */
function tvHighlightRange(from, to) {
  let st = { block: false, tpl: false };
  if (from > 0 && TV.stateAt && TV.stateAt[from]) st = TV.stateAt[from];
  for (let i = from; i < to; i++) {
    if (TV.stateAt) TV.stateAt[i] = st;
    const r = tvTokenizeLine(TV.lines[i], TV.lang, st);
    TV.hl.set(i, r.html);
    st = r.st;
  }
  if (TV.stateAt) TV.stateAt[to] = st;
  if (TV.hl.size > 6000) {
    const mid = Math.floor((from + to) / 2);
    for (const k of [...TV.hl.keys()]) {
      if (Math.abs(k - mid) > 3000) TV.hl.delete(k);
    }
  }
}

// ---------- 轻量语法高亮（手写逐行状态机，零依赖、几 KB） ----------
const TV_KW = {
  js: "break case catch class const continue debugger default delete do else export extends finally for function if import in instanceof let new return super switch this throw try typeof var void while with yield async await static get set of null undefined true false",
  rust: "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while",
  py: "and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield True False None print",
  c: "auto break case char const continue default do double else enum extern float for goto if inline int long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while bool true false null nullptr namespace public private protected class template typename virtual friend this new delete try catch throw final extends implements interface package void",
  go: "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var true false nil",
};
function langOfExt(ext) {
  if (["js", "mjs", "jsx", "ts", "tsx"].includes(ext)) return "js";
  if (ext === "rs") return "rust";
  if (ext === "py") return "py";
  if (["java", "c", "h", "cpp", "hpp", "cc", "cs", "kt", "swift", "scala", "php"].includes(ext)) return "c";
  if (ext === "go") return "go";
  if (ext === "json") return "json";
  if (["html", "htm", "xml", "svg", "vue"].includes(ext)) return "markup";
  if (ext === "css" || ext === "scss") return "css";
  if (["sh", "bash", "zsh"].includes(ext)) return "shell";
  return "text";
}

function tvTokenizeLine(line, lang, st) {
  if (lang === "text") return { html: esc(line), st };
  if (lang === "json") return { html: tvTokenizeJson(line), st };
  if (lang === "markup") return { html: tvTokenizeMarkup(line), st };
  const lineComment = lang === "py" || lang === "shell" ? "#" : "//";
  const kw = TV_KW[lang] ? new Set(TV_KW[lang].split(" ")) : null;
  let out = "", i = 0;
  const n = line.length;
  let plain = "";
  const flush = () => { if (plain) { out += esc(plain); plain = ""; } };
  const push = (txt, cls) => { flush(); out += `<span class="${cls}">${esc(txt)}</span>`; };
  if (st.block) {
    const end = line.indexOf("*/");
    if (end < 0) { out += `<span class="tk-com">${esc(line)}</span>`; return { html: out, st }; }
    out += `<span class="tk-com">${esc(line.slice(0, end + 2))}</span>`;
    i = end + 2;
    st = { block: false, tpl: st.tpl };
  }
  if (st.tpl) {
    let j = i;
    while (j < n) {
      if (line[j] === "\\") { j += 2; continue; }
      if (line[j] === "`") break;
      j++;
    }
    if (j >= n) { out += `<span class="tk-str">${esc(line.slice(i))}</span>`; return { html: out, st }; }
    out += `<span class="tk-str">${esc(line.slice(i, j + 1))}</span>`;
    i = j + 1;
    st = { block: st.block, tpl: false };
  }
  while (i < n) {
    const ch = line[i];
    if (lang !== "css" && line.startsWith(lineComment, i)) {
      push(line.slice(i), "tk-com");
      i = n;
      break;
    }
    if (line.startsWith("/*", i)) {
      const end = line.indexOf("*/", i + 2);
      if (end < 0) {
        push(line.slice(i), "tk-com");
        st = { block: true, tpl: st.tpl };
        i = n;
        break;
      }
      push(line.slice(i, end + 2), "tk-com");
      i = end + 2;
      continue;
    }
    if (ch === '"' || ch === "'" || (ch === "`" && lang === "js")) {
      const q = ch;
      let j = i + 1;
      while (j < n) {
        if (line[j] === "\\") { j += 2; continue; }
        if (line[j] === q) break;
        j++;
      }
      if (j >= n) {
        push(line.slice(i), "tk-str");
        if (q === "`") st = { block: st.block, tpl: true };
        i = n;
        break;
      }
      push(line.slice(i, j + 1), "tk-str");
      i = j + 1;
      continue;
    }
    if (ch >= "0" && ch <= "9" && !/[\w$]/.test(plain.slice(-1))) {
      let j = i;
      while (j < n && /[\w.]/.test(line[j])) j++;
      push(line.slice(i, j), "tk-num");
      i = j;
      continue;
    }
    if (/[A-Za-z_$]/.test(ch) || ch.charCodeAt(0) > 127) {
      let j = i;
      while (j < n && /[\w$]/.test(line[j])) j++;
      const word = line.slice(i, j);
      if (kw && kw.has(word)) push(word, "tk-kw");
      else plain += word;
      i = j;
      continue;
    }
    plain += ch;
    i++;
  }
  flush();
  return { html: out, st };
}

function tvTokenizeJson(line) {
  let out = "", i = 0;
  const n = line.length;
  while (i < n) {
    const ch = line[i];
    if (ch === '"') {
      let j = i + 1;
      while (j < n) {
        if (line[j] === "\\") { j += 2; continue; }
        if (line[j] === '"') break;
        j++;
      }
      const s = line.slice(i, Math.min(j + 1, n));
      let k = j + 1;
      while (k < n && line[k] === " ") k++;
      out += `<span class="${line[k] === ":" ? "tk-key" : "tk-str"}">${esc(s)}</span>`;
      i = j + 1;
      continue;
    }
    if (/[-\d]/.test(ch)) {
      let j = i;
      while (j < n && /[\d.eE+-]/.test(line[j])) j++;
      out += `<span class="tk-num">${esc(line.slice(i, j))}</span>`;
      i = j;
      continue;
    }
    const word = /^(true|false|null)\b/.exec(line.slice(i));
    if (word) {
      out += `<span class="tk-kw">${word[1]}</span>`;
      i += word[1].length;
      continue;
    }
    out += esc(ch);
    i++;
  }
  return out;
}

function tvTokenizeMarkup(line) {
  let out = "", i = 0;
  const n = line.length;
  while (i < n) {
    if (line.startsWith("<!--", i)) {
      const end = line.indexOf("-->", i + 4);
      const stop = end < 0 ? n : end + 3;
      out += `<span class="tk-com">${esc(line.slice(i, stop))}</span>`;
      i = stop;
      continue;
    }
    if (line[i] === "<") {
      const end = line.indexOf(">", i);
      const stop = end < 0 ? n : end + 1;
      const tag = line.slice(i, stop);
      out += tag
        .split(/("[^"]*"|'[^']*')/g)
        .map((p, k) => (k % 2 === 1
          ? `<span class="tk-str">${esc(p)}</span>`
          : `<span class="tk-tag">${esc(p)}</span>`))
        .join("");
      i = stop;
      continue;
    }
    out += esc(line[i]);
    i++;
  }
  return out;
}

// ---------- 选择/复制（数据层选区模型） ----------
function normSel() {
  if (!TV.sel) return null;
  const { a, f } = TV.sel;
  if (a.l < f.l || (a.l === f.l && a.c <= f.c)) return { a, f };
  return { a: f, f: a };
}
function tvSelText() {
  const s = normSel();
  if (!s) return "";
  return TV.text.slice(TV.starts[s.a.l] + s.a.c, TV.starts[s.f.l] + s.f.c);
}
function tvPosFromEvent(e) {
  const spacer = $("viewer-spacer");
  const linesEl = $("viewer-lines");
  const y = e.clientY - spacer.getBoundingClientRect().top;
  const l = Math.max(0, Math.min(TV.lines.length - 1, Math.floor(y / TV_LINE_H)));
  const c = Math.max(0, Math.min(
    TV.lines[l].length,
    Math.round((e.clientX - linesEl.getBoundingClientRect().left) / TV.chW)
  ));
  return { l, c };
}
let tvSelecting = false;
$("viewer-lines").addEventListener("mousedown", (e) => {
  if (viewerMode !== "text" || !TV.lines.length) return;
  const pos = tvPosFromEvent(e);
  TV.sel = { a: pos, f: { ...pos } };
  tvSelecting = true;
  tvRender();
  tvStatus();
  e.preventDefault();
});
document.addEventListener("mousemove", (e) => {
  if (!tvSelecting) return;
  const sc = $("viewer-scroll");
  const r = sc.getBoundingClientRect();
  if (e.clientY < r.top + 28) sc.scrollTop -= 24;
  else if (e.clientY > r.bottom - 28) sc.scrollTop += 24;
  TV.sel.f = tvPosFromEvent(e);
  tvRender();
  tvStatus();
});
document.addEventListener("mouseup", () => { tvSelecting = false; });
$("viewer-lines").addEventListener("dblclick", (e) => {
  if (viewerMode !== "text" || !TV.lines.length) return;
  const pos = tvPosFromEvent(e);
  const line = TV.lines[pos.l];
  let a = pos.c, f = pos.c;
  while (a > 0 && /[\w$-]/.test(line[a - 1])) a--;
  while (f < line.length && /[\w$-]/.test(line[f])) f++;
  if (f > a) {
    TV.sel = { a: { l: pos.l, c: a }, f: { l: pos.l, c: f } };
    tvRender();
    tvStatus();
  }
});

function tvKeys(e) {
  if (e.target && (e.target.tagName === "INPUT" || e.target.tagName === "SELECT")) return;
  const mod = e.ctrlKey || e.metaKey;
  if (!mod) return;
  const k = e.key.toLowerCase();
  if (k === "a") {
    e.preventDefault();
    if (!TV.lines.length) return;
    TV.sel = {
      a: { l: 0, c: 0 },
      f: { l: TV.lines.length - 1, c: TV.lines[TV.lines.length - 1].length },
    };
    tvRender();
    tvStatus();
  } else if (k === "c") {
    const s = normSel();
    if (s && (s.a.l !== s.f.l || s.a.c !== s.f.c)) {
      e.preventDefault();
      copyText(tvSelText());
    }
  } else if (k === "f") {
    e.preventDefault();
    openFindBar();
  }
}

function tvStatus() {
  const parts = [`共 ${TV.lines.length} 行`, `编码 ${TV.encoding}`];
  const s = normSel();
  if (s && (s.a.l !== s.f.l || s.a.c !== s.f.c)) {
    parts.push(`已选 ${TV.starts[s.f.l] + s.f.c - (TV.starts[s.a.l] + s.a.c)} 字`);
  }
  $("viewer-status").textContent = parts.join(" · ");
}

// ---------- 文件内查找 ----------
function openFindBar() {
  $("viewer-findbar").classList.remove("hidden");
  const inp = $("viewer-find-input");
  inp.focus();
  inp.select();
}
function closeFindBar() {
  $("viewer-findbar").classList.add("hidden");
  TV.findQ = "";
  TV.findHits = [];
  TV.findPos = -1;
  $("viewer-find-count").textContent = "";
  tvRender();
}
function tvFindRun() {
  const q = $("viewer-find-input").value;
  TV.findQ = q;
  TV.findHits = [];
  TV.findPos = -1;
  if (q) {
    for (let i = 0; i < TV.lines.length && TV.findHits.length < 2000; i++) {
      if (TV.lines[i].includes(q)) TV.findHits.push(i);
    }
  }
  $("viewer-find-count").textContent = q
    ? (TV.findHits.length ? `共 ${TV.findHits.length} 处` : "无结果")
    : "";
  tvRender();
}
function tvFindJump(dir) {
  if (!TV.findHits.length) {
    tvFindRun();
    if (!TV.findHits.length) return;
  }
  TV.findPos = (TV.findPos + dir + TV.findHits.length) % TV.findHits.length;
  const l = TV.findHits[TV.findPos];
  const c = TV.lines[l].indexOf(TV.findQ);
  TV.sel = { a: { l, c }, f: { l, c: c + TV.findQ.length } };
  const sc = $("viewer-scroll");
  sc.scrollTop = Math.max(0, l * TV_LINE_H - sc.clientHeight / 2);
  $("viewer-find-count").textContent = `${TV.findPos + 1} / ${TV.findHits.length} 处`;
  tvRender();
  tvStatus();
}
$("viewer-find-btn").addEventListener("click", openFindBar);
$("viewer-find-close").addEventListener("click", closeFindBar);
$("viewer-find-input").addEventListener("input", tvFindRun);
$("viewer-find-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); tvFindJump(e.shiftKey ? -1 : 1); }
});
$("viewer-find-prev").addEventListener("click", () => tvFindJump(-1));
$("viewer-find-next").addEventListener("click", () => tvFindJump(1));

// ---------- 格式化快捷处理（显示层，原文不动） ----------
const TV_FMT_LABEL = {
  jsonPretty: "JSON 美化", jsonMin: "JSON 压缩", xmlPretty: "XML 美化",
  reindent: "代码整理缩进", b64enc: "Base64 编码", b64dec: "Base64 解码",
  urlEnc: "URL 编码", urlDec: "URL 解码", tsConv: "时间戳转换",
};
$("viewer-fmt-go").addEventListener("click", () => {
  const kind = $("viewer-fmt").value;
  if (!kind) { showToast("先选择一种处理"); return; }
  tvApplyTransform(kind);
});
$("viewer-copy-all").addEventListener("click", () => {
  if (TV.text) copyText(TV.text);
});
$("viewer-banner-origin").addEventListener("click", () => {
  TV.transformed = null;
  TV.lang = langOfExt(TV.ext);
  $("viewer-banner").classList.add("hidden");
  tvSetText(TV.origText);
});
$("viewer-banner-close").addEventListener("click", () => {
  $("viewer-banner").classList.add("hidden");
});
function fmtDateTime(d) {
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
function tvApplyTransform(kind) {
  const src = TV.origText;
  try {
    if (kind === "tsConv") {
      // 优先看选区里的数字，没有就找全文第一个 10/13 位时间戳
      const selTxt = tvSelText();
      const m = (selTxt && /\d{10,13}/.exec(selTxt)) || /\d{10,13}/.exec(src);
      if (!m) { showToast("没有找到时间戳（10 或 13 位数字）"); return; }
      let v = Number(m[0]);
      if (m[0].length === 10) v *= 1000;
      const d = new Date(v);
      if (isNaN(d.getTime())) { showToast("时间戳超出可表示范围"); return; }
      const out = `${m[0]} → ${fmtDateTime(d)}（本地时间）`;
      TV.transformed = kind;
      $("viewer-banner-text").textContent = `时间戳转换：${out}（原文未修改）`;
      $("viewer-banner").classList.remove("hidden");
      $("viewer-banner-copy").onclick = () => copyText(out);
      return;
    }
    let out;
    if (kind === "jsonPretty") out = JSON.stringify(JSON.parse(src), null, 2);
    else if (kind === "jsonMin") out = JSON.stringify(JSON.parse(src));
    else if (kind === "xmlPretty") out = xmlPretty(src);
    else if (kind === "reindent") out = reindentCode(src);
    else if (kind === "b64enc") out = b64EncodeText(src);
    else if (kind === "b64dec") out = b64DecodeText(src);
    else if (kind === "urlEnc") out = encodeURIComponent(src);
    else if (kind === "urlDec") out = decodeURIComponent(src);
    else return;
    TV.transformed = kind;
    tvSetText(out);
    if (kind === "jsonPretty" || kind === "jsonMin") {
      TV.lang = "json";
      TV.hl.clear();
      TV.stateAt = TV.lines.length <= TV_FULL_HL ? new Array(TV.lines.length + 1) : null;
      if (TV.lines.length <= TV_FULL_HL) tvHighlightRange(0, TV.lines.length);
      tvRender();
    }
    $("viewer-banner-text").textContent =
      `已应用「${TV_FMT_LABEL[kind]}」——显示层处理，原文未修改`;
    $("viewer-banner").classList.remove("hidden");
    $("viewer-banner-copy").onclick = () => copyText(TV.text);
  } catch (e) {
    showToast(`处理失败：${String((e && e.message) || e).slice(0, 80)}`);
  }
}
function xmlPretty(src) {
  const s = src.replace(/>\s+</g, "><").trim();
  const parts = s.split(/(<[^>]+>)/g).filter((p) => p.length);
  let depth = 0;
  const out = [];
  for (const p of parts) {
    if (/^<\//.test(p)) depth = Math.max(0, depth - 1);
    out.push("  ".repeat(depth) + p);
    if (/^<[^!?/][^>]*[^/]>$/.test(p) && !/^<(br|hr|img|input|meta|link)[\s>]/.test(p)) depth++;
  }
  return out.join("\n");
}
/** 通用大括号缩进整理（近似：不解析字符串里的括号，显示层处理，
 * 原文不动，看不顺眼点「查看原文」即可）。 */
function reindentCode(src) {
  let depth = 0;
  const out = [];
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line) { out.push(""); continue; }
    const lead = /^[})\]]/.test(line) ? 1 : 0;
    out.push("  ".repeat(Math.max(0, depth - lead)) + line);
    let delta = 0;
    for (const ch of line) {
      if (ch === "{" || ch === "[" || ch === "(") delta++;
      else if (ch === "}" || ch === "]" || ch === ")") delta--;
    }
    depth = Math.max(0, depth + delta);
  }
  return out.join("\n");
}
function b64EncodeText(s) {
  const bytes = new TextEncoder().encode(s);
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}
function b64DecodeText(s) {
  const bin = atob(s.replace(/\s/g, ""));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
}
