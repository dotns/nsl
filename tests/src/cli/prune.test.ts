import { describe, test, expect, beforeAll, afterAll, afterEach } from "bun:test";
import { spawn, type Subprocess } from "bun";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import {
  allocPort,
  createStateDir,
  cleanupStateDir,
  nsl,
  nslEnv,
  startProxy,
  stopProxy,
  startUpstream,
  type UpstreamServer,
} from "../helpers";

const NSL_BIN = process.env.NSL_BIN ?? "nsl";

interface StoredRoute {
  hostname: string;
  pid: number;
}

function readRoutes(stateDir: string): StoredRoute[] {
  const path = join(stateDir, "routes.json");
  if (!existsSync(path)) return [];
  return JSON.parse(readFileSync(path, "utf8"));
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitFor(cond: () => boolean, timeoutMs = 10_000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (cond()) return true;
    await Bun.sleep(100);
  }
  return cond();
}

function spawnNslRun(name: string, appPort: number, stateDir: string, proxyPort: number): Subprocess {
  return spawn(
    [NSL_BIN, "run", "--name", name, "--port", String(appPort), "--force", "--", "sleep", "60"],
    { env: nslEnv(stateDir, proxyPort), stdout: "pipe", stderr: "pipe" },
  );
}

/** Start `nsl run` and return the owner PID recorded in routes.json. */
async function runAndGetOwner(
  name: string,
  appPort: number,
  stateDir: string,
  proxyPort: number,
): Promise<{ proc: Subprocess; ownerPid: number }> {
  const proc = spawnNslRun(name, appPort, stateDir, proxyPort);
  const host = `${name}.localhost`;
  const ok = await waitFor(() => readRoutes(stateDir).some((r) => r.hostname === host));
  if (!ok) throw new Error(`route ${host} was not registered`);
  const ownerPid = readRoutes(stateDir).find((r) => r.hostname === host)!.pid;
  return { proc, ownerPid };
}

describe("orphaned app processes", () => {
  let stateDir: string;
  let proxyPort: number;
  let upstream: UpstreamServer;
  const owners: number[] = [];

  beforeAll(async () => {
    stateDir = createStateDir();
    proxyPort = allocPort();
    upstream = startUpstream();
    await startProxy(stateDir, proxyPort);
  });

  afterEach(() => {
    for (const pid of owners.splice(0)) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // already gone
      }
    }
  });

  afterAll(async () => {
    await stopProxy();
    upstream.stop();
    cleanupStateDir(stateDir);
  });

  test("SIGHUP to nsl run stops the app and removes its route", async () => {
    const { proc, ownerPid } = await runAndGetOwner("hup", upstream.port, stateDir, proxyPort);
    owners.push(ownerPid);

    proc.kill("SIGHUP");
    await proc.exited;

    expect(await waitFor(() => !isAlive(ownerPid), 5_000)).toBe(true);
    expect(readRoutes(stateDir).some((r) => r.hostname === "hup.localhost")).toBe(false);
  });

  test("prune stops an app whose nsl run was killed", async () => {
    const { proc, ownerPid } = await runAndGetOwner("orphan", upstream.port, stateDir, proxyPort);
    owners.push(ownerPid);

    proc.kill("SIGKILL");
    await proc.exited;
    expect(isAlive(ownerPid)).toBe(true);

    const result = await nsl(["prune"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("orphan.localhost");

    expect(await waitFor(() => !isAlive(ownerPid), 5_000)).toBe(true);
    expect(readRoutes(stateDir).some((r) => r.hostname === "orphan.localhost")).toBe(false);
  });

  test("prune leaves healthy routes alone", async () => {
    const { proc, ownerPid } = await runAndGetOwner("healthy", upstream.port, stateDir, proxyPort);
    owners.push(ownerPid);

    const result = await nsl(["prune"], stateDir, proxyPort);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("Nothing to prune.");
    expect(isAlive(ownerPid)).toBe(true);
    expect(readRoutes(stateDir).some((r) => r.hostname === "healthy.localhost")).toBe(true);

    proc.kill("SIGTERM");
    await proc.exited;
  });
});
