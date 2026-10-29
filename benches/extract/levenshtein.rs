pub fn distance(a: &[char], b: &[char]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

pub fn normalize(text: &str) -> Vec<char> {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect()
}

pub fn character_error_rate(ocr: &str, truth: &str) -> Option<f64> {
    let truth = normalize(truth);
    if truth.is_empty() {
        return None;
    }
    Some(distance(&normalize(ocr), &truth) as f64 / truth.len() as f64)
}
