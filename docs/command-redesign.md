# Command design (0.2)

The command surface, as it stands: what executes where, what each verb
does, and which surfaces speak it. The breaking redesign happened in 0.2;
this file is the contract the crates converge on.

## Principles

1. **One executor.** The engine is the only place a command runs:
   `engine::Session::exec(Command) -> Result<Outcome, Error>`. Nothing
   above it edits an arrangement directly.
2. **Commands speak the model.** Payloads are uris, `Duration`s (whole
   milliseconds on the wire), track/clip indices, bus ids — never
   client-side conveniences (parsed windows, `track@pos` sugar, display
   strings).
3. **Clients are translators, not second implementations.** A client maps
   its own ergonomics onto a `Command` (or a read) and renders the typed
   reply back into its own shape. No client holds execution logic.
4. **Semantics are tested at the engine.** The behaviour specs live in
   engine/exec tests; clients keep thin grammar/rendering tests.
5. **Reading is a command too.** Arrangement state is reachable through
   `Get` (a tree), not by touching engine objects.
6. **The engine and the `bo` lib are frozen.** 0.2.x surface work happens
   in clients and the binary face, not in `bo-core`/`bo-engine`/`bo` lib.

## Where execution lives

```
core::command   Command / Outcome / Reply / data   (model units, serde, ms)
engine::Session exec + lifecycle (backend choice, clock)     — the only executor
bo lib          client::Bo: typed sugar -> Command over a Connection (daemon wire)
daemon (bin)    Session host: JSON lines in -> exec -> JSON lines out; host-level
                snapshot ops (history lives here)
pybo            pyo3 binding of the bo-lib Bo surface, for Python (uv/maturin)
mini CLI (bin)  play pause resume seek stop load — thin translator over the
                same wire, for audition and restore from a shell
```

The daemon speaks only the typed JSON wire: one
[`Command`](bo_core::command::Command) per line, preceded by the caller's
working directory (relative paths resolve against the caller, never the
daemon's), answered by one JSON [`Reply`](bo_core::command::Reply). It is
spawned on demand by the clients and the CLI, and cleans up its socket
when playback finishes, on `stop`, or after an idle timeout.

## The surfaces

### Mini CLI — `bo play pause resume seek stop load`

The shell face is deliberately small: arrangement editing is code, not
text. The CLI keeps the transport verbs and restore-from-snapshot, so a
finished session can be auditioned or rebuilt from a terminal:

| text | Command | Outcome |
|---|---|---|
| `play` | `Play` | `Played { tracks, clips, end, playhead }` |
| `pause` | `Pause` | `Paused { at }` |
| `resume` | `Resume` | `Resumed { at }` |
| `seek <t>` | `Seek { at }` | `Seeked { at }` |
| `stop` | `Stop` | `Stopped` |
| `load <file>` | `Load { snapshot }` | `Loaded` |

(`daemon --socket PATH` is the hidden subcommand clients spawn.) A
`load` restores the snapshot the clients write with `save`.

### Clients — the arrangement verbs

Everything that builds or tunes an arrangement lives in a typed client,
because that is where control flow and computed decisions belong:

| verb | Command | Outcome |
|---|---|---|
| `put` | `Insert { uri, from, to?, on }` | `Inserted { track, clip, landed }` |
| `take` | `Remove { track, clip: ClipHere }` | `Removed { … }` |
| `move` | `Move { track, clip, to }` | `Moved { … }` |
| `route` | `Route { track, bus: RouteBus }` | `Routed { … }` |
| `get` | `Get { path }` | `Outcome::Tree(JSON)` |
| `set` | `Set { path, patcher }` | `Set { path, patched, landed }` |
| `render` | `Render { file, from?, to?, measure, mono }` | `Rendered { … }` |
| `reset` | `Reset` | `Outcome::Reset` |
| `apply` | `Apply` | `Applied` |
| transport | `Play`/`Pause`/…/`Seek`/`Stop` | same outcomes as the CLI |
| `save`/`load`/`check` | `Snapshot`/`Load`/`Check` | snapshot trio (host-level) |

Two clients today, sharing one vocabulary:

* **Rust** — `bo::client::Bo`: `put(Clip, Destination)`, `take(ClipOnTrack)`,
  `route(TrackIndex, BusIndex)`, `set(path, Value)`, `get(path)`, …
  ([`bo::client`](bo::client)).
* **Python** — `pybo` (a pyo3 extension built by uv/maturin, in `py/`),
  binding only the `Bo` struct and its value types: `Timecode`, `trim`,
  `Track(n).at(t)`, the same verbs. Times accept seconds (`1.23`) or
  lenient text (`"1:02.5"`) and format as `HH:MM:SS.fff`; durations in
  replies and the tree are whole milliseconds. Both speak to the same
  daemon, so Python, Rust and the CLI share one arrangement on a socket.

Measurement (`probe`, source `check`) is upstream tooling — Python /
ffmpeg — not a command.

## Get / Set — the arrangement as a tree

Reading is one `Get(path)` returning JSON: the whole tree (`""`), a
subtree, or a leaf. Durations are whole milliseconds; ids are stable and
present on every clip.

```
master        { volume }
transport     { state, playhead, duration }        # read-only
track.N       { name, volume, pan, muted, clips: [ {id, uri, from, to,
                                                      at, gain, fades, …} ] }
bus.N         { name, volume, muted }
```

`Set` writes only the **state zone** (strips, clip params, controls,
names) — the tree is there precisely so the patch can go **deep**:

- leaf path + scalar: validated/clamped, lands via the transport's
  live/pending semantics;
- interior state node + object: **merge** — missing keys untouched; each key
  lands independently;
- deep paths into a clip's control source (`…clips.C.gain_control.rate`)
  merge into the current source and rewrite it typed and validated; an
  object at the control path adds/merges into the source already there
  (curve keyframes), `null` clears the input;
- structure (`track[]`, `bus[]` membership) and clip membership have **no
  set entry**: they are edited by verbs (`Insert`, `Remove`, `Move`,
  `Route`, `Reset`) only, so a `clips` key in a track patch is refused.

Paths are the tree's own (`master.volume`, `track.N`,
`track.N.clips.M`, `track.N.clips.M.gain_control.rate`, `bus.N.muted`) —
shared by text, typed client and Python, defined once in core. A clip is
addressed by its index in the track's ordered list; its stable id rides
inside the object. (Dotted paths split on `.`, so curve keyframes whose
timecodes carry decimals are patched by object merge at the control path,
never as a deeper path.)

Writable today: master volume; `track.N.{name,volume,pan,muted}`;
`bus.N.{name,volume,muted}`; `track.N.clips.M.{gain, fade_in, fade_out,
pan, pan_control, gain_control}` (pan/controls deep as above).

## Snapshots

A snapshot is `{ version, history: [Command], playhead }`. The daemon logs
every mutating command it executes, so `save` is the session's own
history; `load` stages the history on a silent arrangement first (a
failing script leaves the live session untouched) and only then commits;
`check` validates a snapshot file without touching the session. Hosts
intercept `Snapshot`/`Load`/`Check` — the engine refuses them
(`Error::Host`).

## Mini-CLI reply conventions

Replies are rendered from the typed `Outcome`/`Error`, never hand-written
twice:

- status line `ok:` / `err:`;
- timecodes `HH:MM:SS.fff`;
- exit codes: `0` ok, `1` refused, `2` usage.

The CLI writes no arrangement state itself; a `play` on an empty
arrangement is refused rather than silently finishing.

## Version policy

Breaking surface changes bump the minor (0.2.x). The engine and the `bo`
lib are frozen for the rest of 0.2; clients and the binary face are
replanned freely. The snapshot format is versioned (`SNAPSHOT_VERSION`),
so future formats can change shape without guessing.
