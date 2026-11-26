#!/usr/bin/env python3
"""Generate flatpak-builder sources for all crates.io dependencies in Cargo.lock.

Produces the same layout as flatpak-builder-tools' cargo generator, without
third-party dependencies: each crate is downloaded as an archive into
`cargo/vendor/<name>-<version>` and cargo is configured to use that directory
instead of crates.io, so the build runs fully offline (required by Flathub).

Usage: python3 flatpak/gen-cargo-sources.py Cargo.lock > flatpak/cargo-sources.json
"""

import json
import sys
import tomllib

CARGO_CONFIG = """[source.vendored-sources]
directory = "cargo/vendor"

[source.crates-io]
replace-with = "vendored-sources"
"""


def main(lockfile: str) -> None:
    with open(lockfile, "rb") as f:
        lock = tomllib.load(f)

    sources = []
    for pkg in lock.get("package", []):
        source = pkg.get("source")
        if source is None:
            continue  # the workspace crate itself
        if not source.startswith("registry+https://github.com/rust-lang/crates.io-index"):
            sys.exit(f"unsupported source for {pkg['name']}: {source}")
        name, version, checksum = pkg["name"], pkg["version"], pkg["checksum"]
        dest = f"cargo/vendor/{name}-{version}"
        sources.append({
            "type": "archive",
            "archive-type": "tar-gzip",
            "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
            "sha256": checksum,
            "dest": dest,
        })
        sources.append({
            "type": "inline",
            "contents": json.dumps({"package": checksum, "files": {}}),
            "dest": dest,
            "dest-filename": ".cargo-checksum.json",
        })

    sources.append({
        "type": "inline",
        "contents": CARGO_CONFIG,
        "dest": "cargo",
        "dest-filename": "config.toml",
    })
    json.dump(sources, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "Cargo.lock")
