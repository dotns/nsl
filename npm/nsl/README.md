# @dotns/nsl

Replace port numbers with stable, named `.localhost` URLs. For humans and agents.

Installs a platform-specific prebuilt binary via `optionalDependencies` — no
Rust toolchain or post-install download needed.

## Install

```bash
npm install -g @dotns/nsl
```

## Use

```bash
nsl start
nsl run --name myapp -- bun dev
```

See the [project README](https://github.com/dotns/nsl) for full documentation.

## Supported platforms

- Linux x64 (`@dotns/nsl-linux-x64`)
- Linux arm64 (`@dotns/nsl-linux-arm64`)
- macOS x64 (`@dotns/nsl-darwin-x64`)
- macOS arm64 (`@dotns/nsl-darwin-arm64`)
- Windows x64 (`@dotns/nsl-win32-x64`)
