# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it
responsibly. We take security seriously and will respond promptly.

**Email**: <security@hyperi.io>

Please include:

- A description of the vulnerability
- The affected component or version
- Steps to reproduce the issue
- Proof-of-concept code (if applicable)
- Your contact information for follow-up

## What to Expect

- **Acknowledgment**: We will acknowledge receipt of your report within
  5 business days
- **Investigation**: We will investigate and keep you informed of our progress
- **Resolution**: We will work to resolve confirmed vulnerabilities promptly
- **Disclosure**: We will coordinate with you on an appropriate disclosure
  timeline

## Safe Harbour

We will not pursue legal action against security researchers who:

- Report vulnerabilities in good faith
- Make reasonable efforts to avoid privacy violations, data destruction,
  and service disruption
- Do not access or modify data beyond what is necessary to demonstrate
  the vulnerability
- Allow reasonable time for us to address the issue before public disclosure
- Comply with applicable Australian law

## Recognition

With your permission, we will credit you for the discovery of confirmed
vulnerabilities. We do not offer monetary bounties. We value and appreciate
responsible disclosure regardless.

## A database is trusted input

factbook downloads reference databases from sources the deploying operator
names, and then decodes whatever arrives. **Treat a database as trusted
input, and the URL it comes from as part of your trust boundary.**

A deliberately crafted MaxMind DB file can describe a record far larger than
the file. The format's data section is a pointer graph, so a file of a few
hundred bytes can expand to billions of leaves. Before `maxminddb` 0.31 the
reader bounded nesting depth but not expansion, and we measured a 624-byte
file that stopped a lookup returning at all.

What bounds it now: `maxminddb` 0.31 and later caps the values and the string
and byte data a single decode expands, and returns `ResourceLimit` past the
cap rather than continuing. factbook also bounds the map of unmodelled source
fields on each record, at 512 fields and 64 KiB, by stopping the decode as the
limit is reached rather than after.

So: fetch over TLS, from sources you have reason to trust, and check the
published digest where a publisher offers one -- factbook verifies it for
you when `checksum_url` is set. A compromised source or a hostile URL is not
something the crate can defend you against on its own.

## Out of Scope

The following are generally out of scope:

- Social engineering or phishing attacks
- Denial of service (DoS/DDoS) attacks
- Physical security issues
- Attacks requiring access to a user's device or account
- Issues in third-party dependencies (please report these to the relevant
  maintainer)
- Theoretical vulnerabilities without proof of exploitability
- Missing security headers or SSL/TLS configuration issues that are not
  directly exploitable

## Contact

**Security reports**: <security@hyperi.io>

For non-security issues, please use the project's issue tracker.
