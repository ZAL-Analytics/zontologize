pub mod gliner;
pub mod run;

use gliner::Word;

pub const METHOD: &str = "gliner";

fn is_capitalised(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(char::is_uppercase)
        && text.chars().all(char::is_alphabetic)
        && chars.any(char::is_lowercase)
}

pub fn candidates(text: &str, words: &[Word]) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if !is_capitalised(&text[word.start..word.end]) {
            continue;
        }
        let next = words[i + 1..].iter().take(3).find(|w| {
            let part = &text[w.start..w.end];
            !crate::rules::persons::is_particle(part)
        });
        if next.is_some_and(|w| is_capitalised(&text[w.start..w.end])) {
            out.push(i);
        }
    }
    out
}

pub fn windows(
    candidates: &[usize],
    words: usize,
    context: usize,
    max_words: usize,
) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &at in candidates {
        let from = at.saturating_sub(context);
        let to = (at + context + 1).min(words);
        match out.last_mut() {
            Some(last) if from <= last.1 && to - last.0 <= max_words => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::gliner::words;
    use super::*;

    #[test]
    fn candidates_are_two_capitalised_words_close_together() {
        let text = "send it to Jan de Vries and the Pump Skid, not PUMP SKID or pump Skid";
        let found = words(text);
        let starts: Vec<&str> = candidates(text, &found)
            .into_iter()
            .map(|i| &text[found[i].start..found[i].end])
            .collect();
        assert_eq!(starts, ["Jan", "Pump"]);
    }

    #[test]
    fn windows_merge_and_stay_under_the_limit() {
        assert_eq!(windows(&[5, 8, 40], 100, 3, 50), [(2, 12), (37, 44)]);
        assert_eq!(windows(&[0, 4], 6, 3, 5), [(0, 4), (1, 6)]);
        assert!(windows(&[], 10, 3, 5).is_empty());
    }
}
