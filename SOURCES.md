# Data sources

factbook ships no data. Every source is fetched at runtime from its own
publisher, and each publisher sets and states its own terms.

This page points you at those terms. It does not summarise or interpret them --
read them at the source and decide for yourself whether they suit your
deployment.

## Where each publisher states its terms

| source | published by | terms |
|---|---|---|
| DB-IP Lite City, DB-IP Lite ASN | DB-IP | <https://db-ip.com/db/lite.php> |
| GeoLite2 City, GeoLite2 ASN | MaxMind | <https://www.maxmind.com/en/geolite/eula> |
| GeoIP2 City, GeoIP2 ISP | MaxMind | <https://www.maxmind.com/en/end-user-license-agreement> |
| IPinfo Lite | IPinfo | <https://ipinfo.io/developers/ipinfo-lite-database> |
| `origin-asn`, `iptoasn-asn` | sapics/ip-location-db | <https://github.com/sapics/ip-location-db> |

The sapics repository republishes several upstreams and states terms **per
dataset**, not for the repository as a whole. Check the dataset you configured,
not the project.

## Reading this from code

`factbook::geoip::source_terms(selection)` returns the same pointers for
whatever a config actually selects, so a deployment can surface them without
anyone transcribing this table:

```rust
use factbook::geoip::{GeoIpProvider, ProviderSelection, source_terms};

for terms in source_terms(ProviderSelection::from(GeoIpProvider::DbIp)) {
    println!("{} ({}): {}", terms.name, terms.kind, terms.terms_url);
}
```

That call is generated from the same source table the downloader uses, so it
cannot drift from what was actually fetched. This page can.

## Sources you configure yourself

A table source named in your own config has no entry here and no terms reported
by `source_terms`. factbook fetches what you point it at. Whatever governs that
data is between you and wherever you got it.

## This crate's own licence

Apache-2.0, in `LICENSE`. It covers the code and nothing that the code
downloads.
