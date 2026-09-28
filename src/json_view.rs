//! Pretty JSON rendering of a query result: one array, one object per row.
//! Pure functions so the formatting can be unit-tested.

use serde_json::Value;

use crate::db::CellKind;

/// Indentation of one nesting level.
const INDENT: &str = "  ";

/// Pretty JSON lines for `rows`: an array of objects keyed by `headers`
/// in column order (duplicate column names are all kept).
pub fn pretty_rows(
    headers: &[String],
    rows: &[Vec<String>],
    kinds: &[Vec<CellKind>],
) -> Vec<String> {
    if rows.is_empty() {
        return vec!["[]".to_string()];
    }
    let field_indent = INDENT.repeat(2);
    let mut out = String::from("[\n");
    for (r, row) in rows.iter().enumerate() {
        if headers.is_empty() {
            out.push_str(INDENT);
            out.push_str("{}");
        } else {
            out.push_str(INDENT);
            out.push_str("{\n");
            for (c, header) in headers.iter().enumerate() {
                let text = row.get(c).map(String::as_str).unwrap_or("");
                let kind = kinds
                    .get(r)
                    .and_then(|k| k.get(c))
                    .copied()
                    .unwrap_or(CellKind::Untyped);
                out.push_str(&field_indent);
                out.push_str(&quote(header));
                out.push_str(": ");
                out.push_str(&value_json(text, kind).replace('\n', &format!("\n{field_indent}")));
                if c + 1 < headers.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(INDENT);
            out.push('}');
        }
        if r + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out.lines().map(ToString::to_string).collect()
}

/// JSON for one cell (may span lines for nested objects/arrays).
fn value_json(text: &str, kind: CellKind) -> String {
    match kind {
        CellKind::Null => "null".to_string(),
        CellKind::Integer => text.to_string(),
        CellKind::Real if is_json_number(text) => text.to_string(),
        CellKind::Real | CellKind::Blob => quote(text),
        CellKind::Text => nested_or_string(text),
        CellKind::Untyped => match text {
            "t" => "true".to_string(),
            "f" => "false".to_string(),
            _ if is_json_number(text) => text.to_string(),
            _ => nested_or_string(text),
        },
    }
}

/// Exactly a JSON number literal (so `007`, ` 1` and `inf` are not).
fn is_json_number(text: &str) -> bool {
    text.trim() == text && matches!(serde_json::from_str::<Value>(text), Ok(Value::Number(_)))
}

/// Embedded JSON objects/arrays become nested pretty JSON; anything else is a string.
fn nested_or_string(text: &str) -> String {
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(v) = serde_json::from_str::<Value>(text)
        && let Ok(pretty) = serde_json::to_string_pretty(&v)
    {
        return pretty;
    }
    quote(text)
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    fn joined(headers: &[&str], row: &[&str], kinds: &[CellKind]) -> String {
        pretty_rows(&s(headers), &[s(row)], &[kinds.to_vec()]).join("\n")
    }

    #[test]
    fn sqlite_types_map_to_json() {
        use CellKind::*;
        let out = joined(
            &["n", "i", "r", "t", "b", "num_text"],
            &["NULL", "42", "1.5", "hi", "<blob 3 bytes>", "7"],
            &[Null, Integer, Real, Text, Blob, Text],
        );
        assert_eq!(
            out,
            "[\n  {\n    \"n\": null,\n    \"i\": 42,\n    \"r\": 1.5,\n    \"t\": \"hi\",\n    \
             \"b\": \"<blob 3 bytes>\",\n    \"num_text\": \"7\"\n  }\n]"
        );
    }

    #[test]
    fn untyped_values_are_inferred() {
        use CellKind::*;
        let out = joined(
            &["a", "b", "c", "d", "e", "f"],
            &["t", "f", "-3.25", "007", "text", "NULL"],
            &[Untyped, Untyped, Untyped, Untyped, Untyped, Null],
        );
        assert!(out.contains("\"a\": true,"));
        assert!(out.contains("\"b\": false,"));
        assert!(out.contains("\"c\": -3.25,"));
        assert!(out.contains("\"d\": \"007\","));
        assert!(out.contains("\"e\": \"text\","));
        assert!(out.contains("\"f\": null"));
    }

    #[test]
    fn embedded_json_is_nested_and_keeps_key_order() {
        let out = joined(&["j"], &[r#"{"z":1,"a":[1,2]}"#], &[CellKind::Text]);
        assert_eq!(
            out,
            "[\n  {\n    \"j\": {\n      \"z\": 1,\n      \"a\": [\n        1,\n        2\n      \
             ]\n    }\n  }\n]"
        );
        // Invalid JSON stays a string.
        let bad = joined(&["j"], &["{oops"], &[CellKind::Text]);
        assert!(bad.contains("\"j\": \"{oops\""));
    }

    #[test]
    fn duplicate_columns_and_escaping() {
        let out = joined(
            &["id", "id"],
            &["say \"hi\"\nbye", "2"],
            &[CellKind::Text, CellKind::Integer],
        );
        assert!(out.contains(r#""id": "say \"hi\"\nbye","#));
        assert!(out.contains("\"id\": 2\n"));
    }

    #[test]
    fn multiple_and_empty_results() {
        let two = pretty_rows(
            &s(&["a"]),
            &[s(&["1"]), s(&["2"])],
            &[vec![CellKind::Integer], vec![CellKind::Integer]],
        );
        assert_eq!(
            two.join("\n"),
            "[\n  {\n    \"a\": 1\n  },\n  {\n    \"a\": 2\n  }\n]"
        );
        assert_eq!(pretty_rows(&s(&["a"]), &[], &[]), vec!["[]"]);
    }
}
