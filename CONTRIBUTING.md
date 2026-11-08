# Contributing

## Checks

Every change passes these before it is merged:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
ZONTOLOGIZE_TEST_DATABASE_URL=postgres://... cargo test -- --include-ignored
```

The second test run needs PostgreSQL with the `vector`, `age` and `pg_trgm` extensions. A change to a hot
path (walk, hash, extract, chunk, rules, database writes) comes with `cargo bench` numbers.

## Code

- Write the least code that solves the task. No options or abstractions for later.
- No comments or doc comments in code. The reason for a change goes in the commit message.
- No warnings and no inline `#[allow(...)]`. A needed lint exception goes in `Cargo.toml`.
- No `unwrap` or `expect` outside tests, except on values fixed at compile time.
- Errors name the file, key or id they are about.
- Nothing in `src/` knows a domain. Folder names, document types, ID formats, languages, limits
  and model names come from config or parameters.
- No new dependency for a few lines of code. Name the reason when you add one.

## Tests

- Unit tests sit in a `#[cfg(test)] mod tests` at the bottom of the file they test.
- Integration tests live in `tests/`. Tests that need PostgreSQL are `#[ignore]`, use their own
  schema and drop it.
- Fixtures in `tests/fixtures/` are made up. Never commit a real document or real
  organisation data, not even redacted.
- Tests never call a real model or outside service. Model calls use recorded responses from
  `tests/fixtures/llm/`.
- A bug fix comes with a test that fails without it.

## Configuration

- Every setting has a default in `defaults/`, a line in `examples/acme/` and a row in
  `docs/configuration.md`.
- `zontologize.toml` merges key by key; every other config file replaces the built-in file as a
  whole.
- Secrets come from environment variables only. A config file never holds a key or password.

## Database

- SQL lives in `src/db/`, one file per table, with bound parameters. Names from config are
  checked against `^[a-z_][a-z0-9_]*$` and quoted.
- Schema changes go in a new file in `migrations/`. A merged migration is never edited.
- zontologize writes only its own schema, its views schema and its graph.
- Every entity, mention and relation records its document, chunk or page, method and
  confidence.
- A `confirmed` or `rejected` relation is never overwritten.

## Ontology and models

- Extraction schemas, graph labels and validation rules are generated from the Turtle ontology,
  never written by hand.
- Rules first (path rules, ID patterns, gazetteer), models only for what rules cannot do.
- Model output is parsed into typed structs, checked against the ontology and dropped when it
  does not fit.
- Prompts are text files in `prompts/`.
- Every stage has a version constant. Bump it only when the stage output changes; it forces a
  rerun of that stage.

## Git

- Branch from `main`: `feat/<short-task>` or `fix/<short-task>`.
- Commit subjects are imperative and under 72 characters; the body says why.
- A pull request covers one change and passes every check above.
