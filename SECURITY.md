# Security Policy

## Supported versions

sabre is pre-1.0. Only the latest release receives security fixes.

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Report privately via [GitHub Security Advisories](https://github.com/IndawoMaps/sabre/security/advisories/new),
or by email to info@indawomaps.com.

Please include the affected version, a description of the impact, and reproduction steps.
You can expect an initial response within a week.

## Scope notes

sabre parses untrusted binary input (TIFF headers, compressed tile streams) and fetches
attacker-influenceable URLs on behalf of callers. Reports in these areas are especially
welcome:

- Memory-safety or panic-on-malformed-input bugs in the TIFF and compression decoders
- Server-side request forgery via the `url` parameter — a deployment that exposes sabre
  publicly should restrict which hosts it will fetch from
- Resource exhaustion through crafted headers, overview counts, or query geometry
