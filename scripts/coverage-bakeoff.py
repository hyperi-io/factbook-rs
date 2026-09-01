#!/usr/bin/env python3
# Project:   factbook
# File:      scripts/coverage-bakeoff.py
# Purpose:   Measure what fraction of the address space each source answers
# Language:  Python
#
# License:   Apache-2.0
# Copyright: (c) 2026 HYPERI PTY LIMITED
"""Compare the sources factbook models, on coverage and on agreement.

Produces the measurements quoted in docs/data-sources.md. Draws routable IPv4
addresses uniformly from a fixed seed, asks every database for every address,
and reports the fraction each answers plus how often it agrees with a chosen
reference.

Point it at a directory of .mmdb files. `cargo test --test live_provider` with
FACTBOOK_LIVE=1 downloads them, or fetch them by hand from the URLs in
src/geoip/download/source.rs.

Usage:
    uv run --with maxminddb python3 scripts/coverage-bakeoff.py <dir> [--sample N]
"""

from __future__ import annotations

import argparse
import ipaddress
import json
import random
import sys
from pathlib import Path

import maxminddb

# Fixed so a re-run reproduces the published figures exactly.
SEED = 20260902
DEFAULT_SAMPLE = 200_000

# The reference every other source is compared against.
REFERENCE = "GeoLite2-ASN.mmdb"


def routable_sample(count: int) -> list[str]:
    """Uniformly drawn IPv4 addresses that could belong to a public host."""
    rng = random.Random(SEED)
    out: list[str] = []
    while len(out) < count:
        address = ipaddress.IPv4Address(rng.getrandbits(32))
        if address.is_private or address.is_reserved or address.is_multicast:
            continue
        if address.is_loopback or address.is_link_local or address.is_unspecified:
            continue
        out.append(str(address))
    return out


def asn_of(record: dict | None) -> int | None:
    """The autonomous system number, whichever spelling the source uses."""
    if not record:
        return None
    if (number := record.get("autonomous_system_number")) is not None:
        return int(number)
    if (text := record.get("asn")) is not None:
        text = str(text)
        digits = text.removeprefix("AS")
        return int(digits) if digits.isdigit() else None
    return None


def org_of(record: dict | None) -> str | None:
    """The operator name, whichever spelling the source uses."""
    if not record:
        return None
    for key in ("autonomous_system_organization", "as_name", "isp", "organization"):
        if value := record.get(key):
            return str(value)
    return None


def country_of(record: dict | None) -> str | None:
    """The country code, nested or flat."""
    if not record:
        return None
    if (code := record.get("country_code")) is not None:
        return str(code)
    country = record.get("country")
    if isinstance(country, dict):
        return country.get("iso_code")
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="directory holding .mmdb files")
    parser.add_argument("--sample", type=int, default=DEFAULT_SAMPLE)
    parser.add_argument("--json", action="store_true", help="emit machine-readable output")
    args = parser.parse_args()

    databases = sorted(args.directory.glob("*.mmdb"))
    if not databases:
        print(f"no .mmdb files under {args.directory}", file=sys.stderr)
        return 1

    readers = {path.name: maxminddb.open_database(str(path)) for path in databases}
    addresses = routable_sample(args.sample)

    asn: dict[str, dict[str, int]] = {name: {} for name in readers}
    org = dict.fromkeys(readers, 0)
    country = dict.fromkeys(readers, 0)

    for address in addresses:
        for name, reader in readers.items():
            record = reader.get(address)
            if (number := asn_of(record)) is not None:
                asn[name][address] = number
            if org_of(record):
                org[name] += 1
            if country_of(record):
                country[name] += 1

    total = len(addresses)
    results = {
        name: {
            "asn": len(asn[name]) / total,
            "operator": org[name] / total,
            "country": country[name] / total,
        }
        for name in readers
    }

    if args.json:
        print(json.dumps({"sample": total, "seed": SEED, "coverage": results}, indent=2))
    else:
        print(f"sample: {total} uniformly drawn routable IPv4 addresses, seed {SEED}\n")
        print(f"{'source':<28} {'ASN':>8} {'operator':>9} {'country':>8}")
        for name, row in sorted(results.items()):
            print(
                f"{name:<28} {row['asn']:>7.2%} {row['operator']:>8.2%} {row['country']:>7.2%}"
            )

        if REFERENCE in asn:
            print(f"\nASN agreement against {REFERENCE}, where both answer:")
            base = asn[REFERENCE]
            for name in sorted(readers):
                if name == REFERENCE:
                    continue
                shared = set(base) & set(asn[name])
                if not shared:
                    continue
                same = sum(1 for a in shared if base[a] == asn[name][a])
                print(f"  {name:<28} {same / len(shared):>7.2%}  over {len(shared)} shared")

    for reader in readers.values():
        reader.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
