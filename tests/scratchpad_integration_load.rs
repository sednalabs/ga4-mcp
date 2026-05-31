use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ga4_mcp::error::AnalyticsError;
use ga4_mcp::scratchpad::{
    DuckDbEngine, ScratchpadExecutionHooks, ScratchpadIngestColumn, ScratchpadSessionConfig,
    ScratchpadSessionManager, SharedScratchpadEngine,
};
use serde_json::{Map, Value};

fn test_root_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock should be monotonic")
        .as_nanos();
    std::env::temp_dir().join(format!("ga4-mcp-it-{name}-{nanos}"))
}

fn test_manager(
    name: &str,
    max_rows_per_session: usize,
    max_memory_mb: usize,
    query_timeout: Duration,
) -> ScratchpadSessionManager {
    let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
    let config = ScratchpadSessionConfig::new(
        Duration::from_secs(120),
        8,
        64,
        max_rows_per_session,
        max_memory_mb,
    )
    .with_root_dir(test_root_dir(name))
    .with_query_timeout(query_timeout)
    .with_max_sql_bytes(128 * 1024);
    ScratchpadSessionManager::new(engine, config).expect("manager")
}

fn default_columns() -> Vec<ScratchpadIngestColumn> {
    vec![
        ScratchpadIngestColumn {
            name: "id".to_string(),
            logical_type: "integer".to_string(),
        },
        ScratchpadIngestColumn {
            name: "label".to_string(),
            logical_type: "string".to_string(),
        },
    ]
}

fn make_rows(count: usize, offset: usize) -> Vec<Map<String, Value>> {
    (0..count)
        .map(|idx| {
            Map::from_iter([
                (
                    "id".to_string(),
                    Value::Number(((idx + offset) as i64).into()),
                ),
                (
                    "label".to_string(),
                    Value::String(format!("row-{}", idx + offset)),
                ),
            ])
        })
        .collect()
}

fn query_count(
    manager: &ScratchpadSessionManager,
    session_id: &str,
    sql: &str,
) -> Result<i64, AnalyticsError> {
    let hooks = manager
        .default_execution_hooks()
        .with_interrupt_poll_interval(Duration::from_millis(1));
    manager.run_guarded(session_id, sql, hooks, |conn| {
        let mut stmt = conn.prepare(sql).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!("failed to prepare count query: {err}"))
        })?;
        stmt.query_row([], |row| row.get::<_, i64>(0))
            .map_err(|err| {
                AnalyticsError::ScratchpadEngine(format!("failed to execute count query: {err}"))
            })
    })
}

#[test]
fn scratchpad_sessions_isolate_same_table_name() {
    let manager = test_manager("session-isolation", 10_000, 128, Duration::from_secs(3));
    manager.open_session("s1").expect("s1 should open");
    manager.open_session("s2").expect("s2 should open");

    manager
        .ingest_rows("s1", "events", &default_columns(), &make_rows(3, 0))
        .expect("s1 ingest should pass");
    manager
        .ingest_rows("s2", "events", &default_columns(), &make_rows(1, 100))
        .expect("s2 ingest should pass");

    let s1_count =
        query_count(&manager, "s1", "SELECT COUNT(*) FROM events").expect("s1 count should work");
    let s2_count =
        query_count(&manager, "s2", "SELECT COUNT(*) FROM events").expect("s2 count should work");
    assert_eq!(s1_count, 3);
    assert_eq!(s2_count, 1);

    let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
}

#[test]
fn scratchpad_load_supports_paged_queries_over_large_ingest() {
    let manager = test_manager("paged-load", 20_000, 128, Duration::from_secs(3));
    manager
        .open_session("load_session")
        .expect("session should open");

    manager
        .ingest_rows(
            "load_session",
            "events",
            &default_columns(),
            &make_rows(2_000, 0),
        )
        .expect("bulk ingest should succeed");

    let total = query_count(&manager, "load_session", "SELECT COUNT(*) FROM events")
        .expect("total count should succeed");
    let page_1 = query_count(
        &manager,
        "load_session",
        "SELECT COUNT(*) FROM (SELECT id FROM events ORDER BY id LIMIT 250 OFFSET 0) page",
    )
    .expect("page 1 count should succeed");
    let page_2 = query_count(
        &manager,
        "load_session",
        "SELECT COUNT(*) FROM (SELECT id FROM events ORDER BY id LIMIT 250 OFFSET 250) page",
    )
    .expect("page 2 count should succeed");
    let page_8 = query_count(
        &manager,
        "load_session",
        "SELECT COUNT(*) FROM (SELECT id FROM events ORDER BY id LIMIT 250 OFFSET 1750) page",
    )
    .expect("page 8 count should succeed");

    assert_eq!(total, 2_000);
    assert_eq!(page_1, 250);
    assert_eq!(page_2, 250);
    assert_eq!(page_8, 250);

    let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
}

#[test]
fn scratchpad_guarded_query_timeout_is_enforced() {
    let manager = test_manager("timeout", 10_000, 128, Duration::from_millis(8));
    let hooks = ScratchpadExecutionHooks::new(Duration::from_millis(8))
        .with_interrupt_poll_interval(Duration::from_millis(1));

    let err = manager
        .run_guarded("timeout_session", "SELECT 1", hooks, |_conn| {
            std::thread::sleep(Duration::from_millis(40));
            Ok(())
        })
        .expect_err("timeout must trigger");

    assert_eq!(err.code(), "SCRATCHPAD_QUERY_TIMEOUT");
    assert_eq!(err.reason(), "scratchpad_timeout");

    let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
}

#[test]
fn scratchpad_ingest_rejects_rows_beyond_session_bound() {
    let manager = test_manager("row-quota", 500, 64, Duration::from_secs(3));
    manager
        .open_session("quota_session")
        .expect("session should open");

    let err = manager
        .ingest_rows(
            "quota_session",
            "events",
            &default_columns(),
            &make_rows(600, 0),
        )
        .expect_err("ingest over row quota must fail");

    assert_eq!(err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");
    assert_eq!(err.reason(), "scratchpad_limit_exceeded");

    let info = manager
        .session_info("quota_session")
        .expect("session info should resolve");
    assert_eq!(info.rows_used, 0);
    assert_eq!(info.tables_used, 0);

    let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
}
