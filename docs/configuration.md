# Configuration

zontologize reads its settings from a config directory, given with `--config DIR` or the
`ZONTOLOGIZE_CONFIG` environment variable. Every setting has a built-in default (the files in
`defaults/`), so a config directory only needs the keys you want to change.
`examples/acme/` is a complete config directory for a made-up company.

## The config directory

| Path | What it holds |
|---|---|
| `zontologize.toml` | Database, extraction, models, embeddings and every other setting below. |
| `rules.toml` | Path rules, ID patterns, document types, page types, OCR confusions and revision rules. |
| `gazetteer/*.csv` | Known names (customers, sites, ...) with their aliases. |
| `ontology/*.ttl` | The OWL ontology in Turtle, listed in `[ontology] files`. |
| `prompts/*.md` | Optional replacements for the built-in model prompts. |
| `questions.toml` | Optional questions for `zontologize eval embed`. |

## Precedence

1. Built-in defaults (`defaults/zontologize.toml`, `defaults/rules.toml`).
2. The config directory. `zontologize.toml` merges key by key with the defaults; `[scan.kinds]`
   replaces the default table as a whole. `rules.toml` replaces the default rules file as a whole.
3. Environment variables `ZONTOLOGIZE_<SECTION>__<KEY>`, for example
   `ZONTOLOGIZE_EXTRACT__THREADS=4` or `ZONTOLOGIZE_OCR__LANGUAGES='["nl", "en"]'`. The value is
   read as TOML and falls back to a plain string.
4. Command-line flags.

A wrong key or value stops the run before it touches the database, with the file, the key and
what was expected.

## Environment variables

| Variable | Use |
|---|---|
| `ZONTOLOGIZE_CONFIG` | The config directory. |
| `ZONTOLOGIZE_<SECTION>__<KEY>` | Override one setting. |
| `DATABASE_URL` or `PGHOST`, `PGUSER`, `PGPASSWORD`, `PGDATABASE`, ... | The PostgreSQL connection. |
| `OPENROUTER_API_KEY` | Key for the `openrouter` provider. |
| `OLLAMA_HOST` | Address of the Ollama server (default `http://localhost:11434`). |
| `CLAUDE_CODE_BIN` | Path of the `claude` binary for the `claude-code` provider (default: `claude` on `PATH`). |
| `HF_HOME`, `HF_HUB_CACHE` | Where Hugging Face model files are cached, unless `[embed] cache_dir` is set. |

Secrets never go in a config file; a key such as `api_key` in `zontologize.toml` is an error.

## zontologize.toml

### [database]

| Key | Type | Default | What it does |
|---|---|---|---|
| `schema` | string | `"zontologize"` | Schema zontologize writes to; must match `^[a-z_][a-z0-9_]*$`. |
| `max_connections` | integer | `8` | Most database connections a run opens. |

### [extract]

| Key | Type | Default | What it does |
|---|---|---|---|
| `threads` | integer | `0` | Threads per extraction worker; 0 means all cores. |
| `timeout_secs` | integer | `300` | Time one file may take before it gets an error row. |
| `format` | `"markdown"` or `"plain"` | `"markdown"` | Text format of extracted pages. |
| `embedded_depth` | integer | `0` | How deep attachments and embedded files are opened; 0 means not at all. |
| `max_embedded_mb` | integer | `10` | Largest embedded file that is opened. |
| `ocr_embedded_images` | boolean | `false` | Whether images inside documents are OCRed. |
| `workers` | integer | `0` | Extraction worker processes; 0 means one per 4 cores. |
| `worker_memory_mb` | integer | `0` | Memory limit per worker; 0 splits the machine's memory over the workers. |
| `worker_files` | integer | `500` | Files a worker handles before it is restarted. |

### [ocr]

| Key | Type | Default | What it does |
|---|---|---|---|
| `languages` | list of strings | `["en"]` | OCR languages. |
| `model_tier` | `"small"` or `"medium"` | `"small"` | PaddleOCR model size. |

### [ontology]

| Key | Type | Default | What it does |
|---|---|---|---|
| `files` | list of paths | `[]` | Turtle files, relative to the config directory. |
| `languages` | list of strings | `["en"]` | Label languages, the first one preferred. |
| `include` | list of class names | `[]` | Roots of the selected classes (prefixed names or IRIs). |
| `exclude` | list of class names | `[]` | Classes left out of the selection with their subclasses. |
| `document_class` | class name | `""` | Class of the entity every document gets; empty means none. |

### [scan]

| Key | Type | Default | What it does |
|---|---|---|---|
| `batch_size` | integer | `1000` | Files handled per database batch. |
| `hash_threads` | integer | `0` | Threads that hash files; 0 means all cores. |
| `ignore` | list of globs | lock files, `Thumbs.db`, `.git`, ... | Paths never read, in every source. |

`[scan.kinds.<kind>]` maps file extensions to a kind (`pdf`, `word`, `sheet`, `slides`, `mail`,
`text`, `image` by default) and has two keys:

| Key | Type | Default | What it does |
|---|---|---|---|
| `extensions` | list of strings | per kind | Extensions of that kind, without the dot. |
| `max_mb` | integer | `0` | Larger files are skipped; 0 means no limit. |

### [chunk]

| Key | Type | Default | What it does |
|---|---|---|---|
| `max_chars` | integer | `2000` | Longest chunk. |
| `overlap` | integer | `200` | Characters shared by neighbouring chunks. |
| `text_search` | identifier | `"simple"` | PostgreSQL text search configuration of the `tsv` column. |

### [models]

Model names are `provider:model`. Providers for text models are `claude-code`, `openrouter` and
`ollama`. Embeddings are local only: `embed` is `local:<name>`, where `<name>` is an entry of
`[embed.models]`; see [embeddings.md](embeddings.md).

| Key | Type | Default | What it does |
|---|---|---|---|
| `fit` | model name | `"claude-code:sonnet"` | Model for `ontology fit`. |
| `extract` | model name | `"claude-code:sonnet"` | Model for `extract`. |
| `classify` | model name | `"claude-code:haiku"` | Model for `classify`. |
| `embed` | `local:<name>` | `"local:multilingual-e5-base"` | Embedding model for `embed`, `eval embed` and `resolve`. |
| `chars_per_token` | number | `2.0` | Characters per token for estimates. |

### [providers.claude-code], [providers.openrouter], [providers.ollama]

| Key | Type | Default | What it does |
|---|---|---|---|
| `claude-code.concurrency` | integer | `2` | Requests at the same time. |
| `claude-code.timeout_secs` | integer | `300` | Time one request may take. |
| `claude-code.max_window_use` | number | `0.8` | Pause when the subscription usage window is used this far (0 to 1). |
| `openrouter.base_url` | URL | `"https://openrouter.ai/api/v1"` | API address. |
| `openrouter.concurrency` | integer | `8` | Requests at the same time. |
| `openrouter.timeout_secs` | integer | `120` | Time one request may take. |
| `openrouter.poll_minutes` | integer | `30` | How often a paused job checks the credit again. |
| `ollama.concurrency` | integer | `1` | Requests at the same time. |
| `ollama.timeout_secs` | integer | `900` | Time one request may take. |
| `ollama.think` | boolean | `false` | Whether thinking models think before they answer. |

### [budget]

| Key | Type | Default | What it does |
|---|---|---|---|
| `max_usd` | number | `0` | Spending cap per job for paid providers; 0 means no cap. |
| `max_requests` | integer | `0` | Request cap per job; 0 means no cap. |
| `calibration_calls` | integer | `200` | Calls whose measured size replaces the first estimate. |
| `default_output_tokens` | integer | `300` | Output tokens assumed before anything is measured. |

### [fit]

| Key | Type | Default | What it does |
|---|---|---|---|
| `sample` | integer | `1000` | Chunks sent to the fit model. |
| `seed` | string | `"zontologize"` | Seed of the sample. |
| `unmatched_share` | number | `0.3` | Share of the sample from chunks that no rule matched. |
| `min_chars` | integer | `200` | Shortest chunk in the sample. |
| `max_shortlist` | integer | `30` | Most classes offered to the model per chunk. |
| `min_evidence` | integer | `3` | Chunks a proposal needs before it is listed. |
| `proposal_namespace` | IRI | `"urn:zontologize:proposal#"` | Namespace of proposed classes. |
| `out` | path | `"fit"` | Folder of the fit report and proposals. |

### [llm_extract]

| Key | Type | Default | What it does |
|---|---|---|---|
| `min_chars` | integer | `200` | Shortest chunk sent. |
| `sources` | list of source names | `[]` | Only these sources; empty means all. |
| `doc_types` | list of document types | `[]` | Only these document types; empty means all. |
| `paths` | list of globs | `[]` | Only these paths; empty means all. |
| `exclude` | list of globs | `[]` | Paths never sent. |
| `seed` | string | `"zontologize"` | Seed of `extract --sample` and of `eval embed`. |
| `max_per_document` | integer | `3` | Most chunks per document in a sample. |
| `unmatched_share` | number | `0.3` | Share of a sample from chunks that no rule matched. |
| `min_letter_share` | number | `0.5` | Least share of letters among visible characters. |
| `min_ocr_confidence` | number | `0.6` | Least OCR confidence of the pages under a chunk. |
| `max_shortlist` | integer | `30` | Most classes offered per chunk. |
| `confidence` | number | `0.7` | Confidence of model facts until review measures it. |
| `resend_share` | number | `0.2` | Above this share of changed chunks, `extract` asks for `--yes`. |
| `min_evidence` | integer | `3` | Chunks a relation proposal needs. |
| `out` | path | `"extract"` | Folder of the extract report. |

### [classify]

| Key | Type | Default | What it does |
|---|---|---|---|
| `head_chars` | integer | `1500` | Characters of a part sent to the model. |
| `min_chars` | integer | `50` | Parts with less text are not sent. |
| `confidence` | number | `0.7` | Confidence of a model document type. |

### [review]

| Key | Type | Default | What it does |
|---|---|---|---|
| `sample` | integer | `50` | Facts or links shown by `review sample` and `review merges`. |
| `min_verdicts` | integer | `30` | Verdicts per class before the measured precision replaces the configured confidence. |

### [embed]

| Key | Type | Default | What it does |
|---|---|---|---|
| `threads` | integer | `0` | ONNX Runtime threads; 0 means all cores. |
| `batch_size` | integer | `8` | Texts per model call and per database transaction. |
| `max_tokens` | integer | `512` | Token limit for models that do not set their own. |
| `min_chars` | integer | `50` | Shorter chunks are not embedded. |
| `min_letter_share` | number | `0.5` | Chunks with a lower share of letters are not embedded. |
| `min_ocr_confidence` | number | `0.6` | Chunks on OCR pages below this confidence are not embedded. |
| `max_aliases` | integer | `5` | Aliases added to an entity's text. |
| `cache_dir` | path | `""` | Hugging Face cache for model files; empty means the standard cache. |
| `out` | path | `"eval"` | Folder of `eval/embed-report.md`. |
| `eval_sample` | integer | `500` | Chunk queries in `eval embed`, and as many distractor chunks. |
| `eval_max_expected` | integer | `20` | Most chunks per entity used as expected neighbours. |

### [embed.models."name"]

One table per embedding model. The defaults describe five models, `multilingual-e5-base`,
`multilingual-e5-small`, `bge-m3`, `arctic-embed-m-v2.0` and `qwen3-embedding-0.6b` (see
[embeddings.md](embeddings.md)); a config directory can add models or change single keys of a
built-in one.

| Key | Type | Default | What it does |
|---|---|---|---|
| `repo` | string | required | Hugging Face repository, `owner/name`. |
| `revision` | string | `""` | Commit to download; empty means the current `main`, and the commit is recorded. |
| `file` | path | `"onnx/model.onnx"` | ONNX file in the repository. |
| `data_files` | list of paths | `[]` | External weight files next to the ONNX file. |
| `tokenizer` | path | `"tokenizer.json"` | Tokenizer file in the repository. |
| `pooling` | `"mean"`, `"cls"`, `"last"` or `"none"` | required | How token vectors become one vector; `none` for outputs that are already pooled. |
| `output` | string | `""` | Model output to read; empty means the first output. |
| `normalize` | boolean | `true` | Scale vectors to length 1. |
| `passage_prefix` | string | `""` | Text put before every chunk, entity and class. |
| `query_prefix` | string | `""` | Text put before every question. |
| `max_tokens` | integer | `[embed] max_tokens` | Longer texts are cut. |

### [resolve]

| Key | Type | Default | What it does |
|---|---|---|---|
| `strip_words` | list of strings | `[]` | Words removed before names are compared, such as legal forms. |
| `fuzzy_score` | number | `0.92` | Least Jaro-Winkler similarity for a proposed fuzzy match. |
| `embed_score` | number | `0.9` | Least cosine similarity for a proposed embedding match. |
| `max_candidates` | integer | `5` | Nearest entities checked per entity for embedding matches. |
| `max_block` | integer | `1000` | Blocks of names larger than this are not compared fuzzily. |
| `block_chars` | integer | `4` | Names are compared fuzzily only when their first word starts with the same characters. |
| `classes` | list of class names | `[]` | Only entities of these classes and their subclasses; empty means all except document classes. |

### [[sources]]

One entry per folder or table to index.

| Key | Type | Default | What it does |
|---|---|---|---|
| `name` | identifier | required | Source name, unique. |
| `kind` | `"folder"` or `"table"` | `"folder"` | Folder on disk or a database view. |
| `root` | path | required | Folder to walk, relative to the config directory or absolute. |
| `view` | `schema.view` | `""` | View to read for `kind = "table"`. |
| `dataset` | string | `""` | Label stored on documents and entities of this source. |
| `ignore` | list of globs | `[]` | Paths of this source never read. |

## rules.toml

Rules are the cheapest way to find facts. They run in the `rules` stage of `scan`, except
`[[revisions]]`, which `resolve` uses. Class and property names are prefixed names or IRIs of the
ontology.

### [doc_types]

A table from a document type name to a class, for example `invoice = "acme:Invoice"`. Path rules,
page types and `classify` assign these names.

### [[path_rules]]

| Key | Type | Default | What it does |
|---|---|---|---|
| `pattern` | regex | required | Matched against the path relative to the source root; named groups can be used below. |
| `doc_type` | string | `""` | Document type of a matching document. |
| `props` | table | `{}` | Properties stored on the document, with `{group}` replaced. |
| `entities` | list | `[]` | Entities found in the path, see below. |
| `relations` | list | `[]` | Relations between them, see below. |

`[[path_rules.entities]]`: `id` (name used in relations), `class`, `key` and `name` (with
`{group}` replaced; `name` defaults to the key) and `key_case` (`keep`, `upper` or `lower`).

`[[path_rules.relations]]`: `from` and `to` (an entity `id`, or `document` for the document
itself) and `property`.

### [[id_patterns]]

| Key | Type | Default | What it does |
|---|---|---|---|
| `class` | class name | required | Class of the entity. |
| `pattern` | regex | required | The key is the named group `key`, or the whole match. |
| `key_case` | `keep`, `upper` or `lower` | `keep` | Case of the key. |
| `on` | `path`, `text` or `both` | `both` | Where the pattern looks. |

`resolve` also runs these patterns over entity names, so an entity named "Project P-1234" joins
the entity with key `P-1234`.

### [ocr_confusions]

A table from a character to the characters OCR mistakes it for, for example `"0" = "Oo"`. ID
patterns run once more over OCR text with these replaced; a match counts only when its key is
already known. Defaults: `0` for `Oo`, `1` for `Il|`, `5` for `S`, `8` for `B`.

### [[page_types]] and [pages]

`[[page_types]]` splits one file into parts by page: `doc_type`, `pattern` (a regex over the head
of each page) and `starts` (a match starts a new part even when the type stays the same).

| Key | Type | Default | What it does |
|---|---|---|---|
| `pages.head_chars` | integer | `400` | Characters at the top of a page matched by page types. |
| `pages.part_property` | property name | `""` | Property that links a part to its file; needed when page types are set. |

### [[revisions]]

Revision rules link a later revision of a document to the one before it with
`dcterms:replaces`. Without them there are no such links.

| Key | Type | Default | What it does |
|---|---|---|---|
| `pattern` | regex | required | Matched against the relative path; needs the named groups `series` and `rev`. |
| `order` | `"natural"` | `"natural"` | How revisions are ordered: numbers by value, then letters, so `2` comes before `10` and `B1` before `B2`. |

Documents of one source with the same `series` form a chain, and each revision replaces the one
before it. Example:

```toml
[[revisions]]
pattern = '(?P<series>DWG-\d{3}-[A-Z]\d) rev (?P<rev>[A-Z0-9]+)\.'
order = "natural"
```

## Gazetteer

Every `gazetteer/*.csv` file in the config directory has the header
`class,key,name,aliases,lang`:

| Column | What it holds |
|---|---|
| `class` | Class name of the entity. |
| `key` | Stable key, unique within the class. |
| `name` | Name shown. |
| `aliases` | Other spellings, separated by `\|`. |
| `lang` | Language of the aliases, may be empty. |

The name and aliases are found in text and paths, case-insensitively and as whole words.

## Prompts

A file `prompts/<name>.md` in the config directory replaces the built-in prompt of that name.
Variables are written `{name}` and filled in before sending.

| Prompt | Variables |
|---|---|
| `fit-system` | none |
| `fit-user` | `{chunk}`, `{languages}`, `{pages}`, `{path}`, `{shortlist}` |
| `extract-system` | none |
| `extract-user` | `{chunk}`, `{known}`, `{languages}`, `{pages}`, `{path}`, `{properties}`, `{shortlist}` |
| `classify-system` | none |
| `classify-user` | `{doc_types}`, `{pages}`, `{path}`, `{text}` |

## questions.toml

Optional. `zontologize eval embed` scores each embedding model on these questions as well:

```toml
[[questions]]
text = "Which drawing shows the pipe section?"
expect = ["Projects/2021/P-2021-0042*/04 Reports/*"]
```

`expect` lists paths relative to a source root; globs are allowed. A question counts as found at
10 when a chunk of an expected document is among the 10 nearest chunks. A pattern that matches no
present document is an error that names the question.
