# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it
responsibly. We take security seriously and will respond promptly.

**Email**: security@hyperi.io

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

A deliberately crafted MaxMind DB file can make a single lookup take
unbounded time. The format's data section is a pointer graph, and the
decoder follows pointers with a limit on nesting depth but not on how many
values a record expands to. A file of a few hundred bytes can therefore
describe a record with billions of leaves. We have measured a 624-byte file
that stops a lookup returning at all, which takes the calling thread with it.

What factbook bounds today: the map of unmodelled source fields on each
record, at 512 fields and 64 KiB, enforced by stopping the decode as the
limit is reached rather than after. That closes the memory exhaustion and
most of the time cost.

What it does not bound: the decode of the fields the record models by name,
which walks the same pointer graph inside the `maxminddb` reader. A
sufficiently deep crafted file still hangs a lookup there.

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

**Security reports**: security@hyperi.io

For non-security issues, please use the project's issue tracker.
