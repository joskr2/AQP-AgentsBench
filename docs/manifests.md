# Campaign manifests

Version 1 campaigns are JSON, resolved relative to the manifest file. Validation rejects unknown keys, invalid scores, duplicate IDs, and unsafe destination paths before creating workspaces. Models use OpenRouter `provider/model` identifiers. Every matrix combination gets its own workspace and trial number.

```json
{
  "version": 1,
  "concurrency": 2,
  "trials": 1,
  "limits": {"agent_seconds": 300, "cost_usd": 1},
  "models": ["vendor/model"],
  "agents": [{"id": "pi", "kind": "pi"}],
  "harnesses": [{"id": "plain", "mode": "explicit", "files": []}],
  "tasks": [{
    "id": "report",
    "prompt": "Create report.txt summarizing data/input.csv.",
    "image": "your-prepared-agent-image:version",
    "setup": [],
    "attachments": [{"source": "input.csv", "target": "data/input.csv"}],
    "evaluation": {
      "image": "your-prepared-evaluator-image:version",
      "files": [{"source": "verify.py", "target": "verify.py"}],
      "tests": [{"id": "report-content", "score": 5,
        "command": ["python", "/evaluation/verify.py"]}]
    }
  }]
}
```

`agents[].kind` is `pi`, `opencode`, or `command`. Pi and OpenCode optionally accept
an argv `command` override; `command` agents require it because the adapter has no
built-in executable. A generic harness uses the `aqp-agent-v1` protocol documented
in [agent integration](agents.md):

```json
{"id": "my", "kind": "command", "command": ["my-agent"]}
```

Each `evaluation.tests` entry represents **one independently scored unit test**. Use a specific pytest node selector or npm test selector, not an entire suite assigned one score. Exit zero means passed; other exits mean failed. A passing unit earns its assigned score. Skipped/not-run tests do not earn points; infrastructure faults are recorded separately. The total score denominator is the sum of all defined test scores, not only executed tests.

Modification tasks add `repository: {"source": "../repo", "revision": "HEAD"}`. Local revision names are resolved to a commit before campaign execution; HTTP(S) Git URLs require a full commit hash. Local repositories must already have a commit. Dirty files trigger a pre-start warning and are excluded. The source repository is never cleaned or auto-committed. Remote authentication is not inherited; authenticated Git transport is outside this initial manifest.

`harnesses[].mode` is `explicit` or `inherit`. Both use a selected `files` list with `source` and relative `target` inside the isolated agent HOME. Inheritance is an explicit selection of host-installed resources, not copying the host environment. Credentials must not appear in these files; provider configuration must be compatible with the benchmark gateway. See [agent integration](agents.md) for exact supported configuration behavior.

Inputs are copied byte-for-byte. Attachment destinations must not overlap or replace existing repository files. Directory trees cannot contain symlinks; benchmark preparation never follows input symlinks into host data. Evaluation files stay outside the agent workspace during execution.

Selected attachments, evaluator files, and harness files are frozen once per campaign before parallel trials start. Retained definitions point to those snapshots; explicit reruns reuse them rather than rereading changed original files. Initial workspace hashes and the starting commit identify task inputs independently of trial number or model selection.

## Core accounting

Run agent time is the union of active main-agent/subagent intervals. When all tracked agents await human input, that interval is human-wait time; overlapping active work still counts. Cumulative agent duration is the sum of individual durations and can exceed elapsed run duration. Unknown child attribution is not guessed.

Provider attempts are deduplicated by benchmark attempt ID. Missing token or cost fields make that total unknown; known subtotals remain available. Do not add parent usage summaries to child request records. Infrastructure-interrupted runs retain accounting but are excluded from aggregate agent scores. Explicit reruns produce new run records, never overwrite the original.
