//! # Scratchpad SQL Safety
//!
//! Restricted SQL policy for DuckDB scratchpad execution.

use mcp_toolkit_policy_core::sql_read_only::{
    RestrictedSqlError, RestrictedSqlErrorCode, classify_restricted_sql,
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

    match classify_restricted_sql(sql) {
        Ok(()) => {}
        Err(err) => {
            let allow_duckdb_prefix = matches!(err.code, RestrictedSqlErrorCode::NotReadOnlyPrefix)
                && DUCKDB_ALLOWED_PREFIXES
                    .iter()
                    .any(|prefix| upper.starts_with(prefix));
            if !allow_duckdb_prefix {
                return Err(map_core_error(err));
            }
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

fn map_core_error(err: RestrictedSqlError) -> ScratchpadSqlPolicyError {
    let code = match err.code {
        RestrictedSqlErrorCode::EmptySql => ScratchpadSqlPolicyCode::EmptySql,
        RestrictedSqlErrorCode::UnterminatedToken => ScratchpadSqlPolicyCode::UnterminatedToken,
        RestrictedSqlErrorCode::MultipleStatements => ScratchpadSqlPolicyCode::MultipleStatements,
        RestrictedSqlErrorCode::NotReadOnlyPrefix => ScratchpadSqlPolicyCode::NotReadOnlyPrefix,
        RestrictedSqlErrorCode::ForbiddenKeyword => ScratchpadSqlPolicyCode::ForbiddenKeyword,
        RestrictedSqlErrorCode::ForbiddenFunction => ScratchpadSqlPolicyCode::ForbiddenFunction,
        RestrictedSqlErrorCode::ExplainNotReadOnly => ScratchpadSqlPolicyCode::ExplainNotReadOnly,
        RestrictedSqlErrorCode::ClassifierUnavailable => {
            ScratchpadSqlPolicyCode::ClassifierUnavailable
        }
    };

    ScratchpadSqlPolicyError::new(code, err.message)
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
}
