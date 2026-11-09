# Contributing to damper

Contributions are welcome: bug reports, documentation, tests and code. damper
has one maintainer, so a reply can take a while.

Public discussion happens in
[GitHub issues](https://github.com/pecheriere/damper/issues) and pull requests.
Never open a public issue for a suspected vulnerability: follow the
[security policy](SECURITY.md).

## Making a change

1. Find or open an issue first, unless the change is small.
2. Read [`AGENTS.md`](AGENTS.md). It holds the rules for people and coding
   agents alike: ground rules, the layout and the checks to run.
3. Open one pull request per issue and fill in
   [the template](.github/pull_request_template.md). Say what you tested and
   what you did not.

## Sign-off (DCO)

damper uses the [Developer Certificate of Origin](https://developercertificate.org/).
Every commit ends with a `Signed-off-by` line saying that its author has the
right to contribute the change under the project's licenses:

```sh
git commit -s
```

The line is public for good, so use an address you are happy to publish, such as
GitHub's `users.noreply.github.com` address.

## Dependency age

CI refuses a pull request whose `Cargo.lock` brings in a crate version published
less than 7 days before the pull request. A newer release that fixes an advisory
(`RUSTSEC-...` or `GHSA-...`) may be allowed with an entry in
[`.github/crate-age-allow.toml`](.github/crate-age-allow.toml), one per crate
version, each with a reason. Remove the entry when the crate is updated.

## Licensing

Contributions are licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at the user's option.
