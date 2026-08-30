#!/usr/bin/env bash
set -euo pipefail

expected_crates=$'lenso-capability-usage-meter\nlenso-usage-meter-postgres-plugin'
actual_crates="$(find crates -mindepth 2 -maxdepth 2 -name Cargo.toml -print0 | xargs -0 sed -n 's/^name = "\([^"]*\)"/\1/p' | sort)"

if [[ "$actual_crates" != "$expected_crates" ]]; then
  echo "unexpected workspace crate boundary" >&2
  diff -u <(printf '%s\n' "$expected_crates") <(printf '%s\n' "$actual_crates") || true
  exit 1
fi

if rg -n 'path\s*=\s*"(\.\./\.\./|/)' --glob 'Cargo.toml' .; then
  echo "cross-repository or absolute path dependencies are not allowed" >&2
  exit 1
fi

if rg -n 'lenso-platform-|lenso-module-|HostBuilder|HostLinkedModule|ModuleManifest' \
  Cargo.toml crates README.md docs --glob '!**/generated.rs'; then
  echo "legacy Lenso framework dependency or API found" >&2
  exit 1
fi

if rg -n '/Users/[^/[:space:]]+|[A-Za-z]:\\Users\\' README.md docs; then
  echo "public documentation contains a local absolute path" >&2
  exit 1
fi

if rg -n 'CARGO_REGISTRY_TOKEN|CRATES_IO_TOKEN' .github; then
  echo "registry-token publication fallback is not allowed" >&2
  exit 1
fi

capability_manifest=crates/lenso-capability-usage-meter/Cargo.toml
plugin_manifest=crates/lenso-usage-meter-postgres-plugin/Cargo.toml
rg -qx 'publish = true' "$capability_manifest" || {
  echo "the Usage Meter Capability must be explicitly public" >&2
  exit 1
}
rg -qx 'publish = false' "$plugin_manifest" || {
  echo "the PostgreSQL Plugin must remain explicitly private" >&2
  exit 1
}

release_workflow=.github/workflows/release-plz.yml
for release_contract in \
  'id-token: write' \
  "inputs.confirm == 'publish'" \
  "github.ref == 'refs/heads/main'"; do
  if ! rg --fixed-strings --quiet "$release_contract" "$release_workflow"; then
    echo "release workflow is missing required contract: $release_contract" >&2
    exit 1
  fi
done

for capability in 'lenso.usage-meter@1' 'lenso.secrets@1'; do
  if ! rg -q "$capability" README.md docs crates; then
    echo "documented Capability boundary is missing: $capability" >&2
    exit 1
  fi
done

printf 'Usage Meter repository and release boundaries are valid\n'
