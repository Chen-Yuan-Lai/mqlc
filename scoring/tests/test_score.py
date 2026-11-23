import json
from datetime import datetime
import shutil
import urllib.request
from pathlib import Path

import pytest
from pymongo import MongoClient
from testcontainers.community.mongodb import MongoDbContainer

import score

# The official scorer, at the dataset revision scripts/fetch-data.sh pins.
EVAL_SCRIPT_URL = (
    "https://huggingface.co/datasets/MongoDB/AptMQL-Bench/resolve/"
    "ac56b3454184b77b032be0b1866a489427dada05/eval_script.py"
)
REPO_EVAL_SCRIPT = Path(__file__).parents[2] / "data/AptMQL-Bench/eval_script.py"


def gold(sample_id, pipeline):
    return {
        "split": sample_id.split("_")[0], "db_id": "pets", "question": "?", "evidence": "",
        "mongodb_evidence": "", "question_id": 1, "sample_id": sample_id, "SQL": "",
        "MQL": {"collection": "animals", "aggregation_pipeline": pipeline},
    }


def record(sample_id, pipeline=None, error=None, cost_usd=None):
    return {
        "arm": "official", "model": "scripted", "sample_id": sample_id, "candidate": 0,
        "db_id": "pets", "collection": "animals" if pipeline else None,
        "pipeline": pipeline, "row_count": None, "error": error,
        "usage": {"input_tokens": 0, "output_tokens": 0}, "latency_ms": 0,
        "cost_usd": cost_usd, "reply": "",
    }


@pytest.fixture(scope="module")
def mongo_uri():
    with MongoDbContainer("mongo:8.2") as mongo:
        uri = mongo.get_connection_url()
        MongoClient(uri).pets.animals.insert_many([
            {"name": "Tom", "species": "cat", "age": 3, "born": datetime(2022, 1, 1)},
            {"name": "Kit", "species": "cat", "age": 5, "born": datetime(2020, 1, 1)},
            {"name": "Rex", "species": "dog", "age": 7, "born": datetime(2018, 1, 1)},
        ])
        yield uri


@pytest.fixture
def data_dir(tmp_path):
    data = tmp_path / "data"
    data.mkdir()
    if REPO_EVAL_SCRIPT.exists():
        shutil.copy(REPO_EVAL_SCRIPT, data / "eval_script.py")
    else:
        urllib.request.urlretrieve(EVAL_SCRIPT_URL, data / "eval_script.py")
    names = [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1}}]
    samples = [
        gold("dev_1", [{"$match": {"born": {"$gte": {"$date": "2019-01-01T00:00:00Z"}}}}, {"$count": "n"}]),
        gold("dev_2", names),
        gold("dev_3", names),
        gold("dev_4", names),
        gold("train_1", names),
    ]
    (data / "aptmqlbench_data.jsonl").write_text("".join(json.dumps(s) + "\n" for s in samples))
    return data


def test_scores_a_run_with_official_soft_ex_strict_ex_and_exec_rate(
    tmp_path, data_dir, mongo_uri
):
    run = tmp_path / "run"
    run.mkdir()
    records = [
        # Same answer as gold under another field name: right under both
        # metrics. The date is canonical Extended JSON, as mqlc writes it.
        record("dev_1", [
            {"$match": {"born": {"$gte": {"$date": {"$numberLong": "1546300800000"}}}}},
            {"$count": "cats"},
        ], cost_usd=0.25),
        # Right names plus an extra field: Soft-EX only.
        record("dev_2", [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1, "age": 1}}],
               cost_usd=0.5),
        record("dev_3", error={"kind": "parse", "message": "no pipeline"}),
        # Never sent, so neither right nor wrong: left out of every share.
        record("dev_4", error={"kind": "skipped_context_overflow", "message": "too long"}),
        record("train_1", [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1}}]),
    ]
    (run / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))

    summary = score.score_run(run, data_dir, mongo_uri)

    assert summary["samples"] == 4
    assert summary["skipped_context_overflow"] == 1
    assert summary["exec_rate"] == pytest.approx(75)
    assert summary["soft_ex"] == pytest.approx(75)
    assert summary["strict_ex"] == pytest.approx(50)
    # What the run cost; records without a cost (here the skipped one) add nothing.
    assert summary["cost_usd"] == pytest.approx(0.75)
    # The dev split is the reported score (ADR 0006), so it stands apart.
    dev, train = summary["by_split"]["dev"], summary["by_split"]["train"]
    assert (dev["samples"], dev["skipped_context_overflow"]) == (3, 1)
    assert dev["exec_rate"] == pytest.approx(200 / 3)
    assert dev["soft_ex"] == pytest.approx(200 / 3)
    assert dev["strict_ex"] == pytest.approx(100 / 3)
    assert (train["samples"], train["soft_ex"], train["strict_ex"]) == (1, 100, 100)
    per_sample = [json.loads(l) for l in (run / "scores.jsonl").read_text().splitlines()]
    assert [(s["sample_id"], s["soft_ex"], s["strict_ex"], s["skipped"]) for s in per_sample] == [
        ("dev_1", True, True, False),
        ("dev_2", True, False, False),
        ("dev_3", False, False, False),
        ("dev_4", False, False, True),
        ("train_1", True, True, False),
    ]
    assert json.loads((run / "summary.json").read_text()) == summary


def repair(kind, errors, cost_usd, input_tokens=100, output_tokens=10, code=None):
    return {
        "kind": kind,
        "errors": [{"kind": k, "message": "..."} | ({"code": code} if code else {}) for k in errors],
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens,
                  "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        "latency_ms": 5, "cost_usd": cost_usd, "reply": "",
    }


def test_reports_what_was_repaired_what_it_cost_and_how_the_repaired_samples_scored(
    tmp_path, data_dir, mongo_uri
):
    run = tmp_path / "run"
    run.mkdir()
    names = [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1}}]
    records = [
        # Two static repairs, then an execution repair: right in the end.
        record("dev_2", names, cost_usd=0.4) | {"repairs": [
            repair("static_check", ["unknown_field", "type_mismatch"], 0.1),
            repair("static_check", ["unknown_field"], 0.1),
            repair("execution", ["execution"], 0.1, input_tokens=200, code="TypeMismatch"),
        ]},
        # Repaired, still wrong, and run while still failing the check.
        record("dev_3", [{"$match": {"species": "dog"}}, {"$project": {"_id": 0, "name": 1}}],
               cost_usd=0.2) | {
            "repairs": [repair("static_check", ["unwind_non_array"], 0.1)],
            "check_errors": [{"kind": "unwind_non_array", "message": "..."}],
            "unchecked": ["$regex", "$options"],
        },
        record("dev_4", names, cost_usd=0.1) | {"unchecked": ["$regex"]},
        record("train_1", error={"kind": "execution", "code": "BadValue", "message": "..."}),
    ]
    (run / "manifest.json").write_text(json.dumps({"config": {"ir": True}, "invocations": []}))
    (run / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))

    summary = score.score_run(run, data_dir, mongo_uri)
    repairs = summary["repairs"]

    assert repairs["samples"] == 2
    assert repairs["rounds"] == {"static_check": 3, "execution": 1}
    assert repairs["errors"] == {
        "static_check": {"type_mismatch": 1, "unknown_field": 2, "unwind_non_array": 1},
        "execution": {"execution": 1},
    }
    # Samples that had each error fed back at least once.
    assert repairs["samples_by_error"] == {
        "execution": 1, "type_mismatch": 1, "unknown_field": 1, "unwind_non_array": 1,
    }
    assert repairs["still_failing_check"] == {"unwind_non_array": 1}
    assert repairs["tokens"] == {"input_tokens": 500, "output_tokens": 40,
                                 "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
    assert repairs["cost_usd"] == pytest.approx(0.4)
    assert repairs["soft_ex"] == pytest.approx(50)
    # Execution errors told apart by the server's name for them.
    assert repairs["execution_codes"] == {"TypeMismatch": 1}
    assert summary["errors"] == {"execution:BadValue": 1}
    # The IR arm: how many candidates the check could read in full.
    checked = summary["static_check"]
    assert checked["candidates"] == 3
    assert checked["fully_checked"] == pytest.approx(100 / 3)
    assert checked["unchecked"] == {"$regex": 2, "$options": 1}


def test_a_run_without_repairs_reports_none(tmp_path, data_dir, mongo_uri):
    run = tmp_path / "run"
    run.mkdir()
    names = [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1}}]
    (run / "results.jsonl").write_text(json.dumps(record("dev_2", names)) + "\n")

    summary = score.score_run(run, data_dir, mongo_uri)
    repairs = summary["repairs"]

    assert repairs["samples"] == 0
    assert repairs["rounds"] == {}
    assert repairs["soft_ex"] is None
    assert repairs["execution_codes"] == {}
    # No manifest saying the check was on: nothing to say about it.
    assert summary["static_check"] is None
