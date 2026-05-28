use crate::common::types::Row;

pub fn render_rows(headers: &[String], rows: &[Row]) -> String {
    let column_count = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));

    if column_count == 0 {
        return "(0 rows)".to_string();
    }

    let normalized_headers = normalize_headers(headers, column_count);

    let mut lines = Vec::with_capacity(rows.len() + 2);
    let header_line = format_line(normalized_headers.iter().map(String::as_str).collect());
    lines.push(header_line.clone());
    lines.push("-".repeat(header_line.len()));

    for row in rows {
        let cells = (0..column_count)
            .map(|index| row.get(index).map(ToString::to_string).unwrap_or_default())
            .collect::<Vec<_>>();
        lines.push(format_line(cells.iter().map(String::as_str).collect()));
    }

    lines.join("\n")
}

fn normalize_headers(headers: &[String], column_count: usize) -> Vec<String> {
    let mut normalized = headers.to_vec();
    while normalized.len() < column_count {
        normalized.push(format!("col{}", normalized.len() + 1));
    }
    normalized
}

fn format_line(cells: Vec<&str>) -> String {
    cells.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::render_rows;

    #[test]
    fn render_rows_includes_headers_and_values() {
        let output = render_rows(
            &["id".to_string(), "name".to_string()],
            &[
                vec![1_i64.into(), "alice".into()],
                vec![2_i64.into(), "bob".into()],
            ],
        );

        assert!(output.contains("id | name"));
        assert!(output.contains("1 | alice"));
        assert!(output.contains("2 | bob"));
    }
}
