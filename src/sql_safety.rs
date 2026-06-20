//! # Scratchpad SQL Safety
//!
//! Restricted SQL policy for DuckDB scratchpad execution.

use mcp_toolkit_policy_core::{
    DecisionCode, SQL_POLICY_CONTRACT_VERSION, SqlRestrictedPolicyInput,
};
use mcp_toolkit_policy_runtime::{
    PolicyAuthorityDecision, PolicyRuntimeMode, configured_sql_restricted_policy_authority,
    sql_restricted_policy_authority,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchpadSqlPolicyCode {
    EmptySql,
    UnterminatedToken,
    MultipleStatements,
    NotReadOnlyPrefix,
    ForbiddenKeyword,
    ForbiddenFunction,
    ExplainNotReadOnly,
    ClassifierUnavailable,
    SqlTooLarge,
    DuckDbForbiddenKeyword,
    DuckDbForbiddenFunction,
}

impl ScratchpadSqlPolicyCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmptySql => "EMPTY_SQL",
            Self::UnterminatedToken => "UNTERMINATED_TOKEN",
            Self::MultipleStatements => "MULTIPLE_STATEMENTS",
            Self::NotReadOnlyPrefix => "NOT_READ_ONLY_PREFIX",
            Self::ForbiddenKeyword => "FORBIDDEN_KEYWORD",
            Self::ForbiddenFunction => "FORBIDDEN_FUNCTION",
            Self::ExplainNotReadOnly => "EXPLAIN_NOT_READ_ONLY",
            Self::ClassifierUnavailable => "CLASSIFIER_UNAVAILABLE",
            Self::SqlTooLarge => "SQL_TOO_LARGE",
            Self::DuckDbForbiddenKeyword => "DUCKDB_FORBIDDEN_KEYWORD",
            Self::DuckDbForbiddenFunction => "DUCKDB_FORBIDDEN_FUNCTION",
        }
    }
}

impl std::fmt::Display for ScratchpadSqlPolicyCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadSqlPolicyError {
    pub code: ScratchpadSqlPolicyCode,
    pub message: String,
}

impl ScratchpadSqlPolicyError {
    fn new(code: ScratchpadSqlPolicyCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ScratchpadSqlPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ScratchpadSqlPolicyError {}

const DUCKDB_ALLOWED_PREFIXES: [&str; 2] = ["DESCRIBE", "SUMMARIZE"];
const DUCKDB_FORBIDDEN_KEYWORDS: [&str; 6] =
    ["INSTALL", "LOAD", "ATTACH", "DETACH", "EXPORT", "IMPORT"];
const DUCKDB_FORBIDDEN_FUNCTIONS: [&str; 15] = [
    "READ_CSV",
    "READ_CSV_AUTO",
    "READ_PARQUET",
    "READ_JSON",
    "READ_JSON_AUTO",
    "READ_NDJSON",
    "READ_BLOB",
    "READ_TEXT",
    "READ_XLSX",
    "CSV_SCAN",
    "PARQUET_SCAN",
    "JSON_SCAN",
    "POSTGRES_SCAN",
    "MYSQL_SCAN",
    "SQLITE_SCAN",
];

pub fn validate_scratchpad_sql(
    sql: &str,
    max_sql_bytes: usize,
) -> Result<(), ScratchpadSqlPolicyError> {
    if max_sql_bytes == 0 {
        return Err(ScratchpadSqlPolicyError::new(
            ScratchpadSqlPolicyCode::ClassifierUnavailable,
            "scratchpad sql policy is unavailable due to invalid max_sql_bytes",
        ));
    }

    if sql.len() > max_sql_bytes {
        return Err(ScratchpadSqlPolicyError::new(
            ScratchpadSqlPolicyCode::SqlTooLarge,
            format!("sql payload exceeds max size of {max_sql_bytes} bytes"),
        ));
    }

    let lexical = lexical_surface(sql)?;
    let trimmed = lexical.trim();
    let upper = trimmed.to_ascii_uppercase();

    let decision = evaluate_scratchpad_sql_policy(sql);
    if !decision.allow {
        let code = scratchpad_error_code(decision.code.as_deref());
        let allow_duckdb_prefix = matches!(code, ScratchpadSqlPolicyCode::NotReadOnlyPrefix)
            && DUCKDB_ALLOWED_PREFIXES
                .iter()
                .any(|prefix| upper.starts_with(prefix));
        if !allow_duckdb_prefix {
            return Err(ScratchpadSqlPolicyError::new(
                code,
                scratchpad_error_message(code),
            ));
        }
    }

    if contains_forbidden_keyword(&upper) {
        return Err(ScratchpadSqlPolicyError::new(
            ScratchpadSqlPolicyCode::DuckDbForbiddenKeyword,
            "scratchpad policy rejected DuckDB extension/file keyword",
        ));
    }

    if contains_forbidden_function_call(&upper) {
        return Err(ScratchpadSqlPolicyError::new(
            ScratchpadSqlPolicyCode::DuckDbForbiddenFunction,
            "scratchpad policy rejected DuckDB external scan/read function",
        ));
    }

    Ok(())
}

/// Evaluates canonical restricted SQL policy through the configured toolkit authority.
pub fn evaluate_scratchpad_sql_policy(sql: &str) -> PolicyAuthorityDecision {
    let input = SqlRestrictedPolicyInput {
        policy_contract_version: SQL_POLICY_CONTRACT_VERSION.to_string(),
        sql: sql.to_string(),
    };
    configured_sql_restricted_policy_authority().evaluate(&input)
}

/// Evaluates canonical restricted SQL policy through a specific toolkit authority mode.
pub fn evaluate_scratchpad_sql_policy_with_mode(
    sql: &str,
    runtime_mode: PolicyRuntimeMode,
) -> PolicyAuthorityDecision {
    let input = SqlRestrictedPolicyInput {
        policy_contract_version: SQL_POLICY_CONTRACT_VERSION.to_string(),
        sql: sql.to_string(),
    };
    sql_restricted_policy_authority(runtime_mode).evaluate(&input)
}

fn scratchpad_error_code(code: Option<&str>) -> ScratchpadSqlPolicyCode {
    match code.and_then(DecisionCode::parse) {
        Some(DecisionCode::EmptySql) => ScratchpadSqlPolicyCode::EmptySql,
        Some(DecisionCode::UnterminatedToken) => ScratchpadSqlPolicyCode::UnterminatedToken,
        Some(DecisionCode::MultipleStatements) => ScratchpadSqlPolicyCode::MultipleStatements,
        Some(DecisionCode::NotReadOnlyPrefix) => ScratchpadSqlPolicyCode::NotReadOnlyPrefix,
        Some(DecisionCode::ForbiddenKeyword) => ScratchpadSqlPolicyCode::ForbiddenKeyword,
        Some(DecisionCode::ForbiddenFunction) => ScratchpadSqlPolicyCode::ForbiddenFunction,
        Some(DecisionCode::ExplainNotReadOnly) => ScratchpadSqlPolicyCode::ExplainNotReadOnly,
        Some(DecisionCode::ClassifierUnavailable)
        | Some(DecisionCode::SparkRuntimeUnavailable)
        | Some(DecisionCode::InvalidInput)
        | None
        | Some(_) => ScratchpadSqlPolicyCode::ClassifierUnavailable,
    }
}

fn scratchpad_error_message(code: ScratchpadSqlPolicyCode) -> &'static str {
    match code {
        ScratchpadSqlPolicyCode::EmptySql => "sql must not be empty",
        ScratchpadSqlPolicyCode::UnterminatedToken => {
            "restricted mode could not parse SQL lexical surface"
        }
        ScratchpadSqlPolicyCode::MultipleStatements => {
            "restricted mode allows only a single SQL statement"
        }
        ScratchpadSqlPolicyCode::NotReadOnlyPrefix => {
            "restricted mode allows only allowlisted SQL prefixes"
        }
        ScratchpadSqlPolicyCode::ForbiddenKeyword => "restricted mode rejected write/admin SQL",
        ScratchpadSqlPolicyCode::ForbiddenFunction => {
            "restricted mode rejected unsafe function call"
        }
        ScratchpadSqlPolicyCode::ExplainNotReadOnly => {
            "restricted mode allows EXPLAIN only for read-only statements"
        }
        ScratchpadSqlPolicyCode::ClassifierUnavailable => {
            "restricted mode policy classifier unavailable"
        }
        ScratchpadSqlPolicyCode::SqlTooLarge => "sql payload exceeds scratchpad size limit",
        ScratchpadSqlPolicyCode::DuckDbForbiddenKeyword => {
            "scratchpad policy rejected DuckDB extension/file keyword"
        }
        ScratchpadSqlPolicyCode::DuckDbForbiddenFunction => {
            "scratchpad policy rejected DuckDB external scan/read function"
        }
    }
}

fn contains_forbidden_keyword(surface_upper: &str) -> bool {
    surface_upper
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .any(|token| DUCKDB_FORBIDDEN_KEYWORDS.contains(&token))
}

fn contains_forbidden_function_call(surface_upper: &str) -> bool {
    let bytes = surface_upper.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if is_identifier_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_identifier_continue(bytes[i]) {
                i += 1;
            }

            let token = &surface_upper[start..i];
            let mut lookahead = i;
            while lookahead < bytes.len() && bytes[lookahead].is_ascii_whitespace() {
                lookahead += 1;
            }

            if lookahead < bytes.len()
                && bytes[lookahead] == b'('
                && DUCKDB_FORBIDDEN_FUNCTIONS.contains(&token)
            {
                return true;
            }
            continue;
        }

        i += 1;
    }

    false
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn lexical_surface(sql: &str) -> Result<String, ScratchpadSqlPolicyError> {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        LineComment,
        BlockComment(u32),
    }

    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let mut state = State::Normal;
    let mut dollar_tag: Option<Vec<u8>> = None;

    while i < bytes.len() {
        if let Some(tag) = dollar_tag.as_ref() {
            if bytes[i..].starts_with(tag) {
                for _ in 0..tag.len() {
                    out.push(' ');
                }
                i += tag.len();
                dollar_tag = None;
                continue;
            }

            out.push(mask_hidden_byte(bytes[i]));
            i += 1;
            continue;
        }

        match state {
            State::Normal => {
                if bytes[i..].starts_with(b"--") {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    state = State::LineComment;
                    continue;
                }
                if bytes[i..].starts_with(b"/*") {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    state = State::BlockComment(1);
                    continue;
                }
                if bytes[i] == b'\'' {
                    out.push(' ');
                    i += 1;
                    state = State::SingleQuote;
                    continue;
                }
                if bytes[i] == b'"' {
                    out.push(' ');
                    i += 1;
                    state = State::DoubleQuote;
                    continue;
                }
                if let Some(tag_len) = parse_dollar_tag(&bytes[i..]) {
                    let tag = bytes[i..i + tag_len].to_vec();
                    for _ in 0..tag_len {
                        out.push(' ');
                    }
                    i += tag_len;
                    dollar_tag = Some(tag);
                    continue;
                }

                out.push(mask_byte(bytes[i]));
                i += 1;
            }
            State::SingleQuote => {
                if bytes[i] == b'\'' {
                    out.push(' ');
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\'' {
                        out.push(' ');
                        i += 1;
                        continue;
                    }
                    state = State::Normal;
                    continue;
                }

                out.push(mask_hidden_byte(bytes[i]));
                i += 1;
            }
            State::DoubleQuote => {
                if bytes[i] == b'"' {
                    out.push(' ');
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'"' {
                        out.push(' ');
                        i += 1;
                        continue;
                    }
                    state = State::Normal;
                    continue;
                }

                out.push(mask_hidden_byte(bytes[i]));
                i += 1;
            }
            State::LineComment => {
                if bytes[i] == b'\n' {
                    out.push('\n');
                    i += 1;
                    state = State::Normal;
                    continue;
                }
                out.push(' ');
                i += 1;
            }
            State::BlockComment(depth) => {
                if bytes[i..].starts_with(b"/*") {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    state = State::BlockComment(depth + 1);
                    continue;
                }
                if bytes[i..].starts_with(b"*/") {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    continue;
                }
                out.push(mask_hidden_byte(bytes[i]));
                i += 1;
            }
        }
    }

    if dollar_tag.is_some()
        || matches!(
            state,
            State::SingleQuote | State::DoubleQuote | State::BlockComment(_)
        )
    {
        return Err(ScratchpadSqlPolicyError::new(
            ScratchpadSqlPolicyCode::UnterminatedToken,
            "scratchpad policy rejected SQL with unterminated quote/comment",
        ));
    }

    Ok(out)
}

fn parse_dollar_tag(input: &[u8]) -> Option<usize> {
    if input.first().copied()? != b'$' {
        return None;
    }

    let mut j = 1usize;
    while j < input.len() {
        if input[j] == b'$' {
            return Some(j + 1);
        }
        let c = input[j] as char;
        if !c.is_ascii_alphanumeric() && c != '_' {
            return None;
        }
        j += 1;
    }
    None
}

fn mask_byte(b: u8) -> char {
    match b {
        b'\n' => '\n',
        b'\r' => '\r',
        b'\t' => '\t',
        0x20..=0x7e => b as char,
        _ => ' ',
    }
}

fn mask_hidden_byte(b: u8) -> char {
    match b {
        b'\n' => '\n',
        b'\r' => '\r',
        b'\t' => '\t',
        _ => ' ',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn must_err<T>(result: Result<T, ScratchpadSqlPolicyError>) -> ScratchpadSqlPolicyError {
        match result {
            Ok(_) => panic!("expected error"),
            Err(err) => err,
        }
    }

    #[test]
    fn allows_read_only_select() {
        assert!(validate_scratchpad_sql("SELECT 1", 1024).is_ok());
    }

    #[test]
    fn rust_authority_allows_read_only_sql_with_provenance() {
        let decision =
            evaluate_scratchpad_sql_policy_with_mode("SELECT 1", PolicyRuntimeMode::Rust);

        assert!(decision.allow);
        assert_eq!(decision.runtime_mode, PolicyRuntimeMode::Rust);
        assert_eq!(
            decision.policy_contract_version.as_deref(),
            Some(SQL_POLICY_CONTRACT_VERSION)
        );
        assert_eq!(
            decision.decision_source,
            "mcp_toolkit_policy_runtime.sql_restricted.rust"
        );
    }

    #[test]
    fn rejects_duckdb_install_keyword() {
        let err = must_err(validate_scratchpad_sql("INSTALL httpfs", 1024));
        assert_eq!(err.code, ScratchpadSqlPolicyCode::NotReadOnlyPrefix);
    }

    #[test]
    fn rejects_duckdb_external_scan_function() {
        let err = must_err(validate_scratchpad_sql(
            "SELECT * FROM read_csv_auto('a.csv')",
            1024,
        ));
        assert_eq!(err.code, ScratchpadSqlPolicyCode::DuckDbForbiddenFunction);
    }

    #[test]
    fn allows_describe_prefix() {
        assert!(validate_scratchpad_sql("DESCRIBE SELECT 1", 1024).is_ok());
    }

    #[test]
    fn allows_forbidden_keyword_inside_literal() {
        assert!(validate_scratchpad_sql("SELECT 'INSTALL httpfs'", 1024).is_ok());
    }

    #[test]
    fn rejects_sql_over_size_limit() {
        let err = must_err(validate_scratchpad_sql("SELECT 12345", 4));
        assert_eq!(err.code, ScratchpadSqlPolicyCode::SqlTooLarge);
    }

    #[test]
    fn rejects_multiple_statements() {
        let err = must_err(validate_scratchpad_sql("SELECT 1; SELECT 2", 1024));
        assert_eq!(err.code, ScratchpadSqlPolicyCode::MultipleStatements);
    }

    #[test]
    fn preserves_canonical_write_error_surface() {
        let err = must_err(validate_scratchpad_sql("INSERT INTO t VALUES (1)", 1024));
        assert_eq!(err.code, ScratchpadSqlPolicyCode::NotReadOnlyPrefix);
        assert_eq!(
            err.message,
            "restricted mode allows only allowlisted SQL prefixes"
        );
    }
}
