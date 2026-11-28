"""Fetch the pinned public sources and optional official Instinct weights."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from urllib.request import urlopen


ROOT = Path(__file__).resolve().parents[1]
REPO = "https://github.com/tfriedel/spire-mentor"
CHECKOUT = ROOT / ".local/upstream"


def git(*args):
    return subprocess.run(
        ["git", *args], cwd=CHECKOUT, check=True, text=True, stdout=subprocess.PIPE
    ).stdout.strip()


def setup_source():
    revision = (ROOT / "pins/upstream.rev").read_text(encoding="utf-8").strip()
    CHECKOUT.mkdir(parents=True, exist_ok=True)
    if not (CHECKOUT / ".git").is_dir():
        if any(CHECKOUT.iterdir()):
            raise ValueError(".local/upstream exists and is not an empty Git checkout")
        git("init", "--quiet")
    if git("status", "--porcelain", "--untracked-files=all"):
        raise ValueError(".local/upstream contains changes; use a clean checkout")
    ref = subprocess.run(
        ["git", "rev-parse", "--verify", "HEAD"], cwd=CHECKOUT, capture_output=True, text=True
    )
    if ref.stdout.strip() != revision:
        git("fetch", "--depth", "1", f"{REPO}.git", revision)
        git("checkout", "--quiet", "--detach", revision)
    if git("rev-parse", "--verify", "HEAD") != revision:
        raise ValueError("upstream checkout did not resolve to the pinned commit")
    print(f"Spire Mentor {revision}")


def check_source():
    revision = (ROOT / "pins/upstream.rev").read_text(encoding="utf-8").strip()
    if not (CHECKOUT / ".git").is_dir() or git("rev-parse", "--verify", "HEAD") != revision:
        raise ValueError(f"expected Spire Mentor checkout at {revision}; run scripts/setup.py")
    if git("status", "--porcelain", "--untracked-files=all"):
        raise ValueError(".local/upstream contains modifications; build requires the pinned source")
    print(f"Verified clean Spire Mentor {revision}")


def verify_weights(path, pin):
    with path.open("rb") as weights:
        digest = hashlib.file_digest(weights, "sha256").hexdigest()
    if path.stat().st_size != pin["bytes"] or digest != pin["sha256"]:
        raise ValueError(f"unexpected size or SHA-256 for {pin['file']}: {digest}")


def setup_weights():
    pin = json.loads((ROOT / "pins/instinct.json").read_text(encoding="utf-8"))
    path = ROOT / ".local/weights" / pin["file"]
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        verify_weights(path, pin)
    else:
        url = f"{REPO}/releases/download/{pin['release']}/{pin['file']}"
        with tempfile.TemporaryDirectory(dir=path.parent) as staging:
            download = Path(staging) / path.name
            with urlopen(url, timeout=60) as response, download.open("wb") as output:
                shutil.copyfileobj(response, output)
            verify_weights(download, pin)
            download.replace(path)
    print(f"{pin['file']} SHA-256 {pin['sha256']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--source-only", action="store_true", help="skip the model download")
    mode.add_argument("--check-source", action="store_true", help="verify the pinned checkout without fetching or changing it")
    args = parser.parse_args()
    try:
        if args.check_source:
            check_source()
        else:
            setup_source()
            check_source()
            if not args.source_only:
                setup_weights()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Setup failed: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
