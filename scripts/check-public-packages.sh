#!/usr/bin/env bash
set -euo pipefail

cargo_bin="${LENSO_CARGO_BIN:-cargo}"
repository_root="$(git rev-parse --show-toplevel)"
metadata="$($cargo_bin metadata --locked --no-deps --format-version=1)"
target_directory="$(jq -r '.target_directory' <<<"$metadata")"
verification_root="$(mktemp -d "${TMPDIR:-/tmp}/lenso-usage-meter-package.XXXXXX")"
trap 'rm -rf "$verification_root"' EXIT

capability=lenso-capability-usage-meter
plugin=lenso-usage-meter-postgres-plugin
capability_manifest="$(jq -r --arg package "$capability" '.packages[] | select(.name == $package) | .manifest_path' <<<"$metadata")"
plugin_manifest="$(jq -r --arg package "$plugin" '.packages[] | select(.name == $package) | .manifest_path' <<<"$metadata")"

if [[ ! -f "$capability_manifest" || ! -f "$plugin_manifest" ]]; then
  echo "Usage Meter package manifests are missing" >&2
  exit 1
fi

capability_public="$(jq -r --arg package "$capability" '.packages[] | select(.name == $package) | .publish == null or (.publish | length > 0)' <<<"$metadata")"
plugin_public="$(jq -r --arg package "$plugin" '.packages[] | select(.name == $package) | .publish == null or (.publish | length > 0)' <<<"$metadata")"
if [[ "$capability_public" != "true" ]]; then
  echo "$capability is not public in Cargo metadata" >&2
  exit 1
fi
if [[ "$plugin_public" != "false" ]]; then
  echo "$plugin must remain private in Cargo metadata" >&2
  exit 1
fi

required_source_set=(
  build.rs
  capability.json
  schemas/correct-usage-error.schema.json
  schemas/correct-usage-request.schema.json
  schemas/correct-usage-response.schema.json
  schemas/read-usage-window-error.schema.json
  schemas/read-usage-window-request.schema.json
  schemas/read-usage-window-response.schema.json
  schemas/record-usage-error.schema.json
  schemas/record-usage-request.schema.json
  schemas/record-usage-response.schema.json
  src/contract.rs
  src/generated.rs
  src/lib.rs
)

for source in "${required_source_set[@]}"; do
  if [[ ! -f "$(dirname "$capability_manifest")/$source" ]]; then
    printf 'required public Capability source is missing: %s\n' "$source" >&2
    exit 1
  fi
done

for packaged_asset in '"build.rs"' '"capability.json"' '"schemas/*.json"' '"src/*.rs"'; do
  rg --fixed-strings --quiet "$packaged_asset" "$capability_manifest" || {
    printf 'Capability include set is missing %s\n' "$packaged_asset" >&2
    exit 1
  }
done

package_flags=(--locked)
if [[ "${LENSO_PACKAGE_ALLOW_DIRTY:-0}" == "1" ]]; then
  package_flags+=(--allow-dirty)
fi

"$cargo_bin" package --quiet "${package_flags[@]}" -p "$capability"

version="$(jq -r --arg package "$capability" '.packages[] | select(.name == $package) | .version' <<<"$metadata")"
archive="$target_directory/package/$capability-$version.crate"
if [[ ! -s "$archive" ]]; then
  printf 'public package archive is missing: %s\n' "$archive" >&2
  exit 1
fi

tar -xzf "$archive" -C "$verification_root"
extracted="$verification_root/$capability-$version"
if [[ ! -f "$extracted/Cargo.toml" || ! -f "$extracted/Cargo.toml.orig" ]]; then
  echo "normalized manifest pair is missing from the public Capability" >&2
  exit 1
fi
if awk '
  /^\[(build-|dev-)?dependencies(\.|\])/ { in_dependencies = 1; next }
  /^\[/ { in_dependencies = 0 }
  in_dependencies && /(^|[[:space:]])(git|path)[[:space:]]*=/ { print; found = 1 }
  END { exit(found ? 0 : 1) }
' "$extracted/Cargo.toml"; then
  echo "the public Capability retained a non-registry dependency" >&2
  exit 1
fi
if rg -n '/Users/[^/[:space:]]+|[A-Za-z]:\\Users\\' "$extracted"; then
  echo "local absolute path leaked into the public Capability" >&2
  exit 1
fi

for source in "${required_source_set[@]}"; do
  if [[ ! -f "$extracted/$source" ]]; then
    printf 'Capability archive source is missing: %s\n' "$source" >&2
    exit 1
  fi
done

expected_archive_source_set="$(printf '%s\n' "${required_source_set[@]}" | sort)"
actual_archive_source_set="$(
  cd "$extracted"
  find build.rs capability.json schemas src -type f -print | sort
)"
if [[ "$actual_archive_source_set" != "$expected_archive_source_set" ]]; then
  echo "the Capability archive source set changed without an explicit gate update" >&2
  diff -u \
    <(printf '%s\n' "$expected_archive_source_set") \
    <(printf '%s\n' "$actual_archive_source_set") || true
  exit 1
fi

printf 'public Usage Meter Capability archive and source set are valid\n'
