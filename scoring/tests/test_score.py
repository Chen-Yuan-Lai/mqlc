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
        "split": "dev", "db_id": "pets", "question": "?", "evidence": "",
        "mongodb_evidence": "", "question_id": 1, "sample_id": sample_id, "SQL": "",
        "MQL": {"collection": "animals", "aggregation_pipeline": pipeline},
    }


def record(sample_id, pipeline=None, error=None):
    return {
        "arm": "official", "model": "scripted", "sample_id": sample_id, "candidate": 0,
        "db_id": "pets", "collection": "animals" if pipeline else None,
        "pipeline": pipeline, "row_count": None, "error": error,
        "usage": {"input_tokens": 0, "output_tokens": 0}, "latency_ms": 0,
        "cost_usd": None, "reply": "",
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
        ]),
        # Right names plus an extra field: Soft-EX only.
        record("dev_2", [{"$match": {"species": "cat"}}, {"$project": {"_id": 0, "name": 1, "age": 1}}]),
        record("dev_3", error={"kind": "parse", "message": "no pipeline"}),
    ]
    (run / "results.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))

    summary = score.score_run(run, data_dir, mongo_uri)

    assert summary["samples"] == 3
    assert summary["exec_rate"] == pytest.approx(200 / 3)
    assert summary["soft_ex"] == pytest.approx(200 / 3)
    assert summary["strict_ex"] == pytest.approx(100 / 3)
    per_sample = [json.loads(l) for l in (run / "scores.jsonl").read_text().splitlines()]
    assert [(s["sample_id"], s["soft_ex"], s["strict_ex"]) for s in per_sample] == [
        ("dev_1", True, True),
        ("dev_2", True, False),
        ("dev_3", False, False),
    ]
    assert json.loads((run / "summary.json").read_text()) == summary
