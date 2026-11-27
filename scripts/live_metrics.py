"""Summarize Advisor JSONL and its sanitized captures. No game files are read."""

import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path


CONTEXTS = {"combat", "card_reward", "map"}


def latency(values):
    rows = sorted(values)
    if not rows:
        return {"count": 0, "p50_ms": None, "p95_ms": None, "max_ms": None}
    return {
        "count": len(rows),
        "p50_ms": round(rows[math.ceil(0.50 * len(rows)) - 1], 3),
        "p95_ms": round(rows[math.ceil(0.95 * len(rows)) - 1], 3),
        "max_ms": round(rows[-1], 3),
    }


def position(event):
    value = event.get("position") or {}
    if "session" in value and "state_id" in value:
        return value["session"], value["state_id"]
    return None


def load_captures(directory):
    result = {}
    for file in directory.glob("advisor-*.json"):
        _, _, session, state = file.stem.split("-")
        key = (int(session), int(state))
        if key in result:
            raise ValueError(f"Duplicate capture position {key}")
        result[key] = json.loads(file.read_text(encoding="utf-8"))
    return result


def decision_key(event, capture):
    context = event["context"]
    if context == "combat":
        return event["observation_id"]
    if capture is None:
        raise ValueError(f"Missing sanitized capture for {position(event)}")
    actions = [row["action"] for row in event["legal_actions"]]
    if context == "map":
        offered = sorted((row["row"], row["col"], row["room_type"]) for row in actions)
    else:
        offered = sorted(json.dumps({k: v for k, v in row.items() if k != "offer_index"}, sort_keys=True)
                         for row in actions)
    graph = capture["map"]
    graph_identity = [graph["boss"], graph["current"], graph["visited"], graph["nodes"]]
    return json.dumps([capture["act"], capture["floor"], graph_identity, offered], sort_keys=True)


def summarize(events, captures):
    totals = Counter(e["status"] for e in events)
    contexts = defaultdict(Counter)
    reasons = Counter()
    stale_reasons = Counter()
    observation_ids = defaultdict(Counter)
    decisions = defaultdict(set)
    inferred = defaultdict(set)
    forwards = defaultdict(list)
    receive = defaultdict(list)
    published = {}
    longest = {}
    for event in events:
        status = event["status"]
        context = event.get("context")
        key = position(event)
        if context in CONTEXTS:
            contexts[context][status] += 1
        if status == "blocked":
            reasons[event["error"]["code"]] += 1
        elif status == "stale":
            stale_reasons[event["reason"]] += 1
            published.pop(key, None)
        elif status in {"ok", "forced"}:
            published[key] = event["elapsed_ms"]
            observation_ids[context][event["observation_id"]] += 1
            decisions[context].add(decision_key(event, captures.get(key)))
            if event.get("reused"):
                contexts[context]["reused"] += 1
            if status == "ok":
                contexts[context]["inference" if "forward_ms" in event else "cached_ok"] += 1
                if "forward_ms" in event:
                    inferred[context].add(event["observation_id"])
                    forwards[context].append(event["forward_ms"])
                    receive[context].append(event["receive_to_result_ms"])
        elif status == "confirmed" and key in published:
            length = event["elapsed_ms"] - published[key]
            longest[key] = max(longest.get(key, 0), length)
    detail = {}
    for context in sorted(CONTEXTS):
        counts = contexts[context]
        fingerprints = observation_ids[context]
        detail[context] = {
            **counts,
            "unique_observations_ok_or_forced": len(fingerprints),
            "duplicate_observations_ok_or_forced": sum(fingerprints.values()) - len(fingerprints),
            "distinct_decisions_ok_or_forced": len(decisions[context]),
            "unique_scored_observations": len(inferred[context]),
            "duplicate_scored_observations": len(forwards[context]) - len(inferred[context]),
            "receive_to_result": latency(receive[context]),
            "forward": latency(forwards[context]),
        }
    longest_position = max(longest, key=longest.get) if longest else None
    return {
        "events": dict(totals),
        "blocked_reasons": dict(reasons),
        "stale_reasons": dict(stale_reasons),
        "contexts": detail,
        "unique_scored_observations": sum(map(len, inferred.values())),
        "receive_to_result": latency([v for ctx in receive.values() for v in ctx]),
        "forward": latency([v for ctx in forwards.values() for v in ctx]),
        "longest_confirmed_display": (
            {"session": longest_position[0], "state_id": longest_position[1],
             "elapsed_ms": longest[longest_position]} if longest_position else None
        ),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("events", type=Path, nargs="+")
    parser.add_argument("--captures", type=Path, required=True)
    args = parser.parse_args()
    events = [
        json.loads(row)
        for file in args.events
        for row in file.read_text(encoding="utf-8-sig").splitlines()
        if row.strip()
    ]
    print(json.dumps(summarize(events, load_captures(args.captures)), indent=2))


if __name__ == "__main__":
    main()
