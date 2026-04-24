//! Rustc-style diagnostic rendering for `limpid --check`.
//!
//! When a [`Diagnostic`] carries a [`Span`], the renderer emits a
//! multi-line block: header, location arrow, gutter blank, source line,
//! caret row, and an optional `help: ...` line. When no span is
//! attached (file-level errors, summary lines, diagnostics from process
//! bodies that don't yet propagate spans), it falls back to the prior
//! one-line `level: message` format so existing CI consumers see a
//! stable shape.
//!
//! Colours are emitted only when stderr is a TTY. The `TERM=dumb`
//! environment also disables colour to match common terminal
//! conventions. Plain ASCII otherwise.

use std::io::{self, Write};

use crate::dsl::span::SourceMap;

use super::{DiagKind, Diagnostic, Level};

/// Categorical tag printed in square brackets after `error:` /
/// `warning:`. Derived from the structured [`DiagKind`] carried on every
/// diagnostic, so adding a new message variant never requires touching
/// this function.
fn category_of(d: &Diagnostic) -> &'static str {
    match d.kind {
        DiagKind::UnknownIdent | DiagKind::Dataflow => "dataflow",
        DiagKind::TypeMismatch => "type",
        DiagKind::Other => "check",
    }
}

/// Render a diagnostic to stderr. Always lock-free; one diagnostic per
/// call.
pub fn render_diagnostic(d: &Diagnostic, source_map: &SourceMap) {
    let mut stderr = io::stderr().lock();
    let _ = render_to(&mut stderr, d, source_map, color_enabled());
}

/// Lower-level renderer used by tests: writes to any [`Write`], with
/// explicit colour gate.
pub fn render_to(
    out: &mut dyn Write,
    d: &Diagnostic,
    source_map: &SourceMap,
    color: bool,
) -> io::Result<()> {
    let head = match d.level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Info => "note",
    };
    let head_colour = match d.level {
        Level::Error => RED,
        Level::Warning => YELLOW,
        Level::Info => CYAN,
    };

    // Spanless fallback — keeps file-level lines and process-body
    // diagnostics rendering as a single greppable line.
    let Some(span) = d.span else {
        write_coloured(out, color, head_colour, true, head)?;
        writeln!(out, ": {}", d.message)?;
        if let Some(help) = &d.help {
            write_coloured(out, color, CYAN, true, "  help")?;
            writeln!(out, ": {}", help)?;
        }
        return Ok(());
    };

    let Some(resolved) = source_map.resolve(&span) else {
        write_coloured(out, color, head_colour, true, head)?;
        writeln!(out, ": {}", d.message)?;
        if let Some(help) = &d.help {
            write_coloured(out, color, CYAN, true, "  help")?;
            writeln!(out, ": {}", help)?;
        }
        return Ok(());
    };

    // Header: `error[category]: <message>`
    write_coloured(out, color, head_colour, true, head)?;
    write_coloured(
        out,
        color,
        head_colour,
        true,
        &format!("[{}]", category_of(d)),
    )?;
    write_coloured(out, color, BOLD, true, &format!(": {}", d.message))?;
    writeln!(out)?;

    let line_num = format!("{}", resolved.line);
    let pad = " ".repeat(line_num.len());

    // `--> path:line:col`
    write_coloured(out, color, CYAN, true, &format!(" {}--> ", pad))?;
    writeln!(
        out,
        "{}:{}:{}",
        resolved.path.display(),
        resolved.line,
        resolved.col
    )?;

    // Gutter blank line.
    write_coloured(out, color, CYAN, true, &format!(" {} |", pad))?;
    writeln!(out)?;

    // Source line.
    write_coloured(out, color, CYAN, true, &format!(" {} | ", line_num))?;
    writeln!(out, "{}", resolved.line_text)?;

    // Caret row. Trim trailing whitespace inside the span so the caret
    // covers only the visible token (pest spans often over-extend).
    let caret_pad = " ".repeat(resolved.col.saturating_sub(1) as usize);
    let trim_len = effective_span_len(
        &resolved.line_text,
        resolved.col.saturating_sub(1) as usize,
        resolved.span_len as usize,
    );
    let carets = "^".repeat(trim_len.max(1));
    write_coloured(out, color, CYAN, true, &format!(" {} | ", pad))?;
    write!(out, "{}", caret_pad)?;
    write_coloured(out, color, head_colour, true, &carets)?;
    writeln!(out)?;

    if let Some(help) = &d.help {
        write_coloured(out, color, CYAN, true, &format!(" {} = ", pad))?;
        write_coloured(out, color, BOLD, true, "help")?;
        writeln!(out, ": {}", help)?;
    }

    Ok(())
}

/// Within `line` starting at byte offset `start`, return the byte
/// length of `span_len` truncated to the line boundary and not counting
/// trailing whitespace inside the span. Mirrors pest over-extension
/// behaviour (which often pulls the trailing newline / spaces in).
fn effective_span_len(line: &str, start: usize, span_len: usize) -> usize {
    let bytes = line.as_bytes();
    let end = (start + span_len).min(bytes.len());
    let mut len = end.saturating_sub(start);
    while len > 0 {
        let c = bytes[start + len - 1];
        if c == b' ' || c == b'\t' {
            len -= 1;
        } else {
            break;
        }
    }
    len
}

// ---------------------------------------------------------------------------
// ANSI colour helpers (no termcolor dep — keeps the runtime crate light).
// ---------------------------------------------------------------------------

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn write_coloured(
    out: &mut dyn Write,
    color: bool,
    code: &str,
    bold: bool,
    text: &str,
) -> io::Result<()> {
    if color {
        if bold && code != BOLD {
            write!(out, "{}{}{}{}", BOLD, code, text, RESET)?;
        } else {
            write!(out, "{}{}{}", code, text, RESET)?;
        }
    } else {
        write!(out, "{}", text)?;
    }
    Ok(())
}

fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Some(term) = std::env::var_os("TERM")
        && term == "dumb"
    {
        return false;
    }
    is_stderr_tty()
}

fn is_stderr_tty() -> bool {
    // SAFETY: isatty is a pure libc syscall accepting a file descriptor.
    unsafe { isatty(2) != 0 }
}

unsafe extern "C" {
    fn isatty(fd: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::span::Span;

    fn sm(text: &str) -> SourceMap {
        let mut m = SourceMap::new();
        m.add_file("t.limpid", text);
        m
    }

    #[test]
    fn category_dataflow_for_workspace_ref() {
        let d = Diagnostic::error_kind(
            DiagKind::UnknownIdent,
            "[pipeline p] output `o` references `workspace.x` which is not produced by any upstream module",
        );
        assert_eq!(category_of(&d), "dataflow");
    }

    #[test]
    fn category_type_for_function_arg_warning() {
        let d = Diagnostic::warning_kind(
            DiagKind::TypeMismatch,
            "[pipeline p] function `lower` argument 1 expects String, got Int",
        );
        assert_eq!(category_of(&d), "type");
    }

    #[test]
    fn category_dataflow_for_object_overwrite() {
        let d = Diagnostic::warning_kind(
            DiagKind::Dataflow,
            "[pipeline p] assignment to `workspace.geo` overwrites an Object with String; nested references will become dead",
        );
        assert_eq!(category_of(&d), "dataflow");
    }

    #[test]
    fn renders_caret_under_span() {
        let src = "abc workspace.x def\n";
        let map = sm(src);
        // span over `workspace.x` (offsets 4..15)
        let d = Diagnostic::error_kind(
            DiagKind::UnknownIdent,
            "[pipeline p] output `o` references `workspace.x` which is not produced by any upstream module",
        )
        .with_span(Some(Span::new(0, 4, 15)));
        let mut buf = Vec::new();
        render_to(&mut buf, &d, &map, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("error[dataflow]"), "got: {s}");
        assert!(s.contains("--> t.limpid:1:5"), "got: {s}");
        assert!(s.contains("abc workspace.x def"), "got: {s}");
        assert!(s.contains("    ^^^^^^^^^^^"), "got: {s}");
    }

    #[test]
    fn renders_help_line() {
        let src = "abc workspace.x def\n";
        let map = sm(src);
        let d = Diagnostic::error("missing")
            .with_span(Some(Span::new(0, 4, 15)))
            .with_help("did you mean `workspace.y`?");
        let mut buf = Vec::new();
        render_to(&mut buf, &d, &map, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("help: did you mean `workspace.y`?"), "got: {s}");
    }

    #[test]
    fn spanless_falls_back_to_one_line() {
        let map = SourceMap::new();
        let d = Diagnostic::error("file missing");
        let mut buf = Vec::new();
        render_to(&mut buf, &d, &map, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s, "error: file missing\n");
    }

    #[test]
    fn effective_span_len_trims_trailing_space() {
        assert_eq!(effective_span_len("foo   ", 0, 6), 3);
        assert_eq!(effective_span_len("bar", 0, 100), 3);
        assert_eq!(effective_span_len("", 0, 5), 0);
    }
}
