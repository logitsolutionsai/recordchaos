# Security

## Reporting a Problem

Please report security problems privately through GitHub's "Report a vulnerability" button on this repository, not in a public issue. We will reply as soon as we can.

## What recordchaos Does to Stay Safe

- Synthetic data only. `recordchaos run` warns when a base bundle holds values that look like real identifiers and asks for `--i-confirm-synthetic`. The heuristic does not prove that a bundle is de-identified.
- No network except the declared target. The one exception is `--strict`: the HL7 validator downloads FHIR definition packages into `~/.fhir/packages` on its first run. It sends no bundle data. HTTP targets must be on a loopback address, or listed in `allow_hosts` with `--allow-remote` passed. No telemetry.
- Commands run without a shell, as argument arrays.
- Limits: 10 MB per bundle, 5,000 resources, 1,000 cases per run, 25 mutations per case and 1 MB per observed view. JSON parsing is bounded in size and depth.
- No code execution from scenario or target files: user checks are a fixed set of comparisons.
