# Release process

The only public crate in this repository is
`lenso-capability-usage-meter`. The PostgreSQL Plugin remains private and must
not be published.

Publication is manual-only from reviewed `main` through
`.github/workflows/release-plz.yml`. Pushes may refresh a Release-plz pull
request but cannot publish. A live run additionally requires `live=true`, the
literal confirmation `publish`, and `main`.

## Trusted Publisher

Configure the crates.io Trusted Publisher for the public crate with:

- owner: `LioRael`
- repository: `lenso-usage-meter-plugin`
- workflow: `release-plz.yml`
- environment: unset

Only the confirmed live job receives `id-token: write`. There is no registry
token fallback. Initial allocation of an unpublished crate name is a separate,
explicit bootstrap step; never add a registry token or fallback secret to this
repository.

## Required evidence

```sh
cargo fmt --all -- --check
cargo check --locked --workspace --all-targets
cargo test --locked --workspace -- --include-ignored --test-threads=1
cargo clippy --locked --workspace --all-targets -- -D warnings
lenso-contract-codegen workspace check --manifest-path Cargo.toml
./scripts/check-repository-boundary.sh
./scripts/check-public-packages.sh
```

The ignored PostgreSQL acceptance test requires a dedicated database through
`LENSO_POSTGRES_TEST_URL`. Generated Capability projections are locked artifacts
and must not be edited by hand.
