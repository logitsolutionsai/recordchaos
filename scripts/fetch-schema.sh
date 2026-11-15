#!/bin/sh
# Fetch the official FHIR R4 JSON schema into crates/recordchaos-core/schemas/fhir-r4/ (unmodified).
# The repository already contains it; use this to re-check or restore it.
set -eu
here="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
curl -fsSL -o "$tmp/fhir.schema.json.zip" https://hl7.org/fhir/R4/fhir.schema.json.zip
unzip -o -q "$tmp/fhir.schema.json.zip" fhir.schema.json -d "$tmp"
expected="2230406893b4cf002a4ee1e5e2bbeca22ac5d2d4931b3e9ef7b9594bbc376a01"
actual="$(sha256sum "$tmp/fhir.schema.json" | cut -d' ' -f1)"
if [ "$actual" != "$expected" ]; then
  echo "fhir.schema.json SHA-256 is $actual, expected $expected" >&2
  exit 1
fi
cp "$tmp/fhir.schema.json" "$here/crates/recordchaos-core/schemas/fhir-r4/fhir.schema.json"
echo "crates/recordchaos-core/schemas/fhir-r4/fhir.schema.json is in place"
