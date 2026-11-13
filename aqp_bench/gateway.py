"""Fixed-target OpenRouter proxy with per-attempt observed billing."""
from __future__ import annotations

import copy
import hmac
import http.client
import ipaddress
import json
import math
import queue
import secrets
import socket
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

MAX_REQUEST = 8 * 1024 * 1024
MAX_RESPONSE = 16 * 1024 * 1024
MAX_PROVIDER_ERROR_BODY = 4096
MAX_SSE_FRAME = 1024 * 1024
TIMEOUT = 30


def _number(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if math.isfinite(value) and value >= 0:
            return value
    return None


class _ClientDisconnect(Exception):
    pass


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False
    # socketserver defaults to a listen backlog of 5, far below the 32 active
    # slots allowed here. Bursts of concurrent completions then get refused by
    # the kernel (observed as connection resets), so accept a full slot wave.
    request_queue_size = 128

    def __init__(self, address, handler):
        self.slots = threading.BoundedSemaphore(32)
        super().__init__(address, handler)

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            request.close()
            return
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()

    def handle_error(self, request, client_address):
        # Never print exception context which could contain provider data.
        pass


class Gateway:
    """One-run gateway. Cost limits are best-effort, not spend reservations."""

    def __init__(self, api_key, *, host='127.0.0.1', port=0,
                 upstream='https://openrouter.ai/api/v1', on_attempt=None,
                 cost_limit=None):
        target = urlsplit(upstream)
        try:
            loopback = ipaddress.ip_address(target.hostname or '').is_loopback
        except ValueError:
            loopback = False
        if (target.scheme not in ('https', 'http') or not target.hostname
                or target.username or target.password or target.query or target.fragment
                or (target.scheme == 'http' and not loopback)
                or any(c in upstream for c in '\r\n\\')
                or '%' in target.netloc):
            raise ValueError('Upstream requires HTTPS, or loopback HTTP for tests, without credentials/query/fragment')
        try:
            target.port
        except ValueError as exc:
            raise ValueError('Invalid upstream port') from exc
        if not isinstance(api_key, str) or not api_key or any(c in api_key for c in '\r\n'):
            raise ValueError('An explicit nonempty API key is required')
        if cost_limit is not None and _number(cost_limit) is None:
            raise ValueError('Cost limit must be finite and nonnegative')
        self._target = target
        self._api_key = api_key
        self._host, self._port = host, port
        self._callback = on_attempt
        self._notifications = queue.Queue(maxsize=128)
        self._cost_limit = cost_limit
        self._known_cost = 0
        self._records = []
        self._pending = {}
        self._lock = threading.Lock()
        self.token = secrets.token_urlsafe(32)
        self.limit_reached = threading.Event()
        self.infrastructure_failed = threading.Event()
        self._server = None
        if cost_limit == 0:
            self.limit_reached.set()

    @property
    def attempts(self):
        with self._lock:
            return copy.deepcopy(self._records + list(self._pending.values()))

    def __enter__(self):
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.0'

            def setup(self):
                super().setup()
                self.connection.settimeout(TIMEOUT)

            def log_message(self, *args):
                pass

            def do_GET(self):
                gateway._handle(self)

            do_POST = do_GET

        self._server = _Server((self._host, self._port), Handler)
        self.url = f'http://{self._host}:{self._server.server_port}/v1'
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        if self._callback:
            def notify():
                while True:
                    record = self._notifications.get()
                    if record is None:
                        return
                    try:
                        self._callback(record)
                    except Exception:
                        pass
            threading.Thread(target=notify, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join()
        if self._callback:
            try:
                self._notifications.put_nowait(None)
            except queue.Full:
                pass

    def _reply(self, handler, status, message):
        body = json.dumps({'error': message}).encode()
        handler.send_response(status)
        handler.send_header('Content-Type', 'application/json')
        handler.send_header('Content-Length', str(len(body)))
        handler.end_headers()
        handler.wfile.write(body)

    def _handle(self, handler):
        auth = handler.headers.get_all('Authorization', [])
        if len(auth) != 1 or not hmac.compare_digest(auth[0].encode(), ('Bearer ' + self.token).encode()):
            self._reply(handler, 401, 'Gateway capability required')
            return
        completion = handler.command == 'POST' and handler.path == '/v1/chat/completions'
        discovery = handler.command == 'GET' and handler.path == '/v1/models'
        if not (completion or discovery):
            self._reply(handler, 404, 'Unsupported gateway route')
            return
        if completion and self.limit_reached.is_set():
            self._reply(handler, 402, 'Run cost limit reached')
            return
        lengths = handler.headers.get_all('Content-Length', [])
        try:
            if handler.headers.get('Transfer-Encoding') or len(lengths) > 1:
                raise ValueError
            length = int(lengths[0]) if lengths else 0
            if length < 0:
                raise ValueError
        except ValueError:
            self._reply(handler, 400, 'Valid Content-Length required')
            return
        if length > MAX_REQUEST:
            self._reply(handler, 413, 'Request exceeds 8 MiB')
            return
        body = handler.rfile.read(length)
        payload = {}
        if completion:
            try:
                payload = json.loads(body)
                if not isinstance(payload, dict) or not isinstance(payload.get('model'), str):
                    raise ValueError
                if len(body) != length:
                    raise ValueError
            except (ValueError, UnicodeDecodeError):
                self._reply(handler, 400, 'JSON object with model required')
                return
        self._forward(handler, body, payload, completion)

    def _observe(self, record, data):
        if not isinstance(data, dict):
            return
        for source, dest in [('id', 'provider_request_id'), ('model', 'model')]:
            if isinstance(data.get(source), str):
                record[dest] = self._safe(data[source])[:512]
        if data.get('error'):
            record['error_kind'] = 'provider'
            code = data['error'].get('code') if isinstance(data['error'], dict) else None
            if code == 429 or (isinstance(code, int) and code >= 500):
                self.infrastructure_failed.set()
        usage = data.get('usage')
        if not isinstance(usage, dict):
            return
        # Each usage frame represents the request total, never an increment.
        for source, dest in [('prompt_tokens', 'input_tokens'),
                             ('completion_tokens', 'output_tokens'), ('cost', 'cost_usd')]:
            record[dest] = _number(usage.get(source))
        for detail, field in [('prompt_tokens_details', 'cached_tokens'),
                              ('completion_tokens_details', 'reasoning_tokens')]:
            values = usage.get(detail)
            if isinstance(values, dict):
                record[field] = _number(values.get(field))
        record['usage_complete'] = all(record[key] is not None for key in
                                       ('input_tokens', 'output_tokens', 'cost_usd'))

    def _safe(self, value):
        return value.replace(self._api_key, '[redacted]').replace(self.token, '[redacted]')

    def _finish(self, record, started):
        record['duration_seconds'] = time.monotonic() - started
        record['pending'] = False
        with self._lock:
            self._pending.pop(record['attempt_id'], None)
            self._records.append(copy.deepcopy(record))
            if record['cost_usd'] is not None:
                self._known_cost += record['cost_usd']
            if self._cost_limit is not None and self._known_cost >= self._cost_limit:
                self.limit_reached.set()
        if self._callback:
            # Notifications are best-effort; snapshots remain authoritative.
            try:
                self._notifications.put_nowait(copy.deepcopy(record))
            except queue.Full:
                pass

    def _write(self, handler, data):
        try:
            handler.wfile.write(data)
            handler.wfile.flush()
        except OSError as exc:
            raise _ClientDisconnect from exc

    def _forward(self, handler, body, payload, completion):
        started = time.monotonic()
        record = dict(attempt_id=uuid.uuid4().hex,
                      agent_id=self._safe(handler.headers.get('X-AQP-Agent-ID', 'unattributed'))[:512],
                      parent_id=None, model=self._safe(payload.get('model', ''))[:512] or None,
                      status=None, error_kind=None, input_tokens=None, output_tokens=None,
                      cost_usd=None, usage_complete=False)
        parent = handler.headers.get('X-AQP-Parent-ID')
        if parent:
            record['parent_id'] = self._safe(parent)[:512]
        connection_type = http.client.HTTPSConnection if self._target.scheme == 'https' else http.client.HTTPConnection
        conn = connection_type(self._target.hostname, self._target.port, timeout=TIMEOUT)
        if completion:
            with self._lock:
                self._pending[record['attempt_id']] = dict(record, pending=True, duration_seconds=0)
        sent = False
        try:
            suffix = '/chat/completions' if completion else '/models'
            conn.request(handler.command, self._target.path.rstrip('/') + suffix,
                         body if completion else None,
                         {'Authorization': 'Bearer ' + self._api_key,
                          'Content-Type': 'application/json', 'Accept-Encoding': 'identity'})
            response = conn.getresponse()
            record['status'] = response.status
            identifier = response.getheader('X-Generation-Id') or response.getheader('X-Request-ID')
            if identifier:
                record['provider_request_id'] = self._safe(identifier)[:512]
            if response.status >= 300:
                record['error_kind'] = 'provider'
                if response.status == 429 or response.status >= 500:
                    self.infrastructure_failed.set()
                secret_lengths = (len(self._api_key), len(self.token))
                longest_secret = max(secret_lengths)
                scan_limit = MAX_PROVIDER_ERROR_BODY + min(longest_secret, MAX_PROVIDER_ERROR_BODY) + 1
                data = response.read(scan_limit)
                truncated = len(data) > MAX_PROVIDER_ERROR_BODY
                if truncated and longest_secret > MAX_PROVIDER_ERROR_BODY:
                    error_body = '[provider error body omitted: credential exceeds diagnostic scan bound]'
                else:
                    error_body = self._safe(data.decode('utf-8', errors='replace'))
                    safe_bytes = error_body.encode('utf-8')
                    if len(safe_bytes) > MAX_PROVIDER_ERROR_BODY:
                        error_body = safe_bytes[:MAX_PROVIDER_ERROR_BODY].decode('utf-8', errors='ignore')
                        truncated = True
                record['provider_error_body'] = error_body
                record['provider_error_truncated'] = truncated
                try:
                    self._observe(record, json.loads(error_body))
                except (ValueError, UnicodeDecodeError):
                    pass
                sent = True
                if truncated:
                    low, high = 0, len(error_body)
                    while low < high:
                        middle = (low + high + 1) // 2
                        candidate = json.dumps({
                            'error': 'Provider error response truncated',
                            'provider_error_body': error_body[:middle],
                        }).encode()
                        if len(candidate) <= MAX_PROVIDER_ERROR_BODY:
                            low = middle
                        else:
                            high = middle - 1
                    client_body = json.dumps({
                        'error': 'Provider error response truncated',
                        'provider_error_body': error_body[:low],
                    }).encode()
                    handler.send_response(response.status)
                    handler.send_header('Content-Type', 'application/json')
                    handler.send_header('Content-Length', str(len(client_body)))
                    handler.send_header('Cache-Control', 'no-store')
                    handler.end_headers()
                    self._write(handler, client_body)
                else:
                    content_type = response.getheader('Content-Type', '').lower()
                    safe_type = 'application/json' if 'application/json' in content_type else 'text/plain; charset=utf-8'
                    client_body = error_body.encode('utf-8')
                    handler.send_response(response.status)
                    handler.send_header('Content-Type', safe_type)
                    handler.send_header('Content-Length', str(len(client_body)))
                    handler.send_header('Cache-Control', 'no-store')
                    handler.end_headers()
                    self._write(handler, client_body)
                return
            content_type = response.getheader('Content-Type', 'application/json')
            handler.send_response(response.status)
            handler.send_header('Content-Type', 'text/event-stream' if 'text/event-stream' in content_type else 'application/json')
            handler.send_header('Cache-Control', 'no-store')
            handler.end_headers()
            sent = True
            if 'text/event-stream' in content_type:
                frame = []
                frame_size = 0
                done = False
                while True:
                    line = response.readline(MAX_SSE_FRAME + 1)
                    if not line:
                        break
                    frame_size += len(line)
                    if frame_size > MAX_SSE_FRAME:
                        raise ValueError('Upstream SSE frame too large')
                    self._write(handler, self._safe(line.decode('utf-8', errors='replace')).encode())
                    if line.strip() == b'':
                        if frame == [b'[DONE]']:
                            done = True
                        elif frame:
                            try:
                                self._observe(record, json.loads(b'\n'.join(frame)))
                            except (ValueError, UnicodeDecodeError):
                                pass
                        frame = []
                        frame_size = 0
                    elif line.startswith(b'data:'):
                        frame.append(line[5:].strip())
                if not done and record['error_kind'] != 'provider':
                    record['error_kind'] = 'network'
                    self.infrastructure_failed.set()
            else:
                data = response.read(MAX_RESPONSE + 1)
                if len(data) > MAX_RESPONSE:
                    raise ValueError('Upstream response too large')
                try:
                    self._observe(record, json.loads(data))
                except (ValueError, UnicodeDecodeError):
                    record['error_kind'] = 'unknown'
                self._write(handler, self._safe(data.decode('utf-8', errors='replace')).encode())
        except _ClientDisconnect:
            record['error_kind'] = 'client_disconnect'
        except (OSError, http.client.HTTPException):
            record['error_kind'] = 'network'
            self.infrastructure_failed.set()
            if not sent:
                try:
                    self._reply(handler, 502, 'Upstream connection failed')
                except OSError:
                    pass
        except ValueError:
            record['error_kind'] = 'unknown'
        finally:
            conn.close()
            if completion:
                self._finish(record, started)
