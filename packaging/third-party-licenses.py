#!/usr/bin/env python3
"""Writes the license texts of everything compiled into the program to a file.

MIT and Apache licensed libraries are linked statically, and their licenses ask
that their notices accompany the program. Fails if a text cannot be found, so
that a package can never ship with an incomplete file.
"""

import json
import os
import subprocess
import sys

LICENSE_PREFIXES = ("LICEN", "COPYING", "UNLICENSE", "NOTICE")
HEADER = """Third-party software compiled into MonitorBeam

MonitorBeam is licensed under the GNU GPL, version 3 or later. It includes the
following libraries, each under its own license, reproduced below.
"""


def run(*command):
    return subprocess.check_output(command, text=True)


def license_files(directory):
    names = sorted(
        name
        for name in os.listdir(directory)
        if name.upper().startswith(LICENSE_PREFIXES)
        and os.path.isfile(os.path.join(directory, name))
    )
    return [os.path.join(directory, name) for name in names]


def section(title, license_name, paths):
    text = f"\n{'=' * 78}\n{title}\nLicense: {license_name}\n{'=' * 78}\n"
    for path in paths:
        with open(path, encoding="utf-8", errors="replace") as file:
            text += f"\n--- {os.path.basename(path)} ---\n\n{file.read()}\n"
    return text


def main(output):
    host = next(
        line.split(": ")[1]
        for line in run("rustc", "-vV").splitlines()
        if line.startswith("host: ")
    )
    # Only what is really linked on this platform, not build tools or other systems.
    linked = {
        tuple(line.split()[:2])
        for line in run(
            "cargo", "tree", "--locked", "-e", "normal", "--target", host,
            "--prefix", "none", "--format", "{p}",
        ).splitlines()
        if line.strip()
    }
    metadata = json.loads(run("cargo", "metadata", "--format-version", "1", "--locked"))

    text = HEADER
    missing = []
    packages = sorted(metadata["packages"], key=lambda package: package["name"])
    for package in packages:
        if package["name"] == "monitorbeam":
            continue
        if (package["name"], "v" + package["version"]) not in linked:
            continue
        paths = license_files(os.path.dirname(package["manifest_path"]))
        if not paths:
            missing.append(f"{package['name']} {package['version']}")
            continue
        title = f"{package['name']} {package['version']}"
        text += section(title, package["license"], paths)

    # The Rust standard library is linked statically as well.
    documents = os.path.join(run("rustc", "--print", "sysroot").strip(), "share/doc/rust")
    rust_paths = [
        path
        for path in (
            os.path.join(documents, "COPYRIGHT-library.html"),
            os.path.join(documents, "licenses", "MIT.txt"),
            os.path.join(documents, "licenses", "Apache-2.0.txt"),
        )
        if os.path.isfile(path)
    ]
    if len(rust_paths) < 3:
        missing.append("Rust standard library")
    text += section("Rust standard library", "MIT OR Apache-2.0", rust_paths)

    if missing:
        sys.exit("No license text found for: " + ", ".join(missing))
    with open(output, "w", encoding="utf-8") as file:
        file.write(text)
    print(f"Wrote the licenses of {text.count('License: ')} components to {output}")


if __name__ == "__main__":
    main(sys.argv[1])
