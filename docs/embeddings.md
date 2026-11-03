# Embeddings

`zontologize embed` turns chunks, entities and classes into vectors and stores them in pgvector,
so an app can find text by meaning instead of by exact words. The model runs inside zontologize
on the CPU, through ONNX Runtime. There is no API key, no subscription and no network once the
model files are downloaded.

## How it works

- **What is embedded.**
  - Chunks: every chunk of a present document that is at least `[embed] min_chars` long, has a
    letter share of at least `min_letter_share` and lies on pages with OCR confidence of at least
    `min_ocr_confidence`. The text is the chunk text with the model's `passage_prefix`.
  - Entities: name, class label and up to `max_aliases` aliases, for example
    `Acme Pumps (customer); Acme`.
  - Classes: preferred and alternative labels in every ontology language and the definition, for
    every selected class.
- **Only what changed.** Each text gets a key from the model record, the prefix, the token limit
  and the text. A rerun embeds only texts whose key changed, copies the vector of a chunk whose
  text another chunk already has, and removes vectors of chunks and entities that are gone.
- **Restartable.** Vectors are written per batch of `[embed] batch_size` texts. `embed --minutes N`
  stops handing out batches after N minutes; the next run goes on where it stopped, so a first load
  can be spread over several nights.
- **Several models.** `embed --model local:<name>` embeds with another model next to the current
  one; `embed --prune` removes every model but `[models] embed`.
- **Storage.** One table `embeddings` holds chunk and entity vectors of every model as `halfvec`
  (half precision), and `class_embeddings` holds class vectors. Vectors are normalised to length
  1, so cosine distance and inner product rank the same. After the load, `embed` builds an HNSW
  index per model and owner kind (`--no-index` skips it).

## Built-in models

The defaults describe five multilingual models, each pinned to a commit on Hugging Face.

| Name | Repository | Dimensions | Token limit | Pooling | Prefixes |
|---|---|---|---|---|---|
| `multilingual-e5-base` (default) | `intfloat/multilingual-e5-base` | 768 | 512 | mean | `passage: `, `query: ` |
| `multilingual-e5-small` | `intfloat/multilingual-e5-small` | 384 | 512 | mean | `passage: `, `query: ` |
| `bge-m3` | `BAAI/bge-m3` | 1024 | 512 (model: 8192) | cls | none |
| `arctic-embed-m-v2.0` | `Snowflake/snowflake-arctic-embed-m-v2.0` | 768 | 512 (model: 8192) | cls | query: `query: ` |
| `qwen3-embedding-0.6b` | `onnx-community/Qwen3-Embedding-0.6B-ONNX` | 1024 | 512 (model: 32768) | last | query: an instruction |

All five handle Dutch, English, German and about 100 other languages. Speed and recall measured
on the benchmark folder are in [Measured](#measured).

## Choosing a model

1. Put candidates in `--models` and run
   `zontologize eval embed --models local:multilingual-e5-base,local:bge-m3`.
2. Read `eval/embed-report.md`. Per model it shows:
   - chunks per second on this machine;
   - the share of texts cut at the token limit;
   - bytes per vector;
   - recall at 1 and 10 and mean reciprocal rank for three kinds of query:
     - chunk to chunk: chunks that mention the same entity found by rules, in other documents;
     - class to chunk: a class's labels against the chunks with a mention of that class;
     - questions from `questions.toml`, scored against the documents expected to answer them.
3. Pick the best recall that still embeds your archive in the time you have. The rate times the
   number of eligible chunks (`zontologize status` shows it after a run) gives the time of a full
   load.
4. Set `[models] embed` and run `zontologize embed`. The vectors that `eval embed` made for its
   sample are kept and reused.

## Another Hugging Face model

Any repository with an ONNX export can be described in `zontologize.toml`:

```toml
[models]
embed = "local:my-model"

[embed.models."my-model"]
repo = "owner/name"
revision = "<commit>"
file = "onnx/model.onnx"
data_files = []
tokenizer = "tokenizer.json"
pooling = "mean"
output = ""
normalize = true
passage_prefix = ""
query_prefix = ""
max_tokens = 512
```

- **file and data_files.** Look at the repository's file list. The ONNX file is often
  `onnx/model.onnx`. Large models keep their weights in extra files next to it, such as
  `onnx/model.onnx_data`; list every one of them in `data_files`.
- **tokenizer.** The `tokenizer.json` of the repository, sometimes under `onnx/`.
- **pooling.** The model card or `1_Pooling/config.json` tells it: `mean` for mean pooling,
  `cls` for the first token (CLS), `last` for the last token (most decoder models). Use `none`
  when the chosen `output` is already one vector per text (shape `[batch, dimensions]`).
- **prefixes.** Many models expect a prefix such as `query: ` or an instruction for questions,
  and sometimes another one for passages. The model card says which.
- **revision.** Pin a commit so the vectors stay reproducible. An empty revision follows `main`
  and records the commit it downloaded.

Inputs are filled by name: `input_ids` and `attention_mask` always, `token_type_ids` (zeros) and
`position_ids` when the model has them, and an empty key-value cache for decoder exports with
`past_key_values.*` inputs. A model that needs any other input stops at load with the names of its
inputs. Models that cannot be used:

- repositories without an ONNX export (convert them first, for example with Hugging Face Optimum);
- models whose ONNX output is not `float32`;
- sparse and multi-vector outputs (bge-m3's sparse and ColBERT vectors): only dense vectors are
  stored.

## Using the vectors in an app

`embedding_models` records exactly which files made the vectors:

```sql
select id, name, repo, revision, file, data_files, tokenizer, sha256, dimensions, pooling,
       output, passage_prefix, query_prefix, max_tokens, normalize
from zontologize.embedding_models where current;
```

An app embeds a question the same way, here in Python with `onnxruntime`, `tokenizers`,
`huggingface_hub` and `numpy`:

```python
import numpy as np, onnxruntime as ort
from huggingface_hub import hf_hub_download
from tokenizers import Tokenizer

def load(m):
    for extra in m["data_files"]:
        hf_hub_download(m["repo"], extra, revision=m["revision"])
    model = hf_hub_download(m["repo"], m["file"], revision=m["revision"])
    tokenizer = Tokenizer.from_file(hf_hub_download(m["repo"], m["tokenizer"], revision=m["revision"]))
    tokenizer.enable_truncation(m["max_tokens"])
    tokenizer.no_padding()
    return ort.InferenceSession(model), tokenizer

def embed_question(m, session, tokenizer, question):
    enc = tokenizer.encode(m["query_prefix"] + question)
    ids = np.array([enc.ids], dtype=np.int64)
    mask = np.array([enc.attention_mask], dtype=np.int64)
    feed = {}
    for i in session.get_inputs():
        if i.name == "input_ids": feed[i.name] = ids
        elif i.name == "attention_mask": feed[i.name] = mask
        elif i.name == "token_type_ids": feed[i.name] = np.zeros_like(ids)
        elif i.name == "position_ids": feed[i.name] = np.arange(ids.shape[1])[None, :]
        elif i.name.startswith("past_key_values"):
            feed[i.name] = np.zeros((1, i.shape[1], 0, i.shape[3]), dtype=np.float32)
    names = [o.name for o in session.get_outputs()]
    out = session.run([m["output"] or names[0]], feed)[0]
    if m["pooling"] == "mean": v = (out[0] * mask[0][:, None]).sum(0) / mask.sum()
    elif m["pooling"] == "cls": v = out[0, 0]
    elif m["pooling"] == "last": v = out[0, mask.sum() - 1]
    else: v = out[0]
    return v / np.linalg.norm(v) if m["normalize"] else v
```

Then query the partial index of that model. The cast to `halfvec(<dimensions>)` and the
`model_id` and `owner_kind` conditions must match the index, or PostgreSQL scans the whole table:

```sql
select k.id, k.text, e.embedding::halfvec(768) <=> $1::halfvec(768) as distance
from zontologize.embeddings e
join zontologize.chunks k on k.id = e.owner_id
where e.model_id = 1 and e.owner_kind = 'chunk'
order by e.embedding::halfvec(768) <=> $1::halfvec(768)
limit 10;
```

Entities work the same way with `owner_kind = 'entity'`. Classes are few: rank
`class_embeddings` of the model directly. Combine vector search with the `tsv` full-text column
of `chunks` when exact words, numbers or IDs matter.

## Measured

Not measured yet.
