# mqlc

Schema-aware, verifiable NL → MongoDB query compiler, evaluated on
[AptMQL-Bench](https://huggingface.co/datasets/MongoDB/AptMQL-Bench).

Workspace:

- `mqlc-core`: all logic (dataset access, schema profiler, prompt assembly, LLM backends, reply parsing, execution)
- `mqlc`: CLI
- `mqlc-mcp`: MCP server (not implemented yet)

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
| `--arm` | | `official` (or `auto-mdl`, see [Auto MDL](#auto-mdl)) |
| `--mdl-dir` | `MQLC_MDL_DIR` | `mdl/auto` |
| `--max-rows` | | `20` |

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
them all; one with more gets its most common repeated values, min and max for
numbers and dates, and a regex format most of its strings follow. A field
whose values are `_id`s of another collection (any ObjectId, or a key named
for the collection, such as `accountId` for `accounts`) becomes an MDL
relationship. `--split` and `--db` choose the databases; all 21 take about
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
