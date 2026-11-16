# Contributing to Windock

Thanks for helping. Bug reports, ideas, docs fixes and code are all welcome.

## Before you start

- **Bugs:** open an issue with the bug template. Include your Windows version, what you expected,
  what happened, and the end of `%APPDATA%\Windock\windock.log`.
- **Features:** check [PLAN.md](PLAN.md) first; it lists what's planned and in what order. For
  anything bigger than a small fix, open an issue to talk it through before writing code, so we can
  agree on the approach and where it lives in the config.
- **Security issues:** don't open a public issue; see [SECURITY.md](SECURITY.md).

By contributing, you agree your work is released under the project's [MIT license](LICENSE), and
to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Setting up

Windock is a Windows app written in Rust (edition 2024, so a recent stable toolchain). You can
build it on Windows or cross-compile from WSL/Linux.

**On Windows**, with [Rust](https://rustup.rs) and the MSVC build tools (Visual Studio's
"Desktop development with C++" workload):

```powershell
cargo run --release        # build and start the dock
cargo test --release
cargo clippy --release
```

**From WSL or Linux**, with [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin):

```sh
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin --locked
scripts/deploy.sh          # build, install to %LOCALAPPDATA%\Programs\Windock, (re)start it
scripts/deploy.sh --stop   # stop the running dock
cargo xwin clippy --release --target x86_64-pc-windows-msvc --all-targets
```

`.cargo/config.toml` makes `x86_64-pc-windows-msvc` the default target, but give cargo-xwin
`--target` anyway: some commands (like clippy with `--all-targets`) fail to find a linker without it.
Don't pipe `scripts/deploy.sh` into something that exits early (like
`head`): it gets killed before it installs, and the old build keeps running. When it finishes, the log
shows `starting Windock <version>`.

Only one dock runs at a time. Quit the installed one (right-click the dock → **Quit Windock**)
before starting a build some other way.

## Tests

Run them in **release** mode. In debug builds, three of the settings window's raster tests trip a
debug assertion inside egui (a dropped `TexturesDelta`); release builds pass.

From WSL, `cargo xwin test` tries to run the tests under Wine and fails. Build them instead, then run
the `.exe` it prints on Windows, sending the output to a file (piped, it prints nothing):

```sh
cargo xwin test --release --target x86_64-pc-windows-msvc --no-run
# then run target/x86_64-pc-windows-msvc/release/deps/windock-<hash>.exe > test.log
```

CI checks formatting and runs clippy, the build and the tests on Windows for every push and pull request.

Most of Windock is Win32 glue that only a real desktop can test. For anything that changes how
the dock looks or behaves, try it live and say in the pull request what you checked (taskbar shown
and hidden, dock positions, light and dark theme, a second monitor, high DPI, whichever apply).
If a test changes a Windows setting or moves one of Windows' own windows, put it back afterwards.

## Code

- `src/app.rs` is the dock window; the README's [How it works](README.md#how-it-works) table maps
  the rest. Read the module you're changing first and follow the patterns it already uses.
- Keep it light: no polling (use WinEvent hooks, notifications and timers that stop when idle),
  no CPU use while idle, and think twice before adding a dependency.
- Write comments that say *why*, especially around Win32 quirks: which app or Windows version
  needs a workaround, and what goes wrong without it.
- Anything a user can set lives in `config.rs`. Add it to the commented default config there,
  to the settings window (`settings.rs`), and to the README.
- Run `cargo fmt` before committing (`rustfmt.toml` allows lines up to 180 characters). CI checks it.
- `cargo clippy --release` must be clean.

## Commits and pull requests

- One change per pull request; small ones get reviewed faster.
- Write commit subjects as a short imperative sentence describing what changes for the user,
  like the existing history: "Keep the hidden icons open while an icon's menu is up". Use the body
  to explain why, and anything a reviewer wouldn't guess.
- Update the README for anything users will notice, and add a line under **Unreleased** in
  [CHANGELOG.md](CHANGELOG.md).
- Screenshots or a short clip help a lot for visual changes.

## Making a release

Maintainers release by pushing a version tag; `.github/workflows/release.yml` does the rest.

1. Set the new `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` picks it up.
2. In `CHANGELOG.md`, rename **Unreleased** to `## [x.y.z] - YYYY-MM-DD` and start a new, empty
   **Unreleased** section above it. That section becomes the release notes.
3. Commit ("Release x.y.z"), push it to `main`, and wait for CI to pass.
4. Tag and push: `git tag vx.y.z && git push origin vx.y.z`.

The workflow refuses a tag that doesn't match `Cargo.toml`, or that has no changelog section. It
runs the tests, builds `windock.exe`, and publishes a release with the `.exe`, a zip (with the
license, README and changelog) and `SHA256SUMS.txt`. A version with a suffix, like `0.2.0-beta.1`,
is published as a pre-release.
