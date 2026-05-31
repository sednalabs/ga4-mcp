//! # Scratchpad Core
//!
//! DuckDB engine adapter and session lifecycle manager for GA4 scratchpad workflows.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use duckdb::types::Value as DuckValue;
use duckdb::{Connection, params_from_iter};
use mcp_toolkit_observability::{EventContext, Level, emit_event, safe_error, safe_text};
use serde_json::{Map, Value};

use crate::error::AnalyticsError;
use crate::sql_safety::validate_scratchpad_sql;

const MAX_SESSION_ID_LEN: usize = 128;
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_MAX_SQL_BYTES: usize = 65_536;
const DEFAULT_INTERRUPT_POLL_INTERVAL_MS: u64 = 5;
const MAX_TABLE_SCHEMA_COLUMNS_PREVIEW: usize = 32;

#[derive(Debug, Clone)]
pub struct SessionDatabaseConfig {
    pub database_path: PathBuf,
    pub max_memory_mb: usize,
}

pub trait ScratchpadEngine: Send + Sync {
    fn open_session_connection(
        &self,
        config: &SessionDatabaseConfig,
    ) -> Result<Connection, AnalyticsError>;

    fn probe(&self) -> Result<(), AnalyticsError>;
}

#[derive(Debug, Clone, Default)]
pub struct DuckDbEngine;

impl DuckDbEngine {
    pub fn new() -> Result<Self, AnalyticsError> {
        let engine = Self;
        engine.probe()?;
        Ok(engine)
    }
}

impl ScratchpadEngine for DuckDbEngine {
    fn open_session_connection(
        &self,
        config: &SessionDatabaseConfig,
    ) -> Result<Connection, AnalyticsError> {
        if config.max_memory_mb == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_memory_mb",
                "must be greater than zero",
            ));
        }

        let conn = Connection::open(&config.database_path).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to open duckdb database {}: {err}",
                config.database_path.display()
            ))
        })?;

        conn.execute_batch(&format!(
            "PRAGMA memory_limit='{}MB';",
            config.max_memory_mb
        ))
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to apply duckdb memory limit pragma: {err}"
            ))
        })?;

        Ok(conn)
    }

    fn probe(&self) -> Result<(), AnalyticsError> {
        let conn = Connection::open_in_memory().map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!("duckdb bootstrap failed: {err}"))
        })?;
        conn.execute_batch("SELECT 1;").map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!("duckdb probe query failed: {err}"))
        })?;
        Ok(())
    }
}

pub type SharedScratchpadEngine = Arc<dyn ScratchpadEngine>;

#[derive(Debug, Clone)]
pub struct ScratchpadSessionConfig {
    pub session_ttl: Duration,
    pub max_sessions: usize,
    pub max_tables_per_session: usize,
    pub max_rows_per_session: usize,
    pub max_memory_mb: usize,
    pub query_timeout: Duration,
    pub max_sql_bytes: usize,
    pub root_dir: PathBuf,
}

impl ScratchpadSessionConfig {
    pub fn new(
        session_ttl: Duration,
        max_sessions: usize,
        max_tables_per_session: usize,
        max_rows_per_session: usize,
        max_memory_mb: usize,
    ) -> Self {
        Self {
            session_ttl,
            max_sessions,
            max_tables_per_session,
            max_rows_per_session,
            max_memory_mb,
            query_timeout: Duration::from_millis(DEFAULT_QUERY_TIMEOUT_MS),
            max_sql_bytes: DEFAULT_MAX_SQL_BYTES,
            root_dir: default_root_dir(),
        }
    }

    pub fn with_root_dir(mut self, root_dir: PathBuf) -> Self {
        self.root_dir = root_dir;
        self
    }

    pub fn with_query_timeout(mut self, query_timeout: Duration) -> Self {
        self.query_timeout = query_timeout;
        self
    }

    pub fn with_max_sql_bytes(mut self, max_sql_bytes: usize) -> Self {
        self.max_sql_bytes = max_sql_bytes;
        self
    }

    fn validate(&self) -> Result<(), AnalyticsError> {
        if self.session_ttl.is_zero() {
            return Err(AnalyticsError::invalid(
                "scratchpad_session_ttl_secs",
                "must be greater than zero",
            ));
        }
        if self.max_sessions == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_sessions",
                "must be greater than zero",
            ));
        }
        if self.max_tables_per_session == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_tables_per_session",
                "must be greater than zero",
            ));
        }
        if self.max_rows_per_session == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_rows_per_session",
                "must be greater than zero",
            ));
        }
        if self.max_memory_mb == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_memory_mb",
                "must be greater than zero",
            ));
        }
        if self.query_timeout.is_zero() {
            return Err(AnalyticsError::invalid(
                "scratchpad_query_timeout_ms",
                "must be greater than zero",
            ));
        }
        if self.max_sql_bytes == 0 {
            return Err(AnalyticsError::invalid(
                "scratchpad_max_sql_bytes",
                "must be greater than zero",
            ));
        }
        Ok(())
    }
}

fn default_root_dir() -> PathBuf {
    std::env::temp_dir().join("ga4-mcp").join("scratchpad")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadSessionSnapshot {
    pub tables_used: usize,
    pub tables_remaining: usize,
    pub rows_used: usize,
    pub rows_remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadSessionInfo {
    pub session_id: String,
    pub tables_used: usize,
    pub tables_remaining: usize,
    pub rows_used: usize,
    pub rows_remaining: usize,
    pub ttl_seconds_remaining: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadTableInfo {
    pub schema: String,
    pub name: String,
    pub table_type: String,
    pub column_count: usize,
    pub columns: Vec<ScratchpadTableColumnInfo>,
    pub columns_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadTableColumnInfo {
    pub name: String,
    pub logical_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadIngestColumn {
    pub name: String,
    pub logical_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchpadIngestMode {
    Create,
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadIngestStats {
    pub rows_inserted: usize,
    pub columns_inserted: usize,
    pub session_snapshot: ScratchpadSessionSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadDropTableStats {
    pub dropped: bool,
    pub rows_removed: usize,
    pub session_snapshot: ScratchpadSessionSnapshot,
}

#[derive(Debug, Clone, Default)]
pub struct ScratchpadCancelToken {
    cancelled: Arc<AtomicBool>,
}

impl ScratchpadCancelToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct ScratchpadExecutionHooks {
    pub timeout: Duration,
    pub cancel_token: Option<ScratchpadCancelToken>,
    pub interrupt_poll_interval: Duration,
}

impl ScratchpadExecutionHooks {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            cancel_token: None,
            interrupt_poll_interval: Duration::from_millis(DEFAULT_INTERRUPT_POLL_INTERVAL_MS),
        }
    }

    pub fn with_cancel_token(mut self, cancel_token: ScratchpadCancelToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_interrupt_poll_interval(mut self, interval: Duration) -> Self {
        self.interrupt_poll_interval = if interval.is_zero() {
            Duration::from_millis(DEFAULT_INTERRUPT_POLL_INTERVAL_MS)
        } else {
            interval
        };
        self
    }
}

#[derive(Debug)]
struct SessionEntry {
    db_path: PathBuf,
    last_touched: Instant,
    tables_used: usize,
    rows_used: usize,
}

#[derive(Debug, Default)]
struct SessionState {
    sessions: HashMap<String, SessionEntry>,
}

#[derive(Clone)]
pub struct ScratchpadSessionManager {
    engine: SharedScratchpadEngine,
    config: ScratchpadSessionConfig,
    runtime_max_sessions: Arc<AtomicUsize>,
    runtime_max_tables_per_session: Arc<AtomicUsize>,
    state: Arc<Mutex<SessionState>>,
}

impl ScratchpadSessionManager {
    pub fn new(
        engine: SharedScratchpadEngine,
        config: ScratchpadSessionConfig,
    ) -> Result<Self, AnalyticsError> {
        config.validate()?;
        fs::create_dir_all(&config.root_dir).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to create scratchpad root directory {}: {err}",
                config.root_dir.display()
            ))
        })?;

        Ok(Self {
            engine,
            runtime_max_sessions: Arc::new(AtomicUsize::new(config.max_sessions)),
            runtime_max_tables_per_session: Arc::new(AtomicUsize::new(
                config.max_tables_per_session,
            )),
            config,
            state: Arc::new(Mutex::new(SessionState::default())),
        })
    }

    pub fn config(&self) -> &ScratchpadSessionConfig {
        &self.config
    }

    pub fn max_sessions_limit(&self) -> usize {
        self.runtime_max_sessions.load(Ordering::SeqCst)
    }

    pub fn set_max_sessions_limit(&self, max_sessions: usize) -> Result<(), AnalyticsError> {
        if max_sessions == 0 {
            return Err(AnalyticsError::invalid(
                "max_sessions",
                "must be greater than zero",
            ));
        }
        self.runtime_max_sessions
            .store(max_sessions, Ordering::SeqCst);
        Ok(())
    }

    pub fn max_tables_per_session_limit(&self) -> usize {
        self.runtime_max_tables_per_session.load(Ordering::SeqCst)
    }

    pub fn set_max_tables_per_session_limit(
        &self,
        max_tables_per_session: usize,
    ) -> Result<(), AnalyticsError> {
        if max_tables_per_session == 0 {
            return Err(AnalyticsError::invalid(
                "max_tables_per_session",
                "must be greater than zero",
            ));
        }
        self.runtime_max_tables_per_session
            .store(max_tables_per_session, Ordering::SeqCst);
        Ok(())
    }

    pub fn active_session_count(&self) -> Result<usize, AnalyticsError> {
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let count = state.sessions.len();
        drop(state);
        cleanup_paths(removed_paths);
        Ok(count)
    }

    pub fn open_session(&self, session_id: &str) -> Result<ScratchpadSessionInfo, AnalyticsError> {
        let conn = self.open_connection(session_id)?;
        drop(conn);
        self.session_info(session_id)
    }

    pub fn session_info(&self, session_id: &str) -> Result<ScratchpadSessionInfo, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let now = Instant::now();
        let max_tables_per_session = self.max_tables_per_session_limit();

        let info = state
            .sessions
            .get_mut(&session_id)
            .map(|entry| {
                entry.last_touched = now;
                session_info_from_entry(
                    &session_id,
                    entry,
                    max_tables_per_session,
                    self.config.max_rows_per_session,
                    self.config.session_ttl,
                    now,
                )
            })
            .ok_or_else(|| AnalyticsError::scratchpad_session_not_found(session_id.clone()));

        drop(state);
        cleanup_paths(removed_paths);
        info
    }

    pub fn list_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<ScratchpadSessionInfo>, AnalyticsError> {
        if limit == 0 {
            return Err(AnalyticsError::invalid(
                "limit",
                "must be greater than zero",
            ));
        }

        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let now = Instant::now();
        let max_tables_per_session = self.max_tables_per_session_limit();

        let mut sessions = state
            .sessions
            .iter_mut()
            .map(|(session_id, entry)| {
                entry.last_touched = now;
                session_info_from_entry(
                    session_id,
                    entry,
                    max_tables_per_session,
                    self.config.max_rows_per_session,
                    self.config.session_ttl,
                    now,
                )
            })
            .collect::<Vec<_>>();

        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        sessions.truncate(limit);

        drop(state);
        cleanup_paths(removed_paths);
        Ok(sessions)
    }

    pub fn list_tables(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<ScratchpadTableInfo>, AnalyticsError> {
        if limit == 0 {
            return Err(AnalyticsError::invalid(
                "limit",
                "must be greater than zero",
            ));
        }

        let session_id = normalize_session_id(session_id)?;
        let db_path = self.existing_session_db_path(&session_id)?;
        let conn = self
            .engine
            .open_session_connection(&SessionDatabaseConfig {
                database_path: db_path,
                max_memory_mb: self.config.max_memory_mb,
            })?;

        let sql = format!(
            "SELECT table_schema, table_name, table_type
             FROM information_schema.tables
             WHERE table_schema NOT IN ('information_schema', 'pg_catalog')
             ORDER BY table_schema, table_name
             LIMIT {}",
            limit
        );
        let mut stmt = conn.prepare(&sql).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to prepare scratchpad table inventory query: {err}"
            ))
        })?;
        let mut rows = stmt.query([]).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to execute scratchpad table inventory query: {err}"
            ))
        })?;

        let mut tables = Vec::new();
        while let Some(row) = rows.next().map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to read scratchpad table inventory row: {err}"
            ))
        })? {
            let schema = row.get::<_, String>(0).map_err(|err| {
                AnalyticsError::ScratchpadEngine(format!(
                    "failed to decode scratchpad table schema: {err}"
                ))
            })?;
            let name = row.get::<_, String>(1).map_err(|err| {
                AnalyticsError::ScratchpadEngine(format!(
                    "failed to decode scratchpad table name: {err}"
                ))
            })?;
            let table_type = row.get::<_, String>(2).map_err(|err| {
                AnalyticsError::ScratchpadEngine(format!(
                    "failed to decode scratchpad table type: {err}"
                ))
            })?;
            let (column_count, columns, columns_truncated) = fetch_table_column_preview(
                &conn,
                &schema,
                &name,
                MAX_TABLE_SCHEMA_COLUMNS_PREVIEW,
            )?;
            tables.push(ScratchpadTableInfo {
                schema,
                name,
                table_type,
                column_count,
                columns,
                columns_truncated,
            });
        }

        Ok(tables)
    }

    pub fn ingest_rows(
        &self,
        session_id: &str,
        table_name: &str,
        columns: &[ScratchpadIngestColumn],
        rows: &[Map<String, Value>],
    ) -> Result<ScratchpadIngestStats, AnalyticsError> {
        self.ingest_rows_with_mode(
            session_id,
            table_name,
            columns,
            rows,
            ScratchpadIngestMode::Create,
        )
    }

    pub fn ingest_rows_with_mode(
        &self,
        session_id: &str,
        table_name: &str,
        columns: &[ScratchpadIngestColumn],
        rows: &[Map<String, Value>],
        mode: ScratchpadIngestMode,
    ) -> Result<ScratchpadIngestStats, AnalyticsError> {
        let ingest_started = Instant::now();
        if columns.is_empty() {
            return Err(AnalyticsError::invalid(
                "columns",
                "must include at least one column",
            ));
        }

        let session_id = normalize_session_id(session_id)?;
        validate_sql_identifier(table_name, "table_name")?;
        let mut seen_column_names = std::collections::HashSet::new();
        for column in columns {
            validate_sql_identifier(&column.name, "column_name")?;
            if !seen_column_names.insert(column.name.clone()) {
                return Err(AnalyticsError::invalid(
                    "columns",
                    format!("duplicate column name '{}'", column.name),
                ));
            }
        }

        let (db_path, reserved_snapshot) =
            self.reserve_ingest_capacity(&session_id, rows.len(), mode)?;
        let ingest_result = self.persist_ingest_table(&db_path, table_name, columns, rows, mode);
        match ingest_result {
            Ok(()) => {
                let stats = ScratchpadIngestStats {
                    rows_inserted: rows.len(),
                    columns_inserted: columns.len(),
                    session_snapshot: reserved_snapshot,
                };
                emit_event(
                    Level::INFO,
                    "ga4_mcp.scratchpad.load",
                    &EventContext::new().with_session_id(&session_id),
                    &[
                        safe_text("session_id", &session_id),
                        safe_text("table_name", table_name),
                        safe_text("mode", ingest_mode_as_str(mode)),
                        safe_text("rows_inserted", stats.rows_inserted.to_string()),
                        safe_text("columns_inserted", stats.columns_inserted.to_string()),
                        safe_text(
                            "duration_ms",
                            contract_elapsed_ms(ingest_started.elapsed().as_millis()).to_string(),
                        ),
                    ],
                );
                Ok(stats)
            }
            Err(err) => {
                self.rollback_ingest_capacity(&session_id, rows.len(), mode);
                emit_event(
                    Level::WARN,
                    "ga4_mcp.scratchpad.load.error",
                    &EventContext::new().with_session_id(&session_id),
                    &[
                        safe_text("session_id", &session_id),
                        safe_text("table_name", table_name),
                        safe_text("mode", ingest_mode_as_str(mode)),
                        safe_text(
                            "duration_ms",
                            contract_elapsed_ms(ingest_started.elapsed().as_millis()).to_string(),
                        ),
                        safe_error("error", &err),
                    ],
                );
                Err(err)
            }
        }
    }

    pub fn drop_table(
        &self,
        session_id: &str,
        table_name: &str,
        if_exists: bool,
    ) -> Result<ScratchpadDropTableStats, AnalyticsError> {
        let operation_started = Instant::now();
        let session_id = normalize_session_id(session_id)?;
        validate_sql_identifier(table_name, "table_name")?;
        let db_path = self.existing_session_db_path(&session_id)?;
        let conn = self
            .engine
            .open_session_connection(&SessionDatabaseConfig {
                database_path: db_path,
                max_memory_mb: self.config.max_memory_mb,
            })?;

        if !table_exists(&conn, table_name)? {
            if !if_exists {
                return Err(AnalyticsError::invalid(
                    "table_name",
                    format!("table '{table_name}' not found in scratchpad session"),
                ));
            }

            let snapshot = self
                .session_snapshot(&session_id)?
                .ok_or_else(|| AnalyticsError::scratchpad_session_not_found(session_id.clone()))?;
            return Ok(ScratchpadDropTableStats {
                dropped: false,
                rows_removed: 0,
                session_snapshot: snapshot,
            });
        }

        let rows_removed = table_row_count(&conn, table_name)?;
        let drop_sql = format!("DROP TABLE {}", quote_ident(table_name));
        conn.execute_batch(&drop_sql).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to drop scratchpad table '{table_name}': {err}"
            ))
        })?;
        let session_snapshot = self.note_table_dropped(&session_id, rows_removed)?;

        emit_event(
            Level::INFO,
            "ga4_mcp.scratchpad.table.drop",
            &EventContext::new().with_session_id(&session_id),
            &[
                safe_text("session_id", &session_id),
                safe_text("table_name", table_name),
                safe_text("rows_removed", rows_removed.to_string()),
                safe_text(
                    "duration_ms",
                    contract_elapsed_ms(operation_started.elapsed().as_millis()).to_string(),
                ),
            ],
        );

        Ok(ScratchpadDropTableStats {
            dropped: true,
            rows_removed,
            session_snapshot,
        })
    }

    pub fn default_execution_hooks(&self) -> ScratchpadExecutionHooks {
        ScratchpadExecutionHooks::new(self.config.query_timeout)
    }

    pub fn validate_query_sql(&self, sql: &str) -> Result<(), AnalyticsError> {
        validate_scratchpad_sql(sql, self.config.max_sql_bytes)
            .map_err(|err| AnalyticsError::scratchpad_sql_rejected(err.code, err.message))
    }

    pub fn run_guarded<T, F>(
        &self,
        session_id: &str,
        sql: &str,
        hooks: ScratchpadExecutionHooks,
        execute: F,
    ) -> Result<T, AnalyticsError>
    where
        F: FnOnce(&Connection) -> Result<T, AnalyticsError>,
    {
        self.validate_query_sql(sql)?;
        if hooks
            .cancel_token
            .as_ref()
            .is_some_and(ScratchpadCancelToken::is_cancelled)
        {
            return Err(AnalyticsError::scratchpad_query_cancelled());
        }

        let conn = self.open_connection(session_id)?;
        let started = Instant::now();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let watcher = if hooks.timeout > Duration::ZERO || hooks.cancel_token.is_some() {
            Some(spawn_interrupt_watcher(
                conn.interrupt_handle(),
                Arc::clone(&stop_flag),
                hooks.clone(),
            ))
        } else {
            None
        };

        let result = execute(&conn);
        stop_flag.store(true, Ordering::SeqCst);
        let elapsed_ms = contract_elapsed_ms(started.elapsed().as_millis());

        let interrupt_reason = match watcher {
            Some(handle) => handle.join().map_err(|_| {
                AnalyticsError::Internal("scratchpad interrupt watcher panicked".into())
            })?,
            None => None,
        };

        if let Some(reason) = interrupt_reason {
            emit_event(
                Level::WARN,
                "ga4_mcp.scratchpad.query.runtime",
                &EventContext::new().with_session_id(session_id),
                &[
                    safe_text("session_id", session_id),
                    safe_text("outcome", "interrupted"),
                    safe_text(
                        "reason",
                        match reason {
                            InterruptReason::Timeout => "timeout",
                            InterruptReason::Cancelled => "cancelled",
                        },
                    ),
                    safe_text("duration_ms", elapsed_ms.to_string()),
                ],
            );
            return match reason {
                InterruptReason::Timeout => {
                    Err(AnalyticsError::scratchpad_query_timeout(hooks.timeout))
                }
                InterruptReason::Cancelled => Err(AnalyticsError::scratchpad_query_cancelled()),
            };
        }

        if hooks.timeout > Duration::ZERO && started.elapsed() > hooks.timeout {
            emit_event(
                Level::WARN,
                "ga4_mcp.scratchpad.query.runtime",
                &EventContext::new().with_session_id(session_id),
                &[
                    safe_text("session_id", session_id),
                    safe_text("outcome", "timeout"),
                    safe_text("reason", "elapsed_after_completion"),
                    safe_text("duration_ms", elapsed_ms.to_string()),
                ],
            );
            return Err(AnalyticsError::scratchpad_query_timeout(hooks.timeout));
        }

        match result {
            Ok(value) => {
                emit_event(
                    Level::INFO,
                    "ga4_mcp.scratchpad.query.runtime",
                    &EventContext::new().with_session_id(session_id),
                    &[
                        safe_text("session_id", session_id),
                        safe_text("outcome", "ok"),
                        safe_text("duration_ms", elapsed_ms.to_string()),
                    ],
                );
                Ok(value)
            }
            Err(err) => {
                emit_event(
                    Level::WARN,
                    "ga4_mcp.scratchpad.query.runtime",
                    &EventContext::new().with_session_id(session_id),
                    &[
                        safe_text("session_id", session_id),
                        safe_text("outcome", "error"),
                        safe_text("duration_ms", elapsed_ms.to_string()),
                        safe_error("error", &err),
                    ],
                );
                Err(err)
            }
        }
    }

    pub fn open_connection(&self, session_id: &str) -> Result<Connection, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let db_path = self.ensure_session(&session_id)?;

        self.engine.open_session_connection(&SessionDatabaseConfig {
            database_path: db_path,
            max_memory_mb: self.config.max_memory_mb,
        })
    }

    pub fn note_table_created(
        &self,
        session_id: &str,
    ) -> Result<ScratchpadSessionSnapshot, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let entry = self.session_entry_mut(&session_id, &mut state)?;
        let max_tables_per_session = self.max_tables_per_session_limit();

        if entry.tables_used >= max_tables_per_session {
            emit_scratchpad_quota_breach(&session_id, "tables", max_tables_per_session);
            drop(state);
            cleanup_paths(removed_paths);
            return Err(AnalyticsError::scratchpad_limit(
                "tables",
                format!(
                    "max tables per session exceeded ({})",
                    max_tables_per_session
                ),
            ));
        }

        entry.tables_used += 1;
        entry.last_touched = Instant::now();
        let snapshot = snapshot_from_entry(
            entry,
            max_tables_per_session,
            self.config.max_rows_per_session,
        );

        drop(state);
        cleanup_paths(removed_paths);
        Ok(snapshot)
    }

    pub fn note_rows_ingested(
        &self,
        session_id: &str,
        additional_rows: usize,
    ) -> Result<ScratchpadSessionSnapshot, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let entry = self.session_entry_mut(&session_id, &mut state)?;

        let next_rows = entry.rows_used.saturating_add(additional_rows);
        if next_rows > self.config.max_rows_per_session {
            emit_scratchpad_quota_breach(&session_id, "rows", self.config.max_rows_per_session);
            drop(state);
            cleanup_paths(removed_paths);
            return Err(AnalyticsError::scratchpad_limit(
                "rows",
                format!(
                    "max rows per session exceeded ({})",
                    self.config.max_rows_per_session
                ),
            ));
        }

        entry.rows_used = next_rows;
        entry.last_touched = Instant::now();
        let snapshot = snapshot_from_entry(
            entry,
            self.max_tables_per_session_limit(),
            self.config.max_rows_per_session,
        );

        drop(state);
        cleanup_paths(removed_paths);
        Ok(snapshot)
    }

    pub fn note_table_dropped(
        &self,
        session_id: &str,
        rows_removed: usize,
    ) -> Result<ScratchpadSessionSnapshot, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let entry = self.session_entry_mut(&session_id, &mut state)?;
        let max_tables_per_session = self.max_tables_per_session_limit();

        entry.tables_used = entry.tables_used.saturating_sub(1);
        entry.rows_used = entry.rows_used.saturating_sub(rows_removed);
        entry.last_touched = Instant::now();
        let snapshot = snapshot_from_entry(
            entry,
            max_tables_per_session,
            self.config.max_rows_per_session,
        );

        drop(state);
        cleanup_paths(removed_paths);
        Ok(snapshot)
    }

    pub fn session_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<ScratchpadSessionSnapshot>, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let max_tables_per_session = self.max_tables_per_session_limit();
        let snapshot = state.sessions.get_mut(&session_id).map(|entry| {
            entry.last_touched = Instant::now();
            snapshot_from_entry(
                entry,
                max_tables_per_session,
                self.config.max_rows_per_session,
            )
        });

        drop(state);
        cleanup_paths(removed_paths);
        Ok(snapshot)
    }

    pub fn release_session(&self, session_id: &str) -> Result<bool, AnalyticsError> {
        let session_id = normalize_session_id(session_id)?;
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);

        let removed = state
            .sessions
            .remove(&session_id)
            .map(|entry| entry.db_path);

        drop(state);
        cleanup_paths(removed_paths);
        if let Some(path) = removed {
            cleanup_path(path);
            return Ok(true);
        }
        Ok(false)
    }

    pub fn cleanup_expired(&self) -> Result<usize, AnalyticsError> {
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let removed = removed_paths.len();
        drop(state);

        cleanup_paths(removed_paths);
        Ok(removed)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, SessionState>, AnalyticsError> {
        self.state
            .lock()
            .map_err(|_| AnalyticsError::Internal("scratchpad session state lock poisoned".into()))
    }

    fn session_entry_mut<'a>(
        &self,
        session_id: &str,
        state: &'a mut SessionState,
    ) -> Result<&'a mut SessionEntry, AnalyticsError> {
        let max_sessions_limit = self.max_sessions_limit();
        if !state.sessions.contains_key(session_id) && state.sessions.len() >= max_sessions_limit {
            emit_scratchpad_quota_breach(session_id, "sessions", max_sessions_limit);
            return Err(AnalyticsError::scratchpad_limit(
                "sessions",
                format!("max active sessions exceeded ({})", max_sessions_limit),
            ));
        }

        let now = Instant::now();
        Ok(state
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionEntry {
                db_path: session_db_path(&self.config.root_dir, session_id),
                last_touched: now,
                tables_used: 0,
                rows_used: 0,
            }))
    }

    fn reserve_ingest_capacity(
        &self,
        session_id: &str,
        incoming_rows: usize,
        mode: ScratchpadIngestMode,
    ) -> Result<(PathBuf, ScratchpadSessionSnapshot), AnalyticsError> {
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let entry = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| AnalyticsError::scratchpad_session_not_found(session_id.to_string()))?;
        let max_tables_per_session = self.max_tables_per_session_limit();

        if mode == ScratchpadIngestMode::Create && entry.tables_used >= max_tables_per_session {
            emit_scratchpad_quota_breach(session_id, "tables", max_tables_per_session);
            drop(state);
            cleanup_paths(removed_paths);
            return Err(AnalyticsError::scratchpad_limit(
                "tables",
                format!(
                    "max tables per session exceeded ({})",
                    max_tables_per_session
                ),
            ));
        }

        let next_rows = entry.rows_used.saturating_add(incoming_rows);
        if next_rows > self.config.max_rows_per_session {
            emit_scratchpad_quota_breach(session_id, "rows", self.config.max_rows_per_session);
            drop(state);
            cleanup_paths(removed_paths);
            return Err(AnalyticsError::scratchpad_limit(
                "rows",
                format!(
                    "max rows per session exceeded ({})",
                    self.config.max_rows_per_session
                ),
            ));
        }

        if mode == ScratchpadIngestMode::Create {
            entry.tables_used += 1;
        }
        entry.rows_used = next_rows;
        entry.last_touched = Instant::now();
        let snapshot = snapshot_from_entry(
            entry,
            max_tables_per_session,
            self.config.max_rows_per_session,
        );
        let db_path = entry.db_path.clone();

        drop(state);
        cleanup_paths(removed_paths);
        Ok((db_path, snapshot))
    }

    fn rollback_ingest_capacity(
        &self,
        session_id: &str,
        incoming_rows: usize,
        mode: ScratchpadIngestMode,
    ) {
        let Ok(mut state) = self.lock_state() else {
            return;
        };
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        if let Some(entry) = state.sessions.get_mut(session_id) {
            if mode == ScratchpadIngestMode::Create {
                entry.tables_used = entry.tables_used.saturating_sub(1);
            }
            entry.rows_used = entry.rows_used.saturating_sub(incoming_rows);
            entry.last_touched = Instant::now();
        }
        drop(state);
        cleanup_paths(removed_paths);
    }

    fn persist_ingest_table(
        &self,
        db_path: &Path,
        table_name: &str,
        columns: &[ScratchpadIngestColumn],
        rows: &[Map<String, Value>],
        mode: ScratchpadIngestMode,
    ) -> Result<(), AnalyticsError> {
        let conn = self
            .engine
            .open_session_connection(&SessionDatabaseConfig {
                database_path: db_path.to_path_buf(),
                max_memory_mb: self.config.max_memory_mb,
            })?;
        conn.execute_batch("BEGIN TRANSACTION;").map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to begin scratchpad ingest transaction: {err}"
            ))
        })?;

        match mode {
            ScratchpadIngestMode::Create => {
                let create_sql = create_table_sql(table_name, columns);
                if let Err(err) = conn.execute_batch(&create_sql) {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(AnalyticsError::ScratchpadEngine(format!(
                        "failed to create scratchpad table '{table_name}': {err}"
                    )));
                }
            }
            ScratchpadIngestMode::Append => {
                if let Err(err) = validate_append_target_columns(&conn, table_name, columns) {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(err);
                }
            }
        }

        if !rows.is_empty() {
            let insert_sql = insert_sql(table_name, columns);
            let mut stmt = match conn.prepare(&insert_sql) {
                Ok(stmt) => stmt,
                Err(err) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(AnalyticsError::ScratchpadEngine(format!(
                        "failed to prepare scratchpad insert statement: {err}"
                    )));
                }
            };

            for row in rows {
                let values = columns
                    .iter()
                    .map(|column| {
                        json_value_to_duck_value(
                            row.get(&column.name),
                            column.logical_type.as_str(),
                        )
                    })
                    .collect::<Vec<_>>();
                if let Err(err) = stmt.execute(params_from_iter(values.iter())) {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(AnalyticsError::ScratchpadEngine(format!(
                        "failed to insert scratchpad row into '{table_name}': {err}"
                    )));
                }
            }
        }

        conn.execute_batch("COMMIT;").map_err(|err| {
            let _ = conn.execute_batch("ROLLBACK;");
            AnalyticsError::ScratchpadEngine(format!(
                "failed to commit scratchpad ingest transaction: {err}"
            ))
        })?;

        Ok(())
    }

    fn ensure_session(&self, session_id: &str) -> Result<PathBuf, AnalyticsError> {
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let entry = self.session_entry_mut(session_id, &mut state)?;
        entry.last_touched = Instant::now();
        let db_path = entry.db_path.clone();

        drop(state);
        cleanup_paths(removed_paths);
        Ok(db_path)
    }

    fn existing_session_db_path(&self, session_id: &str) -> Result<PathBuf, AnalyticsError> {
        let mut state = self.lock_state()?;
        let removed_paths = prune_expired_locked(&mut state, self.config.session_ttl);
        let now = Instant::now();
        let path = state
            .sessions
            .get_mut(session_id)
            .map(|entry| {
                entry.last_touched = now;
                entry.db_path.clone()
            })
            .ok_or_else(|| AnalyticsError::scratchpad_session_not_found(session_id.to_string()));

        drop(state);
        cleanup_paths(removed_paths);
        path
    }
}

pub type SharedScratchpadSessionManager = Arc<ScratchpadSessionManager>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptReason {
    Timeout,
    Cancelled,
}

fn spawn_interrupt_watcher(
    interrupt_handle: Arc<duckdb::InterruptHandle>,
    stop_flag: Arc<AtomicBool>,
    hooks: ScratchpadExecutionHooks,
) -> JoinHandle<Option<InterruptReason>> {
    thread::spawn(move || {
        let started = Instant::now();
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                return None;
            }

            if hooks
                .cancel_token
                .as_ref()
                .is_some_and(ScratchpadCancelToken::is_cancelled)
            {
                interrupt_handle.interrupt();
                return Some(InterruptReason::Cancelled);
            }

            if hooks.timeout > Duration::ZERO && started.elapsed() >= hooks.timeout {
                interrupt_handle.interrupt();
                return Some(InterruptReason::Timeout);
            }

            thread::sleep(hooks.interrupt_poll_interval);
        }
    })
}

fn validate_sql_identifier(identifier: &str, field: &'static str) -> Result<(), AnalyticsError> {
    let trimmed = identifier.trim();
    if trimmed.is_empty() {
        return Err(AnalyticsError::invalid(field, "must not be empty"));
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return Err(AnalyticsError::invalid(field, "must not be empty"));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(AnalyticsError::invalid(
            field,
            "must start with an ASCII letter or underscore",
        ));
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(AnalyticsError::invalid(
            field,
            "must use [A-Za-z0-9_] characters only",
        ));
    }
    Ok(())
}

fn quote_ident(identifier: &str) -> String {
    let escaped = identifier.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, AnalyticsError> {
    let mut stmt = conn
        .prepare(
            "SELECT 1
             FROM information_schema.tables
             WHERE table_schema NOT IN ('information_schema', 'pg_catalog')
               AND table_name = ?1
             LIMIT 1",
        )
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to prepare scratchpad table existence query: {err}"
            ))
        })?;
    let mut rows = stmt.query([table_name]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to execute scratchpad table existence query: {err}"
        ))
    })?;
    Ok(rows
        .next()
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to read scratchpad table existence row: {err}"
            ))
        })?
        .is_some())
}

fn table_row_count(conn: &Connection, table_name: &str) -> Result<usize, AnalyticsError> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_ident(table_name));
    let mut stmt = conn.prepare(&sql).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to prepare scratchpad row count query for '{table_name}': {err}"
        ))
    })?;
    let row_count_raw = stmt
        .query_row([], |row| row.get::<_, i64>(0))
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to query row count for scratchpad table '{table_name}': {err}"
            ))
        })?;
    usize::try_from(row_count_raw).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "invalid row count for scratchpad table '{table_name}': {err}"
        ))
    })
}

fn ingest_mode_as_str(mode: ScratchpadIngestMode) -> &'static str {
    match mode {
        ScratchpadIngestMode::Create => "create",
        ScratchpadIngestMode::Append => "append",
    }
}

fn logical_type_to_duckdb_type(logical_type: &str) -> &'static str {
    match logical_type {
        "integer" => "BIGINT",
        "number" | "currency" | "distance" | "duration_minutes" => "DOUBLE",
        "boolean" => "BOOLEAN",
        "date" => "DATE",
        "datetime" | "timestamp" => "TIMESTAMP",
        _ => "TEXT",
    }
}

fn create_table_sql(table_name: &str, columns: &[ScratchpadIngestColumn]) -> String {
    let table_ident = quote_ident(table_name);
    let defs = columns
        .iter()
        .map(|column| {
            format!(
                "{} {}",
                quote_ident(&column.name),
                logical_type_to_duckdb_type(&column.logical_type)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("CREATE TABLE {table_ident} ({defs})")
}

fn insert_sql(table_name: &str, columns: &[ScratchpadIngestColumn]) -> String {
    let table_ident = quote_ident(table_name);
    let column_list = columns
        .iter()
        .map(|column| quote_ident(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (0..columns.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {table_ident} ({column_list}) VALUES ({placeholders})")
}

fn validate_append_target_columns(
    conn: &Connection,
    table_name: &str,
    columns: &[ScratchpadIngestColumn],
) -> Result<(), AnalyticsError> {
    let pragma_sql = format!("PRAGMA table_info('{}')", table_name);
    let mut stmt = conn.prepare(&pragma_sql).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to inspect existing table '{table_name}': {err}"
        ))
    })?;
    let mut rows = stmt.query([]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to read table schema for '{table_name}': {err}"
        ))
    })?;

    let mut existing_columns = Vec::new();
    while let Some(row) = rows.next().map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to fetch table schema row for '{table_name}': {err}"
        ))
    })? {
        let name = row.get::<_, String>(1).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to decode table column metadata for '{table_name}': {err}"
            ))
        })?;
        let data_type = row.get::<_, String>(2).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to decode table column type metadata for '{table_name}': {err}"
            ))
        })?;
        existing_columns.push((name, data_type));
    }

    if existing_columns.is_empty() {
        return Err(AnalyticsError::invalid(
            "table_name",
            format!(
                "append mode requires an existing table '{table_name}'; ingest once with append=false first"
            ),
        ));
    }

    let incoming_columns = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    let existing_column_refs = existing_columns
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    if existing_column_refs != incoming_columns {
        let existing_column_names = existing_columns
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        return Err(AnalyticsError::invalid(
            "columns",
            format!(
                "append mode requires identical column order; existing={:?}, incoming={:?}",
                existing_column_names, incoming_columns
            ),
        ));
    }

    let type_mismatches = existing_columns
        .iter()
        .zip(columns.iter())
        .filter_map(|((existing_name, existing_type), incoming)| {
            let existing_family = canonical_duckdb_storage_type(existing_type);
            let incoming_family = logical_type_to_duckdb_type(&incoming.logical_type);
            if existing_family == incoming_family {
                return None;
            }
            Some(format!(
                "{existing_name}: existing={existing_type} (logical={}), incoming={} (storage={incoming_family})",
                duckdb_type_to_logical_type(existing_type),
                incoming.logical_type
            ))
        })
        .collect::<Vec<_>>();
    if !type_mismatches.is_empty() {
        return Err(AnalyticsError::invalid(
            "columns",
            format!(
                "append mode requires type-compatible columns; mismatches: {}",
                type_mismatches.join("; ")
            ),
        ));
    }

    Ok(())
}

fn fetch_table_column_preview(
    conn: &Connection,
    table_schema: &str,
    table_name: &str,
    max_columns: usize,
) -> Result<(usize, Vec<ScratchpadTableColumnInfo>, bool), AnalyticsError> {
    let mut count_stmt = conn
        .prepare(
            "SELECT COUNT(*)
             FROM information_schema.columns
             WHERE table_schema = ?1 AND table_name = ?2",
        )
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to prepare table column count query for '{table_schema}.{table_name}': {err}"
            ))
        })?;
    let column_count_raw = count_stmt
        .query_row([table_schema, table_name], |row| row.get::<_, i64>(0))
        .map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to query table column count for '{table_schema}.{table_name}': {err}"
            ))
        })?;
    let column_count = usize::try_from(column_count_raw).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "invalid table column count for '{table_schema}.{table_name}': {err}"
        ))
    })?;

    let preview_sql = format!(
        "SELECT column_name, data_type, is_nullable
         FROM information_schema.columns
         WHERE table_schema = ?1 AND table_name = ?2
         ORDER BY ordinal_position
         LIMIT {}",
        max_columns
    );
    let mut columns_stmt = conn.prepare(&preview_sql).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to prepare table column preview query for '{table_schema}.{table_name}': {err}"
        ))
    })?;
    let mut rows = columns_stmt.query([table_schema, table_name]).map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to execute table column preview query for '{table_schema}.{table_name}': {err}"
        ))
    })?;

    let mut columns = Vec::new();
    while let Some(row) = rows.next().map_err(|err| {
        AnalyticsError::ScratchpadEngine(format!(
            "failed to read table column preview row for '{table_schema}.{table_name}': {err}"
        ))
    })? {
        let column_name = row.get::<_, String>(0).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to decode table column name for '{table_schema}.{table_name}': {err}"
            ))
        })?;
        let data_type = row.get::<_, String>(1).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to decode table column data type for '{table_schema}.{table_name}': {err}"
            ))
        })?;
        let is_nullable = row.get::<_, String>(2).map_err(|err| {
            AnalyticsError::ScratchpadEngine(format!(
                "failed to decode table column nullability for '{table_schema}.{table_name}': {err}"
            ))
        })?;
        columns.push(ScratchpadTableColumnInfo {
            name: column_name,
            logical_type: duckdb_type_to_logical_type(&data_type).to_string(),
            nullable: is_nullable.eq_ignore_ascii_case("YES"),
        });
    }

    let columns_truncated = column_count > columns.len();
    Ok((column_count, columns, columns_truncated))
}

fn duckdb_type_to_logical_type(data_type: &str) -> &'static str {
    match canonical_duckdb_storage_type(data_type) {
        "TIMESTAMP" => "timestamp",
        "BIGINT" => "integer",
        "DOUBLE" => "number",
        "BOOLEAN" => "boolean",
        "DATE" => "date",
        _ => "string",
    }
}

fn canonical_duckdb_storage_type(data_type: &str) -> &'static str {
    let upper = data_type.trim().to_ascii_uppercase();
    if upper.contains("TIMESTAMP") {
        return "TIMESTAMP";
    }
    if upper.contains("DOUBLE")
        || upper.contains("FLOAT")
        || upper.contains("REAL")
        || upper.contains("DECIMAL")
        || upper.contains("NUMERIC")
    {
        return "DOUBLE";
    }
    match upper.as_str() {
        "BIGINT" | "INTEGER" | "INT8" | "INT4" | "INT2" | "SMALLINT" | "TINYINT" | "HUGEINT"
        | "UBIGINT" | "UINTEGER" | "USMALLINT" | "UTINYINT" => "BIGINT",
        "BOOLEAN" | "BOOL" => "BOOLEAN",
        "DATE" => "DATE",
        _ => "TEXT",
    }
}

fn json_value_to_duck_value(value: Option<&Value>, logical_type: &str) -> DuckValue {
    let Some(value) = value else {
        return DuckValue::Null;
    };
    match value {
        Value::Null => DuckValue::Null,
        Value::Bool(v) => DuckValue::Boolean(*v),
        Value::Number(number) => {
            if logical_type == "integer" {
                if let Some(v) = number.as_i64() {
                    return DuckValue::BigInt(v);
                }
                if let Some(v) = number.as_u64() {
                    return DuckValue::UBigInt(v);
                }
            }
            if let Some(v) = number.as_f64() {
                DuckValue::Double(v)
            } else {
                DuckValue::Text(number.to_string())
            }
        }
        Value::String(text) => {
            if logical_type == "integer" {
                if let Ok(v) = text.parse::<i64>() {
                    return DuckValue::BigInt(v);
                }
                if let Ok(v) = text.parse::<u64>() {
                    return DuckValue::UBigInt(v);
                }
            }
            if matches!(
                logical_type,
                "number" | "currency" | "distance" | "duration_minutes"
            ) && let Ok(v) = text.parse::<f64>()
            {
                return DuckValue::Double(v);
            }
            DuckValue::Text(text.clone())
        }
        Value::Array(_) | Value::Object(_) => DuckValue::Text(value.to_string()),
    }
}

fn normalize_session_id(raw: &str) -> Result<String, AnalyticsError> {
    let session_id = raw.trim();
    if session_id.is_empty() {
        return Err(AnalyticsError::invalid("session_id", "must not be empty"));
    }
    if session_id.len() > MAX_SESSION_ID_LEN {
        return Err(AnalyticsError::invalid(
            "session_id",
            format!("must be <= {MAX_SESSION_ID_LEN} characters"),
        ));
    }
    if !session_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(AnalyticsError::invalid(
            "session_id",
            "must use [A-Za-z0-9_-] characters only",
        ));
    }
    Ok(session_id.to_string())
}

fn session_db_path(root_dir: &Path, session_id: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    root_dir.join(format!("session-{:016x}.duckdb", hasher.finish()))
}

fn prune_expired_locked(state: &mut SessionState, ttl: Duration) -> Vec<PathBuf> {
    let now = Instant::now();
    let mut expired_ids = Vec::new();

    for (session_id, entry) in &state.sessions {
        if now.duration_since(entry.last_touched) >= ttl {
            expired_ids.push(session_id.clone());
        }
    }

    expired_ids
        .into_iter()
        .filter_map(|session_id| {
            state
                .sessions
                .remove(&session_id)
                .map(|entry| entry.db_path)
        })
        .collect()
}

fn snapshot_from_entry(
    entry: &SessionEntry,
    max_tables_per_session: usize,
    max_rows_per_session: usize,
) -> ScratchpadSessionSnapshot {
    ScratchpadSessionSnapshot {
        tables_used: entry.tables_used,
        tables_remaining: max_tables_per_session.saturating_sub(entry.tables_used),
        rows_used: entry.rows_used,
        rows_remaining: max_rows_per_session.saturating_sub(entry.rows_used),
    }
}

fn session_info_from_entry(
    session_id: &str,
    entry: &SessionEntry,
    max_tables_per_session: usize,
    max_rows_per_session: usize,
    session_ttl: Duration,
    now: Instant,
) -> ScratchpadSessionInfo {
    let ttl_remaining =
        session_ttl.saturating_sub(now.saturating_duration_since(entry.last_touched));
    let snapshot = snapshot_from_entry(entry, max_tables_per_session, max_rows_per_session);
    ScratchpadSessionInfo {
        session_id: session_id.to_string(),
        tables_used: snapshot.tables_used,
        tables_remaining: snapshot.tables_remaining,
        rows_used: snapshot.rows_used,
        rows_remaining: snapshot.rows_remaining,
        ttl_seconds_remaining: ttl_remaining.as_secs(),
    }
}

fn cleanup_paths(paths: Vec<PathBuf>) {
    for path in paths {
        cleanup_path(path);
    }
}

fn cleanup_path(path: PathBuf) {
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                target: "ga4_mcp::scratchpad",
                error = %err,
                path = %path.display(),
                "failed to remove scratchpad database file"
            );
        }
    }
}

fn emit_scratchpad_quota_breach(session_id: &str, field: &'static str, limit: usize) {
    emit_event(
        Level::WARN,
        "ga4_mcp.scratchpad.quota_breach",
        &EventContext::new().with_session_id(session_id),
        &[
            safe_text("session_id", session_id),
            safe_text("field", field),
            safe_text("limit", limit.to_string()),
        ],
    );
}

fn contract_elapsed_ms(elapsed_millis: u128) -> u64 {
    if elapsed_millis > u128::from(u64::MAX) {
        u64::MAX
    } else {
        elapsed_millis as u64
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn test_root_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        std::env::temp_dir().join(format!("ga4-mcp-test-{name}-{nanos}"))
    }

    fn test_config(name: &str) -> ScratchpadSessionConfig {
        ScratchpadSessionConfig::new(Duration::from_secs(60), 4, 3, 100, 128)
            .with_root_dir(test_root_dir(name))
    }

    #[test]
    fn duckdb_engine_probe_succeeds() {
        let engine = DuckDbEngine::new().expect("engine should initialize");
        engine.probe().expect("probe must pass");
    }

    #[test]
    fn session_manager_enforces_max_sessions() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 1, 3, 100, 128)
            .with_root_dir(test_root_dir("max-sessions"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");

        let conn = manager
            .open_connection("session_a")
            .expect("first session should succeed");
        drop(conn);

        let err = manager
            .open_connection("session_b")
            .expect_err("second session should exceed limit");
        assert_eq!(err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_updates_runtime_max_sessions_limit() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 1, 3, 100, 128)
            .with_root_dir(test_root_dir("runtime-max-sessions"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");

        let conn = manager
            .open_connection("session_a")
            .expect("first session should succeed");
        drop(conn);
        assert_eq!(manager.max_sessions_limit(), 1);
        assert_eq!(
            manager
                .active_session_count()
                .expect("count should succeed"),
            1
        );

        let err = manager
            .open_connection("session_b")
            .expect_err("second session should exceed initial limit");
        assert_eq!(err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");

        manager
            .set_max_sessions_limit(2)
            .expect("updating runtime limit should succeed");
        assert_eq!(manager.max_sessions_limit(), 2);

        let conn = manager
            .open_connection("session_b")
            .expect("second session should succeed after limit update");
        drop(conn);
        assert_eq!(
            manager
                .active_session_count()
                .expect("count should succeed"),
            2
        );

        let invalid = manager
            .set_max_sessions_limit(0)
            .expect_err("zero runtime limit must fail");
        assert_eq!(invalid.code(), "INVALID_PARAMS");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_updates_runtime_max_tables_limit() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 4, 1, 100, 128)
            .with_root_dir(test_root_dir("runtime-max-tables"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");
        manager
            .open_connection("table_session")
            .expect("session should initialize");

        manager
            .note_table_created("table_session")
            .expect("first table should succeed");
        assert_eq!(manager.max_tables_per_session_limit(), 1);

        let err = manager
            .note_table_created("table_session")
            .expect_err("second table should fail at initial limit");
        assert_eq!(err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");

        manager
            .set_max_tables_per_session_limit(2)
            .expect("updating runtime table limit should succeed");
        assert_eq!(manager.max_tables_per_session_limit(), 2);
        manager
            .note_table_created("table_session")
            .expect("second table should pass after runtime update");

        let invalid = manager
            .set_max_tables_per_session_limit(0)
            .expect_err("zero runtime table limit must fail");
        assert_eq!(invalid.code(), "INVALID_PARAMS");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_enforces_table_and_row_quotas() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 4, 1, 5, 128)
            .with_root_dir(test_root_dir("quotas"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");

        manager
            .open_connection("quota_session")
            .expect("session should initialize");

        let snapshot = manager
            .note_table_created("quota_session")
            .expect("first table should succeed");
        assert_eq!(snapshot.tables_used, 1);
        assert_eq!(snapshot.tables_remaining, 0);

        let table_err = manager
            .note_table_created("quota_session")
            .expect_err("second table should fail");
        assert_eq!(table_err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");

        let rows_snapshot = manager
            .note_rows_ingested("quota_session", 4)
            .expect("rows within limit should succeed");
        assert_eq!(rows_snapshot.rows_used, 4);
        assert_eq!(rows_snapshot.rows_remaining, 1);

        let row_err = manager
            .note_rows_ingested("quota_session", 2)
            .expect_err("rows beyond limit should fail");
        assert_eq!(row_err.code(), "SCRATCHPAD_LIMIT_EXCEEDED");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_cleans_up_expired_sessions() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_millis(5), 4, 3, 100, 128)
            .with_root_dir(test_root_dir("cleanup"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");

        manager
            .open_connection("ephemeral")
            .expect("session should initialize");

        std::thread::sleep(Duration::from_millis(20));
        let removed = manager.cleanup_expired().expect("cleanup should succeed");
        assert_eq!(removed, 1);

        let snapshot = manager
            .session_snapshot("ephemeral")
            .expect("snapshot query should succeed");
        assert!(snapshot.is_none());

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_validates_session_id_charset() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("session-id"))
            .expect("manager should initialize");

        let err = manager
            .open_connection("bad/id")
            .expect_err("slash should be rejected");
        assert_eq!(err.code(), "INVALID_PARAMS");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn session_manager_rejects_unsafe_sql() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("unsafe-sql"))
            .expect("manager should initialize");

        let err = manager
            .validate_query_sql("SELECT * FROM read_csv_auto('input.csv')")
            .expect_err("external scan should be rejected");
        assert_eq!(err.code(), "SCRATCHPAD_SQL_REJECTED");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn guarded_run_respects_cancellation_token() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("cancelled"))
            .expect("manager should initialize");
        let cancel_token = ScratchpadCancelToken::default();
        cancel_token.cancel();

        let hooks = ScratchpadExecutionHooks::new(Duration::from_millis(200))
            .with_cancel_token(cancel_token);

        let err = manager
            .run_guarded("cancelled_session", "SELECT 1", hooks, |_conn| Ok(()))
            .expect_err("cancelled token should fail execution");
        assert_eq!(err.code(), "SCRATCHPAD_QUERY_CANCELLED");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn guarded_run_enforces_timeout_hook() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager =
            ScratchpadSessionManager::new(engine, test_config("timeout")).expect("manager");
        let hooks = ScratchpadExecutionHooks::new(Duration::from_millis(10))
            .with_interrupt_poll_interval(Duration::from_millis(1));

        let err = manager
            .run_guarded("timeout_session", "SELECT 1", hooks, |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            })
            .expect_err("timeout should trigger");
        assert_eq!(err.code(), "SCRATCHPAD_QUERY_TIMEOUT");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn open_session_returns_usage_and_ttl_metadata() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = test_config("open-session").with_query_timeout(Duration::from_millis(250));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");

        let info = manager
            .open_session("session_meta")
            .expect("session should open");
        assert_eq!(info.session_id, "session_meta");
        assert_eq!(info.tables_used, 0);
        assert_eq!(info.rows_used, 0);
        assert!(info.ttl_seconds_remaining <= 60);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn list_sessions_is_bounded_and_sorted() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager =
            ScratchpadSessionManager::new(engine, test_config("list-sessions")).expect("manager");

        manager.open_session("z_session").expect("z session");
        manager.open_session("a_session").expect("a session");

        let sessions = manager.list_sessions(1).expect("list should succeed");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "a_session");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn list_tables_requires_existing_session() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("missing-session"))
            .expect("manager should initialize");

        let err = manager
            .list_tables("missing", 50)
            .expect_err("missing session should fail");
        assert_eq!(err.code(), "SCRATCHPAD_SESSION_NOT_FOUND");

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn list_tables_returns_table_inventory() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager =
            ScratchpadSessionManager::new(engine, test_config("table-inventory")).expect("manager");

        let conn = manager
            .open_connection("inventory_session")
            .expect("session should open");
        conn.execute_batch("CREATE TABLE events(id INTEGER);")
            .expect("table create should succeed");
        drop(conn);

        let tables = manager
            .list_tables("inventory_session", 50)
            .expect("table inventory should succeed");
        let events_table = tables
            .iter()
            .find(|table| table.name == "events")
            .expect("events table should exist");
        assert_eq!(events_table.column_count, 1);
        assert_eq!(events_table.columns.len(), 1);
        assert!(!events_table.columns_truncated);
        assert_eq!(events_table.columns[0].name, "id");
        assert_eq!(events_table.columns[0].logical_type, "integer");
        assert!(events_table.columns[0].nullable);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn list_tables_bounds_schema_preview_columns() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("table-column-preview"))
            .expect("manager");

        let conn = manager
            .open_connection("preview_session")
            .expect("session should open");
        let mut create_sql = String::from("CREATE TABLE wide(");
        for idx in 0..40 {
            if idx > 0 {
                create_sql.push_str(", ");
            }
            create_sql.push_str(&format!("c{idx} INTEGER"));
        }
        create_sql.push(')');
        conn.execute_batch(&create_sql)
            .expect("table create should succeed");
        drop(conn);

        let tables = manager
            .list_tables("preview_session", 50)
            .expect("table inventory should succeed");
        let wide_table = tables
            .iter()
            .find(|table| table.name == "wide")
            .expect("wide table should exist");
        assert_eq!(wide_table.column_count, 40);
        assert_eq!(wide_table.columns.len(), MAX_TABLE_SCHEMA_COLUMNS_PREVIEW);
        assert!(wide_table.columns_truncated);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn ingest_rows_creates_table_and_updates_session_usage() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("ingest"))
            .expect("manager should initialize");
        manager
            .open_session("ingest_session")
            .expect("session should open");

        let columns = vec![
            ScratchpadIngestColumn {
                name: "country".to_string(),
                logical_type: "string".to_string(),
            },
            ScratchpadIngestColumn {
                name: "active_users".to_string(),
                logical_type: "integer".to_string(),
            },
        ];
        let rows = vec![
            Map::from_iter([
                ("country".to_string(), Value::String("US".to_string())),
                ("active_users".to_string(), Value::Number(12.into())),
            ]),
            Map::from_iter([
                ("country".to_string(), Value::String("AU".to_string())),
                ("active_users".to_string(), Value::Number(8.into())),
            ]),
        ];

        let stats = manager
            .ingest_rows("ingest_session", "ga_report", &columns, &rows)
            .expect("ingest should succeed");
        assert_eq!(stats.rows_inserted, 2);
        assert_eq!(stats.columns_inserted, 2);
        assert_eq!(stats.session_snapshot.tables_used, 1);
        assert_eq!(stats.session_snapshot.rows_used, 2);

        let tables = manager
            .list_tables("ingest_session", 50)
            .expect("table list should succeed");
        assert!(tables.iter().any(|table| table.name == "ga_report"));

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn ingest_rows_rolls_back_reserved_capacity_on_failure() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("ingest-rollback"))
            .expect("manager should initialize");
        manager
            .open_session("ingest_rollback")
            .expect("session should open");

        let columns = vec![ScratchpadIngestColumn {
            name: "event_name".to_string(),
            logical_type: "string".to_string(),
        }];
        let rows = vec![Map::from_iter([(
            "event_name".to_string(),
            Value::String("page_view".to_string()),
        )])];

        manager
            .ingest_rows("ingest_rollback", "events", &columns, &rows)
            .expect("first ingest should succeed");
        let err = manager
            .ingest_rows("ingest_rollback", "events", &columns, &rows)
            .expect_err("second ingest should fail due duplicate table");
        assert_eq!(err.code(), "SCRATCHPAD_ENGINE_ERROR");

        let info = manager
            .session_info("ingest_rollback")
            .expect("session info should succeed");
        assert_eq!(info.tables_used, 1);
        assert_eq!(info.rows_used, 1);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn ingest_rows_append_mode_reuses_existing_table_without_consuming_table_quota() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let config = ScratchpadSessionConfig::new(Duration::from_secs(60), 4, 1, 100, 128)
            .with_root_dir(test_root_dir("ingest-append"));
        let manager = ScratchpadSessionManager::new(engine, config).expect("manager");
        manager
            .open_session("append_session")
            .expect("session should open");

        let columns = vec![ScratchpadIngestColumn {
            name: "event_name".to_string(),
            logical_type: "string".to_string(),
        }];
        let first_rows = vec![Map::from_iter([(
            "event_name".to_string(),
            Value::String("page_view".to_string()),
        )])];
        let second_rows = vec![Map::from_iter([(
            "event_name".to_string(),
            Value::String("purchase".to_string()),
        )])];

        manager
            .ingest_rows_with_mode(
                "append_session",
                "events",
                &columns,
                &first_rows,
                ScratchpadIngestMode::Create,
            )
            .expect("create ingest should succeed");
        manager
            .ingest_rows_with_mode(
                "append_session",
                "events",
                &columns,
                &second_rows,
                ScratchpadIngestMode::Append,
            )
            .expect("append ingest should succeed");

        let info = manager
            .session_info("append_session")
            .expect("session info should resolve");
        assert_eq!(info.tables_used, 1);
        assert_eq!(info.rows_used, 2);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn ingest_rows_append_mode_rejects_mismatched_schema_and_rolls_back_rows() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("ingest-append-schema"))
            .expect("manager should initialize");
        manager
            .open_session("append_schema_session")
            .expect("session should open");

        let base_columns = vec![ScratchpadIngestColumn {
            name: "event_name".to_string(),
            logical_type: "string".to_string(),
        }];
        let base_rows = vec![Map::from_iter([(
            "event_name".to_string(),
            Value::String("page_view".to_string()),
        )])];
        manager
            .ingest_rows_with_mode(
                "append_schema_session",
                "events",
                &base_columns,
                &base_rows,
                ScratchpadIngestMode::Create,
            )
            .expect("seed ingest should succeed");

        let mismatched_columns = vec![
            ScratchpadIngestColumn {
                name: "event_name".to_string(),
                logical_type: "string".to_string(),
            },
            ScratchpadIngestColumn {
                name: "country".to_string(),
                logical_type: "string".to_string(),
            },
        ];
        let append_rows = vec![Map::from_iter([
            (
                "event_name".to_string(),
                Value::String("purchase".to_string()),
            ),
            ("country".to_string(), Value::String("AU".to_string())),
        ])];
        let err = manager
            .ingest_rows_with_mode(
                "append_schema_session",
                "events",
                &mismatched_columns,
                &append_rows,
                ScratchpadIngestMode::Append,
            )
            .expect_err("append schema mismatch should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(
            err.to_string()
                .contains("append mode requires identical column order")
        );

        let info = manager
            .session_info("append_schema_session")
            .expect("session info should resolve");
        assert_eq!(info.tables_used, 1);
        assert_eq!(info.rows_used, 1);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn ingest_rows_append_mode_rejects_incompatible_column_types() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("ingest-append-types"))
            .expect("manager should initialize");
        manager
            .open_session("append_type_session")
            .expect("session should open");

        let base_columns = vec![ScratchpadIngestColumn {
            name: "event_count".to_string(),
            logical_type: "integer".to_string(),
        }];
        let base_rows = vec![Map::from_iter([(
            "event_count".to_string(),
            Value::Number(1.into()),
        )])];
        manager
            .ingest_rows_with_mode(
                "append_type_session",
                "events",
                &base_columns,
                &base_rows,
                ScratchpadIngestMode::Create,
            )
            .expect("seed ingest should succeed");

        let mismatched_columns = vec![ScratchpadIngestColumn {
            name: "event_count".to_string(),
            logical_type: "number".to_string(),
        }];
        let append_rows = vec![Map::from_iter([(
            "event_count".to_string(),
            Value::Number(serde_json::Number::from_f64(1.5).expect("number")),
        )])];
        let err = manager
            .ingest_rows_with_mode(
                "append_type_session",
                "events",
                &mismatched_columns,
                &append_rows,
                ScratchpadIngestMode::Append,
            )
            .expect_err("append type mismatch should fail");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(
            err.to_string()
                .contains("append mode requires type-compatible columns")
        );
        assert!(err.to_string().contains("event_count"));

        let info = manager
            .session_info("append_type_session")
            .expect("session info should resolve");
        assert_eq!(info.tables_used, 1);
        assert_eq!(info.rows_used, 1);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn drop_table_reclaims_table_slot_and_rows() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("drop-table-reclaim"))
            .expect("manager");
        manager
            .open_session("drop_session")
            .expect("session should open");

        let columns = vec![ScratchpadIngestColumn {
            name: "event_name".to_string(),
            logical_type: "string".to_string(),
        }];
        let rows = vec![
            Map::from_iter([(
                "event_name".to_string(),
                Value::String("page_view".to_string()),
            )]),
            Map::from_iter([(
                "event_name".to_string(),
                Value::String("purchase".to_string()),
            )]),
        ];
        manager
            .ingest_rows_with_mode(
                "drop_session",
                "events",
                &columns,
                &rows,
                ScratchpadIngestMode::Create,
            )
            .expect("seed ingest should succeed");

        let drop_stats = manager
            .drop_table("drop_session", "events", false)
            .expect("drop should succeed");
        assert!(drop_stats.dropped);
        assert_eq!(drop_stats.rows_removed, 2);
        assert_eq!(drop_stats.session_snapshot.tables_used, 0);
        assert_eq!(drop_stats.session_snapshot.rows_used, 0);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn drop_table_if_exists_returns_not_dropped_when_missing() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("drop-table-if-exists"))
            .expect("manager");
        manager
            .open_session("drop_if_exists_session")
            .expect("session should open");

        let drop_stats = manager
            .drop_table("drop_if_exists_session", "missing_table", true)
            .expect("if_exists drop should not fail");
        assert!(!drop_stats.dropped);
        assert_eq!(drop_stats.rows_removed, 0);
        assert_eq!(drop_stats.session_snapshot.tables_used, 0);
        assert_eq!(drop_stats.session_snapshot.rows_used, 0);

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }

    #[test]
    fn drop_table_errors_when_missing_and_if_exists_false() {
        let engine: SharedScratchpadEngine = Arc::new(DuckDbEngine::new().expect("engine"));
        let manager = ScratchpadSessionManager::new(engine, test_config("drop-table-missing"))
            .expect("manager");
        manager
            .open_session("drop_missing_session")
            .expect("session should open");

        let err = manager
            .drop_table("drop_missing_session", "missing_table", false)
            .expect_err("drop should fail when table is missing");
        assert_eq!(err.code(), "INVALID_PARAMS");
        assert!(err.to_string().contains("not found"));

        let _ = std::fs::remove_dir_all(manager.config().root_dir.clone());
    }
}
