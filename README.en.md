<div align="center">

<img src="app/icons/128x128.png" width="96" alt="BlueLink" />

# BlueLink

**Two computers — no internet, no server in between — chatting and sending files directly over Bluetooth.**

[![Release](https://img.shields.io/github/v/release/123zpc/BlueLink-Refactor?style=flat-square)](https://github.com/123zpc/BlueLink-Refactor/releases)
[![License](https://img.shields.io/github/license/123zpc/BlueLink-Refactor?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue?style=flat-square)](#download--install)
[![Rust](https://img.shields.io/badge/built%20with-Rust%20%2B%20Tauri%202-orange?style=flat-square)](#architecture)

[中文](README.md) | English

</div>

---

BlueLink is a peer-to-peer desktop messaging tool: no accounts, no cloud. Pair two computers and exchange messages, images, and files; it works offline, on internal networks, and in isolated environments. This project is a Rust rewrite of [BlueLink](https://github.com/123zpc/BlueLink), rebuilt and optimized.

## Features

- **Direct Bluetooth chat**: text, images, and files out of the box; after the first pairing, later connections are automatic
- **File transfer**: streamed in chunks, so large files don't eat memory; text-like content is compressed automatically, with resumable transfers and integrity checks
- **Color tags**: 9 colors are 9 categories — pick a color to file a message, rename categories, filter by them later
- **Favorites and pins**: save and pin important messages, jump back to them anytime
- **Clipboard sync**: copy on one computer, paste on the other (text and images)
- **Built-in viewer**: open code and long text in-app, with syntax highlighting and formatting; file names can be corrected by hand
- **AI assistant**: connect your own AI service (OpenAI-compatible API), ask via `@AI` in chat or send a message to AI for explanation; Q&A is stored only on your machine, never sent to the peer
- **Local API**: a built-in OpenAI-compatible endpoint bound to 127.0.0.1 only, so other software on your machine can use your configured AI; keys stay on your machine and never travel over Bluetooth
- **Three transports**: Classic Bluetooth (default), BLE, and SimpleBLE — pick what fits your environment
- **Dark / light themes**; closing the window keeps BlueLink running in the system tray

## Download & Install

Get the package for your system from this repository's [Releases](https://github.com/123zpc/BlueLink-Refactor/releases) page:

| System | Package |
| --- | --- |
| Windows 10 / 11 | `.exe` installer or `.msi` |
| macOS (Apple Silicon) | `.dmg` |
| Linux | `.deb` / `.rpm` / `.AppImage` |

The packages are not commercially code-signed, so Windows or macOS may show a security prompt on first launch — choose to run it anyway. Updates can be checked manually in the app.

## Usage

1. Turn on Bluetooth and start BlueLink on both computers.
2. On one computer, open the connection page and pick the other device (or enter its connection code).
3. Follow the system prompt to pair on first connection; after that, BlueLink reconnects automatically.
4. Once connected, chat and send images and files; received files are saved by date in the downloads folder.

## System Requirements

- A computer with Bluetooth (Classic Bluetooth or BLE)
- Windows 10 / 11, macOS, or a mainstream Linux distribution

## Architecture

A serverless peer-to-peer design: two computers connect directly, with nothing in the middle. The code is split into layers, and each layer only depends on the interfaces of the one below it:

```
app/            App shell: window, tray, local API service, platform integration
ui/             Interface layer: rendering and user interaction (vanilla JS, no framework)
crates/
├─ domain/      Domain layer: core data models for messages, conversations, and settings
├─ transport/   Transport layer: Classic Bluetooth / BLE / SimpleBLE, exposing a single byte stream
├─ protocol/    Protocol layer: chunk numbering, checksums, out-of-order reassembly, zstd compression, resumable transfers
├─ storage/     Storage layer: SQLite persistence for chat history and settings
├─ ai/          AI layer: model access and streaming relay, pluggable
└─ services/    Service layer: orchestration for connections, conversations, files, and clipboard sync
tools/node-cli/ Test node: a headless peer for dual-end testing and automated verification
docs/           Documentation: design docs and release notes
```

Three key design choices: transport and protocol are decoupled, so swapping the Bluetooth transport never touches the layers above, and a failed message or file never takes the connection down with it; reliability is guaranteed by the protocol layer, where out-of-order chunks are reassembled and damaged ones resent; and AI keys live only on your own machine, never cross the Bluetooth link, and all data stays local — nothing is uploaded anywhere.

Tech stack: Rust + Tauri 2.

## License

MIT

---

<div align="center">

Author: ZPC · [github.com/123zpc](https://github.com/123zpc)

</div>
