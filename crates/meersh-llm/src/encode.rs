use meersh_value::Value;
use serde_json::Value as Json;

/// Serializes pipeline input for a prompt.
///
/// A table becomes one header line and one JSON array per row. That costs far fewer tokens
/// than repeating every key in every row, and models read it well. Filesizes, durations and
/// dates are shown the way a human sees them (`1.2 MiB`, `3d 4h`), which is also what the
/// model needs to reason about them. When the budget runs out whole rows are dropped and
/// the model is told so.
pub fn encode(value: &Value, max_chars: usize) -> String {
    match value {
        Value::String(s) => truncate(s, max_chars),
        Value::List(items) if !items.is_empty() && items.iter().all(|v| v.as_record().is_some()) => {
            table(items, max_chars)
        }
        other => truncate(&cell(other).to_string(), max_chars),
    }
}

fn table(rows: &[Value], max_chars: usize) -> String {
    let mut columns: Vec<&str> = Vec::new();
    for row in rows.iter().filter_map(Value::as_record) {
        for key in row.fields.keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    let mut out = format!("columns: {}\n", serde_json::to_string(&columns).unwrap_or_default());
    for (shown, row) in rows.iter().filter_map(Value::as_record).enumerate() {
        let cells: Vec<Json> = columns.iter().map(|c| row.get(c).map_or(Json::Null, cell)).collect();
        let line = Json::Array(cells).to_string();
        if out.len() + line.len() > max_chars {
            out.push_str(&format!("[{} more rows omitted]\n", rows.len() - shown));
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn cell(value: &Value) -> Json {
    match value {
        Value::Filesize(_) | Value::Duration(_) | Value::Date(_) => Json::String(value.to_display()),
        Value::List(items) => Json::Array(items.iter().map(cell).collect()),
        Value::Record(r) => Json::Object(r.fields.iter().map(|(k, v)| (k.clone(), cell(v))).collect()),
        other => other.to_json(),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}\n[{} more characters omitted]", &text[..cut], text.len() - cut),
    }
}
