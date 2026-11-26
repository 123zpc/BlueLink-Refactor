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
  peerConvTitle: "", // ㉛ 当前打开的设备会话标题（顶栏如实用）
  // 入口模型定版（v2.6.4，用户 2026-11-22 亲自复述确认）：会话详情只许由
  // 明确动作打开——会话列表点选、连接成功落点、定位/通知直达；本旗标
  // 记「详情已被打开过」，左栏「聊天」的分流（goChat）以它为恢复优先据。
  chatDetailOpen: false,
  connectedConvId: null, // ㉛ 实际连接的对端会话 id（落点规则用）
  linkState: "off", // #17 连接链路状态：off/connecting/on/reconnecting（断开钮显隐的唯一依据）
  aiConvId: "ai-default",
  replyTo: null,
  renderedChat: new Set(),
  renderedAi: new Set(),
  aiGenerating: false,
  pendingAskRef: null, // ㉒ 问AI 待发送的引用 { convId, msgId }
  locateInProgress: false, // ㉔ 定位期间抑制 loadAiMessages 并发重渲染
  streamEl: null,
  streamText: "",
  streamWaiting: false,
  categories: [],
  catEditId: null,
  pendingFiles: [],
  aiPendingAtt: [], // AI 文本附件待发队列（v1.0.1）：{path, name, size, content}
  aiAttMap: {}, // 当前 AI 会话的附件清单（message_id → [{name,size,path}]）
  transferMap: {},
  // #2 传输权威记账（v2.6.6）：transfer_id → { msgId, received,
  // total, state }。事件先记账再上屏；气泡迟建（分页补页/重渲染）
  // 时按账回放进度与终态。旧版只有 transferMap 单向映射 + 事件
  // 到达时现查气泡 + lastFileBubble 单例兜底：连珠速发时事件先于
  // 气泡到达、或重渲染把单例换掉，进度就冻在半路（v2.6.5 验后
  // #2 真机实锤：文件 130ms 已达，前端双双冻在 66%）。
  transfers: {},
  // 滚动定版（v2.6.2，用户亲自收口）：每容器一个跟随开关——贴底时
  // 为 true；用户手势（滚轮/触摸/拖滚动条/键盘）一出立即置 false，
  // 不靠位置猜；恢复只由自滚回底/点回到最新/本人发送触发。
  follow: { chat: true, ai: true },
  aiWaitRows: {}, // #3：各消息容器当前的 AI 等待行（按 container.id 登记），问题行落库渲染时锚定到其正下方
  quoteFrom: null,
  favIds: new Set(),
  pins: [],
  pinIds: new Set(),
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
// ---- AI 回复 Markdown 渲染（⑩⑫，v2.6.1）----
// AI 页与聊天 @AI 气泡共用。原则：先按行切分原始文本，代码块收原始
// 代码（显示时转义、复制时给原文），其余行先转义再套行内格式——原始
// HTML 永远进不了 DOM。流式中途的半截 Markdown 必须容错：未闭合的
// 围栏先按代码块渲染（等后续分片补上闭合围栏自然成型）。
// 代码块原文按全局序号暂存：渲染出的 HTML 只带序号，复制按钮按序号
// 取原文（塞进 HTML 属性一转义就对不回原文）。序号只增不复用，多条
// 消息同屏互不串扰；暂存超过 500 段时整体清空（极老气泡的复制按钮
// 会提示失效，可接受）。
const MD_CODE_BLOCKS = new Map();
let MD_CODE_SEQ = 0;

function mdInline(t) {
  // t 必须已 esc；在此基础上套行内格式（代码/粗体/斜体/链接）
  return t
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/(^|[^*\w])\*([^*\n]+)\*/g, "$1<i>$2</i>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>')
    .replace(/(^|[\s（(])((?:https?:\/\/)[^\s<）)]+)/g, '$1<a href="$2" target="_blank" rel="noopener noreferrer">$2</a>');
}

function md(s) {
  const lines = String(s ?? "").split("\n");
  let out = "", listOpen = false;
  const closeList = () => { if (listOpen) { out += "</ul>"; listOpen = false; } };
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^```([A-Za-z0-9+#-]*)\s*$/);
    if (fence) {
      closeList();
      const buf = [];
      i++;
      while (i < lines.length && !/^```\s*$/.test(lines[i])) { buf.push(lines[i]); i++; }
      i++; // 吃掉闭合围栏；流式未闭合时 i 已到尾，无妨
      const raw = buf.join("\n");
      if (MD_CODE_BLOCKS.size > 500) MD_CODE_BLOCKS.clear();
      const id = ++MD_CODE_SEQ;
      MD_CODE_BLOCKS.set(id, raw);
      const lang = fence[1] || "代码";
      // #7 代码高亮（v2.6.2）：已闭合的代码块按围栏语言走查看器同一
      // 套 tokenizer 上色（浅/深同主题）；流式中未闭合的块先素色，
      // 收尾闭合后在下一片重渲染时自然补上高亮，不拖卡流式。
      const closedBlock = i <= lines.length && buf.length >= 0 && (i - 1 < lines.length) && /^```\s*$/.test(lines[i - 1] || "");
      let codeHtml = esc(raw);
      const fl = closedBlock && typeof langOfFence === "function" ? langOfFence(fence[1]) : null;
      if (fl && fl !== "text") {
        let st = { block: false, tpl: false };
        codeHtml = buf.map((ln) => {
          const r = tvTokenizeLine(ln, fl, st);
          st = r.st;
          return r.html;
        }).join("\n");
      }
      out += `<div class="md-code"><div class="md-code-head"><span>${esc(lang)}</span><button type="button" class="md-copy" data-mdcopy="${id}">复制</button></div><pre><code>${codeHtml}</code></pre></div>`;
      continue;
    }
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      closeList();
      out += `<div class="md-h md-h${h[1].length}">${mdInline(esc(h[2]))}</div>`;
      i++;
      continue;
    }
    if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      closeList();
      out += '<hr class="md-hr">';
      i++;
      continue;
    }
    const li = line.match(/^\s*(?:[-*+]|\d+[.、)])\s+(.*)$/);
    if (li) {
      if (!listOpen) { out += "<ul class=\"md-ul\">"; listOpen = true; }
      out += `<li>${mdInline(esc(li[1]))}</li>`;
      i++;
      continue;
    }
    closeList();
    if (/^\s*>\s?/.test(line)) {
      out += `<div class="md-q">${mdInline(esc(line.replace(/^\s*>\s?/, "")))}</div>`;
    } else if (line.trim() !== "") {
      out += `<div class="md-p">${mdInline(esc(line))}</div>`;
    }
    i++;
  }
  closeList();
  return out;
}

// 代码块「复制」按钮：事件委托一处接管（气泡是动态渲染的）
document.addEventListener("click", (e) => {
  const btn = e.target.closest ? e.target.closest("[data-mdcopy]") : null;
  if (!btn) return;
  const raw = MD_CODE_BLOCKS.get(Number(btn.dataset.mdcopy));
  if (raw === undefined) { showToast("这段代码已失效，请重新生成"); return; }
  copyText(raw);
});

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
  // #8 万行粘贴防冻：超长内容必然是多行，直接钉上限高度，跳过对
  // 整段文本的 scrollHeight 测量（那会触发整篇布局、冻住界面）。
  if (el.value.length > 3000) {
    el.style.height = COMPOSER_MAX_H + "px";
  } else {
    el.style.height = "auto";
    el.style.height = Math.min(Math.max(el.scrollHeight, COMPOSER_MIN_H), COMPOSER_MAX_H) + "px";
  }
  // #4 思考胶囊对齐：AI 输入行单行时居中、多行时与发送键同排（CSS
  // 按 .grown 切换对齐方式，与聊天输入框的底排逻辑一致）。
  if (el.id === "ai-input") {
    const row = el.closest(".composer-row");
    if (row) row.classList.toggle("grown", el.scrollHeight > COMPOSER_MIN_H + 4 || el.value.length > 3000);
  }
  // #21：输入框长高/缩回时，可见中的 toast 锚定其当前上沿实时跟随
  if (typeof toastVisible === "function" && toastVisible()) positionToast();
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
// ---------- #2 传输记账与气泡绘制（v2.6.6） ----------
/** 进度事件记账：先落账，再由调用方决定画到哪个气泡（账是唯一
 * 权威，气泡在不在场不影响记录完整性）。 */
function noteTransferProgress(transfer_id, msg_id, received, total) {
  let rec = state.transfers[transfer_id];
  if (!rec) rec = state.transfers[transfer_id] = { msgId: null, received: 0, total: 0, state: null };
  if (msg_id) { rec.msgId = msg_id; state.transferMap[msg_id] = transfer_id; }
  rec.received = received;
  rec.total = total;
  return rec;
}
/** 状态事件记账（终态以最后到达的为准）。 */
function noteTransferState(transfer_id, tstate, msgId) {
  let rec = state.transfers[transfer_id];
  if (!rec) rec = state.transfers[transfer_id] = { msgId: null, received: 0, total: 0, state: null };
  // 后端状态事件自带 msg_id（评审补强）：直接绑账，不再依赖
  // progress 事件先行到达——无进度先行的取消/失败也能落定气泡
  if (msgId && !rec.msgId) { rec.msgId = msgId; state.transferMap[msgId] = transfer_id; }
  if (!rec.msgId) {
    const mid = Object.keys(state.transferMap).find((k) => state.transferMap[k] === transfer_id);
    if (mid) rec.msgId = mid;
  }
  rec.state = tstate;
  return rec;
}
function transferRecordOf(msgId) {
  const tid = state.transferMap[msgId];
  return tid ? state.transfers[tid] : null;
}
function bubbleOfMsg(mid) {
  if (!mid) return null;
  const row = document.querySelector(`[data-mid="${mid}"]`);
  return row ? row.querySelector(".bubble") : null;
}
const TRANSFER_TERMINAL = ["done", "cancelled", "verify_failed"];

// ---------- #8 传输圆环（v1.0.4，用户定版） ----------
// 进度从气泡内搬到气泡外侧的 32px 圆环：气泡本体尺寸全程不变。
// 环的模式（dataset.mode）：progress 传输中（绿弧+环心百分比，点环
// 取消）/ queue 排队等待（灰弧慢转）/ verify 校验中（绿弧快转）/
// failed 失败（红环+!，点环重试）/ done 完成动效中（对号描画→淡出）。
const TX_CIRC = 84.82; // 环周长：2π×13.5
/* 完成流转路径（用户定版「吸管液体」）：圆环在左上方（约十点半）开
 * 管口，连接段落在对号第一笔的延长线上（45°、与第一笔同线）直达
 * 左尖，再顺对号走到右尖。路径顺序 = 液体流向：自管口顺时针绕环
 * 一周回管口 → 连接段 → 对号。管口坐标 (3.84,10.14) 是第一笔延长
 * 线与环的交点（解圆方程得），连接段与第一笔同线、入口零拐弯。 */
const TX_FLOW_D = "M3.84 10.14 A13.5 13.5 0 0 1 29.5 16 A13.5 13.5 0 1 1 3.84 10.14 L10.5 16.8 L14.2 20.5 L21.8 12.2";
const TX_CHECK_D = "M10.5 16.8 L14.2 20.5 L21.8 12.2";
let TX_FLOW_LEN = 0, TX_CHECK_LEN = 0;
/** 流转路径总长与对号段长（实测优先；几何 API 不可用时用计算值兜底：
 * 环 84.82 + 连接段 9.42 + 对号 16.49 ≈ 110.73，对号段 16.49）。 */
function txFlowLens(flowEl) {
  if (!TX_FLOW_LEN) {
    try {
      const probe = document.createElementNS("http://www.w3.org/2000/svg", "path");
      probe.setAttribute("d", TX_CHECK_D);
      TX_FLOW_LEN = flowEl.getTotalLength();
      TX_CHECK_LEN = probe.getTotalLength();
      if (!TX_FLOW_LEN || !TX_CHECK_LEN) throw new Error("len0");
    } catch {
      TX_FLOW_LEN = 110.73; TX_CHECK_LEN = 16.49;
    }
  }
  return [TX_FLOW_LEN, TX_CHECK_LEN];
}
/** 停掉完成流转动画并把环元素复位（新状态绘制/收起前调用）。 */
function stopTxFlow(b) {
  if (b._flowRAF) { cancelAnimationFrame(b._flowRAF); b._flowRAF = 0; }
  const ring = b._txRing;
  if (!ring) return;
  const f = ring.querySelector(".tx-flow"), g = ring.querySelector(".tx-grayflow");
  if (f) { f.style.display = "none"; f.style.opacity = ""; }
  if (g) g.style.display = "none";
  const track = ring.querySelector(".tx-track");
  if (track) track.style.display = "";
  const arc = ring.querySelector(".tx-arc");
  if (arc) arc.style.display = "";
}
let txRingSeq = 0; // 等待环相位错开用（相邻环错半圈，不齐步走）
function makeTxRing() {
  const el = document.createElement("div");
  el.className = "tx-ring hidden";
  el.dataset.mode = "";
  el.innerHTML = `<svg class="tx-svg" width="32" height="32" viewBox="0 0 32 32">
    <circle class="tx-track" cx="16" cy="16" r="13.5" fill="none" stroke-width="3"/>
    <g transform="rotate(-90 16 16)"><g class="tx-spin"><circle class="tx-arc" cx="16" cy="16" r="13.5" fill="none" stroke-width="3" stroke-dasharray="${TX_CIRC}" stroke-dashoffset="${TX_CIRC}"/></g></g>
    <path class="tx-grayflow" d="${TX_FLOW_D}" style="display:none"/>
    <path class="tx-flow" d="${TX_FLOW_D}" style="display:none"/>
  </svg><span class="tx-num"></span><span class="tx-sym"></span>`;
  // 等待/校验的旋转相位按创建序错开（等待周期 2.4s，错半圈 = -1.2s）
  el.querySelector(".tx-spin").style.animationDelay = `${-(txRingSeq++ % 2) * 1.2}s`;
  return el;
}
/** 传输状态小字行（气泡之外）：已取消/校验失败的说明与重试入口。 */
function makeTxNote() {
  const el = document.createElement("div");
  el.className = "tx-note hidden";
  el.innerHTML = `<span class="tx-note-text"></span><button class="transfer-retry hidden">重试</button>`;
  el._retryBtn = el.querySelector(".transfer-retry");
  return el;
}
function paintTxNote(b, text, showRetry) {
  const note = b._txNote;
  if (!note) return;
  if (!text && !showRetry) { note.classList.add("hidden"); return; }
  note.classList.remove("hidden");
  note.querySelector(".tx-note-text").textContent = text || "";
  if (note._retryBtn) note._retryBtn.classList.toggle("hidden", !showRetry);
}
/** 把圆环切到某个模式并按需设百分比（progress 模式传 0..100）。 */
function paintTxRing(b, mode, pct) {
  const ring = b._txRing;
  if (!ring) return;
  if (b._ringHideTimer) { clearTimeout(b._ringHideTimer); b._ringHideTimer = 0; }
  if (b._ringWaitTimer) { clearTimeout(b._ringWaitTimer); b._ringWaitTimer = 0; }
  if (b._flowRAF || ring.dataset.mode === "done") stopTxFlow(b);
  const arc = ring.querySelector(".tx-arc");
  const num = ring.querySelector(".tx-num");
  const sym = ring.querySelector(".tx-sym");
  ring.classList.remove("hidden", "tx-progress", "tx-queue", "tx-verify", "tx-failed", "tx-done");
  ring.classList.add("tx-" + mode);
  ring.dataset.mode = mode;
  if (!b._ringShownAt) b._ringShownAt = Date.now();
  if (mode === "progress") {
    const p = Math.max(0, Math.min(100, pct || 0));
    arc.style.strokeDasharray = `${TX_CIRC}`;
    arc.style.strokeDashoffset = `${TX_CIRC * (1 - p / 100)}`;
    num.textContent = `${p}%`;
    sym.textContent = "✕";
    ring.title = "点击取消传输";
  } else if (mode === "queue") {
    arc.style.strokeDasharray = `${TX_CIRC * 0.25} ${TX_CIRC}`;
    arc.style.strokeDashoffset = "0";
    num.textContent = "";
    sym.textContent = "";
    ring.title = "排队等待发送";
  } else if (mode === "verify") {
    arc.style.strokeDasharray = `${TX_CIRC * 0.7} ${TX_CIRC}`;
    arc.style.strokeDashoffset = "0";
    num.textContent = "";
    sym.textContent = "";
    ring.title = "校验中";
  } else if (mode === "failed") {
    arc.style.strokeDasharray = `${TX_CIRC}`;
    arc.style.strokeDashoffset = "0";
    num.textContent = "";
    sym.textContent = "!";
    ring.title = "传输失败，点击重试";
  }
}
function hideTxRing(b) {
  const ring = b._txRing;
  if (!ring) return;
  if (b._ringHideTimer) { clearTimeout(b._ringHideTimer); b._ringHideTimer = 0; }
  if (b._ringWaitTimer) { clearTimeout(b._ringWaitTimer); b._ringWaitTimer = 0; }
  stopTxFlow(b);
  ring.classList.add("hidden");
  ring.classList.remove("tx-done");
  ring.dataset.mode = "";
  b._ringShownAt = 0;
}
/** 完成动效（用户定版「吸管液体」）：满格瞬间，绿色液体从圆环左上方
 * 管口沿与对号第一笔同线的连接段流入对号（流转 780ms、起步即全速，
 * 液体尾巴扫过哪段、哪段灰环随即消失，环被一段段抽空）；只剩对号
 * 停留 870ms 后渐隐 600ms，全程约 2.3s，纯视觉不阻塞后续传输。
 * 小文件传得太快时先补足最小展示时长（450ms）再播，防一闪而过；
 * 回放（翻历史/重渲染）不播、直接收起。 */
function completeTxRing(b, replay) {
  const ring = b._txRing;
  if (!ring) return;
  if (replay || !b._ringShownAt || ring.classList.contains("hidden")) {
    hideTxRing(b);
    return;
  }
  const start = () => {
    b._ringWaitTimer = 0;
    // 已被 hideTxRing 收尾（取消/重发竞态）就不再强行播动效
    if (!ring.isConnected || !b._ringShownAt) return;
    const arc = ring.querySelector(".tx-arc");
    const track = ring.querySelector(".tx-track");
    const flow = ring.querySelector(".tx-flow");
    const gray = ring.querySelector(".tx-grayflow");
    ring.querySelector(".tx-num").textContent = "100%";
    ring.classList.remove("tx-progress", "tx-queue", "tx-verify", "tx-failed");
    ring.classList.add("tx-done");
    ring.dataset.mode = "done";
    ring.title = "";
    if (!flow || !gray) { // 兜底：流转路径缺失时按时收起，不留残环
      b._ringHideTimer = setTimeout(() => hideTxRing(b), 1450);
      return;
    }
    const [LTOT, LCHK] = txFlowLens(flow);
    const LC = TX_CIRC;
    // 切换到流转路径：液段先铺满整环（与满格弧同像素，无缝衔接），
    // 灰环改由流转路径的窗口呈现，随液尾推进分段消失
    arc.style.display = "none";
    track.style.display = "none";
    flow.style.display = "";
    gray.style.display = "";
    flow.style.opacity = "1";
    const setSlug = (tl, h) => {
      flow.style.strokeDasharray = `${Math.max(h - tl, 0.01)} ${LTOT}`;
      flow.style.strokeDashoffset = `${-tl}`;
      const gl = Math.max(LC - tl, 0);
      gray.style.strokeDasharray = `${gl} ${LTOT}`;
      gray.style.strokeDashoffset = `${-tl}`;
    };
    setSlug(0, LC);
    const FLOW_MS = 780, HOLD_MS = 870, FADE_MS = 600;
    const t0 = performance.now();
    const step = (now) => {
      const t = now - t0;
      if (t < FLOW_MS) {
        const e = 1 - Math.pow(1 - t / FLOW_MS, 3); // easeOut：起步即全速
        setSlug((LTOT - LCHK) * e, LC + (LTOT - LC) * e);
      } else if (t < FLOW_MS + HOLD_MS) {
        setSlug(LTOT - LCHK, LTOT);
      } else if (t < FLOW_MS + HOLD_MS + FADE_MS) {
        setSlug(LTOT - LCHK, LTOT);
        flow.style.opacity = `${1 - (t - FLOW_MS - HOLD_MS) / FADE_MS}`;
      } else {
        b._flowRAF = 0;
        hideTxRing(b);
        return;
      }
      b._flowRAF = requestAnimationFrame(step);
    };
    // 兜底：窗口隐藏等场景 rAF 被节流时，到点由定时器收尾
    b._ringHideTimer = setTimeout(() => hideTxRing(b), FLOW_MS + HOLD_MS + FADE_MS + 400);
    b._flowRAF = requestAnimationFrame(step);
  };
  const wait = Math.max(0, 450 - (Date.now() - b._ringShownAt));
  if (wait) b._ringWaitTimer = setTimeout(start, wait);
  else start();
}

/** 把一条传输记录的进度画到气泡上（事件实时绘制与气泡迟建回放共用）。 */
function paintTransferProgress(b, rec) {
  const { received, total } = rec;
  // #18 图片气泡：进度走贴底细条 + 胶囊明示「发送中/接收中」，
  // 与「校验中」严格分开（校验是传输完成后的独立状态）。
  if (b.classList.contains("img-bubble") && b._imgWrap) {
    const strip = b._imgWrap.querySelector(".img-strip");
    const sfill = b._imgWrap.querySelector(".img-strip > div");
    const cap = b._imgWrap.querySelector(".img-cap");
    const pct = total ? Math.floor((received / total) * 100) : 0;
    if (strip && sfill) {
      strip.classList.remove("hidden");
      sfill.style.width = pct + "%";
    }
    if (cap) {
      cap.classList.remove("hidden");
      const mine = b.closest(".msg-row")?.dataset.sender === "me";
      cap.textContent = mine ? `发送中 ${pct}%` : `接收中 ${pct}%`;
    }
    if (received < total && !b.dataset.flowing) {
      const side = b.closest(".msg-row")?.dataset.sender === "me" ? "me" : "peer";
      b.dataset.flowing = side;
      ringSet(side, true);
    }
    return;
  }
  // #8：文件气泡进度改走气泡外圆环（旧气泡内进度条/百分比行/取消
  // 按钮已全撤——它们的显隐就是气泡闪烁变尺寸的病根）
  paintTxRing(b, "progress", total ? Math.floor((received / total) * 100) : 0);
  if (received < total && !b.dataset.flowing) {
    const side = b.closest(".msg-row")?.dataset.sender === "me" ? "me" : "peer";
    b.dataset.flowing = side;
    ringSet(side, true);
  }
}

/** 把一条传输记录的状态画到气泡上（事件实时绘制与迟建回放共用；
 * 对已终态的传输再调用一次即「就地落定」，取消按钮处理用）。 */
function paintTransferState(b, rec, replay) {
  const tstate = rec.state;
  const mid = rec.msgId;
  if (!tstate) return;
  // 重试按钮已随 #8 挪到气泡外小字行（图片气泡的失败卡也用它）
  const retry = b.querySelector(".transfer-retry") || (b._txNote ? b._txNote._retryBtn : null);
  const showRetry = () => {
    if (!retry) return;
    retry.classList.remove("hidden");
    if (b._txNote && retry === b._txNote._retryBtn) b._txNote.classList.remove("hidden");
  };
  if (b.dataset.flowing) { ringSet(b.dataset.flowing, false); delete b.dataset.flowing; }
  // #1/#18 图片气泡的状态机：细条与胶囊承载全部状态，失败换失败卡
  if (b.classList.contains("img-bubble") && b._imgWrap) {
    const wrap = b._imgWrap;
    const strip = wrap.querySelector(".img-strip");
    const sfill = wrap.querySelector(".img-strip > div");
    const cap = wrap.querySelector(".img-cap");
    const fail = wrap.querySelector(".img-fail");
    const photo = wrap.querySelector(".img-photo");
    if (tstate === "queued") {
      if (cap) { cap.classList.remove("hidden"); cap.textContent = "排队等待"; }
    } else if (tstate === "verifying") {
      if (strip && sfill) { strip.classList.remove("hidden"); sfill.style.width = "100%"; }
      if (cap) { cap.classList.remove("hidden"); cap.textContent = "校验中"; }
    } else if (tstate === "done") {
      if (mid) attachImagePhoto(b, mid);
      // ⑥ v1.0.2：回放时同步隐藏、不用 400ms 延时（旧版回放也走延时，
      // 整页重渲染时所有已完成气泡先露进度条再消失、齐闪一下）
      if (replay) {
        if (strip) strip.classList.add("hidden");
        if (cap) cap.classList.add("hidden");
      } else {
        setTimeout(() => {
          if (strip) strip.classList.add("hidden");
          if (cap) cap.classList.add("hidden");
        }, 400);
      }
    } else if (tstate === "verify_failed") {
      if (strip) strip.classList.add("hidden");
      if (cap) cap.classList.add("hidden");
      if (photo) photo.classList.add("hidden");
      if (fail) fail.classList.remove("hidden");
      showRetry();
    } else if (tstate === "cancelled") {
      if (strip) strip.classList.add("hidden");
      if (cap) { cap.classList.remove("hidden"); cap.textContent = "已取消"; }
      showRetry();
    }
    return;
  }
  // #8：文件气泡状态改走圆环 + 气泡外小字行（旧气泡内进度行/按钮全撤）
  if (tstate === "queued") {
    paintTxRing(b, "queue");
    paintTxNote(b, "", false);
  } else if (tstate === "done") {
    // 接收到的单张图片：完成时补挂照片（照片出现本身即完成反馈，
    // attachImagePhoto 会顺手收起圆环）；其余文件走完成对号动效
    const fname = b.querySelector(".file-name");
    if (mid && fname && isImageExt(fileExt(fname.textContent || ""))) attachImagePreview(b, mid);
    completeTxRing(b, replay);
    paintTxNote(b, "", false);
  } else if (tstate === "verifying") {
    paintTxRing(b, "verify");
  } else if (tstate === "cancelled") {
    hideTxRing(b);
    paintTxNote(b, "已取消", true);
  } else if (tstate === "verify_failed") {
    paintTxRing(b, "failed");
    paintTxNote(b, "校验失败，文件不完整", true);
  }
}

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
  // #8：文件/图组气泡预备外挂圆环与状态小字行（组气泡构建函数与各
  // 绘制函数都经 bubble._txRing/_txNote 找它们，须先于 kind 分支备好）
  if (m.kind === "file" || m.kind === "imagegroup") {
    bubble._txRing = makeTxRing();
    bubble._txNote = makeTxNote();
  }
  if (m.reply_to && m.kind === "text" && !opts.ai) {
    const q = document.createElement("div");
    q.className = "quote-block";
    q.textContent = "引用加载中…";
    q.addEventListener("click", () => jumpToQuoted(m));
    bubble.appendChild(q);
    fillQuote(q, m);
  }
  if (m.kind === "imagegroup") {
    row.classList.add("img-row");
    buildGroupBubble(bubble, m, row);
  } else if (m.kind === "file") {
    bubble.innerHTML = `<div class="file-card-inner"><span class="file-ico">${iconUse("i-file", "ico ico-20")}</span><span class="file-main"><span class="file-name">${esc(m.body)}</span><span class="file-meta">文件</span></span></div>`;
    // ㉑ 单张图片在气泡里直接出预览（详见 attachImagePreview）
    if (isImageExt(fileExt(m.body))) attachImagePhoto(bubble, m.id);
    bubble.addEventListener("click", (e) => {
      if (e.target.closest("button")) return;
      openFileSmart(m, e.clientX, e.clientY);
    });
    // #8 重发（失败环点击与小字行重试按钮共用一条路径）
    const doResend = async () => {
      try {
        await invoke("resend_message", { messageId: m.id });
        row.remove();
        state.renderedChat.delete(m.id);
      } catch (err) { showToast("重发失败：" + err); }
    };
    if (bubble._txNote) bubble._txNote._retryBtn.addEventListener("click", doResend);
    // #8 圆环点击：传输/排队/校验中 = 取消传输；失败 = 重发
    if (bubble._txRing) bubble._txRing.addEventListener("click", async () => {
      const mode = bubble._txRing.dataset.mode;
      if (mode === "failed") { await doResend(); return; }
      if (mode !== "progress" && mode !== "queue" && mode !== "verify") return;
      // #2：对已终态的传输点取消 = 就地落定气泡（旧版把取消发给
      // 早已结束的传输、界面纹丝不动，用户以为取消失效）
      const rec = transferRecordOf(m.id);
      if (rec && TRANSFER_TERMINAL.includes(rec.state)) { paintTransferState(bubble, rec); return; }
      const tid = state.transferMap[m.id];
      if (tid) await invoke("cancel_transfer", { transferId: tid });
    });
    // #2 回放：气泡迟建（分页补页/整页重渲染）时按权威账补画
    // 进度与终态——事件比气泡先到也不丢、全部落定
    // ⑥ v1.0.2：已终态（TRANSFER_TERMINAL）回放不再画进度、直接落定——
    // 旧版无条件先画进度再由终态 400ms 后隐藏，连接成功整页重渲染时
    // 本次运行所有已完成气泡齐闪一下进度条。进行中/校验中照旧画进度。
    const trec = transferRecordOf(m.id);
    if (trec) {
      if (trec.total && !TRANSFER_TERMINAL.includes(trec.state)) paintTransferProgress(bubble, trec);
      if (trec.state) paintTransferState(bubble, trec, true);
    }
  } else if (m.sender === "ai") {
    bubble.innerHTML = md(m.body);
  } else if (m.body.length > 20000) {
    // #2 渲染保险（v2.6.5）：巨型正文（旧版本/对方发来的）整篇进 DOM
    // 布局同样冻死——显示截断到 2 万字并注明，全文不在气泡里展开。
    const textDiv = document.createElement("div");
    textDiv.className = "msg-text collapsed";
    textDiv.textContent = m.body.slice(0, 20000);
    const note = document.createElement("div");
    note.className = "muted small";
    note.textContent = `（全文 ${m.body.length.toLocaleString()} 字，过长已截断显示；完整内容请以文件发送）`;
    bubble.appendChild(textDiv);
    bubble.appendChild(note);
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
  if (opts.ai && m.sender === "ai") btns.push(["复制", () => copyText(m.body)], ["重新生成", () => regenerateAi()]);
  // 聊天里的 @AI 回答同样给复制/重新生成（v2.6.1 ⑭：此前只有 AI 页有，
  // 聊天侧点了没反应/没入口）；重新生成走 chat_regenerate_ai 按原问题重答
  if (!opts.ai && m.sender === "ai") btns.push(["复制", () => copyText(m.body)], ["重新生成", () => chatRegenerateAi(m)]);
  // #10 AI 回复收起（v2.6.2）：长回复看过一遍可折成收缩条，状态按
  // 消息 id 持久化（localStorage，重启不丢），两处（AI 页/聊天）同效。
  if (m.sender === "ai") btns.push([aiCollapsed(m.id) ? "展开" : "收起", (e) => toggleAiCollapse(m, bubble, e.currentTarget)]);
  for (const [label, fn] of btns) {
    const b = document.createElement("button");
    b.textContent = label;
    if (label === "收藏") { b.classList.add("fav-btn"); paintFavBtn(b, state.favIds.has(m.id)); }
    if (label === "置顶") { b.classList.add("pin-btn"); paintPinBtn(b, state.pinIds.has(m.id)); }
    if (m.sender === "ai" && (label === "展开" || label === "收起")) b.classList.add("ai-collapse-btn");
    b.addEventListener("click", fn);
    actions.appendChild(b);
  }
  if (m.sender === "ai" && aiCollapsed(m.id)) applyAiCollapse(bubble, m);
  meta.appendChild(status); meta.appendChild(time); meta.appendChild(actions);
  // 聊天里的 AI 回答（@AI 提问产生）加一个「AI 回答」标记，与对方消息区分
  if (!opts.ai && m.sender === "ai") {
    const badge = document.createElement("div");
    badge.className = "ai-badge";
    badge.textContent = "AI 回答";
    wrap.appendChild(badge);
  }
  // #8：文件/图组气泡与圆环并排成一行（环与气泡本体垂直居中、不含
  // 下方 meta 行——用户点验口径），状态小字行挂在行下方、气泡之外
  if (bubble._txRing) {
    const line = document.createElement("div");
    line.className = "bubble-line";
    line.appendChild(bubble);
    line.appendChild(bubble._txRing);
    wrap.appendChild(line);
    wrap.appendChild(bubble._txNote);
  } else {
    wrap.appendChild(bubble);
  }
  wrap.appendChild(meta);
  row.appendChild(wrap);
  row.addEventListener("contextmenu", (e) => { e.preventDefault(); openMsgMenu(m, e.clientX, e.clientY, !!opts.ai); });
  refreshMessageCategory(m.id, tagsSpan);
  return row;
}

// ---------- #10 AI 回复收起 ----------
function aiCollapsedSet() {
  try { return new Set(JSON.parse(localStorage.getItem("bl-collapsed-msgs") || "[]")); }
  catch (_) { return new Set(); }
}
function aiCollapsed(mid) { return aiCollapsedSet().has(mid); }
function applyAiCollapse(bubble, m) {
  const first = (m.body || "").split("\n").map((l) => l.replace(/[#>*`\-]/g, "").trim()).find((l) => l) || "AI 回复";
  bubble.innerHTML = "";
  const bar = document.createElement("div");
  bar.className = "ai-collapsed-bar";
  bar.innerHTML = `<span class="ai-collapsed-text">${esc(first.slice(0, 48))}</span><span class="ai-collapsed-hint">已收起 · 点击展开</span>`;
  bar.addEventListener("click", () => toggleAiCollapse(m, bubble, null));
  bubble.appendChild(bar);
}
function toggleAiCollapse(m, bubble, _btn) {
  const set = aiCollapsedSet();
  if (set.has(m.id)) set.delete(m.id); else set.add(m.id);
  localStorage.setItem("bl-collapsed-msgs", JSON.stringify([...set].slice(-500)));
  const collapsed = set.has(m.id);
  if (collapsed) {
    applyAiCollapse(bubble, m);
  } else {
    bubble.innerHTML = md(m.body);
  }
  // #9 单一状态源（v2.6.5）：状态只认持久化集合，切换后把这条消息的
  // 全部收起/展开控件统一回写——旧版只改被点的那个按钮，从收缩条
  // 展开后动作列按钮还停在「展开」（真机实锤）。展开态只许出现
  // 「收起」、收起态只许出现「展开」，两处入口永远一致。
  const row = bubble.closest(".msg-row");
  if (row) {
    row.querySelectorAll(".ai-collapse-btn").forEach((b) => {
      b.textContent = collapsed ? "展开" : "收起";
    });
  }
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

/** #11 统一消息摘要（v2.6.5）：凡是把一条消息压成一行展示的地方都
 * 走这里——按消息类型生成人话摘要，内部载荷（图片组正文是 JSON
 * 清单）一律不许原样上屏（v2.6.4 在置顶条/收藏/彩签弹层三处实锤）。
 * max 是文本节选的字数上限。 */
function msgSummary(m, max = 60) {
  if (!m) return "";
  if (m.kind === "imagegroup") {
    // 清单体是对象 {v, items:[…]}（不是数组）：取 items 长度
    let n = 0;
    try {
      const g = JSON.parse(m.body);
      const arr = Array.isArray(g) ? g : (g && g.items);
      n = arr ? arr.length : 0;
    } catch (_) { n = 0; }
    return n > 0 ? `图片组 · 共 ${n} 张` : "图片组";
  }
  if (m.kind === "file") {
    return isImageExt(fileExt(m.body || "")) ? `图片 · ${m.body}` : `文件 · ${m.body}`;
  }
  const t = (m.body || "").replace(/\s+/g, " ").trim();
  return t.length > max ? t.slice(0, max) + "…" : t;
}

async function fillQuote(el, m) {
  try {
    let orig = quoteCache.get(m.reply_to);
    if (!orig) {
      orig = await invoke("get_message", { messageId: m.reply_to });
      if (orig) quoteCache.set(m.reply_to, orig);
    }
    if (!orig) { el.textContent = "原消息已删除"; return; }
    const who = orig.sender === "me" ? "我" : orig.sender === "ai" ? "AI" : "对方";
    el.textContent = `${who}：${msgSummary(orig, 80)}`;
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
  // 日期分隔：基准行必须是真消息行——AI 等待行没有时间戳，拿它当基准
  // 会算出假的「新的一天」、把分隔线插到等待气泡和问题之间（#3 实锤）。
  const rows = [...container.querySelectorAll(".msg-row")].filter((r) => !r.dataset.waiting);
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
  // v1.0.1：AI 页用户消息的附件文件片（数据来自 state.aiAttMap，
  // loadAiMessages/发送后都会刷新；实时到达时 map 可能还没刷新，
  // fillAiAttChips 无数据即跳过，发送方 finally 的重渲染会补上）
  if (opts.ai && m.sender === "me") fillAiAttChips(el, m);
  // 消息分组：同日、同发送者、间隔 < 5 分钟时压缩间距
  if (!newDay && last && last.dataset.sender === m.sender && m.created_at - lastTs < 5 * 60 * 1000) {
    el.classList.add("grouped");
  }
  container.appendChild(el);
  renderedSet.add(m.id);
  // #3 等待行锚定（v2.6.5 重做，聊天与 AI 页同机制）：等待/流式行创建
  // 时问题可能还没落库渲染，只能先挂末尾；问题行一到，立刻把等待行
  // 挪到问题行正下方——等待→流式→完成全生命周期不许跳位。旧版只给
  // 聊天容器做了锚定且 AI 页全无，等待气泡永远停在问题上方、完成时
  // 靠整页重渲染「弹回」正确位置（v2.6.4 真机实锤）。
  const wr = state.aiWaitRows && state.aiWaitRows[container.id];
  if (wr && wr.isConnected && m.sender === "me") {
    // 锚点按消息 id 绑定（第三方评审采纳）：未绑定时第一条到达的
    // 「我」的消息就是提问、绑定它；绑定后只有锚点消息本身到达才
    // 挪行——等待期间用户再发普通消息，等待行不许被新消息「拐走」。
    if (!wr.dataset.anchorId) {
      wr.dataset.anchorId = m.id;
      el.after(wr);
    } else if (wr.dataset.anchorId === m.id) {
      el.after(wr);
    }
  }
  // 滚动定版：跟随开着才贴底；关着时 scrollTop 分毫不动。
  const fkey = container.id === "ai-messages" ? "ai" : "chat";
  if (state.follow[fkey]) container.scrollTop = container.scrollHeight;
  if (container.id === "messages") syncBottomBtn();
  if (container.id === "ai-messages") syncAiBottomBtn();
}

/** 唯一的定位链路：切会话→加载消息→滚动到目标→高亮闪烁两次。
 *  colorHex 可选：从某个彩签分类发起的定位（点彩签、悬停列表、分类总览）传该分类颜色，
 *  脉冲高亮就用分类色；引用/搜索/收藏/置顶发起的定位不传，仍用默认蓝色。 */
async function locateMessage(conversationId, messageId, colorHex) {
  const isAi = conversationId === state.aiConvId || conversationId.startsWith("ai");
  // ㉔：定位期间置旗，switchView 触发的 loadAiMessages 不得并发重渲染
  // 同一容器——旧实现两边赛跑：整段加载刚滚到目标，分页加载随后完成
  // 又把容器清空重铺，滚动落点被冲掉（AI 页落点不准的主因）。
  state.locateInProgress = true;
  try {
    // ㉕：定位到非当前打开的 AI 会话时先认领它，否则界面显示的是这个
    // 会话的消息、发送却落进另一个会话（串话）
    if (isAi && conversationId !== state.aiConvId) {
      state.aiConvId = conversationId;
    }
    // 入口模型定版：定位直达同样是「明确打开详情」，置旗供切回恢复
    if (!isAi) state.chatDetailOpen = true;
    switchView(isAi ? "ai" : "chat");
    if (isAi) loadAiConversations();
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
      // 等布局落定（双 rAF）再滚：Markdown 渲染后的高度要一两帧才稳，
      // 提前滚必偏；滚后在短窗口内再校正一次（图片等异步撑高时补正）。
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      el.scrollIntoView({ block: "center" });
      if (colorHex) el.style.setProperty("--flash-ring", colorHex);
      el.classList.add("flash");
      setTimeout(() => {
        if (!el.isConnected) return;
        const r = el.getBoundingClientRect();
        const cr = container.getBoundingClientRect();
        const off = (r.top + r.height / 2) - (cr.top + cr.height / 2);
        if (Math.abs(off) > 48) el.scrollIntoView({ block: "center" });
      }, 400);
      setTimeout(() => { el.classList.remove("flash"); el.style.removeProperty("--flash-ring"); }, 1600);
    }
  } finally {
    state.locateInProgress = false;
  }
  closeDrawer();
}

/** ㉛ 未连接态的聊天页形态：输入行换成提示带（同一位置），顶栏状态
 * 如实。浏览路径（会话列表/消息加载/分页/跳转）一律不看连接状态——
 * 历史是本地数据。 */
function syncOfflineUI() {
  const off = !state.connected;
  $("offline-band").classList.toggle("hidden", !off);
  document.querySelector("#composer .composer-row").classList.toggle("hidden", off);
  // #17 断开钮随连接状态走（v2.6.5）：未连接时根本不显示（旧版只是
  // 禁用、还摆在顶栏——状态写着未连接却挂着「断开」，真机两度实锤）；
  // 连接中/已连接/重连中才出现，可点断开/取消。
  const showDisc = state.linkState && state.linkState !== "off";
  $("btn-disconnect").classList.toggle("hidden", !showDisc);
  $("btn-disconnect").disabled = false;
  syncPeerHead();
}

/** ㉛ 聊天顶栏如实：标题是当前打开会话的设备名；状态点/文案——
 * 未连接=灰点「未连接」；连的就是本会话=绿点「已连接」；连的是别的
 * 设备=绿点并写明「当前连接：X」，不假装本会话可发。 */
function syncPeerHead() {
  // #2：没有打开任何会话时不摆名为「未连接」的假会话标题
  $("peer-name").textContent = state.peerConvTitle || "暂无会话";
  const dot = $("peer-dot");
  const txt = $("peer-state-text");
  if (!state.connected) {
    dot.className = "status-dot";
    txt.textContent = "未连接";
  } else if (state.connectedConvId && state.peerConvId && state.connectedConvId !== state.peerConvId) {
    dot.className = "status-dot on";
    txt.textContent = `当前连接：${state.peerName || "其他设备"}`;
  } else {
    dot.className = "status-dot on";
    txt.textContent = "已连接";
  }
  $("nav-conn-dot").classList.toggle("on", !!state.connected);
}
$("btn-go-connect").addEventListener("click", () => switchView("connect"));
document.querySelector(".capsule").addEventListener("click", () => switchView("connect"));

// ---------- 视图切换 ----------
function switchView(view) {
  state.view = view;
  // 左栏目的地各自高亮；连接页是独立模块、不进左栏（v2.6.3 用户裁定），
  // 身处连接页时没有任何左栏项匹配，天然无高亮——进入路径只有冷启动落点、
  // 未连接提示带「去连接」按钮与顶栏状态胶囊，都直接调 switchView("connect")。
  document.querySelectorAll(".nav-item").forEach((b) =>
    b.classList.toggle("active", b.dataset.view === view));
  $("chat-view").classList.toggle("hidden", view !== "chat");
  $("connect-view").classList.toggle("hidden", view !== "connect");
  $("ai-view").classList.toggle("hidden", view !== "ai");
  $("tags-view").classList.toggle("hidden", view !== "tags");
  $("fav-view").classList.toggle("hidden", view !== "favorites");
  $("settings-view").classList.toggle("hidden", view !== "settings");
  closePinList();
  applyAside();
  syncDrawer();
  if (view === "chat") {
    syncOfflineUI();
    // 首次进聊天页且尚未渲染任何消息时才加载历史；已渲染过则保持
    // 浏览位置（导航互切不丢在看的会话与滚动位置）。
    if (state.peerConvId && state.renderedChat.size === 0) loadChatMessages();
  }
  // 视图刚变可见时补量一次输入框高度（隐藏期间 autoGrow 会跳过）
  autoGrow($("input"));
  autoGrow($("ai-input"));
  if (view === "tags") loadCategoryList();
  if (view === "favorites") loadFavList();
  if (view === "ai") { loadAiConversations(); loadAiMessages(); loadGatewaySettings(); }
  if (view === "settings") loadSettings();
  // 置顶栏按视图各一份（⑲）：进聊天/AI 页时都刷新到本会话的置顶
  if (view === "ai" || view === "chat") refreshPinBar();
  // #21：切页后可见中的 toast 按新页面的输入框重新锚定
  if (typeof toastVisible === "function" && toastVisible()) positionToast();
}

document.querySelectorAll(".nav-item").forEach((b) =>
  b.addEventListener("click", () => {
    // 「聊天」不直达视图：按入口模型定版分流（见 goChat）；其余页签直达。
    if (b.dataset.view === "chat") goChat();
    else switchView(b.dataset.view);
  }));

/** 入口模型定版（v2.6.4，用户 2026-11-22 亲自复述确认）——左栏「聊天」
 * （及命令面板/连接页「进入聊天」等同义入口）的分流，判定顺序：
 * ① 恢复优先：已打开过会话详情 → 回到刚才那个详情（切去别页再切回不丢）；
 * ② 已连接 → 当前连接对端的会话（「有连接了，那么就是这个会话」）；
 * ③ 未连接 → 一律落连接页（不分有无会话），不许落到「进聊天页但没选
 * 会话」的空态。进详情的手动路径唯一：展开会话列表点某个具体会话。
 * QA161 裁定（v2.6.6）：①只在已连接时成立——未连接（含刚断开、含
 * 刚手动看过离线详情）点「聊天」永远落连接页，恢复资格进未连接即清。 */
async function goChat() {
  // QA161 语义裁定（v2.6.6，用户 2026-11-22 拍板）：「切回恢复」只在
  // 已连接 regime 成立——未连接时点「聊天」一律落连接页，哪怕刚手动
  // 打开过离线详情（离线详情不进恢复资格）；顺手清掉恢复资格，进
  // 未连接状态即作废（断开事件处亦有清零，双保险）。
  if (!state.connected) {
    state.chatDetailOpen = false;
    switchView("connect");
    return;
  }
  if (state.chatDetailOpen && state.peerConvId) { switchView("chat"); return; }
  if (state.connected) {
    if (state.connectedConvId) {
      const switching = state.peerConvId !== state.connectedConvId;
      state.peerConvId = state.connectedConvId;
      if (switching) {
        const cur = (await invoke("list_conversations").catch(() => []))
          .find((c) => c.id === state.connectedConvId);
        if (cur) state.peerConvTitle = cur.title || "";
      }
      state.chatDetailOpen = true;
      switchView("chat");
      if (switching) loadChatMessages();
      syncPeerHead();
      return;
    }
    switchView("chat");
    return;
  }
  switchView("connect");
}

// ---------- 会话列表收起/展开（列表为应用级组件：聊天页与连接页可用；收起即整列消失，不留残条） ----------
// 默认收起（用户裁定，两次确认）：每次启动程序都以收起状态进入，不沿用上次退出时的展开状态；
// 本次运行内用户展开/收起的选择照常生效，重启后回到收起。
// 状态只许由悬浮开关改：页面切换永不自动弹开（v2.4.1 修——旧版导航点「聊天」时
// 会强制把收起状态改成展开，用户反馈一切换菜单列表就自己弹开）。
// 开关形态（v2.4.1 用户定版）：唯一的展开/收起入口是骑在左导航/列表与内容区分界线上的
// 圆形悬浮按钮（原页头 » 展开按钮与列表头 « 收起按钮已收敛删除），
// 聊天页与连接页出现（v2.6.4 入口模型定版：身处连接页也要能展开列表、
// 点会话进详情——未连接时进详情的手动路径唯一就是列表点选）。
let asideBootDone = false;
function applyAside() {
  if (!asideBootDone) {
    asideBootDone = true;
    localStorage.setItem("bl-aside", "collapsed");
  }
  const collapsed = localStorage.getItem("bl-aside") === "collapsed";
  // 列表可用页：聊天页与连接页（v2.6.4 起连接页同列）；其余页整列隐藏。
  const onListView = state.view === "chat" || state.view === "connect";
  $("aside").classList.toggle("hidden", !onListView || collapsed);
  // 悬浮开关定位：收起时贴导航右缘、展开时贴列表右缘（按实测宽：导航宽 + 列表实宽，
  // 列表隐藏时实宽为 0，自然落在导航右缘）；图标随状态翻转。
  const fb = $("btn-aside-float");
  fb.classList.toggle("hidden", !onListView);
  if (onListView) {
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
  // ⑲ 加固：拉取失败时保留现有列表、只记日志，绝不先清空再渲染——
  // 旧写法 invoke 一失败列表就整体变空，看起来像「会话不见了」；
  // 单条渲染异常也只跳过该条，不连累整表。
  let convs;
  try {
    convs = await invoke("list_conversations");
  } catch (e) {
    invoke("log_frontend", { msg: "会话列表加载失败（保留现有列表）：" + String(e) }).catch(() => {});
    return;
  }
  const list = $("conv-list");
  list.innerHTML = "";
  const peerConvs = convs.filter((c) => c.kind === "peer");
  // ㉛ 当前打开的会话不被列表刷新顶掉（落点/浏览位置保持）；只有它
  // 不存在了才回落到最近一条设备会话
  if (!peerConvs.some((c) => c.id === state.peerConvId)) {
    state.peerConvId = peerConvs.length ? peerConvs[peerConvs.length - 1].id : null;
  }
  const cur = peerConvs.find((c) => c.id === state.peerConvId);
  state.peerConvTitle = cur ? (cur.title || "") : state.peerConvTitle;
  for (const c of peerConvs) {
    try {
      const item = document.createElement("div");
      item.className = "list-item conv-item";
      item.innerHTML = `<span class="li-main"><span class="li-title"><span>${esc(c.title || "")}</span></span><span class="li-sub">设备会话</span></span>`;
      // ㉛ 点开会话即加载本地历史（与连接状态无关）；点已打开的会话
      // 不重载——浏览位置保持不重置
      item.addEventListener("click", () => {
        const switching = state.peerConvId !== c.id;
        state.peerConvId = c.id;
        state.peerConvTitle = c.title || "";
        state.chatDetailOpen = true; // 入口模型定版：进详情的手动路径唯一就是列表点选
        switchView("chat");
        if (switching) loadChatMessages();
        syncPeerHead();
      });
      list.appendChild(item);
    } catch (_) { /* 单条坏数据跳过，不打断整表 */ }
  }
  syncPeerHead();
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
  state.follow.chat = true;
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

/** 当前视图对应的置顶上下文（会话 + 栏/列表元素）。
 * ⑲ 定案：置顶的写入与单条状态读取一直是全局的（message_marks +
 * pinIds，显示「已置顶」没问题），但置顶栏/置顶列表此前只按设备会话
 * 过滤、且只存在于聊天页——在 AI 页置顶的消息在任何列表里都看不到，
 * 重启也不会恢复（结构性分叉，非数据丢失）。修法：两个视图各一份
 * 置顶栏/列表，各收录本会话的置顶。 */
function pinCtx() {
  return state.view === "ai"
    ? { conv: state.aiConvId, bar: $("ai-pin-bar"), list: $("ai-pin-list") }
    : { conv: state.peerConvId, bar: $("pin-bar"), list: $("pin-list") };
}

async function refreshPinBar() {
  try {
    const pins = await refreshPinIds();
    if (!pins) return;
    renderPinBar(pins, state.peerConvId, $("pin-bar"));
    renderPinBar(pins, state.aiConvId, $("ai-pin-bar"));
    const ctx = pinCtx();
    state.pins = pins.filter((m) => m.conversation_id === ctx.conv);
    if (!state.pins.length) closePinList();
  } catch (_) { /* 忽略 */ }
}

function renderPinBar(pins, convId, bar) {
  if (!bar) return;
  const mine = pins.filter((m) => m.conversation_id === convId);
  if (!mine.length) { bar.classList.add("hidden"); return; }
  bar.classList.remove("hidden");
  // 1 条：保持现行文案与点击直跳；≥2 条：文案提示可多选，点击先弹列表
  bar.innerHTML = mine.length === 1
    ? `${iconUse("i-pin", "ico ico-14")}<span>置顶：${esc(msgSummary(mine[0], 40))}</span>`
    : `${iconUse("i-pin", "ico ico-14")}<span>置顶消息 · 共 ${mine.length} 条（点击选择）</span>`;
  bar.onclick = () => {
    state.pins = mine;
    if (mine.length === 1) locateMessage(mine[0].conversation_id, mine[0].id);
    else togglePinList();
  };
  // ㉖ 单条时置顶栏本身就是那条置顶的条目：右键同样可取消置顶；
  // 多条时右键先展开列表，再在条目上操作
  bar.oncontextmenu = (e) => {
    e.preventDefault();
    if (mine.length === 1) openUnpinMenu(mine[0], e.clientX, e.clientY);
    else { state.pins = mine; openPinList(); }
  };
}

// ---------- 弹出菜单统一定位（v1.0.3 验后 #2） ----------
// 旧实现按写死数字猜菜单尺寸（高 240/160/120、宽 190），项数一多就
// 溢出窗口底边、末项被切（用户截图实证：底部文件消息 7 项菜单约
// 290px > 猜的 240px）。统一改为实测：菜单内容填充完毕后调用，
// 先以 visibility:hidden 显示量出真实宽高，再把左上角钳进视口
// （四边留 4px）；下方放不下自然贴底上移，右侧同理。
function placeMenuAt(menu, x, y) {
  menu.style.visibility = "hidden";
  menu.classList.remove("hidden");
  const w = menu.offsetWidth, h = menu.offsetHeight;
  menu.style.left = Math.max(4, Math.min(x, window.innerWidth - w - 4)) + "px";
  menu.style.top = Math.max(4, Math.min(y, window.innerHeight - h - 4)) + "px";
  menu.style.visibility = "";
}
// 锚点弹出层（彩签层/改名层/置顶列表/彩签悬停列表）收尾钳位：
// 调用方先按锚点设好 top/left 并显示，本函数按实测矩形把探出视口
// 的部分往回挪。矩形差值与 offsetTop/offsetLeft 同为像素偏移，
// 与 offsetParent 无关，可直接相加。
function clampPopIntoView(el) {
  const r = el.getBoundingClientRect();
  let dy = 0, dx = 0;
  if (r.bottom > window.innerHeight - 4) dy = window.innerHeight - 4 - r.bottom;
  if (r.top + dy < 4) dy = 4 - r.top;
  if (r.right > window.innerWidth - 4) dx = window.innerWidth - 4 - r.right;
  if (r.left + dx < 4) dx = 4 - r.left;
  if (dy) el.style.top = (el.offsetTop + dy) + "px";
  if (dx) el.style.left = (el.offsetLeft + dx) + "px";
}

/** ㉖ 取消置顶菜单（置顶列表条目/单条置顶栏共用）：点「取消置顶」先
 * 弹确认，确认才真取消；取消/关弹窗保持置顶不动。 */
function openUnpinMenu(m, x, y) {
  const menu = $("file-menu");
  menu.innerHTML = "";
  const b = document.createElement("button");
  b.textContent = "取消置顶";
  b.addEventListener("click", async () => {
    menu.classList.add("hidden");
    const ok = await showConfirm("取消置顶这条消息？", "取消置顶");
    if (!ok) return;
    await toggleMark(m.id, "pin");
    // 列表开着时按最新置顶集重渲染（refreshPinBar 已在无置顶时关列表）
    if (pinListOpen) {
      if (state.pins && state.pins.length) openPinList();
      else closePinList();
    }
  });
  menu.appendChild(b);
  placeMenuAt(menu, x, y);
}

// ---------- 置顶消息列表（≥2 条时点置顶栏弹出，点哪条跳哪条） ----------
let pinListOpen = false;
function togglePinList() { pinListOpen ? closePinList() : openPinList(); }
function openPinList() {
  const { list, bar } = pinCtx();
  if (!list || !bar) return;
  list.innerHTML = "";
  const head = document.createElement("div");
  head.className = "rl-head";
  head.innerHTML = `<b>置顶消息 · 共 ${state.pins.length} 条</b>`;
  list.appendChild(head);
  for (const m of state.pins) {
    const item = document.createElement("div");
    item.className = "rl-item";
    const main = msgSummary(m, 80);
    item.innerHTML = `<div class="rl-note">${esc(main)}</div><div class="rl-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div>`;
    item.title = main;
    // 回跳复用 locateMessage：目标不在已加载首屏时它会整段加载兜底定位
    item.addEventListener("click", () => { closePinList(); locateMessage(m.conversation_id, m.id); });
    // ㉖ 右键就地取消置顶（带二次确认）
    item.addEventListener("contextmenu", (e) => { e.preventDefault(); e.stopPropagation(); openUnpinMenu(m, e.clientX, e.clientY); });
    list.appendChild(item);
  }
  list.style.top = (bar.offsetTop + bar.offsetHeight + 4) + "px";
  list.classList.remove("hidden");
  clampPopIntoView(list); // v1.0.3 #2：矮窗口下实测钳位防探底（高度另有 46vh 上限）
  pinListOpen = true;
}
function closePinList() {
  pinListOpen = false;
  for (const id of ["pin-list", "ai-pin-list"]) {
    const list = $(id);
    if (list) list.classList.add("hidden");
  }
}
document.addEventListener("mousedown", (e) => {
  if (!pinListOpen) return;
  const { list, bar } = pinCtx();
  if (list && bar && !list.contains(e.target) && !bar.contains(e.target)) closePinList();
});
document.addEventListener("keydown", (e) => { if (e.key === "Escape") closePinList(); });

function setReply(m) {
  state.replyTo = m.id;
  const bar = $("reply-bar");
  bar.classList.remove("hidden");
  bar.innerHTML = `<span>引用：${esc(msgSummary(m, 50))}</span><button class="btn small-btn" id="reply-cancel">取消</button>`;
  $("reply-cancel").addEventListener("click", () => {
    state.replyTo = null;
    bar.classList.add("hidden");
  });
  $("input").focus();
}

// ---------- 代码/长文本识别：自动转成对应后缀的文档发送 ----------
let docSuggestion = null; // {name, kind: "code" | "long"}
let docDismissed = false; // 用户对当前内容选择了「仍按文本发送」

const FENCE_EXT = { rust: "rs", rs: "rs", python: "py", py: "py", javascript: "js", js: "js", typescript: "ts", ts: "ts", go: "go", java: "java", c: "c", cpp: "cpp", "c++": "cpp", json: "json", html: "html", css: "css", markdown: "md", md: "md", shell: "sh", bash: "sh", sh: "sh", text: "txt", txt: "txt", sql: "sql", magicapi: "ms", magic: "ms", magicscript: "ms", ms: "ms", csharp: "cs", "c#": "cs", cs: "cs" };

/** 识别输入内容是否应转文档：围栏代码块 / 全文强特征打分 / 超长
 * 纯文本。返回 {name, kind} 或 null。
 * 语言判定自 v2.6.6（#5）起走 classify.js 的全文权重打分（旧版只
 * 看头几行的特征表把 Java 判成 Python，用户实报）：打分有置信
 * 门槛，判不准就退纯文本——长文走长文.txt，短的按普通文字发。 */
function detectDocument(text) {
  const t = text.trim();
  if (t.length < 60) return null;
  const fence = t.match(/^```([a-zA-Z0-9+#-]*)\s*\n/);
  if (fence) {
    const ext = FENCE_EXT[(fence[1] || "txt").toLowerCase()] || "txt";
    return { name: "代码片段." + ext, kind: "code" };
  }
  if (window.Classify) {
    const r = Classify.classifyText(text);
    if (r.confident) return { name: "代码片段." + r.ext, kind: "code" };
  } else if (!window.__classifyWarned) {
    window.__classifyWarned = 1;
    console.warn("classify.js 未加载，语言识别降级为长文.txt");
  }
  if (t.length > 1000) return { name: "长文.txt", kind: "long" };
  return null;
}

let docBarTimer = 0;
function updateDocBar() {
  // 粘贴/输入事件高频触发：实际检测合并到 120ms 尾沿，主线程不被
  // 逐键检测堵住（#8：发送点击永远有响应）。
  if (docBarTimer) return;
  docBarTimer = setTimeout(() => { docBarTimer = 0; updateDocBarNow(); }, 120);
}
function updateDocBarNow() {
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
  $("attach-count").textContent = state.pendingFiles.length ? `待发送 ${state.pendingFiles.length} 个文件` : "";
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
// ③没有文件对象路径但剪贴板里有图片位图 → 截图粘贴，落盘入队：
//   先试 DataTransfer 的图片项，拿不到再向后端直读系统剪贴板图像
//   （v1.0.1 回落，Windows 截图在 WebView2 里只有这条路通）；
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
/** 草稿总量闸值：点发送时草稿超过这个字数，整体转成一个文件
 * 发送（文字路径要全文落库 + 渲染巨气泡，两头都是冻死源）。
 * 注意只是「发送瞬间」的闸——输入过程中不做任何转换（#4 定版，
 * 见下方粘贴处理）。 */
const PASTE_FILE_THRESHOLD = 100000;

/** 发送瞬间把整份草稿转成一个文件入队：采样分类 → 后端落盘 →
 * 进待发队列。只在 sendCurrent 里调用（#4 定版：转换时机=点击
 * 发送那一刻，按草稿总量整体判定，分批输入的内容合并成一份）。 */
async function stageHugeTextAsFile(text) {
  const det = detectDocument(text.slice(0, 20000));
  const name = det ? det.name : "长文.txt";
  try {
    const path = await invoke("stage_text_document", { name, content: text });
    const added = await stageFilePaths([path]);
    if (!added) throw new Error("文件入队失败");
    showToast(`内容过大（${text.length.toLocaleString()} 字），已整体转成文件「${name}」发送`);
    return true;
  } catch (err) {
    showToast("内容过大，转文件失败：" + err);
    return false;
  }
}

$("input").addEventListener("paste", async (e) => {
  const cd = e.clipboardData;
  if (!cd) return;
  const text = cd.getData("text/plain") || "";
  // #4 草稿整体化（v2.6.6，用户定版）：粘贴一律以文字形态进输入
  // 框累积——不管分几批、每批多大，输入过程中绝不做任何转文件
  // 动作（旧版单批超限即单独转文件，把一份草稿拆成两半，用户实
  // 报）。是否转文件、何种后缀，只在点发送的那一刻按草稿总量
  // 整体判定（sendCurrent 的闸）。防冻靠既有机制兜底：autoGrow
  // 对长内容跳过测量、识别打分有采样上限、草稿不存超限内容。
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
  // ③的回落（v1.0.1）：WebView 没把剪贴板图片交出来时，向后端要系统
  // 剪贴板里的原始图像。Windows 截图（Win+Shift+S）在 WebView2 的
  // DataTransfer 里只有一个取不到数据的 file 占位，getAsFile 拿不到
  // 图，旧版走到这里就直接报「未能读取」——但截图在剪贴板里是位图
  // 数据、本就没有文件形态，只有后端直读剪贴板这条路能拿到。
  // 文件路线与图像路线都确实没有，才落到下面的提示。
  try {
    const staged = await invoke("clipboard_take_image");
    if (staged && staged.path) {
      state.pendingFiles.push({
        path: staged.path,
        name: staged.path.split(/[\\/]/).pop() || "粘贴的图片.png",
        size: staged.size,
      });
      renderAttachBar();
      return;
    }
  } catch (_) { /* 回落失败不单独报错，与下方统一提示合并 */ }
  if (fileItems.length || (cd.files && cd.files.length)) {
    showToast(paths === null ? "此系统暂未支持粘贴文件，请用附件按钮选择" : "未能读取粘贴的文件，已忽略");
  }
});

async function sendCurrent() {
  const raw = $("input").value;
  let text = raw.trim();
  if (!text && !state.pendingFiles.length) return;
  // #2 发送侧同闸（v2.6.5）：不管内容是怎么进输入框的，超限一律先转
  // 文件入队、再走文件队列发送——文字发送路径要全文分类、落库后还要
  // 渲染成巨气泡，30 万行级两头都是冻死源。转文件后 text 清空，下方
  // 文件队列流程接管。
  if (raw.length > PASTE_FILE_THRESHOLD) {
    // 转文件成功后才清输入框（第三方评审采纳）：先清后转一旦失败，
    // 输入框与草稿都已清空、文件又没入队，内容会静默丢失。失败时
    // 输入框原样保留，用户可改走选文件等方式重试。
    const ok = await stageHugeTextAsFile(raw);
    if (!ok) return;
    $("input").value = ""; autoGrow($("input"));
    docDismissed = false;
    updateDocBar();
    invoke("save_draft", { conversationId: state.peerConvId, text: "" }).catch(() => {});
    text = "";
    if (!state.pendingFiles.length) return;
  }
  // 滚动定版：本人点发送 = 主动回到当前，立即贴底并恢复跟随
  state.follow.chat = true;
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
      // #8：普通文件攒批整批交给后端（send_files_batch）——后端先把
      // 整批落库上屏、未轮到的标「排队」，再逐个真传；图片组在队列
      // 中的位置语义不变：攒批在组位置前后各 flush 一次。
      let batch = [];
      const flushBatch = async () => {
        if (!batch.length) return;
        const paths = batch;
        batch = [];
        try {
          const fails = await invoke("send_files_batch", { paths });
          for (const msg of fails || []) showToast(msg);
        } catch (e) { showToast(`文件发送失败：${e}`); }
      };
      for (const f of queue) {
        if (groupOn && isImageExt(fileExt(f.name))) {
          if (groupSent) continue;
          groupSent = true;
          await flushBatch();
          const paths = queue.filter((q) => isImageExt(fileExt(q.name))).map((q) => q.path);
          try { await invoke("send_image_group", { paths }); }
          catch (e) { showToast(`图片组发送失败：${e}`); }
          continue;
        }
        batch.push(f.path);
      }
      await flushBatch();
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
    // ④ 发出即挂等待气泡：回答经事件回推落库前界面必须有动静，
    // 否则走对方转发时十几秒空白像卡死。路由决定等待文案。
    const waitRow = appendChatAiWaiting(src === "peer" || (src === "default" && aiRouteIsPeer()), { bindNext: true });
    try {
      await invoke("chat_ask_ai", { conversationId: state.peerConvId, source: src, text, thinking: currentThinking() });
    } catch (e) {
      showToast("问AI 失败：" + e);
    } finally { waitRow.remove(); delete state.aiWaitRows["messages"]; chatAiBusy = false; }
    return;
  }
  if (docBarTimer) { clearTimeout(docBarTimer); docBarTimer = 0; updateDocBarNow(); }
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
  if (e.isComposing || e.keyCode === 229) return; // 输入法组字回车不发送（与 AI 页对齐，⑦）
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
    // #2：超大内容不存草稿（每次输入都整篇搬运过 IPC 本身就是负担，
    // 且超限内容发送时一律转文件、草稿恢复也没有意义）
    if (!state.peerConvId || $("input").value.length > PASTE_FILE_THRESHOLD) return;
    invoke("save_draft", { conversationId: state.peerConvId, text: $("input").value }).catch(() => {});
  }, 400);
});

$("btn-file").addEventListener("click", async () => {
  // #17 防重入：选择器开着时连点不叠开第二个（叠开时后一个结果会
  // 把前一个吞掉，表现为「选了没入队」）。
  if (state.pickingFiles) return;
  state.pickingFiles = true;
  try {
    // 多选入待发队列（与粘贴/拖放同一条队列，点发送键统一逐个发）
    const picked = await window.__TAURI__.dialog.open({ multiple: true, directory: false });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    const added = await stageFilePaths(paths);
    // 选了却一个都没新增必须明示原因，不许静默（多半是已在队列里）
    if (paths.length && added === 0) showToast("所选文件已在待发队列中");
    $("input").focus();
  } catch (e) { showToast("选择文件失败：" + e); }
  finally { state.pickingFiles = false; }
});

$("btn-disconnect").addEventListener("click", async () => {
  // 连接进行中点断开要连带作废连接令牌（评审补强）：只调 disconnect
  // 时无会话可断、令牌不失效，进行中的尝试可能随后接管成已连接
  await invoke("cancel_connect").catch(() => {});
  await invoke("disconnect");
});
// 重连横幅的「停止重连」：走断开命令——无会话时后端同样会作废
// 重连代号并清目标（见 services disconnect 的无会话分支）。
$("reconnect-stop").addEventListener("click", async () => {
  await invoke("disconnect");
  $("reconnect-banner").classList.add("hidden");
});
$("btn-connected-disconnect").addEventListener("click", async () => { await invoke("disconnect"); });
$("btn-goto-chat").addEventListener("click", () => goChat());

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
    // #19 按名匹配优先（v2.6.5）：BLE 家族的设备 id 随重启/地址轮换
    // 变化，存下的旧 id 永远命不中——旧版因此必须手动重扫、在新鲜
    // 结果里点连接才行。命中以扫描快照里的新鲜 id 为准连接，并把
    // 新 id 回存记录，下次优先按新 id 找。
    const hit = devices.find((d) => d.id === pendingRemembered.deviceId)
      || (pendingRemembered.name ? devices.find((d) => d.name === pendingRemembered.name) : null);
    if (hit) {
      const p = pendingRemembered;
      pendingRemembered = null;
      clearTimeout(rememberedTimer);
      state.btDevice = hit.id;
      state.btDeviceName = hit.name;
      state.connectTransport = p.transport;
      $("last-peer-status").textContent = `已找到「${hit.name}」，正在连接…`;
      state.linkState = "connecting";
      syncOfflineUI();
      invoke("bt_connect", { deviceId: hit.id, code: p.code, transport: p.transport, name: hit.name || "", rssi: hit.rssi ?? null })
        .then(() => saveLastPeer({ transport: p.transport, deviceId: hit.id, code: p.code, name: hit.name }))
        .catch((e) => {
          if (!state.connected) { state.linkState = "off"; syncOfflineUI(); }
          $("last-peer-status").textContent = ""; $("connect-error").textContent = String(e);
        });
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
    const isLastPeer = !isConnectedPeer && state.lastPeer
      && (state.lastPeer.deviceId === d.id || (state.lastPeer.name && state.lastPeer.name === d.name));
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
    pendingRemembered = { deviceId: p.deviceId, name: p.name || "", code: p.code, transport: t };
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
  // #1：输码态标记——矮窗下右栏靠它再收起两处次要行才装得下整页
  $("connect-view").classList.add("code-open");
  $("bt-peer-code").focus();
}

/** 关闭输码卡：回到本机设备卡，清除选中高亮与错误。 */
function closeCodeCard() {
  $("bt-code-card").classList.add("hidden");
  $("connect-view").classList.remove("code-open");
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
  state.linkState = "connecting"; // #17：连接中顶栏出现断开（可取消）
  syncOfflineUI();
  try {
    await invoke("bt_connect", { deviceId: state.btDevice, code, transport: t, name: state.btDeviceName || "", rssi: (lastDevices.find((d) => d.id === state.btDevice) || {}).rssi ?? null });
    if (my !== connectAttempt) return; // 已取消：迟到的成功由后端自行断开
    saveLastPeer({ transport: t, deviceId: state.btDevice, code, name: state.btDeviceName || "对方设备" });
    if (scanning) { await invoke("bt_stop_scan").catch(() => {}); scanning = false; setScanningUI(); }
    setConnectingUI(false);
    closeCodeCard();
  } catch (e) {
    if (my !== connectAttempt) return; // 取消引发的失败不弹错（卡片已关）
    if (!state.connected) { state.linkState = "off"; syncOfflineUI(); }
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
    item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(it.note || msgSummary(m, 80))}</div>`;
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
    item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(msgSummary(m, 80))}</div>`;
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
    pop.style.top = (r.bottom + 6) + "px";
    pop.style.left = (r.left - 120) + "px";
  } else {
    pop.style.top = "80px";
    pop.style.left = "300px";
  }
  clampPopIntoView(pop); // v1.0.3 #2：实测钳位，替代写死猜高/猜宽
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
function openMsgMenu(m, x, y, ai = false) {
  if (m.kind === "file") { openFileMenu(m.id, x, y); return; }
  if (m.kind === "imagegroup") { openGroupMenu(m, x, y); return; }
  if (m.kind === "system") return;
  const menu = $("file-menu");
  menu.innerHTML = "";
  // v2.6.6 #13/#14：菜单按区域生成、文案随状态——AI 区引用走 AI 引用条
  // （原实现复用聊天引用、点了没反应且语义错位），不出现「问AI」；
  // 收藏/置顶文案按当前状态出（取消收藏/取消置顶），与气泡动作列同源。
  const favLabel = state.favIds.has(m.id) ? "取消收藏" : "收藏";
  const pinLabel = state.pinIds.has(m.id) ? "取消置顶" : "置顶";
  let items;
  if (ai) {
    items = [
      ["复制", () => copyText(m.body)],
      [favLabel, () => toggleMark(m.id, "favorite")],
      [pinLabel, () => toggleMark(m.id, "pin")],
      ["引用", () => showAiQuoteBar(m)],
    ];
    if (m.sender === "ai") items.push(["重新生成", () => regenerateAi(m)]);
    const rowEl = document.querySelector(`#ai-messages [data-mid="${m.id}"]`);
    if (rowEl && rowEl.querySelector(".msg-toggle")) {
      items.push([state.collapsedIds.has(m.id) ? "展开" : "收起", () => {
        const t = rowEl.querySelector(".msg-toggle");
        if (t) t.click();
      }]);
    }
  } else {
    items = [
      ["复制", () => copyText(m.body)],
      [favLabel, () => toggleMark(m.id, "favorite")],
      [pinLabel, () => toggleMark(m.id, "pin")],
      ["引用", () => setReply(m)],
      ["问AI", () => askAiAbout(m)],
    ];
  }
  for (const [label, fn] of items) {
    const b = document.createElement("button");
    b.textContent = label;
    b.addEventListener("click", async () => {
      menu.classList.add("hidden");
      try { await fn(); } catch (e) { showToast("操作失败：" + e); }
    });
    menu.appendChild(b);
  }
  placeMenuAt(menu, x, y);
}

// ---------- 文件菜单 ----------
async function openFileMenu(mid, x, y) {
  const menu = $("file-menu");
  let info = null;
  try { info = await invoke("file_info", { messageId: mid }); } catch (_) { /* 忽略 */ }
  const exists = !!(info && info.exists && info.path);
  menu.innerHTML = "";
  const items = [
    ["打开", exists, async () => {
      // #9：后端按扩展名分类——文本/脚本回 "viewer" 走内置查看器，
      // 可执行类后端只做文件夹定位、其余系统打开，都不会执行脚本。
      const r = await invoke("open_file", { path: info.path });
      if (r === "viewer") openTextViewer(info, mid);
    }, ""],
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
    // ㉖ 补齐：文件气泡右键菜单此前没有收藏/置顶，与文本气泡口径对齐
    // v2.6.6 #14：收藏文案随状态（取消收藏），与其它菜单同源
    [state.favIds.has(mid) ? "取消收藏" : "收藏", true, async () => { await toggleMark(mid, "favorite"); }, ""],
    [state.pinIds.has(mid) ? "取消置顶" : "置顶", true, async () => { await toggleMark(mid, "pin"); }, ""],
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
  placeMenuAt(menu, x, y);
}
document.addEventListener("click", (e) => {
  const menu = $("file-menu");
  if (menu && !menu.classList.contains("hidden") && !menu.contains(e.target)) menu.classList.add("hidden");
  const fmtMenu = $("viewer-fmt-menu");
  if (fmtMenu && !fmtMenu.classList.contains("hidden") && !fmtMenu.contains(e.target)) fmtMenu.classList.add("hidden");
  const srcMenu = $("ai-source-menu");
  if (srcMenu && !srcMenu.classList.contains("hidden") && !srcMenu.contains(e.target)) srcMenu.classList.add("hidden");
  const clipModeMenu = $("clip-mode-menu");
  if (clipModeMenu && !clipModeMenu.classList.contains("hidden") && !clipModeMenu.contains(e.target)) clipModeMenu.classList.add("hidden");
  const clipCountMenu = $("clip-count-menu");
  if (clipCountMenu && !clipCountMenu.classList.contains("hidden") && !clipCountMenu.contains(e.target)) clipCountMenu.classList.add("hidden");
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
    d.innerHTML = `<div class="rl-note">${esc(it.note || msgSummary(it.message, 60))}</div><div class="rl-sub">${fmtDay(it.message.created_at)} ${fmtTime(it.message.created_at)}${it.note ? " · " + esc(msgSummary(it.message, 40)) : ""}</div>`;
    // 条目只显示一行（CSS 省略号）；悬停条目时用提示显示完整备注（无备注则显示摘要——图片组等内部载荷不许全文上提示，#11）
    d.title = it.note || msgSummary(it.message, 200);
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
  clampPopIntoView(box); // v1.0.3 #2：实测钳位防探底（高度另有 46vh 上限）
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
    pop.style.top = (r.bottom + 6) + "px";
    pop.style.left = (r.left - 60) + "px";
  }
  clampPopIntoView(pop); // v1.0.3 #2：实测钳位，替代写死猜高/猜宽
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
      item.innerHTML = `<div class="li-sub">${fmtDay(m.created_at)} ${fmtTime(m.created_at)}</div><div>${esc(msgSummary(m, 80))}</div>`;
      item.addEventListener("click", () => locateMessage(m.conversation_id, m.id));
      box.appendChild(item);
    }
  }, 300);
});

// ---------- AI ----------
// ---------- ㉕ AI 会话体系：列表只属于 AI 页 ----------
// AI 会话与聊天（设备）会话是两回事：AI 页自带会话条，可新建、切换、
// 双击改名；发送跟随当前打开的会话（state.aiConvId）。ai-default
// 「AI 助手」是默认会话，问AI 的落点也固定在它。
let aiConvLoading = false;
async function loadAiConversations() {
  if (aiConvLoading) return;
  aiConvLoading = true;
  try {
    await loadAiConversationsInner();
  } finally {
    aiConvLoading = false;
  }
}
async function loadAiConversationsInner() {
  let convs;
  try {
    convs = await invoke("list_conversations");
  } catch (_) { return; }
  let aiConvs = convs.filter((c) => c.kind === "ai");
  // 「AI 助手」（ai-default）必须常驻会话条：它原先只在首次发 AI 消息
  // 时才落库，此前不显胶囊、删除回落也会被下面的有效性检查顶掉。
  // 缺行时补建（后端 INSERT OR IGNORE 幂等）后重拉一次。
  if (!aiConvs.some((c) => c.id === "ai-default")) {
    try {
      await invoke("ensure_ai_conversation");
      const again = await invoke("list_conversations");
      const refiltered = again.filter((c) => c.kind === "ai");
      aiConvs.length = 0;
      aiConvs.push(...refiltered);
    } catch (_) { /* 补建失败不挡列表渲染 */ }
  }
  // #3 胶囊顺序持久化：localStorage 存 id 顺序，拖拽排序后写回；
  // 新会话排在末尾（稳定排序，不打乱既有顺序）。
  let order = [];
  try { order = JSON.parse(localStorage.getItem("bl-ai-order") || "[]"); } catch (_) { order = []; }
  const rank = (id) => {
    const i = order.indexOf(id);
    return i < 0 ? Number.MAX_SAFE_INTEGER : i;
  };
  aiConvs.sort((a, b) => rank(a.id) - rank(b.id));
  // v2.6.6 #10②：「AI 助手」强制永远排第 1——存储顺序里就算被写到
  // 别处，恢复时也先把它摘出来钉回首位，其余按存储顺序。
  const di = aiConvs.findIndex((c) => c.id === "ai-default");
  if (di > 0) { const [d] = aiConvs.splice(di, 1); aiConvs.unshift(d); }
  if (!aiConvs.some((c) => c.id === state.aiConvId)) {
    state.aiConvId = aiConvs.length ? aiConvs[0].id : "ai-default";
  }
  const list = $("ai-conv-list");
  list.innerHTML = "";
  for (const c of aiConvs) {
    const chip = document.createElement("button");
    chip.className = "ai-conv-chip" + (c.id === state.aiConvId ? " active" : "");
    chip.textContent = c.title || "AI 会话";
    // v2.6.6 #10②：默认会话「AI 助手」全套保护——不可改名/删除/移动、
    // 永远第 1、唯一放行清空记录；禁用操作被触发时给 toast 不静默。
    const isDefault = c.id === "ai-default";
    chip.title = isDefault
      ? "「AI 助手」是默认会话，不能改名、删除或移动；右键可清空聊天记录"
      : "点击打开；右键改名/删除；拖拽排序";
    chip.dataset.cid = c.id;
    // 已打开的会话点它不做任何事：重渲染会把紧接着的双击（改名）打断
    chip.addEventListener("click", () => {
      if (chip._dragged) { chip._dragged = false; return; }
      if (c.id !== state.aiConvId) openAiConversation(c.id);
    });
    chip.addEventListener("dblclick", (ev) => {
      ev.stopPropagation();
      if (isDefault) { showToast("「AI 助手」是默认会话，不能改名"); return; }
      renameAiChip(chip, c);
    });
    // #3 右键菜单：明确有「重命名」「删除」，不再只靠双击的隐形入口
    chip.addEventListener("contextmenu", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      openAiChipMenu(chip, c, ev.clientX, ev.clientY);
    });
    // #3 拖拽排序：按下移动超过 6px 才算拖拽（不干扰点击/双击）
    // v2.6.6 #10②：默认会话本身不可拖（拖动尝试给 toast、不静默）；
    // 拖别人时落点钳制在它之后——越不过第 1 位（尝试越位同样提示）。
    chip.addEventListener("pointerdown", (ev) => {
      if (ev.button !== 0) return;
      const startX = ev.clientX;
      let dragging = false;
      let hintShown = false;
      const move = (mv) => {
        if (isDefault) {
          if (!hintShown && Math.abs(mv.clientX - startX) > 6) {
            hintShown = true;
            showToast("「AI 助手」是默认会话，不能移动");
          }
          return;
        }
        if (!dragging && Math.abs(mv.clientX - startX) > 6) {
          dragging = true;
          chip.classList.add("dragging");
        }
        if (!dragging) return;
        const siblings = [...list.querySelectorAll(".ai-conv-chip")];
        const target = siblings.find((el) => el !== chip
          && mv.clientX >= el.getBoundingClientRect().left
          && mv.clientX <= el.getBoundingClientRect().right);
        if (target) {
          const rect = target.getBoundingClientRect();
          if (target.dataset.cid === "ai-default") {
            // 只能落到默认会话之后；往它前面挤时提示一次
            if (mv.clientX < rect.left + rect.width / 2 && !hintShown) {
              hintShown = true;
              showToast("「AI 助手」固定在第一位，不能移到它前面");
            }
            list.insertBefore(chip, target.nextSibling);
          }
          else if (mv.clientX < rect.left + rect.width / 2) list.insertBefore(chip, target);
          else list.insertBefore(chip, target.nextSibling);
        }
      };
      const up = () => {
        document.removeEventListener("pointermove", move);
        document.removeEventListener("pointerup", up);
        if (dragging) {
          chip.classList.remove("dragging");
          chip._dragged = true;
          setTimeout(() => { chip._dragged = false; }, 50);
          const ids = [...list.querySelectorAll(".ai-conv-chip")].map((el) => el.dataset.cid);
          localStorage.setItem("bl-ai-order", JSON.stringify(ids));
        }
      };
      document.addEventListener("pointermove", move);
      document.addEventListener("pointerup", up);
    });
    list.appendChild(chip);
    if (c.id === state.aiConvId) $("ai-title").textContent = c.title || "AI 助手";
  }
}

/** #3 AI 会话胶囊右键菜单（与文件菜单共用 #file-menu 容器）：
 * 重命名（走既有 renameAiChip，菜单关闭不重渲染、输入框不被打断）、
 * 删除（两步确认；默认会话「AI 助手」不可删，菜单里直接不给）。
 * v2.6.6 #10②：默认会话菜单只剩唯一放行的「清空聊天记录」（两步
 * 确认）；普通会话同样给清空入口（会话保留、只清消息）。 */
function openAiChipMenu(chip, conv, x, y) {
  const menu = $("file-menu");
  menu.innerHTML = "";
  const isDefault = conv.id === "ai-default";
  const entries = [];
  if (!isDefault) entries.push(["重命名", () => renameAiChip(chip, conv), ""]);
  entries.push(["清空聊天记录", null, ""]);
  if (!isDefault) {
    entries.push(["删除会话", null, "fm-danger"]);
  }
  for (const [label, fn, cls] of entries) {
    const b = document.createElement("button");
    b.textContent = label;
    if (cls) b.className = cls;
    b.addEventListener("click", async () => {
      if (label === "清空聊天记录") {
        if (state.aiGenerating) {
          showToast("AI 正在生成中，请稍候");
          return;
        }
        // v1.0.2 ④：与设置页范围清空复用同一对话框（会话模式 + 同一
        // 「同时删除相关文件」勾选），口径一致；对话框 + 通用二次确认
        // 本就是两步确认，不再另做菜单内两步武装。
        menu.classList.add("hidden");
        openClearModal({ kind: "conversation", convId: conv.id, title: conv.title || "AI 会话" });
        return;
      }
      if (label === "删除会话") {
        // 两步确认：第一下把按钮变成确认文案，第二下才真删
        if (!b.dataset.armed) {
          b.dataset.armed = "1";
          b.textContent = "确认删除？（含全部消息）";
          return;
        }
        menu.classList.add("hidden");
        try {
          await invoke("delete_ai_conversation", { conversationId: conv.id });
          if (state.aiConvId === conv.id) {
            state.aiConvId = "ai-default";
            await loadAiMessages();
          }
          await loadAiConversations();
          showToast("会话已删除");
        } catch (e) { showToast("删除失败：" + e); }
        return;
      }
      menu.classList.add("hidden");
      try { await fn(); } catch (e) { showToast("操作失败：" + e); }
    });
    menu.appendChild(b);
  }
  placeMenuAt(menu, x, y);
}

function openAiConversation(id) {
  if (state.aiGenerating) { showToast("AI 正在生成中，请稍候"); return; }
  state.aiConvId = id;
  loadAiConversations();
  loadAiMessages();
  refreshPinBar();
}

function renameAiChip(chip, conv) {
  // v2.6.6 #10② 纵深防御：默认会话任何路径都不进改名（后端亦有守卫）
  if (conv.id === "ai-default") { showToast("「AI 助手」是默认会话，不能改名"); return; }
  const input = document.createElement("input");
  input.value = conv.title || "";
  chip.textContent = "";
  chip.appendChild(input);
  input.focus();
  input.select();
  let done = false;
  const commit = async (save) => {
    if (done) return;
    done = true;
    const title = input.value.trim();
    if (save && title && title !== conv.title) {
      try {
        await invoke("rename_ai_conversation", { conversationId: conv.id, title });
      } catch (e) { showToast("改名失败：" + e); }
    }
    loadAiConversations();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") commit(true);
    else if (e.key === "Escape") commit(false);
    e.stopPropagation();
  });
  input.addEventListener("blur", () => commit(true));
  input.addEventListener("click", (e) => e.stopPropagation());
  input.addEventListener("dblclick", (e) => e.stopPropagation());
}

// v2.6.6 #6：AI 会话条滚轮横滚——纵向滚轮转横向滚动，溢出胶囊可达
$("ai-conv-list").addEventListener("wheel", (e) => {
  const el = $("ai-conv-list");
  if (el.scrollWidth <= el.clientWidth) return;
  const d = Math.abs(e.deltaY) >= Math.abs(e.deltaX) ? e.deltaY : e.deltaX;
  if (!d) return;
  e.preventDefault();
  el.scrollLeft += d;
}, { passive: false });

$("btn-ai-new-conv").addEventListener("click", async () => {
  if (state.aiGenerating) { showToast("AI 正在生成中，请稍候"); return; }
  try {
    const conv = await invoke("create_ai_conversation", { title: null });
    state.aiConvId = conv.id;
    await loadAiConversations();
    await loadAiMessages();
    refreshPinBar();
    $("ai-input").focus();
  } catch (e) { showToast("新建会话失败：" + e); }
});

async function loadAiMessages() {
  if (state.locateInProgress) return; // ㉔：定位链路自己管加载，不并发重铺
  await refreshFavIds();
  await refreshPinIds();
  await refreshAiAttMap();
  const msgs = await invoke("list_messages", { conversationId: state.aiConvId, limit: 200 });
  const container = $("ai-messages");
  container.innerHTML = "";
  state.renderedAi.clear();
  for (const m of msgs) appendMessage(container, m, state.renderedAi, { ai: true });
  updateAiState();
}

// ---- AI 等待态 / 跟随滚动 / 思考开关（④⑤⑥，v2.6.1）----
// ④ 此前发出问题后界面零反馈：问题落库要等回答事件才显形、首片到达
// 前流式气泡根本不存在，走对方转发时十几秒没动静像卡死。现在三入口
// 发出即显等待气泡，首片到达原地转流式渲染。
// ⑥ 流式跟随只在距底 ≤80px 时发生；用户上滑看历史不再被抢滚动，
// AI 页右下角给「回到最新」按钮（聊天页已有同类按钮 btn-bottom）。
function nearBottomEl(el, px = 80) {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= px;
}
/** 滚动定版的手势侧（v2.6.2）：手势一出立即停跟随；自滚回底
 * （距底 ≤4px）恢复跟随。程序滚动不经过手势事件，不会误停。 */
function watchScrollDiscipline(el, key) {
  if (!el) return;
  const off = () => { state.follow[key] = false; };
  el.addEventListener("wheel", off, { passive: true });
  el.addEventListener("touchmove", off, { passive: true });
  el.addEventListener("pointerdown", (e) => {
    // 拖滚动条：指针落在容器右缘滚动条区域（clientWidth 之外）
    const r = el.getBoundingClientRect();
    if (e.clientX >= r.left + el.clientWidth) off();
  });
  el.addEventListener("keydown", (e) => {
    if (["PageUp", "PageDown", "ArrowUp", "ArrowDown", "Home", "End", " "].includes(e.key)) off();
  });
  el.addEventListener("scroll", () => {
    if (!state.follow[key] && nearBottomEl(el, 4)) state.follow[key] = true;
  });
}
watchScrollDiscipline($("messages"), "chat");
watchScrollDiscipline($("ai-messages"), "ai");
/** 窗口恢复/重新获焦时把跟随中的容器重贴底（v1.0.4 验后 #7）：后台
 * 收到消息时追加只做一次同步贴底，可窗口隐藏/最小化期间 WebView2
 * 挂起布局、scrollHeight 停在旧值，图片等元素恢复后才定型变高，
 * 底就贴不住、看着像没跟随。恢复时按即时+rAF+180ms 三拍补贴；
 * 跟随已关（用户手势停过）绝不贴——滚动定版不破。容器内图片迟
 * 加载定型同理：load 后跟随开着就补一拍。 */
function resnapIfFollowing() {
  for (const [c, key] of [[$("messages"), "chat"], [$("ai-messages"), "ai"]]) {
    if (!c || c.offsetParent === null || !state.follow[key]) continue;
    const snap = () => { if (state.follow[key]) c.scrollTop = c.scrollHeight; };
    snap();
    requestAnimationFrame(snap);
    setTimeout(snap, 180);
  }
}
window.addEventListener("focus", () => resnapIfFollowing());
listen("window-restored", () => resnapIfFollowing()).catch(() => {});
document.addEventListener("visibilitychange", () => { if (!document.hidden) resnapIfFollowing(); });
for (const [c, key] of [[$("messages"), "chat"], [$("ai-messages"), "ai"]]) {
  if (!c) continue;
  c.addEventListener("load", (e) => {
    if (e.target && e.target.tagName === "IMG" && state.follow[key]) {
      requestAnimationFrame(() => { if (state.follow[key]) c.scrollTop = c.scrollHeight; });
    }
  }, true);
}
function syncAiBottomBtn() {
  const c = $("ai-messages");
  $("btn-ai-bottom").classList.toggle("hidden", nearBottomEl(c, 80));
}
// 默认 AI 这次会走哪条路（与 updateAiState 的口径一致）：local 本机；
// peer 对方；auto 本机配好走本机、否则走对方。等待文案据此区分。
function aiRouteIsPeer() {
  if (state.aiSource === "local") return false;
  if (state.aiSource === "peer") return true;
  return !state.aiLocalReady;
}
function aiWaitingText(peer) {
  return peer ? "正在等待对方 AI 回复…" : "正在等待 AI 回复…";
}
function showAiWaiting(opts = {}) {
  clearAiWaiting();
  const container = $("ai-messages");
  const row = document.createElement("div");
  row.className = "msg-row";
  row.id = "ai-waiting-row";
  row.innerHTML = `<div class="msg"><div class="bubble ai-waiting"><span class="wait-dots"><i></i><i></i><i></i></span>${aiWaitingText(aiRouteIsPeer())}</div></div>`;
  // 锚定纪律（#1，v2.6.6 修正）：新提问必须走 bindNext——问题行还
  // 没落库渲染，先挂末尾不绑任何旧消息，问题行到达时 appendMessage
  // 把等待行绑到它正下方。只有重新生成（问题已在列表里）才走默认
  // 分支锚到最后一条问题之后。旧版新提问也走默认分支：等待行被插
  // 到「上一问」之后、锚点错绑上一问 id，新问题到达时归位永不
  // 触发——等待与流式气泡（同一行原地转的）全程卡在上一条 AI
  // 回复上方，落库重载才恢复（v2.6.5 验后 #1 真机实锤）。
  placeAiWaitingRow(container, row, opts);
  container.scrollTop = container.scrollHeight;
  syncAiBottomBtn();
  state.streamEl = row.querySelector(".bubble");
  state.streamText = "";
  state.streamWaiting = true;
}
function clearAiWaiting() {
  const r = $("ai-waiting-row");
  if (r) r.remove();
  delete state.aiWaitRows["ai-messages"];
  if (state.streamWaiting) {
    state.streamEl = null;
    state.streamText = "";
    state.streamWaiting = false;
  }
}
// ⑤ 思考模式开关：AI 页开关是唯一状态源（localStorage 持久化），
// @AI/问AI/重新生成都读同一个值随请求带上。后端仅对 DeepSeek 兼容
// 接口真正下发 thinking 参数，其余接口忽略并回执提示（见
// ai_thinking_note 事件）。
function currentThinking() {
  // 只在勾选时携带 true：未勾选传 null（不干预）——否则不支持思考
  // 开关的接口每次回答都会误报「已忽略该设置」（自测抓到后修）
  const el = $("ai-thinking");
  return el && el.checked ? true : null;
}

/** #5 自动命名标题（v2.6.5 定案）：首条用户消息截短——上限 10 字、
 * 宁短勿长；优先在标点/空白处断开（断点须落在第 4 字之后，太靠前的
 * 断点丢信息不如满额截断）；纯附件无文字返回 null（保留「新会话」）。 */
function autoNameFromText(text) {
  const t = (text || "").replace(/\s+/g, " ").trim();
  if (!t) return null;
  if (t.length <= 10) return t;
  const head = t.slice(0, 10);
  const marks = ["，", "。", "、", "；", "：", "！", "？", ",", ".", " ", "…", "—"];
  let cut = -1;
  for (const mk of marks) cut = Math.max(cut, head.lastIndexOf(mk));
  return cut >= 4 ? head.slice(0, cut) : head;
}

/** #5 自动命名：只对仍叫「新会话」的会话命名一次（命名后标题不再是
 * 「新会话」，天然只发生一次）；用户手改过的标题永不覆盖；「AI 助手」
 * （ai-default）不参与。失败静默——命名是锦上添花，不打扰提问。 */
async function maybeAutoNameAiConversation(text) {
  const cid = state.aiConvId;
  if (!cid || cid === "ai-default") return;
  const name = autoNameFromText(text);
  if (!name) return;
  try {
    const convs = await invoke("list_conversations");
    const conv = (convs || []).find((c) => c.id === cid && c.kind === "ai");
    if (!conv || conv.title !== "新会话") return;
    await invoke("rename_ai_conversation", { conversationId: cid, title: name });
    loadAiConversations();
  } catch (_) { /* 静默 */ }
}

// ---------- AI 文本附件（v1.0.1） ----------
// 只收文本类文件：类型判定与查看器共用 TEXT_EXTS；图片/二进制明确
// 拒绝（文本模型看不了图，不假装能收）。上限：单件 512 KB、合计
// 512 KB、最多 5 个——超限 toast 拒绝、不静默截断（后端还有第二道闸）。
const AI_ATT_MAX_SINGLE = 512 * 1024;
const AI_ATT_MAX_TOTAL = 512 * 1024;
const AI_ATT_MAX_COUNT = 5;

function renderAiAttachBar() {
  const bar = $("ai-attach-bar");
  const list = $("ai-attach-list");
  list.innerHTML = "";
  for (const f of state.aiPendingAtt) {
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
      state.aiPendingAtt = state.aiPendingAtt.filter((q) => q.path !== f.path);
      renderAiAttachBar();
    });
    chip.appendChild(label);
    chip.appendChild(x);
    list.appendChild(chip);
  }
  $("ai-attach-count").textContent = state.aiPendingAtt.length
    ? `待发送 ${state.aiPendingAtt.length} 个附件（随问题一并发给 AI）` : "";
  bar.classList.toggle("hidden", state.aiPendingAtt.length === 0);
}
$("ai-attach-clear").addEventListener("click", () => {
  state.aiPendingAtt = [];
  renderAiAttachBar();
});

/** 把一批路径逐个校验入 AI 附件队列：类型不对/超限的逐个 toast 说明
 * 并跳过，不连累其余。返回实际入队数量。 */
async function stageAiFilePaths(paths) {
  let added = 0;
  for (const path of paths || []) {
    if (!path || state.aiPendingAtt.some((q) => q.path === path)) continue;
    const base = String(path).split(/[\\/]/).pop() || path;
    let info;
    try { info = await invoke("file_attach_info", { path }); }
    catch (err) { showToast(`无法添加「${base}」：${err}`); continue; }
    if (!TEXT_EXTS.has(fileExt(info.name))) {
      showToast(`「${info.name}」不是文本文件——AI 附件只支持文本文件`);
      continue;
    }
    if (info.size > AI_ATT_MAX_SINGLE) {
      showToast(`「${info.name}」超过 512 KB 单件上限，未添加`);
      continue;
    }
    if (state.aiPendingAtt.length >= AI_ATT_MAX_COUNT) {
      showToast(`附件最多 ${AI_ATT_MAX_COUNT} 个，「${info.name}」未添加`);
      continue;
    }
    const total = state.aiPendingAtt.reduce((s, q) => s + q.size, 0) + info.size;
    if (total > AI_ATT_MAX_TOTAL) {
      showToast(`附件总大小将超过 512 KB 上限，「${info.name}」未添加`);
      continue;
    }
    try {
      const data = await invoke("read_text_file", { path });
      const dec = decodeTextBytes(base64ToBytes(data.base64));
      const content = dec.text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
      state.aiPendingAtt.push({ path, name: info.name, size: info.size, content });
      added++;
    } catch (err) { showToast(`读取「${info.name}」失败：${err}`); }
  }
  if (added) renderAiAttachBar();
  return added;
}

$("btn-ai-file").addEventListener("click", async () => {
  if (state.pickingAiFiles) return;
  state.pickingAiFiles = true;
  try {
    const picked = await window.__TAURI__.dialog.open({ multiple: true, directory: false });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    const added = await stageAiFilePaths(paths);
    if (paths.length && added === 0 && !state.aiPendingAtt.length) {
      /* 逐个的拒绝原因已在 stageAiFilePaths 里 toast 过，这里不叠提示 */
    }
    $("ai-input").focus();
  } catch (e) { showToast("选择文件失败：" + e); }
  finally { state.pickingAiFiles = false; }
});

/** 刷新当前 AI 会话的附件清单（渲染文件片用）。 */
async function refreshAiAttMap() {
  try {
    state.aiAttMap = await invoke("ai_attachments_map", { conversationId: state.aiConvId });
  } catch (_) { state.aiAttMap = {}; }
}

/** 给一条 AI 用户消息的气泡挂附件文件片（名+大小，点击开查看器）。 */
function fillAiAttChips(rowEl, m) {
  const atts = state.aiAttMap[m.id];
  if (!atts || !atts.length) return;
  const bubble = rowEl.querySelector(".bubble");
  if (!bubble || bubble.querySelector(".ai-att-list")) return;
  const list = document.createElement("div");
  list.className = "ai-att-list";
  for (const a of atts) {
    const chip = document.createElement("span");
    chip.className = "attach-chip ai-att-chip";
    chip.title = "点击查看附件内容";
    const label = document.createElement("span");
    label.className = "attach-name";
    label.textContent = `${a.name} · ${fmtSize(a.size)}`;
    chip.appendChild(label);
    chip.addEventListener("click", () => openTextViewer({ name: a.name, path: a.path }, null));
    list.appendChild(chip);
  }
  bubble.appendChild(list);
}

async function sendAi() {
  const text = $("ai-input").value.trim();
  const ref = state.pendingAskRef;
  const atts = state.aiPendingAtt;
  if (state.aiGenerating) return;
  if (!text && !ref && !atts.length) return;
  state.follow.ai = true;
  if (!(await aiReadyOrRemind())) return;
  $("ai-input").value = ""; autoGrow($("ai-input"));
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  showAiWaiting({ bindNext: true }); // 新提问：锚点绑随后到达的问题行（#1）
  try {
    if (ref) {
      // ㉒ 引用模式：引用消息作上下文 + 用户文字作问题一起发出；
      // 没输文字则 extra 为空，后端按引用原文问（兜底）。
      // v1.0.1：带附件时把附件内容按同一包裹格式拼进 extra 随问题走
      // （内容确实送达模型；但引用路径的落库正文本就是组装全文，
      // 文件片不单独登记——附件落库/文件片只在普通发送路径，见报告）
      let extra = text;
      if (atts.length) {
        const blocks = atts.map((a) => `附件：${a.name}\n\`\`\`\n${a.content}\n\`\`\``).join("\n\n");
        extra = extra ? `${extra}\n\n${blocks}` : blocks;
      }
      await invoke("ai_ask_message", {
        conversationId: ref.convId, messageId: ref.msgId,
        thinking: currentThinking(), extra: extra || null,
      });
      if (atts.length) {
        state.aiPendingAtt = [];
        renderAiAttachBar();
      }
      clearAskRef();
    } else {
      await invoke("ai_send", {
        text, thinking: currentThinking(), conversationId: state.aiConvId,
        attachments: atts.map((a) => ({ name: a.name, content: a.content })),
      });
      if (atts.length) {
        state.aiPendingAtt = [];
        renderAiAttachBar();
      }
      // #5：首条消息发出后给「新会话」自动命名（截短规则见上）；
      // 纯附件无文字时用首个附件名兜底
      maybeAutoNameAiConversation(text || (atts.length ? atts[0].name : ""));
    }
  } catch (e) {
    showToast("AI 提问失败：" + e);
  } finally {
    state.aiGenerating = false;
    clearAiWaiting();
    state.streamEl = null;
    state.streamText = "";
    $("btn-ai-stop").classList.add("hidden");
    if (ref) loadAiMessages();
    else if (atts.length) { await refreshAiAttMap(); loadAiMessages(); }
  }
}
async function regenerateAi() {
  if (state.aiGenerating) return;
  state.aiGenerating = true;
  $("btn-ai-stop").classList.remove("hidden");
  showAiWaiting(); // 重新生成：问题已在列表，默认分支锚到它之后（#1）
  try {
    await invoke("ai_regenerate", { thinking: currentThinking(), conversationId: state.aiConvId });
    await loadAiMessages();
  } catch (e) { showToast("重新生成失败：" + e); }
  state.aiGenerating = false;
  clearAiWaiting();
  state.streamEl = null;
  state.streamText = "";
  $("btn-ai-stop").classList.add("hidden");
}
// 聊天 @AI 回答的重新生成（⑭）见下方 chatRegenerateAi（与 chatAiBusy 同处）。
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
    paintAiSource(src);
    const peerLabel = state.peerName || "对方设备";
    const aiStateText =
      src === "local"
        ? ready ? `默认AI：本机AI（${cfg.model}）` : "默认AI：本机AI（尚未配置）"
        : src === "peer"
          ? state.connected ? `默认AI：${peerLabel} 的 AI（经对方转发）` : "默认AI：对方设备 AI（当前未连接设备）"
          : ready
            ? "默认AI：自动（使用本机配置的 AI 接口）"
            : state.connected
              ? `默认AI：自动（本机未配置 · 经${peerLabel}转发回答）`
              : "默认AI：自动（本机未配置 AI，也未连接设备）";
    // ㉚ 顶栏单行化：状态小字与标题同行、超长截断，悬停看全文
    $("ai-state").textContent = aiStateText;
    $("ai-state").title = aiStateText;
  } catch (_) { /* 忽略 */ }
}
// ---------- 默认AI 来源自绘下拉（v1.0.2 ③）：与查看器「格式化 ▾」
// 同款实现（按钮 + 应用内菜单，不用原生 select——展开列表由 Windows
// 系统绘制、样式管不到，用户实报丑）。选项与旧 select 一致：自动 /
// 本机AI / 对方设备AI；当前项菜单里打 ✓。
const AI_SOURCE_LABEL = { auto: "自动", local: "本机AI", peer: "对方设备AI" };
function paintAiSource(src) {
  const btn = $("ai-source-btn");
  if (btn) btn.textContent = `${AI_SOURCE_LABEL[src] || AI_SOURCE_LABEL.auto} ▾`;
}
function closeAiSourceMenu() { $("ai-source-menu").classList.add("hidden"); }
function openAiSourceMenu() {
  const menu = $("ai-source-menu");
  menu.innerHTML = "";
  for (const [v, label] of Object.entries(AI_SOURCE_LABEL)) {
    const b = document.createElement("button");
    b.textContent = (state.aiSource === v ? "✓ " : "") + label;
    b.addEventListener("click", async () => {
      closeAiSourceMenu();
      $("ai-source-btn").focus(); // 选完焦点还给按钮（第三方评审）
      await invoke("set_ai_source", { source: v }).catch(() => {});
      updateAiState();
    });
    menu.appendChild(b);
  }
  // 键盘导航（第三方评审：原生 select 有的方向键/Esc，自绘菜单不能丢）：
  // Esc 关闭并回按钮，上下键在选项间移动。onkeydown 直接赋值，避免
  // 每次打开重复挂监听。
  menu.onkeydown = (e) => {
    const items = [...menu.querySelectorAll("button")];
    const i = items.indexOf(document.activeElement);
    if (e.key === "Escape") {
      e.preventDefault();
      closeAiSourceMenu();
      $("ai-source-btn").focus();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      (items[i + 1] || items[0]).focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      (items[i - 1] || items[items.length - 1]).focus();
    }
  };
  menu.classList.remove("hidden");
  // 打开时焦点落在当前项，键盘用户不用先 Tab 进来。
  const cur = [...menu.querySelectorAll("button")].find((b) => b.textContent.startsWith("✓ "));
  (cur || menu.querySelector("button")).focus();
}
$("ai-source-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  if ($("ai-source-menu").classList.contains("hidden")) openAiSourceMenu();
  else closeAiSourceMenu();
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

// #3 等待行落位（双容器共用）：问题已在列表里（重新生成）时直接插到
// 最后一条「我」的消息之后；问题还没落库渲染（新提问）时先挂末尾，
// 登记后由 appendMessage 在问题行到达时锚定到其正下方。等待行打
// data-waiting 标记：日期分隔/分组计算必须跳过它（它没有时间戳）。
function placeAiWaitingRow(container, row, opts = {}) {
  row.dataset.waiting = "1";
  const rows = [...container.querySelectorAll(".msg-row")].filter((r) => !r.dataset.waiting);
  if (opts.beforeMid) {
    // 重新生成：等待行就插在旧回答之前（即问题之后），锚点绑问题 id
    const arow = rows.find((r) => r.dataset.mid === opts.beforeMid);
    if (arow) {
      arow.before(row);
      const prevMe = [...rows.slice(0, rows.indexOf(arow))].reverse()
        .find((r) => r.dataset.sender === "me");
      row.dataset.anchorId = prevMe ? (prevMe.dataset.mid || "") : "";
      state.aiWaitRows[container.id] = row;
      return row;
    }
  }
  if (opts.bindNext) {
    // 新提问（聊天 @AI）：问题还没落库，先挂末尾、不绑旧消息；
    // appendMessage 会把锚点绑到随后到达的提问本身
    container.appendChild(row);
    row.dataset.anchorId = "";
    state.aiWaitRows[container.id] = row;
    return row;
  }
  const lastMe = [...rows].reverse().find((r) => r.dataset.sender === "me");
  if (lastMe) {
    lastMe.after(row);
    // AI 页重新生成/等待：锚点当场绑这条问题（按 id）。AI 页生成期间
    // 发送被 aiGenerating 锁住，不会有别的「我」的消息来抢锚。
    row.dataset.anchorId = lastMe.dataset.mid || "";
  } else {
    container.appendChild(row);
    row.dataset.anchorId = "";
  }
  state.aiWaitRows[container.id] = row;
  return row;
}

// 在聊天列表挂一行 AI 等待气泡，返回该行元素（用完 remove）。
// 聊天 @AI 与聊天重新生成共用（④）。
function appendChatAiWaiting(peer, opts = {}) {
  const row = document.createElement("div");
  row.className = "msg-row";
  row.dataset.sender = "ai";
  row.innerHTML = `<div class="msg"><div class="bubble ai-waiting"><span class="wait-dots"><i></i><i></i><i></i></span>${aiWaitingText(peer)}</div></div>`;
  const c = $("messages");
  placeAiWaitingRow(c, row, opts);
  if (state.follow.chat) c.scrollTop = c.scrollHeight;
  return row;
}

// 聊天 @AI 回答的重新生成（⑭）：按回答消息 ID 让后端找回原问题重答；
// 等待行先挂上，新答落库后经 message 事件替换旧气泡（后端先生成后删旧，
// 这里成功后直接整页重载当前会话最稳妥）。
async function chatRegenerateAi(m) {
  if (chatAiBusy) { showToast("AI 正在回答上一个问题，请稍候"); return; }
  chatAiBusy = true;
  const waitRow = appendChatAiWaiting(aiRouteIsPeer(), { beforeMid: m.id });
  try {
    await invoke("chat_regenerate_ai", { messageId: m.id, thinking: currentThinking() });
    if (state.peerConvId) await loadChatMessages();
  } catch (e) {
    showToast("重新生成失败：" + e);
  } finally {
    waitRow.remove();
    delete state.aiWaitRows["messages"];
    chatAiBusy = false;
  }
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
// ㉒ 问AI：点完不直接发——切到 AI 页把消息挂为引用上下文条，
// 用户在输入框补自己的问题后发送才真正发出；不输文字直接发送 =
// 按引用原文问（旧行为兜底）。× 可取消引用，取消后发送不带上下文。
/** 把消息挂为 AI 输入框上方的引用上下文条（问AI 与 AI 区右键「引用」共用）。 */
function showAiQuoteBar(m) {
  // 与 askAiAbout 同口径（评审补强）：生成中不挂引用条，免得两个
  // 入口保护不一致、生成结束后误以为引用已随上一问发出
  if (state.aiGenerating) { showToast("AI 正在生成中，请稍候"); return; }
  state.pendingAskRef = { convId: m.conversation_id, msgId: m.id };
  const who = m.sender === "me" ? "我" : m.sender === "ai" ? "AI" : "对方";
  $("ai-quote-text").textContent = `引用${who}的消息：${msgSummary(m, 80)}`;
  $("ai-quote-bar").classList.remove("hidden");
  $("ai-input").focus();
}
function askAiAbout(m) {
  if (state.aiGenerating) { showToast("AI 正在生成中，请稍候"); return; }
  aiReadyOrRemind().then((ok) => {
    if (!ok) return;
    switchView("ai");
    // 问AI 落点维持裁定：固定进 ai-default「AI 助手」（㉒+㉕ 口径），
    // 当前打开的若是别的 AI 会话，先切回默认会话再挂引用条
    if (state.aiConvId !== "ai-default") {
      state.aiConvId = "ai-default";
      loadAiConversations();
      loadAiMessages();
    }
    showAiQuoteBar(m);
  });
}
function clearAskRef() {
  state.pendingAskRef = null;
  $("ai-quote-bar").classList.add("hidden");
}
$("ai-quote-x").addEventListener("click", clearAskRef);
$("btn-ai-send").addEventListener("click", sendAi);
// ⑦ AI 页输入框与聊天框对齐：Enter 发送、Shift+Enter 换行（textarea
// 默认行为，不拦截即可）、高度自适应（wireAutoGrow 已接，上限同为
// COMPOSER_MAX_H）。补输入法守卫：中文输入法组字中的回车是确认候选
// 词，绝不能当发送——聊天框同病，一并对齐。
$("ai-input").addEventListener("keydown", (e) => {
  if (e.isComposing || e.keyCode === 229) return;
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendAi(); }
});
// ① AI 输入框 Ctrl+V 粘贴文件（v1.0.2）：接聊天框同条剪贴板读取链
// （clipboard_file_list 优先——资源管理器里复制的文件只有这条路能拿到）。
// 文件按 AI 文本附件规则校验入队（stageAiFilePaths：文本类、单件与合计
// 512KB、最多 5 个，超限/类型不符逐个 toast）；粘贴图片不走
// clipboard_take_image 回落（那是聊天图片附件的路），明确提示 AI 附件
// 只收文本文件。普通文字默认粘贴进框，不拦截。另：Linux 的 WebKitGTK
// 不向页面暴露图片项，图片粘贴会落进「无文件对象」分支，彼处再以
// clipboard_has_image 补判（只查存在性，不落盘），否则图片会被静默忽略。
$("ai-input").addEventListener("paste", async (e) => {
  const cd = e.clipboardData;
  if (!cd) return;
  const text = cd.getData("text/plain") || "";
  const fileItems = Array.from(cd.items || []).filter((i) => i.kind === "file");
  // 普通文字默认粘贴；但文字与文件同时在剪贴板时，文件不静默丢——
  // 明确提示（第三方评审：旧版直接 return，文件无声消失）。
  if (text.trim()) {
    if (fileItems.length) showToast("已粘贴文字，文件未加入 AI 附件，请用附件按钮添加");
    return;
  }
  if (!fileItems.length && !(cd.files && cd.files.length)) {
    // 既无文字也无文件对象：WebKitGTK 可能什么都不暴露，仍向后端问
    // 一次文件列表；文件路线落空时再问一次剪贴板有无图片——Linux 下
    // 图片项根本不进 DataTransfer，不补这一问，粘贴图片会被静默忽略，
    // 违背「明确提示 AI 附件只收文本」；空剪贴板则静默返回，不提示。
    e.preventDefault();
    let paths = null;
    try { paths = await invoke("clipboard_file_list"); } catch (_) { paths = null; }
    if (paths && paths.length) { await stageAiFilePaths(paths); return; }
    try {
      if (await invoke("clipboard_has_image")) {
        showToast("AI 附件只收文本文件，图片不能作为 AI 附件");
      }
    } catch (_) { /* 探测失败按空剪贴板处理，不打扰 */ }
    return;
  }
  e.preventDefault();
  let paths = null;
  try { paths = await invoke("clipboard_file_list"); } catch (_) { paths = null; }
  if (paths && paths.length) { await stageAiFilePaths(paths); return; }
  const img = fileItems.find((i) => i.type.startsWith("image/"));
  if (img) {
    showToast("AI 附件只收文本文件，图片不能作为 AI 附件");
    return;
  }
  if (fileItems.length || (cd.files && cd.files.length)) {
    showToast(paths === null ? "此系统暂未支持粘贴文件，请用附件按钮选择" : "未能读取粘贴的文件，已忽略");
  }
});

// ---------- ⑦ 窗口重获焦点时把焦点还给输入框（v1.0.2） ----------
// 病根：最小化时 WebView2 把键盘焦点从输入框摘掉、落到页面空白处；
// 恢复时后端只做了窗口级 show+set_focus，前端没有任何还焦点逻辑。
// 口径：只记主输入框（聊天 #input / AI #ai-input）；失焦前最后停在
// 别处控件（改名框、设置字段等）时不抢——那些是瞬态控件，焦点还回去
// 可能撞上它们自己的关闭/校验流程。有对话框/菜单/查看器打开时不抢；
// 焦点已经在某个输入处时不抢。没记录或记录的输入框不在当前页时，按
// 当前页回落到主输入框（聊天页→#input、AI 页→#ai-input）。
let lastComposerId = null; // 最后聚焦的主输入框 id（"input" | "ai-input"）
let lastComposerAt = 0;
let lastOtherInputAt = 0; // 最后一次聚焦别处控件的时间戳（判定先后用）
document.addEventListener("focusin", (e) => {
  const t = e.target;
  if (!t || (t.tagName !== "INPUT" && t.tagName !== "TEXTAREA")) return;
  if (t.id === "input" || t.id === "ai-input") {
    lastComposerId = t.id;
    lastComposerAt = Date.now();
  } else {
    lastOtherInputAt = Date.now();
  }
});
// v1.0.3 验后 #1：还焦点改双路共用实现。页面 window focus 事件在
// 「被动最小化→恢复」时可靠，但手动点标题栏最小化按钮/任务栏图标
// 最小化后恢复，WebView2 的 activeElement 常残留旧值（仍报输入框），
// 旧实现被「已在输入处」守卫误判跳过、键盘焦点其实已丢。故后端在
// 窗口从最小化恢复并获得焦点时另发 window-restored 事件，前端收到后
// 走 force 路径：目标与残留焦点是同一元素时先 blur 再 focus，逼
// WebView2 做一次真实重聚焦。守卫口径与旧实现一致（对话框/菜单/
// 查看器打开不抢、失焦前停在别处控件不抢、焦点真在别处输入控件不抢）。
let lastFocusDoneAt = 0; // 上次真正执行还焦点的时刻（双路同轮去重）
const FOCUS_BLOCKER_SEL = "#modal:not(.hidden), #clear-modal:not(.hidden), #update-modal:not(.hidden), #viewer:not(.hidden), #file-menu:not(.hidden), #viewer-fmt-menu:not(.hidden), #ai-source-menu:not(.hidden), #clip-mode-menu:not(.hidden), #clip-count-menu:not(.hidden)";
function restoreComposerFocus(source, force) {
  const note = (msg) => {
    if (source === "backend") invoke("log_frontend", { msg }).catch(() => {});
  };
  if (document.querySelector(FOCUS_BLOCKER_SEL)) {
    note("窗口恢复还焦点：有对话框/菜单/查看器打开，跳过");
    return;
  }
  if (lastOtherInputAt > lastComposerAt) { // 失焦前最后停在别处控件
    note("窗口恢复还焦点：失焦前停在别处控件，跳过");
    return;
  }
  let el = lastComposerId ? $(lastComposerId) : null;
  if (!el || el.offsetParent === null) {
    el = state.view === "ai" ? $("ai-input") : state.view === "chat" ? $("input") : null;
  }
  if (!el || el.offsetParent === null) {
    note("窗口恢复还焦点：当前页无主输入框，跳过");
    return;
  }
  const active = document.activeElement;
  const activeDesc = active ? active.tagName + (active.id ? "#" + active.id : "") : "无";
  if (active && (active.tagName === "INPUT" || active.tagName === "TEXTAREA") && active !== el) {
    note(`窗口恢复还焦点：焦点在别处输入控件（${activeDesc}），跳过`);
    return;
  }
  if (active === el && !force) return; // 页面路径旧口径：已在输入框就不动
  const now = Date.now();
  if (now - lastFocusDoneAt < 300) return; // 页面事件与后端事件同轮先后到，只执行一次
  lastFocusDoneAt = now;
  if (active === el) el.blur(); // 残留焦点先摘再重落，逼 WebView2 真正重聚焦
  el.focus();
  invoke("log_frontend", { msg: `窗口恢复还焦点（${source}）：恢复前 activeElement=${activeDesc}，已还给 #${el.id}` }).catch(() => {});
  verifyComposerFocus(el);
}
// v1.0.4 验后 #5：还焦点后的延时复核。旧版在 el.focus() 后同步写下
// 「已还焦点」就收工，可 WebView2 在窗口恢复后会异步把键盘焦点重置
// 回 BODY/发送按钮——日志全绿、人却打不了字（双机日志定案）。改成
// 120ms/400ms 两档复核：焦点被抢到非输入元素就再聚焦一次；被用户
// 点到别处输入控件则尊重用户、不抢；对话框打开则收手。400ms 档落
// 一条「最终状态」日志，成功失败都如实记，下次真机一眼定案。
let focusVerifySeq = 0;
function verifyComposerFocus(el) {
  const seq = ++focusVerifySeq;
  const check = (final) => {
    if (seq !== focusVerifySeq) return; // 已有更新一轮还焦点接管
    if (document.querySelector(FOCUS_BLOCKER_SEL)) return; // 对话框接管，收手
    const ae = document.activeElement;
    const desc = (n) => (n ? n.tagName + (n.id ? "#" + n.id : "") : "无");
    if (ae === el) {
      if (final) invoke("log_frontend", { msg: `窗口恢复还焦点：最终确认焦点在 #${el.id}（已恢复）` }).catch(() => {});
      return;
    }
    if (ae && (ae.tagName === "INPUT" || ae.tagName === "TEXTAREA")) {
      if (final) invoke("log_frontend", { msg: `窗口恢复还焦点：最终焦点在别处输入控件 ${desc(ae)}（用户已自行切换，不再抢）` }).catch(() => {});
      return;
    }
    // 焦点落在 BODY/按钮等非输入元素：被异步重置抢走了，再聚焦一次
    el.focus();
    if (final) {
      const ae2 = document.activeElement;
      invoke("log_frontend", { msg: `窗口恢复还焦点：最终确认焦点在 ${desc(ae2)}（${ae2 === el ? "已恢复" : "仍未恢复"}）` }).catch(() => {});
    }
  };
  setTimeout(() => check(false), 120);
  setTimeout(() => check(true), 400);
}
window.addEventListener("focus", () => restoreComposerFocus("page", false));
listen("window-restored", () => restoreComposerFocus("backend", true)).catch(() => {});
$("btn-ai-stop").addEventListener("click", () => invoke("ai_stop"));
// ⑥ AI 页「回到最新」：上滑后出现，点按回底并恢复跟随
$("ai-messages").addEventListener("scroll", syncAiBottomBtn);
$("btn-ai-bottom").addEventListener("click", () => {
  state.follow.ai = true;
  const c = $("ai-messages");
  c.scrollTop = c.scrollHeight;
  syncAiBottomBtn();
});
// ⑤ 思考开关持久化：重开应用保持上次选择
(() => {
  const el = $("ai-thinking");
  if (!el) return;
  el.checked = localStorage.getItem("ai_thinking") === "1";
  el.addEventListener("change", () => localStorage.setItem("ai_thinking", el.checked ? "1" : "0"));
})();

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
/* v2.6.6 toast 收编 + #21 锚定：全应用操作提示只有这一套实现——
 * 统一样式（style.css #toast）、统一进出动画、统一停留时长（普通
 * 2600ms；带动作按钮的 4500ms，留足点击时间，同为本实现的标准档）。
 * 位置口径：锚定当前可见输入框（#composer / #ai-composer）的**当前
 * 上沿** + 12px 间距——输入框自适应长高（上限 168px）时由 autoGrow
 * 钩子实时跟随上移，绝不按固定值写死压框；无输入框的页面（连接/
 * 设置/彩签/收藏等）回落距窗口底 28px。 */
let toastTimer = null;
const TOAST_GAP = 12, TOAST_FALLBACK_BOTTOM = 28;
function positionToast() {
  const t = $("toast");
  if (!t) return;
  let bottom = TOAST_FALLBACK_BOTTOM;
  for (const id of ["composer", "ai-composer"]) {
    const c = $(id);
    if (c && c.offsetParent !== null) {
      const r = c.getBoundingClientRect();
      if (r.height > 0) { bottom = Math.round(window.innerHeight - r.top + TOAST_GAP); break; }
    }
  }
  t.style.bottom = bottom + "px";
}
function toastVisible() { return !$("toast").classList.contains("toast-off"); }
function presentToast(dwellMs, fill) {
  const t = $("toast");
  t.innerHTML = "";
  fill(t);
  positionToast();
  t.classList.remove("toast-off");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("toast-off"), dwellMs);
}
function showToast(text) {
  presentToast(2600, (t) => { t.textContent = text; });
}
/** 带一个动作按钮的 toast（如未配置 AI 时的「去配置」直达设置页）。 */
function showToastAction(text, label, fn) {
  presentToast(4500, (t) => {
    const span = document.createElement("span");
    span.textContent = text;
    const b = document.createElement("button");
    b.className = "toast-action";
    b.textContent = label;
    b.addEventListener("click", () => { t.classList.add("toast-off"); fn(); });
    t.appendChild(span);
    t.appendChild(b);
  });
}
window.addEventListener("resize", () => { if (toastVisible()) positionToast(); });

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
    // ㉚ AI 页顶栏的本地 API 入口做成状态胶囊：一眼看出开/关
    const apiBtn = $("btn-local-api");
    apiBtn.textContent = g.local_enabled ? "本地 API · 开" : "本地 API · 关";
    apiBtn.classList.toggle("api-on", !!g.local_enabled);
    // ㉙ 可用模型一行：数据取网关现成的 gateway_model() 与连接状态，
    // 三态口径——本机真实模型名 / 经对方转发（占位名）/ 暂无可用模型
    const modelEl = $("local-api-model");
    if (!g.local_enabled) modelEl.textContent = "开启后显示";
    else if (g.model && g.model !== "bluelink-proxy") modelEl.textContent = `${g.model}（本机）`;
    else if (g.connected) modelEl.textContent = "bluelink-proxy（经对方设备 AI 转发）";
    else modelEl.textContent = "暂无可用模型（本机未配置 AI，且未连接设备）";
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
const CLIP_MODE_LABEL = { auto: "自动", manual: "手动" };
const CLIP_COUNT_LABEL = { 1: "最新 1 条", 2: "最新 2 条", 3: "最新 3 条", 5: "最新 5 条" };
let clipView = { mode: "auto", count: 1, hotkey: "Ctrl+Alt+V" };
async function loadClipboardSync() {
  try {
    const v = await invoke("get_clipboard_sync");
    clipView = v;
    $("clip-sync-toggle").checked = !!v.local;
    $("clip-sync-status").textContent = clipStatusText(v);
    $("clip-mode-btn").textContent = `${CLIP_MODE_LABEL[v.mode] || "自动"} ▾`;
    $("clip-count-btn").textContent = `${CLIP_COUNT_LABEL[v.count] || CLIP_COUNT_LABEL[1]} ▾`;
    const manual = v.mode === "manual";
    $("clip-hotkey-row").classList.toggle("hidden", !manual);
    $("clip-mode-hint").textContent = manual
      ? "手动：复制的东西先攒着，按快捷键（或点下面的「立即同步一次」）才发给对方。"
      : "自动：这边一复制，那边马上就能粘贴。";
    // 快捷键未生效提示：手动方式且同步已开、但这个组合键没注册上
    $("clip-hotkey-warn").classList.toggle("hidden", !(manual && v.local && !v.hotkey_registered));
    const hk = $("clip-hotkey-input");
    if (document.activeElement !== hk) hk.value = v.hotkey || "Ctrl+Alt+V";
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
    // ⑤ v1.0.2：改完立即同步两处显示与身份缓存——旧版只刷了存储区
    // 的 loadStorageStats()，本机信息卡的 #set-dl 要切页重进设置页
    // 跑 loadSettings() 才更新（用户实报）。
    if (state.identity) state.identity.download_dir = eff;
    $("set-dl").textContent = eff;
    $("store-dir").textContent = eff;
    $("store-dir").title = eff;
    loadStorageStats();
  } catch (e) { showToast("更改保存目录失败：" + e); }
});
$("store-dir-reset").addEventListener("click", async () => {
  try {
    const eff = await invoke("set_download_dir", { path: null });
    showToast("保存目录已恢复默认：" + eff);
    // ⑤ v1.0.2：同上，恢复默认也要同步两处显示与身份缓存。
    if (state.identity) state.identity.download_dir = eff;
    $("set-dl").textContent = eff;
    $("store-dir").textContent = eff;
    $("store-dir").title = eff;
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
  loadGatewaySettings(); // ㉙ AI 配置保存后刷新可用模型一行
});
$("btn-ai-test").addEventListener("click", async () => {
  $("ai-test-result").textContent = "测试中…";
  try {
    // ③ 用表单当前值直测（v2.6.1）：改了地址/模型/密钥不必先保存。
    // 密钥框留空 = 沿用已保存的密钥（表单从不回显明文），后端同口径。
    $("ai-test-result").textContent = await invoke("test_ai_connection", {
      baseUrl: $("ai-base").value.trim(),
      apiKey: $("ai-key").value.trim(),
      model: $("ai-model").value.trim(),
    });
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
// ---------- 剪贴板同步方式/条数自绘下拉（与默认AI 来源同款实现：
// 按钮 + 应用内菜单，✓ 标当前项，Esc/上下键导航，选完焦点还给按钮） ----------
function setupMenuDropdown(btnId, menuId, options, getCurrent, onPick) {
  const btn = $(btnId), menu = $(menuId);
  const close = () => menu.classList.add("hidden");
  const open = () => {
    menu.innerHTML = "";
    const cur = getCurrent();
    for (const [v, label] of options) {
      const b = document.createElement("button");
      b.textContent = (String(cur) === String(v) ? "✓ " : "") + label;
      b.addEventListener("click", async () => {
        close();
        btn.focus();
        await onPick(v);
      });
      menu.appendChild(b);
    }
    menu.onkeydown = (e) => {
      const items = [...menu.querySelectorAll("button")];
      const i = items.indexOf(document.activeElement);
      if (e.key === "Escape") { e.preventDefault(); close(); btn.focus(); }
      else if (e.key === "ArrowDown") { e.preventDefault(); (items[i + 1] || items[0]).focus(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); (items[i - 1] || items[items.length - 1]).focus(); }
    };
    menu.classList.remove("hidden");
    const curBtn = [...menu.querySelectorAll("button")].find((b) => b.textContent.startsWith("✓ "));
    (curBtn || menu.querySelector("button")).focus();
  };
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (menu.classList.contains("hidden")) open(); else close();
  });
}
setupMenuDropdown("clip-mode-btn", "clip-mode-menu",
  [["auto", "自动"], ["manual", "手动"]],
  () => clipView.mode,
  async (v) => {
    await invoke("set_clipboard_sync_mode", { mode: v }).catch(() => {});
    showToast(v === "manual" ? "已改成手动同步：复制的东西先攒着，按快捷键才发给对方" : "已改成自动同步：复制了马上发给对方");
    loadClipboardSync();
  });
setupMenuDropdown("clip-count-btn", "clip-count-menu",
  [[1, "最新 1 条"], [2, "最新 2 条"], [3, "最新 3 条"], [5, "最新 5 条"]],
  () => clipView.count,
  async (v) => {
    await invoke("set_clipboard_sync_count", { count: v }).catch(() => {});
    showToast(`每次同步条数已设为：最新 ${v} 条`);
    loadClipboardSync();
  });
// ---------- 手动同步快捷键录制：点中输入框后按下组合键即录入
// （必须带 Ctrl/Alt/Super 至少一个，不然会跟平时打字冲突）；
// Esc 取消，录完即保存，后端随即重新注册。 ----------
let clipHotkeyRecording = false;
const clipHotkeyInput = $("clip-hotkey-input");
clipHotkeyInput.addEventListener("focus", () => {
  clipHotkeyRecording = true;
  clipHotkeyInput.classList.add("recording");
  clipHotkeyInput.value = "请按下组合键…";
});
clipHotkeyInput.addEventListener("blur", () => {
  clipHotkeyRecording = false;
  clipHotkeyInput.classList.remove("recording");
  loadClipboardSync();
});
clipHotkeyInput.addEventListener("keydown", async (e) => {
  if (!clipHotkeyRecording) return;
  e.preventDefault();
  if (e.key === "Escape") { clipHotkeyInput.blur(); return; }
  if (["Control", "Alt", "Shift", "Meta"].includes(e.key)) return; // 只按了修饰键，等主键
  if (!e.ctrlKey && !e.altKey && !e.metaKey) {
    showToast("快捷键要带上 Ctrl 或 Alt 一起按，不然会跟平时打字冲突");
    return;
  }
  const parts = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Super");
  let key = e.key;
  if (key === " ") key = "Space";
  else if (key.length === 1) key = key.toUpperCase();
  parts.push(key);
  const combo = parts.join("+");
  try {
    await invoke("set_clipboard_sync_hotkey", { hotkey: combo });
    showToast(`手动同步快捷键已设为 ${combo}`);
  } catch (err) { showToast(String(err)); }
  clipHotkeyInput.blur();
});
$("clip-hotkey-reset").addEventListener("click", async () => {
  try {
    await invoke("set_clipboard_sync_hotkey", { hotkey: "Ctrl+Alt+V" });
    showToast("快捷键已恢复默认：Ctrl+Alt+V");
  } catch (err) { showToast(String(err)); }
  loadClipboardSync();
});
// ---------- 立即同步一次（按钮与全局快捷键共用结果口径） ----------
function clipManualResultToast(v) {
  if (!v) return;
  if (v.kind === "sent") showToast(v.count > 1 ? `已把最近 ${v.count} 条同步给对方` : "已同步给对方");
  else if (v.kind === "not_connected") showToast("还没连接设备，先连上再同步");
  else if (v.kind === "local_off") showToast("本机剪贴板同步还没开启");
  else if (v.kind === "peer_off") showToast("对方还没开启剪贴板同步");
  else if (v.kind === "empty") showToast("还没有可同步的内容，先复制点东西");
  else if (v.kind === "occupied") showToast("剪贴板正被其他程序占用，这次没读到新内容，稍后再试");
}
$("btn-clip-sync-now").addEventListener("click", async () => {
  try { clipManualResultToast(await invoke("sync_clipboard_now")); }
  catch (e) { showToast("同步失败：" + e); }
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
// #14 清空聊天记录可选范围（v2.6.5）：先选范围（近 24 小时/近 7 天/
// 近 30 天/全部），再二次确认；后端只删范围内的消息及其彩签/收藏/
// 置顶标记，范围外一条不动。
// v1.0.2 ④：对话框加独立勾选「同时删除相关文件」（默认不勾；勾上才删，
// 且只删所选范围内消息关联的、位于应用下载目录里的实体文件——后端
// 命令带 deleteFiles 参数执行，前端只传开关）；单会话清空（AI 会话
// 菜单的「清空聊天记录」）复用同一对话框（会话模式），口径一致。
const CLEAR_RANGES = {
  "24h": "近 24 小时",
  "7d": "近 7 天",
  "30d": "近 30 天",
  "all": "全部时间",
};
let clearMode = { kind: "range" }; // 或 { kind: "conversation", convId, title }
/** 打开清空对话框：range=设置页按范围清；conversation=只清指定 AI 会话。
 * 勾选每次打开都复位为不勾（默认不删文件，用户每回显式决定）。 */
function openClearModal(mode) {
  clearMode = mode;
  const conv = mode.kind === "conversation";
  $("clear-title").textContent = conv ? `清空「${mode.title}」的聊天记录` : "清空聊天记录";
  $("clear-desc").textContent = conv
    ? "只清除本会话的消息，会话本身保留。此操作不可恢复。"
    : "只清除所选范围内的消息，范围外的记录不动。此操作不可恢复。";
  document.querySelector("#clear-modal .clear-range-list").classList.toggle("hidden", conv);
  $("clear-conv-go").classList.toggle("hidden", !conv);
  $("clear-delete-files").checked = false;
  $("clear-modal").classList.remove("hidden");
}
/** 二次确认文案里的删文件说明（按当前模式写清作用域）。 */
function clearFilesSuffix() {
  if (!$("clear-delete-files").checked) return "";
  const scope = clearMode.kind === "conversation" ? "本会话消息关联的" : "所选范围内消息关联的";
  return `已勾选「同时删除相关文件」：${scope}、应用下载目录里的文件也会一并删除，目录外的文件不受影响。`;
}
$("btn-clear-history").addEventListener("click", () => {
  // 流式进行中不许清（第三方评审采纳）：清完重拉会把正在写的流式
  // 气泡从文档上摘掉，后续分片写进脱离文档的节点、状态错乱
  if (state.aiGenerating || chatAiBusy || state.streamEl) {
    showToast("AI 正在回复中，请等回复完成后再清空");
    return;
  }
  openClearModal({ kind: "range" });
});
$("clear-cancel").addEventListener("click", () => $("clear-modal").classList.add("hidden"));
$("clear-modal").addEventListener("click", (e) => {
  if (e.target === $("clear-modal")) $("clear-modal").classList.add("hidden");
});
document.querySelectorAll("#clear-modal .clear-range").forEach((b) => {
  b.addEventListener("click", async () => {
    const range = b.dataset.range;
    const deleteFiles = $("clear-delete-files").checked;
    $("clear-modal").classList.add("hidden");
    const ok = await showConfirm(
      `确定要清空${CLEAR_RANGES[range]}的聊天记录吗？范围内的消息、彩签归属、收藏与置顶都会一并清除，范围外的记录不受影响，此操作不可恢复。${clearFilesSuffix()}`,
      "清空"
    );
    if (!ok) return;
    try {
      const r = await invoke("clear_chat_history", { range, deleteFiles });
      const filesNote = r.files_deleted ? `，并删除 ${r.files_deleted} 个相关文件` : "";
      $("store-result").textContent = `已清空 ${r.messages} 条聊天记录（${CLEAR_RANGES[range]}）${filesNote}`;
      // 列表按当前会话重新拉取（范围清理后旧渲染集必然过期）
      state.renderedChat.clear();
      state.renderedAi.clear();
      if (state.peerConvId) await loadChatMessages();
      await loadAiMessages();
      refreshRibbon();
      loadStorageStats();
      showToast(`已清空 ${r.messages} 条聊天记录${filesNote}`);
    } catch (e) { showToast("清空失败：" + e); }
  });
});
// 会话模式：只清指定 AI 会话（菜单入口复用同一对话框与同一勾选）
$("clear-conv-go").addEventListener("click", async () => {
  if (clearMode.kind !== "conversation") return;
  // 与菜单入口同一守卫：执行瞬间仍在生成就拦下（清完重拉会把正在写的
  // 流式气泡从文档上摘掉；旧版菜单两步武装在第二下时复查，本处等效）
  if (state.aiGenerating || chatAiBusy || state.streamEl) {
    showToast("AI 正在回复中，请等回复完成后再清空");
    return;
  }
  const { convId } = clearMode;
  const deleteFiles = $("clear-delete-files").checked;
  $("clear-modal").classList.add("hidden");
  const ok = await showConfirm(
    `确定要清空「${clearMode.title}」的聊天记录吗？本会话的消息、彩签归属、收藏与置顶都会一并清除，会话本身保留，此操作不可恢复。${clearFilesSuffix()}`,
    "清空"
  );
  if (!ok) return;
  try {
    const r = await invoke("clear_ai_conversation_messages", { conversationId: convId, deleteFiles });
    const filesNote = r.files_deleted ? `，并删除 ${r.files_deleted} 个相关文件` : "";
    if (state.aiConvId === convId) {
      state.renderedAi.clear();
      await loadAiMessages();
      refreshPinBar();
    }
    showToast(`已清空 ${r.messages} 条聊天记录${filesNote}`);
  } catch (e) { showToast("清空失败：" + e); }
});

// ---------- 导出 ----------
/** 导出当前设备会话为 Markdown 文本文件（路径由保存对话框选定）。 */
async function exportCurrentConv() {
  if (!state.peerConvId) { showToast("还没有可导出的会话"); return; }
  const msgs = await invoke("list_messages", { conversationId: state.peerConvId, limit: 1000 });
  const lines = ["# BlueLink 聊天记录", "", `导出时间：${new Date().toLocaleString("zh-CN")}`, ""];
  for (const m of msgs) {
    const who = m.sender === "me" ? "我" : "对方";
    // #11：导出同样按类型写——图片组清单 JSON 不许原样进导出文件
    const body = m.kind === "text" ? m.body : msgSummary(m, 200);
    lines.push(`- [${fmtDay(m.created_at)} ${fmtTime(m.created_at)}] ${who}：${body}`);
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
    items.push({ label: `打开：${label}`, run: () => { if (v === "chat") goChat(); else switchView(v); } });
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
  // #16 Ctrl+A 作用域（v2.6.5）：只作用于焦点/选区所在的内容区，绝不
  // 跨区把整个界面选蓝。输入框走原生全选（不拦）；光标在某个消息
  // 气泡/查看器文本里 → 只选那个区；其余位置（框架区）直接拦下。
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a") {
    const ae = document.activeElement;
    if (ae && (ae.tagName === "TEXTAREA" || ae.tagName === "INPUT" || ae.tagName === "SELECT" || ae.isContentEditable)) return;
    const sel = window.getSelection();
    // 锚点节点可能是文本节点（取父元素）也可能是元素本身（直接用）
    const anchorNode = sel && sel.anchorNode ? sel.anchorNode : null;
    const anchor = anchorNode
      ? (anchorNode.nodeType === 3 ? anchorNode.parentElement : anchorNode)
      : null;
    const zone = anchor && anchor.closest ? anchor.closest(".bubble, #viewer-lines") : null;
    e.preventDefault();
    if (zone) {
      const range = document.createRange();
      range.selectNodeContents(zone);
      sel.removeAllRanges();
      sel.addRange(range);
    }
  }
});

// ---------- 后端事件 ----------
// #20 「对方正在输入」自灭计时与统一清除口（断开/超时/对端停输共用）
let peerTypingTimer = null;
function clearPeerTyping() {
  clearTimeout(peerTypingTimer);
  peerTypingTimer = null;
  const hint = $("typing-hint");
  if (hint) hint.classList.add("hidden");
}
async function wireEvents() {
  await listen("bt_devices", (e) => renderBtDevices(e.payload || []));
  await listen("connection_state", async (e) => {
    const wasConnected = state.connected;
    state.connected = e.payload.connected;
    if (state.connected) state.linkState = "on";
    $("nav-logo").classList.toggle("linked", state.connected);
    if (!state.connected) ringClearAll();
    state.peerName = e.payload.peer_name;
    // ㉛ 落点规则的数据面：记下实际连上的对端会话（断开时清空）
    state.connectedConvId = state.connected ? (e.payload.conversation_id || null) : null;
    loadGatewaySettings(); // ㉙ 连接状态变化时刷新可用模型一行
    $("connect-status").textContent = state.connected ? "" : $("connect-status").textContent;
    // 连接页的已连接卡与胶囊：连上时已连接卡替换本机设备卡，断开时恢复
    if (state.connected) {
      $("connected-name").textContent = state.peerName || "对方设备";
      $("connected-sub").textContent = state.connectTransport ? transportName(state.connectTransport) : "";
      $("connected-card").classList.remove("hidden");
      $("my-card").classList.add("hidden");
      $("bt-code-card").classList.add("hidden");
      $("connect-view").classList.remove("code-open");
    } else {
      state.connectTransport = null;
      $("connected-card").classList.add("hidden");
      $("my-card").classList.remove("hidden");
    }
    updateCapsule();
    if (lastDevices.length) renderBtDevices(lastDevices);
    await loadConversations();
    if (state.connected) {
      // ㉛ 落点规则（定版）：一律打开实际连上的那个对端的会话——从
      // 会话 A 去连接、连上 B 就落 B 的会话，不回 A
      if (state.connectedConvId) {
        state.peerConvId = state.connectedConvId;
        state.chatDetailOpen = true; // 入口模型定版：连接落点即详情已打开
        const cur = (await invoke("list_conversations").catch(() => []))
          .find((c) => c.id === state.connectedConvId);
        if (cur) state.peerConvTitle = cur.title || "";
      }
      // 在连接页/聊天页时落到该会话；在别的页（AI/设置等）不打断
      if (state.view === "connect" || state.view === "chat") { switchView("chat"); loadChatMessages(); }
      syncOfflineUI();
    } else {
      // 入口定版·断开落点修订（2026-11-22 用户拍板，覆盖旧
      // 「断开不踢出聊天页」）：连接中途断开默认落连接页；断开同时
      // 作废「切回恢复」资格（chatDetailOpen 清零）——离线看会话的
      // 唯一路径恢复为手动展开会话列表点选。自动重连横幅本就浮在
      // 窗口顶层、随之落在连接页上；重连成功仍走上面连接分支自动
      // 落对端会话。#20：「对方正在输入」与连接硬绑定，断开立即清。
      state.linkState = "off";
      clearPeerTyping();
      loadLastPeer();
      syncOfflineUI();
      // 落点只在「真掉线沿」执行一次：重复的未连接事件（重连停止等）
      // 不许把用户正离线翻看的会话详情再次抢走。
      if (wasConnected) {
        state.chatDetailOpen = false;
        if (state.view === "chat") switchView("connect");
      }
    }
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
    state.chatDetailOpen = true; // 通知直达打开详情（入口模型定版的明确动作）
    switchView("chat");
    loadChatMessages(); // ㉛ 通知回窗看历史同样不问连接状态
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
    // #20 双保险（v2.6.5）：提示只在已连接时显示；每次亮起都重置
    // 4 秒自灭计时——对端输到一半断线/停手而「停止输入」事件没到时
    // 也不会永远挂着。断开连接时由 connection_state 直接清（硬绑定）。
    if (e.payload && state.connected) {
      $("typing-hint").classList.remove("hidden");
      ringPulse("peer");
      clearTimeout(peerTypingTimer);
      peerTypingTimer = setTimeout(clearPeerTyping, 4000);
    } else {
      clearPeerTyping();
    }
  });
  await listen("clipboard_state", (e) => {
    const v = e.payload || {};
    $("clip-sync-toggle").checked = !!v.local;
    $("clip-sync-status").textContent = clipStatusText(v);
  });
  // 全局快捷键触发的手动同步结果：与按钮同一套人话提示
  await listen("clipboard_manual_result", (e) => clipManualResultToast(e.payload));
  // 快捷键注册失败（被其他程序占用）：弹提示 + 刷新设置页未生效标记
  await listen("clipboard_hotkey_error", (e) => {
    showToast(String(e.payload || "快捷键注册失败"));
    loadClipboardSync();
  });
  await listen("reconnect_state", (e) => {
    const v = e.payload || {};
    const banner = $("reconnect-banner");
    if (v.state === "trying") {
      state.linkState = "reconnecting"; // #17：重连中顶栏断开钮保留（可取消）
      syncOfflineUI();
      banner.classList.remove("hidden");
      $("reconnect-text").textContent = `与 ${v.peer_name || "对方"} 的连接中断，正在自动重连（第 ${v.attempt || 1} 次）…`;
    } else if (v.state === "connected") {
      banner.classList.add("hidden");
      showToast(`已自动重连到 ${v.peer_name || "对方"}`);
    } else if (v.state === "gave_up") {
      state.linkState = "off";
      syncOfflineUI();
      banner.classList.add("hidden");
      showToast(`自动重连失败：未能恢复与 ${v.peer_name || "对方"} 的连接，请手动重连`);
    }
  });
  await listen("clipboard_synced", (e) => {
    const kind = e.payload && e.payload.kind === "image" ? "图片" : "文字";
    $("clip-sync-status").textContent = `已同步（刚刚收到${kind}）`;
  });
  // #2（v2.6.6）：传输事件一律先记账（state.transfers 权威账）再
  // 按账绘制；气泡不在场（还没渲染/已重渲染）不丢事件，气泡迟建
  // 时在 buildMessageEl 里按账回放。旧版「事件到达时现查气泡 +
  // lastFileBubble 单例兜底」在连珠速发下必冻（见 state.transfers
  // 字段注释的真机定案）。
  await listen("transfer_progress", (e) => {
    const { transfer_id, msg_id, received, total } = e.payload;
    // 中继更新包的传输不进聊天传输账，只喂更新框进度
    if (msg_id && msg_id.startsWith("update:")) { relayOnProgress(transfer_id, msg_id, received, total); return; }
    const rec = noteTransferProgress(transfer_id, msg_id, received, total);
    // 图片组成员的进度按组聚合显示在组气泡上（成员 msg_id → 组 id）
    const gid = msg_id ? (state.groupMemberOf || {})[msg_id] : null;
    if (gid) { updateGroupProgress(gid, msg_id, received, total); return; }
    const b = bubbleOfMsg(msg_id);
    if (b) paintTransferProgress(b, rec);
  });
  await listen("transfer_state", (e) => {
    const { transfer_id, state: tstate, msg_id } = e.payload;
    // 中继更新包同上：只喂更新框状态
    if (msg_id && msg_id.startsWith("update:")) { relayOnState(transfer_id, msg_id, tstate); return; }
    const rec = noteTransferState(transfer_id, tstate, msg_id);
    const mid = rec.msgId;
    // 图片组成员的状态同样按组聚合（完成时顺带刷新组气泡缩略图）
    const gid = mid ? (state.groupMemberOf || {})[mid] : null;
    if (gid) { updateGroupTransferState(gid, mid, tstate); return; }
    const b = bubbleOfMsg(mid);
    if (b) paintTransferState(b, rec);
  });
  await listen("update_package_ready", (e) => { relayOnPackageReady(e.payload || {}); });
  await listen("ai_chunk", (e) => {
    const container = $("ai-messages");
    // #3：只渲染当前打开会话的流——其他会话（问AI 落 ai-default 等）
    // 的片混进当前列表会让气泡出现在错位置。
    if (e.payload.conversation_id && e.payload.conversation_id !== state.aiConvId) return;
    if (!state.streamEl) {
      const row = document.createElement("div");
      row.className = "msg-row";
      row.innerHTML = `<div class="msg"><div class="bubble"></div></div>`;
      // 兜底建行同守锚定纪律（#1）：bindNext——问题已渲染时它就是
      // 末行、挂末尾即其正下方；未渲染时等 appendMessage 绑定归位
      placeAiWaitingRow(container, row, { bindNext: true });
      state.streamEl = row.querySelector(".bubble");
      state.streamText = "";
    }
    // 首片到达：等待气泡原地转流式（④）——清掉等待样式与文案，
    // 之后每片把全文重渲染 Markdown（⑫，半截围栏由 md() 容错）
    if (state.streamWaiting) {
      state.streamEl.classList.remove("ai-waiting");
      state.streamEl.innerHTML = "";
      state.streamWaiting = false;
      const wr = $("ai-waiting-row");
      if (wr) wr.removeAttribute("id");
    }
    state.streamText += e.payload.delta;
    state.streamEl.innerHTML = md(state.streamText);
    // 滚动定版：跟随开着才贴底；手势停跟随后流式再长也不动 scrollTop
    if (state.follow.ai) container.scrollTop = container.scrollHeight;
    syncAiBottomBtn();
  });
  await listen("ai_done", (e) => {
    state.streamEl = null;
    state.streamText = "";
    state.streamWaiting = false;
    loadAiMessages();
  });
  await listen("ai_error", (e) => { clearAiWaiting(); showToast(e.payload.error); });
  // ⑤ 思考开关被当前接口忽略时的一次性小字提示（后端判定后回执）
  await listen("ai_thinking_note", () => {
    const n = $("ai-think-note");
    if (n) n.classList.remove("hidden");
  });
}

// ---------- 主题 ----------
async function applyTheme(mode) {
  document.querySelectorAll("#theme-seg button").forEach((b) =>
    b.classList.toggle("active", b.dataset.themeSet === mode));
  // #10 时序（v2.6.5）：先等原生标题栏变色落地，再翻内容主题。旧版
  // 内容同步翻色、标题栏走异步 IPC 慢一拍，切换瞬间上下两段色
  // （v2.6.4 #10 实锤）。IPC 是本地毫秒级调用，等它回来再翻内容，
  // 两者落在同一帧附近、时差不可感知；加 120ms 兜底防非 Tauri
  // 环境里 invoke 悬挂拖住内容翻色。
  await syncWindowTheme(mode);
  if (mode === "system") document.documentElement.removeAttribute("data-theme");
  else document.documentElement.setAttribute("data-theme", mode);
}
// 原生窗口标题栏跟随主题（深色时不再是一条白杠）：走后端命令
// set_window_theme（Window::set_theme）。失败静默，不阻塞主题切换；
// 非 Tauri 环境（纯浏览器打开页面）直接跳过。返回可等待的 Promise。
function syncWindowTheme(mode) {
  try {
    const p = invoke("set_window_theme", { theme: mode });
    if (p && p.catch) {
      p.catch(() => {});
      return Promise.race([p, new Promise((r) => setTimeout(r, 120))]);
    }
  } catch (_) { /* 忽略 */ }
  return Promise.resolve();
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
// 已是最新给明确提示；发现新版弹窗（版本号+说明全文渲染）→「立即更新」下载
// （进度条；下载中可取消——插件下载无法中途掐断，取消=放弃，下载完不安装）→
// 下载完成再确认 → 安装并自动重启。签名校验由 updater 插件按公钥执行。
let updHandle = null;   // check() 返回的 Update 对象
let updPhase = "idle";  // idle | found | downloading | downloaded | installing
let updAbandoned = false;
// 中继更新上下文（经蓝牙对端代下安装包）：非空时更新框走中继流程，
// 与直连的 updHandle 互斥。reqId/transferId 用于对账传输事件，
// signature 是查版本时对端回传的签名、收包后验签用。
let relayCtx = null;

/** 更新说明 Markdown 子集 → HTML（v1.0.2 换包修复）：发布说明原文是
 *  Markdown，旧实现用 textContent 原样贴出，# / ** 标记裸显，还被
 *  600 字截断砍掉大半。只支持发布说明实际用到的子集：一级标题（与
 *  更新框标题「发现新版本 vX.Y.Z」重复，整行丢弃）、## 小节标题、
 *  - / * 列表、段落、行内 **粗体** 与 `代码`。先逐行转义再套结构，
 *  原始 HTML 永远进不了 DOM；不支持的语法按纯文字呈现，不引库。 */
function renderReleaseNotes(md) {
  const inline = (t) => esc(t)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/`([^`]+)`/g, "<code>$1</code>");
  const out = [];
  let inList = false;
  const closeList = () => { if (inList) { out.push("</ul>"); inList = false; } };
  for (const raw of String(md || "").split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) { closeList(); continue; }
    if (/^#\s/.test(line)) { closeList(); continue; }
    const h = line.match(/^#{2,6}\s+(.*)$/);
    if (h) { closeList(); out.push(`<h4>${inline(h[1])}</h4>`); continue; }
    const li = line.match(/^[-*]\s+(.*)$/);
    if (li) {
      if (!inList) { out.push("<ul>"); inList = true; }
      out.push(`<li>${inline(li[1])}</li>`);
      continue;
    }
    closeList();
    out.push(`<p>${inline(line)}</p>`);
  }
  closeList();
  return out.join("");
}

function updSetStatus(text) { $("update-status").textContent = text; }
function friendlyUpdError(e) {
  const s = String((e && e.message) || e || "");
  // #15 按情况拆分（v2.6.5，用户拍板：不同情况给不同提示，不许再用
  // 一句「或」把两种原因糊在一起）。判定顺序：签名/校验 → 无更新
  // 文件 → 网络。原文由调用方记进运行日志备查。
  // ③ 签名/内容校验失败：更新文件下到了但验不过——单独说清。
  if (/signature|verify|invalid.*(sig|content)|checksum/i.test(s)) {
    return "更新文件校验没有通过，已取消更新。请稍后再试；如果反复出现，请反馈这个问题。";
  }
  // ② 服务器通了但没有更新文件：当前正式版发布时还没有附带更新
  // 文件（latest.json），检查更新暂时用不了——不是你的网络问题。
  if (/valid release JSON|release JSON|404|not found/i.test(s)) {
    return "当前正式版还没有包含更新文件，暂时无法通过应用内更新。等下一个正式版发布后即可使用。";
  }
  // ① 连不上服务器：只说网络。
  if (/network|error sending request|dns|connect|timed out|timeout|unreachable|offline/i.test(s)) {
    return "连不上更新服务器，请检查网络后重试。";
  }
  return s.slice(0, 80) || "未知错误";
}

/** 版本号比较：a 比 b 新返回 true（按点分数字段逐段比，缺段补 0）。 */
function isNewerVersion(a, b) {
  const pa = String(a || "").split(".").map((x) => parseInt(x, 10) || 0);
  const pb = String(b || "").split(".").map((x) => parseInt(x, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d !== 0) return d > 0;
  }
  return false;
}

/** 直连检查的错误是否属于「连不上服务器」一类（与 friendlyUpdError
 * 的网络类判定同口径）：只有这类失败才有资格转蓝牙中继，其余失败
 * （无更新文件、签名问题等）转中继也没用、照旧提示。 */
function isNetworkUpdError(e) {
  const s = String((e && e.message) || e || "");
  return /network|error sending request|dns|connect|timed out|timeout|unreachable|offline/i.test(s);
}

/** 发现新版后的弹窗呈现（直连/中继共用）：版本号 + 说明全文渲染。
 * viaRelay 时状态行按用户定案只写「正在通过蓝牙通道中继更新」。 */
function presentUpdateFound(version, notes, viaRelay) {
  updPhase = "found";
  updAbandoned = false;
  $("update-title").textContent = `发现新版本 v${version}`;
  const notesEl = $("update-notes");
  // 说明全文呈现（Markdown 子集渲染，见 renderReleaseNotes），不再截断
  const n = (notes || "").trim();
  if (n) notesEl.innerHTML = renderReleaseNotes(n);
  else notesEl.textContent = "（此版本未附更新说明）";
  notesEl.scrollTop = 0;
  $("update-progress-wrap").classList.add("hidden");
  $("update-action").textContent = "立即更新";
  $("update-action").disabled = false;
  $("update-cancel").textContent = "以后再说";
  $("update-cancel").disabled = false;
  $("update-modal").classList.remove("hidden");
  updSetStatus(viaRelay ? "正在通过蓝牙通道中继更新" : `发现新版本 v${version}`);
}

/** 直连失败后的中继分流（用户定案）：连着对端且对端支持中继 →
 * 请对端代查；没连对端 → 直接提示；对端太旧 → 如实提示不支持。 */
async function relayCheckFallback() {
  let st = null;
  try { st = await invoke("relay_update_status"); } catch (_) { /* 状态拿不到按未连接处理 */ }
  if (!st || !st.connected) {
    updSetStatus("检查失败，请稍后再试");
    showToast("检查更新失败：无网络且未连接对端，无法检查更新；连接外网机后可通过蓝牙中继更新");
    return;
  }
  if (!st.peer_relay) {
    updSetStatus("检查失败，请稍后再试");
    showToast("对端版本过旧，不支持中继更新");
    return;
  }
  updSetStatus("正在通过蓝牙通道中继更新");
  let info;
  try {
    info = await invoke("relay_update_check");
  } catch (e) {
    updSetStatus("检查失败，请稍后再试");
    invoke("log_frontend", { msg: "中继检查更新失败：" + String((e && e.message) || e || "") }).catch(() => {});
    showToast("中继检查更新失败：" + String((e && e.message) || e || "").slice(0, 80));
    return;
  }
  let cur = "";
  try { cur = await window.__TAURI__.app.getVersion(); } catch (_) { /* 拿不到版本按有新版呈现，不误判已最新 */ }
  if (cur && !isNewerVersion(info.version, cur)) {
    updSetStatus("已是最新版本");
    showToast("已是最新版本");
    return;
  }
  relayCtx = {
    version: info.version,
    signature: info.signature,
    reqId: null,
    transferId: null,
    path: null,
    abandoned: false,
    pendingEv: null,
  };
  presentUpdateFound(info.version, info.notes, true);
}

/** 中继流程的「立即更新 / 安装并重启」按钮动作（与直连共用一个按钮，
 * 靠 relayCtx 分流）：found → 请对端下载回传（受理后进度与到达由
 * 传输事件推进）；downloaded → 拉起安装包并退出本进程。 */
async function relayUpdateAction() {
  const ctx = relayCtx;
  if (!ctx) return;
  if (updPhase === "found") {
    updPhase = "downloading";
    ctx.abandoned = false;
    $("update-action").disabled = true;
    $("update-cancel").textContent = "取消";
    $("update-cancel").disabled = false;
    $("update-progress-wrap").classList.remove("hidden");
    $("update-progress-bar").style.width = "0%";
    $("update-progress-text").textContent = "正在通过蓝牙通道中继更新…";
    updSetStatus("正在通过蓝牙通道中继更新");
    try {
      // 此调用等对端下完包并开始回传才返回（对端下载慢时这里久等，
      // 界面文案已写明走蓝牙中继，不会让用户误以为卡死）
      const res = await invoke("relay_update_fetch");
      if (relayCtx !== ctx) return;
      ctx.reqId = res.req_id;
      // 对端本次取包实际所据的版本与所示不一致：整个更新取消
      // （包随后到达也会被当无主包清掉，不会装错版本）
      if (res.version && res.version !== ctx.version) {
        relayCtx = null;
        updPhase = "idle";
        $("update-modal").classList.add("hidden");
        updSetStatus("检查失败，请稍后再试");
        showToast("对端取回的更新版本与所示版本不一致，已取消更新");
        return;
      }
      // 验签改用与包同一次快照的签名（防两次请求间发布变化错位）
      if (res.signature) ctx.signature = res.signature;
      // reqId 未知期间积压的最后一个传输事件现在补喂（防串台又不丢尾）
      const pe = ctx.pendingEv;
      ctx.pendingEv = null;
      if (pe && pe.kind === "progress") relayOnProgress(pe.transferId, "update:" + ctx.reqId, pe.received, pe.total);
      if (pe && pe.kind === "state") relayOnState(pe.transferId, "update:" + ctx.reqId, pe.tstate);
    } catch (e) {
      if (relayCtx !== ctx) return;
      updPhase = "found";
      $("update-action").disabled = false;
      $("update-progress-wrap").classList.add("hidden");
      showToast("中继下载失败：" + String((e && e.message) || e || "").slice(0, 80));
    }
  } else if (updPhase === "downloaded") {
    updPhase = "installing";
    $("update-action").disabled = true;
    $("update-cancel").disabled = true;
    $("update-action").textContent = "正在安装…";
    try {
      await invoke("install_update_package", { path: ctx.path, signature: ctx.signature, expectedVersion: ctx.version });
      // 成功时本进程随即退出，不会走到这里之后
    } catch (e) {
      updPhase = "downloaded";
      $("update-action").disabled = false;
      $("update-cancel").disabled = false;
      $("update-action").textContent = "安装并重启";
      showToast("安装失败：" + String((e && e.message) || e || "").slice(0, 80));
    }
  }
}

/** 中继传输进度（传输事件按 update:{reqId} 对账后转发到这里）：
 * 只更新更新框的进度条，不进聊天传输账。reqId 未知（取包调用还没
 * 返回）时只留最后一个事件待补喂——本轮的包不可能在这个窗口里
 * 先到，先到的只会是上一轮残留事件，绝不能喂进本轮更新框。 */
function relayOnProgress(transferId, msgId, received, total) {
  if (!relayCtx || relayCtx.abandoned) return;
  if (!relayCtx.reqId) {
    relayCtx.pendingEv = { kind: "progress", transferId, received, total };
    return;
  }
  if (msgId !== "update:" + relayCtx.reqId) return;
  relayCtx.transferId = transferId;
  if (updPhase !== "downloading") return;
  if (total > 0) {
    const pct = Math.min(100, Math.round((received / total) * 100));
    $("update-progress-bar").style.width = pct + "%";
    $("update-progress-text").textContent = `正在通过蓝牙通道中继更新… ${pct}%`;
  }
}

/** 中继传输状态：校验中给文案；失败/被取消回到 found 态可重试。 */
function relayOnState(transferId, msgId, tstate) {
  if (!relayCtx || relayCtx.abandoned) return;
  if (!relayCtx.reqId) {
    relayCtx.pendingEv = { kind: "state", transferId, tstate };
    return;
  }
  if (msgId !== "update:" + relayCtx.reqId) return;
  relayCtx.transferId = transferId;
  if (updPhase !== "downloading") return;
  if (tstate === "verifying") {
    $("update-progress-text").textContent = "正在校验更新包…";
  } else if (tstate === "verify_failed" || tstate === "cancelled") {
    updPhase = "found";
    $("update-action").disabled = false;
    $("update-progress-wrap").classList.add("hidden");
    showToast(tstate === "cancelled" ? "中继传输已取消" : "中继传输校验失败，请重试");
  }
}

/** 中继更新包到达（已过传输层哈希校验）：先验签，验过才呈现
 * 「下载完成」；验不过直接取消更新——绝不给安装入口。 */
async function relayOnPackageReady(p) {
  const ctx = relayCtx;
  if (!ctx || ctx.abandoned) {
    // 无主包（用户已取消/流程已易主）：清掉暂存文件，不留垃圾
    if (p && p.path) invoke("discard_update_package", { path: p.path }).catch(() => {});
    return;
  }
  if (updPhase !== "downloading") return;
  ctx.path = p.path;
  $("update-progress-text").textContent = "正在校验更新包签名…";
  try {
    await invoke("verify_update_package", { path: p.path, signature: ctx.signature, expectedVersion: ctx.version });
  } catch (err) {
    if (relayCtx !== ctx) return;
    relayCtx = null;
    updPhase = "idle";
    $("update-modal").classList.add("hidden");
    updSetStatus("更新包签名校验未通过，已取消更新");
    invoke("log_frontend", { msg: "中继更新包验签失败：" + String((err && err.message) || err || "") }).catch(() => {});
    showToast("更新包签名校验未通过，已取消更新");
    return;
  }
  if (relayCtx !== ctx || ctx.abandoned) return;
  updPhase = "downloaded";
  $("update-title").textContent = "下载完成";
  $("update-notes").textContent = "新版本已通过蓝牙中继接收并校验通过。点「安装并重启」将自动完成安装并重新打开 BlueLink。";
  $("update-progress-wrap").classList.add("hidden");
  $("update-action").textContent = "安装并重启";
  $("update-action").disabled = false;
  $("update-cancel").textContent = "以后再说";
  $("update-cancel").disabled = false;
  updSetStatus("新版本已下载，待安装");
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
    relayCtx = null;
    presentUpdateFound(update.version, update.body, false);
  } catch (e) {
    // ① 原始错误（含插件英文原文）记进运行日志，界面只给人话版
    invoke("log_frontend", { msg: "检查更新失败，原始错误：" + String((e && e.message) || e || "") }).catch(() => {});
    // 直连失败且属网络类：转蓝牙中继分流（用户定案：有网走网络、
    // 无网有对端走中继、无网无对端直接提示）；其余失败照旧提示。
    if (isNetworkUpdError(e)) {
      await relayCheckFallback();
    } else {
      updSetStatus("检查失败，请稍后再试");
      showToast("检查更新失败：" + friendlyUpdError(e));
    }
  } finally {
    btn.disabled = false;
  }
});

// 关于·作者邮箱：双击复制到剪贴板（与消息复制同一条 navigator.clipboard
// 路径，成败都走统一 toast，不静默）
$("author-email").addEventListener("dblclick", async (e) => {
  const text = e.currentTarget.textContent.trim();
  try { await navigator.clipboard.writeText(text); showToast("邮箱已复制"); }
  catch (_) { showToast("复制失败：当前系统剪贴板不可用"); }
});

// ② 设置页「项目」地址：点开走系统浏览器（后端 open_url 只放行 http/https）
$("project-link").addEventListener("click", async (e) => {
  e.preventDefault();
  try { await invoke("open_external_url", { url: "https://github.com/123zpc/BlueLink" }); }
  catch (err) { showToast("打开失败：" + err); }
});

$("update-cancel").addEventListener("click", () => {
  if (relayCtx) {
    // 中继取消：传输已开始就真取消（走现成的传输取消命令），还在
    // 等对端下载就只置放弃标记——取包调用返回后 relayUpdateAction
    // 会发现上下文已易主、静默收尾；未落定的上下文直接清掉。
    relayCtx.abandoned = true;
    if (updPhase === "downloading" && relayCtx.transferId) {
      invoke("cancel_transfer", { transferId: relayCtx.transferId }).catch(() => {});
    }
    if (updPhase === "downloading") updSetStatus("已取消更新");
    if (updPhase === "found") updSetStatus("仅手动检查，不会自动联网");
    relayCtx = null;
    $("update-modal").classList.add("hidden");
    return;
  }
  if (updPhase === "downloading") { updAbandoned = true; updSetStatus("已取消更新"); }
  if (updPhase === "found") updSetStatus("仅手动检查，不会自动联网");
  $("update-modal").classList.add("hidden");
});

$("update-action").addEventListener("click", async () => {
  if (relayCtx) { await relayUpdateAction(); return; }
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
  // 启动预置（#10 评审补强）：显式主题先同步写下属性再走 applyTheme
  // 的窗口同步流程，避免深色偏好下首屏先亮一下浅色（窗口标题栏的
  // 同步在 applyTheme 里紧随其后、启动有闪屏页遮挡，不可感知）
  const bootTheme = localStorage.getItem("bl-theme") || "system";
  if (bootTheme !== "system") document.documentElement.setAttribute("data-theme", bootTheme);
  applyTheme(bootTheme);
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
        if (state.view === "ai") {
          // v1.0.1：在 AI 页放下 → 进 AI 文本附件队列（只收文本文件）
          const added = await stageAiFilePaths(p.paths || []);
          if (added) showToast(`已添加 ${added} 个文本附件，点发送键随问题发给 AI`);
        } else {
          const added = await stageFilePaths(p.paths || []);
          if (added) showToast(`已添加 ${added} 个文件，点发送键发送`);
        }
      } else {
        $("drop-overlay").classList.add("hidden");
      }
    });
  } catch (e) { console.error("拖拽监听注册失败", e); }
  await loadConversations();
  loadLastPeer();
  if (state.identity.connected) state.connected = true;
  // #2 入口定版（v2.6.2，用户确认）：冷启动默认停在连接页，不恢复
  // 上次页面；历史经会话列表进入聊天页时才加载（switchView 兜底）。
  switchView("connect");
  // ㉘ 主界面已就绪：立刻撤外壳并通知后端显示窗口（外壳不设最短停留，
  // 就绪多快就多快切）。ui_ready 只在进程启动这一次调用。
  $("splash").classList.add("done");
  setTimeout(() => $("splash").remove(), 300);
  invoke("ui_ready").catch(() => {});
})();


/* ================= 内置查看器 + 图片组（v2.6.0） =================
 * 查看器形态：页内全屏覆盖层（不用独立窗口）——没有第二个 WebView
 * 的冷启动与二次脚本加载，点击即出画面（秒开红线）；状态全在内存
 * 里，关闭即释放。文本解码走 WebView 自带 TextDecoder（UTF-8 严格
 * 校验失败回退 GBK），图片/媒体走 asset 协议直读本地文件，后端零
 * 新增依赖。 */

// ---------- 文件类型口径（查看器分流/图片组共用） ----------
const IMAGE_EXTS = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "avif", "ico"];
const TEXT_EXTS = new Set(["txt", "md", "log", "json", "xml", "csv", "tsv", "ini", "cfg", "conf", "yaml", "yml", "toml", "properties", "env", "java", "js", "mjs", "ts", "jsx", "tsx", "rs", "py", "go", "c", "h", "cpp", "hpp", "cc", "cs", "php", "rb", "sh", "bash", "zsh", "bat", "cmd", "ps1", "psm1", "vbs", "sql", "html", "htm", "css", "scss", "vue", "kt", "swift", "pl", "r", "lua", "diff", "patch", "dockerfile", "gitignore", "gradle", "cmake", "ms"]);
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
  // v2.6.6 #18③：图组行间距与普通消息拉齐（.img-row 的 6px 是给
  // 连续单图收紧用的，图组是独立消息、不跟着收）
  if (row) row.classList.add("group-row");
  const items = parseManifest(m.body);
  registerGroupMembers(m);
  bubble._gcount = items.length;
  bubble.innerHTML = `
    <div class="gstack">
      <img class="gstack-card c2" alt="" draggable="false">
      <img class="gstack-card c1" alt="" draggable="false">
      <div class="gstack-main">
        <img class="gstack-img hidden" alt="" draggable="false">
        <div class="gstack-ph">${iconUse("i-file", "ico ico-28")}<span class="gstack-ph-name"></span></div>
      </div>
      <span class="gstack-count" title="共 ${items.length} 张">${items.length} 张</span>
      <button class="gstack-nav prev" title="上一张"><svg class="ico ico-16"><use href="#i-chevron-left"/></svg></button>
      <button class="gstack-nav next" title="下一张"><svg class="ico ico-16"><use href="#i-chevron-right"/></svg></button>
    </div>
    `;
  bubble._gidx = 0;
  if (items.length < 2) {
    bubble.querySelectorAll(".gstack-nav").forEach((b) => b.classList.add("hidden"));
  }
  // #6 封面定尺寸（v2.6.5 重做）：监听在建气泡时就绑好（先于任何 src
  // 赋值），且每次 showCard 后若图已在缓存里（complete）立即按自然
  // 尺寸定高——旧版把监听绑在首次 showCard 设完 src 之后、且只靠
  // load 事件，缓存图/竞态下尺寸永远停在默认 240×180，容器高度与
  // 封面实际渲染对不上、气泡下方留死空白（v2.6.4 #6 实锤）。
  // v2.6.6 #18② 定框：框按组内**最大成员**一次定死——各成员按同一
  // 320 上限缩放后取宽/高最大值；成员尺寸靠预加载逐个收集（全员到齐
  // 前框只增不减、到齐即定死），切换只换图、绝不改框。
  const coverImg = bubble.querySelector(".gstack-img");
  const memberSize = new Map();
  const applyStackSize = () => {
    const stack = bubble.querySelector(".gstack");
    if (!stack || !memberSize.size) return;
    let w = 96, h = 72;
    for (const [nw, nh] of memberSize.values()) {
      const scale = Math.min(320 / nw, 320 / nh, 1);
      w = Math.max(w, Math.round(nw * scale));
      h = Math.max(h, Math.round(nh * scale));
    }
    stack.style.width = w + "px";
    stack.style.height = h + "px";
  };
  const noteMemberSize = (idx, nw, nh) => {
    if (!nw || !nh) return;
    const prev = memberSize.get(idx);
    if (prev && prev[0] === nw && prev[1] === nh) return;
    memberSize.set(idx, [nw, nh]);
    applyStackSize();
  };
  coverImg.addEventListener("load", () =>
    noteMemberSize(bubble._gidx, coverImg.naturalWidth, coverImg.naturalHeight));
  const showCard = async () => {
    const files = await groupFilesOf(m.id);
    if (!items.length) return;
    // #1 身后两层用其他照片的真实缩略图（卡牌叠放感）：c1 是第 2 张、
    // c2 是第 3 张（没有就隐藏该层）；只在文件清单变化时重设一次。
    const stack = bubble.querySelector(".gstack");
    const sig = files.map((f) => (f && f.exists ? f.path : "")).join("|");
    if (stack && bubble._gsig !== sig) {
      bubble._gsig = sig;
      const layers = [[1, ".gstack-card.c1"], [2, ".gstack-card.c2"]];
      for (const [idx, sel] of layers) {
        const el = bubble.querySelector(sel);
        const f = files[idx];
        if (el && f && f.exists && f.path) {
          try { el.src = await assetUrl(f.path); el.classList.remove("hidden"); }
          catch (_) { el.classList.add("hidden"); }
        } else if (el) {
          el.classList.add("hidden");
        }
      }
    }
    // #18② 预加载全员尺寸定框（只按文件清单签名跑一次）
    if (bubble._gsizeSig !== sig) {
      bubble._gsizeSig = sig;
      files.forEach((f, idx) => {
        if (!f || !f.exists || !f.path) return;
        const probe = new Image();
        probe.addEventListener("load", () =>
          noteMemberSize(idx, probe.naturalWidth, probe.naturalHeight));
        assetUrl(f.path).then((u) => { probe.src = u; }).catch(() => {});
      });
    }
    const i = Math.max(0, Math.min(bubble._gidx, items.length - 1));
    bubble._gidx = i;
    // #7 计数胶囊跟随当前位置（第几张/共几张，title 写全「共 N 张」）
    const countEl = bubble.querySelector(".gstack-count");
    if (countEl) countEl.textContent = `${i + 1}/${items.length}`;
    const img = bubble.querySelector(".gstack-img");
    const ph = bubble.querySelector(".gstack-ph");
    const f = files[i];
    if (f && f.exists && f.path) {
      try {
        img.src = await assetUrl(f.path);
        img.classList.remove("hidden");
        ph.classList.add("hidden");
        // 缓存图可能不再触发 load：已 complete 就立即登记尺寸定框
        if (img.complete && img.naturalWidth) noteMemberSize(i, img.naturalWidth, img.naturalHeight);
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
  // #8 重发（失败环点击与小字行重试按钮共用一条路径）
  const doResend = async () => {
    try {
      await invoke("resend_message", { messageId: m.id });
      row.remove();
      state.renderedChat.delete(m.id);
    } catch (err) { showToast("重发失败：" + err); }
  };
  if (bubble._txNote) bubble._txNote._retryBtn.addEventListener("click", doResend);
  // #8 圆环点击：传输/校验中 = 取消；失败 = 重发整组
  if (bubble._txRing) bubble._txRing.addEventListener("click", async () => {
    const mode = bubble._txRing.dataset.mode;
    if (mode === "failed") { await doResend(); return; }
    if (mode !== "progress" && mode !== "verify" && mode !== "queue") return;
    // #2：全员已终态时点取消 = 就地落定，不再对结束的传输发取消
    const gp = state.groupProgress[m.id];
    if (items.length && items.every((it) => {
      const s = gp && gp.states.get(it.msg_id);
      return s && TRANSFER_TERMINAL.includes(s);
    })) {
      const last = items[items.length - 1];
      updateGroupTransferState(m.id, last.msg_id, gp.states.get(last.msg_id), bubble);
      return;
    }
    for (const it of items) {
      const tid = state.transferMap[it.msg_id];
      if (tid) { await invoke("cancel_transfer", { transferId: tid }); return; }
    }
  });
  // #2 回放：组气泡迟建时按 groupProgress 记账补画聚合进度与终态
  // ⑥ v1.0.2：图组同病同修——全员已终态时不画聚合进度，终态用 replay
  // 口径同步落定（不闪「已完成」、不等 400ms）；仍有成员在飞时照旧
  // 画聚合进度（终态成员的已传字节已在账里，合计口径不变）。
  const gp0 = state.groupProgress[m.id];
  if (gp0) {
    // 气泡尚在构建（未进文档），逐个补画不可见；最终形态由状态回放
    // （replay 口径）落定：已完成的环直接收起、失败/取消静态呈现
    for (const [mid2, v] of gp0.members) {
      updateGroupProgress(m.id, mid2, v.received, v.total, bubble);
    }
    for (const [mid2, st] of gp0.states) updateGroupTransferState(m.id, mid2, st, bubble, true);
  }
}

function groupProg(gid) {
  if (!state.groupProgress[gid]) state.groupProgress[gid] = { members: new Map(), states: new Map() };
  return state.groupProgress[gid];
}
function groupBubbleOf(gid) {
  const row = document.querySelector(`[data-mid="${gid}"]`);
  return row ? row.querySelector(".bubble") : null;
}
/** 组进度聚合：成员各自的已传字节汇总成一条组进度。
 * bEl 可选：气泡尚在构建（未进文档）时由回放路径直接传入。 */
function updateGroupProgress(gid, memberMsgId, received, total, bEl) {
  const gp = groupProg(gid);
  gp.members.set(memberMsgId, { received, total });
  const b = bEl || groupBubbleOf(gid);
  if (!b) return;
  paintGroupRing(b, gid, false);
}
function updateGroupTransferState(gid, memberMsgId, tstate, bEl, replay) {
  const gp = groupProg(gid);
  gp.states.set(memberMsgId, tstate);
  const b = bEl || groupBubbleOf(gid);
  if (!b) return;
  if (tstate === "done") {
    // 任一成员完成即刷新组气泡（封面/已收图可能要换）
    if (b._showGroupCard) b._showGroupCard();
  }
  paintGroupRing(b, gid, replay);
}
/** 组圆环聚合绘制（#8）：进度按全组总字节；全员落定后按结果落环——
 * 全成=完成动效、有败=红环+小字说明、部分取消=环收起+小字计数+重试；
 * 有人校验且无人还在传=校验环；其余=总进度环（附已完成张数小字）。 */
function paintGroupRing(b, gid, replay) {
  const gp = groupProg(gid);
  let r = 0, t = 0;
  for (const v of gp.members.values()) { r += v.received; t += v.total; }
  // 总数以组内成员数为准（_gcount 为构建时定值；回退取 states 数，
  // 两者取大防个别成员只有进度没状态时被当不存在、提前误判落定）
  const total = Math.max(b._gcount || 0, gp.states.size);
  let done = 0, failed = 0, cancelled = 0, verifying = 0;
  for (const s of gp.states.values()) {
    if (s === "done") done++;
    else if (s === "verify_failed") failed++;
    else if (s === "cancelled") cancelled++;
    else if (s === "verifying") verifying++;
  }
  const settled = done + failed + cancelled;
  if (total > 0 && settled >= total) {
    if (failed > 0) {
      paintTxRing(b, "failed");
      paintTxNote(b, "有图片校验失败，文件不完整", true);
    } else if (done === total) {
      completeTxRing(b, replay);
      paintTxNote(b, "", false);
    } else {
      hideTxRing(b);
      paintTxNote(b, `已完成 ${done} / ${total} 张，其余已取消`, true);
    }
    return;
  }
  // 还在传 = 字节未满且状态没进校验/终态（queued 是已记账状态，
  // 别被它骗成「没在传」——有人排队时不许亮校验环，第三方评审采纳）
  const anyTransferring = [...gp.members.entries()].some(([mid, v]) => {
    if (v.received >= v.total) return false;
    const s = gp.states.get(mid);
    return s === undefined || s === "queued";
  });
  if (verifying > 0 && !anyTransferring) {
    paintTxRing(b, "verify");
  } else {
    paintTxRing(b, "progress", t ? Math.floor((r / t) * 100) : 0);
  }
  paintTxNote(b, done > 0 ? `已完成 ${done} / ${total} 张` : "", false);
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
    [state.favIds.has(m.id) ? "取消收藏" : "收藏", () => toggleMark(m.id, "favorite")],
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
  placeMenuAt(menu, x, y);
}

/** 文件气泡点击分流（v2.6.0）：文本/代码→内置文本查看器，图片→
 * 图片查看器，音视频→内置播放，PDF/Office→系统程序打开（本轮不做
 * 内置），其余与文件不存在时维持原文件菜单。 */
/** #1/#14 单张图片气泡（v2.6.2 定稿）：气泡即图片本体——透明底、
 * 零内边距、圆角 18、按原宽高比定尺寸（最大 320）；状态（发送中/
 * 校验中）用贴下沿的 4px 进度细条 + 图角小胶囊呈现，绝不撑高；
 * 校验失败时照片不展示，只给紧凑失败卡（文件名+失败+重试）。
 * 未落盘（接收中/未验）不出预览，保持文件卡原样等传输事件补挂。 */
async function attachImagePhoto(bubble, messageId) {
  if (bubble.dataset.photoDone) return;
  let info = null;
  try { info = await invoke("file_info", { messageId }); } catch (_) { return; }
  if (!info || !info.exists || !info.path) return;
  if (!isImageExt(fileExt(info.name || ""))) return;
  if (!bubble.isConnected || bubble.dataset.photoDone) return;
  bubble.dataset.photoDone = "1";
  bubble.classList.add("img-bubble");
  const row = bubble.closest(".msg-row");
  if (row) row.classList.add("img-row");
  const inner = bubble.querySelector(".file-card-inner");
  if (inner) inner.classList.add("hidden");
  // #8：气泡变图片本体后圆环退场（进度走贴底细条+胶囊，定版设计）；
  // 重试按钮留在气泡外小字行，失败时由 paintTransferState 亮出
  hideTxRing(bubble);
  const wrap = document.createElement("div");
  wrap.className = "img-wrap";
  wrap.innerHTML = `<img class="img-photo" alt="" draggable="false">
    <div class="img-strip hidden"><div></div></div>
    <span class="img-cap hidden"></span>
    <div class="img-fail hidden"><span class="img-fail-name"></span><span class="img-fail-text">校验失败，文件不完整</span></div>`;
  bubble.insertBefore(wrap, bubble.firstChild);
  bubble._imgWrap = wrap;
  wrap.querySelector(".img-fail-name").textContent = info.name || "";
  const img = wrap.querySelector(".img-photo");
  const applyImgSize = () => {
    const nw = img.naturalWidth || 4, nh = img.naturalHeight || 3;
    const scale = Math.min(320 / nw, 320 / nh, 1);
    wrap.style.width = Math.max(72, Math.round(nw * scale)) + "px";
    wrap.style.height = Math.max(54, Math.round(nh * scale)) + "px";
  };
  img.addEventListener("load", applyImgSize);
  img.addEventListener("error", () => { wrap.classList.add("img-broken"); });
  try {
    img.src = await assetUrl(info.path);
    // #6：缓存图已 complete 时 load 可能不再来，立即定尺寸防高度错位
    if (img.complete && img.naturalWidth) applyImgSize();
  } catch (_) { /* 保持占位 */ }
}

/** 旧名兼容：传输完成事件仍以 attachImagePreview 名义补挂图片。 */
async function attachImagePreview(bubble, messageId) {
  return attachImagePhoto(bubble, messageId);
}

async function openFileSmart(m, x, y) {
  let info = null;
  try { info = await invoke("file_info", { messageId: m.id }); } catch (_) { /* 忽略 */ }
  if (!info || !info.exists || !info.path) { openFileMenu(m.id, x, y); return; }
  const ext = fileExt(info.name || m.body);
  if (TEXT_EXTS.has(ext)) { openTextViewer(info, m.id); return; }
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
  viewerRenameHide();
  closeViewerFmtMenu();
}
function closeViewer() {
  viewerRenameHide();
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
// ⑱ 复制图片：复制当前显示的这一张（组内翻页跟随 IV.idx），走后端
// 原生通道（按路径读图→解码→arboard 写图），不依赖浏览器剪贴板 API
$("vi-copy").addEventListener("click", async () => {
  const it = IV.items[IV.idx];
  if (!it || !it.path) { showToast("图片文件未就绪，无法复制"); return; }
  try {
    const size = await invoke("copy_image_clipboard", { path: it.path });
    showToast(`图片已复制到剪贴板（${size}）`);
  } catch (e) { showToast(String(e)); }
});
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
  TV.degraded = false;
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
  // 严格链（v2.6.1）：整文件严格 UTF-8 → 严格 GBK → 有损兜底。
  // v2.6.0 是「严格 UTF-8 失败就直接 GBK 有损」：UTF-8 文件只要混进
  // 个别坏字节，整篇正常中文会被 GBK 误读成乱码（⑧「例」变 �� 一类）。
  // 兜底时两种有损解码都试，按替换符数量取少者（并列取 UTF-8）：
  // 坏字节只坏它自己那一两个字，不再连累全文。
  try {
    return { text: new TextDecoder("utf-8", { fatal: true }).decode(bytes), enc: "UTF-8" };
  } catch (_) { /* 不是纯 UTF-8，继续严格 GBK */ }
  try {
    return { text: new TextDecoder("gbk", { fatal: true }).decode(bytes), enc: "GBK" };
  } catch (_) { /* 也不是纯 GBK，进有损兜底 */ }
  // 有损兜底的判据不能只比替换符个数：UTF-8 文件坏一两个字节时，
  // GBK 有损解码反而可能替换符更少、却把全篇读成错字（实测翻车）。
  // 先估 UTF-8 损坏字节数：损坏很小（≤3 字节，约一个字的量级）说明
  // 本体就是 UTF-8、只坏了个别字 → UTF-8 有损；损坏大说明本体不是
  // UTF-8 → 再比两种有损解码的替换符个数，少者胜（并列取 UTF-8）。
  if (utf8DamageCount(bytes) <= 3) {
    return { text: new TextDecoder("utf-8").decode(bytes), enc: "UTF-8" };
  }
  const lossyUtf8 = new TextDecoder("utf-8").decode(bytes);
  const lossyGbk = new TextDecoder("gbk").decode(bytes);
  const badCount = (s) => (s.match(/�/g) || []).length;
  if (badCount(lossyGbk) < badCount(lossyUtf8)) {
    return { text: lossyGbk, enc: "GBK" };
  }
  return { text: lossyUtf8, enc: "UTF-8" };
}

// 估算字节流按 UTF-8 解读时的损坏字节数（不合规序列按字节计）。
// 只做损坏量级估计，不做严格校验（过长编码等细节不究）。
function utf8DamageCount(bytes) {
  let i = 0, dmg = 0;
  while (i < bytes.length) {
    const b = bytes[i];
    if (b < 0x80) { i++; continue; }
    let need = 0;
    if (b >= 0xc2 && b <= 0xdf) need = 1;
    else if (b >= 0xe0 && b <= 0xef) need = 2;
    else if (b >= 0xf0 && b <= 0xf4) need = 3;
    else { dmg++; i++; continue; }
    let ok = true;
    for (let k = 1; k <= need; k++) {
      const c = bytes[i + k];
      if (c === undefined || c < 0x80 || c > 0xbf) { ok = false; break; }
    }
    if (ok) i += need + 1; else { dmg++; i++; }
  }
  return dmg;
}

async function openTextViewer(info, msgId) {
  showViewer("text", info.name, "加载中…");
  tvReset();
  TV.name = info.name;
  // #8：记住来源消息 id——查看器改名要落到这条消息的文件身份上；
  // 没有来源消息（理论上不存在此路径）时不给改名入口
  TV.msgId = msgId || null;
  $("viewer-rename-btn").classList.toggle("hidden", !TV.msgId);
  viewerRenameHide();
  let data;
  try {
    data = await invoke("read_text_file", { path: info.path });
  } catch (e) {
    const msg = String(e);
    closeViewer();
    if (msg.startsWith("TOO_LARGE")) {
      // #9：超 100MB 的文本也不许回退系统打开（脚本类会变执行）——
      // 只在文件夹中定位，明示原因。
      showToast("文件过大（超过 100 MB），已在文件夹中定位，请用专业工具查看");
      try { await invoke("reveal_file", { path: info.path }); } catch (_) { /* 忽略 */ }
    } else {
      showToast("打开失败：" + msg);
    }
    return;
  }
  TV.degraded = !!data.degraded;
  if (TV.degraded) showToast("文件较大，已以纯文本模式打开（关闭语法高亮）");
  const dec = decodeTextBytes(base64ToBytes(data.base64));
  TV.origText = dec.text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
  TV.encoding = dec.enc;
  TV.ext = fileExt(info.name);
  TV.lang = langOfExt(TV.ext);
  // ⑨ 围栏文件（v2.6.1）：内容首行恰为 ```<lang> 时，这是从 AI 回答/
  // 文档里整段存下来的代码块——围栏是包装不是正文：去掉首尾围栏行、
  // 用围栏语言高亮。否则顶部会多出一行光秃秃的「python」，正文首行
  // （如 shebang）反而被挤下去（2026-11-21 用户实报）。
  const fence = TV.origText.match(/^```([A-Za-z0-9+#-]*)\s*\n/);
  if (fence) {
    TV.origText = TV.origText.slice(fence[0].length).replace(/\n?```[ \t]*\n?$/, "");
    TV.lang = langOfFence(fence[1]) || TV.lang;
  }
  $("viewer-sub").textContent = `${fmtSize(data.size)} · ${dec.enc}`;
  tvSetText(TV.origText);
}

// 围栏语言名 → 查看器内部高亮语言键（langOfExt 的表）；认不出返回 null，
// 调用方保留按扩展名定的语言。
function langOfFence(name) {
  const n = (name || "").toLowerCase();
  const map = {
    python: "py", py: "py",
    rust: "rust", rs: "rust",
    javascript: "js", js: "js", typescript: "js", ts: "js",
    c: "c", cpp: "c", "c++": "c", java: "c", cs: "c", php: "c", kt: "c",
    go: "go", golang: "go",
    json: "json",
    sql: "sql", magicapi: "ms", magic: "ms", magicscript: "ms", ms: "ms",
    html: "markup", xml: "markup", svg: "markup", vue: "markup",
    css: "css", scss: "css",
    sh: "shell", bash: "shell", shell: "shell", zsh: "shell",
  };
  return map[n] || null;
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
  const body = TV.degraded ? esc(TV.lines[i]) : (TV.hl.get(i) ?? esc(TV.lines[i]));
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
  sql: "select from where group by order having limit offset join left right inner outer full on union all insert into update delete create table alter drop values set and or not null as in is like between exists case when then else end distinct primary key foreign references index view trigger",
  ms: "import var let const return if else for while do break continue function true false null undefined exit try catch throw new in of this async await",
};
function langOfExt(ext) {
  if (["js", "mjs", "jsx", "ts", "tsx"].includes(ext)) return "js";
  if (ext === "rs") return "rust";
  if (ext === "py") return "py";
  if (ext === "sql") return "sql";
  if (ext === "ms") return "ms";
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
  const lineComment = lang === "py" || lang === "shell" ? "#" : lang === "sql" ? "--" : "//";
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
      // 词扫描必须认 Unicode 字母/数字：v2.6.0 及更早内层只认 ASCII \w，
      // 中文标识符/裸中文行进来后 j 一步不进、i 原地死循环把界面冻死
      // （2026-11-21 真机「所有代码文件打开都卡死」根因）。再加兜底：
      // 本分支结束时游标未前进则强制吃掉一个字符，任何输入都不许
      // 原地踏步。
      let j = i;
      while (j < n && /[\p{L}\p{N}_$]/u.test(line[j])) j++;
      if (j === i) j = i + 1;
      const word = line.slice(i, j);
      // SQL 关键字大小写不敏感（表里存小写）；其余语言按原样比
      if (kw && kw.has(lang === "sql" ? word.toLowerCase() : word)) push(word, "tk-kw");
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
// ---------- 格式化菜单（v1.0.1 合并）：按语言格式化 + 全部快捷处理 ----------
// 原三件（「格式化」钮 /「格式化处理…」下拉 /「执行」钮）合一为自绘下拉：
// 「执行」钮的唯一职责就是把下拉选中项交给 tvApplyTransform、没有独立
// 用途（核实结论），随合并撤掉；菜单点项即执行。各处理仍是显示层、
// 原文不动，行为逻辑未变，只换入口与组织。
function closeViewerFmtMenu() { $("viewer-fmt-menu").classList.add("hidden"); }
function openViewerFmtMenu() {
  const menu = $("viewer-fmt-menu");
  menu.innerHTML = "";
  const addItem = (label, fn) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.addEventListener("click", () => { closeViewerFmtMenu(); fn(); });
    menu.appendChild(b);
  };
  addItem(`按语言格式化（${TV_LANG_LABEL[TV.lang] || "通用整理"}）`, () => {
    if (!TV.origText && !TV.text) return;
    tvFormatByLang();
  });
  const sep = document.createElement("div");
  sep.className = "fmt-sep";
  menu.appendChild(sep);
  for (const [kind, label] of Object.entries(TV_FMT_LABEL)) {
    addItem(label, () => tvApplyTransform(kind));
  }
  menu.classList.remove("hidden");
}
$("viewer-fmt-btn").addEventListener("click", (e) => {
  e.stopPropagation();
  if ($("viewer-fmt-menu").classList.contains("hidden")) openViewerFmtMenu();
  else closeViewerFmtMenu();
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

// ---------- #8 查看器内改名（v2.6.6）：改名即最终身份 ----------
// 改名编辑态进出：输入组显隐与顶栏 .editing 让位类必须成对切换（样式见 style.css）
function viewerRenameShow() {
  closeViewerFmtMenu();
  $("viewer-rename-wrap").classList.remove("hidden");
  document.querySelector(".viewer-top").classList.add("editing");
}
function viewerRenameHide() {
  $("viewer-rename-wrap").classList.add("hidden");
  document.querySelector(".viewer-top").classList.remove("editing");
}
$("viewer-rename-btn").addEventListener("click", () => {
  if (!TV.msgId) return;
  const input = $("viewer-rename-input");
  viewerRenameShow();
  input.value = TV.name || "";
  input.focus();
  // 默认选中主名部分（后缀留给用户顺手改，改后缀正是本功能的一半）
  const dot = input.value.lastIndexOf(".");
  input.setSelectionRange(0, dot > 0 ? dot : input.value.length);
});
$("viewer-rename-cancel").addEventListener("click", () => {
  viewerRenameHide();
});
$("viewer-rename-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); $("viewer-rename-ok").click(); }
  if (e.key === "Escape") { e.stopPropagation(); viewerRenameHide(); }
});
$("viewer-rename-ok").addEventListener("click", async () => {
  const newName = $("viewer-rename-input").value.trim();
  if (!newName || !TV.msgId) return;
  if (newName === TV.name) { viewerRenameHide(); return; }
  try {
    const finalName = await invoke("rename_message_file", { messageId: TV.msgId, newName });
    TV.name = finalName;
    TV.ext = fileExt(finalName);
    TV.lang = langOfExt(TV.ext);
    $("viewer-title").textContent = finalName;
    viewerRenameHide();
    // 若正看着显示层变换结果，先回原文再按新语言重排高亮
    if (TV.transformed) {
      TV.transformed = null;
      $("viewer-banner").classList.add("hidden");
      tvSetText(TV.origText);
    } else {
      tvSetText(TV.text);
    }
    // 聊天气泡上的文件名同步（同一条消息可能同时在列表里）
    const fn = document.querySelector(`[data-mid="${TV.msgId}"] .file-name`);
    if (fn) fn.textContent = finalName;
    showToast(`已改名为「${finalName}」`);
  } catch (e) {
    showToast("改名失败：" + e);
  }
});

// ---------- #7 按语言格式化（v2.6.6，全轻量自研、只改显示） ----------
// 入口已并入格式化菜单（见上方 openViewerFmtMenu 第一项），此处只留实现。
const TV_LANG_LABEL = { py: "Python", js: "JavaScript", c: "Java/C 家族", sql: "SQL", ms: "MagicAPI", json: "JSON" };
function tvFormatByLang() {
  const src = TV.origText;
  const lang = TV.lang;
  try {
    let out;
    if (lang === "json") {
      try { out = JSON.stringify(JSON.parse(src), null, 2); }
      catch (_) { out = reindentCode(src, lang); } // 坏 JSON 回退括号重排
    } else if (lang === "sql") {
      out = sqlFormat(src);
    } else if (lang === "py") {
      // Python 红线：缩进即语法，只清行尾空白与空行，绝不动行首
      out = tidyText(src, false);
    } else if (lang === "js" || lang === "c" || lang === "ms" || lang === "rust" || lang === "go") {
      out = reindentCode(tidyText(src, true), lang);
    } else {
      out = tidyText(src, true);
    }
    TV.transformed = "format";
    tvSetText(out);
    $("viewer-banner-text").textContent =
      `已应用「格式化（${TV_LANG_LABEL[lang] || "通用整理"}）」——显示层处理，原文未修改`;
    $("viewer-banner").classList.remove("hidden");
    $("viewer-banner-copy").onclick = () => copyText(TV.text);
  } catch (e) {
    showToast(`格式化失败：${String((e && e.message) || e).slice(0, 80)}`);
  }
}
/** 轻量整理：去行尾空白、连续空行压到至多 1 行、去首尾空行。
 * keepIndent=false（Python）时行首原样保留；true 时顺手把行首
 * 制表符统一成 2 空格（仅限花括号语言的显示整理）。 */
function tidyText(src, keepIndent) {
  const lines = src.split("\n").map((l) => {
    let s = l.replace(/[ \t]+$/g, "");
    if (keepIndent) s = s.replace(/^\t+/, (m) => "  ".repeat(m.length));
    return s;
  });
  const out = [];
  let blank = 0;
  for (const l of lines) {
    if (l.trim() === "") { blank++; if (blank > 1) continue; out.push(""); }
    else { blank = 0; out.push(l); }
  }
  while (out.length && out[0] === "") out.shift();
  while (out.length && out[out.length - 1] === "") out.pop();
  return out.join("\n");
}
/** SQL 轻量格式化：子句关键字断行 + 主要关键字大写；引号内不动。 */
function sqlFormat(src) {
  // 先按引号切段，只在引号外做变换
  const segs = [];
  let cur = "", q = null;
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (q) {
      cur += ch;
      if (ch === q) {
        // SQL 用双写引号转义（'it''s'）：下一个还是同引号则仍在串内
        if (src[i + 1] === q) { cur += q; i++; continue; }
        segs.push({ q: true, s: cur }); cur = ""; q = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"' || ch === "`") {
      if (cur) { segs.push({ q: false, s: cur }); cur = ""; }
      q = ch; cur = ch;
      continue;
    }
    cur += ch;
  }
  if (cur) segs.push({ q: !!q, s: cur });
  const CLAUSES = ["FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "OFFSET",
    "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "OUTER JOIN", "FULL JOIN", "JOIN",
    "UNION ALL", "UNION", "VALUES", "SET", "ON"];
  const KEYWORDS = ["SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "CREATE TABLE",
    "ALTER TABLE", "DROP TABLE", "AND", "OR", "NOT", "NULL", "AS", "IN", "IS", "LIKE",
    "BETWEEN", "EXISTS", "CASE", "WHEN", "THEN", "ELSE", "END", "DISTINCT", "INTO"];
  let out = "";
  for (const seg of segs) {
    if (seg.q) { out += seg.s; continue; }
    let s = seg.s.replace(/\s+/g, " ");
    for (const c of CLAUSES) {
      // 复合 JOIN（LEFT JOIN 等）先替换成占位符，免得随后的裸
      // JOIN 正则把已断好的 LEFT JOIN 再断一次（评审补强）
      const isCompoundJoin = c.includes(" ") && c.endsWith("JOIN");
      s = s.replace(new RegExp("\\s*\\b" + c.replace(/ /g, "\\s+") + "\\b", "gi"),
        isCompoundJoin ? "\n\u0001" + c + "\u0001" : "\n" + c);
    }
    s = s.replace(/\u0001/g, "");
    for (const k of KEYWORDS) {
      s = s.replace(new RegExp("\\b" + k.replace(/ /g, "\\s+") + "\\b", "gi"), k);
    }
    out += s;
  }
  return tidyText(out, true);
}
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
/** 通用大括号缩进整理（v2.6.6 升级：计括号时跳过字符串与注释内
 * 的括号——旧版把字符串里的 } 也计数，缩进会越理越歪。仍是显示
 * 层近似处理，原文不动，看不顺眼点「查看原文」即可）。 */
function reindentCode(src, lang) {
  let depth = 0;
  let blockComment = false;
  const out = [];
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line) { out.push(""); continue; }
    const lead = /^[})\]]/.test(line) ? 1 : 0;
    out.push("  ".repeat(Math.max(0, depth - lead)) + line);
    // 净括号增量：逐字扫，字符串/行注释/块注释内的括号不算
    let delta = 0, q = null;
    for (let i = 0; i < line.length; i++) {
      const ch = line[i], nx = line[i + 1] || "";
      if (blockComment) {
        if (ch === "*" && nx === "/") { blockComment = false; i++; }
        continue;
      }
      if (q) {
        if (ch === "\\") { i++; continue; }
        if (ch === q) q = null;
        continue;
      }
      if (ch === "/" && nx === "/") break;
      if (ch === "#" && (lang === "py" || lang === "ms")) break; // # 只在 Python/MagicScript 是行注释；JS/C 等语言里 # 是私有字段/颜色值，误断会让后续括号不计数（评审补强）
      if (ch === "/" && nx === "*") { blockComment = true; i++; continue; }
      if (ch === '"' || ch === "'" || ch === "`") { q = ch; continue; }
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
