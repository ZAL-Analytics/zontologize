# zontologize

[![Crates.io](https://img.shields.io/crates/v/zontologize.svg)](https://crates.io/crates/zontologize)
[![Downloads](https://img.shields.io/crates/d/zontologize.svg)](https://crates.io/crates/zontologize)
[![License](https://img.shields.io/crates/l/zontologize.svg)](LICENSE)

**Turn a folder of documents into a knowledge graph in PostgreSQL, shaped by your own ontology.**

Point zontologize at folders of PDFs, Office files, mail and scans, give it an OWL ontology, and
it fills PostgreSQL with what those files know: documents, pages, text chunks, the entities they
mention, how those entities relate, and where every fact came from. Search, RAG and agent apps
then read one clean schema instead of a file share.

## What it does

- **Reads everything.** PDF, Word, Excel, PowerPoint, mail and images, with OCR for scans.
- **Fits the ontology to your data.** Start from a reference model; zontologize reports which
  classes your documents use and proposes the missing ones for you to review.
- **Extracts facts with LLMs, safely.** Every answer is checked against the ontology; anything that
  does not fit is dropped. Works with Claude Code, OpenRouter or a local Ollama model.
- **Keeps provenance.** Every entity, relation and value points to its document, page, chunk,
  method and confidence.
- **Cleans up.** Merges duplicate entities, links documents that name or replace each other, and
  reads values with units.
- **Embeds locally.** Multilingual embedding models run on your CPU or GPU into pgvector.
- **Builds a graph.** Projects entities and relations into an Apache AGE graph for Cypher queries.
- **Scales and stays incremental.** Built for hundreds of thousands of files: re-runs only touch
  what changed, and every core is kept busy.

```
folders ─► scan (text, OCR, chunks, rules) ─► extract (LLM + ontology) ─► resolve ─► Postgres + AGE graph
```

## Install

You need Rust (stable), `cmake` and a C++ compiler.

```sh
cargo install zontologize --locked                 # CPU
cargo install zontologize --locked --features gpu  # OCR and embeddings on an NVIDIA GPU (CUDA 13)
```

## Set up

**1. PostgreSQL 18** with `pgvector` (0.8+), Apache AGE (1.8+) and `pg_trgm`. Create the
extensions once as a superuser:

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS age;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

**2. A config directory** for your data. Every file is optional; start by copying
[`examples/acme`](examples/acme):

```
my-config/
  zontologize.toml   sources, database, ontology, OCR, models, budgets
  rules.toml         path rules, ID patterns, document types
  ontology/*.ttl     your ontology
  gazetteer/*.csv    known entities and aliases
  prompts/*.md       prompt overrides
```

**3. Connect and create the schema.** Set `DATABASE_URL` (or the `PG*` variables), plus
`OPENROUTER_API_KEY` if you use OpenRouter. Secrets never go in config files.

```sh
export ZONTOLOGIZE_CONFIG=my-config
zontologize db migrate
zontologize db check
zontologize models check
```

## Run

```sh
zontologize scan            # index new and changed files
zontologize ontology fit    # propose ontology changes for review
zontologize extract         # LLM extraction of entities and relations
zontologize attributes      # values with units
zontologize embed           # local embeddings
zontologize resolve         # merge duplicates, link documents
zontologize graph sync      # rebuild the AGE graph
zontologize status          # what is indexed, recent runs and errors
```

Run the same chain from cron for each new snapshot; only changed files are processed. Model jobs
take `--dry-run` for a cost estimate and `--max-usd` as a cap. Every command has `--help`; the
rest (`classify`, `curate`, `review`, `eval`, `ontology export`) is listed there.

## Docs

- [Configuration](docs/configuration.md): every setting
- [Embeddings](docs/embeddings.md): models, picking one, querying from an app
- [Reading the database](docs/consumers.md): the views, the graph and attributes for apps

## License

Apache-2.0
