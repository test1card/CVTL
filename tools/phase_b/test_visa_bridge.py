#!/usr/bin/env python3
"""Unit tests for the VISA bridge framing. No hardware is touched."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from visa_bridge import BridgeError, candidates, handle_request  # noqa: E402


class FakeInstrument:
    def __init__(self, answer: str = "+298.02\r\n") -> None:
        self.answer = answer
        self.written: list[str] = []

    def write(self, command: str) -> None:
        self.written.append(command)

    def read(self) -> str:
        return self.answer


class BridgeFramingTest(unittest.TestCase):
    def test_write_is_acknowledged(self) -> None:
        inst = FakeInstrument()
        self.assertEqual(
            handle_request(inst, "W smua.source.output = smua.OUTPUT_OFF"),
            b"OK\n",
        )
        self.assertEqual(inst.written, ["smua.source.output = smua.OUTPUT_OFF"])

    def test_query_is_length_prefixed_and_terminator_is_stripped(self) -> None:
        inst = FakeInstrument("+298.02\r\n")
        self.assertEqual(
            handle_request(inst, "Q KRDG? 1"),
            b"R 7\n+298.02\n",
        )
        self.assertEqual(inst.written, ["KRDG? 1"])

    def test_usbtmc_style_newline_is_stripped(self) -> None:
        self.assertEqual(
            handle_request(FakeInstrument("hello\n"), "Q *IDN?"),
            b"R 5\nhello\n",
        )

    def test_empty_and_unknown_requests_are_rejected(self) -> None:
        for request in ("", "X foo", "W", "Q"):
            with self.subTest(request=request):
                with self.assertRaises(BridgeError):
                    handle_request(FakeInstrument(), request)

    def test_non_ascii_and_overlong_commands_are_rejected(self) -> None:
        with self.assertRaises(BridgeError):
            handle_request(FakeInstrument(), "W KRDG? 1\n")
        with self.assertRaises(BridgeError):
            handle_request(FakeInstrument(), "W " + "x" * 1100)
        with self.assertRaises(BridgeError):
            handle_request(FakeInstrument(), "W температура")

    def test_serial_variants_are_both_tried(self) -> None:
        values = candidates("USB0::0x05E6::0x2604::4083236::0::INSTR")
        self.assertIn("USB0::0x05E6::0x2604::4083236\x00::0::INSTR", values)
        values = candidates("USB0::0x05E6::0x2604::4083236\x00::0::INSTR")
        self.assertIn("USB0::0x05E6::0x2604::4083236::0::INSTR", values)


if __name__ == "__main__":
    unittest.main()
