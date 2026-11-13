# AQP-AgentsBench

[![tests](https://github.com/joskr2/AQP-AgentsBench/actions/workflows/tests.yml/badge.svg)](https://github.com/joskr2/AQP-AgentsBench/actions/workflows/tests.yml)

Local, outcome-based coding-agent benchmarks for **Linux x86_64**. Run task matrices across Pi, OpenCode, models, harness configurations, and repeated trials. Keep results and workspaces on your machine.

**Status:** initial implementation. Deterministic tests and an offline simulation exercise the platform; real Docker/agent/OpenRouter and Gentle-ai compatibility still need a user-authorized smoke test. Subagent measurements are limited to identities exposed by the agent/harness—missing attribution is never fabricated. Live campaigns still require a Linux x86_64 Docker host; the offline `demo` and the deterministic test suite run on other hosts too.

## Quick start: no model key, Docker, or paid calls

Requires Python 3.11 or later. From the repository root:

```bash
python -m aqp_bench --artifacts /tmp/aqp-demo demo
python -m aqp_bench --artifacts /tmp/aqp-demo list
python -m aqp_bench --artifacts /tmp/aqp-demo dashboard
```

Open `http://127.0.0.1:8765`. The demo reads an original CSV attachment, simulates an agent writing a report, forwards one request to a local mock provider, runs two hidden weighted checks separately, and retains results. It is explicitly marked `simulation: true`; it is **not** evidence that a real coding agent works.

For an installed CLI, use `python -m pip install -e '.[dev]'`, then replace `python -m aqp_bench` with `aqp-bench`. Runtime code uses the Python standard library; pytest is a development dependency.

## Run a real campaign

1. Prepare local Linux x86_64 Docker images containing the required tools and supported agent version. Start with the [Python/Pi/Gentle image](images/README.md). Images must run as your UID/GID; the platform never pulls images or installs agents implicitly.
2. Write a [version 1 JSON manifest](docs/manifests.md). The bundled [demo manifest](aqp_bench/example/campaign.json) illustrates the shape but its placeholder image/model are **not** live settings.
3. Validate without model calls or Docker:

   ```bash
   python -m aqp_bench validate campaign.json
   ```

4. Supply your OpenRouter key in an explicitly selected environment variable, then launch:

   ```bash
   python -m aqp_bench --artifacts .aqp-bench run campaign.json --api-key-env OPENROUTER_API_KEY
   ```

   This command makes real, potentially billable model requests. All main/child model configuration must use the benchmark gateway. The actual OpenRouter key stays on the host; containers get an ephemeral gateway capability. Only local Docker Unix sockets are supported, not remote Docker contexts.

5. Monitor with a second terminal running `dashboard` against the same artifact directory. The dashboard polls live state; it cannot start, stop, or answer runs.

## Benchmark any harness

Besides Pi and OpenCode, a manifest agent can be any executable harness that speaks the generic `aqp-agent-v1` JSONL protocol: `{"id": "my", "kind": "command", "command": ["my-agent"]}`. The adapter sends the task prompt and model on stdin, streams progress messages back, relays human input requests through the CLI, and treats only `done` as completion. The harness receives `AQP_GATEWAY_TOKEN` and `AQP_GATEWAY_URL`, so an OpenAI-compatible client can route every request through the benchmark gateway and keep per-attempt cost accounting. See [agent integration](docs/agents.md#generic-command-adapter-aqp-agent-v1).

## Human input and stopping

Keep the campaign terminal running. In another terminal using the same `--artifacts` directory:

```bash
python -m aqp_bench questions
python -m aqp_bench answer RUN_ID REQUEST_ID yes
python -m aqp_bench stop RUN_ID
python -m aqp_bench show RUN_ID
python -m aqp_bench rerun RUN_ID --api-key-env OPENROUTER_API_KEY
```

Pending requests contain their full question/choice metadata. Pi confirmations accept `yes/no`; Pi text/select dialogs accept text. OpenCode permissions require `once`, `always`, or `reject`; question groups require JSON, for example `'[["First option"], ["Second option"]]'`. Use `null` to cancel a supported dialog. Each accepted human reply counts once, including approvals.

Time excludes intervals when **all observed agents** are awaiting human input. Other active agents still count. Run elapsed agent time is a union of active intervals; cumulative duration sums individual agents and can exceed elapsed time. Setup and evaluation are measured separately.

Ctrl+C requests a campaign stop; stopped workspaces are still evaluated. Per-run CLI stop and configurable agent-time/cost limits also preserve evaluation, marked incomplete. Setup has its own bounded command timeout. Cost caps are best-effort: parallel in-flight requests may overshoot, and unknown billing is not a zero-dollar bill. Infrastructure-interrupted runs are excluded from aggregate scores, but their diagnostics, requests, usage, and cost remain available. There are no automatic whole-run retries; `rerun` makes a new linked record using the retained definition and selected input snapshots.

## Scoring and artifacts

Each unit test has its own assigned score. Passed units earn their full points; failed/skipped/timed-out/not-run units earn none. Reports show earned/max score, weighted score percentage, and raw passed-test percentage. A broken evaluator is an evaluation failure, not an invented zero agent score. For pytest, select exactly one collected test per entry; the evaluator inspects JUnit results and disables project conftest/autoload plugins. For npm/other commands, authors must ensure zero exit means a real pass, not a skipped or empty suite.

Hidden test files are staged outside the agent workspace and mounted read-only only into separate evaluator containers after the agent container has stopped. Evaluators receive a fresh read-only workspace snapshot. Scoring math runs in the host platform; task authors must write meaningful outcome checks. See [security boundaries](docs/security.md): read-only mounts are not a universal defense against hostile generated code executing inside a test process.

The artifact directory contains SQLite metadata, immutable selected campaign inputs, and per-run `definition.json`, `result.json`, `requests.json`, `transcript.jsonl`, initial file hashes, a baseline, final workspace, and `changes.diff`. Diff capture never executes agent-modified Git configuration. Existing repository inputs use only their selected commit and record its hash; local dirty files trigger a warning and are excluded without modifying the source repository. Attachments keep their original bytes and formats. Artifacts remain until you explicitly remove them; no automatic retention purge or cloud synchronization runs.

## Architecture and contributing

```text
aqp_bench/
  manifest.py, workspace.py       validated inputs and committed snapshots
  metrics.py, scoring.py          timing, accounting, weighted evaluation
  store.py, artifacts.py          local persistence and safe change capture
  docker.py, harness.py           isolated runtime and selected configuration
  adapters.py                    Pi RPC, OpenCode HTTP/SSE, and aqp-agent-v1 JSONL
  gateway.py                     authenticated OpenRouter forwarding
  evaluation.py, runner.py        separate checks and bounded campaigns
  cli.py, dashboard.py            CLI controls and live read-only monitoring
  demo.py, example/               explicit offline simulation
tests/                           deterministic local fixtures
docs/                            manifest, agent, gateway, security contracts
odd/tasks/initial-release.md      specification, decisions, and evidence
```

Run `pytest -q` for deterministic checks. Tests use temporary directories and local fixtures; they must not call paid providers, reuse ambient credentials, or launch remote workloads. When changing behavior, add a failing public-boundary test first, then implement and verify. Keep scenario evaluators hidden from agents, validate final observable behavior rather than attempted actions, and keep tests/docs with each behavior change.

The [implementation specification](odd/tasks/initial-release.md) records confirmed requirements separately from engineering defaults. [Agent integration](docs/agents.md) documents supported protocol surfaces and attribution gaps; [gateway accounting](docs/gateway.md) documents streaming, failures, and limits. Current local chain branches are recorded in the task log. Publishing, pushes, releases, and license selection remain owner decisions; this implementation does not perform them.
