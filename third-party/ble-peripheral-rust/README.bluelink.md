# BlueLink vendored 补丁说明

本目录是 crates.io 上 `ble-peripheral-rust 0.2.0`（MIT）的原样副本，仅按下述
范围打了防御补丁；工作区根 `Cargo.toml` 用 `[patch.crates-io]` 指向这里。
升级该库时先对照本说明把补丁迁移过去，不要直接删目录。

## 为什么打补丁（2026-11-20，真机实证）

用户真机实测：本机用 BLE 连接对方时，**对方的 BlueLink 直接闪退、进程死亡**，
本机侧则表现为一直连不上（对端中途死亡后干等）。

根因定位：该库 Windows 后端的事件处理器（`src/peripheral/winrt/win_event_handler.rs`）
是 WinRT 经 FFI 回调的 `TypedEventHandler` 闭包，运行在系统线程上，其中对大量
**可失败**的 WinRT 调用直接 `.unwrap()`：

- 订阅变更：`SubscribedClients()`、`client.Session()`；
- 读请求：`GetRequestAsync()`、`Session()`、`RespondWithValue()` 等；
- 写请求：`GetRequestAsync()`、`request.Value()`、`request.Respond()` 等；
- 另有 `peripheral_manager.rs` 的 `update_characteristic` 用
  `.expect("Characteristic not found")`，`win_utils.rs` 的
  `to_uuid` / `buffer_to_vec` / `vec_to_buffer` 也全部 unwrap。

对端在订阅/读写过程中断开、GATT 会话正在拆除时，上述调用都会正常返回 Err；
原版一 unwrap 就 panic，而 panic 穿越 FFI 边界会让**整个进程 abort**——与真机
「闪退、无日志、进程直接没了」的表现完全吻合。

应用自身代码（transport 的服务端事件循环、services 握手/会话、protocol 解码）
已逐行审计，均为 Result 式防御处理，不是崩溃源。

## 双机日志证据链（2026-11-20 回传，bluelink-2026-11-20.log + 对端日志）

- 发起端两次点连内置 BLE：步骤 1–4 全过、Hello 发出，然后无下文（对端回的
  HelloAck 永不到达；发起侧握手 20 秒超时旧版只回界面不落日志——已在
  services attach 补「连接失败于步骤5/6：握手超时」定案行）。
- 对端两次都记：连入订阅 → 收到 Hello → 「连接码校验通过，已回 HelloAck，
  握手完成」，随后分别在 15 秒、42 秒内**无声死亡**（无断开/关窗行，日志直接
  跳到下一次节点启动）。同实例里经典蓝牙（两次握手完成）与 SimpleBLE
  （全程健康、干净断开）都没杀进程——凶手锁定内置 BLE 服务端。
- 「已回 HelloAck」只是服务层把帧写进内存通道的时刻，真正发送在 transport
  的通知任务里调本库 `update_characteristic → NotifyValueAsync`：旧代码
  发送失败只 break 不记日志、任务被 vec_to_buffer 的 unwrap panic 杀掉更是
  无声无息，与「对端自以为发出、本机永收不到」严丝合缝；发起端放弃连接后
  对端拆除会话时，订阅/读写 handler 的 unwrap 任一失败即 panic 穿越 FFI
  abort 进程，与「会话后数十秒无声死亡」吻合。
- v2.5.1 配套硬化（应用侧）：通知发送加 5 秒超时 + 失败/超时定案行
  （essential）+ 首帧发出日志（verbose），残余问题下次可凭日志定案。

## 补丁范围（仅此三处，API 与事件语义不变）

1. `src/peripheral/winrt/win_event_handler.rs`：四个处理器（电台、广播状态、
   订阅、读、写）内的 unwrap 全部改为防御式处理——WinRT 调用失败时记
   `log::error` 并提前返回 `Ok(())`；`connected_clients` 的 RwLock 毒化时
   取回内部数据继续用，不因锁 panic。
2. `src/peripheral/winrt/win_utils.rs`：`to_uuid` 解析失败回退 nil UUID；
   `buffer_to_vec` 失败回退空数据；`vec_to_buffer` 失败回退空 buffer。
3. `src/peripheral/winrt/peripheral_manager.rs`：`update_characteristic`
   的 expect 改为返回 `Err`，由调用方按发送失败处理。

## 未覆盖的同类风险（如实记录）

- macOS 后端 `src/peripheral/corebluetooth/peripheral_delegate.rs` 也有
  `characteristic.service().unwrap()` 一类写法（本次真机问题在 Windows，
  且无 macOS 真机证据，未动；后续若上 macOS 真机需同口径处理）。
- 库内部其他平台（bluez）路径未审计，本产品 Windows 为先。
