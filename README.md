# mqlc

Schema-aware, verifiable NL → MongoDB query compiler, evaluated on
[AptMQL-Bench](https://huggingface.co/datasets/MongoDB/AptMQL-Bench).

Workspace:

- `mqlc-core`: all logic (dataset access, schema profiler, prompt assembly, LLM backends, reply parsing, execution)
- `mqlc`: CLI
- `mqlc-mcp`: MCP server (not implemented yet)

## Scores

Claude Haiku 4.5, the official prompt and schema description, temperature 0,
no thinking, no evidence. Dev is the reported score (ADR 0006); all 3,186
compares with the paper.

| Run | Split | n | Exec-Rate | Soft-EX | Strict-EX | Cost (USD) |
|---|---|--:|--:|--:|--:|--:|
| Paper (Haiku 4.5, no knowledge) | all | 3,186 | 98.34 | 47.71 | 23.79 | |
| [baseline](runs/baseline-all/manifest.json) | **dev** | 1,424 | 97.75 | **44.80** | 18.68 | |
| [baseline](runs/baseline-all/manifest.json) | train | 1,762 | 96.99 | 39.61 | 17.88 | |
| [baseline](runs/baseline-all/manifest.json) | all | 3,186 | 97.33 | 41.93 | 18.24 | 4.08 (batch) |
| [thinking](runs/thinking-all/manifest.json) (`--temperature 1 --thinking-budget 2048`) | dev | 1,424 | 99.30 | 45.72 | 22.19 | |
| [thinking](runs/thinking-all/manifest.json) | all | 3,186 | 98.31 | 42.91 | 20.21 | 10.18 (batch) |

Per-database scores are in each run's [`summary.json`](runs/baseline-all/summary.json).

The baseline is 5.8 points of Soft-EX under the paper, beyond the ±3 we
aimed for, and we could not find why (#5, #19). Strict-EX is as far under
(5.5) and Exec-Rate 1 point, so the model answers fewer questions right
rather than the scorer counting differently: the scorer gives the gold
pipelines [99.47%](runs/gold-as-pred/summary.json) Soft-EX against
themselves, 5 replies fail to parse and 4 queries time out. The dataset has
not changed since it was published, and `claude-haiku-4-5` has one snapshot
(`20251001`). The paper does not give its sampling settings, message layout
or how it joined the schema files; train runs test each (paired difference
from the baseline's train half, 95% interval):

| Run | Split | Soft-EX | vs baseline | Cost (USD) |
|---|---|--:|--:|--:|
| [`--arm official-prompt-keys`](runs/prompt-keys-train/manifest.json) | train | 39.33 | −0.28 ± 1.19 | 2.55 |
| [`--temperature 1`](runs/temp1-train/manifest.json) | train | 38.31 | −1.31 ± 1.06 | 2.71 |
| [`--arm official-tagged`](runs/tagged-train/manifest.json) | train | 39.33 | −0.28 ± 1.10 | 2.70 |
| [`--single-message`](runs/single-message-train/manifest.json) | train | 39.27 | −0.34 ± 0.96 | 2.70 |
| [`--temperature 1 --thinking-budget 2048`](runs/thinking-train/manifest.json) | train | 40.64 | +1.02 ± 1.66 | 6.08 |
| [sync instead of batch](runs/sync-train200/manifest.json) | first 200 of train | 35.00 | −0.50 ± 2.20 | 1.12 |

The tagged layout and the single message rule themselves out. Thinking does
not: against the same temperature without it, it gains 2.33 ± 1.68 on train,
and on all 3,186 it brings Exec-Rate to the paper's (98.31 against 98.34) and
gains 0.97 ± 1.23 Soft-EX and 1.98 ± 1.21 Strict-EX over the baseline. That
still leaves Soft-EX 4.8 points under the paper, beyond ±3. The baseline
stays temperature 0 without thinking, the paper's stated setting being
unknown either way. The sync run's 200 questions are all from works_cycles,
so it only shows that batch changes nothing there (5 of 200 changed). Rescoring the baseline's records on
`mongo:8.0` instead of 8.2 gives [42.12](runs/baseline-all-mongo80/summary.json)
on all, 44.80 on dev and 39.95 on train: 16 questions change Soft-EX (6 of
them on dev, cancelling out).

Later experiments compare with this baseline, not the paper. The baseline ran
at f8e38ab (its manifest says `-dirty` for an edit outside the code); the
train runs need the options added with them. The reproduction cost USD 9.34
(#5) and the investigation USD 16.89 (#19).

### Experiment A scores

Where the schema knowledge comes from ([arms](#experiment-a), ADR 0012),
on dev, each arm against the official one on the same model (paired
difference in Soft-EX, exact McNemar p over the questions only one got
right). Temperature 0, no thinking, batch; the auto MDL is the profile at
c094437 (#11).

| Model | `--arm` | Exec-Rate | Soft-EX | vs official | p | Strict-EX | Cost (USD) |
|---|---|--:|--:|--:|--:|--:|--:|
| Haiku 4.5 | [official](runs/baseline-all/manifest.json) (the baseline's dev half) | 97.75 | 44.80 | | | 18.68 | 1.37 |
| Haiku 4.5 | [auto-mdl](runs/expA-dev-auto-mdl/manifest.json) | 97.68 | 47.54 | +2.74 | 0.018 | 20.65 | 1.69 |
| Haiku 4.5 | [official-auto-mdl](runs/expA-dev-official-auto-mdl/manifest.json) | 97.61 | **49.65** | +4.85 | <0.001 | 20.29 | 1.69 |
| Haiku 4.5 | [official-evidence](runs/expA-dev-official-evidence/manifest.json) (upper bound) | 97.47 | 61.24 | +16.44 | <0.001 | 25.91 | 1.37 |
| Sonnet 4.5 | [official](runs/sonnet-dev-official/manifest.json) | 98.53 | 50.07 | | | 27.39 | 3.91 |
| Sonnet 4.5 | [official-auto-mdl](runs/sonnet-dev-official-auto-mdl/manifest.json) | 99.30 | **54.56** | +4.49 | <0.001 | 31.46 | 4.80 |

The auto MDL in place of the official description is no worse (+2.74), and
merged into it gains clearly (+4.85), more than in place of it (+2.11,
p = 0.022). The gain is uneven across databases: merged, it gains 24 points
on debit_card_specializing and 16 on student_club but loses 7 on formula_1.
Evidence, written by hand for each question, is still 11.6 points above it.
Sonnet 4.5 gains about as much from the merged auto MDL as Haiku. The new
runs cost USD 13.46 (#11).

## Setup

Requirements: Rust (stable), Docker Engine with the compose plugin, and
[uv](https://docs.astral.sh/uv/) (downloads the dataset and runs the scorer).

### 1. Dataset and MongoDB

```bash
scripts/restore.sh
```

This downloads AptMQL-Bench (~2.6 GB, pinned to one dataset revision) into `data/AptMQL-Bench` if it is not
there yet, starts `mongo:8.2` from `compose.yaml` (data lives in the
`mongo-data` named volume), and restores all 21 databases by running the
dataset's own `restore.sh` inside the container, so no MongoDB tools are needed
on the host. It is safe to re-run.

On first start the container creates two users:

| User | Password | Role | Used by |
|---|---|---|---|
| `root` | `root` | root | restore only |
| `mqlc` | `mqlc` | `readAnyDatabase` | `mqlc` (default `MQLC_MONGO_URI`) |

MongoDB listens on `127.0.0.1:27017` only. These credentials are for local
development; do not expose this container.

### 2. Anthropic API key

Put it in a `.env` file at the repository root (gitignored), which `mqlc`
reads at startup:

```bash
echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env
chmod 600 .env
```

A variable already set in the environment wins over `.env`. Never commit the
key.

## Usage

Answer one benchmark question with the official prompt and schema
description (Claude Haiku 4.5, temperature 0, no thinking):

```bash
cargo run -p mqlc -- ask dev_1471
```

It prints the starting collection, the generated aggregation pipeline
(relaxed Extended JSON) and the result rows. Pipelines with `$out` or `$merge`
are refused before they reach the database.

| Option | Env | Default |
|---|---|---|
| `--data-dir` | `MQLC_DATA_DIR` | `data/AptMQL-Bench` |
| `--mongo-uri` | `MQLC_MONGO_URI` | `mongodb://mqlc:mqlc@localhost:27017/?authSource=admin` |
| `--model` | `MQLC_MODEL` | `claude-haiku-4-5` |
| `--arm` | | `official` (experiment A: `auto-mdl`, `official-auto-mdl`, `official-evidence`, see [Experiment A](#experiment-a); diagnostics: `official-prompt-keys`, `official-tagged`, below) |
| `--temperature` | | `0` |
| `--mdl-dir` | `MQLC_MDL_DIR` | `mdl/auto` |
| `--human-mdl-dir` | `MQLC_HUMAN_MDL_DIR` | `mdl/human` |
| `--max-rows` | | `20` |

`--arm official-prompt-keys` sends the same official description with each
field line rewritten to the keys the official prompt names (`field_name`,
`field_description`, `data_format`, `value_description`): the dataset's files
call `data_format` `data_type` and add `required`. `--arm official-tagged`
wraps each collection's lines in `<collection name="...">` instead of putting
them under `Collection: <name>`. `--single-message` sends the system prompt
at the start of the user turn instead of as the system prompt.
`--thinking-budget N` turns on extended thinking with N tokens on top of the
reply's 4,096 (Anthropic only, with `--temperature 1`, which the API
requires). All are guesses at how the paper ran (#5, #19); the manifest
records each.

### Other backends

`--backend openai` talks to any OpenAI-compatible `/chat/completions`
endpoint (local Ollama, vLLM, OpenRouter) at `--base-url` (default
`http://localhost:11434/v1`, Ollama), with an optional `OPENAI_API_KEY`.
It needs `--context-window`, the tokens the served model accepts per prompt,
which must match the server: Ollama silently truncates prompts past its
`OLLAMA_CONTEXT_LENGTH`. All three settings can go in `.env` as
`MQLC_BACKEND`, `MQLC_BASE_URL` and `MQLC_CONTEXT_WINDOW`.

```bash
cargo run --release -p mqlc -- --backend openai --model qwen2.5-coder:3b \
  --context-window 8192 bench --out runs/ollama-restaurant --split train --db restaurant --limit 5
```

`--model` is required with it. Before sending, mqlc estimates the prompt at 3
bytes a token (an overestimate for English); a prompt that leaves no room for
the longest reply (4,096 tokens) in the context window is not sent and is
recorded as `skipped_context_overflow` (`mqlc ask` exits 3). The scorer leaves
those out of every share and reports their count. Each manifest invocation
records the endpoint. Anthropic models declare 200K tokens. The OpenAI backend
has no batch mode and no prices, so its calls count as unpriced and do not
add to the spend limit.

### Benchmark runs

```bash
cargo run --release -p mqlc -- bench --out runs/haiku-dev --split dev
uv run --project scoring scoring/score.py runs/haiku-dev
```

`mqlc bench` answers every question of a split (`train`, `dev` or `all`; add
`--db <name>` for one database and `--limit N` for the first N) and appends
one record per answer to `runs/<name>/results.jsonl`. Records are keyed by
(arm, model, sample id, candidate): rerunning the same command skips answered
questions, so an interrupted run (killed, or stopped by an API error such as a
usage limit, exit code 2) is continued by running it again. A half-written
last line from a kill is discarded and that question is asked again.

`--mode batch` sends every unanswered question as one
[Message Batch](https://platform.claude.com/docs/en/build-with-claude/batch-processing)
at half price (cache multipliers included) and checks it every 60 seconds
until it ends, usually within an hour. The batch id is kept in
`runs/<name>/batches.json`, so the command can be interrupted at any time:
rerunning it collects the same batch instead of submitting again. Requests
the batch did not answer (errored, expired, canceled) are not recorded; the
run stops (exit code 2) and a rerun submits them again. While a batch is
pending, a sync run of the same directory is refused, so no question is paid
for twice. Batch records have `"batch": true` and no latency.

Rate limits (429) and overloads (529), on calls and on batch submits and
checks, are retried up to three times, waiting for `retry-after` (5, 10 and
20 s without one, at most 60 s); any other API error, such as a credit
balance that is too low, stops the run at once. `--spend-limit` (default USD
80) stops the run before the next call or batch once every run directory
next to this one (all of `runs/` here) has together cost that much; review
the spend, then rerun with a higher limit to continue. A batch can overshoot
the limit by its own cost, since its cost is known only when it ends.

`runs/<name>/manifest.json` fixes the run's configuration (arm, model,
temperature, thinking, knowledge mode, IR, prompt hash) at the first
invocation and lists every invocation with the mqlc version it ran. Resuming
with a different configuration is refused. Each record has the token usage
(input, output, cache read, cache write), latency and cost; the CLI prints the
running cost. The system prompt and the database's schema block are sent with
prompt caching, and questions are ordered by database so they share the cache.

`scoring/score.py` executes the gold and predicted pipelines and compares the
results with the official `eval_script.py`, writing `scores.jsonl` and
`summary.json` into the run directory:

- **Soft-EX**: `compare_fuzzy` with its defaults, the paper's proposed metric
  (columns matched by value, extra predicted columns allowed)
- **Strict-EX**: the same without extra columns
- **Exec-Rate**: predictions that parsed and executed without error

`summary.json` has the three shares overall, per split (`by_split`; dev is
the reported score, ADR 0006) and per database (`by_db`), and the run's
`cost_usd`.

To compare two scored runs, such as two arms on the same questions:

```bash
uv run --project scoring scoring/compare.py runs/<a> runs/<b>
```

It pairs the questions both runs answered, leaving out any that either run
skipped. For each metric it prints both shares, the number of questions only
A or only B got right, and the exact McNemar p-value over those
disagreements. It also prints the cost of the paired questions.

### Auto MDL

```bash
cargo run --release -p mqlc -- profile
cargo run --release -p mqlc -- --arm auto-mdl bench --out runs/auto-mdl-dev --split dev
```

`mqlc profile` reads up to the first 100,000 documents of every collection
in `_id` order (`--sample-size`), so the same data always gives the same
profile, and writes one
[Wren MDL](https://github.com/Canner/wren-engine) project per database to
`mdl/auto/<db_id>/` (`--mdl-dir`, gitignored): `mdl.json` with one model per
collection and one column per field path, and `instructions.json`. Field
types, presence, arrays, embedded documents and the array a field sits in are
column properties in the `mqlc.` namespace (ADR 0004), so `mdl.json` stays
valid Wren MDL. So are values: a field with at most 20 distinct values lists
them all, null included; one with more gets its most common repeated values,
min and max for numbers and dates, and a regex format most of its strings
follow. Strings with leading or trailing whitespace are counted. A field
whose values are `_id`s of another collection becomes an MDL relationship:
a key named for the collection (`accountId` for `accounts`, or its first
letters, `profId` for `professors`) when more than half of its values are,
any ObjectId when nine in ten are. The prompt says how many matched when
not all did. `--split` and `--db` choose the databases; all 21 take about
a minute, and rerunning overwrites them. On AptMQL-Bench, 100,000 documents
find every field a full scan finds.

```bash
cargo run --release -p mqlc -- profile --exclude financial.clients.birthDate
```

`--exclude <db>.<collection>.<path>` (repeatable) leaves a sensitive field and
everything under it out: MongoDB never returns its values, and the MDL keeps
only its path (`mqlc.excluded`), so neither its values nor the field reach
the prompt. Excluding a field that is not there is an error.

Rendered, the auto MDL of a database takes about 60% of the tokens of its
official schema description (126k against 211k over all 21; works_cycles,
the largest, 30k).

`--arm auto-mdl` (for `ask` and `bench`) puts the auto MDL in the prompt in
place of the official schema description, in the same layout (`Collection:
<name>`, then one JSON line per field), followed by the instructions. The
run's records carry the arm, and the manifest records a hash of each
database's rendered MDL: a rerun may add databases (a smoke run grown into a
full split) but is refused once a database's MDL has changed. A database with no MDL
project stops the run unrecorded.

### Experiment A

Experiment A compares where the schema knowledge comes from (ADR 0012), on
dev:

| `--arm` | Knowledge in the prompt |
|---|---|
| `official` | the official schema description (the paper's baseline) |
| `auto-mdl` | the auto MDL in its place |
| `official-auto-mdl` | the official description with the auto MDL merged into each field line |
| `official-evidence` | the official description and each question's `mongodb_evidence` (the upper bound) |

`official-auto-mdl` keeps each official field line as written and appends
what the auto MDL says of the same path (`in_array`, `present`,
`references`, values, `untrimmed`), with the observed type as
`observed_type` where it differs from the official `data_type`; fields only
the profile saw follow the official ones. The 343 questions without `mongodb_evidence` (140 on dev) get
the official prompt unchanged under `official-evidence` and are scored like
the rest, so all four arms answer the same questions.

`--arm human-mdl` and `--arm official-human-mdl` do the same as the two MDL
arms with a human MDL from `--human-mdl-dir` (default `mdl/human/<db_id>/`):
an auto MDL a person added business definitions (`properties.description` on
a model or column), formulas (a column with `"isCalculated": true` and an
aggregation `expression`) and instructions to. A human MDL may only be
written on train (ADR 0006), so these arms refuse any question on a dev
database before sending anything. `mdl/human/cs_semester` is one, written from the
official description and queries on the data only, without reading any of
the database's questions, evidence or gold pipelines. Every MDL file is read strictly: a key
mqlc would not send, such as a misspelt one, is an error rather than
silently left out. Each manifest invocation records the MDL directory
(`knowledge_dir`). Every arm's manifest also has a per-database hash of the
knowledge it sent (the official description, the MDL, the merged text, and
for `official-evidence` each question's evidence too), so a run refuses to
resume once its dataset or MDL has changed.

### IR coverage

```bash
cargo run -p mqlc -- ir-coverage
```

mqlc parses each pipeline into a typed IR (ADR 0003); a stage it cannot
represent is kept as written, as a raw stage. `ir-coverage` reports, per
split, how many gold pipelines need no raw stage, and the most common
constructs the rest need. The supported operators are exactly those the train
split uses, so train is fully covered:

| split | samples | covered | coverage |
|-------|--------:|--------:|---------:|
| train | 1,762 | 1,762 | 100.0% |
| dev | 1,424 | 1,376 | 96.6% |

The dev gaps are led by `$substr` (16 samples) and `$objectToArray` (14).

## Tests

```bash
scripts/ci.sh                          # what CI runs: fmt, clippy, cargo test, the scorer test
git config core.hooksPath .githooks    # once: fmt and clippy on every commit
```

`scripts/ci.sh rust` and `scripts/ci.sh scoring` run one CI job each; on its
own, `cargo test` is:

```bash
cargo test
```

Integration tests start a throwaway `mongo:8.2` through testcontainers, so
Docker must be running. The LLM is replaced by a scripted fake; nothing else
is mocked. The IR tests need no database but read every gold pipeline, so
they need at least `scripts/fetch-data.sh aptmqlbench_data.jsonl
prompts/s51_text2mql_1time_completion.txt`. The scorer has its own test:

```bash
cd scoring && uv run pytest
```

## Data attribution

AptMQL-Bench © MongoDB, Inc., licensed under CC BY-SA 4.0. It is downloaded
at setup time, not redistributed here.
