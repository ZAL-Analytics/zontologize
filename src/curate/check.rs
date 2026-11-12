use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{Answer, Built, Cluster};
use crate::facts::check::MAX_DROP_TEXT;

pub const AUTO: &str = "auto";
pub const PROPOSED: &str = "proposed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UnknownRef,
    RefTwice,
    ClassNotAllowed,
    NameNotMember,
    Confidence,
    Rejected,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::UnknownRef => "unknown ref",
            Reason::RefTwice => "ref in two decisions",
            Reason::ClassNotAllowed => "class not in the allowed list",
            Reason::NameNotMember => "name not among the members",
            Reason::Confidence => "confidence outside 0-1",
            Reason::Rejected => "joins a rejected pair",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub src_id: i64,
    pub dst_id: i64,
    pub status: &'static str,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub entity_id: i64,
    pub class_iri: Option<String>,
    pub name: Option<String>,
    pub status: &'static str,
    pub confidence: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Checked {
    pub members: Vec<i64>,
    pub links: Vec<Link>,
    pub rows: Vec<Row>,
    pub drops: BTreeMap<String, i64>,
}

impl Checked {
    fn drop(&mut self, reason: Reason, about: &str) {
        let about: String = about.chars().take(MAX_DROP_TEXT).collect();
        *self.drops.entry(format!("{reason}|{about}")).or_default() += 1;
    }
}

fn status(confidence: f64, threshold: f64) -> &'static str {
    if confidence >= threshold {
        AUTO
    } else {
        PROPOSED
    }
}

fn normal(text: &str) -> String {
    text.trim().to_lowercase()
}

pub fn check(answer: Answer, cluster: &Cluster, built: &Built, threshold: f64) -> Checked {
    let mut checked = Checked {
        members: cluster.members.iter().map(|m| m.id).collect(),
        ..Checked::default()
    };
    let refs: HashMap<String, i64> = cluster
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| (format!("m{}", i + 1), m.id))
        .collect();
    let mut uses: HashMap<i64, usize> = HashMap::new();
    let mut groups = Vec::new();
    for group in answer.groups {
        let mut ids = BTreeSet::new();
        let mut unknown = None;
        for reference in &group.members {
            match refs.get(&normal(reference)) {
                Some(id) => {
                    ids.insert(*id);
                }
                None => unknown = Some(reference.clone()),
            }
        }
        if let Some(reference) = unknown.or_else(|| ids.is_empty().then(String::new)) {
            checked.drop(Reason::UnknownRef, &reference);
            continue;
        }
        for id in &ids {
            *uses.entry(*id).or_default() += 1;
        }
        groups.push((group, ids));
    }
    let mut not_entities = Vec::new();
    for entry in answer.not_entities {
        let Some(&id) = refs.get(&normal(&entry.member)) else {
            checked.drop(Reason::UnknownRef, &entry.member);
            continue;
        };
        *uses.entry(id).or_default() += 1;
        not_entities.push((entry, id));
    }
    let twice = |id: &i64| uses.get(id).is_some_and(|n| *n > 1);
    for (group, ids) in groups {
        if ids.iter().any(twice) {
            checked.drop(Reason::RefTwice, &group.members.join(" "));
            continue;
        }
        if !(0.0..=1.0).contains(&group.confidence) {
            checked.drop(Reason::Confidence, &group.confidence.to_string());
            continue;
        }
        let Some(class) = built.classes.iter().find(|c| **c == group.class.trim()) else {
            checked.drop(Reason::ClassNotAllowed, &group.class);
            continue;
        };
        let wanted = normal(&group.name);
        let name = cluster
            .members
            .iter()
            .filter(|m| ids.contains(&m.id))
            .flat_map(|m| std::iter::once(&m.name).chain(&m.aliases))
            .find(|n| normal(n) == wanted);
        let Some(name) = name else {
            checked.drop(Reason::NameNotMember, &group.name);
            continue;
        };
        let rejected = cluster
            .rejected
            .iter()
            .any(|(a, b)| ids.contains(a) && ids.contains(b));
        if rejected {
            checked.drop(Reason::Rejected, &group.members.join(" "));
            continue;
        }
        let status = status(group.confidence, threshold);
        let mut members = ids.iter().copied();
        let Some(root) = members.next() else {
            continue;
        };
        checked.links.extend(members.map(|dst_id| Link {
            src_id: root,
            dst_id,
            status,
            confidence: group.confidence,
        }));
        checked.rows.extend(ids.iter().map(|&entity_id| Row {
            entity_id,
            class_iri: Some(class.clone()),
            name: Some(name.clone()),
            status,
            confidence: group.confidence,
        }));
    }
    for (entry, id) in not_entities {
        if twice(&id) {
            checked.drop(Reason::RefTwice, &entry.member);
            continue;
        }
        if !(0.0..=1.0).contains(&entry.confidence) {
            checked.drop(Reason::Confidence, &entry.confidence.to_string());
            continue;
        }
        checked.rows.push(Row {
            entity_id: id,
            class_iri: None,
            name: None,
            status: status(entry.confidence, threshold),
            confidence: entry.confidence,
        });
    }
    checked.links.sort_by_key(|l| (l.src_id, l.dst_id));
    checked.rows.sort_by_key(|r| r.entity_id);
    checked
}

#[cfg(test)]
mod tests {
    use super::super::tests::{built, cluster, iri, member, ontology};
    use super::super::{GroupOut, NotEntityOut};
    use super::*;

    fn group(members: &[&str], name: &str, class: &str, confidence: f64) -> GroupOut {
        GroupOut {
            members: members.iter().map(|m| m.to_string()).collect(),
            name: name.to_string(),
            class: iri(class),
            confidence,
        }
    }

    fn answer(groups: Vec<GroupOut>, not_entities: &[&str]) -> Answer {
        Answer {
            groups,
            not_entities: not_entities
                .iter()
                .map(|m| NotEntityOut {
                    member: m.to_string(),
                    confidence: 0.95,
                })
                .collect(),
        }
    }

    fn run(answer: Answer, cluster: &Cluster) -> Checked {
        let built = built(&ontology(), cluster);
        check(answer, cluster, &built, 0.9)
    }

    fn reasons(checked: &Checked) -> Vec<String> {
        let mut found: Vec<String> = checked
            .drops
            .keys()
            .map(|k| k.split('|').next().unwrap_or("").to_string())
            .collect();
        found.dedup();
        found
    }

    #[test]
    fn a_merge_links_the_lowest_id_and_names_every_member() {
        let checked = run(
            answer(
                vec![group(&["m1", "M2"], "zuidwal techniek", "Customer", 0.95)],
                &[],
            ),
            &cluster(10),
        );
        assert!(checked.drops.is_empty(), "{:?}", checked.drops);
        assert_eq!(
            checked.links,
            [Link {
                src_id: 10,
                dst_id: 15,
                status: AUTO,
                confidence: 0.95
            }]
        );
        assert_eq!(checked.rows.len(), 2);
        assert!(checked.rows.iter().all(|r| {
            r.name.as_deref() == Some("Zuidwal Techniek")
                && r.class_iri.as_deref() == Some(iri("Customer").as_str())
        }));
    }

    #[test]
    fn rejects_unknown_refs() {
        let checked = run(
            answer(
                vec![group(&["m1", "m3"], "Zuidwal Techniek", "Customer", 0.95)],
                &["m9"],
            ),
            &cluster(10),
        );
        assert_eq!(reasons(&checked), [Reason::UnknownRef.as_str()]);
        assert_eq!(checked.drops.values().sum::<i64>(), 2);
        assert!(checked.links.is_empty() && checked.rows.is_empty());
    }

    #[test]
    fn rejects_a_class_outside_the_list() {
        let checked = run(
            answer(
                vec![group(&["m1", "m2"], "Zuidwal Techniek", "Site", 0.95)],
                &[],
            ),
            &cluster(10),
        );
        assert_eq!(reasons(&checked), [Reason::ClassNotAllowed.as_str()]);
        assert!(checked.rows.is_empty());
    }

    #[test]
    fn rejects_a_ref_in_two_groups_or_also_not_an_entity() {
        let checked = run(
            answer(
                vec![
                    group(&["m1"], "Zuidwal Technick", "Customer", 0.95),
                    group(&["m1", "m2"], "Zuidwal Techniek", "Customer", 0.95),
                ],
                &[],
            ),
            &cluster(10),
        );
        assert_eq!(reasons(&checked), [Reason::RefTwice.as_str()]);
        assert_eq!(checked.drops.values().sum::<i64>(), 2);
        assert!(checked.rows.is_empty());
        let checked = run(
            answer(
                vec![group(&["m2"], "Zuidwal Techniek", "Customer", 0.95)],
                &["m2"],
            ),
            &cluster(10),
        );
        assert_eq!(checked.drops.values().sum::<i64>(), 2);
        assert!(checked.rows.is_empty());
    }

    #[test]
    fn rejects_a_name_not_among_the_members() {
        let mut aliased = cluster(10);
        aliased.members[1].aliases = vec!["ZT".to_string()];
        let checked = run(
            answer(
                vec![group(&["m1", "m2"], "Zuidwal Tech", "Customer", 0.95)],
                &[],
            ),
            &aliased,
        );
        assert_eq!(reasons(&checked), [Reason::NameNotMember.as_str()]);
        let checked = run(
            answer(vec![group(&["m1", "m2"], "zt", "Customer", 0.95)], &[]),
            &aliased,
        );
        assert_eq!(checked.rows[0].name.as_deref(), Some("ZT"));
        let checked = run(
            answer(vec![group(&["m1"], "ZT", "Customer", 0.95)], &[]),
            &aliased,
        );
        assert_eq!(reasons(&checked), [Reason::NameNotMember.as_str()]);
    }

    #[test]
    fn a_group_over_a_rejected_pair_is_dropped() {
        let mut three = cluster(10);
        three.members.push(member(20, "Zuidwal", "Customer"));
        three.rejected = vec![(10, 20)];
        let checked = run(
            answer(
                vec![group(&["m1", "m2", "m3"], "Zuidwal", "Customer", 0.95)],
                &[],
            ),
            &three,
        );
        assert_eq!(reasons(&checked), [Reason::Rejected.as_str()]);
        let checked = run(
            answer(
                vec![group(&["m1", "m2"], "Zuidwal Technick", "Customer", 0.95)],
                &[],
            ),
            &three,
        );
        assert_eq!(checked.links.len(), 1);
    }

    #[test]
    fn status_follows_the_confidence_setting() {
        let checked = run(
            answer(
                vec![group(&["m1", "m2"], "Zuidwal Techniek", "Customer", 0.6)],
                &["m1"],
            ),
            &cluster(10),
        );
        assert_eq!(reasons(&checked), [Reason::RefTwice.as_str()]);
        let checked = run(
            answer(
                vec![group(&["m2"], "Zuidwal Techniek", "Customer", 0.6)],
                &["m1"],
            ),
            &cluster(10),
        );
        assert!(checked.links.is_empty());
        assert_eq!(checked.rows[0].status, AUTO);
        assert_eq!(checked.rows[0].class_iri, None);
        assert_eq!(checked.rows[1].status, PROPOSED);
        let checked = run(
            answer(
                vec![group(&["m1", "m2"], "Zuidwal Techniek", "Customer", 1.5)],
                &[],
            ),
            &cluster(10),
        );
        assert_eq!(reasons(&checked), [Reason::Confidence.as_str()]);
    }
}
