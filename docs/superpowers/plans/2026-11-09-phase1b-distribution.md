# damper Phase 1b: distribution — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship damper as a signed GitHub release that the owner's Homebrew tap turns into a formula. One `brew install` routes every cargo through damper, `brew services` runs `damperd`, and `damper cleanup` undoes it all.

**Architecture:** Two repositories, two PRs.
- **damper** (Tasks 1–6): engine additions (`damperd --version`, `damper cleanup`, a launchd check in `doctor`, a Homebrew-layout shim test), then the release pipeline, ported from warden but cut down to one macOS arm64 archive: `release.yml`, `sign-release.sh` and its test, signers and verification docs. Then the docs.
- **homebrew-tap** (Tasks 7–8): `scripts/formula.py` learns a second project, `damper`, plus its own CI jobs and bump-matrix entry.

The first `Formula/damper.rb` exists only after the first release, so it isn't part of this plan (§8 of the spec).

**Tech Stack:** Rust 1.99.1 (workspace, clap), GitHub Actions, bash, Python 3 (stdlib only, `-I`), Homebrew Ruby DSL.

**Spec:** `docs/superpowers/specs/2026-11-09-phase1b-distribution-design.md`. The main spec is `docs/superpowers/specs/2026-11-08-damper-design.md`. Read the Phase 1b spec before any task.

## Global Constraints

- Apple Silicon only; minimum macOS 15. `MACOSX_DEPLOYMENT_TARGET=15.0` in the release build and `depends_on macos: :sequoia` in the formula. damper's CI already runs on `macos-15`.
- Follow damper's `AGENTS.md` in full:
  - No `unsafe`, no `#[allow]` without a `reason`.
  - Clippy pedantic is denied.
  - Binaries print through a locked `std::io::Write` handle.
  - Never log env vars or argument lists.
  - Files stay under about 800 lines.
  - Commits use `git commit -s` with conventional subjects and no AI attribution.
- Never set `CARGO_TARGET_DIR` or any `CARGO_*` variable. Iterate with `cargo nextest run -p <crate>`. Run the full gate from `AGENTS.md` once before each commit. Keep output short (`--quiet`, `| tail -50`).
- Workflow rules:
  - Every action is pinned by full SHA to a release at least 7 days old.
  - Permissions are `contents: read` at the top, with per-job grants.
  - Every checkout sets `persist-credentials: false`.
  - No build cache in release jobs.
  - `uvx zizmor .` reports no findings.
  - Copy pins from warden's workflows, which already satisfy these rules.
- This repo is public. No private hostnames, user names or paths in code, tests, docs or commit messages: use `/Users/example`, `build-agent` and similar.
- Release signing uses the owner's on-disk git signing key (git's `user.signingkey`), under namespace `damper-release`. Agents never tag, sign or publish a release.
- Launchd label: `homebrew.mxcl.damper`. Plist: `~/Library/LaunchAgents/homebrew.mxcl.damper.plist`.
- The workspace version is `0.1.0`, and the first tag is `v0.1.0`.

## Review Focus

1. **`damper cleanup` with a symlinked or foreign-owned state dir:** refuse to delete through it, say why, and exit non-zero. Never follow a symlink out of the state dir. Tested in Task 2.
2. **`damper cleanup` while a build runs:** the build still finishes with its normal exit code, because its shim feeds itself. Tested in Task 2 with the e2e harness.
3. **`damper cleanup` run twice, or with nothing installed:** succeeds both times and reports "nothing to remove" for each item. Tested in Task 2.
4. **The shim reached through a Homebrew `opt/` symlink into a Cellar path, with rustup's cargo later on `PATH`:** it execs the later cargo with argv[0] `cargo`, never itself. Tested in Task 3.
5. **A release tag that doesn't match the workspace version, or isn't on `main`:** the workflow refuses before building. Tested in Task 4 with a dry-run check script.

---

## damper repository

Work in a fresh worktree on branch `feat/phase1b-distribution`, cut from `main` after the design PR merges.

### Task 1: `damperd --version` and the launch-agent path

**Files:**
- Modify: `crates/daemon/src/main.rs`
- Modify: `crates/proto/src/paths.rs` (add field, ~line 85 and ~line 150)
- Test: `crates/proto/src/paths.rs` tests module (or its existing `tests.rs`), `crates/e2e` for the binary

**Interfaces:**
- Produces:
  - `Paths::launch_agent: PathBuf`. It's `<pw_dir>/Library/LaunchAgents/homebrew.mxcl.damper.plist`. With a state override, it's `<override>/LaunchAgents/homebrew.mxcl.damper.plist`, so tests never touch the real one.
  - `pub const LAUNCHD_LABEL: &str = "homebrew.mxcl.damper";` in `damper_proto::paths`.
  - `damperd --version` prints `damperd 0.1.0` to stdout and exits 0. Any other argument prints usage to stderr and exits 2.

- [ ] **Step 1:** Write the failing tests:
  - `launch_agent_is_under_library_launchagents`: no override, path ends with `Library/LaunchAgents/homebrew.mxcl.damper.plist` and starts with the passwd home.
  - `launch_agent_follows_the_override`: with an override, path == `<override>/LaunchAgents/homebrew.mxcl.damper.plist`.
  - e2e `damperd_version_prints_the_package_version`: runs the built `damperd --version`, asserts stdout == `format!("damperd {}\n", env!("CARGO_PKG_VERSION"))` and exit 0.
  - e2e `damperd_rejects_unknown_arguments`: `damperd --bogus` exits 2.
- [ ] **Step 2:** Run `cargo nextest run -p damper-proto -p damper-e2e`. Expected: the four tests FAIL.
- [ ] **Step 3:** Add the field and the const. In `damperd`, parse args with clap derive (`#[command(name = "damperd", version)]`, no other options) before `start()`. clap is already a workspace dependency, so add it to the daemon with `cargo add -p damperd clap --features derive`.
- [ ] **Step 4:** Run the same command. Expected: PASS.
- [ ] **Step 5:** Run the full gate, then commit `feat: damperd --version and the launch-agent path`.

### Task 2: `damper cleanup`

**Files:**
- Create: `crates/cli/src/cleanup.rs`, `crates/cli/src/cleanup/tests.rs`
- Create: `crates/cli/src/launchd.rs` (the `Launchd` trait and its `launchctl` implementation, shared with Task 3)
- Modify: `crates/cli/src/main.rs` (new `Command::Cleanup`, wired like `Doctor`)
- Test: `crates/cli/src/cleanup/tests.rs`, `crates/e2e/tests/cleanup.rs`

**Interfaces:**
- Consumes: `Paths::launch_agent`, `LAUNCHD_LABEL` (Task 1).
- Produces:
  - In `launchd.rs`:
    ```rust
    pub enum Loaded { Yes, No }
    pub enum Bootout { Stopped, NotLoaded }
    pub trait Launchd {
        fn is_loaded(&self, label: &str) -> std::io::Result<Loaded>;
        fn bootout(&self, label: &str) -> std::io::Result<Bootout>;
    }
    pub struct Launchctl; // runs /bin/launchctl, domain gui/<getuid()>
    ```
    `is_loaded` runs `launchctl print gui/<uid>/<label>`. `bootout` runs `launchctl bootout gui/<uid>/<label>`. Map launchctl's "not loaded" result to `No` / `NotLoaded`, and any other failure to `Err`. Check launchctl's exit codes for each case on the Mac, and record them as consts with a comment. Never pass user input to launchctl: the label is the const.
  - In `cleanup.rs`:
    ```rust
    pub enum Removed { Done, NothingThere }
    pub struct Step { pub what: &'static str, pub path: Option<PathBuf>, pub result: Result<Removed, String> }
    pub fn cleanup(paths: &Paths, launchd: &dyn Launchd) -> Vec<Step>
    ```
    The steps run in order:
    1. Stop the agent (`bootout`).
    2. Remove `paths.launch_agent`, a regular file only.
    3. Remove `paths.config`, then its parent directory if it's empty, and only when there's no override.
    4. Remove `paths.state` recursively.
  - The CLI prints one line per step: `removed <path>`, `nothing at <path>`, `stopped homebrew.mxcl.damper`, `homebrew.mxcl.damper was not loaded`, or `could not <what>: <reason>`. It exits 0 when every step is `Ok`, and 1 otherwise. Every step runs even if an earlier one failed.

- [ ] **Step 1:** Write the failing unit tests, using a fake `Launchd` and a tempdir as the state override:
  - `cleanup_removes_everything_it_owns`: agent plist, config and state tree all exist. After cleanup, none exist, every step is `Ok(Done)`, and the fake saw one `bootout(LAUNCHD_LABEL)`.
  - `cleanup_twice_reports_nothing_there`: a second run gives all `Ok(NothingThere)` and the bootout step `NotLoaded`.
  - `cleanup_refuses_a_symlinked_state_dir`: state is a symlink to a dir holding a sentinel file. The step is `Err`, the sentinel still exists, and the exit is 1.
  - `cleanup_refuses_a_state_dir_owned_by_someone_else`: simulate it through the same ownership check `paths.rs` uses (`ensure_private_dir`'s rules). Expose a `pub(crate)` check from `damper_proto::paths` rather than duplicating it.
  - `cleanup_refuses_a_symlinked_launch_agent`: the plist path is a symlink. Leave it in place, and the step is `Err`.
  - `cleanup_continues_after_a_bootout_error`: the fake bootout returns `Err`. Files are still removed and the exit is 1.
- [ ] **Step 2:** Run `cargo nextest run -p damper`. Expected: FAIL (module missing).
- [ ] **Step 3:** Implement `launchd.rs`, `cleanup.rs` and `Command::Cleanup` (doc comment: `/// Stop the daemon's launch agent and remove damper's files, before brew uninstall.`).
  - Recursive removal must not follow symlinks inside the state dir: use `symlink_metadata`, and unlink a symlink rather than descending into it. `std::fs::remove_dir_all` already doesn't follow symlinks. Use it after the top-level checks, and say so in a comment.
- [ ] **Step 4:** Run the same command. Expected: PASS.
- [ ] **Step 5:** Write the e2e test `cleanup_mid_build_lets_the_build_finish` in `crates/e2e/tests/cleanup.rs`. Start the daemon and a `fake-cargo` build through the shim, using the harness the signal tests use. Run `damper cleanup` with the same state override and a fake launchd: add a hidden `DAMPER_TEST_NO_LAUNCHCTL=1` switch only if the e2e harness can't inject one, and document it as test-only in `docs/usage.md`'s environment table. Assert the build exits 0 and the state dir is gone afterwards.
- [ ] **Step 6:** Run `cargo nextest run -p damper-e2e cleanup`. Expected: PASS.
- [ ] **Step 7:** Run the full gate, then commit `feat: damper cleanup`.

### Task 3: `doctor` launchd check and the Homebrew-layout shim test

**Files:**
- Modify: `crates/cli/src/doctor.rs` (new check `fn launch_agent(...) -> Check`; extend the `daemon` failure detail), `crates/cli/src/doctor/tests.rs`
- Create: `crates/e2e/tests/homebrew_layout.rs`

**Interfaces:**
- Consumes: `Launchd`, `Loaded`, `Launchctl` (Task 2); `LAUNCHD_LABEL` (Task 1).
- Produces:
  - `doctor` takes `&dyn Launchd`: `pub fn doctor(paths: &Paths, path_var: &OsStr, launchd: &dyn Launchd) -> Vec<Check>`. `main.rs` passes `&Launchctl`.
  - The `launch agent` check runs only when the first cargo on `PATH` canonicalizes to a path containing `/Cellar/damper/` (a Homebrew install). Otherwise it's omitted, so source installs aren't told to use brew.
    - Loaded: ok, `homebrew.mxcl.damper is loaded`.
    - Not loaded: fail, `homebrew.mxcl.damper is not loaded; run: brew services start damper`.
  - The `daemon` check's failure detail ends with `; start it with: brew services start damper` under the same Homebrew condition.

- [ ] **Step 1:** Write the failing tests in `doctor/tests.rs`, building a fake Cellar layout in a tempdir: `<tmp>/Cellar/damper/0.1.0/bin/damper-shim` and `<tmp>/bin/cargo -> ../Cellar/damper/0.1.0/bin/damper-shim`, with `<tmp>/bin` on the test `PATH`.
  - `launch_agent_check_appears_for_a_homebrew_install`, with the fake reporting `Yes` → ok.
  - `launch_agent_check_names_brew_services_when_not_loaded`, with `No` → fail, detail contains `brew services start damper`.
  - `launch_agent_check_is_absent_for_a_source_install`: shim outside any `Cellar` → no check named `launch agent`.
- [ ] **Step 2:** Write the failing e2e test `shim_via_homebrew_symlink_execs_the_next_cargo` in `homebrew_layout.rs`, with the same layout:
  - The real built `damper-shim` is copied into the fake Cellar, with an `opt/damper -> Cellar/damper/0.1.0` symlink and `bin/cargo -> ../opt/damper/bin/damper-shim`.
  - A second dir later on `PATH` holds `cargo`, a `fake-cargo` that records its argv[0] and exits 0.
  - Run `<tmp>/bin/cargo --version` with `DAMPER=off`. Assert the exit is 0, the recorded argv[0] is the later dir's `cargo` path, not canonicalized, and the shim didn't recurse: one record only.
- [ ] **Step 3:** Run `cargo nextest run -p damper -p damper-e2e`. Expected: the new tests FAIL. The e2e test may already pass, since `resolve.rs` handles symlinks. If so, keep it as the regression test and note that in the commit body.
- [ ] **Step 4:** Implement the check and the detail. Keep `doctor.rs` under 800 lines.
- [ ] **Step 5:** Run the same command. Expected: PASS.
- [ ] **Step 6:** Run the full gate, then commit `feat: doctor checks the launch agent of a Homebrew install`.

### Task 4: Release workflow

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `.github/scripts/release_tag.py`, `.github/scripts/test_release_tag.py` (tag/version check, unit-tested in CI's existing script-test step)
- Modify: `.github/workflows/ci.yml` only if `workflow_call` needs inputs (it already has `workflow_call:`)

**Interfaces:**
- Consumes: `ci.yml` via `uses: $/.github/workflows/ci.yml`; `.github/scripts/crate_age.py` (`--base REV | --no-base`).
- Produces: a draft release `vX.Y.Z` holding exactly `damper-X.Y.Z-macos-arm64.tar.gz`. The archive contains `damper-X.Y.Z-macos-arm64/{damper,damperd,damper-shim,LICENSE-MIT,LICENSE-APACHE,README.md}`. It has a build-provenance attestation for that archive, and the job output `sums` holds its SHA-256. Task 5's `sign-release.sh` relies on these exact names.

Jobs, in order. Port each from warden's `release.yml` (same line ranges as its jobs of the same name) and remove what doesn't apply:
- `on-main`: refuse a tag whose commit isn't on `origin/main`. Then run `python3 -I .github/scripts/release_tag.py "$GITHUB_REF_NAME" Cargo.toml`, which refuses a tag that isn't `v` + `[workspace.package] version`.
- `crate-age`: `needs: on-main`. With no earlier `v*` tag merged into `HEAD^`, run `crate_age.py --base 2cf5202` with this comment: `# The first release: PR #1 merged under the owner's recorded crate-age override (see the PR #1 comment); versions added since are checked.` Otherwise run `--base <previous tag>`, as warden does.
- `ci`: `needs: on-main`, `uses: $/.github/workflows/ci.yml`.
- `build`: `needs: [on-main, crate-age, ci]`, `runs-on: macos-15`, `env: MACOSX_DEPLOYMENT_TARGET: "15.0"`.
  - No cache. Run `cargo build --release --locked -p damper -p damperd -p damper-shim`.
  - Assemble the archive.
  - Check each binary's minimum OS with `vtool -show-build` (or `otool -l`, `LC_BUILD_VERSION minos 15.0`) and fail if it doesn't match.
  - Upload as an artifact.
- `attest`: `needs: build`, `permissions: {id-token: write, attestations: write}`. Download, check the digest against `build`'s output, attest in the same job, and output `sums`, as warden's `attest-cli` does.
- `draft-release`: `needs: attest`, `permissions: contents: write`. Refuse anything but the attested archive. Create a draft with `gh release create "$TAG" --draft --verify-tag --notes-file <CHANGELOG section>` (CHANGELOG from Task 6) and upload the archive.
- `report-incomplete`: as warden's.

- [ ] **Step 1:** Write the failing tests in `test_release_tag.py`:
  - `test_accepts_matching_tag` (`v0.1.0` with version `0.1.0`)
  - `test_refuses_mismatch` (`v0.1.1`)
  - `test_refuses_missing_v` (`0.1.0`)
  - `test_refuses_suffix` (`v0.1.0-rc1`, since releases are plain semver)
  - Each refusal exits non-zero with a message naming both values.
- [ ] **Step 2:** Run `python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'`. Expected: FAIL.
- [ ] **Step 3:** Implement `release_tag.py` (stdlib `tomllib`, ruff-clean with `.github/ruff.toml`) and `release.yml`.
- [ ] **Step 4:** Run the unit tests, then `ruff check`/`ruff format --check` as CI does, then `uvx zizmor .` and `actionlint .github/workflows/release.yml` if installed. Expected: PASS, no findings.
- [ ] **Step 5:** Commit `ci: release workflow for macOS arm64 archives`. A real run needs a tag, which only the owner makes. Record in the PR that the workflow is unexercised until the first tag.

### Task 5: Release signing and verification

**Files:**
- Create: `scripts/sign-release.sh` (port of warden's, 154 lines), `scripts/test-sign-release.sh` (port of warden's test, 315 lines)
- Create: `docs/release-signers`, `docs/verifying-releases.md` (port of warden's, image parts removed)
- Modify: `.github/workflows/ci.yml` (run `scripts/test-sign-release.sh` in `lint`, as warden's CI does), `AGENTS.md` (add a "Releases" section)

**Interfaces:**
- Consumes: the draft layout from Task 4.
- Produces:
  - `scripts/sign-release.sh vX.Y.Z [public-key-file]` uploads `damper-X.Y.Z-SHA256SUMS` and `damper-X.Y.Z-SHA256SUMS.sig` (namespace `damper-release`) to the draft.
  - `docs/release-signers` holds one line: the public key from `git config user.signingkey`, with `namespaces="git,damper-release"` and principal `pecheriere@users.noreply.github.com` (warden's principal). The tap's copy (Task 7) must match it byte for byte after the header comment.

- [ ] **Step 1:** Port `test-sign-release.sh` first, swapping warden's names: archive set, namespace, no image attestation. Keep its stubbed-`gh` approach. It covers:
  - refusing an unsigned tag
  - refusing a tag signed by an unlisted key
  - refusing a draft with an extra or missing file
  - refusing an archive with no attestation for this workflow, tag and commit
  - refusing a published (non-draft) release
  - success uploading exactly the two files
- [ ] **Step 2:** Run `scripts/test-sign-release.sh`. Expected: FAIL (no script).
- [ ] **Step 3:** Port `sign-release.sh`. It defaults to git's `user.signingkey`, verifies the tag with `git verify-tag` against `docs/release-signers`, checks attestations with `gh attestation verify --signer-workflow pecheriere/damper/.github/workflows/release.yml --source-ref refs/tags/$TAG`, and signs with `ssh-keygen -Y sign -n damper-release`.
- [ ] **Step 4:** Run `scripts/test-sign-release.sh` and `shellcheck scripts/*.sh`. Expected: PASS.
- [ ] **Step 5:** Write `docs/release-signers` from `ssh-keygen -y -f "$(git config user.signingkey | sed 's/\.pub$//')"`, or read the `.pub` directly. Write `docs/verifying-releases.md`. In `AGENTS.md`, add the Releases section: warden's steps 1–5 adapted. Cover the release PR bump, the owner's signed tag, the workflow, `sign-release.sh`, `gh release edit --draft=false`, and the tap's hourly bump. Include warden's paragraph that an agent never tags or releases without a yes for that version and commit.
- [ ] **Step 6:** Run the full gate and `uvx zizmor .`, then commit `feat: release signing script and verification docs`.

### Task 6: Docs and changelog

**Files:**
- Modify: `README.md`, `docs/usage.md` (Install at lines 8–41, plus a new "Uninstall" section after "Turning damper off")
- Create: `CHANGELOG.md` (Keep a Changelog). `## [Unreleased]` lists Phase 1 and Phase 1b in user terms. The release PR renames it to `## [0.1.0] - <date>`, which is the owner's release step and not part of this plan.

- [ ] **Step 1:** README install block: `brew install pecheriere/tap/damper`, `brew services start damper`, `damper doctor`. Uninstall: `damper cleanup`, then `brew uninstall damper`. Requirements: Apple Silicon, macOS 15 or later.
- [ ] **Step 2:** `docs/usage.md`:
  - "Install" leads with Homebrew and says that `brew install` routes every `cargo` because Homebrew's `bin` comes first on `PATH`.
  - Name the conflict with brew's `rust` formula.
  - Move the current from-source steps under "### Building from source", and delete "There is no `brew services` support yet; it comes in Phase 1b."
  - New "## Uninstall": what `damper cleanup` removes (the agent, its plist, the state dir, the config) and why it must run before `brew uninstall`, since Homebrew doesn't stop services on uninstall.
- [ ] **Step 3:** Run `rg -n "Phase 1b|no .brew services" README.md docs/usage.md`. Expected: no matches.
- [ ] **Step 4:** Commit `docs: Homebrew install, uninstall and changelog`.

Then open the damper PR (`Closes #2` only after the tap PR also merges, so use `Part of #2`). Fill in the PR template and state what was verified locally versus CI only.

---

## homebrew-tap repository

Separate worktree, branch `feat/damper-formula`. Read the tap's `AGENTS.md` first. Every commit in this PR must be signed by a key in `maintainer-signers` (`scripts/scope.py`). The on-disk key qualifies.

### Task 7: `formula.py` renders damper

**Files:**
- Modify: `scripts/formula.py` (221 lines), `scripts/test_formula.py`, `scripts/scope.py:27` (`FORMULAE = ("warden", "landfall", "damper")`), `scripts/test_scope.py`
- Create: `damper-release-signers` (a header comment like `release-signers`, plus damper's `docs/release-signers` line)
- Create: `scripts/testdata/damper-*` fixtures (a signed test manifest from a throwaway key, as the warden fixtures are made)

**Interfaces:**
- Produces:
  - `python3 -I scripts/formula.py [--project warden|damper] render|check ...`. `--project` defaults to `warden`, and warden's behaviour and output stay byte-identical, because `main`'s CI runs the base's script.
  - The damper project: repo `pecheriere/damper`, namespace `damper-release`, signers file `damper-release-signers`, platforms `macos-arm64` only, manifest `damper-<v>-SHA256SUMS`.
  - Template:
    ```ruby
    class Damper < Formula
      desc "Machine-wide cargo jobserver sized by memory pressure"
      homepage "https://github.com/pecheriere/damper"
      license any_of: ["MIT", "Apache-2.0"]
      url ".../damper-<v>-macos-arm64.tar.gz"
      sha256 "<sum>"
      depends_on arch: :arm64
      depends_on macos: :sequoia
      conflicts_with "rust", because: "both install a `cargo` executable"
      def install
        bin.install "damper", "damperd"
        bin.install "damper-shim" => "cargo"
      end
      service do
        run opt_bin/"damperd"
        keep_alive true
      end
      def caveats
        # four lines: brew services start damper; damper doctor;
        # damper cleanup before brew uninstall; damper disable / DAMPER=off
      end
      test do
        assert_equal "damper #{version}", shell_output("#{bin}/damper --version").strip
        assert_equal "damperd #{version}", shell_output("#{bin}/damperd --version").strip
        # the shim with no daemon must fall back to the real cargo or fail as cargo would;
        # assert it doesn't hang: run `#{bin}/cargo --version` with a fake cargo later on PATH
      end
    end
    ```
    No `log_path` (spec §5.3). Keep the header comment warden's formula has. The builder picks the `service` process type after checking launchd's throttling for `Background`. The default (none) is fine unless testing shows otherwise.

- [ ] **Step 1:** Write the failing tests in `test_formula.py`:
  - `test_damper_renders_expected_formula`, a golden file in `testdata`
  - `test_damper_refuses_bad_signature`
  - `test_damper_refuses_key_not_in_damper_signers`, including warden's own signers file: the namespaces must not cross
  - `test_damper_refuses_downgrade`
  - `test_warden_output_unchanged`: the existing warden golden still matches with no `--project`
  - in `test_scope.py`: `test_damper_bump_is_in_scope`
- [ ] **Step 2:** Run `python3 -I -m unittest discover -s scripts -p 'test_*.py'`. Expected: the new tests FAIL.
- [ ] **Step 3:** Implement: a small `Project` dataclass table in `formula.py`, with no behaviour change for warden. Keep the file under 800 lines; if it grows past ~400, move the templates into `scripts/formula_templates.py`.
- [ ] **Step 4:** Run the same command. Expected: PASS.
- [ ] **Step 5:** Commit `feat: formula.py renders damper`.

### Task 8: Tap CI and bump for damper

**Files:**
- Modify: `.github/workflows/ci.yml` (new jobs `damper-manifest` and `damper-formula`), `.github/workflows/bump.yml` (damper matrix leg), `AGENTS.md` (file table rows, setup notes)

**Interfaces:**
- Consumes: `formula.py --project damper` (Task 7).
- Produces:
  - Check names `damper-manifest` and `damper-formula (macos-arm64)`. The owner adds them to the `main` ruleset's required checks; that's a settings change, and the controller asks first.
  - Both jobs skip cleanly (exit 0 with a notice) while `Formula/damper.rb` doesn't exist, so this PR can merge before the first release.

- [ ] **Step 1:** `damper-manifest`: copy the warden `manifest` job (ci.yml:60–101) with `--project damper` and `Formula/damper.rb`. Follow the same base-vs-change rule: the base's script judges unless the base has no damper support, in which case the change's script runs, as the landfall job does at ci.yml:216–223.
- [ ] **Step 2:** `damper-formula`: copy the `formula` job (ci.yml:140–178) with one leg, `macos-15` arm64, running `brew install --formula ./Formula/damper.rb`, `brew test damper` and `brew audit --strict damper`. Add a comment explaining why it's a separate job: folding it into the matrix would rename required checks.
- [ ] **Step 3:** In `bump.yml`, add damper to the matrix with `--project damper`. Bump skips damper until `Formula/damper.rb` exists, as it does for landfall (see tap `AGENTS.md`).
- [ ] **Step 4:** Run `uvx zizmor .` (no findings) and the script tests (PASS).
- [ ] **Step 5:** Update `AGENTS.md`: file-table rows for `damper-release-signers`, and a note that damper's first formula is committed by the maintainer after damper's first release, after which bumps are automatic. Commit `ci: check and bump the damper formula`.

Open the tap PR, linking damper #2. In the PR, say that the jobs are unexercised against a real release until v0.1.0.

---

## After both PRs merge (owner steps, not agent tasks)

1. A release PR in damper moves `[Unreleased]` to `[0.1.0]`.
2. The owner runs `git tag -s v0.1.0`, pushes the tag, waits for the draft, runs `scripts/sign-release.sh v0.1.0`, then `gh release edit v0.1.0 --draft=false`.
3. The owner, or an agent the owner tells to, commits the first `Formula/damper.rb` with `python3 -I scripts/formula.py --project damper render 0.1.0`. Then the owner approves making the damper check jobs required in the tap's `main` ruleset.
4. On the Mac:
   - `brew install pecheriere/tap/damper && brew services start damper && damper doctor`
   - `which cargo` shows `/opt/homebrew/bin/cargo`
   - A build shows a session in `damper status`.
   - `damper cleanup && brew uninstall damper` leaves no agent, plist, state dir or shim behind.
   - Close #2.

## Not done here, tracked

- Reproducible-build check for the macOS archive (warden's `repeatable-build.yml` is Docker/Linux based). The controller opens an issue for it during Task 4.
