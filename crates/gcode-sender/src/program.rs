//! Turning an uploaded G-code file into the lines that are streamed.

use serde::{Deserialize, Serialize};

/// One line that will be sent to the controller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgramLine {
    /// 1-based line number in the uploaded file, for error reports.
    pub number: usize,
    pub text: String,
}

/// Strips comments, blank lines and `%` program delimiters. Comments are
/// removed because they waste space in the controller's small RX buffer.
pub fn prepare(source: &str) -> Vec<ProgramLine> {
    source
        .lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let text = strip_comments(raw);
            let text = text.trim();
            if text.is_empty() || text == "%" {
                return None;
            }
            Some(ProgramLine {
                number: i + 1,
                text: text.to_string(),
            })
        })
        .collect()
}

fn strip_comments(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_paren = false;
    for c in line.chars() {
        match c {
            ';' if !in_paren => break,
            '(' => in_paren = true,
            ')' if in_paren => in_paren = false,
            _ if !in_paren => out.push(c),
            _ => {}
        }
    }
    out
}
