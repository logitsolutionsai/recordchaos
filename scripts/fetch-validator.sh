#!/bin/sh
# Download the HL7 Java validator (about 205 MB) for the strict layer.
# recordchaos never bundles it. Needs Java 17 or later to run.
set -eu
version="${1:-7.0.6}"
curl -fL -o validator_cli.jar "https://github.com/hapifhir/org.hl7.fhir.core/releases/download/${version}/validator_cli.jar"
echo "validator_cli.jar ${version} downloaded. Its first run downloads FHIR packages (needs network). Run: recordchaos run ... --strict --validator-jar validator_cli.jar"
