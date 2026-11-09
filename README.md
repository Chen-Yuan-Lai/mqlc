# mqlc

Schema-aware, verifiable NL → MongoDB query compiler, evaluated on
[AptMQL-Bench](https://huggingface.co/datasets/MongoDB/AptMQL-Bench).

Workspace:

- `mqlc-core`: all logic (dataset access, prompt assembly, LLM backends, reply parsing, execution)
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

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

Put it in your shell profile, never in the repo.

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
| `--max-rows` | | `20` |

### Benchmark runs

```bash
cargo run --release -p mqlc -- bench --out runs/haiku-dev --split dev
uv run --project scoring scoring/score.py runs/haiku-dev
```

`mqlc bench` answers every question of a split (`train`, `dev` or `all`; add
`--limit N` for the first N) and appends one record per answer to
`runs/<name>/results.jsonl`. Records are keyed by (arm, model, sample id,
candidate): rerunning the same command skips answered questions, so an
interrupted run (killed, or stopped by an API error such as a usage limit,
exit code 2) is continued by running it again. A half-written last line from a
kill is discarded and that question is asked again.

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

## Tests

```bash
cargo test
```

Integration tests start a throwaway `mongo:8.2` through testcontainers, so
Docker must be running. The LLM is replaced by a scripted fake; nothing else
is mocked. The scorer has its own test:

```bash
cd scoring && uv run pytest
```

## Data attribution

AptMQL-Bench © MongoDB, Inc., licensed under CC BY-SA 4.0. It is downloaded
at setup time, not redistributed here.
