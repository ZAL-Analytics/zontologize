use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};

use sha2::{Digest, Sha256};

use crate::facts::check::MARKUP;
use crate::text::fold;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: i64,
    pub document_id: i64,
    pub stratum: String,
    pub text_hash: u64,
    pub letters: f64,
    pub ocr_confidence: Option<f64>,
    pub matched: bool,
}

pub struct Settings<'a> {
    pub size: usize,
    pub seed: &'a str,
    pub max_per_document: usize,
    pub unmatched_share: f64,
    pub min_letter_share: f64,
    pub min_ocr_confidence: f64,
}

#[derive(Debug, Default, PartialEq)]
pub struct Selected {
    pub ids: Vec<i64>,
    pub duplicates: usize,
    pub junk: usize,
}

pub fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    fold(text).hash(&mut hasher);
    hasher.finish()
}

pub fn letter_share(text: &str) -> f64 {
    let (mut letters, mut visible) = (0usize, 0usize);
    for c in text
        .chars()
        .filter(|c| !c.is_whitespace() && !MARKUP.contains(c) && *c != '-' && *c != '_')
    {
        visible += 1;
        if c.is_alphabetic() {
            letters += 1;
        }
    }
    if visible == 0 {
        return 0.0;
    }
    letters as f64 / visible as f64
}

pub fn first_folder(path: &str) -> &str {
    match path.split_once('/') {
        Some((folder, _)) => folder,
        None => "",
    }
}

fn rank(seed: &str, id: i64) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update(id.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

fn spread(candidates: Vec<&Candidate>, seed: &str) -> Vec<i64> {
    let mut strata: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
    for candidate in candidates {
        strata
            .entry(&candidate.stratum)
            .or_default()
            .push(candidate);
    }
    let mut ranked: Vec<(usize, [u8; 8], i64)> = Vec::new();
    for list in strata.values_mut() {
        list.sort_by_key(|c| rank(seed, c.id));
        for (position, candidate) in list.iter().enumerate() {
            ranked.push((position, rank(seed, candidate.id), candidate.id));
        }
    }
    ranked.sort();
    ranked.into_iter().map(|(_, _, id)| id).collect()
}

pub fn select(candidates: &[Candidate], settings: &Settings) -> Selected {
    let mut selected = Selected::default();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by_key(|c| c.id);
    let mut kept: Vec<&Candidate> = Vec::new();
    for candidate in ordered {
        let low_ocr = candidate
            .ocr_confidence
            .is_some_and(|c| c < settings.min_ocr_confidence);
        if low_ocr || candidate.letters < settings.min_letter_share {
            selected.junk += 1;
            continue;
        }
        if !seen.insert(candidate.text_hash) {
            selected.duplicates += 1;
            continue;
        }
        kept.push(candidate);
    }
    kept.sort_by_key(|c| rank(settings.seed, c.id));
    let mut per_document: HashMap<i64, usize> = HashMap::new();
    kept.retain(|c| {
        let count = per_document.entry(c.document_id).or_default();
        *count += 1;
        *count <= settings.max_per_document
    });
    let (matched, unmatched): (Vec<&Candidate>, Vec<&Candidate>) =
        kept.into_iter().partition(|c| c.matched);
    let matched = spread(matched, settings.seed);
    let unmatched = spread(unmatched, settings.seed);
    let wanted = (settings.size as f64 * settings.unmatched_share).round() as usize;
    let mut take_unmatched = wanted.min(unmatched.len());
    let take_matched = (settings.size - take_unmatched).min(matched.len());
    take_unmatched = (settings.size - take_matched).min(unmatched.len());
    let (first, second) = (&matched[..take_matched], &unmatched[..take_unmatched]);
    let mut order: Vec<(f64, i64)> = Vec::new();
    for part in [first, second] {
        for (i, id) in part.iter().enumerate() {
            order.push((i as f64 / part.len() as f64, *id));
        }
    }
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    selected.ids = order.into_iter().map(|(_, id)| id).collect();
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: i64, document: i64, stratum: &str, text: &str, matched: bool) -> Candidate {
        Candidate {
            id,
            document_id: document,
            stratum: stratum.to_string(),
            text_hash: text_hash(text),
            letters: letter_share(text),
            ocr_confidence: None,
            matched,
        }
    }

    fn settings(size: usize) -> Settings<'static> {
        Settings {
            size,
            seed: "s",
            max_per_document: 2,
            unmatched_share: 0.25,
            min_letter_share: 0.5,
            min_ocr_confidence: 0.6,
        }
    }

    #[test]
    fn letters_and_folders() {
        assert!((letter_share("ab 12") - 0.5).abs() < 1e-9);
        assert_eq!(letter_share("   "), 0.0);
        assert!((letter_share("| ab | 12 |\n|---|---|") - 0.5).abs() < 1e-9);
        assert_eq!(first_folder("2021/a/b.pdf"), "2021");
        assert_eq!(first_folder("b.pdf"), "");
        assert_eq!(text_hash("The  Pump"), text_hash("the pump"));
    }

    #[test]
    fn drops_junk_and_duplicates_and_caps_documents() {
        let mut low = candidate(4, 3, "a", "a scanned page of text", true);
        low.ocr_confidence = Some(0.3);
        let all = vec![
            candidate(1, 1, "a", "pump report one", true),
            candidate(2, 2, "a", "Pump  report ONE", true),
            candidate(3, 2, "a", "12 34 56 78 90", true),
            low,
            candidate(5, 5, "a", "valve report", true),
            candidate(6, 5, "a", "valve report two", true),
            candidate(7, 5, "a", "valve report three", true),
        ];
        let picked = select(&all, &settings(10));
        assert_eq!(picked.duplicates, 1);
        assert_eq!(picked.junk, 2);
        let mut ids = picked.ids.clone();
        ids.sort();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids.iter().filter(|id| [5, 6, 7].contains(id)).count(), 2);
        assert!(ids.contains(&1));
    }

    #[test]
    fn spreads_over_strata_and_keeps_the_unmatched_share() {
        let mut all = Vec::new();
        for i in 0..40 {
            all.push(candidate(i, i, "big", &format!("big text {i}"), true));
        }
        for i in 40..44 {
            all.push(candidate(i, i, "small", &format!("small text {i}"), true));
        }
        for i in 44..60 {
            all.push(candidate(i, i, "big", &format!("none text {i}"), false));
        }
        let picked = select(&all, &settings(8));
        assert_eq!(picked.ids.len(), 8);
        assert_eq!(picked.ids.iter().filter(|id| **id >= 44).count(), 2);
        assert!(
            picked
                .ids
                .iter()
                .filter(|id| (40..44).contains(*id))
                .count()
                >= 2
        );
        assert_eq!(select(&all, &settings(8)), picked);
    }
}
