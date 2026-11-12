# mqlc

## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues on Chen-Yuan-Lai/mqlc (via `gh`). See `docs/agents/issue-tracker.md`.

### Triage labels

Default five-role vocabulary: needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Delivering a ticket

A ticket is done when CI is green on the pushed commit and the issue is closed with a completion record:

1. Before committing, run every command in `.github/workflows/ci.yml` locally (Docker must be running for the testcontainers tests). Commit with `Refs #<n>`.
2. Push, then wait on the run with `gh run watch <run-id> --exit-status`.
3. Comment the completion record on the issue. It is the only shared record of the work: `runs/` is gitignored and review findings otherwise live only in the session transcript. It covers:
   - what was delivered;
   - every live run: configuration, Soft-EX / Strict-EX / Exec-Rate, cost, and what the sample can and cannot show;
   - every code-review finding with its fix, or the reason it was left;
   - every deviation from an acceptance criterion, stated before ticking it.
4. Tick the acceptance criteria and close the issue.

## Live runs

`mqlc ask` and `mqlc bench` spend real API credit. Smoke-test with `bench --limit 20` and run a full split only when the user asks for it. The first 20 dev questions are all one database (toxicology), so a smoke run checks the pipeline, never the baseline. `ANTHROPIC_API_KEY` stays in the environment only.

## Failures in a run

When adding an LLM backend or any new failure path, classify each failure per `docs/adr/0011-infrastructure-failures-stop-the-run.md`: unavailable infrastructure stops the run unrecorded; anything the model produced is recorded and scores as wrong.
