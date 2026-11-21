//! ㉓ 上下文预算：token 估算口径与裁剪纯函数的回归测试。

use bluelink_services::{ai_kept_from, estimate_tokens, AI_HISTORY_TOKEN_BUDGET};

#[test]
fn token估算口径() {
    // ASCII 约 4 字符 1 token
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    // 中文约 1 字 1 token
    assert_eq!(estimate_tokens("你好世界"), 4);
    // 混合：英文段按 4 字符折算、中文逐字
    assert_eq!(estimate_tokens("hello你好"), 2 + 2);
}

#[test]
fn 预算内全保留() {
    let bodies: Vec<&str> = vec!["你好", "你好呀", "最近怎么样"];
    assert_eq!(ai_kept_from(&bodies), 0);
}

#[test]
fn 超预算从最老丢弃且最新无条件保留() {
    // 每条 1000 个中文字 ≈ 1004 token；30 条远超 24K 预算
    let big = "字".repeat(1000);
    let bodies: Vec<&str> = (0..30).map(|_| big.as_str()).collect();
    let from = ai_kept_from(&bodies);
    assert!(from > 0, "应当丢弃最老的一部分");
    // 保留段估算不超预算
    let kept_tokens: usize = bodies[from..]
        .iter()
        .map(|b| estimate_tokens(b) + 4)
        .sum();
    assert!(
        kept_tokens <= AI_HISTORY_TOKEN_BUDGET,
        "保留段估算 {kept_tokens} 超预算 {}",
        AI_HISTORY_TOKEN_BUDGET
    );
    // 最新一条即使单条就超预算也必须保留
    let huge = "字".repeat(AI_HISTORY_TOKEN_BUDGET + 5000);
    let bodies2: Vec<&str> = vec!["旧消息", huge.as_str()];
    assert_eq!(ai_kept_from(&bodies2), 1, "最新一条无条件保留，只丢旧的");
}
