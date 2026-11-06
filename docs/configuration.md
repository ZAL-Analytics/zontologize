# Configuration

Settings come from, in order of precedence (last wins):

1. Built-in defaults: `defaults/zontologize.toml` and `defaults/rules.toml` (the full key list).
2. The config directory (`--config DIR` or `ZONTOLOGIZE_CONFIG`). Only set what you change.
   `zontologize.toml` merges key by key, tables included (`[providers.*]`, `[embed.models.*]`,
   `resolve.similar.weights`), with one exception: `[scan.kinds]` replaces the default kinds as a
   whole, so list every kind you want to read. Every other file (`rules.toml`, `prompts/*.md`)
   replaces the built-in file as a whole: a `rules.toml` without `[ocr_confusions]` has no OCR
   confusions, and one with `[[page_types]]` needs `pages.head_chars`.
3. Environment: `ZONTOLOGIZE_<SECTION>__<KEY>`, for example `ZONTOLOGIZE_EXTRACT__THREADS=4`.
   Deeper keys add another `__` (`ZONTOLOGIZE_PROVIDERS__OLLAMA__THINK=true`). A value is read
   as TOML, except for keys that hold text, which take it as is.
4. Command-line flags.

An unknown key or bad value stops the run before it touches the database.

## Environment variables

| Variable | Use |
|---|---|
| `DATABASE_URL` or `PGHOST`, `PGUSER`, `PGPASSWORD`, ... | PostgreSQL connection |
| `OPENROUTER_API_KEY` | `openrouter` provider |
| `OLLAMA_HOST` | Ollama server (default `http://localhost:11434`) |
| `CLAUDE_CODE_BIN` | `claude` binary (default: on `PATH`) |
| `HF_HOME`, `HF_HUB_CACHE` | Hugging Face model cache |

Secrets never go in config files.

The `claude` binary gets a cleared environment with only `PATH`, `HOME`, `USER`, `LOGNAME`,
`LANG`, `LC_*`, `TZ`, `TMPDIR`, the `XDG_*` folders, `CLAUDE_CONFIG_DIR`, the proxy variables
and the CA certificate variables (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `SSL_CERT_DIR`). It
logs in with its own stored login; `ANTHROPIC_API_KEY` and database variables are not passed.

## zontologize.toml

In the tables, `0` means "all cores", "no limit" or "auto" unless noted.

### Sources

```toml
[[sources]]
name = "projects"          # unique
kind = "folder"            # folder (default) or table
root = "/data/projects"    # absolute or relative to the config directory
dataset = "current"        # optional label
ignore = ["**/*.bak"]      # optional globs

[[sources]]
name = "register"
kind = "table"
view = "files.register"    # schema.view, read only
root = "/data/register"    # files named by the view must lie under it
```

A `table` source reads its file list from a view instead of walking `root`. The view needs the
columns `id` (any type, sorted as text), `path` (the path stored for the document), `file` (the
file to read, relative to `root` or absolute under it; null: `path`), `sha256` (null: the file is
hashed), `size` and `modified` (timestamp). `ignore` and `[scan.kinds]` apply to `path`.

### Core

| Key | Default | Meaning |
|---|---|---|
| `database.schema` | `"zontologize"` | Schema to write |
| `database.graph` | `"zontologize_graph"` | AGE graph that `graph sync` rebuilds; must differ from the schemas |
| `database.api_schema` | `"zontologize_api"` | Schema of the views for apps, made by `db migrate` |
| `database.max_connections` | `8` | Connection pool size |
| `ontology.files` | `[]` | Turtle files, relative to the config directory |
| `ontology.languages` | `["en"]` | Label languages, first preferred |
| `ontology.include` / `exclude` | `[]` | Root classes to select / leave out |
| `ontology.document_class` | `""` | Class given to every document |
| `ocr.languages` | `["en"]` | OCR languages |
| `ocr.model_tier` | `"small"` | `small` or `medium` |
| `scan.ignore` | lock and system files | Globs never read |
| `scan.batch_size` | `1000` | Files per round of the walk |
| `scan.hash_threads` | `0` | Threads that hash files |
| `scan.kinds.<kind>.extensions` | per kind | File extensions per kind |
| `scan.kinds.<kind>.max_mb` | `0` | Skip larger files |
| `chunk.max_chars` / `overlap` | `2000` / `200` | Chunk size |
| `chunk.text_search` | `"simple"` | PostgreSQL text search config |

### Extraction

| Key | Default | Meaning |
|---|---|---|
| `extract.workers` | `0` | Worker processes (0: one per 4 cores) |
| `extract.threads` | `0` | Threads per worker |
| `extract.worker_memory_mb` | `0` | Memory per worker (0: split evenly) |
| `extract.timeout_secs` | `300` | Per file |
| `extract.format` | `"markdown"` | `markdown` or `plain` |
| `extract.embedded_depth` | `0` | How deep to open attachments |
| `extract.max_embedded_mb` | `10` | Largest attachment opened |
| `extract.worker_files` | `500` | Files a worker process handles before it is replaced |
| `extract.ocr_embedded_images` | `false` | OCR images inside documents |

### Models

Names are `provider:model`. Providers: `claude-code`, `openrouter`, `ollama`. Embeddings are
local only (`local:<name>`, see [embeddings.md](embeddings.md)).

| Key | Default |
|---|---|
| `models.fit` | `"claude-code:sonnet"` |
| `models.extract` | `"claude-code:sonnet"` |
| `models.classify` | `"claude-code:haiku"` |
| `models.attributes` | `"claude-code:sonnet"` |
| `models.embed` | `"local:arctic-embed-m-v2.0"` |
| `models.chars_per_token` | `2.0` |

`models.chars_per_token` turns characters into tokens for the cost estimate until calls have been
recorded.

Per provider (`[providers.claude-code]`, `[providers.openrouter]`, `[providers.ollama]`):
`concurrency`, `timeout_secs`; plus `max_window_use` (claude-code), `base_url` (openrouter),
`think` (ollama), `poll_minutes` (openrouter: how often to check the key when funds run out).

| Key | Default | Meaning |
|---|---|---|
| `budget.max_usd` | `0` | Spend cap per job |
| `budget.max_requests` | `0` | Request cap per job |
| `budget.calibration_calls` | `200` | Recorded calls used to estimate tokens per request |
| `budget.default_output_tokens` | `300` | Output tokens per request until calls are recorded |
| `llm.retry_backoff_secs` | `2` | First wait before a retry; doubles per attempt |
| `llm.wait_margin_secs` | `60` | Extra wait after a usage window resets |
| `llm.unknown_reset_wait_minutes` | `30` | Wait when a limit gives no reset time |
| `llm.progress_secs` | `60` | Seconds between progress lines of a model job |

### Stage tuning

The defaults work for most data. Commonly changed:

| Key | Default | Meaning |
|---|---|---|
| `fit.sample` | `1000` | Chunks sent to `ontology fit` |
| `fit.out` | `"fit"` | Report folder |
| `fit.seed` / `llm_extract.seed` | `"zontologize"` | Sample seed |
| `fit.min_chars` | `200` | Shortest chunk sampled |
| `fit.unmatched_share` / `llm_extract.unmatched_share` | `0.3` | Share of the sample taken from chunks no class name matched |
| `fit.max_shortlist` / `llm_extract.max_shortlist` | `30` | Most classes offered per chunk |
| `fit.min_evidence` / `llm_extract.min_evidence` | `3` | Chunks needed before a new class or term is proposed |
| `fit.proposal_namespace` | `"urn:zontologize:proposal#"` | IRI prefix of proposed classes |
| `llm_extract.sources` / `doc_types` / `paths` | `[]` | Limit what `extract` sends |
| `llm_extract.exclude` | `[]` | Paths never sent |
| `llm_extract.min_chars` | `200` | Shortest chunk sent |
| `llm_extract.confidence` | `0.7` | Confidence of model facts |
| `llm_extract.max_per_document` | `3` | Most chunks per document in a sample |
| `llm_extract.min_letter_share` / `embed.min_letter_share` | `0.5` | Chunks with fewer letters are skipped |
| `llm_extract.min_ocr_confidence` / `embed.min_ocr_confidence` | `0.6` | Chunks on OCR pages below this are skipped |
| `llm_extract.resend_share` | `0.2` | Share of chunks sent again without `--yes` |
| `llm_extract.out` | `"extract"` | Report folder |
| `llm_extract.max_known` | `0` | Most known entities offered per chunk (0: `llm_extract.max_shortlist`) |
| `llm_extract.threads` | `0` | Threads that build requests (0: `scan.hash_threads`) |
| `attributes.confidence` | `0.7` | Confidence of model values until reviews set it |
| `attributes.context_chars` | `400` | End of the previous chunk sent as context, for table headers |
| `attributes.max_text_chars` | `120` | Longest text value kept |
| `attributes.max_properties` | `30` | Most properties asked per chunk; properties named in the text go first |
| `attributes.max_subjects` | `50` | Most known things offered per chunk, in entity order |
| `attributes.out` | `"attributes"` | Report folder |
| `attributes.sources` / `doc_types` / `paths` | `[]` | Limit what `attributes` sends (empty: the `llm_extract` value) |
| `attributes.seed` | `""` | Sample seed (empty: `llm_extract.seed`) |
| `attributes.resend_share` | unset | Share of chunks sent again without `--yes` (unset: `llm_extract.resend_share`) |
| `attributes.threads` | `0` | Threads that build requests (0: `scan.hash_threads`) |
| `attributes.unit_symbols` | `12` | Unit symbols listed per quantity kind in the prompt |
| `classify.head_chars` | `1500` | Text sent per part |
| `classify.min_chars` | `50` | Parts with less text are not sent |
| `classify.confidence` | `0.7` | Confidence of a model document type |
| `fit.definition_chars` / `llm_extract.definition_chars` / `classify.definition_chars` | `200` | Longest class definition in a prompt |
| `review.sample` | `50` | Facts shown by `review sample` |
| `review.min_verdicts` | `30` | Reviews of a class or property before they set its confidence (0: never) |
| `embed.threads` / `batch_size` | `0` / `8` | Embedding throughput |
| `embed.min_chars` | `50` | Shorter chunks are skipped |
| `embed.max_tokens` | `512` | Longest input in tokens for a model that sets no `max_tokens` |
| `embed.max_aliases` | `5` | Aliases added to an entity's text |
| `embed.out` | `"eval"` | Report folder of `eval embed` |
| `embed.eval_sample` / `eval_max_expected` | `500` / `20` | Questions made by `eval embed`, most expected chunks per question |
| `embed.cache_dir` | `""` | Model cache (empty: standard HF cache) |
| `embed.progress_secs` | `30` | Seconds between progress lines |
| `resolve.strip_words` | `[]` | Words ignored in names, such as legal forms |
| `resolve.fuzzy_score` / `embed_score` | `0.92` / `0.9` | Match thresholds |
| `resolve.classes` | `[]` | Limit to these classes |
| `resolve.max_candidates` | `5` | Nearest neighbours compared per entity |
| `resolve.block_chars` / `max_block` | `4` / `1000` | Name prefix that groups candidates, largest group compared |
| `resolve.similar.property` | `"urn:zontologize#similarTo"` | IRI of similarity links |
| `resolve.similar.tolerance` | `0.2` | Relative gap at which a value no longer counts as close |
| `resolve.similar.min_shared` / `min_score` / `max_links` | `2` / `0.8` / `5` | Values two entities must share, lowest score, links per entity |
| `resolve.similar.weights` | `{}` | Weight per property name, default 1 |

`attributes` also uses `llm_extract.min_chars`.

Batch sizes and threads, rarely changed:

| Key | Default | Meaning |
|---|---|---|
| `scan.extract_batch` / `chunk_batch` / `rules_batch` | `200` / `200` / `100` | Rows read per round by extract, chunk and the text rules |
| `scan.rules_threads` | `0` | Threads of the rules stage (0: `scan.hash_threads`) |
| `chunk.threads` | `0` | Threads of the chunk stage (0: `scan.hash_threads`) |
| `llm_extract.read_batch` / `attributes.read_batch` | `2000` | Chunks read per round when planning |
| `classify.read_batch` | `500` | Documents read per round |
| `embed.read_batch` | `2000` | Rows read per round |
| `resolve.neighbour_batch` | `500` | Entities per nearest-neighbour query |

### Export

| Key | Default | Meaning |
|---|---|---|
| `export.ontology_iri` | `"urn:zontologize:ontology"` | IRI of the `owl:Ontology` that `ontology export` writes |

## Units and attribute properties

Attribute values need units with conversion factors. Download QUDT's vocabularies (CC BY 4.0)
into the config directory and list them in `ontology.files`:

```sh
curl -L -H "Accept: text/turtle" -o ontology/qudt-unit.ttl https://qudt.org/vocab/unit/
curl -L -H "Accept: text/turtle" -o ontology/qudt-quantitykind.ttl https://qudt.org/vocab/quantitykind/
```

A datatype property becomes an attribute when its domain is selected and its range is a number
(`xsd:decimal`, `xsd:integer`, ...) or text (`xsd:string`, no range). A number with a unit names
its quantity kind; a unit fits when it has the same dimension vector and, when any unit of
that dimension lists the kind with `qudt:hasQuantityKind`, also lists it (or lists no kind).
A property's domain and range may be an `owl:unionOf` of classes. A property without a domain
(or an object property without a range) fits no class and is not extracted:

```turtle
ex:dischargePressure a owl:DatatypeProperty ;
    rdfs:domain ex:Pump ; rdfs:range xsd:decimal ;
    qudt:hasQuantityKind quantitykind:Pressure ;
    zontologize:similarityTolerance 0.1 ;          # optional, see resolve.similar
    skos:prefLabel "discharge pressure"@en, "persdruk"@nl .
```

Units are matched on `qudt:symbol`, `rdfs:label` and `skos:altLabel`, exact
first, then case-insensitive when only one unit fits. Add the spellings your documents use as
`skos:altLabel` in the local file (`unit:M3-PER-HR skos:altLabel "m3/hr" .`); the
`attributes-report.md` lists the unit texts that matched nothing. Gauge pressure is a unit of
its own with an offset of one atmosphere, so values convert to absolute pascal:

```turtle
ex:BAR_G a qudt:Unit ; qudt:symbol "barg" ; skos:altLabel "bar(g)" ;
    qudt:conversionMultiplier 100000.0 ; qudt:conversionOffset 1.01325 ;
    qudt:hasDimensionVector qkdv:A0E0L-1I0M1H0T-2D0 .
```

## rules.toml

Rules find facts without a model. Class and property names are prefixed names or IRIs.
Path patterns match the path relative to the source root; `{group}` inserts a named group.
`examples/acme/rules.toml` is a full example.

```toml
[doc_types]
invoice = "acme:Invoice"

[[path_rules]]
pattern = '^Projects/(?P<project>P-\d{4}-\d{4})/'
entities = [{ id = "p", class = "acme:Project", key = "{project}" }]
relations = [{ from = "document", to = "p", property = "acme:describes" }]

[[path_rules]]
pattern = '/Invoices/'
doc_type = "invoice"

[[id_patterns]]
class = "acme:Project"
pattern = '\bP-\d{4}-\d{4}\b'
on = "both"                  # path, text or both

[ocr_confusions]
"0" = "Oo"

[[page_types]]               # split one file into parts by page
doc_type = "invoice"
pattern = '(?i)^invoice'

[pages]
head_chars = 400             # page text searched for a page type
part_property = "acme:partOf"

[[revisions]]                # link later revisions with dcterms:replaces
pattern = '(?P<series>DWG-\d{3}) rev (?P<rev>[A-Z0-9]+)\.'
```

## Gazetteer

`gazetteer/*.csv` with header `class,key,name,aliases,lang`. Aliases are separated by `|`.
Names and aliases match as whole words, case-insensitive.

## Prompts

`prompts/<name>.md` replaces a built-in prompt (`fit-*`, `extract-*`, `classify-*`, `attributes-*`, see
`prompts/`). Variables are written `{name}`. A user prompt must contain the text variable
(`{chunk}`, or `{text}` for `classify-user`). A value that contains `<<<` or `>>>` gets a space
inside the run (`<< <`), so the text cannot close the delimiters.

## questions.toml

Optional test questions for `eval embed`:

```toml
[[questions]]
text = "Which drawing shows the pipe section?"
expect = ["Projects/2021/P-2021-0042*/04 Reports/*"]
```
