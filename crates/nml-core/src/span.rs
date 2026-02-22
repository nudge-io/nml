use serde::Serialize;

/// A byte offset range in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn empty(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }
}

/// A line and column location in the source text (1-indexed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl Location {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// Maps byte offsets to line/column locations.
pub struct SourceMap {
    line_starts: Vec<usize>,
}

impl SourceMap {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, ch) in source.char_indices() {
            if ch == '\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    pub fn location(&self, offset: usize) -> Location {
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let column = offset - self.line_starts[line];
        Location::new(line + 1, column + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_map() {
        let source = "hello\nworld\nfoo";
        let map = SourceMap::new(source);
        assert_eq!(map.location(0), Location::new(1, 1));
        assert_eq!(map.location(5), Location::new(1, 6));
        assert_eq!(map.location(6), Location::new(2, 1));
        assert_eq!(map.location(12), Location::new(3, 1));
    }

    #[test]
    fn test_span_merge() {
        let a = Span::new(5, 10);
        let b = Span::new(8, 15);
        let merged = a.merge(b);
        assert_eq!(merged, Span::new(5, 15));
    }
}
