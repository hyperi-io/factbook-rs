<!-- Project:   factbook -->
<!-- File:      docs/data-sources.md -->
<!-- Purpose:   Which source to configure, and what each one answers -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Data sources

Reference for choosing what factbook fetches. Every source below is modelled as a row in the source table, so selecting one is config rather than code.

## The default source needs no account

An unconfigured deployment gets **DB-IP Lite** for location and **sapics `origin-asn`** for networks.

Measured over 200,000 uniformly drawn routable IPv4 addresses, on builds published by 2026-09-02, asking every database for every address:

| source | answers with an ASN | answers with a country |
|---|---|---|
| sapics `origin-asn` | **94.8%** | not published |
| IPinfo Lite | 84.5% | 99.8% |
| sapics `iptoasn-asn` | 84.3% | not published |
| MaxMind GeoLite2 | 84.2% | 99.5% |
| DB-IP Lite | 83.7% | **99.9%** |

`origin-asn` reaches ten points more of the address space than anything else in that field, MaxMind included, and DB-IP edges out both comparators on country. Neither asks for a signup.

Where a source and MaxMind both answer, they give the same autonomous system number 98.8% to 99.7% of the time. `origin-asn` sits at the bottom of that band and the top of the coverage one. Some of the disagreement is method rather than error: it reports the origin seen in BGP where MaxMind reports a registry view, and the two differ legitimately on a prefix announced by more than one network.

Uniform draws are not weighted by traffic, and live traffic concentrates in allocated space where every source does better. Coverage across the whole space is the number that matters for a default, because it measures where a deployment would be left blind.

Reproduce it with `scripts/coverage-bakeoff.py`, pointed at a directory of downloaded databases. The sample is drawn from a fixed seed, so a re-run gives the same addresses.

## What each source publishes

What each fills in a record, not what the upstream product sells:

| provider | tier | location | network | credential |
|---|---|---|---|---|
| `db_ip` | free | country, continent, city, region, coordinates | ASN, operator name blank on every row | none |
| `sapics_origin_asn` | free | -- | ASN, operator | none |
| `sapics_ip_to_asn` | free | -- | ASN, registry handle | none |
| `ip_info` | free | country, continent | ASN, operator, domain | token |
| `max_mind` | free | country, continent, city, region, coordinates, timezone | ASN, operator | account ID + licence key |
| `max_mind` | paid | as free | ASN, operator | account ID + licence key |
| `custom` | -- | whatever you point it at | whatever you point it at | none |

**Do not name `db_ip` for the network half.** Its free ASN database is structurally valid and matches addresses, and its operator-name column is blank on every row. A staged database that resolves nothing for the field its kind exists to hold is refused, so `provider: db_ip` across both halves loses the ASN download on every attempt and no ASN database lands. Name each half instead -- `city: db_ip`, `asn: sapics_origin_asn` -- which is the default. That check needs the lookup half compiled in and `verify_content` left on. Without either, the database lands and every operator name reads empty.

`sapics_origin_asn` names operators by their legal name where `sapics_ip_to_asn` uses the registry handle -- "Google LLC" against "GOOGLE". Prefer the former unless you are matching against registry data.

IPinfo publishes one file carrying both halves, so one selection for location fills the network fields from the same file.

The paid MaxMind line downloads GeoIP2-ISP for the network half. Its distinguishing fields -- the ISP and organisation names, which differ from the network's registered operator, and the mobile country and network codes -- have no typed field on `GeoIpRecord`, so they arrive in `record.extra` under the names the database writes: `isp`, `organization`, `mobile_country_code`, `mobile_network_code`. The same holds for anything else a source has that the typed fields do not cover, free builds included -- geoname ids, confidence scores, `traits`, and names in languages other than English.

`custom` downloads nothing. Set `city_db_path` and `asn_db_path` and factbook reads the files there.

factbook verifies a published digest before a download may replace what is on disk, where the provider publishes one. sapics publishes a `.sha256` for each file under a separate `checksum` release tag, and MaxMind serves a digest of the archive at the same endpoint, gated on the same account. DB-IP and IPinfo publish none, so the volume floor and the content checks stand in.

That gap is one reason the ASN default is `origin-asn` rather than DB-IP: of the sources needing no account, the two sapics sets alone can be checked against a digest from their publisher, and the location half has no such option at all. The blank operator column above is the second reason.

## Selecting a source

Each half is chosen separately, because no single provider does both well:

```yaml
geoip:
  provider:
    city: db_ip
    asn: sapics_origin_asn
```

Naming one provider applies it to both halves:

```yaml
geoip:
  provider: max_mind
```

A paid line is stated, never inferred from a credential being present:

```yaml
geoip:
  provider:
    city:
      provider: max_mind
      tier: paid
    asn: sapics_origin_asn
```

`factbook::geoip::validate` checks the selection when config loads, so a missing credential or an unmodelled tier is reported by name rather than as a rejected request on the first fetch.

factbook ships no data and fetches at runtime from whatever a deployment configures. Each publisher sets and states the terms for its data, and `SOURCES.md` in the repository root points at where. `factbook::geoip::source_terms` returns the same pointers for whatever a config actually selects.

Anything factbook has no row for becomes a table source instead -- a URL, a file name, an encoding and the key that reaches a row. See [getting-started.md](getting-started.md).
