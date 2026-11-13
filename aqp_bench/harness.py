"""Selected configuration staging, without host credential inheritance."""
from __future__ import annotations

import json
from pathlib import Path, PurePosixPath
import re
import shutil
from urllib.parse import urlsplit

_SECRET_NAMES = {'auth.json', '.env', 'credentials', 'credentials.json', 'id_rsa', 'id_ed25519'}
_SECRET_KEYS = {'apikey', 'api_key', 'token', 'password', 'secret', 'access_token', 'refresh_token'}


def _check_file(path):
    if path.is_symlink():
        raise ValueError('Harness symlinks are unsupported; select real files')
    if path.name.lower() in _SECRET_NAMES or path.name.startswith('.env.'):
        raise ValueError('Harness credential/secret files must not be inherited')
    if not path.is_file():
        raise ValueError('Harness entries must be regular files or directories')
    data = path.read_bytes()
    if b'PRIVATE KEY-----' in data or re.search(rb'\bsk-[A-Za-z0-9_-]{16,}', data):
        raise ValueError('Harness secret-bearing files must not be inherited')
    if path.suffix == '.json':
        try:
            value = json.loads(data)
        except (ValueError, UnicodeError):
            raise ValueError('Harness JSON must be valid UTF-8 JSON') from None
        def inspect(item):
            if isinstance(item, dict):
                for key, child in item.items():
                    if key.lower() in _SECRET_KEYS and child:
                        if not isinstance(child, str) or not (child.startswith('{env:') or child.startswith('$')):
                            raise ValueError('Harness contains literal credentials; use explicit environment references')
                    inspect(child)
            elif isinstance(item, list):
                for child in item:
                    inspect(child)
        inspect(value)


def stage(files: list[dict], destination: Path) -> None:
    """Validate every source before copying; directory symlinks are rejected too."""
    destination = Path(destination)
    planned = []
    for entry in files:
        relative = PurePosixPath(entry['target'])
        if relative.is_absolute() or not relative.parts or any(p in ('..', '') for p in relative.parts) or '\\' in entry['target']:
            raise ValueError('Harness targets must be safe nonempty relative paths')
        target = destination / str(relative)
        if not target.resolve().is_relative_to(destination.resolve()):
            raise ValueError('Harness target escapes through a symlink')
        source = Path(entry['source'])
        if source.is_symlink():
            raise ValueError('Harness symlinks are unsupported')
        if source.is_dir():
            for child in source.rglob('*'):
                if child.is_symlink():
                    raise ValueError('Harness symlinks are unsupported')
                if not child.is_dir():
                    _check_file(child)
        else:
            _check_file(source)
        if target.exists() or any(target == old or target in old.parents or old in target.parents for _, old in planned):
            raise ValueError('Harness targets must not overlap existing or selected files')
        planned.append((source, target))
    for source, target in planned:
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, target)
        else:
            shutil.copy2(source, target)


def configure(home: Path, kind: str, model: str, gateway_url: str, token: str) -> dict[str, str]:
    """Add one benchmark provider while preserving non-provider harness settings."""
    home = Path(home)
    url = urlsplit(gateway_url)
    if url.scheme not in ('http', 'https') or not url.hostname or url.username or url.password or url.query or url.fragment or url.path.rstrip('/') != '/v1':
        raise ValueError('Gateway URL must be an absolute HTTP(S) /v1 endpoint')
    env = {'HOME':'/home/agent', 'AQP_GATEWAY_TOKEN':token}
    if kind == 'pi':
        path = home / '.pi/agent/models.json'
        provider = {'baseUrl':gateway_url, 'api':'openai-completions', 'apiKey':'$AQP_GATEWAY_TOKEN',
                    'models':[{'id':model, 'name':model}]}
        generated = {'providers':{'aqp':provider}}
        env['PI_CODING_AGENT_DIR'] = '/home/agent/.pi/agent'
    elif kind == 'opencode':
        path = home / '.config/opencode/opencode.json'
        provider = {'npm':'@ai-sdk/openai-compatible', 'name':'Benchmark gateway',
                    'options':{'baseURL':gateway_url, 'apiKey':'{env:AQP_GATEWAY_TOKEN}'}, 'models':{model:{'name':model}}}
        generated = {'provider':{'aqp':provider}, 'enabled_providers':['aqp'], 'model':f'aqp/{model}', 'small_model':f'aqp/{model}'}
        env.update({'XDG_CONFIG_HOME':'/home/agent/.config', 'XDG_DATA_HOME':'/home/agent/.local/share',
                    'OPENCODE_CONFIG':'/home/agent/.config/opencode/opencode.json', 'OPENCODE_DISABLE_AUTOUPDATE':'true'})
    elif kind == 'command':
        # A generic command harness owns its model routing; generating Pi/OpenCode
        # provider files would be wrong. It still needs the container-facing
        # gateway endpoint and capability to account provider attempts.
        env['AQP_GATEWAY_URL'] = gateway_url
        return env
    else:
        raise ValueError(f'Unsupported agent kind: {kind}')
    # No JSONC parser or implicit credential import: reject ambiguous alternatives.
    for conflict in (home / '.config/opencode/opencode.jsonc', home / '.local/share/opencode/auth.json', home / '.pi/agent/auth.json'):
        if conflict.exists():
            raise ValueError('Remove incompatible credential/JSONC configuration from selected harness')
    for ancestor in (path, *path.parents):
        # Host layout above the selected home (for example /var on macOS) is not
        # part of the untrusted destination; home and everything below it is.
        if ancestor.is_symlink():
            raise ValueError('Configuration destination cannot contain symlinks')
        if ancestor == home:
            break
    existing = json.loads(path.read_text()) if path.exists() else {}
    _validate_models(existing, model, kind)
    key = 'providers' if kind == 'pi' else 'provider'
    if existing.get(key) and existing[key] != generated[key]:
        raise ValueError('Selected harness provider configuration conflicts with mandatory benchmark gateway')
    if kind == 'opencode':
        for name in ('model', 'small_model', 'enabled_providers'):
            if name in existing and existing[name] != generated[name]:
                raise ValueError(f'Selected harness {name} conflicts with mandatory benchmark provider')
    if kind == 'pi':
        settings_path = home / '.pi/agent/settings.json'
        if settings_path.is_symlink():
            raise ValueError('Configuration destination cannot contain symlinks')
        settings = json.loads(settings_path.read_text()) if settings_path.exists() else {}
        defaults = {'defaultProvider':'aqp', 'defaultModel':model, 'enabledModels':[f'aqp/{model}']}
        for name, value in defaults.items():
            if name in settings and settings[name] != value:
                raise ValueError(f'Selected harness {name} conflicts with benchmark provider')
        settings.update(defaults)
    else:
        # Inline overrides win over repository-level provider/model defaults.
        # This does not sandbox arbitrary plugin programs or their own clients.
        env['OPENCODE_CONFIG_CONTENT'] = json.dumps(generated)
    existing.update(generated)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(existing, indent=2) + '\n')
    if kind == 'pi':
        settings_path.write_text(json.dumps(settings, indent=2) + '\n')
    return env


def _validate_models(value, model, kind):
    if isinstance(value, dict):
        for key, child in value.items():
            if key in ('model', 'small_model') and isinstance(child, str) and child not in (model, f'aqp/{model}'):
                raise ValueError('Selected harness model conflicts with benchmark provider')
            if key == 'defaultProvider' and child != 'aqp':
                raise ValueError('Selected harness provider conflicts with benchmark gateway')
            _validate_models(child, model, kind)
    elif isinstance(value, list):
        for child in value:
            _validate_models(child, model, kind)


def validate_workspace(workspace: Path, kind: str, model: str) -> None:
    """Reject known project-level routing overrides before running the agent.

    This is configuration validation, NOT a sandbox for executable extensions.
    """
    if kind == 'command':
        # No known project-level routing format exists for an arbitrary harness;
        # validate what is knowable instead of inventing Pi/OpenCode rules.
        return
    root = Path(workspace)
    candidates = ([root/'opencode.json', root/'opencode.jsonc'] if kind == 'opencode'
                  else [root/'.pi/settings.json', root/'.pi/models.json'])
    directory = root / ('.opencode' if kind == 'opencode' else '.pi')
    if directory.exists():
        candidates += list(directory.rglob('*.json')) + list(directory.rglob('*.jsonc'))
        for path in directory.rglob('*.md'):
            # Agent definitions with fixed external models cannot be preserved
            # honestly while routing every child through the benchmark provider.
            text = path.read_text()
            if text.startswith('---'):
                front = text.split('---', 2)
                if len(front) > 2:
                    for match in re.finditer(r'^model:\s*([^\n]+)', front[1], re.MULTILINE):
                        if match.group(1).strip().strip('"\'') != f'aqp/{model}':
                            raise ValueError('Project agent model conflicts with benchmark provider')
    for path in set(candidates):
        if path.exists():
            if path.is_symlink():
                raise ValueError('Project provider configuration cannot be a symlink')
            if path.suffix == '.jsonc':
                raise ValueError('Project JSONC routing cannot be validated; use explicit JSON')
            config = json.loads(path.read_text())
            _validate_models(config, model, kind)
            if config.get('provider') or config.get('providers'):
                raise ValueError('Project provider configuration conflicts with mandatory benchmark gateway')
