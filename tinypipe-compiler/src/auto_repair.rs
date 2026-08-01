//! Auto-Repair — format compiler errors into human-readable LLM feedback.
//!
//! # Purpose
//!
//! When the compiler rejects code (sanitizer, transform, validator, or codegen),
//! this module produces a structured, human-readable report that TinyOS can
//! pass to an LLM so the LLM can fix its own code (auto-repair loop).
//!
//! # Format
//!
//! Each report contains:
//! - Error location (line, column)
//! - Human-readable description
//! - The offending source line
//! - Available context (e.g., valid tool names, allowed imports)
//!
//!
//! # Usage
//!
//! ```rust
//! use tinypipe_compiler::auto_repair;
//!
//! fn repair(code: &str) -> Option<String> {
//!     // transform() returns Result<ExecutionPlan, Vec<TransformError>>
//!     match tinypipe_compiler::transform::transform(code) {
//!         Ok(plan) => {
//!             match tinypipe_compiler::validator::validate(&plan) {
//!                 Ok(()) => None, // success
//!                 Err(errors) => {
//!                     let report = auto_repair::from_validation_errors(&errors, code);
//!                     Some(report.to_string())
//!                 }
//!             }
//!         }
//!         Err(transform_errors) => {
//!             // Take the first transform error
//!             if let Some(err) = transform_errors.first() {
//!                 Some(auto_repair::from_transform_error(err, code).to_string())
//!             } else {
//!                 Some("Unknown compiler error".to_string())
//!             }
//!         }
//!     }
//! }
//! ```

use std::fmt;

/// A structured, LLM-friendly compiler error report.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// Error type label.
    pub error_type: String,
    /// Line number (1-indexed).
    pub line: usize,
    /// Column number (1-indexed).
    pub column: usize,
    /// Human-readable error message.
    pub message: String,
    /// The offending source line, if available.
    pub source_line: Option<String>,
    /// Additional context for the LLM (e.g., valid tool names).
    pub context: Vec<String>,
    /// Number of attempts so far (for the auto-repair loop).
    pub attempt: u32,
    /// Maximum attempts before giving up.
    pub max_attempts: u32,
}

impl RepairReport {
    /// Create a new repair report.
    pub fn new(error_type: &str, line: usize, column: usize, message: &str) -> Self {
        Self {
            error_type: error_type.to_string(),
            line,
            column,
            message: message.to_string(),
            source_line: None,
            context: Vec::new(),
            attempt: 0,
            max_attempts: 3,
        }
    }

    /// Attach the source line for context.
    pub fn with_source_line(mut self, code: &str) -> Self {
        self.source_line = extract_line(code, self.line);
        self
    }

    /// Attach context hints for the LLM.
    pub fn with_context(mut self, hints: Vec<String>) -> Self {
        self.context = hints;
        self
    }

    /// Set the attempt counter.
    pub fn with_attempt(mut self, attempt: u32) -> Self {
        self.attempt = attempt;
        self
    }

    /// Set the max attempts.
    pub fn with_max_attempts(mut self, max: u32) -> Self {
        self.max_attempts = max;
        self
    }
}

impl fmt::Display for RepairReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "━━━ Compiler Feedback ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        )?;

        if self.attempt > 0 {
            writeln!(f, "  Deneme {}/{}", self.attempt, self.max_attempts)?;
        }

        writeln!(
            f,
            "  {} ({}:{}): {}",
            self.error_type, self.line, self.column, self.message
        )?;

        if let Some(ref line) = self.source_line {
            writeln!(f)?;
            writeln!(f, "  Satır {}: {}", self.line, line)?;
        }

        if !self.context.is_empty() {
            writeln!(f)?;
            for hint in &self.context {
                writeln!(f, "  → {}", hint)?;
            }
        }

        if self.attempt < self.max_attempts {
            writeln!(f)?;
            writeln!(f, "  Lütfen kodu düzeltip tekrar gönderin.")?;
        } else {
            writeln!(f)?;
            writeln!(
                f,
                "  Maksimum deneme sayısına ulaşıldı ({}) — sonraki aşamaya geçiliyor.",
                self.max_attempts
            )?;
        }

        writeln!(
            f,
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        )?;
        Ok(())
    }
}

// ─── Factory Functions ──────────────────────────────────────────────

/// Build a report from a sanitizer error.
pub fn from_sanitize_error(line: usize, column: usize, message: &str, code: &str) -> RepairReport {
    RepairReport::new("SANITIZER HATASI", line, column, message).with_source_line(code)
}

/// Build a report from a vec of validation errors.
pub fn from_validation_errors(
    errors: &[crate::validator::ValidationError],
    code: &str,
) -> RepairReport {
    let mut report = RepairReport::new(
        "VALIDASYON HATASI",
        errors
            .first()
            .map(|e| usize_from_node_id(&e.node_id))
            .unwrap_or(1),
        1,
        &format!("{} doğrulama hatası bulundu", errors.len()),
    )
    .with_source_line(code);

    let error_details: Vec<String> = errors
        .iter()
        .map(|e| format!("- {}: {}", e.node_id, e.message))
        .collect();
    report.context = error_details;

    report
}

// Note: uses usize for node_id so it can accept numeric indices at the call site.
fn usize_from_node_id(node_id: &str) -> usize {
    // Try to extract a line number from node_id (e.g., "node_5" → 5)
    if let Some(num) = node_id
        .rsplit('_')
        .next()
        .and_then(|s| s.parse::<usize>().ok())
    {
        return num;
    }
    // Try direct parse
    node_id.parse().unwrap_or(1)
}

/// Build a report from a transform error (which wraps sanitizer or parser errors).
pub fn from_transform_error(err: &crate::transform::TransformError, code: &str) -> RepairReport {
    RepairReport::new("TRANSFORM HATASI", err.line, err.column, &err.message).with_source_line(code)
}

/// Build a report from a generic parse error string.
pub fn from_parse_error(error_str: &str, code: &str) -> RepairReport {
    // Try to extract line/column from common rustpython_parser error formats
    let (line, col) = extract_line_col_from_error(error_str);
    RepairReport::new("PARSE HATASI", line, col, error_str).with_source_line(code)
}

/// Build a report from a codegen error.
pub fn from_codegen_error(error_str: &str, code: &str) -> RepairReport {
    RepairReport::new("CODEGEN HATASI", 1, 1, error_str).with_source_line(code)
}

/// Build a combined report from a full pipeline failure.
///
/// Calls transform(), then if that fails returns a report from the error.
/// If transform succeeds, runs validate() and returns a report from validation errors.
///
/// Returns `None` if the pipeline succeeds (no errors).
pub fn check_code(code: &str, attempt: u32, max_attempts: u32) -> Option<RepairReport> {
    match crate::transform::transform(code) {
        Ok(plan) => {
            match crate::validator::validate(&plan) {
                Ok(()) => None, // Success
                Err(errors) => {
                    let mut report = from_validation_errors(&errors, code);
                    report.attempt = attempt;
                    report.max_attempts = max_attempts;
                    Some(report)
                }
            }
        }
        Err(transform_errors) => {
            if let Some(err) = transform_errors.first() {
                let mut report = from_transform_error(err, code);
                report.attempt = attempt;
                report.max_attempts = max_attempts;
                Some(report)
            } else {
                Some(RepairReport {
                    error_type: "BİLİNMEYEN HATA".into(),
                    line: 1,
                    column: 1,
                    message: "Compiler hatası (detay yok)".into(),
                    source_line: None,
                    context: vec![],
                    attempt,
                    max_attempts,
                })
            }
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Extract a specific line from source code (1-indexed).
fn extract_line(code: &str, line: usize) -> Option<String> {
    code.lines()
        .nth(line.saturating_sub(1))
        .map(|s| s.to_string())
}

/// Extract line and column numbers from a rustpython_parser error string.
fn extract_line_col_from_error(s: &str) -> (usize, usize) {
    // Common patterns:
    //   "Error at line 5, column 12: ..."
    //   "SyntaxError: ... (line 5, column 12)"
    //   "5:12: ..."
    // Try "line X, column Y" pattern
    if let Some(pos) = s.find("line ") {
        let after_line = &s[pos + 5..];
        if let Some(end) = after_line.find(&[',', ')'][..]) {
            if let Ok(line) = after_line[..end].trim().parse::<usize>() {
                // Look for "column Y" after "line X"
                if let Some(col_pos) = after_line.find("column ") {
                    let after_col = &after_line[col_pos + 7..];
                    let col_end = after_col
                        .find(&[',', ')', ':', ' '][..])
                        .unwrap_or(after_col.len());
                    if let Ok(col) = after_col[..col_end].trim().parse::<usize>() {
                        return (line, col);
                    }
                }
                return (line, 1);
            }
        }
    }
    // Try "X:Y:" pattern at start
    if let Some(colon) = s.find(':') {
        if let Ok(line) = s[..colon].trim().parse::<usize>() {
            let rest = &s[colon + 1..];
            if let Some(colon2) = rest.find(':') {
                if let Ok(col) = rest[..colon2].trim().parse::<usize>() {
                    return (line, col);
                }
            }
            return (line, 1);
        }
    }
    (1, 1) // default
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_formatting() {
        let report = RepairReport::new("TEST", 5, 12, "Tool 'foo' not found")
            .with_source_line("    x = call(\"foo\", arg=1)\n")
            .with_context(vec!["Available tools: ['bar', 'baz']".into()]);
        let out = report.to_string();
        assert!(out.contains("TEST"));
        assert!(out.contains("5:12"));
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn test_check_code_ok() {
        // Simple valid code: has input parameter, uses it, returns
        let result = check_code("def graph(x: int):\n    return x", 1, 3);
        assert!(
            result.is_none(),
            "valid code should produce no report, got: {:?}",
            result
        );
    }

    #[test]
    fn test_check_code_sanitize_error() {
        // Code that will fail sanitizer: using disallowed async
        let result = check_code(
            "def graph():\n    import os\n    x = os.system('rm -rf /')",
            1,
            3,
        );
        assert!(result.is_some(), "malicious code should produce a report");
        if let Some(report) = result {
            // Sanitizer errors include the word "yasak" or "blocked" or similar
            assert!(!report.message.is_empty(), "should have a message");
        }
    }

    #[test]
    fn test_check_code_validation_error() {
        // Code that passes sanitizer/transform but fails validator (no return)
        let result = check_code("def graph():\n    pass", 1, 3);
        assert!(result.is_some(), "invalid code should produce a report");
    }

    #[test]
    fn test_check_code_attempt_tracking() {
        let result = check_code("def graph():\n    import os", 2, 3);
        assert!(result.is_some());
        if let Some(ref report) = result {
            assert_eq!(report.attempt, 2);
            assert_eq!(report.max_attempts, 3);
        }
    }

    #[test]
    fn test_extract_line() {
        let code = "line1\nline2\nline3\n";
        assert_eq!(extract_line(code, 1), Some("line1".into()));
        assert_eq!(extract_line(code, 2), Some("line2".into()));
        assert_eq!(extract_line(code, 3), Some("line3".into()));
        assert_eq!(extract_line(code, 99), None);
    }

    #[test]
    fn test_extract_line_col_default() {
        let (l, c) = extract_line_col_from_error("unknown error");
        assert_eq!(l, 1);
        assert_eq!(c, 1);
    }

    #[test]
    fn test_report_with_attempt_info() {
        let report = RepairReport::new("TEST", 1, 1, "error")
            .with_attempt(1)
            .with_max_attempts(3);
        let out = report.to_string();
        assert!(out.contains("1/3"), "attempt info should show: {out}");

        let report2 = RepairReport::new("TEST", 1, 1, "error")
            .with_attempt(3)
            .with_max_attempts(3);
        let out2 = report2.to_string();
        assert!(
            out2.contains("3/3") || out2.contains("max"),
            "should show final attempt: {out2}"
        );
    }
}
