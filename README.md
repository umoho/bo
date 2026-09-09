# bo

An audio editing and mixing engine for automated agents. `bo` maintains an
arrangement — clips placed on stacked tracks, routed into buses — inside a
persistent daemon session, and exposes that session through a typed
command protocol. Clients in Rust and Python issue arrangement verbs;
the bundled command-line front end provides transport control and
snapshot restore.

[English](README.md) | [中文](README.zh-CN.md)

## Architecture

Execution is centralized. The engine is the single executor of
`Session::exec(Command)`; the data model and the command vocabulary live
in `bo-core`. All clients reach the same daemon over a Unix socket and
share one arrangement per socket.

```
engine          Session: executes commands, drives transport and backends
daemon (bin)    hosts a Session; speaks typed JSON over a Unix socket;
                records command history and performs snapshot operations
bo lib          bo::client::Bo: typed client over the daemon wire
pybo            Python binding (pyo3, managed by uv) of the Bo surface
mini CLI        bo play|pause|resume|seek|stop|load <snapshot>
```

The engine's public surface is `Session`. The daemon is the only consumer
of the engine in this repository; arrangement edits never bypass the
command path.

## Interfaces

### Clients (arrangement editing)

Arrangement verbs are issued from code, where control flow and computed
decisions belong:

- **Python** — [`pybo`](py/), a pyo3 extension built with uv. It binds the
  `Bo` surface and its value types. Times are accepted as seconds or
  lenient text and rendered as `HH:MM:SS.fff`; wire durations are whole
  milliseconds.
- **Rust** — `bo::client::Bo` in this crate.

The full verb set is `put`, `take`, `move`, `route`, `set`, `get`,
`render`, `reset`, `apply`, the transport verbs, and the snapshot trio
`save`/`load`/`check`. Structure is edited exclusively through these
verbs; the arrangement is read and patched through `get`/`set`.

### Command-line front end

The shell interface is restricted to operations that a terminal performs
well: auditioning and restoring a session.

```console
$ bo play | pause | resume | seek <t> | stop | load <snapshot.bo>
```

Exit codes: `0` success, `1` refused operation, `2` usage error.

## Session lifecycle

The daemon is spawned on demand by the first client that addresses a
socket (`$TMPDIR/bo/daemon.sock` by default; override with `--socket` or
`socket=`). It terminates and removes its socket when playback finishes,
on `stop`, or after `BO_IDLE_TIMEOUT` seconds without commands while idle
(default 600; `0` disables). Relative source paths and render targets are
resolved against the working directory of the invoking process.

## Time model

All surfaces speak one time dialect. `Timecode` accepts seconds
(`1.23`), lenient text (`SS`, `MM:SS`, `HH:MM:SS`, optional `.fff`
fraction), and formats canonically as `HH:MM:SS.fff`. Durations in
replies and in the arrangement tree are whole milliseconds.

A `put` of a whole source is probed at the session (its end is resolved
by decoding); a `trim` with a closed span does not access the source
until playback or render.

## Data model

- **Source → Clip → Track.** A clip is a `from..to` slice of a source
  positioned at a track timecode (`at`). Clips on one track do not
  overlap; tracks stack in the mix. Clip identifiers are stable per track
  and never reused.
- **Control sources.** A curve, an LFO, or a sidechain may be connected
  to a clip's pan or gain input (`track.N.clips.M.pan_control`,
  `track.N.clips.M.gain_control`). The parameter is the static base plus
  the sum of its sources, evaluated identically in live playback and
  offline render.
- **Group buses.** `route` directs a track's output into a group bus; a
  single strip (volume, mute) controls the group before the signal
  reaches the master.
- **Reading and writing the arrangement.** `get("")` returns the full
  arrangement as JSON (tracks, clips, buses, transport). `set(path, …)`
  applies a deep patch to the state zone. Structural membership is not
  patchable; it is modified through the verbs.

## Editing semantics

State edits (gain, fades, pan, mute) are applied to a running mix as they
are issued. Clips appended past the end of a track's queue join that
queue immediately. Edits a running graph cannot express — removing or
moving a clip, rerouting — are held pending and take effect at the next
`apply`, which rebuilds the graph from the current playback position.

## Snapshots

A snapshot is the session's own command history plus its playhead: a
versioned, replayable script. `save` writes it; `load` stages it on a
silent session first, so a failing snapshot leaves the current session
untouched, then commits atomically; `check` validates a snapshot file
without modifying the session.

## Repository layout

```
core/         data model and command vocabulary
engine/       Session: the single executor; nothing else is public
bo lib        client::Bo over a Connection (daemon wire)
src/cli.rs    mini command-line front end (transport and load)
src/daemon.rs daemon: typed JSON wire, host-level snapshots
py/           pybo: pyo3 binding of the Bo surface (uv + maturin)
scripts/      install.sh / uninstall.sh
```

## Installation

Prerequisites: **Rust 1.88+** for the engine, the daemon, and the CLI;
**uv** for the Python binding.

```console
$ ./scripts/install.sh      # installs the bo binary (cargo install) and pybo
$ ./scripts/uninstall.sh    # removes both
```

`bo` is installed to `~/.cargo/bin`. `pybo` is built as a single abi3
wheel (Python ≥ 3.10) and installed into `$BO_PYTHON`, the active
virtualenv, or `python3`. If the target interpreter is externally managed
(PEP 668), set `BO_PIP_BREAK=1`. The `bo` binary must be reachable on
`PATH` for `pybo` to spawn its daemon.

To build from the checkout instead:

```console
$ cargo build --release
$ cd py && uv sync
$ uv run pytest
```

`BO_BACKEND=silent` selects a deterministic headless backend for tests
and CI; without an audio device the daemon falls back to silence.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
