//! Turns values into text for the terminal. Rendering happens only at the very end of a
//! pipeline. Up to there everything stays data.

use std::io::IsTerminal;

use meersh_value::{Record, Value};

pub struct RenderOptions {
    pub width: usize,
    pub color: bool,
}

impl RenderOptions {
    pub fn detect() -> Self {
        let width = terminal_size::terminal_size()
            .map(|(w, _)| w.0 as usize)
            .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
            .unwrap_or(120);
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        RenderOptions { width, color }
    }
}

const MIN_COLUMN: usize = 6;

pub fn render(value: &Value, opts: &RenderOptions) -> String {
    match value {
        Value::Nothing => String::new(),
        Value::String(s) => s.clone(),
        Value::List(items) if items.is_empty() => "[empty list]".into(),
        Value::List(items) if items.iter().all(|v| matches!(v, Value::Record(_))) => {
            let rows: Vec<&Record> = items.iter().filter_map(Value::as_record).collect();
            table(&rows, opts, 0)
        }
        Value::List(items) => {
            let rows: Vec<Record> = items.iter().map(|v| Record::new().with("value", v.clone())).collect();
            table(&rows.iter().collect::<Vec<_>>(), opts, 0)
        }
        Value::Record(r) => record(r, opts),
        scalar => scalar.to_display(),
    }
}

fn record(r: &Record, opts: &RenderOptions) -> String {
    let key_width = r.fields.keys().map(|k| k.chars().count()).max().unwrap_or(0);
    let value_width = opts.width.saturating_sub(key_width + 7).max(MIN_COLUMN);
    let mut out = String::new();
    if let Some(name) = r.type_name() {
        out.push_str(&paint(opts, "2", &format!("<{name}>")));
        out.push('\n');
    }
    for (k, v) in &r.fields {
        let key = paint(opts, "1;32", &format!("{k:>key_width$}"));
        out.push_str(&format!("{key}  {}\n", fit(&cell_text(v), value_width)));
    }
    out.pop();
    out
}

/// Renders one chunk of a stream. `offset` continues the row numbers of earlier chunks.
pub fn render_chunk(items: &[Value], offset: usize, opts: &RenderOptions) -> String {
    let rows: Vec<Record> = items
        .iter()
        .map(|v| match v {
            Value::Record(r) => r.clone(),
            other => Record::new().with("value", other.clone()),
        })
        .collect();
    table(&rows.iter().collect::<Vec<_>>(), opts, offset)
}

fn table(rows: &[&Record], opts: &RenderOptions, offset: usize) -> String {
    let mut columns: Vec<&str> = Vec::new();
    for row in rows {
        for key in row.fields.keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| columns.iter().map(|c| row.get(c).map(cell_text).unwrap_or_default()).collect())
        .collect();
    let index_width = (offset + rows.len()).saturating_sub(1).to_string().len().max(1);
    let mut widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| cells.iter().map(|r| r[i].chars().count()).chain([c.chars().count()]).max().unwrap_or(0))
        .collect();

    // Shrink the widest column until the table fits, then drop columns from the right.
    let total = |widths: &[usize]| index_width + 4 + widths.iter().map(|w| w + 3).sum::<usize>();
    let mut shown = columns.len();
    while total(&widths[..shown]) > opts.width {
        let (widest, width) = widths[..shown].iter().copied().enumerate().max_by_key(|(_, w)| *w).unwrap_or((0, 0));
        if width > MIN_COLUMN * 2 {
            widths[widest] -= 1;
        } else if shown > 1 {
            shown -= 1;
        } else {
            break;
        }
    }

    let line = |left: &str, mid: &str, right: &str| {
        let mut s = format!("{left}{}", "─".repeat(index_width + 2));
        for w in &widths[..shown] {
            s.push_str(&format!("{mid}{}", "─".repeat(w + 2)));
        }
        s + right
    };
    let mut out = vec![line("╭", "┬", "╮")];
    let mut header = format!("│ {} ", paint(opts, "1;32", &format!("{:>index_width$}", "#")));
    for (c, w) in columns.iter().zip(&widths).take(shown) {
        header.push_str(&format!("│ {} ", paint(opts, "1;32", &pad(&fit(c, *w), *w, false))));
    }
    out.push(header + "│");
    out.push(line("├", "┼", "┤"));
    for (i, (row, texts)) in rows.iter().zip(&cells).enumerate() {
        let mut s = format!("│ {} ", paint(opts, "2", &format!("{:>index_width$}", i + offset)));
        for (c, (text, w)) in columns.iter().zip(texts.iter().zip(&widths)).take(shown) {
            let value = row.get(c);
            let right =
                matches!(value, Some(Value::Int(_) | Value::Float(_) | Value::Filesize(_) | Value::Duration(_)));
            let text = pad(&fit(text, *w), *w, right);
            s.push_str(&format!("│ {} ", style(opts, row, c, &text)));
        }
        out.push(s + "│");
    }
    out.push(line("╰", "┴", "╯"));
    if shown < columns.len() {
        out.push(paint(
            opts,
            "2",
            &format!("{} more columns hidden: {}", columns.len() - shown, columns[shown..].join(", ")),
        ));
    }
    out.join("\n")
}

/// Type aware styling. A real implementation would let each record type register its own
/// renderer, for example `LS_COLORS` for `fs.entry`.
fn style(opts: &RenderOptions, row: &Record, column: &str, text: &str) -> String {
    match (row.type_name(), column, row.get("type").and_then(Value::as_str)) {
        (Some("fs.entry"), "name", Some("dir")) => paint(opts, "1;34", text),
        (Some("fs.entry"), "name", Some("symlink")) => paint(opts, "36", text),
        _ => text.to_string(),
    }
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.replace(['\n', '\r', '\t'], " "),
        Value::List(items)
            if !items.is_empty()
                && items.len() <= 5
                && items.iter().all(|i| !matches!(i, Value::List(_) | Value::Record(_))) =>
        {
            items.iter().map(Value::to_display).collect::<Vec<_>>().join(", ")
        }
        other => other.to_display(),
    }
}

fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
    s.push('…');
    s
}

fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text.chars().count()));
    if right { fill + text } else { text.to_string() + &fill }
}

fn paint(opts: &RenderOptions, code: &str, text: &str) -> String {
    if opts.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
}
