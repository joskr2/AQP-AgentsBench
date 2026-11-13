import json
from pathlib import Path
import pytest
from aqp_bench.harness import stage, configure


def test_stage_preserves_selected_files_and_bytes(tmp_path):
    source = tmp_path / 'selected'
    source.mkdir()
    (source / 'AGENTS.md').write_bytes(b'instructions\n')
    home = tmp_path / 'home'
    stage([{'source': str(source), 'target': '.pi/agent/skills'}], home)
    assert (home / '.pi/agent/skills/AGENTS.md').read_bytes() == b'instructions\n'


@pytest.mark.parametrize('target', ['../escape', '/absolute', '.', 'x/../../escape'])
def test_stage_rejects_unsafe_target(tmp_path, target):
    source = tmp_path / 'instructions'
    source.write_text('safe')
    with pytest.raises(ValueError):
        stage([{'source': str(source), 'target': target}], tmp_path / 'home')


def test_stage_rejects_symlinks_and_secrets_before_copy(tmp_path):
    source = tmp_path / 'selected'
    source.mkdir()
    (source / 'auth.json').write_text('{"apiKey":"actual-key"}')
    home = tmp_path / 'home'
    with pytest.raises(ValueError, match='credential|secret'):
        stage([{'source': str(source), 'target': '.pi/agent'}], home)
    assert not home.exists()
    (source / 'auth.json').unlink()
    (source / 'link').symlink_to('/etc/passwd')
    with pytest.raises(ValueError, match='symlink'):
        stage([{'source': str(source), 'target': 'skills'}], home)


@pytest.mark.parametrize('kind,path', [('pi', '.pi/agent/models.json'), ('opencode', '.config/opencode/opencode.json')])
def test_configure_routes_gateway_without_persisting_token(tmp_path, kind, path):
    env = configure(tmp_path, kind, 'org/model', 'http://host.docker.internal:1234/v1', 'ephemeral-token')
    text = (tmp_path / path).read_text()
    assert 'ephemeral-token' not in text
    assert 'http://host.docker.internal:1234/v1' in text
    assert env['AQP_GATEWAY_TOKEN'] == 'ephemeral-token'
    assert env['HOME'] == '/home/agent'
    assert 'org/model' in text


def test_configure_preserves_harness_and_rejects_other_providers(tmp_path):
    root = tmp_path / '.config/opencode'
    root.mkdir(parents=True)
    config = root / 'opencode.json'
    config.write_text(json.dumps({'instructions':['AGENTS.md'], 'plugin':['gentle-ai']}))
    configure(tmp_path, 'opencode', 'org/model', 'http://gateway/v1', 'local')
    assert json.loads(config.read_text())['plugin'] == ['gentle-ai']
    config.write_text('{"provider":{"other":{}}}')
    with pytest.raises(ValueError, match='provider'):
        configure(tmp_path, 'opencode', 'org/model', 'http://gateway/v1', 'local')


def test_stage_rejects_json_secrets_and_configure_symlink_escape(tmp_path):
    source = tmp_path / 'settings.json'
    source.write_text('{"apiKey":"literal-secret"}')
    with pytest.raises(ValueError, match='credential'):
        stage([{'source':str(source),'target':'settings.json'}],tmp_path/'home')
    outside = tmp_path/'outside'
    outside.mkdir()
    home = tmp_path/'home'
    home.mkdir()
    (home/'.pi').symlink_to(outside, target_is_directory=True)
    with pytest.raises(ValueError, match='symlink'):
        configure(home,'pi','org/model','http://gateway/v1','local')
    assert list(outside.iterdir()) == []


def test_unsupported_harness_does_not_write_configuration(tmp_path):
    with pytest.raises(ValueError, match='Unsupported'):
        configure(tmp_path/'home','other','org/model','http://gateway/v1','local')
    assert not (tmp_path/'home').exists()


def test_configure_command_leaves_model_routing_to_the_harness(tmp_path):
    home = tmp_path / 'home'
    env = configure(home, 'command', 'org/model', 'http://gateway/v1', 'local')
    assert env == {'HOME': '/home/agent', 'AQP_GATEWAY_TOKEN': 'local',
                   'AQP_GATEWAY_URL': 'http://gateway/v1'}
    assert not home.exists()
