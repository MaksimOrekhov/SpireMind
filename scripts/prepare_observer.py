"""Export the pinned mod and apply the SpireMind observer patch without touching the game."""

import argparse
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

from setup import CHECKOUT, ROOT, check_source


def prepare(output):
    check_source()
    output = output.resolve()
    if output.exists():
        raise ValueError(f"{output} already exists; choose an unused --output")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="spire-mentor-observer-", dir=output.parent) as name:
        staging = Path(name)
        archive = staging / "spire-mentor.tar"
        source = staging / "source"
        source.mkdir()
        subprocess.run(
            ["git", "-C", str(CHECKOUT), "archive", f"--output={archive}", "HEAD", "spire-mentor-mod"],
            check=True,
        )
        with tarfile.open(archive) as tar:
            tar.extractall(source, filter="data")
        patch = ROOT / "patches/spire-mentor-observer.patch"
        env = {**os.environ, "GIT_CEILING_DIRECTORIES": str(staging)}
        subprocess.run(["git", "apply", "--check", str(patch)], cwd=source, env=env, check=True)
        subprocess.run(["git", "apply", str(patch)], cwd=source, env=env, check=True)
        source.rename(output)
    print(f"Prepared observer source in {output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / ".local/observer-source")
    args = parser.parse_args()
    try:
        prepare(args.output)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Observer preparation failed: {error}\n")


if __name__ == "__main__":
    main()
