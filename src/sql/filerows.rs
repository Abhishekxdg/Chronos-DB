//! The rows of a delimited file (the data of a file_fdw foreign table): CSV with quotes, or Postgres's text format
//! with backslash escapes. Each row comes as its fields, a field equal to the null string being null.

use super::*;

fn denied() -> SqlError {
    SqlError { code: "42501", message: "permission denied to read a server file".into(), info: None }
}

/// Postgres's 42501 for setting file_fdw's `filename` option without the privileges of
/// pg_read_server_files (a file_fdw foreign table here is a view over `__file_rows`).
pub(super) fn filename_denied() -> SqlError {
    SqlError {
        code: "42501",
        message: "permission denied to set the \"filename\" option of a file_fdw foreign table".into(),
        info: Some(Box::new(Violation {
            detail: Some("Only roles with privileges of the \"pg_read_server_files\" role may set this option.".into()),
            ..Violation::default()
        })),
    }
}

/// `__file_rows(path, format, header, delimiter, quote, null)`. Read as the role reading it, which
/// must have the privileges of pg_read_server_files (stricter than Postgres, whose file_fdw checks
/// them only when the foreign table is made: here a view of anyone's could name any file); never
/// an agent's session; and only where the server lets anyone (see `copy::server_files_allowed`).
pub(super) fn rows(a: &[Value]) -> Result<Vec<Vec<Value>>> {
    if !roles::may_use_server_files(roles::READ_SERVER_FILES) || !super::copy::server_files_allowed() {
        return Err(denied());
    }
    let path = a[0].to_text();
    let text = std::fs::read_to_string(&path).map_err(|e| SqlError {
        code: "58P01",
        message: format!("could not open file \"{path}\" for reading: {e}"),
        info: None,
    })?;
    let csv = a[1].to_text().eq_ignore_ascii_case("csv");
    let header = matches!(&a[2], Value::Bool(true)) || matches!(a[2].to_text().as_str(), "true" | "t" | "on" | "1");
    let ch = |v: &Value, d: char| v.to_text().chars().next().unwrap_or(d);
    let (delim, quote) = (ch(&a[3], if csv { ',' } else { '\t' }), ch(&a[4], '"'));
    let nul = if a[5] == Value::Null { if csv { String::new() } else { "\\N".into() } } else { a[5].to_text() };
    // (in the text format the marker was read as one character)
    let nul = if !csv && nul == "\\N" { "\u{1}".to_string() } else { nul };
    let mut out: Vec<Vec<Value>> = vec![];
    for (i, rec) in records(&text, csv, delim, quote).into_iter().enumerate() {
        if i == 0 && header {
            continue;
        }
        out.push(
            rec.into_iter().map(|(f, quoted)| if !quoted && f == nul { Value::Null } else { Value::Text(f) }).collect(),
        );
    }
    Ok(out)
}

/// The records: each a list of (field, whether it was quoted).
fn records(text: &str, csv: bool, delim: char, quote: char) -> Vec<Vec<(String, bool)>> {
    let c: Vec<char> = text.chars().collect();
    let (mut out, mut i) = (vec![], 0);
    while i < c.len() {
        let (mut rec, mut field, mut quoted) = (vec![], String::new(), false);
        loop {
            if i >= c.len() {
                rec.push((std::mem::take(&mut field), quoted));
                break;
            }
            let x = c[i];
            if csv && x == quote && field.is_empty() && !quoted {
                quoted = true;
                i += 1;
                while i < c.len() {
                    if c[i] == quote {
                        if c.get(i + 1) == Some(&quote) {
                            field.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    field.push(c[i]);
                    i += 1;
                }
                continue;
            }
            if !csv && x == '\\' && i + 1 < c.len() {
                field.push(match c[i + 1] {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    // (the null marker \N, kept apart from an N)
                    'N' => '\u{1}',
                    other => other,
                });
                i += 2;
                continue;
            }
            if x == delim {
                rec.push((std::mem::take(&mut field), quoted));
                quoted = false;
                i += 1;
                continue;
            }
            if x == '\n' || x == '\r' {
                rec.push((std::mem::take(&mut field), quoted));
                i += if x == '\r' && c.get(i + 1) == Some(&'\n') { 2 } else { 1 };
                break;
            }
            field.push(x);
            i += 1;
        }
        // (a blank line is no record)
        if !(rec.len() == 1 && rec[0].0.is_empty() && !rec[0].1) {
            out.push(rec);
        }
    }
    out
}
