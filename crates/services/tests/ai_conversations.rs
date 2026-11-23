//! ㉕ AI 会话体系：新建/改名/按会话发送互不串话/默认会话保留。

use bluelink_domain::*;
use bluelink_services::Node;
use std::path::PathBuf;

fn 临时目录(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bluelink-aiconv-{tag}-{}", new_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn ai会话新建改名与隔离() {
    let a = Node::new(&临时目录("a"), "甲").unwrap();

    // 默认会话按需创建、标题固定
    let def = a.ensure_ai_conversation().unwrap();
    assert_eq!(def, "ai-default");

    // 新建两条 AI 会话
    let c1 = a.create_ai_conversation(None).unwrap();
    let c2 = a
        .create_ai_conversation(Some("  工作讨论 ".into()))
        .unwrap();
    assert!(c1.id.starts_with("ai-") && c1.id != "ai-default");
    assert_eq!(c1.title, "新会话");
    assert_eq!(c2.title, "工作讨论", "标题应去空白");

    // 改名持久化；空名拒绝；设备会话拒绝（本节点无设备会话，用不存在 id 验拒绝路径）
    a.rename_ai_conversation(&c1.id, "灵感记录").unwrap();
    assert!(a.rename_ai_conversation(&c1.id, "  ").is_err());
    assert!(a.rename_ai_conversation("不存在的会话", "x").is_err());
    let convs = a.storage.list_conversations().unwrap();
    let ai_convs: Vec<_> = convs
        .iter()
        .filter(|c| c.kind == ConversationKind::Ai)
        .collect();
    assert_eq!(ai_convs.len(), 3, "默认 + 新建两条都在");
    assert_eq!(
        ai_convs.iter().find(|c| c.id == c1.id).unwrap().title,
        "灵感记录"
    );

    // 按会话发送落点隔离：未配 AI 时发送会失败，但用户消息应先落到
    // 指定会话（ai_push_and_answer 先落库再调用），且不落到默认会话
    let before_def = a
        .storage
        .list_messages("ai-default", None, 100)
        .unwrap()
        .len();
    let _ = a
        .ai_send("只进新会话的问题", None, Some(c1.id.clone()), vec![])
        .await;
    let in_c1 = a.storage.list_messages(&c1.id, None, 100).unwrap();
    assert!(
        in_c1.iter().any(|m| m.body == "只进新会话的问题"),
        "问题应落在指定会话"
    );
    let after_def = a
        .storage
        .list_messages("ai-default", None, 100)
        .unwrap()
        .len();
    assert_eq!(before_def, after_def, "默认会话不应被串入");

    // 不存在的会话 id 发送应明确报错，不许静默落到默认会话
    assert!(a
        .ai_send("x", None, Some("ai-不存在".into()), vec![])
        .await
        .is_err());
}
