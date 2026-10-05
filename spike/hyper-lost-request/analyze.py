#!/usr/bin/env python3
"""Scans spike logs (balerix#129) for the stale-want chain on each client
HTTP/1 connection:
  race          - a dispatcher poll ends with a request queued (rx_len>=1)
                  and its want::Taker in `Want`: want() landed after give();
  stale_want    - a poll ends with a request in flight or a response body
                  being read and the Taker still in `Want` (the race's want
                  survived the take; a fresh connection's Busy/Init/Init
                  with `Want` is legitimate and not counted);
  behind_body   - a request is queued onto a connection that is still reading
                  an earlier response's body (it waits for that body to end);
  behind_watch  - the same, where the earlier request is a watch: the queued
                  request waits for the watch to end (timeoutSeconds=290);
  spike_dumps   - "SPIKE lost" lines: a reconcile stuck 30 s or a probe 5 s.
usage: analyze.py [-v] <log>...
"""
import re
import sys

POLL_END = re.compile(r"h1conn\{conn=(\d+)\}: .*spike: poll end (\S+): (.*)$")
TOOK = re.compile(r"h1conn\{conn=(\d+)\}: .*spike: poll_msg took (\S+ \S+)")
QUEUED = re.compile(r"spike: try_send_request conn=(\d+) (\S+ \S+) queued=true")


def scan(path):
    state, current = {}, {}
    stale = race = 0
    behind = []
    dumps = 0
    with open(path, errors="replace") as f:
        for n, line in enumerate(f, 1):
            if "SPIKE lost" in line:
                dumps += 1
                continue
            m = POLL_END.search(line)
            if m:
                conn, st = m.group(1), m.group(3)
                state[conn] = st
                if "taker=Taker { state: Want }" in st:
                    if re.search(r"rx_len=[1-9]", st):
                        race += 1
                    if "callback=true" in st or "reading: Body" in st:
                        stale += 1
                continue
            m = TOOK.search(line)
            if m:
                current[m.group(1)] = m.group(2)
                continue
            m = QUEUED.search(line)
            if m:
                conn, req = m.groups()
                st = state.get(conn, "")
                if "reading: Body" in st and "callback=false" in st:
                    behind.append((n, conn, req, current.get(conn, "?")))
    return race, stale, behind, dumps


def main():
    args = sys.argv[1:]
    verbose = args[:1] == ["-v"]
    if verbose:
        args = args[1:]
    tot = [0, 0, 0, 0, 0]
    for path in args:
        race, stale, behind, dumps = scan(path)
        watch = [b for b in behind if "watch=true" in b[3]]
        tot = [a + b for a, b in zip(tot, [race, stale, len(behind), len(watch), dumps])]
        if verbose and (watch or dumps):
            print(f"{path}: race={race} stale_want={stale} behind_body={len(behind)} behind_watch={len(watch)} spike_dumps={dumps}")
            for n, conn, req, cur in watch:
                print(f"  line {n} conn={conn} {req[:100]}\n    behind {cur[:140]}")
    print(
        f"total: race={tot[0]} stale_want={tot[1]} behind_body={tot[2]} "
        f"behind_watch={tot[3]} spike_dumps={tot[4]}"
    )


main()
