//! Source span + multi-file source map for diagnostic rendering.
//!
//! A [`Span`] is a `(file_id, start, end)` byte range. The `file_id`
//! lets the (future) include loader attribute spans across multiple
//! physical files; in single-file usage `file_id` is always `0`.
//!
//! [`SourceMap`] owns the source text for each registered file and
//! resolves a `Span` into a [`ResolvedSpan`] (`path`, 1-based `line`,
//! 1-based `col`, the line text, and the byte length within the line).
//! The renderer in `check::render` consumes that to draw the rustc-
//! style snippet + caret.

use std::path::{Path, PathBuf};

/// Byte range into one of the source files registered in a
/// [`SourceMap`]. The half-open `[start, end)` form mirrors what pest
/// gives us via `pair.as_span().start()/end()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub file_id: u32,
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(file_id: u32, start: usize, end: usize) -> Self {
        Self {
            file_id,
            start,
            end,
        }
    }

    #[allow(dead_code)]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    #[allow(dead_code)]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Owns source text for every file that contributed AST nodes, so a
/// diagnostic carrying a [`Span`] can be rendered with the original
/// snippet.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

#[derive(Debug, Clone)]
struct SourceFile {
    path: PathBuf,
    text: String,
    /// Byte offset of the start of each line. `line_starts[0] == 0`.
    line_starts: Vec<usize>,
}

/// Span resolved against a [`SourceMap`]: physical file path, 1-based
/// line/column, the full line text, and the byte length within the line.
#[derive(Debug, Clone)]
pub struct ResolvedSpan {
    pub path: PathBuf,
    pub line: u32,
    pub col: u32,
    pub line_text: String,
    /// Byte length of the span clipped to the first line (the renderer
    /// only ever underlines a single-line caret today).
    pub span_len: u32,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a file's source text and return its `file_id`. Spans
    /// produced by parsing this text must use the returned id.
    pub fn add_file(&mut self, path: impl Into<PathBuf>, text: impl Into<String>) -> u32 {
        let text: String = text.into();
        let line_starts = compute_line_starts(&text);
        let id = self.files.len() as u32;
        self.files.push(SourceFile {
            path: path.into(),
            text,
            line_starts,
        });
        id
    }

    /// Convenience for callers that have just one file: `add_file`
    /// using `<input>` as the synthetic path. Used by tests and the
    /// in-memory `analyze_str` helper.
    #[allow(dead_code)]
    pub fn add_anonymous(&mut self, text: impl Into<String>) -> u32 {
        self.add_file(PathBuf::from("<input>"), text)
    }

    /// Number of registered files. Useful for sanity checks (e.g. test
    /// assertions that include expansion did register every physical file).
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Look up a registered file's source text by id.
    #[allow(dead_code)]
    pub fn source(&self, file_id: u32) -> Option<&str> {
        self.files.get(file_id as usize).map(|f| f.text.as_str())
    }

    /// Look up a registered file's path by id.
    #[allow(dead_code)]
    pub fn path(&self, file_id: u32) -> Option<&Path> {
        self.files.get(file_id as usize).map(|f| f.path.as_path())
    }

    /// Resolve a span to file path + line/col + the line's text. Returns
    /// `None` if the span's `file_id` isn't registered.
    pub fn resolve(&self, span: &Span) -> Option<ResolvedSpan> {
        let file = self.files.get(span.file_id as usize)?;
        let (line_idx, line_start) = locate_line(&file.line_starts, span.start);
        let line_end = file
            .line_starts
            .get(line_idx + 1)
            .copied()
            .unwrap_or(file.text.len());
        let line_text = file.text[line_start..line_end]
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();
        let col = span.start.saturating_sub(line_start) as u32 + 1;
        let clipped_end = span.end.min(line_end).min(line_start + line_text.len());
        let span_len = clipped_end.saturating_sub(span.start) as u32;
        Some(ResolvedSpan {
            path: file.path.clone(),
            line: line_idx as u32 + 1,
            col,
            line_text,
            span_len,
        })
    }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// Binary search for the line that `offset` falls into. Returns
/// `(line_index, line_start_offset)`.
fn locate_line(line_starts: &[usize], offset: usize) -> (usize, usize) {
    match line_starts.binary_search(&offset) {
        Ok(i) => (i, line_starts[i]),
        Err(i) => {
            // i is the insertion point — the offset falls inside line i-1.
            let line = i.saturating_sub(1);
            (line, line_starts[line])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_first_line() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("a.limpid", "hello world\nfoo bar\n");
        let r = sm.resolve(&Span::new(id, 6, 11)).unwrap();
        assert_eq!(r.line, 1);
        assert_eq!(r.col, 7);
        assert_eq!(r.line_text, "hello world");
        assert_eq!(r.span_len, 5);
    }

    #[test]
    fn resolve_subsequent_line() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("a.limpid", "hello\nfoo bar\n");
        let r = sm.resolve(&Span::new(id, 6, 9)).unwrap();
        assert_eq!(r.line, 2);
        assert_eq!(r.col, 1);
        assert_eq!(r.line_text, "foo bar");
        assert_eq!(r.span_len, 3);
    }

    #[test]
    fn resolve_unknown_file_returns_none() {
        let sm = SourceMap::new();
        assert!(sm.resolve(&Span::new(0, 0, 1)).is_none());
    }

    #[test]
    fn span_len_caps_at_line_end() {
        let mut sm = SourceMap::new();
        let id = sm.add_file("a.limpid", "abc\ndef\n");
        // Span over-extends past line 1 — len should cap at 3 bytes.
        let r = sm.resolve(&Span::new(id, 0, 100)).unwrap();
        assert_eq!(r.line_text, "abc");
        assert_eq!(r.span_len, 3);
    }
}
