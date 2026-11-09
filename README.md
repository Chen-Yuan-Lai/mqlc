# mqlc

Schema-aware, verifiable NL → MongoDB query compiler, evaluated on
[AptMQL-Bench](https://huggingface.co/datasets/MongoDB/AptMQL-Bench).

Workspace:

- `mqlc-core`: all logic (dataset access, prompt assembly, LLM backends, reply parsing, execution)
- `mqlc`: CLI
- `mqlc-mcp`: MCP server (not implemented yet)

## Setup

Requirements: Rust (stable), Docker Engine with the compose plugin, and
[uv](https://docs.astral.sh/uv/) (used to download the dataset).

### 1. Dataset and MongoDB

```bash
scripts/restore.sh
```

This downloads AptMQL-Bench (~2.6 GB) into `data/AptMQL-Bench` if it is not
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

## Tests

```bash
cargo test
```

Integration tests start a throwaway `mongo:8.2` through testcontainers, so
Docker must be running. The LLM is replaced by a scripted fake; nothing else
is mocked.

## Data attribution

AptMQL-Bench © MongoDB, Inc., licensed under CC BY-SA 4.0. It is downloaded
at setup time, not redistributed here.
