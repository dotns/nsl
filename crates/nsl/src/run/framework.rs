/// Find a free port in the given range.
///
/// Strategy: try a random port first, then scan sequentially.
pub fn find_free_port(range_start: u16, range_end: u16) -> anyhow::Result<u16> {
    use std::net::TcpListener;

    // Try random ports first (up to 10 attempts)
    for _ in 0..10 {
        let port = range_start + (rand_u16() % (range_end - range_start + 1));
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    // Sequential fallback
    for port in range_start..=range_end {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    anyhow::bail!("no free port found in range {}-{}", range_start, range_end)
}

/// Simple pseudo-random u16 using time-based seed.
fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (t ^ (t >> 16)) as u16
}

/// Inject framework-specific flags (--port, --host) if not already present.
///
/// Direct invocations (`vite`, `npx vite`, `astro dev`, …) are detected
/// by scanning the joined command string. Package-manager wrappers
/// (`bun run dev`, `npm run dev`, `yarn dev`, `pnpm run dev`) hide the
/// framework name behind a script alias, so when direct detection
/// misses we resolve the script through `./package.json` and check
/// what that script actually runs.
pub fn inject_framework_flags(args: &[String], port: u16) -> Vec<String> {
    let mut result = args.to_vec();

    let cmd_str = args.join(" ");
    let framework = detect_framework(&cmd_str).or_else(|| detect_framework_via_package_json(args));

    if let Some(fw) = framework {
        let has_port = args.iter().any(|a| a.starts_with("--port"));
        let has_host = args.iter().any(|a| a.starts_with(fw.host_flag));

        if has_port && has_host {
            return result;
        }

        // npm / pnpm / yarn-run require `--` before forwarding args to
        // the underlying script. bun / yarn-without-run forward
        // trailing args unchanged.
        let wrapper = detect_pkg_manager_wrapper(args);
        if wrapper.needs_separator && !args.iter().any(|a| a == "--") {
            result.push("--".to_string());
        }
        if !has_port {
            result.push("--port".to_string());
            result.push(port.to_string());
            if fw.strict_port {
                result.push("--strictPort".to_string());
            }
        }
        if !has_host {
            result.push(fw.host_flag.to_string());
            result.push(fw.host.to_string());
        }
    }

    result
}

/// Best-effort: if `args` looks like `<pkg-mgr> [run] <script>`, read
/// `./package.json`, look up that script, and re-run framework
/// detection on its actual command.
fn detect_framework_via_package_json(args: &[String]) -> Option<FrameworkHint> {
    let script_name = npm_script_name(args)?;
    let cwd = std::env::current_dir().ok()?;
    let pkg_path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&pkg_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let script_value = json.get("scripts")?.get(&script_name)?.as_str()?;
    detect_framework(script_value)
}

/// Extract the npm-style script name from a wrapper command, if any.
/// Returns `None` for direct invocations (`vite`, `node server.js`, …).
fn npm_script_name(args: &[String]) -> Option<String> {
    let first = args.first().map(|s| s.as_str())?;
    let second = args.get(1).map(|s| s.as_str()).unwrap_or("");
    match (first, second) {
        // `npm run <name>` / `pnpm run <name>` / `yarn run <name>` /
        // `bun run <name>`.
        ("npm" | "pnpm" | "yarn" | "bun", "run") => args.get(2).cloned(),
        // `yarn <name>` / `bun <name>` — second arg is the script when
        // it doesn't look like a flag.
        ("yarn" | "bun", s) if !s.is_empty() && !s.starts_with('-') => Some(s.to_string()),
        _ => None,
    }
}

struct WrapperInfo {
    needs_separator: bool,
}

/// Decide whether to insert `--` before forwarded flags so the
/// wrapper passes them on instead of trying to parse them itself.
fn detect_pkg_manager_wrapper(args: &[String]) -> WrapperInfo {
    let first = args.first().map(|s| s.as_str()).unwrap_or("");
    let second = args.get(1).map(|s| s.as_str()).unwrap_or("");
    match (first, second) {
        ("npm" | "pnpm", "run") => WrapperInfo {
            needs_separator: true,
        },
        ("yarn", "run") => WrapperInfo {
            needs_separator: true,
        },
        _ => WrapperInfo {
            needs_separator: false,
        },
    }
}

/// Replace the literal app-port placeholder in command arguments.
pub fn replace_port_placeholders(args: &[String], port: u16) -> Vec<String> {
    let port = port.to_string();
    args.iter()
        .map(|arg| arg.replace("NSL_PORT", &port))
        .collect()
}

struct FrameworkHint {
    strict_port: bool,
    host_flag: &'static str,
    host: &'static str,
}

fn detect_framework(cmd: &str) -> Option<FrameworkHint> {
    if cmd.contains("vite") || cmd.contains("react-router") {
        Some(FrameworkHint {
            strict_port: true,
            host_flag: "--host",
            host: "127.0.0.1",
        })
    } else if cmd.contains("astro") || cmd.contains(" ng ") || cmd.contains("react-native") {
        Some(FrameworkHint {
            strict_port: false,
            host_flag: "--host",
            host: "127.0.0.1",
        })
    } else if cmd.contains("expo") {
        Some(FrameworkHint {
            strict_port: false,
            host_flag: "--host",
            host: "localhost",
        })
    } else if cmd.contains("wrangler") && cmd.contains("dev") {
        // `wrangler dev` ignores the PORT env var, and its bind-address
        // flag is `--ip`; `--host` means the zone host to route to.
        Some(FrameworkHint {
            strict_port: false,
            host_flag: "--ip",
            host: "127.0.0.1",
        })
    } else {
        None
    }
}

/// Wait for an app to become ready by polling a TCP connection.
///
/// Polling strategy: first 5 attempts at 200ms interval, then 500ms.
#[allow(dead_code)]
pub async fn wait_for_app(
    port: u16,
    timeout_secs: u64,
    child: &mut tokio::process::Child,
) -> anyhow::Result<()> {
    if timeout_secs == 0 {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt: u32 = 0;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("app exited before becoming ready (exit status: {})", status);
            }
            Err(e) => {
                anyhow::bail!("failed to check child process status: {}", e);
            }
            Ok(None) => {}
        }

        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "app did not become ready within {}s timeout, continuing anyway",
                timeout_secs
            );
            return Ok(());
        }

        let interval = if attempt < 5 {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_millis(500)
        };
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

/// Wait for an app to become ready (variant for process-wrap child).
#[cfg(unix)]
pub async fn wait_for_app_wrapped(
    port: u16,
    timeout_secs: u64,
    child: &mut Box<dyn process_wrap::tokio::ChildWrapper>,
) -> anyhow::Result<()> {
    if timeout_secs == 0 {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut attempt: u32 = 0;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("app exited before becoming ready (exit status: {})", status);
            }
            Err(e) => {
                anyhow::bail!("failed to check child process status: {}", e);
            }
            Ok(None) => {}
        }

        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "app did not become ready within {}s timeout, continuing anyway",
                timeout_secs
            );
            return Ok(());
        }

        let interval = if attempt < 5 {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_millis(500)
        };
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_free_port() {
        let port = find_free_port(4000, 4999).unwrap();
        assert!((4000..=4999).contains(&port));

        let listener = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(listener.is_ok());
    }

    #[test]
    fn test_inject_framework_flags_vite() {
        let args = vec!["npx".to_string(), "vite".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert!(result.contains(&"--port".to_string()));
        assert!(result.contains(&"4000".to_string()));
        assert!(result.contains(&"--strictPort".to_string()));
        assert!(result.contains(&"--host".to_string()));
        assert!(result.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn test_inject_framework_flags_no_override() {
        let args = vec![
            "npx".to_string(),
            "vite".to_string(),
            "--port".to_string(),
            "3000".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
        ];
        let result = inject_framework_flags(&args, 4000);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_framework_flags_unknown() {
        let args = vec!["python".to_string(), "server.py".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert_eq!(result, args);
    }

    #[test]
    fn test_npm_script_name_extraction() {
        assert_eq!(
            npm_script_name(&["npm".into(), "run".into(), "dev".into()]),
            Some("dev".into())
        );
        assert_eq!(
            npm_script_name(&["pnpm".into(), "run".into(), "dev".into()]),
            Some("dev".into())
        );
        assert_eq!(
            npm_script_name(&["bun".into(), "run".into(), "dev".into()]),
            Some("dev".into())
        );
        assert_eq!(
            npm_script_name(&["yarn".into(), "dev".into()]),
            Some("dev".into())
        );
        assert_eq!(
            npm_script_name(&["bun".into(), "dev".into()]),
            Some("dev".into())
        );
        // Flags are not scripts.
        assert_eq!(npm_script_name(&["yarn".into(), "--help".into()]), None);
        // Not a wrapper.
        assert_eq!(npm_script_name(&["vite".into()]), None);
    }

    #[test]
    fn test_detect_pkg_manager_wrapper_needs_separator() {
        assert!(
            detect_pkg_manager_wrapper(&["npm".into(), "run".into(), "dev".into()]).needs_separator
        );
        assert!(
            detect_pkg_manager_wrapper(&["pnpm".into(), "run".into(), "dev".into()])
                .needs_separator
        );
        assert!(
            detect_pkg_manager_wrapper(&["yarn".into(), "run".into(), "dev".into()])
                .needs_separator
        );
        // bun-run forwards trailing args without `--`.
        assert!(
            !detect_pkg_manager_wrapper(&["bun".into(), "run".into(), "dev".into()])
                .needs_separator
        );
        // yarn <script> without `run` forwards directly.
        assert!(!detect_pkg_manager_wrapper(&["yarn".into(), "dev".into()]).needs_separator);
        // Plain commands.
        assert!(!detect_pkg_manager_wrapper(&["vite".into()]).needs_separator);
    }

    /// End-to-end: `bun run dev` in a directory whose `package.json`
    /// has `scripts.dev = "vite"` should inject `--port`, `--host`,
    /// and `--strictPort` (no `--` separator because bun forwards
    /// trailing args natively).
    #[test]
    fn test_inject_via_package_json_bun_run_vite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();

        // Temporarily change CWD for the duration of the test. Tests
        // in the same crate that mutate CWD must not run in parallel
        // with this one. cargo's test harness serializes the closure
        // because we hold a guard on a Mutex below.
        let _guard = with_cwd(dir.path());
        let args = vec!["bun".into(), "run".into(), "dev".into()];
        let result = inject_framework_flags(&args, 4000);

        assert!(result.contains(&"--port".to_string()));
        assert!(result.contains(&"4000".to_string()));
        assert!(result.contains(&"--strictPort".to_string()));
        assert!(result.contains(&"--host".to_string()));
        assert!(result.contains(&"127.0.0.1".to_string()));
        // No `--` for bun.
        assert!(!result.contains(&"--".to_string()));
    }

    /// `npm run dev` with vite in package.json: inject WITH the `--`
    /// separator so npm forwards the new flags to vite instead of
    /// trying to parse them itself.
    #[test]
    fn test_inject_via_package_json_npm_run_vite_inserts_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();

        let _guard = with_cwd(dir.path());
        let args = vec!["npm".into(), "run".into(), "dev".into()];
        let result = inject_framework_flags(&args, 4000);

        assert!(result.contains(&"--".to_string()));
        // `--` must come BEFORE `--port`.
        let dash_pos = result.iter().position(|a| a == "--").unwrap();
        let port_pos = result.iter().position(|a| a == "--port").unwrap();
        assert!(dash_pos < port_pos);
    }

    /// No injection when the script doesn't run a recognised framework.
    #[test]
    fn test_no_inject_when_script_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"node server.js"}}"#,
        )
        .unwrap();

        let _guard = with_cwd(dir.path());
        let args = vec!["bun".into(), "run".into(), "dev".into()];
        let result = inject_framework_flags(&args, 4000);

        assert_eq!(result, args);
    }

    /// Helper: serialise `set_current_dir` so concurrent test threads
    /// don't trample each other.
    fn with_cwd(path: &std::path::Path) -> CwdGuard {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        CwdGuard {
            prev,
            _lock: Box::new(guard),
        }
    }

    struct CwdGuard {
        prev: std::path::PathBuf,
        _lock: Box<dyn std::any::Any + 'static>,
    }
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    /// `wrangler dev` ignores PORT, so nsl must inject `--port` and the
    /// wrangler-specific `--ip` bind flag (never `--host`, which means
    /// the zone host in wrangler).
    #[test]
    fn test_inject_framework_flags_wrangler_dev() {
        let args = vec!["wrangler".to_string(), "dev".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert!(result.contains(&"--port".to_string()));
        assert!(result.contains(&"4000".to_string()));
        assert!(result.contains(&"--ip".to_string()));
        assert!(result.contains(&"127.0.0.1".to_string()));
        assert!(!result.contains(&"--host".to_string()));
        assert!(!result.contains(&"--strictPort".to_string()));
    }

    /// `wrangler deploy` is not a dev server; nothing is injected.
    #[test]
    fn test_no_inject_wrangler_deploy() {
        let args = vec!["wrangler".to_string(), "deploy".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert_eq!(result, args);
    }

    /// Hono's Cloudflare template: `npm run dev` with
    /// `scripts.dev = "wrangler dev"` injects via the `--` separator.
    #[test]
    fn test_inject_via_package_json_npm_run_wrangler() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"wrangler dev"}}"#,
        )
        .unwrap();

        let _guard = with_cwd(dir.path());
        let args = vec!["npm".into(), "run".into(), "dev".into()];
        let result = inject_framework_flags(&args, 4000);

        let dash_pos = result.iter().position(|a| a == "--").unwrap();
        let port_pos = result.iter().position(|a| a == "--port").unwrap();
        assert!(dash_pos < port_pos);
        assert!(result.contains(&"--ip".to_string()));
    }

    #[test]
    fn test_inject_framework_flags_expo() {
        let args = vec!["npx".to_string(), "expo".to_string(), "start".to_string()];
        let result = inject_framework_flags(&args, 4000);
        assert!(result.contains(&"localhost".to_string()));
    }

    #[test]
    fn test_replace_port_placeholders_whole_arg() {
        let args = vec![
            "./server".to_string(),
            "-port".to_string(),
            "NSL_PORT".to_string(),
        ];

        let result = replace_port_placeholders(&args, 4000);

        assert_eq!(result, vec!["./server", "-port", "4000"]);
    }

    #[test]
    fn test_replace_port_placeholders_inside_arg() {
        let args = vec![
            "./server".to_string(),
            "--addr=127.0.0.1:NSL_PORT".to_string(),
        ];

        let result = replace_port_placeholders(&args, 4000);

        assert_eq!(result, vec!["./server", "--addr=127.0.0.1:4000"]);
    }

    #[tokio::test]
    async fn test_wait_for_app_disabled() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();

        let result = wait_for_app(0, 0, &mut child).await;
        assert!(result.is_ok());

        child.kill().await.ok();
    }

    #[tokio::test]
    async fn test_wait_for_app_already_listening() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();

        let result = wait_for_app(port, 5, &mut child).await;
        assert!(result.is_ok());

        child.kill().await.ok();
        drop(listener);
    }

    #[tokio::test]
    async fn test_wait_for_app_child_exits_early() {
        let mut child = tokio::process::Command::new("true").spawn().unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result = wait_for_app(19999, 5, &mut child).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("exited before becoming ready"));
    }
}
