"""Score an `mqlc bench` run with the official AptMQL-Bench eval_script.py.

Runs both the gold pipeline and the predicted one against MongoDB, then compares
the results with the official `compare_fuzzy`:

- Soft-EX: `compare_fuzzy` with its defaults (the metric the paper proposes);
  the prediction may return extra fields.
- Strict-EX: the same comparison without extra fields (`allow_column_subset=False`).
- Exec-Rate: share of predictions that parsed and executed without error.

Writes `scores.jsonl` (one line per record) and `summary.json` into the run
directory, and prints the summary.

    uv run --project scoring scoring/score.py runs/<name>
"""

import argparse
import importlib.util
import json
import sys
from collections import defaultdict
from pathlib import Path

from bson import json_util

DEFAULT_URI = "mongodb://mqlc:mqlc@localhost:27017/?authSource=admin"


def load_eval_script(data_dir: Path):
    spec = importlib.util.spec_from_file_location("eval_script", data_dir / "eval_script.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def read_jsonl(path: Path, loads=json.loads) -> list[dict]:
    return [loads(line) for line in path.read_text().splitlines() if line.strip()]


def unique_records(records: list[dict]) -> list[dict]:
    """First record per (arm, model, sample, candidate); mqlc never writes
    duplicates, but two processes sharing a run directory could."""
    seen, out = set(), []
    for r in records:
        key = (r["arm"], r["model"], r["sample_id"], r["candidate"])
        if key in seen:
            print(f"warning: duplicate record {key} ignored", file=sys.stderr)
            continue
        seen.add(key)
        out.append(r)
    return out


def compare(ev, gold: str, pred: str, **kwargs) -> tuple[bool, str]:
    try:
        result = ev.compare_fuzzy(gold, pred, **kwargs)
    except Exception as e:  # the official comparison raises on odd shapes
        return False, f"{type(e).__name__}: {e}"
    return bool(result["is_match"]), result["message"]


def score_record(ev, mongo_uri: str, record: dict, sample: dict) -> dict:
    out = {"sample_id": record["sample_id"], "db_id": record["db_id"], "candidate": record["candidate"]}
    if record.get("error") or record.get("pipeline") is None:
        error = record.get("error") or {"kind": "parse", "message": "no pipeline"}
        return out | {"exec_ok": False, "soft_ex": False, "strict_ex": False,
                      "message": f"{error['kind']}: {error['message']}"}
    # Records hold canonical Extended JSON; decode it into BSON types here.
    pipeline = json_util.loads(json.dumps(record["pipeline"]))
    pred_rows = ev.query_mongodb(mongo_uri, record["db_id"], record["collection"], pipeline, [])
    if pred_rows is None:
        return out | {"exec_ok": False, "soft_ex": False, "strict_ex": False,
                      "message": "execution failed"}
    gold_rows = ev.query_mongodb(
        mongo_uri, sample["db_id"], sample["MQL"]["collection"],
        sample["MQL"]["aggregation_pipeline"], [],
    )
    if gold_rows is None:
        return out | {"exec_ok": True, "soft_ex": False, "strict_ex": False,
                      "message": "gold pipeline failed to execute"}
    gold, pred = json_util.dumps(gold_rows), json_util.dumps(pred_rows)
    soft, message = compare(ev, gold, pred)
    strict, _ = compare(ev, gold, pred, allow_column_subset=False)
    return out | {"exec_ok": True, "soft_ex": soft, "strict_ex": strict, "message": message}


def percentages(scores: list[dict]) -> dict:
    n = len(scores)
    share = lambda key: 100 * sum(s[key] for s in scores) / n if n else 0.0
    return {"samples": n, "exec_rate": share("exec_ok"),
            "soft_ex": share("soft_ex"), "strict_ex": share("strict_ex")}


def score_run(run_dir: Path, data_dir: Path, mongo_uri: str) -> dict:
    ev = load_eval_script(data_dir)
    samples = {s["sample_id"]: s
               for s in read_jsonl(data_dir / "aptmqlbench_data.jsonl", json_util.loads)}
    scores = [score_record(ev, mongo_uri, r, samples[r["sample_id"]])
              for r in unique_records(read_jsonl(run_dir / "results.jsonl"))]

    with (run_dir / "scores.jsonl").open("w") as f:
        for s in scores:
            f.write(json.dumps(s) + "\n")
    by_db = defaultdict(list)
    for s in scores:
        by_db[s["db_id"]].append(s)
    summary = percentages(scores) | {
        "by_db": {db: percentages(rows) for db, rows in sorted(by_db.items())},
    }
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--data-dir", type=Path, default=Path("data/AptMQL-Bench"))
    parser.add_argument("--mongo-uri", default=DEFAULT_URI)
    args = parser.parse_args()

    summary = score_run(args.run_dir, args.data_dir, args.mongo_uri)
    print(f"{'database':<26}{'n':>5}{'Exec-Rate':>11}{'Soft-EX':>9}{'Strict-EX':>11}")
    rows = list(summary["by_db"].items()) + [("all", summary)]
    for db, s in rows:
        print(f"{db:<26}{s['samples']:>5}{s['exec_rate']:>10.2f}%{s['soft_ex']:>8.2f}%"
              f"{s['strict_ex']:>10.2f}%")


if __name__ == "__main__":
    sys.exit(main())
