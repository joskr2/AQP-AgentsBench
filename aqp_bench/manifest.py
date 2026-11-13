"""Validate portable JSON campaign manifests before execution side effects."""

import copy
import itertools
import json
import math
import re
from pathlib import Path, PurePosixPath
from urllib.parse import urlparse


def relative_path(value):
    if not isinstance(value, str) or not value or "\\" in value:
        raise ValueError("Expected a nonempty relative POSIX path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(p in ("..", ".git") for p in path.parts) or str(path) == ".":
        raise ValueError(f"Unsafe relative path: {value}")
    return str(path)


def _keys(obj, allowed, required=()):
    if not isinstance(obj, dict) or set(obj) - set(allowed) or set(required) - set(obj):
        raise ValueError(f"Expected object with keys {sorted(allowed)}; required {sorted(required)}")


def _text(value, name):
    if not isinstance(value, str) or not value.strip() or "\x00" in value:
        raise ValueError(f"{name} must be a nonempty string")
    return value


def _number(value, name, integer=False, positive=True):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be numeric")
    if not math.isfinite(value) or (positive and value <= 0) or (not positive and value < 0):
        raise ValueError(f"{name} must be finite and {'positive' if positive else 'nonnegative'}")
    if integer and not isinstance(value, int):
        raise ValueError(f"{name} must be an integer")


def _command(command):
    if not isinstance(command, list) or not command:
        raise ValueError("Commands must be nonempty argv arrays, not shell strings")
    for item in command:
        _text(item, "command argument")


def _items(items, name):
    if not isinstance(items, list) or not items:
        raise ValueError(f"{name} must be a nonempty array")
    seen = set()
    for item in items:
        identifier = item.get("id") if isinstance(item, dict) else None
        if not isinstance(identifier, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", identifier):
            raise ValueError(f"Invalid {name} id")
        if identifier in seen:
            raise ValueError(f"Duplicate {name} id: {identifier}")
        seen.add(identifier)


def _files(files, base):
    if not isinstance(files, list):
        raise ValueError("files must be an array")
    targets = []
    for entry in files:
        _keys(entry, {"source", "target"}, {"source", "target"})
        target = relative_path(entry["target"])
        if any(target == old or target.startswith(old + "/") or old.startswith(target + "/") for old in targets):
            raise ValueError("File targets must not overlap")
        targets.append(target)
        source = (base / _text(entry["source"], "source")).absolute()
        if not source.exists() or source.is_symlink():
            raise ValueError(f"Missing source or symlink source: {source}")
        entry.update(source=str(source), target=target)


def load_campaign(path):
    path = Path(path).absolute()
    try:
        data = json.loads(path.read_text(), parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"Cannot load campaign: {exc}") from exc
    _keys(data, {"version", "tasks", "agents", "models", "harnesses", "trials", "concurrency", "limits"},
          {"version", "tasks", "agents", "models"})
    if type(data["version"]) is not int or data["version"] != 1:
        raise ValueError("Only manifest version 1 is supported")
    data.setdefault("trials", 1)
    data.setdefault("concurrency", 1)
    data.setdefault("limits", {})
    data.setdefault("harnesses", [{"id": "none", "mode": "explicit", "files": []}])
    for key in ("trials", "concurrency"):
        _number(data[key], key, integer=True)
    _keys(data["limits"], {"agent_seconds", "cost_usd"})
    for key, value in data["limits"].items():
        _number(value, key)
    if not isinstance(data["models"], list) or not data["models"]:
        raise ValueError("models must be a nonempty array")
    for model in data["models"]:
        _text(model, "model")
        if "/" not in model:
            raise ValueError("Use OpenRouter model IDs in provider/model form")
    if len(set(data["models"])) != len(data["models"]):
        raise ValueError("Duplicate models")
    for name in ("tasks", "agents", "harnesses"):
        _items(data[name], name)
    for agent in data["agents"]:
        _keys(agent, {"id", "kind", "command"}, {"id", "kind"})
        if agent["kind"] not in ("pi", "opencode", "command"):
            raise ValueError("Supported live agents: pi, opencode, command")
        if agent["kind"] == "command" and "command" not in agent:
            raise ValueError("command agents require a nonempty argv 'command'")
        if "command" in agent:
            _command(agent["command"])
    for harness in data["harnesses"]:
        _keys(harness, {"id", "mode", "files"}, {"id", "mode"})
        if harness["mode"] not in ("explicit", "inherit"):
            raise ValueError("Harness mode must be explicit or inherit")
        harness.setdefault("files", [])
        _files(harness["files"], path.parent)
    for task in data["tasks"]:
        _keys(task, {"id", "prompt", "image", "setup", "repository", "attachments", "evaluation"},
              {"id", "prompt", "image", "evaluation"})
        _text(task["prompt"], "prompt")
        _text(task["image"], "image")
        task.setdefault("setup", [])
        if not isinstance(task["setup"], list):
            raise ValueError("setup must be an array of argv arrays")
        for command in task["setup"]:
            _command(command)
        if "repository" in task:
            repo = task["repository"]
            _keys(repo, {"source", "revision"}, {"source", "revision"})
            source = _text(repo["source"], "repository source")
            revision = _text(repo["revision"], "revision")
            if revision.startswith("-"):
                raise ValueError("Invalid repository revision")
            if urlparse(source).scheme in ("https", "http"):
                if not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", revision):
                    raise ValueError("Remote repository revisions must be full commit hashes")
                if urlparse(source).username or urlparse(source).password:
                    raise ValueError("Do not embed credentials in repository URLs")
            else:
                local = (path.parent / source).absolute()
                if not local.is_dir():
                    raise ValueError("Repository must be a local directory or HTTP(S) Git URL")
                repo["source"] = str(local)
        task.setdefault("attachments", [])
        _files(task["attachments"], path.parent)
        evaluation = task["evaluation"]
        _keys(evaluation, {"image", "files", "tests", "timeout_seconds"}, {"tests"})
        evaluation.setdefault("image", task["image"])
        evaluation.setdefault("timeout_seconds", 60)
        evaluation.setdefault("files", [])
        _text(evaluation["image"], "evaluation image")
        _number(evaluation["timeout_seconds"], "evaluation timeout")
        _files(evaluation["files"], path.parent)
        _items(evaluation["tests"], "tests")
        for test in evaluation["tests"]:
            _keys(test, {"id", "score", "command"}, {"id", "score", "command"})
            _number(test["score"], "test score", positive=False)
            _command(test["command"])
        if not sum(test["score"] for test in evaluation["tests"]):
            raise ValueError("Evaluation must have positive total score")
    return data


def expand_runs(campaign):
    return [{"task": copy.deepcopy(task), "agent": copy.deepcopy(agent), "model": model,
             "harness": copy.deepcopy(harness), "trial": trial, "limits": dict(campaign["limits"])}
            for task, agent, model, harness, trial in itertools.product(
                campaign["tasks"], campaign["agents"], campaign["models"], campaign["harnesses"],
                range(1, campaign["trials"] + 1))]
