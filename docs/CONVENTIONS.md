# Project Conventions

Hard-won rules from development experience. These supplement CLAUDE.md
and apply to all contributors (human and AI).

## Code changes

- **Fix at the source, not with workarounds.** If a config file is wrong,
  fix the config file. Do not add env var overrides, shims, or wrapper
  hacks. Fix the actual problem in the proper layer.

- **Fix obvious bugs immediately.** If a fix is simple and clear (e.g.
  one-line change, wrong variable name, wrong port), make the fix. Do
  not file an issue and defer it.

- **No speculative fixes.** When fixing a regression, make only the
  minimal targeted fix. Do not add speculative improvements on top.

- **Delete unused code.** Dead scripts, config entries, and wrapper
  files are distractions, not references. If it's not used, delete it.
  History is in git.

- **No Unicode in scripts.** Never use Unicode punctuation (curly
  quotes, em dashes, etc.) in scripts or code files. ASCII only.

## Git workflow

- **Merge commits only.** Always use merge commits for PRs. Never
  squash. Squashing destroys per-commit history and makes bisect harder.

- **Release from main only.** Tags and releases must always be created
  from main. Never tag from a feature branch. Merge to main first,
  then tag.

- **Merge completed work immediately.** Do not leave finished, verified
  PRs sitting open. Merge them before moving on to new work.

## Builds and releases

- **"Re-install" means local rebuild.** When asked to re-install,
  rebuild locally and `brew install` from the local tarball. Do NOT
  tag or create a GitHub release.

- **Fresh disk for releases.** `build-release.sh` must use `truncate`
  for root.img. Never copy the live disk — it contains 2+ GB of local
  state.

- **Linux binaries built in Linux.** All Linux binaries (pelagos,
  pelagos-guest, pelagos-dns) are built natively in the build VM.
  No cross-compilation from macOS. See `scripts/full-rebuild.sh`.

## Scripts and automation

- **Do not run scripts unbidden.** Do not run a script immediately
  after creating it unless explicitly asked.

- **Multi-step sudo procedures must be scripts.** Never present a list
  of individual sudo commands to run manually. Write a runnable script.

- **Warn before external actions.** Warn explicitly before presenting
  or running scripts that contact external services (Apple notarytool,
  GitHub API, package registries, etc.).

## Homebrew tap tokens

Classic PATs with `public_repo` scope only. Fine-grained tokens cannot
write to organization repos (known GitHub bug as of May 2026). See
`docs/HOMEBREW_TAP_TOKEN.md` for full setup and rotation procedure.
