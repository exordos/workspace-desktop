//! Догоняет удаление обычного серверного черновика после включения защиты.

use std::{
    future::{Future, poll_fn},
    task::Poll,
    time::Duration,
};

use tokio::{
    sync::{broadcast, watch},
    time::{Instant, sleep_until},
};

use crate::{
    auth::{Service, SessionSnapshot},
    messenger::MessengerSource,
    sync::{Sync, SyncChange, SyncUpdate},
};

const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Следит за удалениями владельца до смены входа, повторяет работу после восстановления сети.
pub(in crate::messenger) async fn run_protected_draft_purges(
    source: &MessengerSource,
    sync: &Sync,
    session: &SessionSnapshot,
    mut ready: watch::Receiver<bool>,
) {
    let mut updates = sync.subscribe();
    loop {
        let online = *ready.borrow_and_update();
        let retry = online && purge_batch(source, sync, session).await;
        tokio::select! {
            changed = ready.changed() => if changed.is_err() { return; },
            changed = wait_for_draft_purge(&mut updates, session) => if !changed { return; },
            () = sleep_until(Instant::now() + RETRY_DELAY), if retry => {}
        }
    }
}

/// Ожидает появления метки удаления для входа этого рабочего процесса.
async fn wait_for_draft_purge(
    updates: &mut broadcast::Receiver<SyncUpdate>,
    session: &SessionSnapshot,
) -> bool {
    loop {
        match updates.recv().await {
            Ok(update)
                if update.context == session.context
                    && update.generation == session.generation
                    && matches!(update.change, SyncChange::DraftPurgePending) =>
            {
                return true;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return true,
            Err(broadcast::error::RecvError::Closed) => return false,
            Ok(_) => {}
        }
    }
}

/// Удаляет подтверждённые версии. Сохраняет только метаданные для неудавшихся попыток.
async fn purge_batch(source: &MessengerSource, sync: &Sync, session: &SessionSnapshot) -> bool {
    let auth = sync.auth();
    let owner = &session.context;
    let generation = session.generation;
    let Ok(Ok(pending)) = auth.with_active_generation(owner, generation, || {
        sync.storage().pending_protected_draft_purges(owner)
    }) else {
        return true;
    };
    if pending.is_empty() {
        return false;
    }
    let Ok(token) = auth
        .access_token_for_generation(owner, Service::Messenger, generation)
        .await
    else {
        return true;
    };
    let mut retry = false;
    for (scope, version) in pending {
        let revision = source
            .rest
            .protected_draft_revision(owner, &token, scope, version.id)
            .await;
        let confirmed = match revision {
            Ok(None) => true,
            Ok(Some(revision)) => {
                let mut delete = std::pin::pin!(
                    source
                        .rest
                        .delete_protected_draft_revision(owner, &token, version.id, revision,)
                );
                // Допускаем первый запрос только пока действует вход, выдавший этот токен.
                let mut started = false;
                let result = poll_fn(|context| {
                    if started {
                        return delete.as_mut().poll(context).map(Ok);
                    }
                    match auth
                        .with_active_generation(owner, generation, || delete.as_mut().poll(context))
                    {
                        Ok(poll) => {
                            started = true;
                            poll.map(Ok)
                        }
                        Err(error) => Poll::Ready(Err(error)),
                    }
                })
                .await;
                matches!(result, Ok(Ok(true)))
            }
            Err(_) => false,
        };
        if !confirmed {
            retry = true;
            continue;
        }
        if !matches!(
            auth.with_active_generation(owner, generation, || {
                sync.storage()
                    .confirm_protected_draft_purge(owner, scope, version)
            }),
            Ok(Ok(()))
        ) {
            retry = true;
        }
    }
    retry
}
