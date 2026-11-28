"""Export the pinned mod and apply the SpireMind observer patch without touching the game."""

import argparse
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

from setup import CHECKOUT, ROOT, check_source


def prepare(output, native_current):
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
        mod = source / "spire-mentor-mod"
        if native_current:
            patch = ROOT / "patches/spire-mentor-native-current.patch"
            args = ["git", "apply", "--unidiff-zero"]
            subprocess.run([*args, "--check", str(patch)], cwd=source, env=env, check=True)
            subprocess.run([*args, str(patch)], cwd=source, env=env, check=True)
        schema = (ROOT / "patches/native-current-v1.json").read_text(encoding="utf-8")
        (mod / "SpireMindCurrent.cs").write_bytes((ROOT / "patches/SpireMindCurrent.cs").read_bytes())
        (mod / "SpireMindCurrentSchema.cs").write_text(
            "namespace SpireMentor;\n"
            "internal static class SpireMindCurrentSchema\n"
            "{\n"
            f"    internal static readonly bool Enabled = {str(native_current).lower()};\n"
            f'    internal const string Json = """\n{schema}""";\n'
            "}\n",
            encoding="utf-8",
        )
        source.rename(output)
    print(f"Prepared observer source in {output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / ".local/observer-source")
    parser.add_argument("--native-current", action="store_true", help="Build the native-current observer variant")
    args = parser.parse_args()
    try:
        prepare(args.output, args.native_current)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Observer preparation failed: {error}\n")


if __name__ == "__main__":
    main()
