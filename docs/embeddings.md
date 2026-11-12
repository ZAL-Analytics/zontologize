# Embeddings

`zontologize embed` turns chunks, entities and classes into vectors in pgvector. Models run
locally on the CPU, or on the GPU (see `[gpu]` in [configuration.md](configuration.md)), through
ONNX Runtime; files are downloaded once from Hugging Face. No API key.

Reruns embed only changed texts. `embed --minutes N` stops after N minutes and the next run goes
on, so a large first load can be spread over several nights. `embed --model local:<name>` adds a
second model next to the current one; `embed --prune` removes all but `[models] embed`.

## Built-in models

| Name | Dimensions | Pooling | Texts/s on 8 cores |
|---|---|---|---|
| `multilingual-e5-small` | 384 | mean | ~16 |
| `multilingual-e5-base` | 768 | mean | ~6.6 |
| `arctic-embed-m-v2.0` (default) | 768 | cls | ~4.4 |
| `bge-m3` | 1024 | cls | ~2 |
| `qwen3-embedding-0.6b` | 1024 | last | ~1 |

All are multilingual and capped at 512 tokens by default. Speed is for 512-token texts.

## Choosing a model

```sh
zontologize eval embed --models local:multilingual-e5-base,local:arctic-embed-m-v2.0
```

`eval/embed-report.md` shows speed and recall per model on a sample of your data (plus
`questions.toml` if present). Pick one, set `[models] embed`, run `zontologize embed`.

## Another Hugging Face model

Any repository with an ONNX export (`float32` output, dense vectors):

```toml
[models]
embed = "local:my-model"

[embed.models."my-model"]
repo = "owner/name"
revision = "<commit>"            # empty follows main
file = "onnx/model.onnx"
data_files = []                  # external weight files, such as onnx/model.onnx_data
tokenizer = "tokenizer.json"
pooling = "mean"                 # mean, cls, last or none (see the model card)
output = ""                      # empty: first output
normalize = true
passage_prefix = ""
query_prefix = ""                # for example "query: "
max_tokens = 512
```

## Querying from an app

`embedding_models` records the model files and settings, so an app can embed a question the same
way. Query with the cast and conditions of the model's partial index, or PostgreSQL scans the
whole table:

```sql
select k.id, k.text
from zontologize.embeddings e
join zontologize.chunks k on k.id = e.owner_id
where e.model_id = 1 and e.owner_kind = 'chunk'
order by e.embedding::halfvec(768) <=> $1::halfvec(768)
limit 10;
```

Use `owner_kind = 'entity'` for entities and `class_embeddings` for classes. Combine with the
`tsv` full-text column of `chunks` for exact words and IDs.
