//! `compare_symbols`: a line diff of two symbol bodies, each addressed by
//! symbol + file (+ repo).

use anyhow::{anyhow, Context, Result};
use similar::{Algorithm, ChangeTag, DiffOp, DiffTag, TextDiff};

use super::{collapse_whitespace, symbol_location, validate_path, CodeIntelEngine};
use crate::symbols::Symbol;

/// Arguments for `compare_symbols`. Empty side fields fall back to the shared
/// ones: `*_repo` to `repo`, `right_symbol` to `left_symbol`.
pub struct CompareSymbolsQuery<'a> {
    pub repo: &'a str,
    pub left_symbol: &'a str,
    pub left_file: Option<&'a str>,
    pub left_repo: Option<&'a str>,
    pub right_symbol: Option<&'a str>,
    pub right_file: Option<&'a str>,
    pub right_repo: Option<&'a str>,
    pub ignore_whitespace: bool,
    pub ignore_case: bool,
    pub context_lines: usize,
    /// Cap on printed diff lines; 0 or `usize::MAX` = no cap.
    pub max_lines: usize,
}

pub(crate) struct DiffOptions {
    pub ignore_whitespace: bool,
    pub ignore_case: bool,
    pub context_lines: usize,
    pub max_lines: usize,
}

/// One side of the comparison: the symbol's lines and where they sit in the file.
pub(crate) struct Side<'a> {
    pub label: &'a str,
    pub location: &'a str,
    /// File line number of `lines[0]`.
    pub start_line: usize,
    pub lines: &'a [String],
}

struct ResolvedSide {
    repo: String,
    symbol: Symbol,
    lines: Vec<String>,
    start_line: usize,
    /// The file on disk ends before the indexed symbol does.
    stale: bool,
}

impl ResolvedSide {
    fn label(&self) -> &str {
        self.symbol
            .qualified_name
            .as_deref()
            .unwrap_or(&self.symbol.name)
    }

    fn location(&self) -> String {
        format!(
            "{}:{} lines {}-{} ({} lines)",
            self.repo,
            self.symbol.file_path,
            self.start_line,
            self.start_line + self.lines.len().saturating_sub(1),
            self.lines.len()
        )
    }
}

impl CodeIntelEngine {
    pub async fn compare_symbols(&self, q: CompareSymbolsQuery<'_>) -> Result<String> {
        if q.left_symbol.is_empty() {
            anyhow::bail!("'left_symbol' is required");
        }
        let right_symbol = q
            .right_symbol
            .filter(|s| !s.is_empty())
            .unwrap_or(q.left_symbol);
        tracing::info!(
            left = q.left_symbol,
            right = right_symbol,
            "compare_symbols"
        );

        let left = self.resolve_side(
            "left",
            q.left_repo.filter(|r| !r.is_empty()).unwrap_or(q.repo),
            q.left_symbol,
            q.left_file,
        )?;
        let right = self.resolve_side(
            "right",
            q.right_repo.filter(|r| !r.is_empty()).unwrap_or(q.repo),
            right_symbol,
            q.right_file,
        )?;

        if left.repo == right.repo
            && left.symbol.file_path == right.symbol.file_path
            && left.symbol.start_line == right.symbol.start_line
        {
            anyhow::bail!(
                "Both sides resolve to the same symbol {} ({}) — pass 'right_file' or \
                 'right_symbol' to pick the other one",
                left.label(),
                symbol_location(&left.symbol)
            );
        }

        let (left_location, right_location) = (left.location(), right.location());
        let opts = DiffOptions {
            ignore_whitespace: q.ignore_whitespace,
            ignore_case: q.ignore_case,
            context_lines: q.context_lines,
            max_lines: q.max_lines,
        };
        let mut out = render_symbol_diff(
            &Side {
                label: left.label(),
                location: &left_location,
                start_line: left.start_line,
                lines: &left.lines,
            },
            &Side {
                label: right.label(),
                location: &right_location,
                start_line: right.start_line,
                lines: &right.lines,
            },
            &opts,
        );
        for side in [&left, &right].into_iter().filter(|s| s.stale) {
            out.push_str(&format!(
                "\n**Note**: `{}` ends at line {} on disk but the index has {} up to line {} — \
                 the index is stale and the body above is cut short; run reindex.\n",
                side.symbol.file_path,
                side.start_line + side.lines.len().saturating_sub(1),
                side.label(),
                side.symbol.end_line
            ));
        }
        Ok(out)
    }

    fn resolve_side(
        &self,
        side: &str,
        repo: &str,
        symbol_name: &str,
        file: Option<&str>,
    ) -> Result<ResolvedSide> {
        if repo.is_empty() {
            return Err(self.repo_not_found_error(""));
        }
        let repo = self.resolve_repo_name(repo)?;
        let symbol = self.resolve_symbol_strict(side, &repo, symbol_name, file)?;
        let (lines, start_line, stale) = self.symbol_body(&repo, &symbol)?;
        Ok(ResolvedSide {
            repo,
            symbol,
            lines,
            start_line,
            stale,
        })
    }

    /// Exactly one symbol or an error: unlike `get_symbol_definition`, which
    /// prints the first namesake, diffing the wrong one is worse than refusing.
    fn resolve_symbol_strict(
        &self,
        side: &str,
        repo: &str,
        symbol_name: &str,
        file: Option<&str>,
    ) -> Result<Symbol> {
        let symbols = self
            .symbols
            .get(repo)
            .ok_or_else(|| self.repo_not_found_error(repo))?;
        let lookup = self.resolve_symbol_lookup(repo, symbol_name, file, None, &symbols)?;
        tracing::debug!(
            side,
            repo,
            symbol = symbol_name,
            file = file.unwrap_or(""),
            candidates = lookup.symbols.len(),
            "compare_symbols: resolve"
        );
        if let [only] = lookup.symbols.as_slice() {
            return Ok((*only).clone());
        }
        let candidates = lookup
            .symbols
            .iter()
            .map(|s| {
                format!(
                    "{} ({})",
                    s.qualified_name.as_deref().unwrap_or(&s.name),
                    symbol_location(s)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(anyhow!(
            "{side}: '{symbol_name}' matches {} symbols in repository '{repo}': {candidates}. \
             Pass '{side}_file' with a path, a suffix of one or a path fragment to pick one",
            lookup.symbols.len()
        ))
    }

    /// The symbol's lines as they are on disk now, the file line number of the
    /// first one, and whether the file ends before the indexed symbol does.
    fn symbol_body(&self, repo: &str, symbol: &Symbol) -> Result<(Vec<String>, usize, bool)> {
        let repo_path = self.get_repo_path(repo)?;
        let path = validate_path(&repo_path, &symbol.file_path)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", symbol.file_path))?;
        let lines: Vec<&str> = content.lines().collect();
        let range = crate::text::clamp_line_range(
            symbol.start_line.saturating_sub(1),
            symbol.end_line,
            lines.len(),
        );
        let stale = range.end < symbol.end_line;
        if range.is_empty() {
            anyhow::bail!(
                "{}:{} has {} lines but the index puts '{}' at lines {}-{} — the index is \
                 stale; run reindex",
                repo,
                symbol.file_path,
                lines.len(),
                symbol.name,
                symbol.start_line,
                symbol.end_line
            );
        }
        if stale {
            tracing::warn!(
                repo,
                file = %symbol.file_path,
                end_line = symbol.end_line,
                file_lines = lines.len(),
                "compare_symbols: file is shorter than the indexed symbol; index is stale, reindex"
            );
        }
        let start_line = range.start + 1;
        let body = lines[range].iter().map(|l| (*l).to_string()).collect();
        Ok((body, start_line, stale))
    }
}

fn normalize_line(line: &str, opts: &DiffOptions) -> String {
    let line = if opts.ignore_whitespace {
        collapse_whitespace(line.trim())
    } else {
        line.to_string()
    };
    if opts.ignore_case {
        line.to_lowercase()
    } else {
        line
    }
}

fn op_line_count(op: &DiffOp) -> usize {
    let (tag, old, new) = op.as_tag_tuple();
    match tag {
        DiffTag::Equal | DiffTag::Delete => old.len(),
        DiffTag::Insert => new.len(),
        DiffTag::Replace => old.len() + new.len(),
    }
}

fn file_range(start_line: usize, range: std::ops::Range<usize>) -> String {
    if range.is_empty() {
        return format!("{} (none)", start_line + range.start);
    }
    format!(
        "{}-{}",
        start_line + range.start,
        start_line + range.end - 1
    )
}

pub(crate) fn render_symbol_diff(left: &Side, right: &Side, opts: &DiffOptions) -> String {
    let left_keys: Vec<String> = left.lines.iter().map(|l| normalize_line(l, opts)).collect();
    let right_keys: Vec<String> = right
        .lines
        .iter()
        .map(|l| normalize_line(l, opts))
        .collect();
    let left_refs: Vec<&str> = left_keys.iter().map(String::as_str).collect();
    let right_refs: Vec<&str> = right_keys.iter().map(String::as_str).collect();
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .diff_slices(&left_refs, &right_refs);

    let (mut inserted, mut deleted) = (0usize, 0usize);
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => {}
            DiffTag::Delete => deleted += old.len(),
            DiffTag::Insert => inserted += new.len(),
            DiffTag::Replace => {
                deleted += old.len();
                inserted += new.len();
            }
        }
    }

    let mut out = format!(
        "# compare_symbols: {} ↔ {}\n\n**Left**: `{}`\n**Right**: `{}`\n",
        left.label, right.label, left.location, right.location
    );
    let flags = format!(
        "ignore_whitespace={} ignore_case={}",
        opts.ignore_whitespace, opts.ignore_case
    );

    if inserted == 0 && deleted == 0 {
        let note = if left.lines == right.lines {
            String::new()
        } else {
            format!(" modulo whitespace/case ({flags})")
        };
        out.push_str(&format!(
            "**Result**: identical{note} ({} lines)\n",
            left.lines.len()
        ));
        tracing::debug!(
            left_lines = left.lines.len(),
            right_lines = right.lines.len(),
            "compare_symbols: identical"
        );
        return out;
    }

    let groups = diff.grouped_ops(opts.context_lines);
    out.push_str(&format!(
        "**Similarity**: {:.1}% · +{inserted} −{deleted} · {} hunk(s) · {flags}\n",
        diff.ratio() * 100.0,
        groups.len()
    ));

    let max_line = (left.start_line + left.lines.len()).max(right.start_line + right.lines.len());
    let width = max_line.to_string().len();
    let cap = if opts.max_lines == 0 {
        usize::MAX
    } else {
        opts.max_lines
    };
    let mut printed = 0usize;
    let mut truncated_at: Option<usize> = None;

    for (gi, group) in groups.iter().enumerate() {
        if printed >= cap {
            truncated_at = Some(gi);
            break;
        }
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        out.push_str(&format!(
            "\n## Hunk {}: left {} → right {}\n```diff\n",
            gi + 1,
            file_range(
                left.start_line,
                first.old_range().start..last.old_range().end
            ),
            file_range(
                right.start_line,
                first.new_range().start..last.new_range().end
            ),
        ));
        let mut hunk_printed = 0usize;
        'ops: for op in group {
            for change in diff.iter_changes(op) {
                if printed >= cap {
                    break 'ops;
                }
                let (marker, old_no, new_no, text) = match change.tag() {
                    ChangeTag::Equal => {
                        let (o, n) = (change.old_index(), change.new_index());
                        (' ', o, n, o.map(|i| left.lines[i].as_str()))
                    }
                    ChangeTag::Delete => {
                        let o = change.old_index();
                        ('-', o, None, o.map(|i| left.lines[i].as_str()))
                    }
                    ChangeTag::Insert => {
                        let n = change.new_index();
                        ('+', None, n, n.map(|i| right.lines[i].as_str()))
                    }
                };
                let num = |idx: Option<usize>, base: usize| {
                    idx.map(|i| (base + i).to_string()).unwrap_or_default()
                };
                out.push_str(&format!(
                    "{marker} {:>width$} {:>width$} │ {}\n",
                    num(old_no, left.start_line),
                    num(new_no, right.start_line),
                    text.unwrap_or(""),
                ));
                printed += 1;
                hunk_printed += 1;
            }
        }
        out.push_str("```\n");
        let hunk_total: usize = group.iter().map(op_line_count).sum();
        if hunk_printed < hunk_total {
            truncated_at = Some(gi);
            break;
        }
    }

    if let Some(gi) = truncated_at {
        let hidden_lines: usize = groups[gi..]
            .iter()
            .flat_map(|g| g.iter().map(op_line_count))
            .sum::<usize>()
            - partial_printed(&groups[gi], printed, &groups[..gi]);
        out.push_str(&format!(
            "\n*Diff truncated: {hidden_lines} more line(s) in {} hunk(s). Raise 'max_lines' \
             (0 = no limit) or narrow with get_symbol_definition(offset, max_lines).*\n",
            groups.len() - gi
        ));
    }

    tracing::debug!(
        left_lines = left.lines.len(),
        right_lines = right.lines.len(),
        hunks = groups.len(),
        inserted,
        deleted,
        ratio = diff.ratio(),
        truncated = truncated_at.is_some(),
        "compare_symbols: diff"
    );
    out
}

/// Lines of the cut-off hunk that were printed before the cap hit.
fn partial_printed(cut: &[DiffOp], printed: usize, before: &[Vec<DiffOp>]) -> usize {
    let full: usize = before
        .iter()
        .flat_map(|g| g.iter().map(op_line_count))
        .sum();
    let cut_total: usize = cut.iter().map(op_line_count).sum();
    printed.saturating_sub(full).min(cut_total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn opts() -> DiffOptions {
        DiffOptions {
            ignore_whitespace: true,
            ignore_case: false,
            context_lines: 3,
            max_lines: 400,
        }
    }

    fn render(left: &[String], right: &[String], opts: &DiffOptions) -> String {
        render_symbol_diff(
            &Side {
                label: "L",
                location: "r:a.bsl",
                start_line: 100,
                lines: left,
            },
            &Side {
                label: "R",
                location: "r:b.bsl",
                start_line: 200,
                lines: right,
            },
            opts,
        )
    }

    #[test]
    fn identical_bodies_say_identical() {
        let body = lines("Процедура А()\n\tВозврат;\nКонецПроцедуры");
        let out = render(&body, &body, &opts());
        assert!(out.contains("**Result**: identical (3 lines)"), "{out}");
        assert!(!out.contains("modulo"), "{out}");
    }

    #[test]
    fn reindented_body_is_identical_modulo_whitespace() {
        let left = lines("Процедура А()\n\tВозврат;\nКонецПроцедуры");
        let right = lines("Процедура А()\n        Возврат;   \nКонецПроцедуры");
        let out = render(&left, &right, &opts());
        assert!(out.contains("identical modulo whitespace/case"), "{out}");

        let strict = DiffOptions {
            ignore_whitespace: false,
            ..opts()
        };
        let out = render(&left, &right, &strict);
        assert!(out.contains("+1 −1"), "{out}");
    }

    #[test]
    fn case_only_difference_needs_ignore_case() {
        let left = lines("Если А Тогда\nКонецЕсли;");
        let right = lines("если а тогда\nконецесли;");
        assert!(render(&left, &right, &opts()).contains("+2 −2"));
        let folded = DiffOptions {
            ignore_case: true,
            ..opts()
        };
        assert!(render(&left, &right, &folded).contains("identical modulo whitespace/case"));
    }

    #[test]
    fn changes_carry_file_line_numbers() {
        let left = lines("a\nb\nc\nd\ne");
        let right = lines("a\nb\nX\nd\ne\nf");
        let out = render(&left, &right, &opts());
        assert!(out.contains("+2 −1"), "{out}");
        assert!(out.contains("- 102     │ c"), "{out}");
        assert!(out.contains("+     202 │ X"), "{out}");
        assert!(out.contains("+     205 │ f"), "{out}");
        assert!(out.contains("  100 200 │ a"), "{out}");
        assert!(
            out.contains("## Hunk 1: left 100-104 → right 200-205"),
            "{out}"
        );
    }

    #[test]
    fn distant_changes_split_into_hunks() {
        let left: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[2] = "changed 2".into();
        right[15] = "changed 15".into();
        let narrow = DiffOptions {
            context_lines: 1,
            ..opts()
        };
        let out = render(&left, &right, &narrow);
        assert!(out.contains("2 hunk(s)"), "{out}");
        assert!(
            out.contains("## Hunk 2: left 114-116 → right 214-216"),
            "{out}"
        );
        assert!(
            !out.contains("line 8"),
            "context leaked past 1 line:\n{out}"
        );
    }

    #[test]
    fn long_diff_is_truncated_with_count() {
        let left: Vec<String> = (0..50).map(|i| format!("old {i}")).collect();
        let right: Vec<String> = (0..50).map(|i| format!("new {i}")).collect();
        let capped = DiffOptions {
            max_lines: 10,
            ..opts()
        };
        let out = render(&left, &right, &capped);
        let shown = out.lines().filter(|l| l.contains('│')).count();
        assert_eq!(shown, 10, "{out}");
        assert!(
            out.contains("Diff truncated: 90 more line(s) in 1 hunk(s)"),
            "{out}"
        );
        assert!(
            out.matches("```").count().is_multiple_of(2),
            "unclosed fence:\n{out}"
        );

        let unlimited = DiffOptions {
            max_lines: 0,
            ..opts()
        };
        let out = render(&left, &right, &unlimited);
        assert!(!out.contains("truncated"), "{out}");
        assert_eq!(out.lines().filter(|l| l.contains('│')).count(), 100);
    }

    #[test]
    fn truncation_between_hunks_counts_whole_hunks() {
        let left: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[2] = "changed 2".into();
        right[25] = "changed 25".into();
        let capped = DiffOptions {
            context_lines: 1,
            max_lines: 4,
            ..opts()
        };
        let out = render(&left, &right, &capped);
        assert!(
            out.contains("Diff truncated: 4 more line(s) in 1 hunk(s)"),
            "{out}"
        );
    }
}
