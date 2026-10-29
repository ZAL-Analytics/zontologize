# zontologize

A Rust CLI that indexes folders into PostgreSQL, shaped by an ontology.

Point it at one or more folders and an OWL ontology (Turtle). It fills its own schema with
documents, pages, text chunks, entities, relations and the provenance of every fact (file, page,
chunk, method, confidence), and mirrors entities and relations into an Apache AGE graph. Apps
read the database for search, RAG and agents.

It is built for large folders that are re-indexed on a schedule. Each run is an incremental
snapshot: only files and stages that changed are processed, on CPU, with all cores busy.

## Status

Early development.

## Planned features

- **Ontology fit.** Start from a public reference model (for example CFIHOS or ISO 14224
  extracts), let `ontology fit` report which classes the documents use and propose subclasses
  and aliases.
- **Scan.** Walk, hash (sha256), extract text per page with `xberg`, OCR scans, chunk, and full-text index.
- **Rules first.** Path rules, ID patterns and gazetteers create entities and relations without a
  model.
- **LLM extraction.** Anthropic API, Claude Code, OpenRouter or Ollama, with a JSON schema built
  from the ontology. Output is validated against domain and range rules; responses are cached
  and runs have budget caps.
- **Embeddings and resolution.** pgvector embeddings for chunks, entities and class labels;
  duplicate entities merged by keys, aliases and similarity.
- **Graph and export.** AGE graph sync, Turtle export of the fitted ontology, and a stable views
  schema for consumers that shows complete runs only.
- **Attributes and units.** Numeric attributes normalised to SI units.

## Requirements

- Rust stable (edition 2024)
- PostgreSQL 18 with the `vector` (0.8+) and `age` (1.8+) extensions installed
- cmake and a C++ compiler; network access on first build. 

## Setup

```sh
cargo build --release
```

zontologize never creates extensions. Prepare the database once as a superuser:

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS age;
```

The connection comes from `DATABASE_URL` or the standard libpq variables (`PGHOST`, `PGUSER`,
`PGPASSWORD`, `PGDATABASE`, ...). Then create the schema:

```sh
zontologize --config my-config db migrate
zontologize --config my-config db check
```

## Configuration

Everything organisation-specific lives in a config directory outside this repo
(`--config <dir>` or `ZONTOLOGIZE_CONFIG`). Every file is optional; missing files use the
built-in defaults in `defaults/`. See `examples/acme/` for a complete made-up example.

```
my-config/
  zontologize.toml   database, sources, ontology selection, OCR, models, budgets
  ontology/*.ttl     reference ontology and local extensions
  rules.toml         path rules, ID patterns, ignore lists
  gazetteer/*.csv    known entities and aliases
  prompts/*.md       replacements for built-in prompts
```

## Intended use

Run it from cron or a job queue against the same config directory:

```sh
zontologize --config my-config ontology check     # validate the ontology
zontologize --config my-config scan               # index new and changed files
zontologize --config my-config ontology fit       # report and proposals, reviewed by a person
zontologize --config my-config extract            # LLM extraction for what rules missed
zontologize --config my-config embed
zontologize --config my-config resolve
zontologize --config my-config graph sync
zontologize --config my-config status
```

Only `db migrate` and `db check` exist today; the other commands arrive with their phases.

## License

Apache-2.0
