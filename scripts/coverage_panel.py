"""Compare two builds on validated observations or rank redacted blocker evidence."""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
import subprocess


def load_panel(executable, files):
    observations = {}
    for offset in range(0, len(files), 24):
        batch = files[offset:offset + 24]
        result = subprocess.run(
            [str(executable.resolve()), "replay", *map(str, batch)],
            capture_output=True, encoding="utf-8", check=False,
        )
        payload = json.loads(result.stdout)
        if result.returncode not in (0, 1) or len(payload.get("observations", [])) != len(batch):
            raise ValueError(f"Replay did not return every row using {executable.name}")
        for row in payload["observations"]:
            key = (row["context"], row["observation_id"])
            stable = {k: row[k] for k in ("status", "error", "legal_actions",
                                         "recommendations", "forced_action_id") if k in row}
            if key in observations and observations[key] != stable:
                raise ValueError(f"Non-deterministic replay for {key}")
            observations[key] = stable
    return observations


def summary(panel):
    totals = defaultdict(Counter)
    reasons = defaultdict(Counter)
    for (context, _), result in panel.items():
        totals[context][result["status"]] += 1
        if result["status"] == "blocked":
            error = result["error"]
            reasons[context][f'{error["code"]}: {error["message"]}'] += 1
    return {"outcomes": dict(totals), "blocked_reasons": dict(reasons)}


def compare(args):
    files = sorted(args.captures.glob("advisor-*.json"))
    if not files:
        raise ValueError("No validated advisor-*.json observations")
    before = load_panel(args.baseline, files)
    after = load_panel(args.candidate, files)
    if before.keys() != after.keys():
        raise ValueError("Builds emitted different observation IDs")
    transitions = Counter()
    shared = Counter()
    changed = []
    for (context, fingerprint), old in before.items():
        new = after[(context, fingerprint)]
        transitions[f'{context}.{old["status"]}->{new["status"]}'] += 1
        if old["status"] in ("ok", "forced") and new["status"] in ("ok", "forced"):
            shared[context] += 1
            if old != new:
                changed.append({"context": context, "observation_id": fingerprint})
    print(json.dumps({"source": args.source, "files": len(files),
                      "unique_observations": len(before), "before": summary(before),
                      "after": summary(after), "transitions": transitions,
                      "previously_accepted": shared, "ranking_or_action_changes": changed}, indent=2))
    return int(bool(changed) or any(
        old["status"] in ("ok", "forced") and after[key]["status"] not in ("ok", "forced")
        for key, old in before.items()
    ))


def blockers(args):
    events = Counter()
    fingerprints = defaultdict(set)
    evidence = defaultdict(set)
    evidence_events = Counter()
    revisions = Counter()
    truncated = 0
    for file in sorted(args.corpus.glob("blocked-*.json")):
        row = json.loads(file.read_text(encoding="utf-8"))
        if row.get("kind") != "blocked_diagnostic" or row.get("schema_version") != 1:
            raise ValueError(f"Invalid diagnostic record: {file.name}")
        context = row["context"] if row["in_scope"] else "outside_scope"
        events[context] += 1
        revisions[(row["producer_revision"], row["upstream_revision"])] += 1
        truncated += int(row["truncated"])
        fingerprint = row["public_fingerprint"]
        if not fingerprint:
            continue
        fingerprints[context].add(fingerprint)
        seen = set()
        for item in row["blockers"]:
            path = re.sub(r"\[\d+\]", "[]", item["path"])
            key = (context, path, item["id"] if "id" in item else None, item["reason"], item["code"])
            evidence[key].add(fingerprint)
            seen.add(key)
        evidence_events.update(seen)
    if not events:
        raise ValueError("No blocked-*.json diagnostics")
    ranked = sorted(evidence, key=lambda key: (-len(evidence[key]), -evidence_events[key], str(key)))
    print(json.dumps({"source": "redacted_blocked_diagnostics", "events": events,
        "unique_public_fingerprints": {k: len(v) for k,v in fingerprints.items()},
        "truncated_records": truncated,
        "revisions": [{"producer": p, "engine": e, "events": n} for (p,e),n in sorted(revisions.items())],
        "ranked": [{"context": c, "path": p, "id": i, "reason": r, "code": code,
                    "events": evidence_events[(c,p,i,r,code)], "unique_states": len(evidence[(c,p,i,r,code)])}
                   for c,p,i,r,code in ranked]}, indent=2))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(required=True)
    panel = commands.add_parser("compare")
    panel.add_argument("--baseline", type=Path, required=True)
    panel.add_argument("--candidate", type=Path, required=True)
    panel.add_argument("--captures", type=Path, required=True)
    panel.add_argument("--source", choices=("sanitized-live", "synthetic"), required=True)
    panel.set_defaults(run=compare)
    audit = commands.add_parser("blockers")
    audit.add_argument("corpus", type=Path)
    audit.set_defaults(run=blockers)
    args = parser.parse_args()
    try:
        return args.run(args)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"Coverage panel failed: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
