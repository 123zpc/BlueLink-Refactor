<div align="center">

<img src="app/icons/128x128.png" width="96" alt="BlueLink" />

# BlueLink

**两台电脑，不联网、不经过任何服务器，用蓝牙直接聊天和传文件。**

[![Release](https://img.shields.io/github/v/release/123zpc/BlueLink-Refactor?style=flat-square)](https://github.com/123zpc/BlueLink-Refactor/releases)
[![License](https://img.shields.io/github/license/123zpc/BlueLink-Refactor?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue?style=flat-square)](#下载与安装)
[![Rust](https://img.shields.io/badge/built%20with-Rust%20%2B%20Tauri%202-orange?style=flat-square)](#架构)

中文 | [English](README.en.md)

</div>

---

BlueLink 是一款点对点的桌面通讯工具：没有账号、没有云端，两台电脑配对后即可互发消息、图片和文件，断网、内网、隔离环境都能用。本项目是 [BlueLink](https://github.com/123zpc/BlueLink) 的 Rust 重构优化版。

## 功能

- **蓝牙直连聊天**：文字、图片、文件，开箱即用；首次配对后，后续自动直连
- **文件传输**：流式分片发送，大文件不占内存；文本类自动压缩，支持断点续传和完整性校验
- **彩签分类**：9 种颜色就是 9 个分类，给消息选色即归类，可改名、按分类筛选回看
- **收藏与置顶**：重要消息一键收藏、置顶，随时定位
- **剪贴板同步**：在一台电脑复制，另一台直接粘贴（文字与图片）
- **内置查看器**：代码和长文在应用内打开，语法高亮、代码格式化，文件名可手动修正
- **AI 助手**：接入你自己的 AI 服务（OpenAI 兼容接口），聊天中可 @AI 提问、把消息发给 AI 解读；问答只保存在本机，不发给对方
- **本地 API**：内置 OpenAI 兼容的本地接口（仅本机回环地址），可供其他软件调用你配置的 AI；密钥只保存在本机，永不经过蓝牙传输
- **三种传输**：经典蓝牙（默认）、BLE、SimpleBLE，按环境任选
- **深色 / 浅色主题**，关闭窗口缩到系统托盘后台待命

## 下载与安装

到本仓库的 [Releases](https://github.com/123zpc/BlueLink-Refactor/releases) 页面下载对应系统的安装包：

| 系统 | 安装包 |
| --- | --- |
| Windows 10 / 11 | `.exe` 安装程序或 `.msi` |
| macOS（Apple Silicon） | `.dmg` |
| Linux | `.deb` / `.rpm` / `.AppImage` |

安装包未做商业签名，Windows 或 macOS 首次运行时可能出现安全提示，选择继续运行即可。应用内可手动检查更新。

## 使用

1. 两台电脑都打开蓝牙并启动 BlueLink。
2. 在一台电脑上进入连接页，选择对方设备（或输入对方的连接码）。
3. 首次连接时按系统提示完成配对，之后打开即可自动重连。
4. 连接成功后即可聊天、发图片和文件；接收的文件按日期保存在下载目录中。

## 系统要求

- 带蓝牙的电脑（经典蓝牙或 BLE 均可）
- Windows 10 / 11、macOS、Linux 主流发行版

## 架构

无服务器的点对点结构，两台电脑直连、没有任何中转。代码按层拆分，上层只依赖下层的接口：

```
app/            应用外壳：窗口、托盘、本地 API 服务、系统平台适配
ui/             界面层：页面呈现与用户交互（原生 JS，无框架）
crates/
├─ domain/      领域层：消息、会话、设置等核心数据模型
├─ transport/   传输层：经典蓝牙 / BLE / SimpleBLE，对上只交付一条字节流
├─ protocol/    协议层：分片编号、校验、乱序重组、zstd 压缩、断点续传
├─ storage/     存储层：SQLite 持久化，聊天记录与设置落库
├─ ai/          AI 层：模型接入与流式转发，可插拔
└─ services/    业务层：连接、会话、文件、剪贴板同步的业务编排
tools/node-cli/ 测试节点：无界面对端，用于双端联调与自动验证
docs/           文档：设计文档与发布说明
```

关键设计就三条：传输与协议解耦，换传输方式不影响上层，单个消息或文件失败绝不连累连接；可靠性由协议层兜底，乱序能重组、损坏能重发；AI 密钥只存本机、永不经过蓝牙，全部数据只存本地、不上传。

技术栈：Rust + Tauri 2。

## 许可证

MIT

---

<div align="center">

作者：ZPC · [github.com/123zpc](https://github.com/123zpc)

</div>
