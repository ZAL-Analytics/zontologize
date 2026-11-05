# Configuration

Settings come from, in order of precedence (last wins):

1. Built-in defaults: `defaults/zontologize.toml` and `defaults/rules.toml` (the full key list).
2. The config directory (`--config DIR` or `ZONTOLOGIZE_CONFIG`). Only set what you change.
   `rules.toml` and `[scan.kinds]` replace the defaults as a whole.
3. Environment: `ZONTOLOGIZE_<SECTION>__<KEY>`, for example `ZONTOLOGIZE_EXTRACT__THREADS=4`.
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

## zontologize.toml

In the tables, `0` means "all cores", "no limit" or "auto" unless noted.

### Sources

```toml
[[sources]]
name = "projects"          # unique
root = "/data/projects"    # absolute or relative to the config directory
dataset = "current"        # optional label
ignore = ["**/*.bak"]      # optional globs
```

### Core

| Key | Default | Meaning |
|---|---|---|
| `database.schema` | `"zontologize"` | Schema to write |
| `database.max_connections` | `8` | Connection pool size |
| `ontology.files` | `[]` | Turtle files, relative to the config directory |
| `ontology.languages` | `["en"]` | Label languages, first preferred |
| `ontology.include` / `exclude` | `[]` | Root classes to select / leave out |
| `ontology.document_class` | `""` | Class given to every document |
| `ocr.languages` | `["en"]` | OCR languages |
| `ocr.model_tier` | `"small"` | `small` or `medium` |
| `scan.ignore` | lock and system files | Globs never read |
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
| `extract.ocr_embedded_images` | `false` | OCR images inside documents |

### Models

Names are `provider:model`. Providers: `claude-code`, `openrouter`, `ollama`. Embeddings are
local only (`local:<name>`, see [embeddings.md](embeddings.md)).

| Key | Default |
|---|---|
| `models.fit` | `"claude-code:sonnet"` |
| `models.extract` | `"claude-code:sonnet"` |
| `models.classify` | `"claude-code:haiku"` |
| `models.embed` | `"local:arctic-embed-m-v2.0"` |

Per provider (`[providers.claude-code]`, `[providers.openrouter]`, `[providers.ollama]`):
`concurrency`, `timeout_secs`; plus `max_window_use` (claude-code), `base_url` (openrouter),
`think` (ollama).

| Key | Default | Meaning |
|---|---|---|
| `budget.max_usd` | `0` | Spend cap per job |
| `budget.max_requests` | `0` | Request cap per job |

### Stage tuning

The defaults work for most data. Commonly changed:

| Key | Default | Meaning |
|---|---|---|
| `fit.sample` | `1000` | Chunks sent to `ontology fit` |
| `fit.out` | `"fit"` | Report folder |
| `llm_extract.sources` / `doc_types` / `paths` | `[]` | Limit what `extract` sends |
| `llm_extract.exclude` | `[]` | Paths never sent |
| `llm_extract.min_chars` | `200` | Shortest chunk sent |
| `llm_extract.confidence` | `0.7` | Confidence of model facts |
| `classify.head_chars` | `1500` | Text sent per part |
| `review.sample` | `50` | Facts shown by `review sample` |
| `embed.threads` / `batch_size` | `0` / `8` | Embedding throughput |
| `embed.min_chars` | `50` | Shorter chunks are skipped |
| `embed.cache_dir` | `""` | Model cache (empty: standard HF cache) |
| `resolve.strip_words` | `[]` | Words ignored in names, such as legal forms |
| `resolve.fuzzy_score` / `embed_score` | `0.92` / `0.9` | Match thresholds |
| `resolve.classes` | `[]` | Limit to these classes |

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
part_property = "acme:partOf"

[[revisions]]                # link later revisions with dcterms:replaces
pattern = '(?P<series>DWG-\d{3}) rev (?P<rev>[A-Z0-9]+)\.'
```

## Gazetteer

`gazetteer/*.csv` with header `class,key,name,aliases,lang`. Aliases are separated by `|`.
Names and aliases match as whole words, case-insensitive.

## Prompts

`prompts/<name>.md` replaces a built-in prompt (`fit-*`, `extract-*`, `classify-*`, see
`prompts/`). Variables are written `{name}`.

## questions.toml

Optional test questions for `eval embed`:

```toml
[[questions]]
text = "Which drawing shows the pipe section?"
expect = ["Projects/2021/P-2021-0042*/04 Reports/*"]
```
