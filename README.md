# BlueLink-Refactor

BlueLink 的**完全重构专属仓库**。原仓库 [123zpc/BlueLink](https://github.com/123zpc/BlueLink) 保持原样、不再改动，所有重构工作只在本仓库进行。

**重构原则（2026-11-17 定）：这是完全重写，不是代码搬运。只保留最终功能与核心思想 — 无网蓝牙点对点聊天与文件传输；其余一律不保留，原有代码可以全部放弃。语言、技术栈、协议、界面全部重新选型，不受原版 Java / Swing / RFCOMM 实现的约束。**

## 这是什么

BlueLink 是一款无网环境下基于蓝牙的即时聊天与文件传输工具：两台电脑点对点直连，不经过云端、不经过局域网。

原版（v2.0.1-preview）只支持 Windows。本仓库的目标是把它重构成一个**跨平台、传输更快、体积更小**的版本。

## 重构目标

1. **跨平台**：支持 Windows / macOS / Linux。原版传输层直接调用 Windows Winsock（JNA 调 `ws2_32.dll`），这是单平台的根源 — 重构第一步就是把传输层抽象出来，各平台各自实现。
2. **传输提速**：流式分片传输（不再整文件读入内存再整包压缩），该压缩的才压缩，支持大文件与断点续传。
3. **架构瘦身**：去掉为 AI 聊天记忆引入的整套 Spring Boot + 内嵌 Redis，改为轻量实现；AI 功能做成可插拔模块。
4. **界面重做**：聊天界面组件化、长会话不卡顿；emoji 改为按需加载，不再打包数千张图片。
5. **体积与代码规范**：仓库与安装包瘦身，补测试、补 `.gitignore`，版本号与发布标签对齐。

## 已定选型

- **语言：Rust**（放弃 Java — 跨平台蓝牙生态对比见 [docs/DECISIONS.md](docs/DECISIONS.md)）
- **界面：Tauri 2**，按成熟桌面通讯软件的标准做，拒绝玩具感与模板感
- **传输：双通道** — BLE 负责发现/聊天/信令，文件走平台高速通道，流式分片、可续传
- **代码注释一律中文**

项目文档（按开发流程逐阶段评审确认）：
- 现状测量与分阶段方案：[docs/REFACTOR_PLAN.md](docs/REFACTOR_PLAN.md)
- 选型与产品标准：[docs/DECISIONS.md](docs/DECISIONS.md)
- 产品需求（PRD）：[docs/PRD.md](docs/PRD.md)
- 整体架构：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- 开发流程与评审关口：[docs/PROCESS.md](docs/PROCESS.md)

## 基线

- 原仓库基线提交：`807c8edb91b97bcd7826c5efb1595c699504f057`（main，2026-03-25，合并 PR #2）
- 原版规模：38 个 Java 文件 / 9,525 行；仓库 28 MB（其中 emoji 图片 15 MB）
- 原版技术栈：Java 17、Swing + FlatLaf、JNA / Winsock RFCOMM、自研二进制协议、H2、Spring Boot 3.2.5 + Spring AI 1.0.0-M6 + 内嵌 Redis

## 许可证

沿用原项目 MIT 许可证。
