"""Score an `mqlc bench` run with the official AptMQL-Bench eval_script.py.

Runs both the gold pipeline and the predicted one against MongoDB, then compares
the results with the official `compare_fuzzy`:

- Soft-EX: `compare_fuzzy` with its defaults (the metric the paper proposes);
  the prediction may return extra fields.
- Strict-EX: the same comparison without extra fields (`allow_column_subset=False`).
- Exec-Rate: share of predictions that parsed and executed without error.

Records skipped for a context overflow (the prompt was never sent) are neither
right nor wrong: they are left out of all three shares and counted apart.

Writes `scores.jsonl` (one line per record) and `summary.json` (overall, by
split and by database, plus what the run cost, its errors by kind, what was
repaired, how much of each pipeline the static check could read and, for a
Claude Code run, the tools it called) into the run directory, and prints the summary. `compare.py` compares two scored runs
question by question.

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
SKIPPED = "skipped_context_overflow"


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
    out = {"sample_id": record["sample_id"], "db_id": record["db_id"],
           "candidate": record["candidate"], "skipped": False}
    if (record.get("error") or {}).get("kind") == SKIPPED:
        return out | {"exec_ok": False, "soft_ex": False, "strict_ex": False, "skipped": True,
                      "message": f"{SKIPPED}: {record['error']['message']}"}
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
    scored = [s for s in scores if not s["skipped"]]
    n = len(scored)
    share = lambda key: 100 * sum(s[key] for s in scored) / n if n else 0.0
    return {"samples": n, SKIPPED: len(scores) - n, "exec_rate": share("exec_ok"),
            "soft_ex": share("soft_ex"), "strict_ex": share("strict_ex")}


def repair_report(records: list[dict], scores: list[dict]) -> dict:
    """Experiment B: what errors were fed back for a repair (per kind of
    repair), what the repairs cost, and how the repaired samples scored."""
    repaired = [(r, s) for r, s in zip(records, scores) if r.get("repairs")]
    rounds, errors, samples_by_error, still_failing, codes = (
        defaultdict(int), defaultdict(lambda: defaultdict(int)), defaultdict(int), defaultdict(int),
        defaultdict(int))
    tokens = {k: 0 for k in ("input_tokens", "output_tokens",
                             "cache_read_input_tokens", "cache_creation_input_tokens")}
    cost = 0.0
    for r, _ in repaired:
        seen = set()
        for rep in r["repairs"]:
            rounds[rep["kind"]] += 1
            for e in rep["errors"]:
                errors[rep["kind"]][e["kind"]] += 1
                seen.add(e["kind"])
                if e.get("code"):
                    codes[e["code"]] += 1
            for k in tokens:
                tokens[k] += rep["usage"].get(k, 0)
            cost += rep.get("cost_usd") or 0
        for kind in seen:
            samples_by_error[kind] += 1
    for r in records:
        for kind in {e["kind"] for e in r.get("check_errors", [])}:
            still_failing[kind] += 1
    scored = [s for _, s in repaired if not s["skipped"]]
    return {
        "samples": len(repaired),
        "rounds": dict(sorted(rounds.items(), key=lambda kv: kv[0] != "static_check")),
        "errors": {kind: dict(sorted(by.items())) for kind, by in errors.items()},
        "samples_by_error": dict(sorted(samples_by_error.items())),
        "still_failing_check": dict(sorted(still_failing.items())),
        "tokens": tokens,
        "cost_usd": cost,
        "soft_ex": 100 * sum(s["soft_ex"] for s in scored) / len(scored) if scored else None,
        "execution_codes": dict(sorted(codes.items())),
    }


def error_counts(records: list[dict]) -> dict:
    """The final error of each record by kind, execution errors by the
    server's name for them (`execution:TypeMismatch`)."""
    counts = defaultdict(int)
    for r in records:
        e = r.get("error")
        if e and e["kind"] != SKIPPED:
            counts[f"{e['kind']}:{e['code']}" if e.get("code") else e["kind"]] += 1
    return dict(sorted(counts.items()))


def check_report(run_dir: Path, records: list[dict]) -> dict | None:
    """For a run with the static check on (the IR arm): how many candidates
    it could read in full, and for how many each construct it cannot read
    left the rest unchecked."""
    manifest = run_dir / "manifest.json"
    if not manifest.exists() or not json.loads(manifest.read_text())["config"].get("ir"):
        return None
    candidates = [r for r in records if r.get("pipeline") is not None]
    unchecked = defaultdict(int)
    for r in candidates:
        for construct in r.get("unchecked", []):
            unchecked[construct] += 1
    full = sum(1 for r in candidates if not r.get("unchecked"))
    return {
        "candidates": len(candidates),
        "fully_checked": 100 * full / len(candidates) if candidates else 0.0,
        "unchecked": dict(sorted(unchecked.items(), key=lambda kv: (-kv[1], kv[0]))),
    }


def tool_report(run_dir: Path, records: list[dict]) -> dict | None:
    """For a run answered by Claude Code (experiment E): how many tools it
    called per sample, and how often each tool was called and by how many
    samples. Skipped records called nothing and are left out."""
    manifest = run_dir / "manifest.json"
    if (not manifest.exists()
            or json.loads(manifest.read_text())["config"].get("backend") != "claude-code"):
        return None
    asked = [r for r in records if (r.get("error") or {}).get("kind") != SKIPPED]
    by_tool, using = defaultdict(int), defaultdict(int)
    for r in asked:
        for tool, n in r.get("tool_calls", {}).items():
            by_tool[tool] += n
            using[tool] += 1
    total = sum(by_tool.values())
    return {
        "samples": len(asked),
        "total": total,
        "per_sample": total / len(asked) if asked else None,
        "samples_without": sum(1 for r in asked if not r.get("tool_calls")),
        "by_tool": dict(sorted(by_tool.items())),
        "samples_using": dict(sorted(using.items())),
    }


def score_run(run_dir: Path, data_dir: Path, mongo_uri: str) -> dict:
    ev = load_eval_script(data_dir)
    samples = {s["sample_id"]: s
               for s in read_jsonl(data_dir / "aptmqlbench_data.jsonl", json_util.loads)}
    records = unique_records(read_jsonl(run_dir / "results.jsonl"))
    scores = [score_record(ev, mongo_uri, r, samples[r["sample_id"]]) for r in records]

    with (run_dir / "scores.jsonl").open("w") as f:
        for s in scores:
            f.write(json.dumps(s) + "\n")
    by_split, by_db = defaultdict(list), defaultdict(list)
    for s in scores:
        by_split[samples[s["sample_id"]]["split"]].append(s)
        by_db[s["db_id"]].append(s)
    summary = percentages(scores) | {
        "cost_usd": sum(r.get("cost_usd") or 0 for r in records),
        "by_split": {split: percentages(rows) for split, rows in sorted(by_split.items())},
        "by_db": {db: percentages(rows) for db, rows in sorted(by_db.items())},
        "errors": error_counts(records),
        "repairs": repair_report(records, scores),
        "static_check": check_report(run_dir, records),
        "tool_calls": tool_report(run_dir, records),
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
    print(f"{'database':<26}{'n':>5}{'skipped':>9}{'Exec-Rate':>11}{'Soft-EX':>9}{'Strict-EX':>11}")
    rows = (list(summary["by_db"].items())
            + [(f"[{split}]", s) for split, s in summary["by_split"].items()]
            + [("all", summary)])
    for db, s in rows:
        print(f"{db:<26}{s['samples']:>5}{s[SKIPPED]:>9}{s['exec_rate']:>10.2f}%"
              f"{s['soft_ex']:>8.2f}%{s['strict_ex']:>10.2f}%")
    print(f"cost ${summary['cost_usd']:.4f}")
    if summary["tool_calls"]:
        t = summary["tool_calls"]
        print(f"tool calls: {t['total']} ({t['per_sample']:.2f} per sample, "
              f"{t['samples_without']} samples called none)")
        for tool, n in t["by_tool"].items():
            print(f"  {tool:<40}{n:>6} calls{t['samples_using'][tool]:>6} samples")
    if summary["static_check"]:
        c = summary["static_check"]
        top = ", ".join(f"{k} {n}" for k, n in list(c["unchecked"].items())[:8])
        print(f"static check read {c['fully_checked']:.1f}% of {c['candidates']} candidates in full"
              + (f"; unread: {top}" if top else ""))
    repairs = summary["repairs"]
    if repairs["samples"]:
        print(f"\nrepaired {repairs['samples']} samples (Soft-EX {repairs['soft_ex']:.2f}% after), "
              f"repairs cost ${repairs['cost_usd']:.4f}")
        for kind, n in repairs["rounds"].items():
            by_error = ", ".join(f"{e} {c}" for e, c in repairs["errors"][kind].items())
            print(f"  {kind:<13}{n:>6} rounds  errors fed back: {by_error}")
        if repairs["execution_codes"]:
            print("  execution errors fed back: " + ", ".join(
                f"{c} {n}" for c, n in repairs["execution_codes"].items()))
        if repairs["still_failing_check"]:
            print("  run while still failing the static check: " + ", ".join(
                f"{e} {c}" for e, c in repairs["still_failing_check"].items()))


if __name__ == "__main__":
    sys.exit(main())
