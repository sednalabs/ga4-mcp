//! # Shared scratchpad integration
//!
//! GA4 uses the toolkit's DuckDB engine, private database custody, scoped
//! session registry and quotas. Provider-specific row normalization, diagnostics
//! and configured policy authority remain at the GA4 tool boundary.

pub use mcp_toolkit_scratchpad::{
    DuckDbEngine, ScratchpadCancelToken, ScratchpadDropTableStats, ScratchpadEngine,
    ScratchpadError, ScratchpadExecutionHooks, ScratchpadIngestColumn, ScratchpadIngestMode,
    ScratchpadIngestStats, ScratchpadQueryProjection, ScratchpadScope, ScratchpadSessionConfig,
    ScratchpadSessionInfo, ScratchpadSessionManager, ScratchpadSessionSnapshot,
    ScratchpadTableColumnInfo, ScratchpadTableInfo, SessionDatabaseConfig, SharedScratchpadEngine,
    SharedScratchpadSessionManager,
};
