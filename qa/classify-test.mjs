/* #5 语言识别测试集（v2.6.6）：带标签片段，node 直接跑。
 * 用法：node qa/classify-test.mjs —— 输出逐条结果与总准确率；
 * 优先语言（py/java/js/sql/ms/json）片段必须全对，总准确率 ≥ 90%
 * 才算达标。判不准退 .txt 的片段期望值写 "txt"。 */
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { classifyText } = require("../ui/classify.js");

const CASES = [
  // ---------- Python ----------
  ["py", `import os
import json

def load_config(path):
    with open(path, "r") as f:
        data = json.load(f)
    if data is None:
        print("empty")
        return {}
    return data

class App:
    def __init__(self):
        self.ready = False

if __name__ == "__main__":
    cfg = load_config("a.json")
`],
  ["py", `from datetime import datetime, timedelta

async def fetch_all(urls):
    results = []
    for u in urls:
        try:
            r = await get(u)
            results.append(r)
        except Exception as e:
            print(f"failed: {e}")
    return results

flag = True
value = None
`],
  // ---------- Java（用户实报被误判为 py 的类别） ----------
  ["java", `package com.example.demo;

import java.util.ArrayList;
import java.util.List;

public class Demo {
    public static void main(String[] args) {
        List<String> names = new ArrayList<>();
        names.add("hello");
        System.out.println(names.size());
    }
}
`],
  ["java", `import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
public class UserController {
    @GetMapping("/users")
    public String list() throws Exception {
        return "ok";
    }
}
`],
  ["java", `public class Calculator {
    private int total = 0;

    public int add(int a, int b) {
        int sum = a + b;
        total += sum;
        return sum;
    }
}
`],
  // ---------- JavaScript ----------
  ["js", `const fs = require("fs");

function readAll(dir) {
  const files = fs.readdirSync(dir);
  files.forEach((f) => {
    console.log(f);
  });
  return files.length;
}

module.exports = { readAll };
`],
  ["js", `import React, { useState } from "react";

export default function Counter() {
  const [count, setCount] = useState(0);
  const el = document.getElementById("app");
  window.addEventListener("resize", () => {
    console.log("resize", count);
  });
  return count;
}
`],
  // ---------- SQL ----------
  ["sql", `SELECT u.id, u.name, COUNT(o.id) AS cnt
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
WHERE u.status = 'active'
GROUP BY u.id, u.name
ORDER BY cnt DESC
LIMIT 20;
`],
  ["sql", `INSERT INTO logs (user_id, action, created_at)
VALUES (42, 'login', NOW());

CREATE TABLE sessions (
  id BIGINT PRIMARY KEY,
  token VARCHAR(64) NOT NULL
);
`],
  // ---------- MagicAPI ----------
  ["ms", `import db;
import http;

var userId = request.getHeader("X-User-Id");
var rows = db.select("""
    select * from orders where user_id = #{userId}
""");
return response.json({
    code: 0,
    data: rows
});
`],
  ["ms", `import db;

var id = params.id::int;
var name = params.name::string;
db.table("users").insert({
    id: id,
    name: name
});
exit 200, "ok";
`],
  // MagicAPI vs JS 易混对：都有大括号与变量声明，靠裸 import 与 :: 定胜负
  ["ms", `import request;
import response;

var page = request.getParameter("page")::int;
var data = db.page("""
    select id, title from articles order by id desc
""", page, 10);
return data;
`],
  // ---------- JSON（vs JS 易混对） ----------
  ["json", `{
  "name": "bluelink",
  "version": "2.6.6",
  "private": true,
  "scripts": {
    "dev": "vite",
    "build": "vite build"
  },
  "deps": ["a", "b"]
}
`],
  ["json", `[
  {"id": 1, "name": "甲", "tags": ["x", "y"], "on": true},
  {"id": 2, "name": "乙", "tags": [], "on": false, "note": null}
]
`],
  // JS 对象字面量（带函数/未引号键）不许判成 JSON
  ["js", `const config = {
  name: "demo",
  start() {
    console.log("starting");
  },
  retries: 3,
};
export default config;
`],
  // ---------- 其余语言（通用计分） ----------
  ["rs", `use std::collections::HashMap;

fn main() {
    let mut map = HashMap::new();
    map.insert("a", 1);
    println!("size = {}", map.len());
}
`],
  ["go", `package main

import "fmt"

func main() {
    resp, err := call()
    if err != nil {
        fmt.Println("failed", err)
        return
    }
    fmt.Println(resp)
}
`],
  ["sh", `#!/bin/bash
export PATH=$HOME/bin:$PATH
for f in *.log; do
  echo "processing $f"
  chmod 644 "$f"
done
`],
  ["html", `<!DOCTYPE html>
<html>
<head><title>hi</title></head>
<body>
  <div class="wrap">
    <p>hello</p>
    <button id="ok">OK</button>
  </div>
</body>
</html>
`],
  // 短 Python（只有与 MagicAPI 同名的裸导入）不许误判成 ms
  ["py", `import os
import json
import math

print(os.getcwd())
print(json.dumps({"a": 1}))
print(math.sqrt(16))
`],
  // 短 MagicAPI（带分号裸导入 + :: 转换）
  ["ms", `import db;
var n = db.selectInt("select count(*) from users");
var id = params.id::long;
`],
  // ---------- 应判不准 → txt ----------
  ["txt", `今天天气不错，我们下午去公园散步，顺便买点水果回来。
记得把窗户关好，天气预报说晚上可能下雨。
`],
  ["txt", `hello world this is a plain sentence without any code at all
just some words in a row and nothing else really
`],
  ["txt", `a = 1
b = 2
c = a + b
`],
];

let pass = 0;
const fails = [];
for (const [expect, text] of CASES) {
  const r = classifyText(text);
  const ok = r.ext === expect;
  if (ok) pass++;
  else fails.push({ expect, got: r.ext, top: r.top, second: r.second, scores: r.scores });
  console.log(`${ok ? "✓" : "✗"} 期望 ${expect} → 得到 ${r.ext}（top=${r.top}, second=${r.second}）`);
}
const acc = ((pass / CASES.length) * 100).toFixed(1);
console.log(`\n准确率：${pass}/${CASES.length} = ${acc}%`);
if (fails.length) {
  console.log("失败明细：");
  for (const f of fails) console.log(" ", JSON.stringify(f));
}
process.exit(fails.length ? 1 : 0);
