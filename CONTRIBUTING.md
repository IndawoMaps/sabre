# Contributing to sabre

Thanks for taking an interest. sabre is early — pre-1.0 — so the most valuable
contributions right now are bug reports against real rasters and fixes to the
[known limitations](README.md#known-limitations).

## Getting set up

```bash
git clone https://github.com/IndawoMaps/sabre.git
cd sabre
cargo test --workspace
```

[mise](https://mise.jdx.dev) provisions the full toolchain (Rust, pnpm, uv, just) if you
want the demo map and Python benchmark comparison as well:

```bash
mise install
mise run dev
```

## Before you open a pull request

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

CI runs exactly these on Linux and macOS, plus an advisory `cargo clippy`.

### The PR title is the changelog entry

PRs are squash-merged, and the PR title becomes the commit on `main`. Releases and
`CHANGELOG.md` are generated from those commits, so the title has to be a
[conventional commit](https://www.conventionalcommits.org):

| Title | Changelog | Release |
| --- | --- | --- |
| `feat: Render hillshade from a DEM` | Features | minor |
| `fix: Fetch a page once when several readers need it` | Bug Fixes | patch |
| `perf: Decode LZW strips without a copy` | Performance | patch |
| `feat!: Rename the style parameters` | Features, marked breaking | major (minor while 0.x) |
| `docs:` `refactor:` `test:` `build:` `ci:` `chore:` | not listed | none |

A scope is optional: `fix(browser): ...`. Write the subject the way you would write any
commit subject here. The commits inside the PR are yours; only the title reaches `main`.
The `pr title` check fails until the title parses.

## House rules

**Do not run `cargo fmt`.** The codebase uses deliberate column alignment in match arms,
struct literals and parameter lists, which rustfmt destroys. There is no `rustfmt.toml`
and no fmt check in CI on purpose. Match the style of the file you are editing.

**Clippy is advisory, not gating.** There is a small backlog of lint warnings. Please
don't fix unrelated lints in a feature PR — it makes the diff hard to review. A dedicated
lint-cleanup PR is welcome.

**Keep `server` runtime-agnostic above the shim line.** Endpoint logic lives in
`core` also compiles to `wasm32-unknown-unknown` for the browser target, and that
constraint is enforced rather than hoped for: `crates/core/tests/wasm_safe.rs` fails if anything
outside `timing.rs` reads a clock, because `Instant::now()` compiles there and panics at
runtime. Changes to the render or query paths should run `just browser-test`, which
executes in a real browser — `cargo check --target wasm32-unknown-unknown` does not catch
this class of bug.

`crates/server/src/api.rs` and takes a `RangeReader`; only `native.rs` may
know which runtime they are on. A new endpoint goes in `api.rs` and the shared router, not
in one shim.

**Keep `core` free of I/O.** `sabre-core` must not know about HTTP, files, or any runtime.
Everything I/O-shaped enters through the `RangeReader` trait. This is what lets the crate
compile to WASM and run identically on a server, at the edge, and in a browser. A PR that
adds `reqwest` to `core` will be turned down, however convenient.

**Rendering changes need snapshot tests.** If you change how pixels are produced, run:

```bash
cargo test -p sabre-core -- --ignored generate_fixtures
```

and commit the regenerated fixtures *in the same commit*, so reviewers can see the visual
diff alongside the code.

## Releases

Nobody tags a release by hand. [release-please](https://github.com/googleapis/release-please)
keeps a PR open titled `chore(main): release x.y.z`, updated on every merge to `main`,
with the next version and its changelog. Merging it is the release: the `release`
workflow tags it, writes the GitHub release, attaches the Linux binaries, stages
`@sabremaps/browser`, `@sabremaps/ol` and `@sabremaps/react-native` on npm for approval
(below), and tags the images `x.y.z` and `latest`.

Everything shares one version: the workspace `Cargo.toml`, the workspace crates in
`Cargo.lock`, every package's `package.json`, and the React Native package's
`build.gradle`, as listed in `release-please-config.json`. A new npm package needs adding
there.

npm publishes through trusted publishing: each package names this repo, `release.yml` and
the `Publish` environment as its trusted publisher on npmjs.com, so no token is stored.
The publisher is limited to staged publishing, so the release only *stages* each package.
Nothing is public until a maintainer approves it with 2FA: `npm stage list`, then
`npm stage approve <id>` for each, or approve them on npmjs.com. Approve
`@sabremaps/browser` before `@sabremaps/ol`, which depends on it.

If a release's npm job fails, stage that release again with
`gh workflow run release.yml -f tag=vX.Y.Z`. It builds the tagged code with the workflow as
it is now; re-running the failed job would repeat the workflow as it was at the release. That setting only exists once
the package does, so **a new package's first version is published by hand**. Download the
tarball CI built for it (the `react-native` job uploads `sabremaps-react-native`, with the
iOS library the `react-native-ios` job built on macOS), run
`npm publish ./<tarball> --access public` -- the `./` matters, since npm reads a bare
`dir/file.tgz` as a GitHub repo -- then add the trusted publisher before the next release.

Dependabot opens grouped minor/patch updates weekly. They are merged automatically once CI
passes; majors wait for a human, who can opt one in with the `automerge` label.

**The release bot.** With only the default `GITHUB_TOKEN`, GitHub doesn't run workflows
for pushes and PRs that token makes: CI doesn't run on the release PR, and a Dependabot
merge doesn't update it. A GitHub App fixes both. Create one owned by the org with
*Contents*, *Pull requests* and *Issues* read & write, install it on this repo, then set
the repo variable `RELEASE_BOT_CLIENT_ID` and the secret `RELEASE_BOT_PRIVATE_KEY`. The
workflows use it when it's there and fall back to the default token when it isn't.

## Reporting a rendering bug

Raster bugs are hard to reproduce from prose. The most useful report includes:

- A link to a COG that reproduces it, or the output of `GET /info?url=...`
- The full tile URL you requested, including all style parameters
- What you got and what you expected

`GET /tile-info/{z}/{x}/{y}?url=...` tells you which overview level and pixel window sabre
chose, which usually narrows down coverage and pyramid bugs immediately.

## Licensing of contributions

sabre is licensed under the [Apache License 2.0](LICENSE.md). By submitting a pull request you
agree that your contribution is licensed under the same terms.
