# Targets

A target is the importer under test. recordchaos resets it, gives it each bundle of a case in turn, then reads a view from it. recordchaos never interprets clinical meaning: the view is whatever JSON the target returns, read at the pointers you declare.

## Command Targets

Any program, in any language. Commands are argument arrays, run without a shell.

```toml
kind = "command"
reset   = ["./my-importer", "reset"]     # optional
ingest  = ["./my-importer", "ingest"]    # the bundle arrives on stdin
observe = ["./my-importer", "observe"]   # print the view as JSON on stdout
timeout_ms = 5000                        # per command, default 5000
cwd = "."                                # optional, relative to this file
env = { IMPORTER_MODE = "test" }         # optional

[accept]
rejected_exit_codes = [2]                # exit 2 with a message = rejected cleanly
flag_view = "flagged"                    # optional: a view entry that, when truthy, means "accepted but flagged"

[view]                                   # name = JSON Pointer into the observed JSON
latest_glucose = "/latest/synthetic-glucose"
patient_count  = "/patients/count"
```

Program paths are relative to the folder you run recordchaos from.

- Exit 0 means accepted. An exit code in `rejected_exit_codes` means rejected cleanly. Any other exit code, a signal or a timeout is a crash.
- The first non-empty line of stderr is kept as the error line. It is part of the failure's fingerprint, so keep it stable: do not print timestamps or random ids on that line.
- `observe` must print at most 1 MB of JSON.

## HTTP Targets

A local FHIR server.

```toml
kind = "http"
base_url = "http://127.0.0.1:8080/fhir"
timeout_ms = 10000
ingest = { method = "POST", path = "/", content_type = "application/fhir+json", as_transaction = true }
reset  = { method = "POST", path = "/$expunge", body = "expunge-all.json" }   # optional

[view.latest_glucose]
get = "/Observation?patient=pat-a&code=https://example.org/recordchaos/codes|synthetic-glucose&_sort=-date&_count=1"
pointer = "/entry/0/resource"
```

- 2xx is accepted, 4xx is rejected (an OperationOutcome body is noted), 5xx, a timeout or a dropped connection is a crash.
- `as_transaction = true` turns a collection bundle into a transaction with a PUT request per entry.
- recordchaos only sends synthetic data to loopback hosts (`localhost`, `127.0.0.1`, `::1`). A non-loopback host must be listed in `allow_hosts` and you must pass `--allow-remote`.

```toml
allow_hosts = ["fhir.test.internal"]
```

## How Cases Run

Cases run on the target one at a time, because reset, ingest and observe are a stateful sequence. `--jobs` sets the threads used for validation.

1. The base bundle runs first. Its view is the baseline that most checks compare against. If the target does not accept the base bundle, the run stops with exit 3.
2. For each case: reset, ingest each step (two steps for `observation.correction_sequence`), then observe when every step was accepted.

## Checks You Can Declare

Add your own checks to the target file. There is no expression language and no code execution: only these comparisons.

```toml
[[oracle]]
id = "one_patient"
view = "patient_count"
check = "equals"            # equals, not_equals, exists, absent, count_equals, same_as_base
value = 1
applies_to = "valid"        # valid (default), invalid or all
sentence = "The importer created a second patient record."
```

## Built-In Checks

| Check | Applies to | Passes when |
|---|---|---|
| `no_crash` | every case | No 5xx, no timeout, exit code 0 or a rejected exit code |
| `invalid_rejected` | invalid cases | The target rejected it, or flagged it at a declared view entry |
| `same_as_base` | meaning-preserving operators | Every declared view entry equals the baseline view |
| `latest_correction_selected` | operators 1 and 2 | The `latest_*` entry for the corrected code holds the corrected id or value |
| `no_cross_patient_merge` | operator 18 | Entries with "patient" in their name are unchanged from the baseline |

A `latest_*` entry belongs to a code when its pointer or GET path contains the code, or when the part of its name after `latest_` is part of the code (`latest_glucose` for `synthetic-glucose`).
