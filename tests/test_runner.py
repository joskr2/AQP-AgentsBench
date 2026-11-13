import json
import subprocess
import threading
import time
from pathlib import Path

import pytest

from aqp_bench.runner import coerce_answer, run_campaign
from aqp_bench.store import Store


class Runtime:
    def __init__(self):
        self.containers = {}
        self.stopped = []
        self.lifecycle = []

    def start(self, image, workspace, **kwargs):
        identity = str(len(self.containers))
        self.containers[identity] = (Path(workspace), kwargs)
        self.lifecycle.append(("start", identity))
        if "evaluation" in kwargs:
            assert "0" in self.stopped
            assert kwargs["readonly_workspace"]
        return identity

    def exec(self, identity, command, **kwargs):
        workspace, options = self.containers[identity]
        assert not (workspace / "hidden.py").exists()
        if command[:2] == ["sh", "-c"]:
            return subprocess.CompletedProcess(command, 0, "", "")
        return subprocess.CompletedProcess(command, 0 if (workspace / "answer.txt").exists() else 1, "", "")

    def stop(self, identity):
        self.stopped.append(identity)
        self.lifecycle.append(("stop", identity))

    def remove(self, identity):
        self.lifecycle.append(("remove", identity))

    def transport_for(self, identity):
        return "docker_cp"

    def export_workspace(self, identity, workspace):
        self.lifecycle.append(("export", identity))


class Gateway:
    def __init__(self, key, **kwargs):
        self.token = "ephemeral-local-secret"
        self.url = "http://0.0.0.0:12345/v1"
        self.attempts = [{"attempt_id": "one", "agent_id": "unattributed", "input_tokens": 2,
                          "output_tokens": 3, "cost_usd": 0.01, "error_kind": None}]
        self.limit_reached = threading.Event()
        self.infrastructure_failed = threading.Event()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        pass


class Adapter:
    def __init__(self, runtime, container, command=None):
        self.runtime, self.container = runtime, container

    def run(self, prompt, model, on_event, ask, stop):
        workspace, _ = self.runtime.containers[self.container]
        assert not (workspace / "hidden.py").exists()
        on_event({"type": "agent_start", "agent_id": "main"})
        (workspace / "answer.txt").write_text("42")
        on_event({"type": "message", "text": "provider-key ephemeral-local-secret"})
        on_event({"type": "agent_end", "agent_id": "main"})


def spec(tmp_path):
    hidden = tmp_path / "hidden.py"
    hidden.write_text("assert True")
    return {"concurrency": 1, "trials": 1, "limits": {}, "models": ["vendor/model"],
            "agents": [{"id": "pi", "kind": "pi"}],
            "harnesses": [{"id": "plain", "mode": "explicit", "files": []}],
            "tasks": [{"id": "answer", "prompt": "Write answer.txt", "image": "fixture",
                       "attachments": [], "setup": [],
                       "evaluation": {"image": "fixture-eval", "files": [{"source": str(hidden), "target": "hidden.py"}],
                                      "timeout_seconds": 1,
                                      "tests": [{"id": "answer", "score": 5, "command": ["python", "/evaluation/hidden.py"]}]}}]}


def test_campaign_separates_hidden_evaluation_and_redacts_known_secrets(tmp_path):
    store = Store(tmp_path / "artifacts")
    results = run_campaign(spec(tmp_path), store, api_key="provider-key", runtime_factory=Runtime,
                           gateway_factory=Gateway, adapter_factory=Adapter, notify=lambda _: None)
    result = results[0]
    assert result["status"] == "completed"
    assert result["evaluation"]["earned_score"] == 5
    assert result["metrics"]["request_count"] == 1
    assert result["metrics"]["cost_usd"] == 0.01
    assert result["metrics"]["attribution_complete"] is False
    assert "provider_errors" not in result
    persisted = (store.run_dir(result["id"]) / "transcript.jsonl").read_text()
    assert "provider-key" not in persisted and "ephemeral-local-secret" not in persisted
    assert (store.run_dir(result["id"]) / "workspace/answer.txt").read_text() == "42"
    assert (store.run_dir(result["id"]) / "changes.diff").exists()


def test_campaign_exposes_sanitized_provider_errors(tmp_path):
    class RejectedGateway(Gateway):
        def __init__(self, key, **kwargs):
            super().__init__(key, **kwargs)
            self.attempts[0].update(status=404, model="vendor/model", error_kind="provider",
                                    provider_error_body='{"error":"guardrail rejected"}',
                                    provider_error_truncated=False)

    class RejectedAdapter(Adapter):
        def run(self, *args):
            raise RuntimeError('404 "Upstream request failed"')

    result = run_campaign(spec(tmp_path), Store(tmp_path / "artifacts"), api_key="provider-key",
                          runtime_factory=Runtime, gateway_factory=RejectedGateway,
                          adapter_factory=RejectedAdapter, notify=lambda _: None)[0]
    assert result["status"] == "agent_failed"
    assert result["provider_errors"] == [{"attempt_id": "one", "status": 404,
                                           "model": "vendor/model",
                                           "body": '{"error":"guardrail rejected"}',
                                           "truncated": False}]


def test_campaign_exports_copy_workspace_before_container_cleanup(tmp_path):
    runtime = Runtime()
    result = run_campaign(spec(tmp_path), Store(tmp_path / "artifacts"), api_key="provider-key",
                          runtime_factory=lambda: runtime, gateway_factory=Gateway,
                          adapter_factory=Adapter, notify=lambda _: None)[0]
    assert result["workspace_transport"] == "docker_cp"
    assert any("Docker bind mounts could not read" in warning for warning in result["warnings"])
    assert runtime.lifecycle.index(("export", "0")) < runtime.lifecycle.index(("stop", "0"))
    assert runtime.lifecycle.index(("stop", "0")) < runtime.lifecycle.index(("remove", "0"))


def test_copy_back_failure_marks_infrastructure_failure_and_skips_evaluation(tmp_path):
    class FailingCopyRuntime(Runtime):
        def export_workspace(self, identity, workspace):
            raise RuntimeError("docker cp export failed")

    result = run_campaign(spec(tmp_path), Store(tmp_path / "artifacts"), api_key="provider-key",
                          runtime_factory=FailingCopyRuntime, gateway_factory=Gateway,
                          adapter_factory=Adapter, notify=lambda _: None)[0]
    assert result["status"] == "infrastructure_failed"
    assert result["infrastructure_incidents"] is True
    assert "Workspace copy-back failed" in result["error"]
    assert "evaluation" not in result


def test_manual_stop_still_evaluates_workspace(tmp_path):
    class Stopped(Adapter):
        def run(self, prompt, model, on_event, ask, stop):
            super().run(prompt, model, on_event, ask, stop)
            stop.set()

    result = run_campaign(spec(tmp_path), Store(tmp_path / "a"), api_key="key", runtime_factory=Runtime,
                          gateway_factory=Gateway, adapter_factory=Stopped, notify=lambda _: None)[0]
    assert result["status"] == "incomplete"
    assert result["evaluation"]["score_percent"] == 100


def test_network_interrupted_run_retains_evaluation_without_ranking(tmp_path):
    class NetworkGateway(Gateway):
        def __enter__(self):
            self.infrastructure_failed.set()
            return self

    class Failed(Adapter):
        def run(self, *args):
            raise RuntimeError("provider connection interrupted")

    result = run_campaign(spec(tmp_path), Store(tmp_path / "a"), api_key="key", runtime_factory=Runtime,
                          gateway_factory=NetworkGateway, adapter_factory=Failed, notify=lambda _: None)[0]
    assert result["status"] == "infrastructure_failed"
    assert result["metrics"]["request_count"] == 1
    assert result["evaluation"]["earned_score"] == 0


def test_human_reply_types_preserve_protocol_choices():
    assert coerce_answer("yes", {"method": "confirm"}) is True
    assert coerce_answer("once", {"permission": "bash"}) == "once"
    assert coerce_answer('[["first"]]', {"questions": [{"question": "Choose"}]}) == [["first"]]


def test_source_manifest_is_not_mutated(tmp_path):
    data = spec(tmp_path)
    before = json.dumps(data, sort_keys=True)
    run_campaign(data, Store(tmp_path / "a"), api_key="key", runtime_factory=Runtime,
                 gateway_factory=Gateway, adapter_factory=Adapter, notify=lambda _: None)
    assert json.dumps(data, sort_keys=True) == before


def test_agent_time_limit_stops_and_evaluates(tmp_path):
    class Slow(Adapter):
        def run(self, prompt, model, on_event, ask, stop):
            on_event({"type": "agent_start", "agent_id": "main"})
            assert stop.wait(2), "Agent-time limit was not enforced"

    data = spec(tmp_path)
    data["limits"] = {"agent_seconds": 0.02}
    result = run_campaign(data, Store(tmp_path / "a"), api_key="key", runtime_factory=Runtime,
                          gateway_factory=Gateway, adapter_factory=Slow, notify=lambda _: None)[0]
    assert result["status"] == "incomplete"
    assert result["stop_reason"] == "agent_time_limit"
    assert result["evaluation"]["earned_score"] == 0


def test_cli_human_answers_count_once_and_exclude_wait(tmp_path):
    store = Store(tmp_path / "a")

    class Question(Adapter):
        def run(self, prompt, model, on_event, ask, stop):
            on_event({"type": "agent_start", "agent_id": "main"})
            on_event({"type": "agent_wait", "agent_id": "main"})
            assert ask("Continue?", {"method": "confirm", "agent_id": "main"}) is True
            on_event({"type": "agent_resume", "agent_id": "main"})
            super().run(prompt, model, on_event, ask, stop)

    def notify(text):
        question = json.loads(text)
        time.sleep(.03)
        assert store.answer(question["run_id"], question["request_id"], "yes")

    result = run_campaign(spec(tmp_path), store, api_key="key", runtime_factory=Runtime,
                          gateway_factory=Gateway, adapter_factory=Question, notify=notify)[0]
    assert result["human_interventions"] == 1
    assert result["timing"]["human_wait_seconds"] >= .03


def test_stop_during_setup_still_evaluates(tmp_path):
    store = Store(tmp_path / "a")

    class SetupRuntime(Runtime):
        def exec(self, identity, command, **kwargs):
            if command == ["wait-for-stop"]:
                store.stop(store.list()[0]["id"])
                deadline = time.monotonic() + 2
                while identity not in self.stopped and time.monotonic() < deadline:
                    time.sleep(.01)
                return subprocess.CompletedProcess(command, 1, "", "stopped")
            return super().exec(identity, command, **kwargs)

    data = spec(tmp_path)
    data["tasks"][0]["setup"] = [["wait-for-stop"]]
    result = run_campaign(data, store, api_key="key", runtime_factory=SetupRuntime,
                          gateway_factory=Gateway, adapter_factory=Adapter, notify=lambda _: None)[0]
    assert result["status"] == "incomplete"
    assert result["evaluation"]["earned_score"] == 0


@pytest.mark.parametrize(("system", "machine"), [("Darwin", "arm64"), ("Linux", "aarch64")])
def test_default_docker_runtime_rejects_non_linux_x86_64(tmp_path, monkeypatch, system, machine):
    monkeypatch.setattr("platform.system", lambda: system)
    monkeypatch.setattr("platform.machine", lambda: machine)
    with pytest.raises(ValueError, match="Linux x86_64"):
        run_campaign(spec(tmp_path), Store(tmp_path / "a"), api_key="key")
    assert not (tmp_path / "a" / "runs").exists()


def test_injected_runtime_bypasses_linux_platform_gate(tmp_path, monkeypatch):
    monkeypatch.setattr("platform.system", lambda: "Darwin")
    monkeypatch.setattr("platform.machine", lambda: "arm64")
    result = run_campaign(spec(tmp_path), Store(tmp_path / "a"), api_key="key", runtime_factory=Runtime,
                          gateway_factory=Gateway, adapter_factory=Adapter, notify=lambda _: None)[0]
    assert result["status"] == "completed"
    assert result["evaluation"]["earned_score"] == 5
