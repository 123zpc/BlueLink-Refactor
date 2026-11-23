// ⑧ 回归测试（v2.6.1）：文本查看器解码严格链。
// 从 ui/app.js 抽出真实的 decodeTextBytes（不复制代码，防漂移），喂合成
// 文件：纯 GBK、UTF-8 混个别坏字节、纯 UTF-8 对照。
// 用法：node qa/decode_regression.mjs（退出码 0 = 通过）
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
const start = src.indexOf("function decodeTextBytes(");
const end = src.indexOf("\n}\n", start) + 3;
if (start < 0) {
  console.error("抽取失败：找不到 decodeTextBytes");
  process.exit(1);
}
// decodeTextBytes 依赖同文件后面的 utf8DamageCount，一并抽出
const dmgStart = src.indexOf("function utf8DamageCount(");
const dmgEnd = src.indexOf("\n}\n", dmgStart) + 3;
if (dmgStart < 0) {
  console.error("抽取失败：找不到 utf8DamageCount");
  process.exit(1);
}
const decodeTextBytes = new Function(
  `${src.slice(start, end)}\n${src.slice(dmgStart, dmgEnd)}; return decodeTextBytes;`
)();

let fail = 0;
function check(name, cond, detail) {
  if (!cond) fail++;
  console.log(`${cond ? "通过" : "失败"} [${name}] ${detail}`);
}

// 1) 纯 GBK 文件：「用法示例：中文」GBK 字节
const gbk = new Uint8Array([
  0xd3, 0xc3, 0xb7, 0xa8, 0xca, 0xbe, 0xc0, 0xfd, 0xa3, 0xba, 0xd6, 0xd0, 0xce,
  0xc4,
]);
const r1 = decodeTextBytes(gbk);
check("纯 GBK", r1.enc === "GBK" && r1.text === "用法示例：中文", `enc=${r1.enc} text=${r1.text}`);

// 2) UTF-8 文件混一个坏字节：「用法示例：测试」把「例」中间字节改坏。
//    期望：走有损兜底且选 UTF-8——坏的只有那一个字，其余完好无损。
const u8 = new TextEncoder().encode("用法示例：测试");
// 「例」是第 4 个字，前 3 个字各 3 字节，例的字节偏移 9..12，坏中间字节
u8[10] = 0xff;
const r2 = decodeTextBytes(u8);
const bad = (r2.text.match(/�/g) || []).length;
check(
  // 损坏只许落在被改坏的那一个字上（替换符 ≤3：首字节/坏字节/孤儿
  // 续字节各一个，是 WHATWG 有损解码的标准结果），其余全文必须完好
  "UTF-8 混坏字节",
  r2.enc === "UTF-8" && bad <= 3 && r2.text.includes("用法示") && r2.text.includes("：测试"),
  `enc=${r2.enc} 替换符=${bad} text=${r2.text}`
);

// 3) 纯 UTF-8 对照：严格命中、无替换符
const r3 = decodeTextBytes(new TextEncoder().encode("用法示例：中文测试"));
check("纯 UTF-8", r3.enc === "UTF-8" && r3.text === "用法示例：中文测试", `enc=${r3.enc}`);

// 4) 纯 ASCII 对照
const r4 = decodeTextBytes(new TextEncoder().encode("hello world"));
check("纯 ASCII", r4.text === "hello world", `enc=${r4.enc}`);

if (fail) {
  console.error(`共 ${fail} 条未通过`);
  process.exit(1);
}
console.log("全部通过");
