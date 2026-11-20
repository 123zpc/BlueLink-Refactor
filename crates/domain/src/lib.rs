//! 领域层：纯数据类型与规则，不依赖任何基础设施。
//! 标签、收藏、置顶等"消息标记"与消息定位所需的核心类型都在这里定义。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 生成一个新的唯一 ID（消息、会话、传输等共用）。
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 当前时间的 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 由设备 ID 派生 6 位连接码（详细设计 1.3）：SHA-256 前 8 字节取模 1,000,000。
pub fn connection_code(device_id: &str) -> String {
    let digest = Sha256::digest(device_id.as_bytes());
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&digest[..8]);
    let n = u64::from_be_bytes(buf) % 1_000_000;
    format!("{n:06}")
}

/// 连接码的展示格式：3+3 分组，如 "123 456"。
pub fn format_code(code: &str) -> String {
    if code.len() == 6 {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// 本机/对端设备信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub device_id: String,
    pub name: String,
    pub code: String,
    pub last_seen: i64,
}

/// 会话类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConversationKind {
    Peer,
    Ai,
}

/// 会话。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    pub peer_device_id: Option<String>,
    pub title: String,
    pub draft: String,
    pub last_read_id: Option<String>,
    pub created_at: i64,
}

/// 消息发送方。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Sender {
    Me,
    Peer,
    Ai,
    System,
}

/// 消息类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Text,
    File,
    System,
}

/// 消息状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Sending,
    Sent,
    Delivered,
    Received,
    Failed,
}

/// 一条消息。定位链路只认 `id`，标签/收藏/搜索结果都回指到它。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub sender: Sender,
    pub kind: MessageKind,
    pub body: String,
    pub file_id: Option<String>,
    pub reply_to: Option<String>,
    pub status: MessageStatus,
    pub created_at: i64,
    /// 本机落库序号（messages 表 rowid）：显示顺序的唯一依据（2026-11-20 v2.5.1）。
    /// created_at 是发送方墙钟，双机时钟不一致时按它排序必然乱序；seq 是本机
    /// 入账的单调序号，发送与接收都按入账先后，永不被墙钟污染。
    /// 0 = 尚未落库（构造后、insert 前）；insert_message 返回并由调用方回填。
    #[serde(default)]
    pub seq: i64,
}

/// 文件元信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub path: String,
    pub mime: Option<String>,
}

/// 消息标记类型（收藏/置顶走统一模型；标签走多对多关联）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MarkKind {
    Favorite,
    Pin,
}

impl MarkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarkKind::Favorite => "favorite",
            MarkKind::Pin => "pin",
        }
    }
}

/// 标签（带全库消息计数，供标签总览页使用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub color: i64,
    pub count: i64,
}

/// 彩签分类（v2.2 起的新模型）：颜色即分类——9 个颜色（0–8）各是一个分类，
/// 每个分类可命名，一条消息至多归入一个分类。颜色序号与调色板一一对应：
/// 0 灰、1 红、2 橙、3 黄、4 绿、5 青、6 蓝、7 紫、8 粉。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Category {
    pub color: i64,
    pub name: String,
    pub count: i64,
}

/// 彩签分类的默认名（未命名时按颜色显示）。
pub fn category_default_name(color: i64) -> &'static str {
    match color {
        0 => "灰色",
        1 => "红色",
        2 => "橙色",
        3 => "黄色",
        4 => "绿色",
        5 => "青色",
        6 => "蓝色",
        7 => "紫色",
        8 => "粉色",
        _ => "未命名",
    }
}

/// 一条消息的彩签归属：分类颜色 + 可选备注（备注优先于分类名展示）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageCategory {
    pub color: i64,
    pub note: Option<String>,
}

/// 某分类下的一条消息及其备注（彩签悬停列表与总览抽屉用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CategorizedMessage {
    pub message: Message,
    pub note: Option<String>,
}

/// 字节区间集合工具：断点续传用它计算已收区间与缺失区间。
/// 区间为左闭右开 [start, end)。
pub type Ranges = Vec<(u64, u64)>;

/// 合并重叠/相邻区间并排序。
pub fn merge_ranges(mut ranges: Ranges) -> Ranges {
    ranges.retain(|(s, e)| e > s);
    ranges.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

/// 计算 [0, total) 中尚未覆盖的缺失区间。
pub fn missing_ranges(received: &Ranges, total: u64) -> Ranges {
    let merged = merge_ranges(received.clone());
    let mut out = Vec::new();
    let mut cursor = 0u64;
    for (s, e) in merged {
        if s > cursor {
            out.push((cursor, s.min(total)));
        }
        cursor = cursor.max(e);
        if cursor >= total {
            return out;
        }
    }
    if cursor < total {
        out.push((cursor, total));
    }
    out
}

/// 已收字节数。
pub fn covered_bytes(received: &Ranges) -> u64 {
    merge_ranges(received.clone())
        .iter()
        .map(|(s, e)| e - s)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 连接码是稳定的六位数字() {
        let a = connection_code("device-abc");
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(a, connection_code("device-abc"));
        assert_ne!(a, connection_code("device-xyz"));
        assert_eq!(format_code(&a).len(), 7);
    }

    #[test]
    fn 区间合并与缺失计算() {
        let recv = vec![(0, 100), (50, 150), (300, 400)];
        assert_eq!(covered_bytes(&recv), 250);
        assert_eq!(missing_ranges(&recv, 500), vec![(150, 300), (400, 500)]);
        assert_eq!(
            missing_ranges(&vec![(0, 500)], 500),
            Vec::<(u64, u64)>::new()
        );
        assert_eq!(missing_ranges(&vec![], 10), vec![(0, 10)]);
    }
}
