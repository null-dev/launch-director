# Launch Director

<div align="center">
  <img src=".github/branding/icon.svg" alt="{project_name} Logo" width="200" height="200">
  
  <p>
    <strong>Rust desktop app that builds & launches locally-developed projects. Use apps you develop without needing to package/install them.</strong>
  </p>
</div>

## What It Does

> [!NOTE]
> Launch Director can only launch projects that use [mise](https://mise.jdx.dev/).

Given a project directory, Launch Director:

1. Resolves build/run `mise` tasks with this priority:
   - Build: `_launch_director_build`, then `build`
   - Run: `_launch_director_run`, then `run`
2. Starts the build task immediately. If it runs longer than 2 seconds, a build window opens and streams build output.
3. If build succeeds, Launch Director will launch the program using the run task.
4. If the program crashes instantly (within 2 seconds), a failure window will be shown.

## Requirements

- Rust toolchain (for building Launch Director)
- `mise` installed and available on `PATH`
- A target project directory with a `mise.toml` that defines:
  - Build task: `_launch_director_build` or `build`
  - Run task: `_launch_director_run` or `run`

If both variants exist, Launch Director prefers `_launch_director_*`.

## Usage

```bash
launch-director --project /path/to/project
```

Help:

```bash
launch-director --help
```

## Development

Run locally from source:

```bash
cargo run -- -p /path/to/project
```

Build:

```bash
cargo build
```
