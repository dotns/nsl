//! Static file serving through the proxy.
//!
//! `nsl serve [DIR]` binds an in-process HTTP server (built on the same
//! hyper-1 stack as the proxy) to an allocated app port, registers a route
//! owned by the current process, and serves `DIR` with
//! [`tower_http::services::ServeDir`] until interrupted. Unlike `nsl run`
//! there is no child process: this `nsl` process *is* the upstream.
//!
//! `ServeDir` covers files, `index.html`, content-type detection and range
//! requests. The `--spa` and `--list` behaviours are layered on top by
//! post-processing its `404` responses (SPA fallback to `index.html`, and a
//! generated directory listing), since `ServeDir` provides neither natively.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use http_body_util::{BodyExt, Empty, Full, combinators::UnsyncBoxBody};
use hyper::body::{Bytes, Incoming};
use hyper::header;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::discover::infer_project_name;
use crate::routes::{RouteOwner, RouteStore};
use crate::run::find_free_port;
use crate::utils::{extract_hostname_prefix, format_urls, parse_hostname};

/// Concrete response body for the static server. Boxing unifies the
/// `ServeDir`, listing and SPA bodies into one nameable type, mirroring the
/// proxy's `ProxyBody` idiom.
type StaticBody = UnsyncBoxBody<Bytes, io::Error>;

/// Characters escaped when building a URL path segment for a listing link.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/');

/// Serve a static directory through the nsl proxy.
pub async fn serve_app(
    config: &Config,
    dir: &Path,
    name_override: Option<&str>,
    path: &str,
    strip_prefix: bool,
    spa: bool,
    list: bool,
) -> anyhow::Result<()> {
    // Resolve and validate the directory up front so failures are reported
    // before we touch the proxy or routes.
    let root = std::fs::canonicalize(dir)
        .map_err(|e| anyhow::anyhow!("cannot serve {}: {}", dir.display(), e))?;
    if !root.is_dir() {
        anyhow::bail!("not a directory: {}", root.display());
    }

    let hostname = match name_override {
        Some(name) => {
            parse_hostname(name, &config.domains).map_err(|e| anyhow::anyhow!("{}", e))?
        }
        None => {
            let project = infer_project_name(&root);
            parse_hostname(&project, &config.domains).map_err(|e| anyhow::anyhow!("{}", e))?
        }
    };

    // Ensure the proxy daemon is running.
    if crate::proxy::ensure_proxy_running(config).await? {
        tracing::info!("proxy not running, started daemon");
    }

    // `nsl serve` is the server itself, so the port is purely internal: always
    // auto-allocate and ignore any configured `[app].port`. Bind *before*
    // registering the route, so the listener is already accepting the instant
    // the route becomes visible.
    let app_port = find_free_port(config.app_port_range.0, config.app_port_range.1)?;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), app_port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind 127.0.0.1:{}: {}", app_port, e))?;

    // Register the route, owned by this process. A static server ignores the
    // Host header, so `change_origin` is always false.
    let pid = std::process::id();
    let owner = build_owner(pid, &root);
    let store = RouteStore::new(config.resolve_state_dir());
    store
        .add_route_with_owner(
            &hostname,
            app_port,
            pid,
            Some(owner),
            config.app_force,
            false,
            path,
            strip_prefix,
        )
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    print_serve_info(config, &root, app_port, pid, &hostname, path, spa, list);

    let result = accept_until_shutdown(listener, root.clone(), spa, list).await;

    // Best-effort route cleanup. Even if this fails, the dead pid causes the
    // proxy to prune the route automatically.
    let path_filter = if path == "/" { None } else { Some(path) };
    let _ = store.remove_route_for_pid(&hostname, path_filter, pid);

    result
}

/// Build the route owner record for the current process.
fn build_owner(pid: u32, root: &Path) -> RouteOwner {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    RouteOwner {
        pid,
        platform: crate::platform::current_platform().to_string(),
        cwd,
        command: vec![
            "nsl".to_string(),
            "serve".to_string(),
            root.display().to_string(),
        ],
        process_group: crate::platform::current_process_group(pid),
        start_time: crate::platform::current_process_start_time(pid),
    }
}

/// Run the static file server, accepting connections until a shutdown signal.
///
/// A single `ServeDir` handles files and `index.html`; its `404`s are then
/// post-processed: when `list` is set and the path is a directory a listing is
/// generated, and when `spa` is set the root `index.html` is served with a
/// `200`. `ServeDir` never errors (IO failures become HTTP responses), so the
/// `Infallible` arm is unreachable.
async fn accept_until_shutdown(
    listener: TcpListener,
    root: PathBuf,
    spa: bool,
    list: bool,
) -> anyhow::Result<()> {
    let serve_dir = ServeDir::new(&root).append_index_html_on_directories(true);
    let service = service_fn(move |req: Request<Incoming>| {
        let dir = serve_dir.clone();
        let root = root.clone();
        async move {
            let req_path = req.uri().path().to_string();
            let mut res = dir.oneshot(req).await.unwrap_or_else(|e| match e {});
            if res.status() == StatusCode::NOT_FOUND {
                if list && let Some(listing) = directory_listing(&root, &req_path).await {
                    return Ok(listing);
                }
                if spa && let Some(index) = spa_index(&root).await {
                    return Ok(index);
                }
            } else if res.status().is_redirection() {
                // `ServeDir` appends a trailing slash via an absolute redirect;
                // make it relative so it survives a proxy mount prefix.
                relativize_location(&mut res);
            }
            Ok::<Response<StaticBody>, Infallible>(res.map(|b| b.boxed_unsync()))
        }
    });
    accept_loop(listener, service, shutdown_signal()).await
}

/// Accept connections, serving each on its own task, until `shutdown` fires.
async fn accept_loop<S>(
    listener: TcpListener,
    service: S,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()>
where
    S: hyper::service::Service<
            Request<Incoming>,
            Response = Response<StaticBody>,
            Error = Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _peer)) => stream,
                    Err(e) => {
                        tracing::warn!("accept error: {}", e);
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let service = service.clone();
                tokio::spawn(async move {
                    if let Err(e) = AutoBuilder::new(TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::debug!("connection error: {}", e);
                    }
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory listing (`--list`) and SPA fallback (`--spa`)
// ---------------------------------------------------------------------------

/// Generate an HTML directory listing for `req_path` if it resolves to a
/// directory under `root`. Returns `None` (leaving the original `404`) when the
/// path is not an existing directory.
async fn directory_listing(root: &Path, req_path: &str) -> Option<Response<StaticBody>> {
    let target = resolve_dir(root, req_path).await?;

    // Without a trailing slash, relative links would resolve against the
    // parent. Redirect to the slashed form (relative, so it stays correct even
    // when mounted under a path prefix at the proxy).
    if !req_path.ends_with('/') {
        let last = req_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
        return Some(redirect(&format!("{}/", last)));
    }

    let mut entries: Vec<(String, bool)> = Vec::new();
    let mut rd = tokio::fs::read_dir(&target).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        entries.push((name, is_dir));
    }
    // Directories first, then case-sensitive name order.
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Some(html_response(
        StatusCode::OK,
        render_listing(req_path, &entries),
    ))
}

/// Rewrite an absolute trailing-slash redirect `Location` to a relative one.
///
/// `ServeDir` redirects `/dir` to the absolute `/dir/`, which points outside
/// the mount when the route is mounted under a prefix with `--strip`. Reducing
/// it to the final path segment (`dir/`) keeps the redirect correct in both the
/// root and mounted cases.
fn relativize_location<B>(res: &mut Response<B>) {
    let Some(rel) = res
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .filter(|loc| loc.starts_with('/'))
        .map(|loc| {
            format!(
                "{}/",
                loc.trim_end_matches('/').rsplit('/').next().unwrap_or("")
            )
        })
    else {
        return;
    };
    if let Ok(value) = header::HeaderValue::from_str(&rel) {
        res.headers_mut().insert(header::LOCATION, value);
    }
}

/// Securely resolve `req_path` to an existing directory under `root`.
///
/// Defence is layered:
/// 1. Percent-decode the *whole* path first, then split on `/`, so encoded
///    separators and dots (`%2e%2e%2f`) collapse into real `..` components that
///    the next step can see — rather than slipping through as opaque segments.
/// 2. Reject any `..` (traversal) or NUL component outright.
/// 3. Canonicalize the target and require it to stay within the (already
///    canonical) `root`. This is the authoritative guard: it also defeats
///    OS-specific separators (e.g. Windows `\`, drive/UNC prefixes) and
///    symlinks that point outside `root`, since canonicalization resolves them
///    to a real absolute path that then fails the containment check.
///
/// Only directory *listings* flow through here; file bytes are served by
/// `ServeDir`, which has its own traversal protection.
async fn resolve_dir(root: &Path, req_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode_str(req_path).decode_utf8().ok()?;
    let mut target = root.to_path_buf();
    for comp in decoded.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return None,
            _ if comp.contains('\0') => return None,
            _ => target.push(comp),
        }
    }
    let canon = tokio::fs::canonicalize(&target).await.ok()?;
    if !canon.starts_with(root) {
        return None;
    }
    let meta = tokio::fs::metadata(&canon).await.ok()?;
    meta.is_dir().then_some(canon)
}

/// Render the listing HTML. Links are relative to the current directory URL so
/// they work regardless of any proxy mount prefix.
fn render_listing(req_path: &str, entries: &[(String, bool)]) -> String {
    let title = html_escape(req_path);
    let mut html = String::new();
    let _ = write!(
        html,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Index of {title}</title>\
         <style>body{{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,\
         Helvetica,Arial,sans-serif;max-width:720px;margin:40px auto;padding:0 16px}}\
         h1{{font-size:18px;word-break:break-all}}ul{{list-style:none;padding:0}}\
         li{{padding:4px 0;border-bottom:1px solid #eee}}a{{text-decoration:none;color:#0070f3}}\
         a:hover{{text-decoration:underline}}footer{{margin-top:24px;color:#999;font-size:13px}}</style>\
         </head><body><h1>Index of {title}</h1><ul>"
    );
    if req_path != "/" {
        let _ = write!(html, "<li><a href=\"../\">../</a></li>");
    }
    for (name, is_dir) in entries {
        let href = utf8_percent_encode(name, PATH_SEGMENT);
        let slash = if *is_dir { "/" } else { "" };
        let _ = write!(
            html,
            "<li><a href=\"{href}{slash}\">{}{slash}</a></li>",
            html_escape(name)
        );
    }
    let _ = write!(html, "</ul><footer>nsl serve</footer></body></html>");
    html
}

/// Serve the root `index.html` with a `200` status for SPA client routes.
async fn spa_index(root: &Path) -> Option<Response<StaticBody>> {
    let bytes = tokio::fs::read(root.join("index.html")).await.ok()?;
    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(boxed_full(Bytes::from(bytes)))
            .expect("spa response is valid"),
    )
}

/// Minimal HTML text escaping for listing entry names and titles.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Box a `Full` body into [`StaticBody`] (the `Infallible` error coerces to the
/// body's `io::Error` type).
fn boxed_full(body: Bytes) -> StaticBody {
    Full::new(body)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn html_response(status: StatusCode, html: String) -> Response<StaticBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(boxed_full(Bytes::from(html)))
        .expect("listing response is valid")
}

fn redirect(location: &str) -> Response<StaticBody> {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header(header::LOCATION, location)
        .body(
            Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("redirect response is valid")
}

/// Resolve when the process receives an interrupt/terminate signal.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return,
    };
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Print the connection banner, mirroring `nsl run`'s output.
fn print_serve_info(
    config: &Config,
    root: &Path,
    app_port: u16,
    pid: u32,
    hostname: &str,
    path: &str,
    spa: bool,
    list: bool,
) {
    let prefix = extract_hostname_prefix(hostname, &config.domains);
    let urls = format_urls(
        &prefix,
        &config.domains,
        config.proxy_port,
        config.proxy_https,
        &config.domain_displays,
    );
    let path_suffix = if path != "/" { path } else { "" };

    println!();
    println!("nsl v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  Serving: {}", root.display());
    if spa {
        println!("  Mode:    SPA (index.html fallback)");
    }
    if list {
        println!("  Mode:    directory listing");
    }
    println!("  Port:    {} (allocated)", app_port);
    println!("  PID:     {}", pid);
    println!();
    println!("  URLs:");
    for url in &urls {
        println!("    {}{}", url, path_suffix);
    }
    println!();
    println!(
        "  Proxy:   http://127.0.0.1:{} (running)",
        config.proxy_port
    );
    println!();
    println!("  press ctrl+c to stop");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_owner_records_serve_command_and_pid() {
        let pid = std::process::id();
        let owner = build_owner(pid, Path::new("/tmp/site"));
        assert_eq!(owner.pid, pid);
        assert_eq!(owner.command, vec!["nsl", "serve", "/tmp/site"]);
        assert_eq!(owner.platform, crate::platform::current_platform());
    }

    #[test]
    fn html_escape_escapes_markup() {
        assert_eq!(html_escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
        assert_eq!(html_escape("plain.txt"), "plain.txt");
    }

    #[test]
    fn render_listing_encodes_links_and_appends_slash() {
        // `render_listing` renders entries in the order given (the caller sorts).
        let entries = vec![("a dir".to_string(), true), ("file.txt".to_string(), false)];
        let html = render_listing("/", &entries);
        // Directory name percent-encoded in href, trailing slash appended.
        assert!(html.contains("href=\"a%20dir/\">a dir/</a>"));
        assert!(html.contains("href=\"file.txt\">file.txt</a>"));
        // Root listing has no parent link.
        assert!(!html.contains("\"../\""));
    }

    #[test]
    fn render_listing_has_parent_link_below_root() {
        let html = render_listing("/sub/", &[]);
        assert!(html.contains("href=\"../\""));
    }

    #[tokio::test]
    async fn directory_listing_lists_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();

        let res = directory_listing(&root, "/")
            .await
            .expect("listing produced");
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("hello.txt"));
        assert!(html.contains("nested/"));
        // Directories are sorted before files.
        let dir_idx = html.find("nested/").expect("dir entry");
        let file_idx = html.find("hello.txt").expect("file entry");
        assert!(dir_idx < file_idx, "directories list before files");
    }

    #[test]
    fn relativize_location_reduces_absolute_redirect_to_segment() {
        let mut res = Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, "/sub%20folder/")
            .body(())
            .unwrap();
        relativize_location(&mut res);
        assert_eq!(res.headers()[header::LOCATION], "sub%20folder/");

        // A nested path keeps only the final segment, so it resolves relative
        // to the browser's current directory (mount-safe).
        let mut nested = Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, "/a/b/c/")
            .body(())
            .unwrap();
        relativize_location(&mut nested);
        assert_eq!(nested.headers()[header::LOCATION], "c/");
    }

    #[tokio::test]
    async fn directory_listing_redirects_without_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();

        let res = directory_listing(&root, "/docs").await.expect("redirect");
        assert_eq!(res.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(res.headers()[header::LOCATION], "docs/");
    }

    #[tokio::test]
    async fn resolve_dir_rejects_traversal_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"x").unwrap();
        std::fs::create_dir(tmp.path().join("ok")).unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();

        // Plain and percent-encoded traversal, including encoded separators.
        assert!(resolve_dir(&root, "/../").await.is_none());
        assert!(resolve_dir(&root, "/../../etc").await.is_none());
        assert!(resolve_dir(&root, "/%2e%2e/").await.is_none());
        assert!(resolve_dir(&root, "/%2e%2e%2f%2e%2e%2fetc").await.is_none());
        assert!(resolve_dir(&root, "/ok/../../etc").await.is_none());
        // NUL byte injection.
        assert!(resolve_dir(&root, "/ok%00/").await.is_none());
        // Files are not directories; missing paths resolve to nothing.
        assert!(resolve_dir(&root, "/a.txt").await.is_none());
        assert!(resolve_dir(&root, "/nope").await.is_none());
        // Legitimate directories resolve.
        assert!(resolve_dir(&root, "/").await.is_some());
        assert!(resolve_dir(&root, "/ok/").await.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_dir_rejects_symlink_escape() {
        // A symlink inside root that points outside root must not be listable:
        // canonicalization resolves it and the containment check rejects it.
        let root_tmp = tempfile::tempdir().unwrap();
        let outside_tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root_tmp.path()).unwrap();
        let outside = std::fs::canonicalize(outside_tmp.path()).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        assert!(resolve_dir(&root, "/escape/").await.is_none());
    }
}
