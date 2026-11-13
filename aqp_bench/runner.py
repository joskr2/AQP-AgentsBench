"""Bounded campaigns: isolated work, observable accounting, separate evaluation."""

import copy
import hashlib
import json
import platform
import shutil
import tempfile
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from . import harness
from .adapters import CommandAdapter, OpenCodeAdapter, PiAdapter
from .artifacts import inventory, write_diff
from .docker import DockerRuntime
from .evaluation import evaluate
from .gateway import Gateway
from .manifest import expand_runs
from .metrics import Timeline, summarize_attempts
from .workspace import copy_files, preflight_repository, prepare


def coerce_answer(text, metadata):
    """Validate protocol replies before counting/persisting a human submission."""
    if text == "null":
        return None
    if metadata.get("method") == "confirm":
        if text.lower() not in ("true", "false", "yes", "no"):
            raise ValueError("Confirmation requires yes/no or true/false")
        return text.lower() in ("true", "yes")
    if "permission" in metadata:
        if text not in ("once", "always", "reject"):
            raise ValueError("Permission reply must be once, always, or reject")
        return text
    if "questions" in metadata:
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            raise ValueError("Reply must be JSON: one string array per question") from None
        questions = metadata["questions"]
        if not isinstance(value, list) or len(value) != len(questions):
            raise ValueError("Reply requires one answer array per question")
        for answers, question in zip(value, questions):
            if not isinstance(answers, list) or any(not isinstance(a, str) for a in answers):
                raise ValueError("Each question answer must be an array of strings")
            labels = [o["label"] for o in question.get("options", [])]
            if not question.get("multiple") and len(answers) > 1:
                raise ValueError("Question permits only one selection")
            if question.get("custom") is False and any(a not in labels for a in answers):
                raise ValueError("Answer must match a presented option")
        return value
    if metadata.get("method") == "select" and text not in metadata.get("options", []):
        raise ValueError("Reply must match a presented option")
    return text


def _redact(value, secrets):
    if isinstance(value, str):
        for secret in secrets:
            if secret:
                value = value.replace(secret, "[redacted]")
        return value
    if isinstance(value, dict):
        return {str(k): _redact(v, secrets) for k, v in value.items()}
    if isinstance(value, list):
        return [_redact(v, secrets) for v in value]
    return value


def run_campaign(campaign, store, *, api_key, runtime_factory=DockerRuntime,
                 gateway_factory=Gateway, adapter_factory=None, notify=print, cancel=None):
    # The Docker runtime only supports local Linux x86_64 daemons. Injected
    # runtimes (offline demo, test fixtures) carry their own platform contract.
    if runtime_factory is DockerRuntime and (
            platform.system() != "Linux" or platform.machine().lower() not in ("x86_64", "amd64")):
        raise ValueError("The initial release requires Linux x86_64")
    if not isinstance(api_key, str) or not api_key:
        raise ValueError("Supply an explicit OpenRouter API key via the selected environment variable")
    campaign = copy.deepcopy(campaign)
    for task in campaign["tasks"]:
        if task.get("repository"):
            task["repository"], warnings = preflight_repository(task["repository"])
            for warning in warnings:
                notify("WARNING: " + warning)
    campaign_id = uuid.uuid4().hex
    # Freeze selected inputs once, so parallel trials cannot see different bytes.
    inputs = store.root / "campaigns" / campaign_id
    for task in campaign["tasks"]:
        for role, entries in (("attachments", task.get("attachments", [])),
                              ("evaluation", task["evaluation"].get("files", []))):
            destination = inputs / "tasks" / task["id"] / role
            destination.mkdir(parents=True)
            copy_files(entries, destination)
            for entry in entries:
                entry["source"] = str(destination / entry["target"])
    for selected in campaign["harnesses"]:
        destination = inputs / "harnesses" / selected["id"]
        destination.mkdir(parents=True)
        harness.stage(selected.get("files", []), destination)
        for entry in selected.get("files", []):
            entry["source"] = str(destination / entry["target"])
    specs = expand_runs(campaign)
    cancel = cancel or threading.Event()
    results = []
    with ThreadPoolExecutor(max_workers=campaign["concurrency"]) as pool:
        futures = [pool.submit(_run, spec, campaign_id, store, api_key, runtime_factory,
                               gateway_factory, adapter_factory, notify, cancel) for spec in specs]
        try:
            for future in as_completed(futures):
                results.append(future.result())
        except KeyboardInterrupt:
            cancel.set()
            notify("Stopping agents; retained workspaces will still be evaluated.")
            results = [future.result() for future in futures]
    return sorted(results, key=lambda r: (r["task"], r["agent"], r["model"], r["harness"], r["trial"]))


def _run(spec, campaign_id, store, api_key, runtime_factory, gateway_factory, adapter_factory, notify, cancel):
    identifier = uuid.uuid4().hex
    directory = store.run_dir(identifier)
    result = {"id": identifier, "campaign_id": campaign_id, "task": spec["task"]["id"],
              "agent": spec["agent"]["id"], "model": spec["model"], "harness": spec["harness"]["id"],
              "trial": spec["trial"], "status": "preparing", "started_at": time.time(),
              "human_interventions": 0, "warnings": []}
    secrets = [api_key]
    timeline = Timeline()
    lock = threading.RLock()
    stop = threading.Event()
    finished = threading.Event()
    runtime = runtime_factory()
    container, gateway, watcher = None, None, None
    cause = []
    attempts = []
    start = time.monotonic()
    store.save(result)

    def save():
        with lock:
            result["timing"] = timeline.snapshot()
            if gateway is not None:
                result["metrics"] = summarize_attempts(gateway.attempts)
            result["human_interventions"] = store.interventions(identifier)
            result["pending_questions"] = store.questions(identifier)
            store.save(_redact(result, secrets))

    def event(data):
        timeline.event(data)
        store.event(identifier, _redact(data, secrets))

    def ask(question, metadata):
        request_id = store.question(identifier, _redact(question, secrets), _redact(metadata, secrets))
        notify(json.dumps({"run_id": identifier, "request_id": request_id,
                           "question": _redact(question, secrets), "metadata": _redact(metadata, secrets)}))
        while not stop.wait(.05):
            text = store.response(identifier, request_id)
            if text is not None:
                return coerce_answer(text, metadata)
        raise RuntimeError("Run stopped while awaiting human input")

    def watch():
        stop_sent = False
        while not finished.wait(.1):
            if cancel.is_set() or store.stop_requested(identifier):
                cause.append("manual_stop")
                stop.set()
            limit = spec["limits"].get("agent_seconds")
            if limit is not None and timeline.snapshot()["agent_seconds"] >= limit:
                cause.append("agent_time_limit")
                stop.set()
            if gateway.limit_reached.is_set():
                cause.append("cost_limit")
                stop.set()
            if stop.is_set() and not stop_sent:
                stop_sent = True
                try:
                    runtime.stop(container)
                except Exception as error:
                    result["stop_error"] = str(error)
            save()

    workspace = directory / "workspace"
    try:
        metadata = prepare(spec["task"], workspace)
        result["input"] = metadata
        result["warnings"].extend(metadata["warnings"])
        before = inventory(workspace)
        baseline = directory / "baseline"
        shutil.copytree(workspace, baseline, symlinks=True, ignore=shutil.ignore_patterns(".git"))
        (directory / "initial-files.json").write_text(json.dumps(before, indent=2) + "\n")
        (directory / "definition.json").write_text(json.dumps(_redact(spec, secrets), indent=2) + "\n")
        input_identity = {"prompt": spec["task"]["prompt"], "commit": metadata["commit"], "files": before}
        result["input_sha256"] = hashlib.sha256(json.dumps(input_identity, sort_keys=True).encode()).hexdigest()
        harness.validate_workspace(workspace, spec["agent"]["kind"], spec["model"])
        with tempfile.TemporaryDirectory(prefix="aqp-home-") as temporary_home:
            home = Path(temporary_home)
            harness.stage(spec["harness"].get("files", []), home)
            with gateway_factory(api_key, host="0.0.0.0", cost_limit=spec["limits"].get("cost_usd")) as gateway:
                secrets.append(gateway.token)
                url = gateway.url.replace("0.0.0.0", "host.docker.internal").replace("127.0.0.1", "host.docker.internal")
                environment = harness.configure(home, spec["agent"]["kind"], spec["model"], url, gateway.token)
                container = runtime.start(spec["task"]["image"], workspace, home=home,
                                          environment=environment, copy_back=True)
                transport_for = getattr(runtime, "transport_for", None)
                transport = transport_for(container) if callable(transport_for) else None
                if transport is not None:
                    result["workspace_transport"] = transport
                if transport == "docker_cp":
                    result["warnings"].append(
                        "Docker bind mounts could not read the workspace; used Docker CLI copy transport.")
                watcher = threading.Thread(target=watch, daemon=True)
                watcher.start()
                for command in spec["task"].get("setup", []):
                    if stop.is_set():
                        break
                    setup = runtime.exec(container, command, timeout=300)
                    if setup.returncode and not stop.is_set():
                        raise RuntimeError("Task setup failed: " + setup.stderr)
                result["setup_seconds"] = time.monotonic() - start
                if cancel.is_set() or store.stop_requested(identifier):
                    cause.append("manual_stop")
                    stop.set()
                result["status"] = "running"
                save()
                adapter_type = adapter_factory or {"pi": PiAdapter, "opencode": OpenCodeAdapter,
                                                   "command": CommandAdapter}[spec["agent"]["kind"]]
                try:
                    if not stop.is_set():
                        adapter_type(runtime, container, spec["agent"].get("command")).run(
                            spec["task"]["prompt"], spec["model"], event, ask, stop)
                    result["status"] = "incomplete" if stop.is_set() else "completed"
                except Exception as error:
                    result["error"] = str(error)
                    if stop.is_set():
                        result["status"] = "incomplete"
                    elif gateway.infrastructure_failed.is_set():
                        result["status"] = "infrastructure_failed"
                    elif any(a.get("status") and 400 <= a["status"] < 500 and a["status"] != 429 for a in gateway.attempts):
                        result["status"] = "agent_failed"
                    else:
                        result["status"] = "integration_failed"
                finally:
                    finished.set()
                    if watcher is not None:
                        watcher.join(timeout=5)
                    timeline.finish()
                    export_error = None
                    exporter = getattr(runtime, "export_workspace", None)
                    if container is not None and callable(exporter):
                        try:
                            exporter(container, workspace)
                        except Exception as error:
                            export_error = error
                            result["infrastructure_incidents"] = True
                    cleanup_error = None
                    if container is not None:
                        for cleanup in (runtime.stop, runtime.remove):
                            try:
                                cleanup(container)
                            except Exception as error:
                                cleanup_error = cleanup_error or error
                        if cleanup_error is None:
                            container = None
                    if export_error is not None:
                        raise RuntimeError(f"Workspace copy-back failed: {export_error}") from export_error
                    if cleanup_error is not None:
                        raise cleanup_error
                if gateway.limit_reached.is_set():
                    result["status"] = "incomplete"
                    cause.append("cost_limit")
                attempts = gateway.attempts
                result["infrastructure_incidents"] = gateway.infrastructure_failed.is_set()
        result["stop_reason"] = cause[0] if cause else ("manual_stop" if stop.is_set() else None)
        result["status_before_evaluation"] = result["status"]
        result["status"] = "evaluating"
        save()
        eval_start = time.monotonic()
        try:
            result["evaluation"] = evaluate(runtime, workspace, spec["task"]["evaluation"],
                                             scratch_parent=directory)
            result["status"] = result.pop("status_before_evaluation")
        except Exception as error:
            result["status"] = "evaluation_failed"
            result["evaluation_error"] = str(error)
        result["evaluation_seconds"] = time.monotonic() - eval_start
        write_diff(before, workspace, directory / "changes.diff", baseline)
    except Exception as error:
        result["status"] = "infrastructure_failed"
        result["error"] = str(error)
    finally:
        finished.set()
        stop.set()
        if watcher is not None:
            watcher.join(timeout=5)
        timeline.finish()
        if container is not None:
            for cleanup in (runtime.stop, runtime.remove):
                try:
                    cleanup(container)
                except Exception as error:
                    result["cleanup_error"] = result.get("cleanup_error") or str(error)
        result["timing"] = timeline.snapshot()
        final_attempts = gateway.attempts if gateway is not None else attempts
        result["metrics"] = summarize_attempts(final_attempts)
        provider_errors = [{"attempt_id": attempt.get("attempt_id"),
                            "status": attempt.get("status"),
                            "model": attempt.get("model"),
                            "body": attempt["provider_error_body"],
                            "truncated": attempt.get("provider_error_truncated", False)}
                           for attempt in final_attempts if attempt.get("provider_error_body")]
        if provider_errors:
            result["provider_errors"] = provider_errors
        (directory / "requests.json").write_text(json.dumps(_redact(final_attempts, secrets), indent=2) + "\n")
        if result["status"] == "completed" and not result["metrics"]["request_count"]:
            result["warnings"].append("No model requests observed; accounting and gateway coverage are unverified.")
            result["metrics"].update(cost_usd=None, input_tokens=None, output_tokens=None, accounting_complete=False)
        result["human_interventions"] = store.interventions(identifier)
        result["pending_questions"] = []
        result["measurement_capabilities"] = {
            "request_attribution": "observed_headers_only",
            "subagent_timing": "observed_session_events_only" if spec["agent"]["kind"] == "opencode" else "unavailable_without_harness_instrumentation"}
        result["duration_seconds"] = time.monotonic() - start
        result["finished_at"] = time.time()
        store.save(_redact(result, secrets))
    return _redact(result, secrets)
