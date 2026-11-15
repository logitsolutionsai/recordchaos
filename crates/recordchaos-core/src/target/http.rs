//! HTTP targets: a local FHIR server. Requests are blocking; recordchaos runs
//! one case at a time, so no async runtime is needed.

use std::collections::BTreeMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{AcceptRules, StepKind, StepOutcome, Target, View, ViewEntry, first_line};
use crate::error::{Error, Result};

/// One request in the target file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequestSpec {
    pub method: String,
    pub path: String,
    pub content_type: String,
    /// Body text for reset requests (read from the file named in the target).
    pub body: Option<String>,
    /// Turn a collection bundle into a transaction before posting it.
    pub as_transaction: bool,
}

/// An HTTP target's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub base_url: String,
    pub ingest: HttpRequestSpec,
    pub reset: Option<HttpRequestSpec>,
    pub view: BTreeMap<String, ViewEntry>,
    pub timeout_ms: u64,
    pub accept: AcceptRules,
    pub max_view_bytes: usize,
}

/// Scheme, host and port of a URL. Small parser so we do not need a URL crate.
pub fn parse_url(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => default_port,
        };
        return Some((scheme, host.to_string(), port));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// True when the host is, or resolves only to, loopback addresses.
pub fn is_loopback_host(host: &str, port: u16) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip.is_loopback();
    }
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback())
        }
        Err(_) => false,
    }
}

/// Refuse non-loopback hosts unless `--allow-remote` is given and the host is
/// listed in `allow_hosts`.
pub fn check_host(base_url: &str, allow_hosts: &[String], allow_remote: bool) -> Result<()> {
    let (_, host, port) = parse_url(base_url).ok_or_else(|| {
        Error::input(format!("base_url {base_url:?} is not an http or https URL"))
    })?;
    if is_loopback_host(&host, port) {
        return Ok(());
    }
    let listed = allow_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host));
    if allow_remote && listed {
        return Ok(());
    }
    if listed {
        Err(Error::input(format!(
            "{host} is not a loopback address. It is in allow_hosts, so pass --allow-remote to use it."
        )))
    } else {
        Err(Error::input(format!(
            "{host} is not a loopback address. recordchaos only sends synthetic data to localhost unless the host is in allow_hosts and --allow-remote is passed."
        )))
    }
}

/// Turn a bundle into a transaction: type transaction and a PUT request per
/// entry with a resource id.
pub fn to_transaction(bundle: &Value) -> Value {
    let mut out = bundle.clone();
    if let Some(map) = out.as_object_mut() {
        map.insert(String::from("type"), json!("transaction"));
    }
    if let Some(Value::Array(entries)) = out.get_mut("entry") {
        for e in entries.iter_mut() {
            let rt = e
                .pointer("/resource/resourceType")
                .and_then(Value::as_str)
                .map(str::to_string);
            let id = e
                .pointer("/resource/id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let request = match (rt, id) {
                (Some(rt), Some(id)) => json!({"method": "PUT", "url": format!("{rt}/{id}")}),
                (Some(rt), None) => json!({"method": "POST", "url": rt}),
                _ => continue,
            };
            if let Some(m) = e.as_object_mut() {
                m.insert(String::from("request"), request);
            }
        }
    }
    out
}

/// A target that talks to a FHIR server.
pub struct HttpTarget {
    cfg: HttpConfig,
    agent: ureq::Agent,
}

impl HttpTarget {
    pub fn new(cfg: HttpConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(cfg.timeout_ms.max(1))))
            .http_status_as_error(false)
            .build()
            .into();
        Self { cfg, agent }
    }

    fn url(&self, path: &str) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        if path.is_empty() || path == "/" {
            base.to_string()
        } else if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }

    /// Send one request. Returns the outcome and the body text.
    fn send(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: Option<String>,
    ) -> (StepOutcome, String) {
        let url = self.url(path);
        let start = Instant::now();
        let result = match (method.to_ascii_uppercase().as_str(), body) {
            ("POST", Some(b)) => self
                .agent
                .post(url.as_str())
                .header("Content-Type", content_type)
                .send(b),
            ("POST", None) => self.agent.post(url.as_str()).send_empty(),
            ("PUT", Some(b)) => self
                .agent
                .put(url.as_str())
                .header("Content-Type", content_type)
                .send(b),
            ("PUT", None) => self.agent.put(url.as_str()).send_empty(),
            ("DELETE", _) => self.agent.delete(url.as_str()).call(),
            ("GET", _) => self
                .agent
                .get(url.as_str())
                .header("Accept", "application/fhir+json")
                .call(),
            (other, _) => {
                return (
                    StepOutcome::crashed(format!("unsupported HTTP method {other}")),
                    String::new(),
                );
            }
        };
        let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(mut res) => {
                let status = res.status().as_u16();
                let text = res
                    .body_mut()
                    .with_config()
                    .limit(self.cfg.max_view_bytes as u64)
                    .read_to_string()
                    .unwrap_or_default();
                let kind = match status {
                    200..=299 => StepKind::Accepted,
                    400..=499 => StepKind::Rejected,
                    _ => StepKind::Crashed,
                };
                let operation_outcome = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| {
                        v.get("resourceType")
                            .and_then(Value::as_str)
                            .map(|t| t == "OperationOutcome")
                    })
                    .unwrap_or(false);
                let line = if kind == StepKind::Accepted {
                    String::new()
                } else {
                    first_line(text.as_bytes())
                };
                (
                    StepOutcome {
                        kind,
                        code: Some(i64::from(status)),
                        http: true,
                        first_line: line,
                        operation_outcome,
                        elapsed_ms,
                    },
                    text,
                )
            }
            Err(ureq::Error::Timeout(_)) => (
                StepOutcome {
                    kind: StepKind::Timeout,
                    code: None,
                    http: true,
                    first_line: format!("no response within {} ms", self.cfg.timeout_ms),
                    operation_outcome: false,
                    elapsed_ms,
                },
                String::new(),
            ),
            Err(e) => (
                StepOutcome {
                    kind: StepKind::Crashed,
                    code: None,
                    http: true,
                    first_line: e.to_string(),
                    operation_outcome: false,
                    elapsed_ms,
                },
                String::new(),
            ),
        }
    }
}

impl Target for HttpTarget {
    fn describe(&self) -> String {
        self.cfg.base_url.clone()
    }

    fn view_spec(&self) -> &BTreeMap<String, ViewEntry> {
        &self.cfg.view
    }

    fn accept(&self) -> &AcceptRules {
        &self.cfg.accept
    }

    fn reset(&mut self) -> Result<StepOutcome> {
        let Some(spec) = self.cfg.reset.clone() else {
            return Ok(StepOutcome::accepted());
        };
        let (step, _) = self.send(
            &spec.method,
            &spec.path,
            &spec.content_type,
            spec.body.clone(),
        );
        Ok(step)
    }

    fn ingest(&mut self, bundle: &Value) -> Result<StepOutcome> {
        let spec = self.cfg.ingest.clone();
        let body = if spec.as_transaction {
            to_transaction(bundle)
        } else {
            bundle.clone()
        };
        let text = crate::json::canonical(&body)?;
        let (step, _) = self.send(&spec.method, &spec.path, &spec.content_type, Some(text));
        Ok(step)
    }

    fn observe(&mut self) -> Result<(StepOutcome, View)> {
        let mut view = View::new();
        let mut last = StepOutcome::accepted();
        last.http = true;
        let entries: Vec<(String, ViewEntry)> = self
            .cfg
            .view
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, entry) in entries {
            let Some(get) = entry.get.as_deref() else {
                continue;
            };
            let (step, text) = self.send("GET", get, "application/fhir+json", None);
            if step.kind != StepKind::Accepted {
                return Ok((step, View::new()));
            }
            let doc: Value = match serde_json::from_str(&text) {
                Ok(d) => d,
                Err(e) => {
                    let mut bad = step.clone();
                    bad.kind = StepKind::Crashed;
                    bad.first_line = format!("GET {get} returned invalid JSON: {e}");
                    return Ok((bad, View::new()));
                }
            };
            if let Some(v) = doc.pointer(&entry.pointer) {
                view.insert(name, v.clone());
            }
            last = step;
        }
        Ok((last, view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080/fhir"),
            Some(("http".into(), "127.0.0.1".into(), 8080))
        );
        assert_eq!(
            parse_url("https://Example.org/fhir"),
            Some(("https".into(), "example.org".into(), 443))
        );
        assert_eq!(
            parse_url("http://[::1]:9000/"),
            Some(("http".into(), "::1".into(), 9000))
        );
        assert_eq!(parse_url("ftp://x"), None);
    }

    #[test]
    fn only_loopback_by_default() {
        assert!(check_host("http://127.0.0.1:8080/fhir", &[], false).is_ok());
        assert!(check_host("http://localhost:8080/fhir", &[], false).is_ok());
        assert!(check_host("http://[::1]:8080/fhir", &[], false).is_ok());
        assert!(check_host("http://10.0.0.5/fhir", &[], false).is_err());
        let allow = vec![String::from("10.0.0.5")];
        assert!(check_host("http://10.0.0.5/fhir", &allow, false).is_err());
        assert!(check_host("http://10.0.0.5/fhir", &allow, true).is_ok());
        assert!(check_host("http://10.0.0.6/fhir", &allow, true).is_err());
    }

    #[test]
    fn transaction_adds_requests() {
        let b = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "urn:uuid:x", "resource": {"resourceType": "Patient", "id": "p1"}}
        ]});
        let t = to_transaction(&b);
        assert_eq!(t["type"], "transaction");
        assert_eq!(t["entry"][0]["request"]["url"], "Patient/p1");
    }
}
