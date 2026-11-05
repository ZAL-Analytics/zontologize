# zontologize

A Rust CLI that indexes folders into PostgreSQL, shaped by an OWL ontology (Turtle). It stores
documents, pages, chunks, entities, relations and the provenance of every fact in its own schema.
Runs are incremental: only changed files and stages are processed. CPU only.

Status: early development.

## Requirements

- Rust stable (edition 2024), cmake and a C++ compiler
- PostgreSQL 18 with the `vector` (0.8+) and `age` (1.8+) extensions

## Setup

```sh
cargo build --release
```

Create the extensions once as a superuser (zontologize never does this itself):

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS age;
```

Set `DATABASE_URL` (or the `PG*` variables), then create the schema:

```sh
zontologize --config my-config db migrate
zontologize --config my-config db check
```

## Config

A config directory (`--config DIR` or `ZONTOLOGIZE_CONFIG`) holds everything specific to your
data. All files are optional; see `examples/acme/` for a complete example and
[docs/configuration.md](docs/configuration.md) for every setting.

```
my-config/
  zontologize.toml   database, sources, ontology, OCR, models, budgets
  rules.toml         path rules, ID patterns, document types
  ontology/*.ttl     ontology files
  gazetteer/*.csv    known entities and aliases
  prompts/*.md       prompt overrides
```

## Run

```sh
zontologize --config my-config ontology check   # validate the ontology
zontologize --config my-config scan             # index new and changed files
zontologize --config my-config ontology fit     # propose ontology changes for review
zontologize --config my-config classify         # document types rules missed
zontologize --config my-config extract          # LLM extraction
zontologize --config my-config embed            # local embeddings, see docs/embeddings.md
zontologize --config my-config resolve          # merge duplicates, link documents
zontologize --config my-config status
```

Each command has `--help`.

## License

Apache-2.0
