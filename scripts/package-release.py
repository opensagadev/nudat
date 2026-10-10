"""Package and smoke-test a native nudat CLI build (Python 3.12+)."""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-tag", action="store_true")
    parser.add_argument("--target")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    with (root / "cli/Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    if args.check_tag:
        tag = os.environ.get("RELEASE_TAG", "")
        if tag != f"nudat-cli-v{version}":
            parser.error(f"tag {tag!r} must match CLI version nudat-cli-v{version}")
        return
    if not args.target:
        parser.error("--target is required when packaging")

    windows = "-windows-" in args.target
    binary_name = "nudat.exe" if windows else "nudat"
    binary = root / "target" / args.target / "release" / binary_name
    bundle = f"nudat-cli-v{version}-{args.target}"
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    archive = dist / (bundle + (".zip" if windows else ".tar.gz"))
    files = [binary, root / "README.md", root / "LICENSE"]
    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
            for path in files:
                output.write(path, f"{bundle}/{path.name}")
    else:
        with tarfile.open(archive, "w:gz") as output:
            for path in files:
                output.add(path, arcname=f"{bundle}/{path.name}")

    # Run the extracted executable to catch missing dependencies or lost permissions.
    with tempfile.TemporaryDirectory() as directory:
        if windows:
            with zipfile.ZipFile(archive) as packed:
                packed.extractall(directory)
        else:
            with tarfile.open(archive) as packed:
                packed.extractall(directory, filter="data")
        executable = Path(directory) / bundle / binary_name
        result = subprocess.check_output([str(executable), "--version"], text=True)
        if result.strip() != f"nudat {version}":
            raise RuntimeError(f"Unexpected binary version: {result!r}")

    with archive.open("rb") as contents:
        checksum = hashlib.file_digest(contents, "sha256").hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="utf-8", newline="\n"
    )
    print(archive)


if __name__ == "__main__":
    main()
