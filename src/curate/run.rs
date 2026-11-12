use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};
use globset::GlobSet;
use sqlx::PgPool;

use super::check::{Checked, check};
use super::{
    Answer, Built, Cluster, Context, Member, Pattern, Relation, TASK, USER_VARIABLES, build,
};
use crate::config::{self, Config};
use crate::db;
use crate::db::entity_classes::Override;
use crate::db::facts::Confidence;
use crate::db::llm::Owner;
use crate::db::resolve::{NewLink, SAME_AS};
use crate::llm::ModelName;
use crate::llm::prompt::{self, Prompt};
use crate::llm::stage::{self, Base, Chunk, Counts, Keys, Plan, Stage};
use crate::ontology::{self, Ontology};

pub use crate::llm::job::{Options, Outcome};

struct Setup<'a> {
    base: Base<'a>,
    ontology: Ontology,
    system: Prompt,
    user: Prompt,
    clusters: BTreeMap<i64, Cluster>,
    skipped: usize,
    sources: [(&'static str, usize); 4],
    out: PathBuf,
}

struct Prepared {
    cluster: Cluster,
    built: Built,
}

pub async fn run(
    config_dir: Option<&Path>,
    url: Option<&str>,
    options: &Options,
) -> Result<Outcome> {
    let config = config::load(config_dir)?;
    if config.models.curate.is_empty() {
        bail!("[models] curate is empty; set it to provider:model, for example claude-code:sonnet");
    }
    let name = ModelName::configured(&config.models.curate, &config.providers)
        .map_err(|e| anyhow::anyhow!("models.curate: {e}"))?;
    if config.ontology.files.is_empty() {
        bail!("[ontology] files is empty");
    }
    let loaded = ontology::load_checked(config_dir, &config.ontology)?;
    let system = prompt::load(config_dir, "curate-system")?;
    system.check(&[])?;
    let user = prompt::load(config_dir, "curate-user")?;
    user.check(&USER_VARIABLES)?;
    user.require("members")?;
    let lock = match options.dry_run {
        true => None,
        false => Some(db::runs::lock(url, &config.database.schema).await?),
    };
    let pool = db::pool::connect(
        url,
        &config.database.schema,
        config.database.max_connections,
    )
    .await?;
    let result = curate(
        &pool,
        &config,
        &name,
        loaded.ontology,
        system,
        user,
        options,
    )
    .await;
    pool.close().await;
    db::runs::release(lock).await;
    result
}

async fn curate(
    pool: &PgPool,
    config: &Config,
    name: &ModelName,
    ontology: Ontology,
    system: Prompt,
    user: Prompt,
    options: &Options,
) -> Result<Outcome> {
    let curate = &config.curate;
    let patterns = super::patterns(&ontology, &curate.not_in_class)?;
    let (clusters, skipped, sources) = clusters(pool, config, &ontology, &patterns).await?;
    let version = ontology.version.clone();
    let setup = Setup {
        base: Base {
            pool,
            config,
            name,
            task: TASK,
            ontology_version: &version,
            sources: &[],
            paths: &[],
            read_batch: clusters.len().max(1),
            seed: "",
            resend_share: 1.0,
            confidence: curate.confidence,
        },
        ontology,
        system,
        user,
        clusters,
        skipped,
        sources,
        out: PathBuf::from(&curate.out),
    };
    let outcome = stage::run(&setup, options).await?;
    if !options.dry_run {
        let mut stats = crate::resolve::Stats::default();
        crate::resolve::canonical(pool, &mut stats).await?;
        println!(
            "curate: {} clusters, {} canonical ids changed",
            stats.clusters, stats.canonical_changed
        );
    }
    Ok(outcome)
}

type Clusters = (BTreeMap<i64, Cluster>, usize, [(&'static str, usize); 4]);

async fn clusters(
    pool: &PgPool,
    config: &Config,
    ontology: &Ontology,
    patterns: &[Pattern],
) -> Result<Clusters> {
    let proposed = db::curate::proposed(pool).await?;
    let roots = db::curate::roots(pool).await?;
    let classes: HashMap<i64, String> = roots.iter().map(|r| (r.id, r.class_iri.clone())).collect();
    let normaliser = crate::resolve::names::Normaliser::new(&config.resolve.strip_words);
    let names = db::curate::names(pool).await?;
    let disjoint = super::disjoint(
        ontology,
        &normaliser,
        &names,
        &classes,
        config.curate.max_members,
    );
    let (components, skipped) =
        super::components(&proposed, &disjoint.pairs, config.curate.max_members);
    let skipped = skipped + disjoint.skipped;
    let mut hits = disjoint.hits;
    hits.extend(super::misfits(
        ontology,
        &db::curate::ends(pool).await?,
        &classes,
    ));
    hits.extend(super::name_hits(ontology, patterns, &roots));
    let components = super::join(components, hits);
    let sources = super::sources(&components);
    let ids: Vec<i64> = components
        .iter()
        .flat_map(|c| c.ids.iter().copied())
        .collect();
    let around = config.curate.context_chars;
    let mut snippets: HashMap<i64, Vec<String>> = HashMap::new();
    for row in db::curate::snippets(pool, &ids, around).await? {
        snippets
            .entry(row.entity_id)
            .or_default()
            .push(crate::review::context(
                &row.text,
                Some(row.span_start),
                Some(row.span_end),
                around,
            ));
    }
    let mut relations: HashMap<i64, Vec<Relation>> = HashMap::new();
    for row in db::curate::relations(pool, &ids).await? {
        relations.entry(row.entity_id).or_default().push(Relation {
            property: row.property_iri,
            outgoing: row.outgoing,
            name: row.name,
            class: row.class_iri,
        });
    }
    let mut members: HashMap<i64, Member> = db::curate::members(pool, &ids)
        .await?
        .into_iter()
        .map(|row| {
            let member = Member {
                id: row.id,
                name: row.name,
                class: row.class_iri,
                aliases: row.aliases,
                mentions: row.mentions,
                snippets: snippets.remove(&row.id).unwrap_or_default(),
                relations: relations.remove(&row.id).unwrap_or_default(),
            };
            (row.id, member)
        })
        .collect();
    let rejected = db::curate::rejected(pool, &ids).await?;
    let mut out = BTreeMap::new();
    for component in components {
        let set: BTreeSet<i64> = component.ids.iter().copied().collect();
        let cluster = Cluster {
            id: component.ids[0],
            members: component
                .ids
                .iter()
                .filter_map(|id| members.remove(id))
                .collect(),
            links: component.links,
            hits: component.hits,
            rejected: rejected
                .iter()
                .filter(|(a, b)| set.contains(a) && set.contains(b))
                .copied()
                .collect(),
        };
        out.insert(cluster.id, cluster);
    }
    Ok((out, skipped, sources))
}

fn link(
    checked: &super::check::Link,
    method: &str,
    sources: &HashMap<i64, (Option<i64>, Option<i64>)>,
) -> NewLink {
    let of = |id: i64| sources.get(&id).copied().unwrap_or_default();
    let (a, b) = (of(checked.src_id), of(checked.dst_id));
    let chunk_id = a.0.or(b.0);
    NewLink {
        src_id: checked.src_id,
        dst_id: checked.dst_id,
        property: SAME_AS.to_string(),
        method: method.to_string(),
        status: checked.status,
        confidence: checked.confidence,
        props: serde_json::json!({}),
        document_id: if chunk_id.is_none() {
            a.1.or(b.1)
        } else {
            None
        },
        chunk_id,
        span_start: None,
        span_end: None,
    }
}

impl Stage for Setup<'_> {
    type Item = Prepared;
    type Answer = Answer;
    type Checked = Checked;
    const SAMPLED: &'static str = "clusters";
    const WRITES: &'static str = "decisions";
    const OWNER: Owner = Owner::Entity;

    fn base(&self) -> &Base<'_> {
        &self.base
    }

    fn keys(item: &Prepared) -> Keys<'_> {
        Keys {
            chunk_id: item.cluster.id,
            key: &item.built.cache_key,
            cache_key: &item.built.cache_key,
            request: &item.built.request,
        }
    }

    fn drops(checked: &Checked) -> &BTreeMap<String, i64> {
        &checked.drops
    }

    async fn read(
        &self,
        after: i64,
        _paths: Option<&GlobSet>,
        _sampling: bool,
        _plan: &mut Plan,
    ) -> Result<Option<(i64, Vec<Chunk>)>> {
        let chunks: Vec<Chunk> = self
            .clusters
            .range(after + 1..)
            .map(|(id, _)| Chunk {
                id: *id,
                key: None,
                source: String::new(),
                document_id: 0,
                doc_type: None,
                path: String::new(),
                text: String::new(),
                ocr_confidence: None,
                matched: true,
            })
            .collect();
        Ok(chunks.last().map(|c| c.id).map(|last| (last, chunks)))
    }

    async fn prepare(&self, ids: &[i64]) -> Result<Vec<Prepared>> {
        let config = self.base.config;
        let context = Context {
            ontology: &self.ontology,
            languages: &config.ontology.languages,
            max_shortlist: config.curate.max_shortlist,
            definition_chars: config.curate.definition_chars,
            system: &self.system,
            user: &self.user,
            model: self.base.name,
        };
        Ok(ids
            .iter()
            .filter_map(|id| self.clusters.get(id))
            .map(|cluster| Prepared {
                built: build(&context, cluster),
                cluster: cluster.clone(),
            })
            .collect())
    }

    fn check(&self, item: Prepared, answer: Answer) -> Checked {
        check(
            answer,
            &item.cluster,
            &item.built,
            self.base.config.curate.confidence,
        )
    }

    async fn write(
        &self,
        checked: &[Checked],
        method: &str,
        _confidence: &Confidence,
        version_id: i64,
    ) -> Result<()> {
        let method = format!("{}{method}", db::curate::METHOD);
        let pool = self.base.pool;
        for cluster in checked {
            let sources = db::resolve::provenance(pool, &cluster.members).await?;
            let links: Vec<NewLink> = cluster
                .links
                .iter()
                .map(|l| link(l, &method, &sources))
                .collect();
            let rows: Vec<Override> = cluster
                .rows
                .iter()
                .map(|r| Override {
                    entity_id: r.entity_id,
                    class_iri: r.class_iri.clone(),
                    name: r.name.clone(),
                    method: method.clone(),
                    status: r.status.to_string(),
                    confidence: r.confidence,
                    ontology_version_id: version_id,
                })
                .collect();
            db::curate::write(pool, &cluster.members, &links, &rows).await?;
        }
        Ok(())
    }

    fn summary(&self, plan: &Plan, counts: &Counts, _started: Instant) {
        println!(
            "curate: {} clusters, {} larger than max_members skipped, {} from the cache, \
             {} to send, {} invalid before",
            plan.eligible, self.skipped, counts.hits, counts.send, counts.invalid,
        );
        let sources: Vec<String> = self
            .sources
            .iter()
            .map(|(source, count)| format!("{count} from {source}"))
            .collect();
        println!("curate: clusters {}", sources.join(", "));
    }

    async fn clear(&self, _stale: &[i64], _dry_run: bool) -> Result<()> {
        Ok(())
    }

    async fn tidy(&self) -> Result<()> {
        Ok(())
    }

    async fn report(&self, job: Option<i64>) -> Result<()> {
        super::report::write_report(
            self.base.pool,
            self.base.config,
            &self.out,
            self.skipped,
            &self.sources,
            job,
        )
        .await
    }
}
