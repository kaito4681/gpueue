#!/usr/bin/env python3
"""Tiny daemon stand-in for the helper end-to-end test.

Usage: client.py <socket> <script.json>

The script is a JSON list of steps:
  {"send": <request object without id>, "expect": {"result": "..."}}
  {"event": {"event": "...", ...}, "timeout": 30}   wait for a matching event
  {"sleep": 1.5}
Matching is by subset: every key in the expectation must equal the
received value (recursively). Events received while waiting for responses
are kept for later "event" steps. Exits 1 on the first mismatch.
"""
import json
import socket
import sys
import time


def subset(want, got):
    if isinstance(want, dict):
        return isinstance(got, dict) and all(k in got and subset(v, got[k]) for k, v in want.items())
    if isinstance(want, list):
        return isinstance(got, list) and len(want) == len(got) and all(map(subset, want, got))
    return want == got


class Conn:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.buf = b""
        self.events = []
        self.next_id = 1
        self.send({"type": "hello", "protocol_version": 1, "binary_version": "test"})
        hello = self.read(10)
        if hello is None or hello.get("type") != "hello":
            raise SystemExit(f"no hello: {hello}")

    def send(self, msg):
        self.s.sendall(json.dumps(msg).encode() + b"\n")

    def read(self, timeout):
        self.s.settimeout(timeout)
        while b"\n" not in self.buf:
            try:
                chunk = self.s.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                return None
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def request(self, req, timeout=60):
        rid = self.next_id
        self.next_id += 1
        self.send({"type": "request", "id": rid, **req})
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = self.read(max(0.1, deadline - time.time()))
            if msg is None:
                raise SystemExit(f"no response to {req}")
            if msg["type"] == "event":
                self.events.append(msg)
            elif msg["type"] == "response" and msg["id"] == rid:
                return msg
        raise SystemExit(f"timeout waiting for response to {req}")

    def wait_event(self, want, timeout):
        deadline = time.time() + timeout
        while True:
            for i, ev in enumerate(self.events):
                if subset(want, ev):
                    return self.events.pop(i)
            left = deadline - time.time()
            if left <= 0:
                raise SystemExit(f"event {want} not seen; have {self.events}")
            msg = self.read(left)
            if msg is not None and msg["type"] == "event":
                self.events.append(msg)


def main():
    conn = Conn(sys.argv[1])
    with open(sys.argv[2]) as f:
        steps = json.load(f)
    for step in steps:
        if "send" in step:
            resp = conn.request(step["send"], step.get("timeout", 60))
            print("->", json.dumps(step["send"])[:120], "\n<-", json.dumps(resp))
            if not subset(step.get("expect", {}), resp):
                raise SystemExit(f"unexpected response, wanted {step.get('expect')}")
        elif "event" in step:
            ev = conn.wait_event(step["event"], step.get("timeout", 30))
            print("event", json.dumps(ev))
            if "ack" in step:
                conn.request({"op": "ack_events", "up_to": ev["seq"]})
        elif "sleep" in step:
            time.sleep(step["sleep"])
    print("OK")


if __name__ == "__main__":
    main()
