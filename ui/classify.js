/* BlueLink 文档语言识别（v2.6.6，#5 重写）：全文强特征权重打分。
 *
 * 背景：旧版 detectDocument 只看前几行猜类型，Java 代码被当成
 * Python 发出（用户真机实报）。用户定案：按全文强特征打分——
 * 强特征高权重、弱特征低权重，逐行单遍扫描累加；最高分语言胜出；
 * 最高分不足门槛、或第一名与第二名太接近（易混淆）时宁可退回
 * .txt，也不许给错后缀。识别不准时用户可在查看器里手动改名改
 * 后缀（#8），那是最终身份。
 *
 * 本文件是纯函数、无 DOM 依赖：浏览器里经 <script> 先于 app.js
 * 加载（挂到 window.Classify），node 里可直接 require 做测试集
 * 回归（qa/classify-test.mjs）。
 */
(function (global) {
  "use strict";

  // 语言注册表：后缀、展示名。顺序即平局时的优先序（不依赖它定胜负，
  // 只用于稳定排序）。
  var LANGS = [
    { ext: "py", name: "Python" },
    { ext: "java", name: "Java" },
    { ext: "js", name: "JavaScript" },
    { ext: "sql", name: "SQL" },
    { ext: "ms", name: "MagicAPI" },
    { ext: "json", name: "JSON" },
    { ext: "rs", name: "Rust" },
    { ext: "go", name: "Go" },
    { ext: "c", name: "C" },
    { ext: "cpp", name: "C++" },
    { ext: "cs", name: "C#" },
    { ext: "sh", name: "Shell" },
    { ext: "html", name: "HTML" },
    { ext: "css", name: "CSS" },
    { ext: "md", name: "Markdown" },
  ];

  // 计分门槛：最高分 < MIN_SCORE 或与次名差距 < MIN_MARGIN（且次名
  // 分数不低）→ 判不准，退 .txt。
  var MIN_SCORE = 6;
  var MIN_MARGIN = 4;
  var MARGIN_RATIO = 2; // 或最高分 ≥ 次名 × 2 也算拉开

  function add(scores, ext, n) {
    scores[ext] = (scores[ext] || 0) + n;
  }

  // 逐行强特征扫描。每条规则命中一次计一次分（同一行同一规则只计
  // 一次），特征按「有多专属」定权重：专属语法 8–10 分、较强信号
  // 5 分、中信号 2–3 分、弱信号 1 分。
  function scoreLines(lines) {
    var s = {};
    var i, line, t;
    for (i = 0; i < lines.length; i++) {
      line = lines[i];
      t = line.trim();
      if (!t) continue;

      /* ---------- MagicAPI（MagicScript）专属强特征 ---------- */
      // 裸模块导入：import db; —— 无引号、无 from。带分号是
      // MagicAPI 的 canonical 写法（JS 的裸导入必须带引号、Java
      // 没有这种形式），权重最高；不带分号时只认 MagicAPI 专属
      // 模块名——os/json/math 等与 Python 标准库同名，无分号一律
      // 不给分（防把短 Python 片段误判成 MagicAPI，测试集有对子）。
      if (/^import\s+(db|http|request|response|log|magic|env|config|file|os|thread|math|date|json|redis|mongo|es|kafka|ws)\s*;$/.test(t)) add(s, "ms", 10);
      else if (/^import\s+(db|http|request|response|magic)\s*$/.test(t)) add(s, "ms", 8);
      // :: 类型转换（value::int / x::string / d::date）
      if (/::\s*(int|long|double|float|string|date|bool|boolean|byte|short|decimal|bigint)\b/.test(line)) add(s, "ms", 8);
      // db.模块.方法( 链式调用
      if (/\bdb\.(select|selectOne|selectInt|selectValue|page|insert|update|delete|count|exists|transaction|cache|table)\s*\(/.test(line)) add(s, "ms", 5);
      if (/^import\s+['"][^'"]+['"]\s+as\s+\w+\s*;?$/.test(t)) add(s, "ms", 5); // import 'xxx' as y;
      if (/\bresponse\.json\s*\(|\brequest\.getHeader\s*\(|\bmagic\.invoke\s*\(/.test(line)) add(s, "ms", 4);
      if (/^exit\s+/.test(t)) add(s, "ms", 2); // MagicScript 的 exit 返回值

      /* ---------- Python ---------- */
      if (/^#!.*\bpython[0-9.]*\b/.test(t)) add(s, "py", 10);
      if (/^def\s+\w+\s*\(.*\)\s*:$/.test(t)) add(s, "py", 6);
      if (/^class\s+\w+(\s*\(.*\))?\s*:$/.test(t)) add(s, "py", 5);
      if (/^elif\b/.test(t)) add(s, "py", 6);
      if (/^import\s+(os|sys|re|json|time|datetime|random|math|collections|itertools|functools|pathlib|typing|asyncio|requests|numpy|pandas)\b/.test(t)) add(s, "py", 5);
      if (/^from\s+[\w.]+\s+import\s+/.test(t)) add(s, "py", 4);
      if (/^print\s*\(/.test(t)) add(s, "py", 3);
      if (/\bself\.\w+/.test(line)) add(s, "py", 2);
      if (/^\s*(if|for|while|try|with)\b.*:\s*(#.*)?$/.test(line) && !/[;{}]\s*$/.test(t)) add(s, "py", 1);
      if (/^(async\s+def|await\s+\w)/.test(t)) add(s, "py", 3);
      if (/\b(True|False|None)\b/.test(line)) add(s, "py", 2);
      if (/^@\w+/.test(t)) add(s, "py", 2); // 装饰器（Java 注解多在类/方法签名行尾随类型，行首 @ 更偏 Python/JS）

      /* ---------- Java ---------- */
      if (/^package\s+[\w.]+\s*;/.test(t)) add(s, "java", 8);
      if (/^import\s+(java|javax|jakarta|org\.springframework|com\.)[\w.]*\s*;/.test(t)) add(s, "java", 8);
      if (/\bpublic\s+(static\s+)?(final\s+)?class\s+\w+/.test(line)) add(s, "java", 7);
      if (/\bpublic\s+static\s+void\s+main\s*\(/.test(line)) add(s, "java", 8);
      if (/\bSystem\.out\.println\s*\(/.test(line)) add(s, "java", 7);
      if (/\bprivate\s+(static\s+)?[\w<>\[\]]+\s+\w+\s*[;(=]/.test(line)) add(s, "java", 4);
      if (/^@(Override|Autowired|Service|Component|RestController|RequestMapping|GetMapping|PostMapping|Entity|Table|Column|Test)\b/.test(t)) add(s, "java", 6);
      if (/\b(String|Integer|Long|Boolean|Double|List|ArrayList|HashMap|Optional)\s+\w+\s*=/.test(line)) add(s, "java", 3);
      if (/\bthrows\s+\w*Exception\b/.test(line)) add(s, "java", 4);
      if (/^import\s+\w[\w.]*\.[\w.]+\s*;$/.test(t)) add(s, "java", 3); // 带分号且包名带点的普通 import（MagicAPI 的 import db; 无点，不给 Java 分——评审补强）

      /* ---------- JavaScript / TypeScript ---------- */
      if (/\bconsole\.log\s*\(/.test(line)) add(s, "js", 6);
      if (/^import\s+.*\bfrom\s+['"][^'"]+['"]\s*;?$/.test(t)) add(s, "js", 6);
      if (/^import\s+['"][^'"]+['"]\s*;?$/.test(t)) add(s, "js", 4);
      if (/^const\s+\w+\s*=\s*require\s*\(/.test(t)) add(s, "js", 7);
      if (/\bmodule\.exports\b|\bexports\.\w+\s*=/.test(line)) add(s, "js", 6);
      if (/\b(const|let|var)\s+\w+\s*=/.test(line)) add(s, "js", 2);
      if (/=>\s*\{/.test(line) || /=>\s*\w/.test(line)) add(s, "js", 2);
      if (/\bdocument\.(getElementById|querySelector|addEventListener)\b/.test(line)) add(s, "js", 6);
      if (/\bwindow\.\w+/.test(line)) add(s, "js", 3);
      if (/^export\s+(default\s+)?(function|const|class|\{)/.test(t)) add(s, "js", 5);
      if (/\basync\s+function\b|\bawait\s+\w+\s*\(/.test(line)) add(s, "js", 2);
      if (/\b(interface|enum)\s+\w+\s*\{/.test(line)) add(s, "js", 2); // TS 倾向（仍归 .js 家族处理）

      /* ---------- SQL ---------- */
      if (/^\s*(SELECT|INSERT\s+INTO|UPDATE\s+\w+\s+SET|DELETE\s+FROM|CREATE\s+TABLE|ALTER\s+TABLE|DROP\s+TABLE)\b/i.test(line)) add(s, "sql", 7);
      if (/\bFROM\s+[\w."]+\b/i.test(line) && /\b(WHERE|JOIN|GROUP\s+BY|ORDER\s+BY|LIMIT)\b/i.test(line)) add(s, "sql", 4);
      if (/^\s*(WHERE|GROUP\s+BY|ORDER\s+BY|HAVING|LIMIT|JOIN|LEFT\s+JOIN|INNER\s+JOIN|ON)\b/i.test(line)) add(s, "sql", 3);
      if (/;\s*$/.test(t) && /\b(SELECT|FROM|WHERE)\b/i.test(line)) add(s, "sql", 2);

      /* ---------- Rust ---------- */
      if (/\bfn\s+(main|new)\s*\(/.test(line)) add(s, "rs", 4);
      if (/\blet\s+mut\s+\w+/.test(line)) add(s, "rs", 4);
      if (/\bprintln!\s*\(/.test(line)) add(s, "rs", 7);
      if (/^use\s+[\w:]+::/.test(t)) add(s, "rs", 4);
      if (/\bimpl\s+\w+\s+for\s+\w+/.test(line)) add(s, "rs", 6);
      if (/^pub\s+(fn|struct|enum|trait)\b/.test(t)) add(s, "rs", 5);

      /* ---------- Go ---------- */
      if (/^package\s+main\s*$/.test(t)) add(s, "go", 6);
      if (/\bfmt\.Print(ln|f)?\s*\(/.test(line)) add(s, "go", 6);
      if (/^func\s+(\(\w+\s+\*?\w+\)\s+)?\w+\s*\(/.test(t)) add(s, "go", 4);
      if (/\berr\s*!=\s*nil\b/.test(line)) add(s, "go", 5);
      if (/^import\s*\(\s*$/.test(t)) add(s, "go", 3);

      /* ---------- C / C++ / C# ---------- */
      if (/^#include\s*[<"]/.test(t)) { add(s, "c", 5); add(s, "cpp", 5); }
      if (/\bprintf\s*\(/.test(line)) add(s, "c", 4);
      if (/\bstd::(cout|cin|vector|string|endl)\b/.test(line)) add(s, "cpp", 7);
      if (/^using\s+System\s*;/.test(t)) add(s, "cs", 8);
      if (/\bConsole\.WriteLine\s*\(/.test(line)) add(s, "cs", 7);
      if (/\bnamespace\s+[\w.]+\s*\{/.test(line)) add(s, "cs", 4);

      /* ---------- Shell ---------- */
      if (/^#!.*\b(bash|sh|zsh)\b/.test(t)) { add(s, "sh", 10); }
      if (/^(echo|export|source|chmod|mkdir|cd|ls|cp|mv|rm|curl|wget|sudo|apt|brew|npm|yarn|git|docker|kubectl)\s+/.test(t)) add(s, "sh", 1);
      if (/\$\{\w+\}|\$\w+/.test(line) && /(=|echo|if|then)/.test(line)) add(s, "sh", 1);

      /* ---------- HTML / CSS / Markdown ---------- */
      if (/^\s*<!DOCTYPE\s+html/i.test(line) || /^\s*<\/?(html|head|body|div|span|p|a|ul|li|table|form|input|button|script|style|section|header|footer|nav|main)\b/i.test(line)) add(s, "html", 5);
      if (/^\s*[.#]?[\w-]+\s*\{\s*$/.test(line) && lines.length > 2) add(s, "css", 2);
      if (/^\s*[\w-]+\s*:\s*[^;]+;\s*$/.test(line)) add(s, "css", 1);
      if (/^#{1,6}\s+\S/.test(t)) add(s, "md", 3);
      if (/^\s*[-*]\s+\[[ x]\]\s+/.test(line)) add(s, "md", 4);
    }
    return s;
  }

  // JSON 先行判定：整体能 parse 且顶层是对象/数组 → JSON（置信最高，
  // 不进打分池——JSON 与 JS 字面量太像，必须先摘出来）。
  function tryJson(text) {
    var t = text.trim();
    if (t.length < 2) return false;
    var first = t.charAt(0);
    if (first !== "{" && first !== "[") return false;
    try {
      var v = JSON.parse(t);
      return v !== null && typeof v === "object";
    } catch (e) {
      return false;
    }
  }

  /**
   * 识别一段文本的语言。
   * @param {string} text 全文
   * @returns {{ext: string, name: string, confident: boolean,
   *            scores: Object, top: number, second: number}}
   *   confident=false 时 ext 恒为 "txt"（判不准、宁缺勿错）。
   */
  function classifyText(text) {
    var plain = { ext: "txt", name: "纯文本", confident: false, scores: {}, top: 0, second: 0 };
    if (!text || !text.trim()) return plain;
    // 围栏语言标记在粘贴入口已处理（app.js），这里只认内容本身。
    if (tryJson(text)) {
      return { ext: "json", name: "JSON", confident: true, scores: { json: 99 }, top: 99, second: 0 };
    }
    var lines = text.split("\n");
    // 超大文本采样打分：语言在文件内均匀分布，采样不改结论，却能
    // 把 30 万行级粘贴的判定压回毫秒级（发送路径有 ≤1 秒硬线）。
    if (lines.length > 12000) {
      lines = lines.slice(0, 10000).concat(lines.slice(-2000));
    }
    var scores = scoreLines(lines);
    var ranked = LANGS.map(function (l) {
      return { ext: l.ext, name: l.name, score: scores[l.ext] || 0 };
    }).sort(function (a, b) { return b.score - a.score; });
    var top = ranked[0], second = ranked[1] || { key: null, score: 0 };
    var confident =
      top.score >= MIN_SCORE &&
      (top.score - second.score >= MIN_MARGIN || top.score >= second.score * MARGIN_RATIO);
    if (!confident) {
      return { ext: "txt", name: "纯文本", confident: false, scores: scores, top: top.score, second: second.score };
    }
    return { ext: top.ext, name: top.name, confident: true, scores: scores, top: top.score, second: second.score };
  }

  var api = {
    classifyText: classifyText,
    _scoreLines: scoreLines,
    _tryJson: tryJson,
    LANGS: LANGS,
  };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  global.Classify = api;
})(typeof window !== "undefined" ? window : globalThis);
