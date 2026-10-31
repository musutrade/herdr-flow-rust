#!/usr/bin/env python3
"""Best-effort local native-hook receipts. No control output or delivery claim."""
import datetime
import fcntl
import json
import os
import re
import select
import stat
import sys
import time
import uuid

INPUT_LIMIT = 65536
FILE_LIMIT = 256
CLAUDE_ERRORS = frozenset((
    'rate_limit', 'overloaded', 'authentication_failed', 'oauth_org_not_allowed',
    'account_on_hold', 'billing_error', 'invalid_request', 'model_not_found',
    'server_error', 'max_output_tokens', 'cloud_credential_error', 'unknown',
))
EVENTS = {'claude': {'StopFailure'}, 'codex': {'Stop', 'PostModelRequest'},
          'hermes': {'api_request_error', 'on_session_end'}}


class Refused(Exception):
    """Contains only a program-owned diagnostic code."""


def read_input(fd):
    end = time.monotonic() + 1.0
    data = bytearray()
    os.set_blocking(fd, False)
    while True:
        remaining = end - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise Refused('input_timeout')
        chunk = os.read(fd, min(8192, INPUT_LIMIT + 1 - len(data)))
        if not chunk:
            return bytes(data)
        data.extend(chunk)
        if len(data) > INPUT_LIMIT:
            raise Refused('input_too_large')


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise Refused('duplicate_key')
        result[key] = value
    return result


def invalid_constant(_value):
    raise Refused('invalid_json_constant')


def identifier(value):
    if value is None or value == '':
        return None
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9_.:-]{1,128}', value):
        raise Refused('invalid_identity')
    return value


def hermes_fields(payload):
    # bf0203a: shell_hooks._serialize_payload / _payload_fields. Only session
    # is promoted; turn/outcome/request fields live in extra, not at top level.
    extra = payload.get('extra', {})
    if not isinstance(extra, dict):
        raise Refused('invalid_envelope')
    for key in ('turn_id', 'completed', 'failed', 'interrupted', 'api_request_id',
                'turn_exit_reason'):
        if key in payload:
            raise Refused('unexpected_top_level_field')
    if 'session_id' in extra and extra['session_id'] != payload.get('session_id'):
        raise Refused('identity_conflict')
    return extra


def normalize(source, payload):
    if source not in EVENTS or not isinstance(payload, dict):
        raise Refused('invalid_source_or_object')
    event = payload.get('hook_event_name')
    if not isinstance(event, str) or event not in EVENTS[source]:
        raise Refused('unsupported_event')
    fields = hermes_fields(payload) if source == 'hermes' else payload
    session = identifier(payload.get('session_id'))
    turn = None if source == 'claude' else identifier(fields.get('turn_id'))
    error = 'unknown'
    status = 'unknown'
    if source == 'claude':
        scope = 'reported_api_turn_failure'
        status = 'failed'
        if isinstance(payload.get('error'), str) and payload['error'] in CLAUDE_ERRORS:
            error = payload['error']
    elif source == 'codex':
        scope = 'stop_observed' if event == 'Stop' else 'model_request'
        if event == 'Stop':
            status = 'stop_observed'
        elif payload.get('status') in ('completed', 'failed', 'cancelled'):
            status = 'request_' + payload['status']
    else:
        scope = 'attempt_error' if event == 'api_request_error' else 'reported_turn_outcome'
        if event == 'api_request_error':
            status = 'attempt_error'
        else:
            flags = [fields.get(k) for k in ('completed', 'failed', 'interrupted')]
            if session and turn and all(type(v) is bool for v in flags) and sum(flags) == 1:
                status = ('completed', 'failed', 'interrupted')[flags.index(True)]
    return {
        'schema_version': 1, 'receive_id': uuid.uuid4().hex,
        'source': source, 'native_event': event, 'session_id': session,
        'turn_id': turn, 'received_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'event_scope': scope, 'reported_status': status, 'error_class': error,
        'identity_status': 'source_claimed' if session else 'missing_session',
    }


def open_directory(path):
    # Walk from / with dirfd + O_NOFOLLOW: neither existing parent aliases nor
    # a last-component symlink may redirect the configured destination.
    parts = path.split('/')
    if not path.startswith('/') or any(p in ('', '.', '..') for p in parts[1:]):
        raise Refused('invalid_output_path')
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open('/', flags)
    try:
        for i, part in enumerate(parts[1:]):
            if i == len(parts) - 2:
                try:
                    os.mkdir(part, 0o700, dir_fd=fd)
                except FileExistsError:
                    pass
            child = os.open(part, flags, dir_fd=fd)
            os.close(fd)
            fd = child
        info = os.fstat(fd)
        if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise Refused('output_not_private')
        return fd
    except BaseException:
        os.close(fd)
        raise


def store(path, receipt):
    data = (json.dumps(receipt, ensure_ascii=True, sort_keys=True) + '\n').encode()
    if len(data) > 4096:
        raise Refused('output_too_large')
    directory = open_directory(path)
    lock = None
    try:
        lock = os.open('.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC,
                       0o600, dir_fd=directory)
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise Refused('invalid_lock')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        # Count all entries except our lock; no automatic retention deletion.
        with os.scandir(directory) as entries:
            count = 0
            for entry in entries:
                count += entry.name != '.lock'
                if count >= FILE_LIMIT:
                    raise Refused('output_full')
        name = receipt['receive_id'] + '.json'
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o600, dir_fd=directory)
        try:
            with os.fdopen(fd, 'wb') as output:
                output.write(data)
        except BaseException:
            os.unlink(name, dir_fd=directory)
            raise
    finally:
        if lock is not None:
            os.close(lock)
        os.close(directory)


def main():
    try:
        if len(sys.argv) != 5 or sys.argv[1] != '--source' or sys.argv[3] != '--output':
            raise Refused('invalid_arguments')
        raw = read_input(0)
        payload = json.loads(raw, object_pairs_hook=unique_object, parse_constant=invalid_constant)
        store(sys.argv[4], normalize(sys.argv[2], payload))
    except Refused as error:
        diagnostic(str(error))
    except Exception:
        diagnostic('input_or_storage_failed')
    return 0


def diagnostic(code):
    try:
        os.write(2, ('native_hook:' + code + '\n').encode())
    except OSError:
        pass


if __name__ == '__main__':
    sys.exit(main())
