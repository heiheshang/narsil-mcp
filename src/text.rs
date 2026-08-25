//! UTF-8-safe text truncation helpers.
//!
//! Byte-slicing a `&str` at a fixed offset (`&s[..500]`) panics when the cut
//! lands inside a multi-byte character. Cyrillic is 2 bytes per character, so
//! roughly every second offset is unsafe — on a 1C/BSL corpus a plain
//! `&content[..500]` is a crash, not an edge case. The same bug has now been
//! fixed three times in different files; these helpers exist so display
//! truncation is written once and cannot regress.

use std::borrow::Cow;

/// The longest prefix of `s` that fits in `max_bytes`, cut on a character
/// boundary. Returns `s` unchanged when it already fits.
///
/// The result can be up to 3 bytes shorter than `max_bytes` — the boundary
/// moves backwards, never forwards, so the cap is never exceeded.
#[must_use]
pub fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[..s.floor_char_boundary(max_bytes)]
}

/// The longest *suffix* of `s` that fits in `max_bytes`, cut on a character
/// boundary. Used for "first 4 … last 4" redaction of secrets, where the tail
/// is as likely to land mid-character as the head.
#[must_use]
pub fn truncate_start(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    &s[s.ceil_char_boundary(s.len() - max_bytes)..]
}

/// [`truncate`], with `...` appended when something was actually cut.
///
/// Borrows when the text already fits, so the common case does not allocate.
#[must_use]
pub fn truncate_with_ellipsis(s: &str, max_bytes: usize) -> Cow<'_, str> {
    if s.len() <= max_bytes {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(format!("{}...", truncate(s, max_bytes)))
    }
}

/// Whether [`truncate`] would shorten `s`.
#[must_use]
pub fn is_truncated(s: &str, max_bytes: usize) -> bool {
    s.len() > max_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module exists for: every one of these cuts lands
    /// inside a Cyrillic character and used to panic with
    /// "byte index N is not a char boundary".
    #[test]
    fn test_truncate_never_splits_cyrillic() {
        let text = "xСуммаДокументаПоВсемСтрокамТабличнойЧастиТоваровИУслуг = 0;";
        assert!(
            !text.is_char_boundary(60),
            "test string lost its odd offset"
        );

        for max in 1..text.len() {
            let cut = truncate(text, max);
            assert!(cut.len() <= max, "truncate exceeded the cap at {max}");
            assert!(text.starts_with(cut), "truncate did not return a prefix");
            // Constructing `cut` at all proves the boundary was respected.
        }
    }

    #[test]
    fn test_truncate_start_never_splits_cyrillic() {
        let text = "postgres://user:парольОтБазы@";
        for max in 1..text.len() {
            let tail = truncate_start(text, max);
            assert!(
                tail.len() <= max,
                "truncate_start exceeded the cap at {max}"
            );
            assert!(
                text.ends_with(tail),
                "truncate_start did not return a suffix"
            );
        }
    }

    #[test]
    fn test_truncate_returns_input_when_it_fits() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn test_truncate_start_returns_input_when_it_fits() {
        assert_eq!(truncate_start("hello", 10), "hello");
        assert_eq!(truncate_start("hello", 5), "hello");
        assert_eq!(truncate_start("hello", 3), "llo");
        assert_eq!(truncate_start("", 0), "");
    }

    /// A boundary-shortened cut is still under the cap, and the caller can tell
    /// truncation happened from the input length rather than the output length.
    #[test]
    fn test_multibyte_cut_lands_below_the_cap() {
        let cyrillic = "Процедура ОбработкаПроведения() ".repeat(2_000);
        let cut = truncate(&cyrillic, 23_000);
        assert_eq!(cut.len(), 22_999, "expected the boundary to move backwards");
        assert!(is_truncated(&cyrillic, 23_000));
        assert!(!is_truncated(cut, 23_000));
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
        assert!(matches!(
            truncate_with_ellipsis("hello", 10),
            Cow::Borrowed(_)
        ));

        // Multi-byte: the ellipsis is appended to a boundary-safe prefix.
        let cyrillic = "Процедура";
        let cut = truncate_with_ellipsis(cyrillic, 5);
        assert_eq!(
            cut, "Пр...",
            "the cap of 5 bytes holds two 2-byte characters"
        );
    }

    #[test]
    fn test_truncate_handles_astral_planes() {
        // 4-byte characters: the boundary can move back up to 3 bytes.
        let emoji = "🔥🔥🔥";
        assert_eq!(truncate(emoji, 5), "🔥");
        assert_eq!(truncate_start(emoji, 5), "🔥");
        assert_eq!(truncate(emoji, 0), "");
        assert_eq!(truncate_start(emoji, 0), "");
    }
}
