use std::collections::{BTreeMap, BTreeSet};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::routes::{RouteMapping, RouteStore};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

#[derive(Debug, Serialize)]
pub struct Finding {
    pub level: Level,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Finding {
    fn new(level: Level, message: impl Into<String>, hint: Option<&str>) -> Self {
        Self {
            level,
            message: message.into(),
            hint: hint.map(str::to_string),
        }
    }
    pub fn ok(message: impl Into<String>) -> Self {
        Self::new(Level::Ok, message, None)
    }
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(Level::Info, message, None)
    }
    pub fn warn(message: impl Into<String>, hint: &str) -> Self {
        Self::new(Level::Warn, message, Some(hint))
    }
    pub fn fail(message: impl Into<String>, hint: &str) -> Self {
        Self::new(Level::Fail, message, Some(hint))
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub warnings: usize,
    pub failures: usize,
}

impl Report {
    pub fn new(findings: Vec<Finding>) -> Self {
        let count = |l| findings.iter().filter(|f| f.level == l).count();
        let warnings = count(Level::Warn);
        let failures = count(Level::Fail);
        Self {
            findings,
            warnings,
            failures,
        }
    }
}

/// What answers on the configured proxy port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyProbe {
    Nsl,
    Other,
    Closed,
}

/// Proxy lifecycle files in the state directory.
#[derive(Debug)]
pub struct ProxyFiles {
    pub pid: Option<u32>,
    pub pid_alive: bool,
    pub port: Option<u16>,
}

/// Run all checks and print the report. Exits 1 when any check fails.
pub async fn run(config: &Config, json: bool) -> anyhow::Result<()> {
    let state_dir = config.resolve_state_dir();
    let mut findings = vec![check_state_dir(&state_dir)];

    let probe = probe_proxy(config.proxy_port).await;
    findings.extend(check_proxy(
        probe,
        config.proxy_port,
        &crate::status::read_proxy_files(&state_dir),
    ));

    if config.proxy_https || crate::status::read_tls_marker(&state_dir) {
        findings.push(check_ca(&state_dir));
    }

    let routes = RouteStore::new(state_dir).load_routes()?;
    if routes.is_empty() {
        findings.push(Finding::info("no routes are registered"));
    } else {
        findings.push(Finding::ok(format!("{} active route(s)", routes.len())));
        findings.extend(check_route_listeners(&routes));
        findings.extend(check_path_routes(&routes));
        findings.extend(check_dns(&dns_names(
            &routes,
            &config.domains,
            &config.domain_displays,
        )));
    }

    let report = Report::new(findings);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    if report.failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_report(report: &Report) {
    for f in &report.findings {
        let tag = match f.level {
            Level::Ok => "[ok]  ",
            Level::Warn => "[warn]",
            Level::Fail => "[fail]",
            Level::Info => "[info]",
        };
        println!("{tag} {}", f.message);
        if let Some(hint) = &f.hint {
            println!("       -> {hint}");
        }
    }
    println!();
    println!(
        "{} warning(s), {} failure(s)",
        report.warnings, report.failures
    );
}

fn check_state_dir(dir: &Path) -> Finding {
    if !dir.exists() {
        return Finding::info(format!(
            "state directory {} will be created on first run",
            dir.display()
        ));
    }
    let probe = dir.join(".doctor-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Finding::ok(format!("state directory {} is writable", dir.display()))
        }
        Err(e) => Finding::fail(
            format!("state directory {} is not writable: {e}", dir.display()),
            "fix its permissions or set [paths].state_dir / NSL_STATE_DIR",
        ),
    }
}

async fn probe_proxy(port: u16) -> ProxyProbe {
    if crate::proxy::is_proxy_running(port).await {
        ProxyProbe::Nsl
    } else if is_listening(port) {
        ProxyProbe::Other
    } else {
        ProxyProbe::Closed
    }
}

fn check_proxy(probe: ProxyProbe, configured: u16, files: &ProxyFiles) -> Vec<Finding> {
    let mut findings = Vec::new();
    if let Some(pid) = files.pid
        && !files.pid_alive
    {
        findings.push(Finding::warn(
            format!("stale proxy PID file (pid {pid} is not running)"),
            "nsl stop",
        ));
    }
    let mismatch = files.port.filter(|&p| files.pid_alive && p != configured);
    if let Some(running) = mismatch {
        findings.push(Finding::warn(
            format!("proxy is running on :{running}, but config listens on :{configured}"),
            "nsl reload",
        ));
    }
    match probe {
        ProxyProbe::Nsl => {
            findings.push(Finding::ok(format!("proxy is responding on :{configured}")))
        }
        ProxyProbe::Other => findings.push(Finding::fail(
            format!("port {configured} is in use by another program"),
            "stop that program, or change [proxy].listen / NSL_LISTEN",
        )),
        ProxyProbe::Closed if mismatch.is_none() => findings.push(Finding::warn(
            format!("proxy is not running on :{configured}"),
            "nsl start",
        )),
        ProxyProbe::Closed => {}
    }
    findings
}

fn check_ca(state_dir: &Path) -> Finding {
    ca_finding(
        crate::certs::ca_trust_status(state_dir),
        &crate::certs::ca_cert_path(state_dir),
    )
}

fn ca_finding(trusted: Option<bool>, ca_cert_path: &Path) -> Finding {
    match trusted {
        Some(true) => Finding::ok("local CA is trusted"),
        Some(false) => Finding::warn(
            "HTTPS is enabled but the local CA is not trusted",
            "sudo nsl trust",
        ),
        None => Finding::new(
            Level::Info,
            "CA trust cannot be checked on Windows",
            Some(&crate::certs::windows_trust_command(ca_cert_path)),
        ),
    }
}

fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok()
}

fn route_label(route: &RouteMapping) -> String {
    if route.path_prefix == "/" {
        route.hostname.clone()
    } else {
        format!("{}{}", route.hostname, route.path_prefix)
    }
}

fn check_route_listeners(routes: &[RouteMapping]) -> Vec<Finding> {
    routes
        .iter()
        .filter(|r| !is_listening(r.port))
        .map(|r| {
            Finding::warn(
                format!("route {} -> :{} has no listener", route_label(r), r.port),
                "start the app, or remove the route with `nsl route <name> --remove`",
            )
        })
        .collect()
}

fn check_path_routes(routes: &[RouteMapping]) -> Vec<Finding> {
    let mut hosts: BTreeMap<&str, bool> = BTreeMap::new();
    for r in routes {
        let has_root = hosts.entry(r.hostname.as_str()).or_insert(false);
        *has_root |= r.path_prefix == "/";
    }
    hosts
        .into_iter()
        .filter(|(_, has_root)| !has_root)
        .map(|(host, _)| {
            Finding::warn(
                format!("{host} has only path routes; requests outside them return 404"),
                "register a `/` route for this host if that is not intended",
            )
        })
        .collect()
}

/// Hostnames to resolve: every route label under each non-`localhost` domain
/// that is not fronted by an external proxy (`[proxy.display]`).
fn dns_names(
    routes: &[RouteMapping],
    domains: &[String],
    displays: &[crate::config::DomainDisplay],
) -> Vec<String> {
    let labels: BTreeSet<String> = routes
        .iter()
        .map(|r| crate::utils::extract_hostname_prefix(&r.hostname, domains))
        .collect();
    let names: BTreeSet<String> = labels
        .iter()
        .flat_map(|label| {
            domains
                .iter()
                .filter(|d| d.as_str() != "localhost" && !displays.iter().any(|x| &x.suffix == *d))
                .map(move |d| format!("{label}.{d}"))
        })
        .collect();
    names.into_iter().collect()
}

fn check_dns(names: &[String]) -> Vec<Finding> {
    names
        .iter()
        .filter(|name| {
            !(name.as_str(), 80)
                .to_socket_addrs()
                .is_ok_and(|mut addrs| addrs.any(|a| a.ip().is_loopback()))
        })
        .map(|name| {
            Finding::warn(
                format!("{name} does not resolve to a loopback address"),
                "sudo nsl hosts sync",
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::RouteMapping;

    fn route(hostname: &str, port: u16, path_prefix: &str) -> RouteMapping {
        RouteMapping {
            hostname: hostname.to_string(),
            port,
            pid: 0,
            change_origin: false,
            path_prefix: path_prefix.to_string(),
            strip_prefix: false,
            owner: None,
        }
    }

    fn files(pid: Option<u32>, pid_alive: bool, port: Option<u16>) -> ProxyFiles {
        ProxyFiles {
            pid,
            pid_alive,
            port,
        }
    }

    fn levels(findings: &[Finding]) -> Vec<Level> {
        findings.iter().map(|f| f.level).collect()
    }

    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn proxy_healthy() {
        let f = check_proxy(ProxyProbe::Nsl, 3355, &files(Some(1), true, Some(3355)));
        assert_eq!(levels(&f), vec![Level::Ok]);
    }

    #[test]
    fn proxy_not_running() {
        let f = check_proxy(ProxyProbe::Closed, 3355, &files(None, false, None));
        assert_eq!(levels(&f), vec![Level::Warn]);
        assert_eq!(f[0].hint.as_deref(), Some("nsl start"));
    }

    #[test]
    fn proxy_port_used_by_other_program() {
        let f = check_proxy(ProxyProbe::Other, 3355, &files(None, false, None));
        assert_eq!(levels(&f), vec![Level::Fail]);
        assert!(f[0].message.contains("another program"));
    }

    #[test]
    fn proxy_stale_pid_file() {
        let f = check_proxy(ProxyProbe::Closed, 3355, &files(Some(1), false, Some(3355)));
        assert!(
            f.iter()
                .any(|x| x.level == Level::Warn && x.message.contains("stale"))
        );
    }

    #[test]
    fn proxy_port_mismatch_suggests_reload() {
        let f = check_proxy(ProxyProbe::Closed, 3355, &files(Some(1), true, Some(8080)));
        assert_eq!(levels(&f), vec![Level::Warn]);
        assert_eq!(f[0].hint.as_deref(), Some("nsl reload"));
    }

    #[test]
    fn path_routes_without_root_warn() {
        let f = check_path_routes(&[route("shop.localhost", 4000, "/api")]);
        assert_eq!(levels(&f), vec![Level::Warn]);
        assert!(f[0].message.contains("shop.localhost"));
    }

    #[test]
    fn path_routes_with_root_ok() {
        let f = check_path_routes(&[
            route("shop.localhost", 4000, "/"),
            route("shop.localhost", 4001, "/api"),
        ]);
        assert!(f.is_empty());
    }

    #[test]
    fn route_listener_checks() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        let closed = closed_port();
        let f = check_route_listeners(&[
            route("up.localhost", open, "/"),
            route("down.localhost", closed, "/"),
        ]);
        assert_eq!(levels(&f), vec![Level::Warn]);
        assert!(f[0].message.contains("down.localhost"));
    }

    #[test]
    fn dns_names_skip_localhost_and_dedupe() {
        let domains = vec!["localhost".to_string(), "test".to_string()];
        let names = dns_names(
            &[
                route("shop.localhost", 4000, "/"),
                route("shop.localhost", 4001, "/api"),
                route("api.localhost", 4002, "/"),
            ],
            &domains,
            &[],
        );
        assert_eq!(names, vec!["api.test".to_string(), "shop.test".to_string()]);
    }

    #[test]
    fn dns_names_skip_domains_with_display_override() {
        let domains = vec![
            "localhost".to_string(),
            "test".to_string(),
            "dev.example.com".to_string(),
        ];
        let displays = vec![crate::config::DomainDisplay {
            suffix: "dev.example.com".to_string(),
            https: true,
            port: None,
        }];
        let names = dns_names(&[route("shop.localhost", 4000, "/")], &domains, &displays);
        assert_eq!(names, vec!["shop.test".to_string()]);
    }

    #[test]
    fn report_counts_and_json_shape() {
        let report = Report::new(vec![
            Finding::ok("a"),
            Finding::warn("b", "fix b"),
            Finding::fail("c", "fix c"),
            Finding::info("d"),
        ]);
        assert_eq!(report.warnings, 1);
        assert_eq!(report.failures, 1);
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["findings"][1]["level"], "warn");
        assert_eq!(json["findings"][1]["hint"], "fix b");
        assert!(json["findings"][0].get("hint").is_none());
    }

    #[test]
    fn ca_finding_levels() {
        let path = std::path::Path::new("/state/ca.pem");
        let unknown = ca_finding(None, path);
        assert_eq!(unknown.level, Level::Info);
        assert!(
            unknown
                .hint
                .as_deref()
                .unwrap()
                .contains("certutil -user -addstore Root")
        );
        assert!(unknown.hint.as_deref().unwrap().contains("/state/ca.pem"));

        let untrusted = ca_finding(Some(false), path);
        assert_eq!(untrusted.level, Level::Warn);
        assert_eq!(untrusted.hint.as_deref(), Some("sudo nsl trust"));

        assert_eq!(ca_finding(Some(true), path).level, Level::Ok);
    }

    #[test]
    fn state_dir_checks() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(check_state_dir(tmp.path()).level, Level::Ok);
        assert_eq!(
            check_state_dir(&tmp.path().join("missing")).level,
            Level::Info
        );
    }
}
