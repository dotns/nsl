import { describe, test, expect, beforeAll, afterAll } from "bun:test";
import {
  allocPort,
  createStateDir,
  cleanupStateDir,
  nsl,
  startProxy,
  stopProxy,
  startUpstream,
  type UpstreamServer,
} from "../helpers";

interface Finding {
  level: "ok" | "warn" | "fail" | "info";
  message: string;
  hint?: string;
}

interface Report {
  findings: Finding[];
  warnings: number;
  failures: number;
}

async function doctor(stateDir: string, port: number) {
  const result = await nsl(["doctor", "--json"], stateDir, port);
  return { ...result, report: JSON.parse(result.stdout) as Report };
}

describe("nsl doctor with a running proxy", () => {
  let stateDir: string;
  let proxyPort: number;
  let upstream: UpstreamServer;

  beforeAll(async () => {
    stateDir = createStateDir();
    proxyPort = allocPort();
    upstream = startUpstream();
    await startProxy(stateDir, proxyPort);
  });

  afterAll(async () => {
    await stopProxy();
    upstream.stop();
    cleanupStateDir(stateDir);
  });

  test("healthy setup exits 0 and reports the proxy as ok", async () => {
    await nsl(["route", "healthy", String(upstream.port)], stateDir, proxyPort);

    const { exitCode, report } = await doctor(stateDir, proxyPort);
    expect(exitCode).toBe(0);
    expect(report.failures).toBe(0);
    expect(
      report.findings.some((f) => f.level === "ok" && f.message.includes(`:${proxyPort}`)),
    ).toBe(true);

    await nsl(["route", "healthy", "--remove"], stateDir, proxyPort);
  });

  test("route to a closed port is a warning", async () => {
    const closed = allocPort();
    await nsl(["route", "down", String(closed)], stateDir, proxyPort);

    const { exitCode, report } = await doctor(stateDir, proxyPort);
    expect(exitCode).toBe(0);
    expect(
      report.findings.some((f) => f.level === "warn" && f.message.includes("down.localhost")),
    ).toBe(true);

    await nsl(["route", "down", "--remove"], stateDir, proxyPort);
  });
});

describe("nsl doctor when the proxy port is taken", () => {
  let stateDir: string;
  let squatter: UpstreamServer;

  beforeAll(() => {
    stateDir = createStateDir();
    squatter = startUpstream();
  });

  afterAll(() => {
    squatter.stop();
    cleanupStateDir(stateDir);
  });

  test("reports a failure and exits 1", async () => {
    const { exitCode, report } = await doctor(stateDir, squatter.port);
    expect(exitCode).toBe(1);
    expect(report.failures).toBeGreaterThan(0);
    expect(
      report.findings.some((f) => f.level === "fail" && f.message.includes("another program")),
    ).toBe(true);
  });
});
