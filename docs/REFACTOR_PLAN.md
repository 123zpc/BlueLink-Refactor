# BlueLink 重构方案

> 基线：原仓库 123zpc/BlueLink @ `807c8ed`（main）。原仓库一个字都不动，本方案只在本仓库落地。
> 现状数据均为 2026-11-17 对原仓库代码的实测，不是估算。
>
> **完全重写原则（2026-11-17 定）：只保留最终功能与核心思想（无网蓝牙点对点聊天 + 文件传输，以及后加的 AI 助手功能定位）。其余全部不保留 — 原有代码可以全部放弃，语言、技术栈、协议与界面重新选型；下文涉及原版实现之处仅作现状记录与反面参照，不构成沿用承诺。选型评估需把非 Java 方案（如 Rust / Tauri、Flutter、Kotlin Multiplatform 等跨平台栈）与 Java 方案放在同一张桌子上比，以跨平台蓝牙可用性为第一标准。**

## 一、现状测量

| 维度 | 实测 |
|---|---|
| 代码 | 38 个 Java 文件 / 9,525 行；零测试 |
| 最大类 | ModernQQFrame 1,238 行；AiChatPanel 781；MainChatPanel 738；ConnectionPanel 732 — 前四个 UI 类占全项目 1/3 以上 |
| 仓库体积 | 28 MB，其中 `src/main/resources/emojis` 3,689 张 PNG 占 15 MB；`.git` 13 MB |
| 运行时栈 | Swing 桌面程序 + Spring Boot 3.2.5 + Spring AI 1.0.0-M6（里程碑版）+ Spring Data Redis + 内嵌 Redis 服务器（端口 26379，仅为 AI 多轮聊天记忆） |
| 平台 | 仅 Windows 10/11：传输层经 JNA 直接调用 `ws2_32.dll` / `irprops.cpl` 的蓝牙 RFCOMM |
| 卫生 | `data/bluelink.mv.db`（在用的 H2 数据库文件）与 `.idea/` 被提交进 git；`.gitignore` 只有一行 `target/`；pom 版本 `1.0-SNAPSHOT` 与 v2.0.1-preview 标签不一致；`dependency-reduced-pom.xml` 为 shade 残留 |

## 二、传输层问题（优先级最高）

1. **整文件整包模型**：`sendFile` 把整个文件读入内存（`new byte[(int) file.length()]`，上限 200 MB），`ProtocolWriter.createPacket` 再把全量数据 GZIP 压缩成一个巨包，接收端整包读入（上限 300 MB）再解压。一个文件在内存中同时存在原文、压缩、成品包三到四份。
   → 改为**流式分片**：固定大小分片（如 64 KB）边读边发边写盘，内存占用恒定，文件大小不再受内存限制，并为断点续传留出分片序号。
2. **无差别压缩**：聊天短消息、AI 流式小片段、已压缩的图片/zip 都走 GZIP + CRC32。
   → 按类型与大小决定是否压缩；小包与流式片段直发。
3. **分块过小 + 逐块开销**：发送/接收缓冲 8 KB、解压缓冲 4 KB，每块一次 JNA `send` 调用并复制一份临时数组；`DataInputStream` 未加缓冲，包头每个字段都触发一次 `recv`。
   → 加大缓冲、加 `BufferedInputStream`、JNA 层用直接内存缓冲避免逐块复制。
4. **两个正确性 bug（不是优化，是必修）**：
   - `JnaSocketOutputStream.write` 不检查 `send()` 返回值，Winsock 部分发送时会被当作全部发出，造成协议错位；
   - `sendFile` 只调一次 `fis.read(fileData)` 就假设读满，不符合 `InputStream` 契约。
5. **热路径日志**：全项目 74 处 `System.out/err.println`、32 处 `printStackTrace`，协议与会话热路径上尤多。
   → 换成可关闭的分级日志，发布版默认关。
6. **无 socket 调优**：没有任何 `setsockopt`（收发缓冲等）。

经典蓝牙 EDR 的有效带宽上限约 2 Mbps 量级，协议做不到超过物理上限，但把整包压缩、内存复制、8 KB 小块和逐块日志去掉后，实际吞吐应能明显接近上限，且大文件不再受 200 MB 与内存限制。

## 三、跨平台方案（本次重构的核心决策点）

原版的 Windows 绑定有两层：① JNA → Winsock RFCOMM；② 打包用 jpackage + Inno Setup（Windows 专属）。

重构原则：**业务与协议只依赖一个传输接口，平台差异收进各自的后端实现。**

```
core/            协议（分片、校验、会话状态机）、业务逻辑 — 纯 Java，无平台 API
transport/       BluetoothTransport 接口：扫描、配对显示码、连接、收发字节流
  transport-windows/   Winsock RFCOMM（复用原版 JNA 经验）
  transport-linux/     BlueZ（D-Bus / RFCOMM socket）
  transport-macos/     IOBluetooth RFCOMM（需原生桥接，是三平台里限制最多的一家）
ui/              界面层，只依赖 core
ai/              可插拔 AI 模块（OpenAI 兼容接口），不启用时零依赖、零启动开销
```

**已定（2026-11-17）：语言换 Rust，双通道并行 — 详见 DECISIONS.md。以下为当初的权衡记录：**
- 继续 RFCOMM：与原版行为一致、吞吐较高，但 macOS 的 IOBluetooth RFCOMM 支持与签名/权限限制最多，跨平台一致性最差。
- 改走 BLE GATT：三平台（Windows WinRT、macOS CoreBluetooth、Linux BlueZ）都有官方 BLE 支持，一致性最好；代价是单连接吞吐低于 RFCOMM，且分片/MTU 协商要认真做。
- 折中：传输接口下双实现，先落地 Windows RFCOMM + Linux，macOS 与 BLE 作为同一接口的后续实现，用实测数据决定默认通道。

这个选择会影响协议分片设计，**动工前需要先定**。

## 四、架构瘦身

1. **拆掉 Spring Boot + 内嵌 Redis**：AI 多轮记忆用内存 + 本地文件（H2 已在项目里）即可；AI 改为普通 HTTP 客户端调用 OpenAI 兼容接口，流式输出自己处理。预期收益：启动从"起一个容器"变回"起一个桌面程序"，安装包与常驻内存大幅下降。
2. **拆上帝类**：ModernQQFrame（1,238 行）等按窗口/面板/状态拆分；`AppConfig`、`DatabaseManager` 的静态全局状态改为显式配置对象传递。
3. **协议与业务解耦**：`BluetoothSession` 目前靠 `"MSG"` / `"AI_REQ"` 等字符串包名做分发 — 改为带类型字段的分片协议 + 独立的会话状态机，UI 只订阅事件。
4. **补测试**：先给协议编解码、分片重组、连接码生成这类纯逻辑补单元测试，再动传输。

## 五、界面

1. 聊天记录目前每条消息都是一个真实气泡组件树，长会话组件只增不减 — 改为分页渲染 + 离屏气泡回收/复用，历史加载已有分页（每次 20 条）可保留。
2. Emoji：3,689 张 Twemoji PNG 全量打包（15 MB），且缺图时 `EmojiLoader` 还会去 CDN 下载 — 与"离线工具"定位矛盾。改为系统 emoji 字体渲染或只打包常用子集，彻底去掉联网兜底。
3. 跨平台后 UI 需保证三平台字体与 emoji 渲染一致。按完全重写原则，UI 框架不预设沿用 Swing/FlatLaf，与语言/技术栈选型一并重新评估。

## 六、分阶段落地

| 阶段 | 内容 | 验收 |
|---|---|---|
| 0 | 本仓库建立，基线与方案落档（当前阶段） | 原仓库零改动；本仓库可构建空骨架 |
| 1 | 定传输接口与分片协议 v2（含续传）；Rust 工程骨架与三端 CI | 协议文档 + 编解码单测通过；三端 CI 全绿 |
| 2 | Windows 后端流式传输实现，修 partial send / 单次 read 两个 bug | 大文件（如 1 GB）传输内存恒定、可续传 |
| 3 | 去 Spring/Redis，AI 模块化；拆上帝类与静态状态 | 启动时间、安装包体积对比基线有数 |
| 4 | Linux 后端（BlueZ）；三平台打包（jpackage 各平台原生包） | Win + Linux 实机互传通过 |
| 5 | macOS 后端；UI 组件化与 emoji 改造 | 三平台实机互传通过；长会话滚动流畅 |

## 七、红线

- 原仓库 123zpc/BlueLink 不做任何修改（不提交、不推送、不改设置）。
- 每阶段先在本仓库出可运行版本再推进下一阶段；版本号从本仓库起重新规范（与发布标签一致）。

## 八、质量标准（2026-11-17 定）

最终结果以“完美”为验收线，不是“能跑就行”：每个阶段必须有可验证的验收（见第六节表格），没有实测数据的结论不算完成；传输、启动、体积等关键指标都要与原版基线做前后对比、用数字说话；三平台都要实机验证，不拿模拟或推测代替。宁可阶段推进慢，也不交付半成品。
