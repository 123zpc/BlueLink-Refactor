// ⑪ 回归测试（v2.6.1）：查看器高亮 tokenizer 死循环。
// 从 ui/app.js 抽出真实的 tvTokenizeLine 及其依赖（TV_KW / esc /
// tvTokenizeJson / tvTokenizeMarkup，不复制代码，防漂移），喂 v2.6.0
// 会冻死界面的毒行：中文标识符、裸中文行、CSS 中文字体名、SQL 中文
// 注释。每行必须毫秒级返回。
// 用法：node qa/tokenizer_regression.mjs（退出码 0 = 通过）
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";

const src = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
const kwStart = src.indexOf("const TV_KW = {");
const endMark = src.indexOf("// ---------- 选择/复制", kwStart);
const escStart = src.indexOf("function esc(");
const escEnd = src.indexOf("\n}", escStart) + 2;
if (kwStart < 0 || endMark < 0 || escStart < 0) {
  console.error("抽取失败：标记缺失（TV_KW / 选择复制节 / esc）");
  process.exit(1);
}
const code =
  src.slice(escStart, escEnd) + "\n" + src.slice(kwStart, endMark);
const tvTokenizeLine = new Function(`${code}; return tvTokenizeLine;`)();

const cases = [
  ["rust 中文标识符", "let 变量 = 计算总价(单价, 数量);", "rust"],
  ["裸中文行（无语言）", "用法示例：直接双击打开即可", "text"],
  ["裸中文行（py 高亮）", "用法示例：直接双击打开即可", "py"],
  ["python 中文注释行", "# 这是中文注释，说明用途", "py"],
  ["css 中文字体名", "font-family: \"微软雅黑\", sans-serif;", "css"],
  ["sql 中文注释（c 系）", "-- 查询用户表（中文注释）", "c"],
  ["中文字符串混排", "let s = \"你好世界\"; let 计数 = 0;", "rust"],
  ["纯英文对照", "fn main() { let x = compute(1, 2); }", "rust"],
  ["空行", "", "rust"],
  ["只有符号", "} else { /* 完 */ }", "c"],
  ["json 中文键值", "{\"名称\": \"测试\", \"值\": 42}", "json"],
  ["markup 中文文本", "<div class=\"甲\">中文内容</div>", "markup"],
];

let fail = 0;
for (const [name, line, lang] of cases) {
  const t0 = performance.now();
  const r = tvTokenizeLine(line, lang, {});
  const ms = performance.now() - t0;
  const ok = ms < 100 && r && typeof r.html === "string";
  if (!ok) fail++;
  console.log(
    `${ok ? "通过" : "失败"} [${name}] ${ms.toFixed(2)}ms html=${r ? r.html.length : -1} 字符`
  );
}
if (fail) {
  console.error(`共 ${fail} 条未通过`);
  process.exit(1);
}
console.log("全部通过：无死循环、无超时");
