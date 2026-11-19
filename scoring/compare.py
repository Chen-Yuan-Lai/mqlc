"""Compare two scored `mqlc bench` runs question by question.

Pairs the records of run A and run B by sample and candidate. A record
skipped for a context overflow in either run is left out, as is one only one
run answered. For each metric it reports both shares over the paired
questions, how many questions only A or only B got right, and the exact
two-sided McNemar p-value over those disagreements. Cost is summed over the
paired questions.

    uv run --project scoring scoring/compare.py runs/<a> runs/<b>
"""

import argparse
import sys
from math import comb
from pathlib import Path

from score import read_jsonl

METRICS = ("exec_ok", "soft_ex", "strict_ex")
LABELS = {"exec_ok": "Exec-Rate", "soft_ex": "Soft-EX", "strict_ex": "Strict-EX"}


def key(row: dict) -> tuple:
    return row["sample_id"], row["candidate"]


def mcnemar(only_a: int, only_b: int) -> float:
    n = only_a + only_b
    tail = sum(comb(n, i) for i in range(min(only_a, only_b) + 1)) / 2**n
    return min(1.0, 2 * tail)


def compare_runs(run_a: Path, run_b: Path) -> dict:
    a, b = ({key(s): s for s in read_jsonl(run / "scores.jsonl") if not s["skipped"]}
            for run in (run_a, run_b))
    paired = sorted(a.keys() & b.keys())
    n = len(paired)
    result = {"paired": n, "unpaired": {"a": len(a.keys() - b.keys()), "b": len(b.keys() - a.keys())}}
    for metric in METRICS:
        only_a = sum(a[k][metric] and not b[k][metric] for k in paired)
        only_b = sum(b[k][metric] and not a[k][metric] for k in paired)
        share = lambda run: 100 * sum(run[k][metric] for k in paired) / n if n else 0.0
        result[metric] = {"a": share(a), "b": share(b), "only_a": only_a, "only_b": only_b,
                          "p": mcnemar(only_a, only_b)}
    result["cost_usd"] = {}
    for name, run in (("a", run_a), ("b", run_b)):
        costs = {key(r): r.get("cost_usd") or 0 for r in read_jsonl(run / "results.jsonl")}
        result["cost_usd"][name] = sum(costs.get(k, 0) for k in paired)
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("run_a", type=Path)
    parser.add_argument("run_b", type=Path)
    args = parser.parse_args()

    r = compare_runs(args.run_a, args.run_b)
    print(f"A = {args.run_a}\nB = {args.run_b}")
    print(f"{r['paired']} paired questions; unpaired: {r['unpaired']['a']} only in A, "
          f"{r['unpaired']['b']} only in B")
    print(f"{'metric':<11}{'A':>8}{'B':>8}{'only A':>8}{'only B':>8}{'p':>8}")
    for metric in METRICS:
        m = r[metric]
        print(f"{LABELS[metric]:<11}{m['a']:>7.2f}%{m['b']:>7.2f}%{m['only_a']:>8}{m['only_b']:>8}"
              f"{m['p']:>8.3f}")
    print(f"cost       ${r['cost_usd']['a']:.4f} vs ${r['cost_usd']['b']:.4f}")


if __name__ == "__main__":
    sys.exit(main())
