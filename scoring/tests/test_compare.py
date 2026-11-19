import json

import pytest

import compare


def write_run(path, outcomes, cost=0.001):
    """`outcomes` maps a sample id to (soft_ex, strict_ex), or to None when
    the record was skipped for a context overflow."""
    path.mkdir()
    scores, results = [], []
    for sample_id, outcome in outcomes.items():
        skipped = outcome is None
        soft, strict = outcome or (False, False)
        scores.append({"sample_id": sample_id, "db_id": "pets", "candidate": 0,
                       "skipped": skipped, "exec_ok": not skipped, "soft_ex": soft,
                       "strict_ex": strict, "message": ""})
        results.append({"sample_id": sample_id, "candidate": 0,
                        "cost_usd": None if skipped else cost})
    (path / "scores.jsonl").write_text("".join(json.dumps(s) + "\n" for s in scores))
    (path / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in results))
    return path


def test_compares_two_runs_question_by_question(tmp_path):
    # Soft-EX: A alone is right on q1..q6, B alone on none -> 6 vs 0.
    # Strict-EX: A alone on q1, B alone on q7..q15 -> 1 vs 9.
    a, b = {}, {}
    for i in range(1, 7):
        a[f"dev_{i}"], b[f"dev_{i}"] = (True, i == 1), (False, False)
    for i in range(7, 16):
        a[f"dev_{i}"], b[f"dev_{i}"] = (True, False), (True, True)
    a["dev_16"], b["dev_16"] = (True, True), (True, True)
    # Skipped in one run, or answered in only one: not paired.
    a["dev_17"], b["dev_17"] = (True, True), None
    a["dev_18"] = (True, True)
    b["dev_19"] = (False, False)

    result = compare.compare_runs(write_run(tmp_path / "a", a, cost=0.002),
                                  write_run(tmp_path / "b", b, cost=0.001))

    assert result["paired"] == 16
    assert result["unpaired"] == {"a": 2, "b": 1}
    soft, strict = result["soft_ex"], result["strict_ex"]
    assert (soft["a"], soft["b"]) == (pytest.approx(100), pytest.approx(10 / 16 * 100))
    assert (soft["only_a"], soft["only_b"]) == (6, 0)
    # Exact two-sided McNemar, by hand: 2 * (1/2)^6, and 2 * (1 + 10) / 2^10.
    assert soft["p"] == pytest.approx(0.03125)
    assert (strict["only_a"], strict["only_b"]) == (1, 9)
    assert strict["p"] == pytest.approx(0.021484375)
    # Cost over the paired questions only.
    assert result["cost_usd"] == {"a": pytest.approx(0.032), "b": pytest.approx(0.016)}


def test_runs_that_never_disagree_have_p_of_one(tmp_path):
    same = {"dev_1": (True, False), "dev_2": (False, False)}
    result = compare.compare_runs(write_run(tmp_path / "a", same), write_run(tmp_path / "b", same))
    assert (result["soft_ex"]["only_a"], result["soft_ex"]["only_b"]) == (0, 0)
    assert result["soft_ex"]["p"] == 1.0
