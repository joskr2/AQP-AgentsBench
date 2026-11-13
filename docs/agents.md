# Agent and container integration

The adapters implement Pi JSONL RPC, OpenCode v1.2 HTTP APIs, and the generic
`aqp-agent-v1` JSONL protocol for arbitrary command harnesses. Deterministic
fixtures prove the local protocol behavior; **a real Docker/agent/model run has
not been verified**. The offline demo is not agent-compatibility evidence.

## Prepare an image

Provide an already-built Linux x86_64 image with `sh`, `sleep`, the chosen agent,
and the task's tools installed. Images must work under the invoking host UID/GID,
without root-only caches or installation steps. The runtime never installs tools
or pulls an image. Images and selected executable harness extensions are trusted
inputs, not a security boundary against hostile programs.

Only local Unix-socket Docker daemons are supported. `DOCKER_HOST=unix:///...`
can explicitly select a local/rootless socket; SSH/TCP Docker hosts are rejected
without probing them. The runtime ignores ambient Docker context selection and
pins the selected local socket for every operation. It does not inspect or reuse
SSH sessions, registry logins, or remote daemon credentials.

| Runtime boundary | Behavior |
|---|---|
| Workspace | Dedicated host directory bind-mounted at `/workspace` when visible to the daemon; otherwise copied through the Docker CLI and exported before container removal |
| Configuration | Selected isolated home at `/home/agent`; files are copied into container tmpfs, never bind-mounted from host temp paths |
| User | Host UID/GID, dropped capabilities, no new privileges |
| Networking | Normal bridge egress and `host.docker.internal` host-gateway alias |
| Server | OpenCode listens inside its bridge container, authenticated with an ephemeral password; no published host port |
| Evaluation | Separate container with a fresh snapshot under the run artifact directory, optional read-only `/workspace`, and trusted read-only `/evaluation` inputs; copy fallback removes write permission and runs tests as a UID distinct from the file owner |
| Cleanup | Agent workspace is exported before stopping/removing its container; evaluation remains a separate container |

`DockerRuntime.start(image, workspace, *, home=None, environment=None,
evaluation=None, readonly_workspace=False, copy_back=False)` returns a container identity.
Generated home staging is copied through the Docker client into container tmpfs,
avoiding a bind mount from host temporary directories. Marker probes verify each
requested host bind. If a probe fails, the runtime retries once without host binds
and stages full directory trees into the container root filesystem with
`docker cp`; Docker does not support direct `docker cp` to or from tmpfs. The
fallback container starts as the agent UID with `/tmp` as its initial workdir, so
copied workspace files and directories retain the expected ownership and modes.
Read-only evaluator inputs are copied from a private host staging tree with write
bits removed, and evaluator commands run as a distinct unprivileged UID. Copy
failures are fatal and include both the probe and transfer errors. With
`copy_back=True`, the caller must invoke `export_workspace(container, workspace)`
before removal; export copies the complete rootfs workspace into a fresh host
staging directory and replaces the prior tree, preserving deletions and root
directory metadata.
The normal bind path remains unchanged. Snapshots are placed under the selected
artifact directory. `exec(container, argv, timeout=300)` returns a text `CompletedProcess` so
the caller checks the exit code. `spawn(container, argv)` returns binary stdin/stdout
pipes; Pi may opt into a bounded stderr pipe, which is drained concurrently and
retained up to 64 KiB in startup/EOF diagnostics.
Before each evaluator test, required files are checked inside the container; missing
or unreadable mounts are reported as evaluator infrastructure errors, not test failures.
`address(container)` returns its bridge IPv4 address. `stop`/`remove` tolerate
missing containers, but do not suppress daemon failures. Host-to-bridge HTTP
requires native Linux Docker networking. The tested Docker Desktop/WSL daemon could
not read workspace files from the WSL filesystem; the runtime detects this with a
temporary marker and falls back to client-mediated copy. A complete Docker copy
round-trip is tested locally; provider/model-backed execution remains a separate
integration check.

## Select a harness without inheriting secrets

Both explicit and inherit modes stage the manifest's selected `files` only.
`harness.stage(files, home)` preserves bytes and directory trees; it rejects
traversal, symlinks, overlapping targets, credential filenames, recognized private
keys, and literal credential fields in JSON. It does not infer or copy host
configuration automatically. Review arbitrary selected text/code yourself:
secret-pattern checks are not a universal secret detector.

`harness.configure(home, kind, model, gateway_url, token)` preserves unrelated
settings, instructions, hooks, skills, and plugins. For Pi/OpenCode it adds the
`aqp` provider and returns the **only** explicit environment the caller should pass
to the container; a `command` harness instead receives only the base environment
and owns its model routing. The local gateway token is an environment reference,
never literal configuration; the upstream OpenRouter key must never enter the
container. Conflicting providers,
model defaults, credential stores, and ambiguous JSONC are rejected instead of
silently discarded. Pi gets gateway model defaults; OpenCode also gets runtime
inline overrides. [Pi model configuration](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/models.md)
and [OpenCode provider configuration](https://opencode.ai/docs/providers/) describe
these custom-provider APIs. [OpenCode precedence](https://opencode.ai/docs/config/)
puts runtime inline configuration after project configuration.

Call `harness.validate_workspace(workspace, kind, model)` before execution to
reject known repository-level provider/model overrides. Executable plugins may
spawn their own clients, acquire credentials, or alter networking; normal outbound
egress is NOT a forced-routing firewall. Such harness behavior is unsupported and
must be audited before comparing runs. Do not claim complete accounting for an
arbitrary harness whose requests bypass the gateway.

## Completion and human replies

Pi starts with `--mode rpc --no-session --provider aqp --model MODEL`. It consumes
UTF-8 LF-delimited JSON continuously. Prompt acceptance is not completion:
`agent_end` may precede retries/compaction, so the adapter requires `agent_settled`.
A handled prompt is the documented no-run exception. Earlier Pi versions lacking
that event fail closed when the process exits; configure an agent-time limit to
bound an older long-lived process. See [Pi RPC](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/rpc.md).

Pi dialog `ask` metadata contains the complete request. Replies are booleans for
`confirm`, strings for `select`/`input`/`editor`, or `None` to cancel. Notification
methods do not count as interventions. Unsupported terminal-only extension widgets
cannot be preserved through RPC; see [Pi extension UI](https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/rpc-extension-ui.md).

OpenCode targets the v1.2 server surface, with v1.2.27 source as the contract
reference, not a live-tested version pin. Other API families are rejected by the
health version check. It creates a session, subscribes to SSE before dispatch,
and uses the blocking `/session/:id/message` response, not asynchronous prompt
acceptance. A daemon HTTP reader remains active during human input.
[Server API](https://opencode.ai/docs/server/) documents this lifecycle.
Permission replies must be `once`, `always`, or `reject`; see the
[v1.2.27 permission route](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/server/routes/permission.ts).
Question replies are `string[][]`, one string-list per question, or `None` to
reject; see the [question route](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/opencode/src/server/routes/question.ts)
and [generated types](https://raw.githubusercontent.com/anomalyco/opencode/v1.2.27/packages/sdk/js/src/v2/gen/types.gen.ts).

Human callbacks run separately from readers; stop remains observable while a
callback waits. The CLI owns counting submitted replies. Cancellation stops the
container, not just the local `docker exec` client. A blocked host callback cannot
be forcibly killed and must respect the enclosing CLI's cancellation lifecycle.

## Generic `command` adapter: `aqp-agent-v1`

Any executable harness, including one written in another language such as Rust,
can be benchmarked without Pi/OpenCode coupling. The manifest agent
`{"id": "my", "kind": "command", "command": ["my-agent", "--flag"]}` starts that
argv inside the task container; `command` is mandatory and must be a nonempty argv
array for this kind. Pi and OpenCode keep it optional.

Transport: one UTF-8 JSON object per line, each terminated by `\n`, over the
process stdin/stdout. stderr is diagnostics only, not protocol; the adapter
retains at most 64 KiB and includes it in startup/EOF errors.

Harness → agent records:

| Record | Meaning |
|---|---|
| `{"type":"prompt","id":"<uuid>","model":"provider/model","prompt":"<text>"}` | One run's task prompt and configured model |
| `{"type":"answer","request_id":"<id>","value":<JSON>}` | Human reply to a previous `input_request`; `value` is the reply as JSON |
| `{"type":"stop"}` | Best-effort cancellation notice; the runtime also stops the container |

Agent → harness records:

| Record | Meaning |
|---|---|
| `{"type":"agent_start","agent_id":"main"}` | Optional lifecycle start; the adapter emits a normalized `agent_start` if absent |
| `{"type":"agent_end","agent_id":"main"}` | Optional lifecycle end; the adapter emits a normalized `agent_end` if absent |
| `{"type":"message","text":"..."}` | Text progress |
| `{"type":"input_request","request_id":"<id>","question":"...","metadata":{...}}` | Requests a human reply; the adapter calls `ask(question, metadata)` and writes the `answer` record with the returned value |
| `{"type":"done","stop_reason":"end_turn"}` | Terminal success; any other `stop_reason` is a failure |
| `{"type":"done","stop_reason":"error","error":"..."}` | Terminal failure; the adapter raises `RuntimeError` with that message |

Rules:

- Accepting the prompt is not completion. The adapter waits for `done`; stdout
  EOF before `done` is an error that reports the exit code and bounded stderr.
- Only `stop_reason: "end_turn"` is success. `error`, `aborted`, `max_tokens`, `length`,
  `cancelled`, or a missing reason fail the run with the reported detail.
- Protocol lines are capped at 1 MiB; duplicates of one `request_id` are ignored; unknown record
  types are ignored so future extensions stay compatible.
- Human callbacks run on a separate thread: a slow or failing `ask` never blocks
  the stdout reader. Each `input_request` is surrounded by normalized
  `agent_wait`/`agent_resume` events; reply errors surface as a `RuntimeError`.
- If the run is stopped, the adapter writes a best-effort `{"type":"stop"}` record
  and calls `runtime.stop(container)`.
- `command` emits no Pi/OpenCode provider configuration. The harness receives the
  staged `HOME` and the base environment: `AQP_GATEWAY_TOKEN` and
  `AQP_GATEWAY_URL` (container-facing base URL ending in `/v1`). Point an
  OpenAI-compatible provider at that URL and use the `model` from the `prompt`
  record to route every request through the benchmark gateway; otherwise model
  routing and gateway coverage are the harness's own responsibility and must not
  be claimed.
- Treat the task workspace as untrusted input: do not read the gateway endpoint
  or credentials from repository files, and keep harness state outside the
  workspace so it cannot pollute the evaluated diff.
- `answer.value` is JSON, but the runner validates it against the question
  metadata before returning it: confirmations become booleans, permission
  replies stay strings, and OpenCode question groups need `string[][]`.

Minimal Python agent on stdin/stdout:

```python
#!/usr/bin/env python3
import json, sys

def send(record):
    sys.stdout.write(json.dumps(record) + "\n")
    sys.stdout.flush()

prompt = json.loads(sys.stdin.readline())
assert prompt["type"] == "prompt"
send({"type": "agent_start", "agent_id": "main"})
send({"type": "message", "text": "working on: " + prompt["prompt"]})
send({"type": "input_request", "request_id": "q-1",
      "question": "Continue?", "metadata": {"method": "confirm"}})
reply = json.loads(sys.stdin.readline())
assert reply == {"type": "answer", "request_id": "q-1", "value": True}
send({"type": "message", "text": "approved"})
send({"type": "done", "stop_reason": "end_turn"})
```

## Attribution and measurement limits

Pi RPC provides no standardized child-agent identity in these records; only its
observed main lifecycle is normalized. OpenCode child sessions are attributed by
actual `session.created` parent IDs and observed child idle statuses. Opaque
external subprocesses are not invented as measured children. Child restarts or
missed events need separate capability proof before claiming exact child timing.
Neither provider configuration invents an `X-AQP-Agent-ID`: a provider shared by
main and child sessions cannot honestly label every call `main`. Gateway requests
remain `unattributed` unless an audited harness supplies real request identities.
Run-level gateway totals still include every observed provider attempt.

Non-success provider responses retain a UTF-8 diagnostic body capped at 4 KiB.
The gateway redacts the configured API key and its local capability token before
forwarding the response to the agent or storing it in `requests.json`; truncated
responses are marked and wrapped as JSON. Run results expose these diagnostics
under `provider_errors` so CLI output and the dashboard can show provider-side
causes (for example, OpenRouter guardrails) instead of only the agent's generic
HTTP error. Treat provider error text as untrusted diagnostic data.

No adapter automatically retries a whole run, classifies every protocol error as
infrastructure failure, or uses agent-supplied token/cost estimates as billing
truth. Configuring one provider is not proof of live compatibility or exhaustive
subagent accounting.

## Local checks

```bash
PYTHONPATH=. pytest -q -p no:cacheprovider tests/test_adapters.py tests/test_docker.py tests/test_harness.py
```

These checks use fake Docker calls, local Python subprocesses, and a loopback HTTP
fixture. No agent image, network provider, paid request, or ambient credential is
used. A user-authorized live smoke test remains required for each selected image,
agent version, and executable harness combination.
