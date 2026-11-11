#!/usr/bin/env python3
"""Loopback bridge between the CVTL Rust transport and PyVISA.

Protocol (one request per connection line):

    client -> bridge   W <command>\\n   |  Q <command>\\n
    bridge -> client   OK\\n            |  R <length>\\n<payload>\\n  |  ERR <text>\\n

The bridge binds to loopback only, serves one client at a time, accepts one
resource from the command line and never evaluates anything it receives.
It is a Phase B bring-up tool, not part of the Phase A build.
"""

from __future__ import annotations

import argparse
import json
import re
import socket
import sys

import pyvisa

MAX_COMMAND = 1024
MAX_RESPONSE = 1024
DRAIN_MS = 80
USB_RE = re.compile(
    r"USB\d*::(?P<vendor>0[xX][0-9A-Fa-f]+)::(?P<product>0[xX][0-9A-Fa-f]+)::"
    r"(?P<serial>.*?)::(?P<interface>\d+)::INSTR"
)


class BridgeError(RuntimeError):
    """A request that the bridge refuses without touching the instrument."""


def one_line(value: object) -> str:
    return " ".join(str(value).split())[:240]


def drain(inst: pyvisa.Resource) -> int:
    """Discard unread USBTMC messages left by an earlier session."""
    inst.timeout = DRAIN_MS
    count = 0
    while True:
        try:
            inst.read()
        except pyvisa.errors.VisaIOError:
            break
        count += 1
    return count


def candidates(resource: str) -> list[str]:
    values = [resource]
    match = USB_RE.fullmatch(resource)
    if match:
        serial = match.group("serial")
        for variant in (serial.rstrip("\x00"), serial + "\x00"):
            value = (
                f"USB0::{match.group('vendor')}::{match.group('product')}::"
                f"{variant}::{match.group('interface')}::INSTR"
            )
            if value not in values:
                values.append(value)
    return values


def open_instrument(resource: str, timeout_ms: int):
    manager = pyvisa.ResourceManager("@py")
    last: Exception | None = None
    for candidate in candidates(resource):
        try:
            inst = manager.open_resource(candidate)
            inst.timeout = timeout_ms
            if candidate.upper().startswith("USB"):
                inst.read_termination = None
                inst.write_termination = "\n"
                drain(inst)
                inst.timeout = timeout_ms
            else:
                inst.read_termination = "\n"
                inst.write_termination = "\n"
            idn = inst.query("*IDN?").strip()
            return manager, inst, candidate, idn
        except Exception as error:  # noqa: BLE001 - the next candidate may work
            last = error
    try:
        manager.close()
    except Exception:
        pass
    raise RuntimeError(f"не удалось открыть {resource}: {last}") from last


def handle_request(inst, line: str) -> bytes:
    if not line:
        raise BridgeError("пустой запрос")
    if len(line) > MAX_COMMAND:
        raise BridgeError("слишком длинная команда")
    if not line.isascii() or any(ord(char) < 0x20 or ord(char) > 0x7E for char in line):
        raise BridgeError("команда должна быть печатным ASCII без окончания")
    if line.startswith("W "):
        inst.write(line[2:])
        return b"OK\n"
    if line.startswith("Q "):
        inst.write(line[2:])
        data = inst.read()
        if isinstance(data, str):
            data = data.encode("ascii")
        data = data.rstrip(b"\r\n")
        if len(data) > MAX_RESPONSE:
            raise BridgeError("слишком длинный ответ прибора")
        return b"R %d\n" % len(data) + data + b"\n"
    raise BridgeError("неизвестный запрос")


def safe_send(conn: socket.socket, payload: bytes) -> None:
    try:
        conn.sendall(payload)
    except OSError:
        pass


def handle_client(conn: socket.socket, inst) -> None:
    reader = conn.makefile("rb")
    try:
        while True:
            raw = reader.readline(MAX_COMMAND + 2)
            if not raw:
                return
            try:
                line = raw.rstrip(b"\r\n").decode("ascii")
            except UnicodeDecodeError as error:
                raise BridgeError("не ASCII") from error
            response = handle_request(inst, line)
            safe_send(conn, response)
    finally:
        reader.close()


def serve(inst, manager, resource: str, host: str, port: int, timeout_ms: int) -> None:
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((host, port))
    server.listen(1)
    actual_port = server.getsockname()[1]
    print(
        json.dumps(
            {
                "resource": resource,
                "host": host,
                "port": actual_port,
                "backend": str(manager),
            },
            ensure_ascii=False,
        ),
        flush=True,
    )
    print(f"bridge: {resource} on {host}:{actual_port}", file=sys.stderr, flush=True)
    try:
        while True:
            conn, address = server.accept()
            if not address[0].startswith("127.") and address[0] != "::1":
                conn.close()
                continue
            conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            try:
                handle_client(conn, inst)
            except BridgeError as error:
                safe_send(conn, f"ERR {one_line(error)}\n".encode("ascii", "replace"))
            except Exception as error:  # noqa: BLE001 - boundary is unknown, reopen
                safe_send(
                    conn,
                    f"ERR {one_line(type(error).__name__ + ': ' + str(error))}\n".encode(
                        "ascii", "replace"
                    ),
                )
                conn.close()
                try:
                    inst.close()
                except Exception:
                    pass
                try:
                    manager.close()
                except Exception:
                    pass
                manager, inst, resource, _ = open_instrument(resource, timeout_ms)
                print(f"bridge: reopened {resource}", file=sys.stderr, flush=True)
                continue
            finally:
                conn.close()
    finally:
        try:
            inst.close()
        finally:
            server.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="CVTL VISA loopback bridge")
    parser.add_argument("--resource", required=True, help="VISA resource, e.g. GPIB0::12::INSTR")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--timeout-ms", type=int, default=5000)
    parser.add_argument("--check", action="store_true", help="open, query *IDN?, print, exit")
    args = parser.parse_args()
    if args.host not in {"127.0.0.1", "::1", "localhost"}:
        parser.error("bridge binds to loopback only")
    if not (1 <= args.timeout_ms <= 120_000):
        parser.error("timeout-ms must be 1..120000")

    manager, inst, resource, idn = open_instrument(args.resource, args.timeout_ms)
    try:
        if args.check:
            print(
                json.dumps(
                    {"resource": resource, "idn": idn, "backend": str(manager)},
                    ensure_ascii=False,
                )
            )
            return 0
        serve(inst, manager, resource, args.host, args.port, args.timeout_ms)
    except KeyboardInterrupt:
        return 130
    finally:
        try:
            inst.close()
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
