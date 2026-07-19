//! Split a migration file into individual SQL statements.
//!
//! `database::transaction` takes one SQL string per statement, so a file has
//! to be split on `;` — but only on semicolons that sit at the top level:
//!
//! - `'...'` single-quoted strings (with `''` escape)
//! - `E'...'` Postgres escape strings (backslash escapes, `\'` does not close)
//! - `"..."` quoted identifiers
//! - `$$...$$` / `$tag$...$tag$` Postgres dollar quoting
//! - `-- line comments` and `/* block comments */` (nested, as in Postgres)
//!
//! Statements are trimmed and the terminating `;` is dropped. Chunks that
//! contain only whitespace and comments are discarded (the database worker
//! rejects empty SQL). A `$1`-style positional parameter is not a dollar
//! quote: a dollar quote tag must not start with a digit.

/// Scanner state.
#[derive(Debug, PartialEq)]
enum State {
    Normal,
    LineComment,
    BlockComment { depth: u32 },
    SingleQuote { backslash_escapes: bool },
    DoubleQuote,
    DollarQuote { tag: String },
}

/// Split `sql` into statements. Errors on unterminated strings, quoted
/// identifiers, block comments, or dollar quotes.
pub fn split_statements(sql: &str) -> Result<Vec<String>, String> {
    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut statements = Vec::new();
    // Byte offset of the first real content of the current chunk — `None`
    // while we've only seen whitespace/comments, so a leading header comment
    // is not glued onto the statement that follows it.
    let mut start: Option<usize> = None;
    let mut i = 0usize;

    let mark = |start: &mut Option<usize>, i: usize| {
        if start.is_none() {
            *start = Some(i);
        }
    };

    while i < bytes.len() {
        let c = bytes[i];
        match &mut state {
            State::Normal => match c {
                b';' => {
                    if let Some(s) = start.take() {
                        let stmt = sql[s..i].trim();
                        if !stmt.is_empty() {
                            statements.push(stmt.to_string());
                        }
                    }
                    i += 1;
                }
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    state = State::LineComment;
                    i += 2;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = State::BlockComment { depth: 1 };
                    i += 2;
                }
                b'\'' => {
                    // `E'...'` (or `e'...'`) enables backslash escapes. The E
                    // must be a standalone token prefix, not the tail of an
                    // identifier (`TABLE'` cannot occur; `RAISE'msg'` could).
                    let backslash_escapes = i >= 1
                        && (bytes[i - 1] == b'E' || bytes[i - 1] == b'e')
                        && (i < 2 || !is_ident_char(bytes[i - 2]));
                    state = State::SingleQuote { backslash_escapes };
                    mark(&mut start, i);
                    i += 1;
                }
                b'"' => {
                    state = State::DoubleQuote;
                    mark(&mut start, i);
                    i += 1;
                }
                b'$' => {
                    mark(&mut start, i);
                    if let Some(tag) = read_dollar_tag(&sql[i..]) {
                        i += tag.len() + 2;
                        state = State::DollarQuote { tag };
                    } else {
                        i += 1;
                    }
                }
                _ => {
                    if !c.is_ascii_whitespace() {
                        mark(&mut start, i);
                    }
                    i += 1;
                }
            },
            State::LineComment => {
                if c == b'\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BlockComment { depth } => {
                if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    *depth -= 1;
                    if *depth == 0 {
                        state = State::Normal;
                    }
                    i += 2;
                } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    *depth += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            State::SingleQuote { backslash_escapes } => {
                if *backslash_escapes && c == b'\\' {
                    i += 2; // skip the escaped char, whatever it is
                } else if c == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2; // '' escape — still inside the string
                    } else {
                        state = State::Normal;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            State::DoubleQuote => {
                if c == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        i += 2; // "" escape
                    } else {
                        state = State::Normal;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            State::DollarQuote { tag } => {
                let closer_len = tag.len() + 2;
                if c == b'$' && sql[i..].starts_with(&format!("${tag}$")) {
                    state = State::Normal;
                    i += closer_len;
                } else {
                    i += 1;
                }
            }
        }
    }

    match state {
        State::Normal | State::LineComment => {}
        State::BlockComment { .. } => return Err("unterminated block comment".into()),
        State::SingleQuote { .. } => return Err("unterminated string literal".into()),
        State::DoubleQuote => return Err("unterminated quoted identifier".into()),
        State::DollarQuote { tag } => {
            return Err(format!("unterminated dollar-quoted block (${tag}$)"))
        }
    }

    if let Some(s) = start {
        let stmt = sql[s..].trim();
        if !stmt.is_empty() {
            statements.push(stmt.to_string());
        }
    }
    Ok(statements)
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// At a `$` in normal state, read a dollar-quote opener `$tag$` and return
/// its tag (may be empty for `$$`). Returns `None` when this `$` is not a
/// dollar-quote opener (e.g. positional param `$1`).
fn read_dollar_tag(s: &str) -> Option<String> {
    debug_assert!(s.starts_with('$'));
    let rest = &s[1..];
    let end = rest.find('$')?;
    let tag = &rest[..end];
    let mut chars = tag.chars();
    match chars.next() {
        None => Some(String::new()), // $$
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            if chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
                Some(tag.to_string())
            } else {
                None
            }
        }
        Some(_) => None, // $1, $2 … positional params
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<String> {
        split_statements(sql).unwrap()
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(
            split("CREATE TABLE a (id int); CREATE TABLE b (id int);"),
            ["CREATE TABLE a (id int)", "CREATE TABLE b (id int)"]
        );
    }

    #[test]
    fn last_statement_without_semicolon() {
        assert_eq!(split("SELECT 1;\nSELECT 2"), ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_string_literal() {
        assert_eq!(
            split("INSERT INTO t VALUES ('a;b'); SELECT 1;"),
            ["INSERT INTO t VALUES ('a;b')", "SELECT 1"]
        );
    }

    #[test]
    fn doubled_quote_escape_inside_string() {
        assert_eq!(
            split("INSERT INTO t VALUES ('it''s; fine'); SELECT 1;"),
            ["INSERT INTO t VALUES ('it''s; fine')", "SELECT 1"]
        );
    }

    #[test]
    fn escape_string_with_backslash_quote() {
        assert_eq!(
            split(r"INSERT INTO t VALUES (E'a\'b;c'); SELECT 1;"),
            [r"INSERT INTO t VALUES (E'a\'b;c')", "SELECT 1"]
        );
    }

    #[test]
    fn plain_string_does_not_treat_backslash_as_escape() {
        // In standard SQL '\' is a complete one-char string; the following
        // ; terminates the statement.
        assert_eq!(split(r"SELECT '\'; SELECT 2;"), [r"SELECT '\'", "SELECT 2"]);
    }

    #[test]
    fn semicolon_inside_quoted_identifier() {
        assert_eq!(
            split(r#"CREATE TABLE "weird;name" (id int);"#),
            [r#"CREATE TABLE "weird;name" (id int)"#]
        );
    }

    #[test]
    fn dollar_quoted_function_body() {
        let sql = "CREATE FUNCTION f() RETURNS trigger AS $$\nBEGIN\n  UPDATE t SET x = 1;\n  RETURN NEW;\nEND;\n$$ LANGUAGE plpgsql; SELECT 1;";
        let stmts = split(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("UPDATE t SET x = 1;"));
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn tagged_dollar_quote_with_inner_dollars() {
        let sql = "DO $body$ BEGIN PERFORM $$nested; text$$; END; $body$; SELECT 1;";
        let stmts = split(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("DO $body$"));
    }

    #[test]
    fn positional_params_are_not_dollar_quotes() {
        assert_eq!(
            split("SELECT * FROM t WHERE a = $1; SELECT $2;"),
            ["SELECT * FROM t WHERE a = $1", "SELECT $2"]
        );
    }

    #[test]
    fn line_comment_hides_semicolon() {
        assert_eq!(
            split("SELECT 1 -- trailing; comment\n; SELECT 2;"),
            ["SELECT 1 -- trailing; comment", "SELECT 2"]
        );
    }

    #[test]
    fn block_comment_hides_semicolon_and_nests() {
        assert_eq!(
            split("SELECT /* a; /* nested; */ b; */ 1; SELECT 2;"),
            ["SELECT /* a; /* nested; */ b; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn comment_only_chunks_are_dropped() {
        assert_eq!(
            split("-- header comment\n\nSELECT 1;\n-- trailer\n/* done */\n"),
            ["SELECT 1"]
        );
        assert_eq!(split("-- nothing here\n"), Vec::<String>::new());
        assert_eq!(split("  ;;  ; "), Vec::<String>::new());
    }

    #[test]
    fn unterminated_constructs_error() {
        assert!(split_statements("SELECT 'oops").is_err());
        assert!(split_statements("SELECT \"oops").is_err());
        assert!(split_statements("SELECT /* oops").is_err());
        assert!(split_statements("DO $$ BEGIN END;").is_err());
        assert!(split_statements("DO $tag$ oops $othertag$;").is_err());
    }

    #[test]
    fn line_comment_at_eof_is_fine() {
        assert_eq!(split("SELECT 1; -- done"), ["SELECT 1"]);
    }

    #[test]
    fn e_prefix_only_when_standalone_token() {
        // `TYPE'...'`: the quote follows an identifier ending in E, but that
        // E is part of `TYPE`, so no backslash escaping: '\' closes at the
        // next quote.
        let stmts = split(r"SELECT CAST('a' AS SOMETYPE'\'); SELECT 2;");
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn realistic_sqlite_migration() {
        let sql = "\
-- create users
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL DEFAULT 'anon;user'
);

CREATE INDEX idx_users_name ON users (name);
";
        let stmts = split(sql);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'anon;user'"));
        assert!(stmts[1].starts_with("CREATE INDEX"));
    }
}
