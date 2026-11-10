# damper Phase 1b: distribution — design

Status: draft for the owner's approval, 2026-11-09. Issue: [#2](https://github.com/pecheriere/damper/issues/2).
Parent design: `docs/superpowers/specs/2026-11-08-damper-design.md` (the "main spec").

Tags: (owner's decision) the owner decided it. (spec) already decided in the main spec. **[call]** Claude's call, open to the owner's veto. **[guess]** an assumption the builder confirms before relying on it.

## 1. Intent

The Phase 1 engine works, but the only way to install it is from source, with a hand-made `PATH` entry and a `damperd` left running in a terminal. Phase 1b packages damper so that one `brew install` puts every cargo build on the Mac, the owner's and the agents', through damper. Uninstalling must leave the Mac as it was. (owner's decision)

The goal of damper as a whole is to make full use of the Mac's CPU and memory without the owner ever noticing a slowdown. (owner's decision) Phase 1b doesn't change the engine's behaviour. It makes damper the thing that actually runs.

## 2. Goals

1. **Release pipeline.** A `v*` tag the owner pushes builds the macOS arm64 binaries (`damper`, `damperd`, `damper-shim`), attests them, and leaves them on a draft GitHub release. The owner signs the manifest with a key that isn't on GitHub (the on-disk git signing key for now, §4), then publishes the release. (spec)
2. **One approval.** The owner approves a release once, on damper's side: the signature, plus publishing the draft. The tap then picks up the release with no further action from the owner. (owner's decision) The tap's hourly `bump` workflow already works this way for warden.
3. **`brew install damper` does everything Homebrew allows.** (owner's decision)
   - It installs the shim as `cargo` in Homebrew's `bin`, so every `cargo` on the Mac goes through damper.
   - It ships a `service` block, so `brew services start damper` runs `damperd` as a launchd user agent at login.
   - Its caveats print the one step Homebrew won't let a formula do: `brew services start damper`.
4. **Clean removal.** A `damper cleanup` command stops and unloads the launchd agent, removes its plist, and deletes damper's state directory. The caveats and docs tell users to run it before `brew uninstall damper`. (owner's decision): uninstall must clean up. The mechanism is a **[call]**, because Homebrew limits what a formula can do on uninstall (§4).
5. **Minimum macOS** is pinned in the release build and in the formula. (spec)
6. **Docs.** `docs/usage.md` and the README lead with the brew install. Building from source stays as a section for development. (spec)

## 3. Non-goals

- **Switching the owner's own setup.** The owner's dotfiles shim (`~/.dotfiles-bin/cargo`, `nice -n 15`) and the "at most about three building agents" rule in `rules/rust.md` both stay until the memory acceptance run ([#3](https://github.com/pecheriere/damper/issues/3)) passes. That run is tracked separately in #3. (owner's decision) Note that installing damper is the cutover anyway: on the owner's `PATH`, `/opt/homebrew/bin` comes before `~/.dotfiles-bin` and `~/.cargo/bin`. The damper shim runs first and execs the next `cargo` on `PATH`, which is the dotfiles shim, which runs rustup's proxy. The chain is harmless. Its only cost is that `nice -n 15` stacks on top of utility QoS. (owner's decision: accepted)
- **A GitHub environment approval on the release workflow.** Issue #2 lists one. It's dropped: the owner wants a single approval, and a second click adds friction without adding security, because anything running as the owner could approve it (§4). (owner's decision)
- Linux and Intel builds. The main spec makes damper Apple Silicon only. (spec)
- The menu bar app and the tap's first cask (later phase). (spec)
- Engine changes, apart from the `damper cleanup` command and the `doctor` checks in §5.4.

## 4. Constraints and facts the design rests on

- **Homebrew won't start a service at install time.** `brew services start` is always a separate command, so the caveats name it. Until it runs, the shim finds no daemon and execs plain cargo. (spec: fail open)
- **`brew uninstall` doesn't stop a running service** (checked in Homebrew 7.0.12's `uninstall.rb`). The launchd agent stays loaded, and `~/Library/LaunchAgents/homebrew.mxcl.damper.plist` stays behind, pointing at a binary that's gone. Formulae get no uninstall hook, and `zap` exists only for casks. That's why `damper cleanup` exists.
- **The shim already detects itself through symlinks and hardlinks** (`crates/shim/src/resolve.rs`), so Homebrew's `bin/cargo → Cellar/…/damper-shim` link doesn't recurse. It must not canonicalize the cargo it finds, because rustup picks the toolchain from argv[0]. (spec) The builder adds an e2e or integration test for the Homebrew layout: the shim reached through an `opt/` symlink into a Cellar path, with a second `cargo` later on `PATH`.
- **Brew's `rust` formula also installs `bin/cargo`.** The formula declares `conflicts_with "rust"`, so installing damper next to a linked `rust` gives a clear message. **[call]** `rust` is also a build dependency of Rust formulae (`cargo-nextest`, which the owner has from a bottle). If one is built from source while damper is installed, Homebrew installs `rust` and its link step fails, but builds use `opt/rust` and still work. The builder confirms this **[guess]** and documents it. Homebrew's `rustup` formula is keg-only and doesn't clash. **[guess]**
- **The tap's `scripts/formula.py` is hard-wired to warden** (repo, namespace, filename and template). Landfall has its own `landfall.py`. damper needs the same: either generalise the renderer or add `damper.py`. The builder picks, and follows the tap's `AGENTS.md`. A tap change that isn't a pure formula bump needs every commit signed by a key in `maintainer-signers` (`scripts/scope.py`). (tap rule)
- **Release signing key: the owner's on-disk git signing key, for now.** This is the same key warden uses: warden's `docs/release-signers`, the tap's `release-signers` and the tap's `maintainer-signers` all list the key that git's `user.signingkey` names. (owner's decision, 2026-11-09) Because it is git's default signing key, `git tag -s` signs the tag with it and `sign-release.sh` can default to it, exactly as warden's does. The signers entry carries `namespaces="git,damper-release"`, so one entry covers the tag and the manifest.
  - **Accepted risk (owner's decision):** anything running as the owner can use this key, agents included, so a signature isn't proof that the owner approved a release. Approval rests on the rule that only the owner tags and signs releases. A GitHub environment approval wouldn't close this gap: the same user token can approve a deployment through the API. The owner plans to move release signing to a key that asks each time, once warden can broker it. That move is a key rotation (new key in both signers files) and changes nothing else in this design.
- **Immutable releases** are on for the repo, so the signed manifest must be uploaded to the draft before publishing, as with warden. (repo setting)
- **crate-age.** The release workflow gates on CI and crate-age, as warden's does. warden's release job checks every version added since the previous release tag, and all of them when there is none. PR #1 merged with 9 versions that hadn't yet aged 7 days, under a recorded owner override (PR #1 comment). They clear by 2026-11-16. The first release may go out before then. (owner's decision, 2026-11-09) Mechanism **[call]**: when there's no previous tag, the release job compares against the commit where PR #1 merged (`2cf5202`), not the empty tree, with a comment linking the override. Versions added after that merge are still checked. The allow-list can't be used, because it only takes advisory fixes.
- **No publish job, so no environment reviewer.** The public-repo baseline puts publish jobs behind a GitHub environment with a required reviewer. This workflow never publishes: it leaves a draft, and the owner publishes it by hand after signing. So there's no publish job to gate. (owner's decision: one approval)

## 5. Design

### 5.1 Release workflow (`.github/workflows/release.yml`)

Modelled on warden's `release.yml`, keeping only what damper needs: no container image, no .deb or .rpm, no server and no demo. (spec)

- Trigger: a `v[0-9]*` tag. An `on-main` job checks that the tagged commit is on `main`. Then come the full CI and the crate-age check.
- Build on a native `macos-*` arm64 runner, with `MACOSX_DEPLOYMENT_TARGET` set to the minimum (§5.5). Build one archive, `damper-<version>-macos-arm64.tar.gz`, holding the three binaries plus the licences and the README.
- An attest job that builds nothing: it checks the artifact digests it received and runs `actions/attest-build-provenance` in the same job, warden's pattern.
- A final job creates the **draft** release with the archive. The workflow never publishes.
- Pin every action by SHA, set least-privilege `permissions` per job, and get zero findings from `uvx zizmor .` (owner's rule)
- Port `repeatable-build.yml` from warden if it applies to a single macOS archive. That's the builder's call; record why if it's skipped.

### 5.2 Signing and publishing (`scripts/sign-release.sh`, `docs/release-signers`, `docs/verifying-releases.md`, `AGENTS.md`)

Port warden's `sign-release.sh`. It defaults to git's `user.signingkey` and refuses to sign unless that key is listed in `docs/release-signers`, the tag carries a good signature from the same key, the draft holds exactly the expected files, and the archive has a build attestation from this workflow at that tag and commit. It then uploads `damper-<version>-SHA256SUMS` and its `.sig`. Add warden's "Releases" section to `AGENTS.md`, adapted. The owner's steps, which are the whole of the owner's approval:

1. `git tag -s vX.Y.Z && git push origin vX.Y.Z`
2. Wait for the draft, then run `scripts/sign-release.sh vX.Y.Z`.
3. Publish the draft.

### 5.3 Tap (`pecheriere/homebrew-tap`, separate PR)

- A damper renderer (§4) that verifies the manifest signature against the tap's own copy of damper's signers, refuses to downgrade, and renders `Formula/damper.rb`.
- A `release-signers`-style file for damper (the same key as warden's today, under the `damper-release` namespace), its entry in `FORMULAE` and the `bump` matrix, and CI tests mirroring `test_formula.py`.
- damper builds only for macOS arm64, while the existing formula jobs also run two Linux legs. So damper gets its own jobs, not a leg in the existing matrix, which would rename checks `main` already requires. Those new jobs (e.g. `damper-manifest`, `damper-formula (macos-arm64)`) must be added to the tap's `main` ruleset as required checks before the first damper bump can merge. Otherwise a bump with a bad signature could auto-merge. **[call]**: a repo-settings change, so it waits for the owner's OK when the tap PR lands (§8)
- The first `Formula/damper.rb` is committed by the maintainer, as landfall's first formula was. Every later bump is automatic.
- Formula contents:
  - `depends_on arch: :arm64`, `depends_on macos: :<minimum>`, `conflicts_with "rust"`.
  - Install `damper` and `damperd` into `bin`, and `damper-shim` into `bin` as `cargo`. (owner's decision: brew install does everything)
  - `service do`: run `opt_bin/"damperd"`, `keep_alive true`, with no `log_path` or `error_log_path`. `damperd` writes everything it needs to its own log in the state dir, which `damper cleanup` removes. A log under Homebrew's `var/log` would survive both `damper cleanup` and `brew uninstall`. **[call]** If launchd discarding stderr hides startup failures, `damperd` must write them to its state-dir log before exiting. Don't use `process_type :background`: launchd would throttle `damperd` itself, and it has to sample promptly. The builder picks the right process type. **[call]**
  - Caveats: run `brew services start damper`, run `damper doctor` to check, run `damper cleanup` before uninstalling, and `damper disable` / `DAMPER=off` to turn it off.
  - `test do`: `damper --version` and `damperd --version` match the formula version. Running `cargo --version` through the shim in the test sandbox, with no daemon present, proves the fail-open path. **[call]**

### 5.4 Engine additions (damper repo)

- **`damper cleanup`**: stops and unloads the launchd agent (`launchctl bootout gui/<uid>/homebrew.mxcl.damper`), removes the plist from `~/Library/LaunchAgents`, then removes damper's state directory (`Paths::state`, including `run/`, `log/`, `config` and `disabled`). Rules:
  - Idempotent: running it twice, or with nothing installed, succeeds and says what it found.
  - It prints each thing it removes.
  - It removes only paths it resolves itself, under the passwd home or `DAMPER_STATE_DIR`, and refuses symlinked state dirs, matching `paths.rs`'s private-dir checks. It never removes anything outside them.
  - Builds running at that moment must still complete: their shims lose the daemon and feed themselves (existing fail-open). (spec)
  - Whether it also asks before deleting the config file is a **[call]** for the builder. The default is to delete without asking, since the user asked for cleanup.
- **`damper doctor`** gains two checks. **[call]**
  - The first `cargo` on `PATH` is the shim. If not, say which `cargo` wins and why.
  - The launchd agent is loaded and `damperd` answers. If not, print `brew services start damper`.
- `--version` on all three binaries, if not already present, for the formula test.

### 5.5 Minimum macOS

The main spec's rule: the oldest macOS that still gets security updates when v1 ships. Today's macOS is 27, and Apple usually patches the two releases before the current one, which makes **macOS 15** the minimum. **[guess]** Check Apple's security-releases page at build time. Also check that the APIs damper uses (libproc `kproc_info_raw`, memory-pressure sources, `taskpolicy` QoS) exist on macOS 15. Set it in three places that must agree: `MACOSX_DEPLOYMENT_TARGET` in the release build, `depends_on macos:` in the formula, and a line in the README. If CI can run the test suite on a macOS 15 runner, add it to the matrix. **[call]**

### 5.6 Upgrades

The main spec already covers this (row "`brew upgrade` during builds"): running sessions finish untouched, and a new `damperd` either adopts sessions or lets their shims feed themselves. The builder checks whether `brew upgrade` restarts a running service **[guess]**, and documents what the user must do (`brew services restart damper`) if it doesn't.

### 5.7 Docs

- README: install is `brew install pecheriere/tap/damper`, then `brew services start damper`, then `damper doctor`. Uninstall is `damper cleanup`, then `brew uninstall damper`. Add the minimum macOS and Apple Silicon only.
- `docs/usage.md`: "Install" leads with Homebrew. The current from-source steps move to a "Building from source" section. Remove "There is no `brew services` support yet". Add an "Uninstall" section.
- `docs/verifying-releases.md`: how to check the signature and the attestation, ported from warden.

## 6. What "done" looks like

- A dry run on a fork or a pre-release tag shows the workflow leaving a correct draft with an attestation that `gh attestation verify` accepts. The owner decides whether a throwaway pre-release tag on the real repo is acceptable. A test tag on the real repo is the owner's action, never an agent's.
- The tap PR's CI renders and checks `Formula/damper.rb` from a signed test manifest.
- After the first real release, on the owner's Mac:
  - `brew install` puts the shim first, so `which cargo` shows `/opt/homebrew/bin/cargo`.
  - `brew services start damper` starts `damperd`, and `damper doctor` is clean.
  - A build goes through a session.
  - `damper cleanup && brew uninstall damper` leaves no launchd agent, plist, state dir or `cargo` shim behind, and `cargo` resolves to the dotfiles shim again.

## 7. Left to the builder

- Splitting the work into PRs. Expected: damper repo (release workflow, signing script, `cleanup`, `doctor`, docs) and tap repo (renderer, signers, first formula).
- Generalising `formula.py` versus adding `damper.py`.
- Whether to port `repeatable-build.yml`.
- The launchd process type for `damperd`.
- Exact `doctor` wording.

## 8. Needs the owner (not agent work)

- Tag, sign and publish the first release.
- Approve the tap ruleset change that makes the damper check jobs required (§5.3). Agents make the change only after that approval.
- The first `Formula/damper.rb`: the tap's `AGENTS.md` has the maintainer commit a project's first formula. `maintainer-signers` lists the on-disk key, so this is a policy step, not a technical one. The owner either commits it or tells the agent to.
