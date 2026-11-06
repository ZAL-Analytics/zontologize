use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::db::resolve::DocumentEntity;
use crate::rules::Revision;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Number(u128),
    Text(String),
}

fn parts(text: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        let digit = c.is_ascii_digit();
        let mut run = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() != digit {
                break;
            }
            run.push(c);
            chars.next();
        }
        if digit {
            out.push(Part::Number(run.parse().unwrap_or(u128::MAX)));
        } else {
            out.push(Part::Text(run.to_lowercase()));
        }
    }
    out
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (left, right) = (parts(a), parts(b));
    for (x, y) in left.iter().zip(&right) {
        let order = match (x, y) {
            (Part::Number(x), Part::Number(y)) => x.cmp(y),
            (Part::Number(_), Part::Text(_)) => Ordering::Less,
            (Part::Text(_), Part::Number(_)) => Ordering::Greater,
            (Part::Text(x), Part::Text(y)) => x.cmp(y),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len()).then_with(|| a.cmp(b))
}

type Series<'a> = BTreeMap<(i64, usize, &'a str), Vec<(&'a str, usize)>>;

pub fn chains(documents: &[DocumentEntity], revisions: &[Revision]) -> Vec<(usize, usize)> {
    let mut series: Series = BTreeMap::new();
    for (i, document) in documents.iter().enumerate() {
        for (rule, revision) in revisions.iter().enumerate() {
            if let Some((name, rev)) = revision.parse(&document.path) {
                series
                    .entry((document.source_id, rule, name))
                    .or_default()
                    .push((rev, i));
                break;
            }
        }
    }
    let mut links = Vec::new();
    for list in series.values_mut() {
        list.sort_by(|a, b| natural_cmp(a.0, b.0).then(a.1.cmp(&b.1)));
        list.dedup_by(|later, earlier| natural_cmp(later.0, earlier.0) == Ordering::Equal);
        for pair in list.windows(2) {
            links.push((pair[1].1, pair[0].1));
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_compares_numbers_by_value() {
        let mut revs = vec!["10", "B2", "2", "B", "A", "B1"];
        revs.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(revs, ["2", "10", "A", "B", "B1", "B2"]);
        assert_eq!(natural_cmp("rev 02", "rev 2"), "rev 02".cmp("rev 2"));
        assert_eq!(natural_cmp("0", "1"), Ordering::Less);
    }

    fn document(id: i64, path: &str) -> DocumentEntity {
        DocumentEntity {
            document_id: id,
            source_id: 1,
            path: path.to_string(),
            entity_id: id * 10,
        }
    }

    #[test]
    fn a_chain_of_three_revisions_gives_two_links() {
        let revision = Revision {
            regex: regex::Regex::new(r"(?P<series>DWG-\d+) rev (?P<rev>\w+)").unwrap(),
            order: crate::rules::RevisionOrder::Natural,
        };
        let documents = vec![
            document(1, "drawings/DWG-310 rev B.pdf"),
            document(2, "drawings/DWG-310 rev A.pdf"),
            document(3, "drawings/DWG-310 rev C.pdf"),
            document(4, "drawings/DWG-311 rev A.pdf"),
            document(5, "letters/letter.txt"),
        ];
        let links = chains(&documents, &[revision]);
        assert_eq!(links, [(0, 1), (2, 0)]);
    }
}
