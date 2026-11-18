# Contributing to corax

Thank you for considering a contribution!

## Ground rules

* **Nagios behaviour is the specification.** corax is a drop-in replacement; a change that
  makes corax behave differently from Nagios 3.5 / Livestatus needs a very good reason and
  must be documented in `docs/compatibility.md`.
* **Never break a running site.** Untrusted input (Livestatus queries, external commands,
  check result files) must not be able to crash or block the core.
* **Tests pin behaviour.** Every bug fix comes with a test that fails without it.

## Workflow

1. Open an issue for larger changes so the design can be discussed first.
2. Fork, create a topic branch, keep commits focused.
3. Make sure these pass locally:
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
4. Open a pull request and fill in the template.

## Commit messages

Use the imperative mood ("Add servicesbyhostgroup table"), reference issues (`Fixes #12`).

## Developer Certificate of Origin

By contributing you certify that you have the right to submit your contribution under the
project's license (GPL-2.0-or-later), as described by the
[Developer Certificate of Origin](https://developercertificate.org/). Please sign off your
commits (`git commit -s`).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
