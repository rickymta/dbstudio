//! Statement splitting.

use dbstudio_core::{ScriptDialect, TextRange};

/// Ranges of the statements of `text` (terminators and `GO` lines excluded).
///
/// Scaffold: one range for the whole non-empty text.
pub fn split(d: ScriptDialect, text: &str) -> Vec<TextRange> {
    let _ = d;
    if text.trim().is_empty() {
        return Vec::new();
    }
    vec![TextRange {
        start: 0,
        end: crate::char_len(text),
    }]
}

/// Incremental splitter for big files (SQL import): feed chunks, receive complete statements.
#[derive(Debug, Clone)]
pub struct StreamSplitter {
    dialect: ScriptDialect,
    buffer: String,
}

impl StreamSplitter {
    /// New splitter for `d`.
    pub fn new(d: ScriptDialect) -> Self {
        StreamSplitter {
            dialect: d,
            buffer: String::new(),
        }
    }

    /// Dialect in use.
    pub fn dialect(&self) -> ScriptDialect {
        self.dialect
    }

    /// Feed a chunk; returns the statements completed by it. Scaffold: buffers everything.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        Vec::new()
    }

    /// End of input; returns the remaining statement, if any. Scaffold: the whole buffered text.
    pub fn finish(self) -> Vec<String> {
        if self.buffer.trim().is_empty() {
            Vec::new()
        } else {
            vec![self.buffer]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_contract_whole_text_is_one_range() {
        assert!(split(ScriptDialect::Postgres, " \n").is_empty());
        let r = split(ScriptDialect::Postgres, "select 1; select 2");
        assert_eq!(r, vec![TextRange { start: 0, end: 18 }]);
    }

    #[test]
    fn stub_contract_stream_splitter_returns_all_on_finish() {
        let mut s = StreamSplitter::new(ScriptDialect::MsSql);
        assert!(s.push("select 1;").is_empty());
        assert!(s.push(" select 2").is_empty());
        assert_eq!(s.finish(), vec!["select 1; select 2".to_string()]);
        assert!(
            StreamSplitter::new(ScriptDialect::Oracle)
                .finish()
                .is_empty()
        );
    }
}
