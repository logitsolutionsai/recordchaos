//! A small FHIR R4 importer with four seeded bugs, used by the demo and by CI.
//!
//! It keeps its state in a JSON file and speaks the command-target contract:
//!
//! ```text
//! buggy_importer [--fixed] [--state PATH] reset
//! buggy_importer [--fixed] [--state PATH] ingest   < bundle.json
//! buggy_importer [--fixed] [--state PATH] observe  > view.json
//! ```
//!
//! Exit 0 accepted, exit 2 rejected with a message on stderr.
//!
//! The seeded bugs (all four are off with `--fixed`):
//! 1. Keeps the first Observation seen for a code and effective time, so a
//!    correction or a final version is ignored (operators 1 and 2).
//! 2. Crashes on `urn:uuid` references (operator 15).
//! 3. Treats entry order as time order: the last Observation in the bundle
//!    is "latest" (operator 19).
//! 4. Accepts dangling references silently (operator 17).
//!
//! Synthetic test data only. This is example code, not a real importer.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use recordchaos_core::refs::{RefGraph, Resolution};
use recordchaos_core::time;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    patients: Vec<String>,
    observations: Vec<Obs>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Obs {
    id: String,
    code: String,
    effective: String,
    effective_epoch: i64,
    issued_epoch: i64,
    status: String,
    value: Option<Value>,
}

struct Args {
    fixed: bool,
    state: PathBuf,
    command: String,
}

fn parse_args() -> Result<Args, String> {
    let mut fixed = false;
    let mut state = PathBuf::from("target/buggy-importer-state.json");
    let mut command = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fixed" => fixed = true,
            "--state" => state = PathBuf::from(it.next().ok_or("--state needs a path")?),
            "reset" | "ingest" | "observe" => command = Some(a),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        fixed,
        state,
        command: command
            .ok_or("usage: buggy_importer [--fixed] [--state PATH] reset|ingest|observe")?,
    })
}

fn load(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn first_code(res: &Value) -> Option<String> {
    res.get("code")?
        .get("coding")?
        .as_array()?
        .iter()
        .find_map(|c| c.get("code").and_then(Value::as_str))
        .map(str::to_string)
}

fn effective(res: &Value) -> Option<&str> {
    res.get("effectiveDateTime")
        .or_else(|| res.get("effectiveInstant"))
        .or_else(|| res.get("effectivePeriod").and_then(|p| p.get("start")))
        .and_then(Value::as_str)
}

fn epoch(text: Option<&str>) -> i64 {
    text.and_then(time::parse)
        .map(|t| t.epoch)
        .unwrap_or(i64::MIN)
}

fn status_rank(s: &str) -> u8 {
    match s {
        "corrected" | "amended" => 3,
        "final" => 2,
        "preliminary" => 1,
        _ => 0,
    }
}

/// Ok(()) accepted; Err(message) rejected.
fn ingest(fixed: bool, store: &mut Store, bundle: &Value) -> Result<(), String> {
    if bundle.get("resourceType").and_then(Value::as_str) != Some("Bundle") {
        return Err(String::from("not a Bundle"));
    }
    let entries: Vec<&Value> = bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let graph = RefGraph::build(bundle);
    if !graph.duplicate_full_urls().is_empty() {
        return Err(String::from("two entries share one fullUrl"));
    }
    for e in &entries {
        let Some(res) = e.get("resource") else {
            continue;
        };
        match res.get("resourceType").and_then(Value::as_str) {
            Some("Observation") => {
                if res.get("code").is_none() {
                    return Err(String::from("Observation without code"));
                }
                if res.get("status").is_none() {
                    return Err(String::from("Observation without status"));
                }
            }
            Some("Encounter") => {
                let start = epoch(res.pointer("/period/start").and_then(Value::as_str));
                let end = res.pointer("/period/end").and_then(Value::as_str);
                if end.is_some() && epoch(end) < start {
                    return Err(String::from("Encounter ends before it starts"));
                }
            }
            _ => {}
        }
    }
    for (site, resolution) in graph.sites.iter().zip(graph.resolved.iter()) {
        if !fixed && site.reference.starts_with("urn:uuid:") {
            // Bug 2: the lookup table only knows Type/id and URLs.
            panic!("unsupported reference form");
        }
        if fixed && *resolution == Resolution::Unresolved {
            // Bug 4 is the missing check: the buggy importer skips this.
            return Err(format!(
                "{} does not resolve inside the bundle",
                site.reference
            ));
        }
    }
    if !graph.cross_patient().is_empty() {
        return Err(String::from("a resource links records of two patients"));
    }

    for e in &entries {
        let Some(res) = e.get("resource") else {
            continue;
        };
        let id = res
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match res.get("resourceType").and_then(Value::as_str) {
            Some("Patient") => {
                if !store.patients.contains(&id) {
                    store.patients.push(id);
                }
            }
            Some("Observation") => {
                let (Some(code), Some(eff)) = (first_code(res), effective(res)) else {
                    continue;
                };
                let value = res
                    .pointer("/valueQuantity/value")
                    .cloned()
                    .or_else(|| res.get("valueString").cloned());
                let obs = Obs {
                    id,
                    code,
                    effective: eff.to_string(),
                    effective_epoch: epoch(Some(eff)),
                    issued_epoch: epoch(res.get("issued").and_then(Value::as_str)),
                    status: res
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    value,
                };
                if fixed {
                    if let Some(pos) = store.observations.iter().position(|o| o.id == obs.id) {
                        store.observations[pos] = obs;
                        continue;
                    }
                    if let Some(pos) = store.observations.iter().position(|o| {
                        o.code == obs.code && o.effective_epoch == obs.effective_epoch
                    }) {
                        let old = &store.observations[pos];
                        if (obs.issued_epoch, status_rank(&obs.status))
                            >= (old.issued_epoch, status_rank(&old.status))
                        {
                            store.observations[pos] = obs;
                        }
                        continue;
                    }
                    store.observations.push(obs);
                } else {
                    // Bug 1: the first Observation seen for a code and
                    // effective time wins, so corrections are ignored.
                    let seen = store.observations.iter().any(|o| {
                        o.id == obs.id || (o.code == obs.code && o.effective == obs.effective)
                    });
                    if !seen {
                        store.observations.push(obs);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn view(fixed: bool, store: &Store) -> Value {
    let mut latest: Map<String, Value> = Map::new();
    let mut best: Vec<(String, (i64, i64))> = Vec::new();
    for o in &store.observations {
        let entry = json!({"id": o.id, "value": o.value.clone().unwrap_or(Value::Null)});
        if fixed {
            let rank = (o.effective_epoch, o.issued_epoch);
            match best.iter_mut().find(|(c, _)| *c == o.code) {
                Some((_, r)) if rank <= *r => {}
                Some((_, r)) => {
                    *r = rank;
                    latest.insert(o.code.clone(), entry);
                }
                None => {
                    best.push((o.code.clone(), rank));
                    latest.insert(o.code.clone(), entry);
                }
            }
        } else {
            // Bug 3: the last Observation in entry order is "latest".
            latest.insert(o.code.clone(), entry);
        }
    }
    json!({
        "latest": Value::Object(latest),
        "patients": {"count": store.patients.len(), "ids": store.patients},
        "observations": {"count": store.observations.len()}
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(64);
        }
    };
    match args.command.as_str() {
        "reset" => match save(&args.state, &Store::default()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("cannot reset: {e}");
                ExitCode::from(1)
            }
        },
        "ingest" => {
            let mut text = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut text) {
                eprintln!("cannot read stdin: {e}");
                return ExitCode::from(1);
            }
            let bundle: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("rejected: not JSON: {e}");
                    return ExitCode::from(2);
                }
            };
            let mut store = load(&args.state);
            match ingest(args.fixed, &mut store, &bundle) {
                Ok(()) => match save(&args.state, &store) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("cannot save: {e}");
                        ExitCode::from(1)
                    }
                },
                Err(msg) => {
                    eprintln!("rejected: {msg}");
                    ExitCode::from(2)
                }
            }
        }
        _ => {
            let store = load(&args.state);
            println!("{}", view(args.fixed, &store));
            ExitCode::SUCCESS
        }
    }
}
