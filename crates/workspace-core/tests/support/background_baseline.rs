//! Измеряет задержку фоновой записи в двух изолированных тестовых сессиях.
//! Прогон запускается явно и печатает параметры и задержки для сравнения.

use super::*;
use workspace_core::storage::{HistoryMetadata, HistoryTime, MessageBody, MessagePlacement};

/// Подаёт одинаковый поток в два Runtime-подключения и проверяет каждую запись.
#[tokio::test]
#[ignore = "явный измерительный прогон"]
async fn background_messenger_baseline() {
    const EVENTS: u64 = 1000;
    let fixture = Fixture::new(|user, _| Ok(project_token(user, "refreshed", None)));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("baseline.sqlite3");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let sync = Arc::new(EventSync::new(fixture.auth.clone(), storage));
    let (connections, mut receive) = async_mpsc::unbounded_channel();
    let runtime = Runtime::new(sync.clone(), vec![Arc::new(TestSource(connections))]).unwrap();
    let (stop, stopped) = oneshot::channel();
    let worker = tokio::spawn(runtime.run(async {
        let _ = stopped.await;
    }));
    let owners = [fixture.login("a"), fixture.login("b")];
    let mut senders = HashMap::new();
    for _ in 0..2 {
        let (session, sender) = tokio::time::timeout(Duration::from_secs(5), receive.recv())
            .await
            .expect("тайм-аут подключения")
            .expect("источник закрыт");
        assert!(session.cursor.is_none());
        senders.insert(session.context.account_id, sender);
    }
    let mut latencies = Vec::new();
    for version in 1..=EVENTS {
        for owner in &owners {
            let event = Event::IncomingDirectMessage {
                cursor: Cursor {
                    epoch_generation: "test-epoch".into(),
                    epoch_version: version,
                },
                message: Message {
                    id: uuid::Uuid::from_u128(version as u128).to_string(),
                    body: MessageBody::Markdown("Тестовое сообщение".into()),
                },
                conversation_id: uuid::Uuid::from_u128(10001).to_string(),
                placement: Some(MessagePlacement {
                    stream_id: uuid::Uuid::from_u128(10001).to_string(),
                    topic_id: uuid::Uuid::from_u128(10002).to_string(),
                }),
                metadata: Some(HistoryMetadata {
                    author_id: Some(uuid::Uuid::from_u128(10003)),
                    created_at: HistoryTime::parse("2026-01-01T00:00:00Z").unwrap(),
                    updated_at: None,
                }),
            };
            let start = Instant::now();
            senders[&owner.account_id].send(Ok(event)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if sync
                        .cursor(owner, "messenger")
                        .unwrap()
                        .is_some_and(|cursor| cursor.epoch_version == version)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("тайм-аут фиксации события");
            latencies.push(start.elapsed().as_micros());
        }
    }
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM sync_messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, (EVENTS * 2) as i64);
    for owner in &owners {
        let owner_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sync_messages WHERE account_id = ?1",
                [&owner.account_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner_count, EVENTS as i64);
        assert_eq!(
            sync.cursor(owner, "messenger")
                .unwrap()
                .unwrap()
                .epoch_version,
            EVENTS
        );
    }
    latencies.sort_unstable();
    println!(
        "accounts=2 events_per_account={EVENTS} messages={count} cursor={EVENTS} latency_us_p50={} latency_us_p95={} latency_us_max={}",
        latencies[latencies.len() / 2],
        latencies[latencies.len() * 95 / 100],
        latencies.last().unwrap()
    );
}
