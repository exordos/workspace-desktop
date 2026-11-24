//! Проверяет совместное ожидание «Избранного» в рамках одного входа.

use std::cell::Cell;

use crate::context::AccountId;

use super::*;

/// Запускает два ожидания одновременно и проверяет, что запрос выполняется один раз.
#[tokio::test]
async fn shares_one_live_request_per_owner_and_generation() {
    let owner = OwnerContext {
        account_id: AccountId("a".into()),
        server: "https://workspace.test".into(),
        organization_id: "org".into(),
        project_id: "project".into(),
        user_id: "user".into(),
    };
    let requests = SelfChatRequests::default();
    let first = requests.request(&owner, SessionGeneration(1));
    let second = requests.request(&owner, SessionGeneration(1));
    let new_session = requests.request(&owner, SessionGeneration(2));
    let mut other_owner = owner.clone();
    other_owner.project_id = "other-project".into();
    let other = requests.request(&other_owner, SessionGeneration(1));
    assert!(!Arc::ptr_eq(&first, &new_session));
    assert!(!Arc::ptr_eq(&first, &other));

    let calls = Cell::new(0);
    let resolve = || async {
        calls.set(calls.get() + 1);
        tokio::task::yield_now().await;
        Err(SelfChatError::Unavailable)
    };
    let (a, b) = tokio::join!(first.get_or_init(resolve), second.get_or_init(resolve));
    assert_eq!(a, b);
    assert_eq!(calls.get(), 1);

    drop((first, second));
    let retry = requests.request(&owner, SessionGeneration(1));
    assert!(retry.get().is_none());
}
