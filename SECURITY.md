# Security policy

damper runs as a per-user daemon and wraps cargo, so reports are welcome and
taken seriously. It has one maintainer, who is also the security contact. This
policy promises only what one person can keep.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub security advisories:
<https://github.com/pecheriere/damper/security/advisories/new>. Only the
maintainer and the people added to the report can read it. You need a GitHub
account.

Please do not open a public issue or pull request for a suspected
vulnerability. Include the version, your macOS version and steps to reproduce.
Do not include real secrets or private details of your systems.

## Response time

You get a first response **within 14 days** of your report, acknowledging it and
saying what happens next. If you have heard nothing after 14 days, add a comment
to the report. If that gets no answer either, open a public issue asking the
maintainer to look at it, without any details of the vulnerability.

There is no fixed deadline for the fix, because it depends on the problem.

## How reports are handled

1. **Triage.** The maintainer reproduces the problem in the private report,
   decides whether it is a vulnerability and how severe it is, and tells you the
   outcome.
2. **Fix.** The fix is prepared privately where possible, and you are welcome to
   review it. It ships in a new release.
3. **Disclosure.** The date is agreed with you. Nothing is disclosed before the
   fix is available, unless the vulnerability is already being exploited or is
   already public.
4. **Publication.** An advisory is published when the release is out.

## Credit

Reporters are credited by name or GitHub account in the advisory, unless they
ask not to be.

## Supported versions

Only the latest release receives security fixes. damper is before 1.0, so the
release with a fix can also contain breaking changes.
