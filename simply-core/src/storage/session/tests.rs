//! Integration tests for Session with storage

use std::sync::Arc;

use llm::{ChatMessage, ChatPayload, ContentBlock};

use crate::agent::ConversationContext;
use crate::storage::coordinator::StorageCoordinator;
use crate::storage::ids::{ConversationId, UserId};
use crate::storage::implementations::memory::{
    MemoryAssetStore, MemoryBlobStore, MemoryEntityStore,
    MemoryStorage, MemoryTextStore, MemoryTurnStore,
};
use crate::storage::session::Session;

fn make_test_coordinator() -> Arc<StorageCoordinator<MemoryStorage>> {
    Arc::new(StorageCoordinator::new(
        Arc::new(MemoryBlobStore::new()),
        Arc::new(MemoryAssetStore::new()),
        Arc::new(MemoryTextStore::new()),
        Arc::new(MemoryEntityStore::new()),
        Arc::new(MemoryTurnStore::new()),
    ))
}

async fn create_test_conversation(
    coordinator: &Arc<StorageCoordinator<MemoryStorage>>,
) -> ConversationId {
    let user_id = UserId::new();
    coordinator
        .create_conversation(&user_id, Some("Test"))
        .await
        .unwrap()
}

#[tokio::test]
async fn test_session_new_is_empty() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let session = Session::<MemoryStorage>::new(coordinator, conversation_id);
    assert_eq!(session.len(), 0);
    assert!(session.is_empty());
}

#[tokio::test]
async fn test_session_add_and_commit() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let mut session = Session::<MemoryStorage>::new(coordinator.clone(), conversation_id.clone());

    session.add(ChatMessage::user(ChatPayload::new(vec![
        ContentBlock::Text { text: "Hello".to_string() },
    ])));
    assert_eq!(session.len(), 1);

    session.commit().await.unwrap();

    // After commit, messages are persisted. Open a new session to verify.
    let reopened = Session::<MemoryStorage>::open(coordinator, conversation_id).await.unwrap();
    assert_eq!(reopened.len(), 1);
}

#[tokio::test]
async fn test_session_open_loads_history() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    {
        let mut session = Session::<MemoryStorage>::new(coordinator.clone(), conversation_id.clone());
        session.add(ChatMessage::user(ChatPayload::new(vec![
            ContentBlock::Text { text: "Hello, assistant!".to_string() },
        ])));
        session.add(ChatMessage::assistant(ChatPayload::new(vec![
            ContentBlock::Text { text: "Hello, user!".to_string() },
        ])));
        session.commit().await.unwrap();
    }

    let opened = Session::<MemoryStorage>::open(coordinator, conversation_id).await.unwrap();
    assert_eq!(opened.len(), 2);
}

#[tokio::test]
async fn test_session_messages_includes_pending() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let mut session = Session::<MemoryStorage>::new(coordinator.clone(), conversation_id.clone());

    // Commit first message
    session.add(ChatMessage::user(ChatPayload::new(vec![
        ContentBlock::Text { text: "Committed".to_string() },
    ])));
    session.commit().await.unwrap();

    // Add pending message
    session.add(ChatMessage::assistant(ChatPayload::new(vec![
        ContentBlock::Text { text: "Pending".to_string() },
    ])));

    // messages() should include both
    let guard = session.messages().await.unwrap();
    assert_eq!(guard.len(), 2);
}

#[tokio::test]
async fn test_session_commit_is_noop_when_empty() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let mut session = Session::<MemoryStorage>::new(coordinator, conversation_id);
    session.commit().await.unwrap(); // should not error
    assert_eq!(session.len(), 0);
}

#[tokio::test]
async fn test_session_multi_content_blocks() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let mut session = Session::<MemoryStorage>::new(coordinator.clone(), conversation_id.clone());

    session.add(ChatMessage::user(ChatPayload::new(vec![
        ContentBlock::Text { text: "Part 1".to_string() },
        ContentBlock::Text { text: "Part 2".to_string() },
    ])));
    session.commit().await.unwrap();

    let mut reopened = Session::<MemoryStorage>::open(coordinator, conversation_id).await.unwrap();
    let guard = reopened.messages().await.unwrap();
    // The message has 2 content blocks
    assert_eq!(guard[0].payload.content.len(), 2);
}

#[tokio::test]
async fn test_session_consecutive_commits_reuse_turn() {
    let coordinator = make_test_coordinator();
    let conversation_id = create_test_conversation(&coordinator).await;

    let mut session = Session::<MemoryStorage>::new(coordinator.clone(), conversation_id.clone());

    // Two user messages committed separately should share a turn (same role)
    session.add(ChatMessage::user(ChatPayload::new(vec![
        ContentBlock::Text { text: "First".to_string() },
    ])));
    session.commit().await.unwrap();

    session.add(ChatMessage::user(ChatPayload::new(vec![
        ContentBlock::Text { text: "Second".to_string() },
    ])));
    session.commit().await.unwrap();

    assert_eq!(session.len(), 2);
}
