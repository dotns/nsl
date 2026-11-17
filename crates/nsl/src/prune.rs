use std::time::{Duration, Instant};

use crate::config::Config;
use crate::platform::{force_kill_app_process, is_process_alive, kill_app_process};
use crate::routes::{RouteOwner, RouteStore, is_orphan};

const TERM_WAIT: Duration = Duration::from_secs(3);
const KILL_WAIT: Duration = Duration::from_secs(1);

/// Stop app processes whose `nsl run` supervisor is gone and remove their routes.
pub fn run(config: &Config, force: bool) -> anyhow::Result<()> {
    let store = RouteStore::new(config.resolve_state_dir());
    store.compact()?;

    let orphans: Vec<_> = store
        .load_routes()?
        .into_iter()
        .filter(|r| r.owner.as_ref().is_some_and(is_orphan))
        .collect();

    if orphans.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    let mut failed = 0;
    for route in &orphans {
        let Some(owner) = route.owner.as_ref() else {
            continue;
        };
        let path = if route.path_prefix == "/" {
            ""
        } else {
            route.path_prefix.as_str()
        };
        let label = format!("{}{} (pid {})", route.hostname, path, route.pid);
        match stop(owner, force) {
            Ok(()) => {
                store.remove_route_for_pid(&route.hostname, Some(&route.path_prefix), route.pid)?;
                println!("stopped {label}");
            }
            Err(e) => {
                failed += 1;
                println!("kept {label}: {e}");
            }
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} orphaned app(s) could not be stopped");
    }
    Ok(())
}

fn stop(owner: &RouteOwner, force: bool) -> anyhow::Result<()> {
    kill_app_process(owner)?;
    if wait_for_exit(owner.pid, TERM_WAIT) {
        return Ok(());
    }
    if !force {
        anyhow::bail!("still running after SIGTERM; retry with `nsl prune --force`");
    }
    force_kill_app_process(owner)?;
    if wait_for_exit(owner.pid, KILL_WAIT) {
        return Ok(());
    }
    anyhow::bail!("still running after SIGKILL")
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !is_process_alive(pid)
}
