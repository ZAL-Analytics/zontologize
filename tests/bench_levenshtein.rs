#[path = "../benches/extract/levenshtein.rs"]
mod levenshtein;

mod tests {
    use super::levenshtein::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn distance_counts_edits() {
        assert_eq!(distance(&chars(""), &chars("")), 0);
        assert_eq!(distance(&chars("abc"), &chars("")), 3);
        assert_eq!(distance(&chars(""), &chars("abc")), 3);
        assert_eq!(distance(&chars("kitten"), &chars("sitting")), 3);
        assert_eq!(distance(&chars("P-2022-0158"), &chars("P-2O22-0158")), 1);
        assert_eq!(distance(&chars("pomp"), &chars("pomp")), 0);
    }

    #[test]
    fn distance_handles_multibyte_characters() {
        assert_eq!(distance(&chars("één"), &chars("een")), 2);
    }

    #[test]
    fn error_rate_ignores_whitespace_layout() {
        assert_eq!(character_error_rate("a  b\nc", "a b c"), Some(0.0));
        assert_eq!(character_error_rate("abcd", "abce"), Some(0.25));
        assert_eq!(character_error_rate("text", "  "), None);
    }
}
