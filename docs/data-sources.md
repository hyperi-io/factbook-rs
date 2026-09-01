<!-- Project:   factbook -->
<!-- File:      docs/data-sources.md -->
<!-- Purpose:   Which source to configure, and what each one answers -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Data sources

Reference for choosing what factbook fetches. Every source below is modelled as a row in the source table, so selecting one is config rather than code.

## The Default Source Needs No Account

An unconfigured deployment gets **DB-IP Lite** for location and **sapics `origin-asn`** for networks.

Measured over 200,000 uniformly drawn routable IPv4 addresses, asking every database for every address:

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
| `db_ip` | free | country, continent, city, region, coordinates | ASN, operator | none |
| `sapics_origin_asn` | free | -- | ASN, operator | none |
| `sapics_ip_to_asn` | free | -- | ASN, registry handle | none |
| `ip_info` | free | country, continent | ASN, operator, domain | token |
| `max_mind` | free | country, continent, city, region, coordinates, timezone | ASN, operator | account ID + licence key |
| `max_mind` | paid | as free | ASN, operator | account ID + licence key |
| `custom` | -- | whatever you point it at | whatever you point it at | none |

`sapics_origin_asn` names operators by their legal name where `sapics_ip_to_asn` uses the registry handle -- "Google LLC" against "GOOGLE". Prefer the former unless you are matching against registry data.

IPinfo publishes one file carrying both halves, so selecting it for location fills the network fields too.

The paid MaxMind line downloads GeoIP2-ISP for the network half, and its distinguishing fields -- the ISP and organisation names, which differ from the network's registered operator -- have nowhere to go: `GeoIpRecord` carries no field for them. Selecting the paid tier does not surface them.

`custom` downloads nothing. Set `city_db_path` and `asn_db_path` and factbook reads what is already there.

## Only some sources publish a digest

factbook verifies a published digest before a download is allowed to replace what is on disk. Whether one exists is the provider's choice:

- **sapics** publishes `.sha256` beside each release asset.
- **MaxMind** serves a digest of the archive at the same endpoint, gated on the same account.
- **DB-IP** and **IPinfo** publish none. The volume floor and the content checks stand in.

That gap is one reason the ASN default is `origin-asn` rather than DB-IP: the two sapics sets are the only ones needing no account whose downloads can be checked against the publisher's own digest, and the location half has no such option at all.

## Selecting a source

Each half is chosen on its own, because no single provider does both well:

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

A paid line is named, never inferred from a credential being present:

```yaml
geoip:
  provider:
    city:
      provider: max_mind
      tier: paid
    asn: sapics_origin_asn
```

`factbook::geoip::validate` checks the selection when config loads, so a missing credential or an unmodelled tier is reported by name rather than as a rejected request on the first fetch.

## Terms are the publisher's to state

factbook ships no data and fetches at runtime from whatever a deployment configures. Each publisher sets and states its own terms, and `SOURCES.md` in the repository root points at where. `factbook::geoip::source_terms` returns the same pointers for whatever a config actually selects.

Anything factbook has no row for is named directly as a table source instead -- a URL, a file name, an encoding and the key that reaches a row. See [getting-started.md](getting-started.md).
