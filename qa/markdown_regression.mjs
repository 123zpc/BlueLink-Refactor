// ⑩⑫ 回归测试（v2.6.1）：AI 回复 Markdown 渲染器 md()。
// 从 ui/app.js 抽出真实的 md/mdInline 与代码块暂存（不复制代码，防漂移），
// 验证：标题/粗体/列表/分隔线/链接渲染、原始 HTML 被转义不注入、代码块
// 带语言名与复制按钮且暂存的是原始代码、流式半截围栏（未闭合）容错。
// 用法：node qa/markdown_regression.mjs（退出码 0 = 通过）
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
const escStart = src.indexOf("function esc(");
const escEnd = src.indexOf("\n}", escStart) + 2;
const mdStart = src.indexOf("const MD_CODE_BLOCKS");
const mdEnd = src.indexOf("// 代码块「复制」按钮", mdStart);
if (escStart < 0 || mdStart < 0 || mdEnd < 0) {
  console.error("抽取失败：标记缺失");
  process.exit(1);
}
const code =
  src.slice(escStart, escEnd) + "\n" + src.slice(mdStart, mdEnd);
const { md, MD_CODE_BLOCKS } = new Function(
  `${code}; return { md, MD_CODE_BLOCKS };`
)();

let fail = 0;
function check(name, cond, detail = "") {
  if (!cond) fail++;
  console.log(`${cond ? "通过" : "失败"} [${name}] ${detail}`);
}

const sample = [
  "### 结论",
  "这是**重点**，还有 `code` 片段。",
  "- 第一项",
  "- 第二项",
  "---",
  "看 [文档](https://example.com/a?x=1) 和 https://example.com/b",
  "```rust",
  "fn main() { println!(\"<hi>\"); }",
  "```",
].join("\n");
const html = md(sample);
check("标题", html.includes('md-h3') && html.includes("结论"));
check("粗体", html.includes("<b>重点</b>"));
check("行内代码", html.includes("<code>code</code>"));
check("列表", html.includes("<li>第一项</li>") && html.includes("<li>第二项</li>"));
check("分隔线", html.includes("md-hr"));
check("链接", html.includes('href="https://example.com/a?x=1"') && html.includes('href="https://example.com/b"'));
check("代码块头部", html.includes("md-code-head") && html.includes("rust") && html.includes("复制"));
check("代码被转义显示", html.includes("&lt;hi&gt;") && !html.includes('println!("<hi>")'));
const stored = [...MD_CODE_BLOCKS.values()];
check("复制暂存为原始代码", stored.length === 1 && stored[0] === 'fn main() { println!("<hi>"); }', JSON.stringify(stored));

// 原始 HTML 注入：必须被转义成文本，不产生标签
const xss = md("你好 <script>alert(1)</script> **粗**");
check("HTML 注入被转义", !xss.includes("<script>") && xss.includes("&lt;script&gt;") && xss.includes("<b>粗</b>"));

// 流式半截：未闭合围栏先按代码块渲染，不许把围栏行当正文吐出
const half = md("前言\n```python\nprint(1)\nprint(2)");
check("半截围栏容错", half.includes("md-code") && half.includes("print(2)") && !half.includes("```"));

// 流式半截粗体/行内代码：不闭合就原样显示，不许抛错或吞字
const half2 = md("这是**没闭合的粗体 和 `半截代码");
check("半截行内格式不吞字", half2.includes("没闭合的粗体") && half2.includes("半截代码"));

if (fail) {
  console.error(`共 ${fail} 条未通过`);
  process.exit(1);
}
console.log("全部通过");
