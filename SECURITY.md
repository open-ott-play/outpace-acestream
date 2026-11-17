# Security policy

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/open-ott-play/outpace-acestream/security/advisories/new).
If the form is unavailable, email **alvit.work@gmail.com** privately. Public
issues are for non-sensitive defects and feature requests.

Include the affected commit/release, prerequisites, a minimal synthetic
reproduction, expected and actual results, and impact. Do not include live
credentials, personal data or access to devices you do not own.

## Response and supported versions

Maintainers aim to acknowledge a private report within 14 days, investigate its
scope, and agree on remediation and disclosure with the reporter. If no reply
arrives within 14 days, follow up privately. Confirmed security defects are
prioritized by impact; critical defects are handled immediately. Known medium-or-higher
vulnerabilities in supported project results are fixed within 60 days of public
disclosure. If an upstream fix is unavailable, document a reviewed mitigation or
disable the affected supported path rather than claiming it is clean.

Security fixes target the current default branch and latest published release,
where one exists. Older snapshots are not maintained security branches. Release
notes must identify security fixes, affected versions, available CVE/GHSA
identifiers and upgrade actions without
disclosing credentials. Credit reporters of fixed vulnerabilities unless they
request anonymity, and coordinate disclosure before publishing private details.
This policy is a commitment for handling reports, not
a claim that no vulnerabilities exist or that past reports met a response SLA.

## Project boundary

This project provides the Outpace AceStream engine, its container images and the
`outpace` executable derived from [Outpace](https://github.com/jamezrin/outpace).

The engine resolves and delivers public, unencrypted AceStream sources; it does
not decrypt protected content and is not an official Ace Stream product. Live
peer availability is outside the project's control. See [licensing](docs/licensing.md)
and `NOTICE` for upstream attribution and retained AGPL-3.0-or-later terms.
