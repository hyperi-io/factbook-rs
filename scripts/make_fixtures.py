#!/usr/bin/env python3
# Project:   factbook
# File:      scripts/make_fixtures.py
# Purpose:   Build the MMDB test fixtures the suite and the benches read
# Language:  Python
#
# License:   Apache-2.0
# Copyright: (c) 2026 HYPERI PTY LIMITED
"""Write the five test databases under tests/data.

Each carries the MMDB binary format and one published record shape -- MaxMind's
City, ASN and ISP schemas, and IPinfo Lite's flat one -- holding the handful of
public values the Rust tests assert. No provider's data is redistributed.

Two of the five exist for the fields no typed record field names. city-rich
carries the full City shape a paid or a current free build writes -- geoname
ids, confidence scores, names in several languages, traits, a represented
country -- and isp carries the GeoIP2-ISP shape. Both prove a lookup answers
with what the source holds rather than with what the record was written for.

Every network here is either asserted by a test in src/geoip/enricher.rs or
src/geoip/refresh.rs, or enumerated by benches/geoip_lookup.rs, which re-checks
each one resolves before it times anything.

Usage:
    uv run --with mmdb_writer --with netaddr python3 scripts/make_fixtures.py
"""

from __future__ import annotations

import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from mmdb_writer import MMDBWriter
from netaddr import IPSet

OUT_DIR = Path(__file__).resolve().parent.parent / "tests" / "data"

# Fixed, so a rebuild is byte-identical and the build stamp stays far enough
# behind the file's own timestamp for the download suite's age test.
BUILD_EPOCH = int(datetime(2026, 1, 1, tzinfo=timezone.utc).timestamp())


class FixedEpochWriter(MMDBWriter):
    """A writer that stamps the constant above rather than reading the clock."""

    def _build_meta(self) -> dict[str, Any]:
        meta = super()._build_meta()
        meta["build_epoch"] = BUILD_EPOCH
        return meta


def place(
    *,
    continent: tuple[str, str],
    country: tuple[str, str],
    time_zone: str,
    latitude: float,
    longitude: float,
    accuracy_radius: int,
    city: str | None = None,
    postal: str | None = None,
    subdivisions: list[tuple[str, str]] | None = None,
) -> dict[str, Any]:
    """One record in MaxMind's City schema, omitting whatever was not given.

    No metro_code is ever written: the field is deprecated upstream and the
    suite asserts an absent field stays absent rather than reading as zero.
    """
    record: dict[str, Any] = {
        "continent": {"code": continent[0], "names": {"en": continent[1]}},
        "country": {"iso_code": country[0], "names": {"en": country[1]}},
        "location": {
            "accuracy_radius": accuracy_radius,
            "latitude": latitude,
            "longitude": longitude,
            "time_zone": time_zone,
        },
    }
    if city is not None:
        record["city"] = {"names": {"en": city}}
    if postal is not None:
        record["postal"] = {"code": postal}
    if subdivisions:
        record["subdivisions"] = [
            {"iso_code": code, "names": {"en": name}} for code, name in subdivisions
        ]
    return record


ASIA = ("AS", "Asia")
EUROPE = ("EU", "Europe")
NORTH_AMERICA = ("NA", "North America")

# Names carrying an accent are escaped, so the source file stays ASCII.
LINKOPING = "Link\u00f6ping"
OSTERGOTLAND = "\u00d6sterg\u00f6tland County"

CITY_RECORDS: list[tuple[str, dict[str, Any]]] = [
    # Two subdivisions, largest first -- the reader takes the last one, so this
    # is the entry that separates West Berkshire from England.
    (
        "2.125.160.216/29",
        place(
            city="Boxford",
            continent=EUROPE,
            country=("GB", "United Kingdom"),
            postal="OX1",
            time_zone="Europe/London",
            latitude=51.75,
            longitude=-1.25,
            accuracy_radius=100,
            subdivisions=[("ENG", "England"), ("WBK", "West Berkshire")],
        ),
    ),
    (
        "89.160.20.112/28",
        place(
            city=LINKOPING,
            continent=EUROPE,
            country=("SE", "Sweden"),
            time_zone="Europe/Stockholm",
            latitude=58.4167,
            longitude=15.6167,
            accuracy_radius=76,
            subdivisions=[("E", OSTERGOTLAND)],
        ),
    ),
    (
        "89.160.20.128/25",
        place(
            city=LINKOPING,
            continent=EUROPE,
            country=("SE", "Sweden"),
            time_zone="Europe/Stockholm",
            latitude=58.4167,
            longitude=15.6167,
            accuracy_radius=76,
            subdivisions=[("E", OSTERGOTLAND)],
        ),
    ),
    # The one IPv6 network on the city side; the ASN fixture deliberately covers
    # different v6 space, so a half-filled record still has to answer.
    (
        "2001:218::/32",
        place(
            continent=ASIA,
            country=("JP", "Japan"),
            time_zone="Asia/Tokyo",
            latitude=35.6897,
            longitude=139.6922,
            accuracy_radius=100,
        ),
    ),
    (
        "67.43.156.0/24",
        place(
            continent=NORTH_AMERICA,
            country=("US", "United States"),
            time_zone="America/New_York",
            latitude=40.7128,
            longitude=-74.0060,
            accuracy_radius=1000,
        ),
    ),
    (
        "81.2.69.142/31",
        place(
            city="London",
            continent=EUROPE,
            country=("GB", "United Kingdom"),
            time_zone="Europe/London",
            latitude=51.5074,
            longitude=-0.1278,
            accuracy_radius=10,
            subdivisions=[("ENG", "England")],
        ),
    ),
    (
        "81.2.69.144/28",
        place(
            city="London",
            continent=EUROPE,
            country=("GB", "United Kingdom"),
            time_zone="Europe/London",
            latitude=51.5074,
            longitude=-0.1278,
            accuracy_radius=10,
            subdivisions=[("ENG", "England")],
        ),
    ),
    (
        "81.2.69.160/27",
        place(
            city="London",
            continent=EUROPE,
            country=("GB", "United Kingdom"),
            time_zone="Europe/London",
            latitude=51.5074,
            longitude=-0.1278,
            accuracy_radius=10,
            subdivisions=[("ENG", "England")],
        ),
    ),
    (
        "81.2.69.192/28",
        place(
            city="London",
            continent=EUROPE,
            country=("GB", "United Kingdom"),
            time_zone="Europe/London",
            latitude=51.5074,
            longitude=-0.1278,
            accuracy_radius=10,
            subdivisions=[("ENG", "England")],
        ),
    ),
    (
        "175.16.199.0/24",
        place(
            city="Changchun",
            continent=ASIA,
            country=("CN", "China"),
            time_zone="Asia/Shanghai",
            latitude=43.88,
            longitude=125.3228,
            accuracy_radius=50,
        ),
    ),
    (
        "202.196.224.0/20",
        place(
            continent=ASIA,
            country=("PH", "Philippines"),
            time_zone="Asia/Manila",
            latitude=14.5995,
            longitude=120.9842,
            accuracy_radius=1000,
        ),
    ),
    (
        "214.78.0.0/17",
        place(
            continent=NORTH_AMERICA,
            country=("US", "United States"),
            time_zone="America/Chicago",
            latitude=37.751,
            longitude=-97.822,
            accuracy_radius=1000,
        ),
    ),
    (
        "216.160.83.56/29",
        place(
            city="Milton",
            continent=NORTH_AMERICA,
            country=("US", "United States"),
            postal="98354",
            time_zone="America/Los_Angeles",
            latitude=47.2513,
            longitude=-122.3149,
            accuracy_radius=22,
            subdivisions=[("WA", "Washington")],
        ),
    ),
]

# Neither 2.125.160.216 nor any IPv6 address the city fixture holds appears
# here: several tests turn on one database answering where the other does not.
ASN_RECORDS: list[tuple[str, dict[str, Any]]] = [
    (
        "2001:2000::/20",
        {
            "autonomous_system_number": 1299,
            "autonomous_system_organization": "TeliaSonera International Carrier",
        },
    ),
    (
        "1.0.0.0/24",
        {
            "autonomous_system_number": 15169,
            "autonomous_system_organization": "Google Inc.",
        },
    ),
    (
        "89.160.0.0/17",
        {
            "autonomous_system_number": 29518,
            "autonomous_system_organization": "Bredband2 AB",
        },
    ),
]

# Katakana for Boxford, escaped so the source file stays ASCII.
BOXFORD_JA = "\u30dc\u30c3\u30af\u30b9\u30d5\u30a9\u30fc\u30c9"

# The City shape as a current build actually writes it: the typed record names
# eight of these paths and the rest have nowhere to go but the extra map.
CITY_RICH_RECORDS: list[tuple[str, dict[str, Any]]] = [
    (
        "2.125.160.216/29",
        {
            "city": {
                "confidence": 25,
                "geoname_id": 2655045,
                "names": {"de": "Boxford", "en": "Boxford", "ja": BOXFORD_JA},
            },
            "continent": {
                "code": "EU",
                "geoname_id": 6255148,
                "names": {"en": "Europe", "fr": "Europe"},
            },
            "country": {
                "confidence": 99,
                "geoname_id": 2635167,
                "is_in_european_union": False,
                "iso_code": "GB",
                "names": {"en": "United Kingdom", "fr": "Royaume-Uni"},
            },
            "location": {
                "accuracy_radius": 100,
                "average_income": 32323,
                "latitude": 51.75,
                "longitude": -1.25,
                "population_density": 348,
                "time_zone": "Europe/London",
            },
            "postal": {"code": "OX1", "confidence": 20},
            "registered_country": {
                "geoname_id": 2635167,
                "is_in_european_union": False,
                "iso_code": "GB",
                "names": {"en": "United Kingdom"},
            },
            "represented_country": {
                "geoname_id": 6252001,
                "iso_code": "US",
                "names": {"en": "United States"},
                "type": "military",
            },
            # Largest first, so the typed region field takes the second and the
            # first survives only in the extra map.
            "subdivisions": [
                {"confidence": 40, "geoname_id": 6269131, "iso_code": "ENG",
                 "names": {"en": "England"}},
                {"confidence": 40, "geoname_id": 3333217, "iso_code": "WBK",
                 "names": {"en": "West Berkshire"}},
            ],
            "traits": {
                "is_anycast": True,
                "is_satellite_provider": False,
                "user_type": "residential",
            },
        },
    ),
    # A whole /24, so the benchmark has 256 distinct addresses to read cold.
    (
        "81.2.69.0/24",
        {
            "city": {
                "confidence": 60,
                "geoname_id": 2643743,
                "names": {"de": "London", "en": "London", "fr": "Londres"},
            },
            "continent": {
                "code": "EU",
                "geoname_id": 6255148,
                "names": {"en": "Europe", "fr": "Europe"},
            },
            "country": {
                "confidence": 99,
                "geoname_id": 2635167,
                "is_in_european_union": False,
                "iso_code": "GB",
                "names": {"en": "United Kingdom", "fr": "Royaume-Uni"},
            },
            "location": {
                "accuracy_radius": 10,
                "average_income": 41000,
                "latitude": 51.5074,
                "longitude": -0.1278,
                "population_density": 5701,
                "time_zone": "Europe/London",
            },
            "postal": {"code": "EC1A", "confidence": 40},
            "registered_country": {
                "geoname_id": 2635167,
                "is_in_european_union": False,
                "iso_code": "GB",
                "names": {"en": "United Kingdom"},
            },
            "subdivisions": [
                {"confidence": 70, "geoname_id": 6269131, "iso_code": "ENG",
                 "names": {"en": "England"}},
            ],
            "traits": {"is_anycast": False, "user_type": "business"},
        },
    ),
]

# GeoIP2-ISP, the paid edition whose distinguishing fields the record has never
# had a slot for. Only the ASN pair is typed; isp, organization and the mobile
# codes reach a consumer through the extra map or not at all.
ISP_RECORDS: list[tuple[str, dict[str, Any]]] = [
    (
        "89.160.20.112/28",
        {
            "autonomous_system_number": 29518,
            "autonomous_system_organization": "Bredband2 AB",
            "isp": "Bredband2 AB",
            "organization": "Bredband2 Customer",
        },
    ),
    (
        "1.0.0.0/24",
        {
            "autonomous_system_number": 15169,
            "autonomous_system_organization": "Google Inc.",
            "isp": "Telstra Mobile",
            "mobile_country_code": "505",
            "mobile_network_code": "01",
            "organization": "Telstra Mobile Data",
        },
    ),
]

# IPinfo publishes no test database, so the decoder would otherwise only be
# exercised by a live run with a token. Field names, types and the AS-prefixed
# asn string are copied from the real 23 MB download.
IPINFO_RECORDS: list[tuple[str, dict[str, Any]]] = [
    (
        "8.8.8.0/24",
        {
            "as_domain": "google.com",
            "as_name": "Google LLC",
            "asn": "AS15169",
            "continent": "North America",
            "continent_code": "NA",
            "country": "United States",
            "country_code": "US",
        },
    ),
    (
        "1.1.1.0/24",
        {
            "as_domain": "cloudflare.com",
            "as_name": "Cloudflare, Inc.",
            "asn": "AS13335",
            "continent": "Oceania",
            "continent_code": "OC",
            "country": "Australia",
            "country_code": "AU",
        },
    ),
    (
        "2606:4700:4700::/48",
        {
            "as_domain": "cloudflare.com",
            "as_name": "Cloudflare, Inc.",
            "asn": "AS13335",
            "continent": "North America",
            "continent_code": "NA",
            "country": "United States",
            "country_code": "US",
        },
    ),
    # A record with no network fields, to prove a partial one still resolves.
    # Routable space: a documentation or reserved range short-circuits before
    # the database is read.
    (
        "45.45.45.0/24",
        {
            "continent": "Oceania",
            "continent_code": "OC",
            "country": "Australia",
            "country_code": "AU",
        },
    ),
]


def build(
    name: str,
    database_type: str,
    records: list[tuple[str, dict[str, Any]]],
    int_type: str = "auto",
) -> None:
    """Write one database, IPv6 with the IPv4 space mapped under ::/96.

    The Rust reader's scalar decoders are type-exact -- a field typed Option<u32>
    will not read a uint16 off the wire -- so int_type has to match the schema
    the record is decoded into, not the magnitude of the value.
    """
    writer = FixedEpochWriter(
        ip_version=6,
        database_type=database_type,
        ipv4_compatible=True,
        int_type=int_type,
    )

    for network, record in records:
        writer.insert_network(IPSet([network]), record)

    path = OUT_DIR / name
    writer.to_db_file(str(path))
    print(f"{path} -- {path.stat().st_size} bytes")


def main() -> int:
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    # The only integer in the City schema this writes is accuracy_radius, a u16,
    # which is what "auto" picks for every value here.
    build("city-test.mmdb", "factbook-city-test", CITY_RECORDS)
    # autonomous_system_number is a u32 in the ASN schema.
    build("asn-test.mmdb", "factbook-asn-test", ASN_RECORDS, int_type="u32")
    # The reader keys the flat schema off this exact database_type prefix.
    build("IPinfo-Lite-Test.mmdb", "ipinfo bundle_location_lite.mmdb", IPINFO_RECORDS)
    # accuracy_radius is still the only integer the typed decode reads here.
    build("city-rich-test.mmdb", "factbook-city-rich-test", CITY_RICH_RECORDS)
    # autonomous_system_number is a u32 in the ISP schema too.
    build("isp-test.mmdb", "factbook-isp-test", ISP_RECORDS, int_type="u32")

    return 0


if __name__ == "__main__":
    sys.exit(main())
