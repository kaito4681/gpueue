#!/usr/bin/env python3
"""Tiny daemon stand-in for the helper end-to-end test.

Usage: client.py <socket> <script.json>

The script is a JSON list of steps:
  {"send": <request object without id>, "expect": {"result": "..."}}
  {"event": {"event": "...", ...}, "timeout": 30}   wait for a matching event
  {"sleep": 1.5}
  {"batch": [<request>, ...], "expect": {...}, "save": "<file>"}
      send every request before reading any response; each response must
      match "expect" (if given); "save" writes the responses as a JSON list
      (a connection that breaks leaves null for the unanswered ones)
  {"events": {"event": "...", ...}, "count": N, "timeout": 30, "ack": true}
      wait for N matching events
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
        self.responses = {}
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

    def send_request(self, req):
        rid = self.next_id
        self.next_id += 1
        self.send({"type": "request", "id": rid, **req})
        return rid

    def response(self, rid, timeout=60):
        """The response to request rid, or None if the connection ends."""
        deadline = time.time() + timeout
        while rid not in self.responses:
            left = deadline - time.time()
            if left <= 0:
                raise SystemExit(f"timeout waiting for response {rid}")
            msg = self.read(left)
            if msg is None:
                return None
            if msg["type"] == "event":
                self.events.append(msg)
            elif msg["type"] == "response":
                self.responses[msg["id"]] = msg
        return self.responses.pop(rid)

    def request(self, req, timeout=60):
        resp = self.response(self.send_request(req), timeout)
        if resp is None:
            raise SystemExit(f"no response to {req}")
        return resp

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
        elif "batch" in step:
            ids = [conn.send_request(r) for r in step["batch"]]
            resps = [conn.response(rid, step.get("timeout", 60)) for rid in ids]
            for req, resp in zip(step["batch"], resps):
                print("->", json.dumps(req)[:120], "\n<-", json.dumps(resp))
            if "save" in step:
                with open(step["save"], "w") as f:
                    json.dump(resps, f)
            if "expect" in step and not all(r is not None and subset(step["expect"], r) for r in resps):
                raise SystemExit(f"unexpected response, wanted {step['expect']}")
        elif "events" in step:
            seen = set()
            for _ in range(step["count"]):
                ev = conn.wait_event(step["events"], step.get("timeout", 30))
                print("event", json.dumps(ev))
                key = json.dumps({k: v for k, v in ev.items() if k != "seq"}, sort_keys=True)
                if key in seen:
                    raise SystemExit(f"event reported twice: {ev}")
                seen.add(key)
                if step.get("ack"):
                    conn.request({"op": "ack_events", "up_to": ev["seq"]})
        elif "sleep" in step:
            time.sleep(step["sleep"])
    print("OK")


if __name__ == "__main__":
    main()
