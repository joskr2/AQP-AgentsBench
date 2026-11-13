"""Gateway integration tests use loopback providers, never paid APIs."""
import http.client
import json
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

import pytest

from aqp_bench.gateway import Gateway


@contextmanager
def provider(respond):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            respond(self, body)

        do_GET = do_POST

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f'http://127.0.0.1:{server.server_port}/api/v1'
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def reply(handler, body, status=200):
    data = json.dumps(body).encode()
    handler.send_response(status)
    handler.send_header('Content-Type', 'application/json')
    handler.send_header('Content-Length', str(len(data)))
    handler.end_headers()
    handler.wfile.write(data)


def request(gateway, body=None, path='/v1/chat/completions', token=True, **headers):
    address = urlsplit(gateway.url)
    connection = http.client.HTTPConnection(address.hostname, address.port, timeout=3)
    if token:
        headers['Authorization'] = 'Bearer ' + gateway.token
    connection.request('GET' if body is None else 'POST', path,
                       None if body is None else json.dumps(body), headers=headers)
    response = connection.getresponse()
    status, data = response.status, response.read()
    connection.close()
    return status, data


def attempts(gateway, count=1):
    deadline = time.monotonic() + 3
    records = gateway.attempts
    while (len(records) < count or any(r.get('pending') for r in records)) and time.monotonic() < deadline:
        time.sleep(.005)
        records = gateway.attempts
    assert len(records) == count and not any(r.get('pending') for r in records)
    return records


USAGE = {'prompt_tokens': 12, 'completion_tokens': 3, 'cost': .02,
         'prompt_tokens_details': {'cached_tokens': 4},
         'completion_tokens_details': {'reasoning_tokens': 1}}


def test_forward_account_and_limit():
    seen = []
    def respond(h, body):
        seen.append((h.path, dict(h.headers), json.loads(body)))
        reply(h, {'id': 'gen-1', 'model': 'actual/model', 'usage': USAGE})
    with provider(respond) as upstream, Gateway('private-key', upstream=upstream, cost_limit=.02) as gateway:
        status, body = request(gateway, {'model': 'requested/model'}, **{'X-AQP-Agent-ID': 'child', 'X-AQP-Parent-ID': 'main'})
        assert status == 200
        record = attempts(gateway)[0]
        assert record['agent_id'] == 'child' and record['parent_id'] == 'main'
        assert record['input_tokens'] == 12 and record['output_tokens'] == 3
        assert record['cost_usd'] == .02 and record['usage_complete']
        assert record['cached_tokens'] == 4 and record['reasoning_tokens'] == 1
        assert record['model'] == 'actual/model' and record['provider_request_id'] == 'gen-1'
        assert record['duration_seconds'] >= 0
        assert gateway.limit_reached.is_set()
        assert request(gateway, {'model': 'm'})[0] == 402
        assert len(seen) == 1
        assert seen[0][0] == '/api/v1/chat/completions'
        assert seen[0][1]['Authorization'] == 'Bearer private-key'
        assert 'private-key' not in json.dumps(gateway.attempts)
        record['cost_usd'] = 100
        assert gateway.attempts[0]['cost_usd'] == .02


def test_inflight_attempt_is_visible_with_unknown_billing():
    started, release = threading.Event(), threading.Event()
    def respond(handler, body):
        started.set()
        assert release.wait(3)
        reply(handler, {'usage': USAGE})
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        client = threading.Thread(target=lambda: request(gateway, {'model': 'm'}))
        client.start()
        assert started.wait(2)
        try:
            records = gateway.attempts
            assert len(records) == 1
            assert records[0]['cost_usd'] is None
            assert records[0]['pending'] is True
        finally:
            release.set()
            client.join(3)
        deadline = time.monotonic() + 2
        while gateway.attempts[0].get('pending') and time.monotonic() < deadline:
            time.sleep(.005)
        assert len(gateway.attempts) == 1
        assert gateway.attempts[0]['cost_usd'] == .02


def test_auth_paths_bounds_and_discovery():
    seen = []
    def respond(h, body):
        seen.append(h.path)
        reply(h, {'data': []})
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        assert request(gateway, {}, token=False)[0] == 401
        for path in ['/v1/chat/completions?url=http://bad', '/other', 'http://bad/v1/models']:
            assert request(gateway, {}, path=path)[0] == 404
        address = urlsplit(gateway.url)
        conn = http.client.HTTPConnection(address.hostname, address.port, timeout=3)
        conn.putrequest('POST', '/v1/chat/completions')
        conn.putheader('Authorization', 'Bearer ' + gateway.token)
        conn.putheader('Content-Length', str(8 * 1024 * 1024 + 1))
        conn.endheaders()
        response = conn.getresponse()
        assert response.status == 413
        response.read()
        conn.close()
        assert request(gateway, path='/v1/models')[0] == 200
        assert seen == ['/api/v1/models']
        assert gateway.attempts == []


def test_stream_is_live_and_usage_not_summed():
    release = threading.Event()
    def respond(h, body):
        h.send_response(200)
        h.send_header('Content-Type', 'text/event-stream')
        h.end_headers()
        h.wfile.write(b': keepalive\n\ndata: {"choices":[{"delta":{"content":"hi"}}]}\n\n')
        h.wfile.flush()
        assert release.wait(3)
        data = json.dumps({'id': 'gen-sse', 'model': 'm', 'usage': USAGE}).encode()
        h.wfile.write(b'data: ' + data + b'\n\ndata: ' + data + b'\n\ndata: [DONE]\n\n')
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        address = urlsplit(gateway.url)
        conn = http.client.HTTPConnection(address.hostname, address.port, timeout=2)
        conn.request('POST', '/v1/chat/completions', json.dumps({'model': 'm', 'stream': True}), {'Authorization': 'Bearer ' + gateway.token})
        response = conn.getresponse()
        assert response.readline() == b': keepalive\n'
        release.set()
        assert b'[DONE]' in response.read()
        conn.close()
        record = attempts(gateway)[0]
        assert record['cost_usd'] == .02 and record['input_tokens'] == 12


@pytest.mark.parametrize('status', [400, 429, 503, 302])
def test_provider_errors_and_redirects(status):
    seen = []
    capability = []
    def respond(h, body):
        seen.append(1)
        h.send_response(status)
        h.send_header('Location', 'http://127.0.0.1:1/secret')
        body = json.dumps({'error': {'message': 'guardrail rejected', 'token': 'private-key',
                                     'capability': capability[0]}}).encode()
        h.send_header('Content-Length', str(len(body)))
        h.end_headers()
        h.wfile.write(body)
    with provider(respond) as upstream, Gateway('private-key', upstream=upstream) as gateway:
        capability.append(gateway.token)
        code, body = request(gateway, {'model': 'm'})
        assert code == status
        assert b'guardrail rejected' in body and b'[redacted]' in body
        assert b'private-key' not in body
        assert gateway.token.encode() not in body
        record = attempts(gateway)[0]
        assert record['error_kind'] == 'provider'
        assert 'guardrail rejected' in record['provider_error_body']
        assert 'private-key' not in record['provider_error_body']
        assert gateway.token not in record['provider_error_body']
        assert record['provider_error_truncated'] is False
        assert record['cost_usd'] is None and not record['usage_complete']
        assert gateway.infrastructure_failed.is_set() == (status in (429, 503))
        assert seen == [1]


def test_provider_error_body_is_bounded_and_marks_truncation():
    diagnostic = 'x' * 6000
    def respond(handler, body):
        handler.send_response(400)
        handler.send_header('Content-Type', 'application/json')
        handler.send_header('Content-Length', str(len(diagnostic) + 12))
        handler.end_headers()
        handler.wfile.write(('{"error":"' + diagnostic + '"}').encode())
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        code, body = request(gateway, {'model': 'm'})
        record = attempts(gateway)[0]
        assert code == 400
        assert len(body) <= 4096
        assert len(record['provider_error_body'].encode()) <= 4096
        assert record['provider_error_truncated'] is True
        assert b'truncated' in body


@pytest.mark.parametrize('diagnostic', [b'\x80' * 5000, b'\x00' * 5000])
def test_provider_error_bounds_invalid_utf8_and_json_escaping(diagnostic):
    def respond(handler, body):
        handler.send_response(400)
        handler.send_header('Content-Length', str(len(diagnostic)))
        handler.end_headers()
        handler.wfile.write(diagnostic)
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        code, body = request(gateway, {'model': 'm'})
        record = attempts(gateway)[0]
        assert code == 400
        assert len(body) <= 4096
        assert len(record['provider_error_body'].encode()) <= 4096
        assert record['provider_error_truncated'] is True


def test_provider_error_redacts_credentials_crossing_diagnostic_boundary():
    diagnostic = b'x' * 4090 + b'private-key'
    def respond(handler, body):
        handler.send_response(400)
        handler.send_header('Content-Length', str(len(diagnostic)))
        handler.end_headers()
        handler.wfile.write(diagnostic)
    with provider(respond) as upstream, Gateway('private-key', upstream=upstream) as gateway:
        assert request(gateway, {'model': 'm'})[0] == 400
        record = attempts(gateway)[0]
        assert 'private-key' not in record['provider_error_body']
        assert not record['provider_error_body'].endswith('privat')


def test_missing_usage_and_network_fault():
    with provider(lambda h, b: reply(h, {'choices': []})) as upstream, Gateway('key', upstream=upstream) as gateway:
        assert request(gateway, {'model': 'm'})[0] == 200
        assert attempts(gateway)[0]['cost_usd'] is None
        assert not gateway.limit_reached.is_set()
    # The closed local listening port deterministically refuses a new connection.
    with Gateway('key', upstream=upstream) as gateway:
        assert request(gateway, {'model': 'm'})[0] == 502
        assert attempts(gateway)[0]['error_kind'] == 'network'
        assert gateway.infrastructure_failed.is_set()


@pytest.mark.parametrize('url', ['http://example.com/api/v1', 'https://user:pass@example.com', 'https://example.com/?x=1', 'file:///tmp/x', 'https://example.com/#x'])
def test_unsafe_upstream_rejected(url):
    with pytest.raises(ValueError):
        Gateway('key', upstream=url)


def test_parallel_attempts_and_callback_isolation():
    def broken_callback(record):
        record['cost_usd'] = 999
        raise RuntimeError('callback must not break forwarding')
    with provider(lambda h, b: reply(h, {'usage': USAGE})) as upstream, Gateway('key', upstream=upstream, on_attempt=broken_callback) as gateway:
        threads = [threading.Thread(target=request, args=(gateway, {'model': 'm'})) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        records = attempts(gateway, 8)
        assert len({r['attempt_id'] for r in records}) == 8
        assert all(r['cost_usd'] == .02 for r in records)


def test_truncated_stream_is_infrastructure_not_free_success():
    def respond(h, body):
        h.send_response(200)
        h.send_header('Content-Type', 'text/event-stream')
        h.end_headers()
        h.wfile.write(b'data: {"choices":[]}\n\n')
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        assert request(gateway, {'model': 'm', 'stream': True})[0] == 200
        record = attempts(gateway)[0]
        assert record['error_kind'] == 'network'
        assert record['cost_usd'] is None and not record['usage_complete']
        assert gateway.infrastructure_failed.is_set()


def test_stream_error_event_preserves_billing():
    def respond(h, body):
        h.send_response(200)
        h.send_header('Content-Type', 'text/event-stream')
        h.end_headers()
        data = {'error': {'code': 503}, 'usage': USAGE}
        h.wfile.write(b'data: ' + json.dumps(data).encode() + b'\n\ndata: [DONE]\n\n')
    with provider(respond) as upstream, Gateway('key', upstream=upstream) as gateway:
        request(gateway, {'model': 'm', 'stream': True})
        record = attempts(gateway)[0]
        assert record['error_kind'] == 'provider'
        assert record['cost_usd'] == .02
        assert gateway.infrastructure_failed.is_set()


def test_listen_backlog_covers_the_concurrency_allowance():
    from aqp_bench.gateway import _Server

    # socketserver's default backlog (5) is smaller than the 32 active slots,
    # which caused intermittent client connection resets during bursts.
    assert _Server.request_queue_size >= 64


def test_parallel_burst_above_the_default_backlog_is_served():
    with provider(lambda h, b: reply(h, {'usage': USAGE})) as upstream, Gateway('key', upstream=upstream) as gateway:
        threads = [threading.Thread(target=request, args=(gateway, {'model': 'm'})) for _ in range(16)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        records = attempts(gateway, 16)
        assert len({r['attempt_id'] for r in records}) == 16
