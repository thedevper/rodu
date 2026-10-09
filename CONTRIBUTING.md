# Contributing to Rodu

Thanks for helping. Rodu is licensed under the [Apache License 2.0](LICENSE), and contributions
come in under the same license (section 5 of the license).

## Sign off every commit (DCO)

Each commit must certify the [Developer Certificate of Origin 1.1](https://developercertificate.org/):
that you wrote the change, or otherwise have the right to submit it under the project's license.
You certify it by adding a `Signed-off-by` line that matches the commit's author:

```sh
git commit -s -m "fix: ..."
# adds: Signed-off-by: Your Name <you@example.com>
```

CI rejects a pull request with an unsigned commit. To sign off commits you already made:

```sh
git rebase --signoff origin/main
git push --force-with-lease
```

**Code written for an employer may belong to the employer.** Sign off only work you have the
right to contribute, for example work done on your own time and equipment, or with your
employer's written permission.

## Keep real data out

The repository is public. Tests, fixtures, examples and docs use invented data only (`DEMO`,
`ACME`, `alice`). Never paste task titles, names, IDs, tokens or exports from a real workspace,
including anything from your employer or a customer.

## Dependencies and licences

Every crate that ends up in a release must be under a licence in [`about.toml`](about.toml).
`cargo xtask notices` (needs `cargo install cargo-about --locked --features cli`) writes
`dist/THIRD-PARTY-NOTICES.txt` and fails on anything else, so a new
dependency with an unlisted licence needs a deliberate decision, not a quiet addition.

## Before opening a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p rodu-web --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace
cargo xtask notices  # when dependencies change
cargo xtask web      # when the web board changes
```
