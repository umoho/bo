# bo

> An audio editor, mixer, and player for agents. Build tracks, place clips,
> tune the mix, then play it live or render it to a file — from a Python
> script, a Rust program, or the shell. Not a DAW yet, but this is the shape
> one would grow from.

[English](README.md) | [中文](README.zh-CN.md)

**bo** is a command-driven audio engine. You describe a session — clips
placed on stacked tracks, each clip a slice of an audio source — tune it,
and hear the result through `play` (your sound device) or `render`
(offline, to a wav file). There is no project file: the arrangement lives
in a daemon session and is written out as a **snapshot** (`save`) and
rebuilt from it (`load`).

## Two faces, one session

The engine and the data model are the spine; everything above them speaks
a typed command wire to the same daemon, so every face shares one
arrangement per socket.

* **Code is the editor.** Arrangement verbs — placing, taking, moving,
  routing, patching, rendering — live in typed clients:
  * **Python**: [`pybo`](py/) (a pyo3 extension, managed with uv). See the
    [quick start](#quick-start-python) below.
  * **Rust**: `bo::client::Bo` in this crate.
* **The shell is the transport.** The mini CLI keeps what a terminal is
  for — audition and restore:
  ```console
  $ bo play | pause | resume | seek <t> | stop | load <snapshot.bo>
  ```

The daemon is spawned on demand by whichever client first touches a
socket (`$TMPDIR/bo/daemon.sock` by default; `--socket` or `socket=...`
for another), and cleans up after itself when playback finishes, on
`stop`, or after `BO_IDLE_TIMEOUT` seconds of silence (default 600; `0`
disables). It exits and removes its socket when done.

## Quick start (Python)

```python
import pybo

bo = pybo.Bo()                                  # the shared daemon
bo.put(pybo.trim("voice.wav", "0:30-1:00"),     # a slice of a source
       pybo.Track(0).at("0:00"))                # on track 0 at the start
bo.put("bed.wav", pybo.Track(1).at("0:00"))     # a whole source (probed)
bo.route(on=1, bus="music")                     # several tracks under one bus
bo.route(on=0, bus="music")
bo.set("bus.0.volume", 0.35)                    # one knob ducks the whole bus
bo.set("track.0.volume", 0.4)
bo.get("track.1")                               # read the tree back
bo.render("mix.wav")
bo.save("show.bo")                              # a snapshot for later
```

Times are one dialect everywhere: `Timecode(1.23)` and `Timecode("1.23")`
are 1.23 seconds, `Timecode("1:02.5")` is a minute and change, and
`str(t)` is `HH:MM:SS.fff`. Durations in replies and the tree are whole
milliseconds. A `put` of a whole source is probed (its end resolved)
where the arrangement lives; a closed `trim(...)` span touches no disk
until it plays or renders. Errors are typed (`pybo.BoError`).

Restore and audition from the shell:

```console
$ bo load show.bo
ok: loaded 'show.bo'
$ bo play
ok: 2 tracks, 2 clips, ends 00:01:00.000, playing from 00:00:00.000
$ bo stop
ok: stopped
```

Exit codes: `0` ok, `1` refused, `2` usage. The arrangement verbs that
were once text commands — `put`, `ls`, `set`, `render`, `save`, … — are
now client calls; `bo help` documents the shell surface.

## The model

* **Source → Clip → Track**: a clip is a `from..to` slice of a source
  parked at a track timecode (`at`); tracks never overlap their own clips
  and stack across the mix. Ids are stable per track and never reused.
* **A source is a gesture** — a curve, an LFO or a sidechain plugs into a
  clip's pan or gain input (`track.N.clips.M.pan_control` /
  `gain_control`):
  `{"type":"curve","0":1,"3.2":-1}`, `{"type":"lfo","shape":"sine",
  "rate":1,"depth":0.5}`, `{"type":"sidechain","bus":"group.0"}` — the
  parameter rides the static base plus the source, identically live and
  rendered.
* **Group buses**: `route` several tracks into one bus; one strip
  (volume, mute) controls the group before the master hears them — a
  radio music bus or voice bus.
* **Read/write as a tree**: `get("")` returns the whole arrangement
  (tracks, clips, buses, transport) as JSON; `set("track.0", …)` deep
  patches the state zone. Structure is edited by the verbs only.

## Editing while it plays

A gain, a fade, a pan or a mute lands on the running mix as it is set; a
clip placed past a track's queue joins it. What a running graph cannot
take — a clip taken or moved, a re-route — waits (`landed: pending`) for
an `apply`, which rebuilds from where the audio really is.

## Snapshots

A snapshot (`save`) is the session's own command history plus its
playhead: a versioned, replayable script. `load` stages it atomically — a
failing snapshot leaves the session untouched — and `check` validates a
snapshot file without touching the session.

## Layout

```
core/      model units: Command/Outcome, the tree, timecode text
engine/    the only executor: Session::exec over a transport/backend
bo lib     client::Bo over a Connection (daemon wire); frozen for 0.2
src/cli.rs the mini CLI (transport + load)
src/daemon.rs  the daemon: JSON wire in, JSON wire out, host-level snapshots
py/        pybo: pyo3 binding of the Bo surface (uv + maturin)
```

## Install

Requires **Rust 1.88+** for the engine, CLI and daemon; **uv** for the
Python binding.

Install both faces for this machine:

```console
$ ./scripts/install.sh      # bo CLI/daemon (cargo install) + pybo (Python)
$ ./scripts/uninstall.sh    # the reverse
```

`bo` lands in `~/.cargo/bin`. pybo is built as one abi3 wheel (Python
≥ 3.10, any CPython) and installed into `$BO_PYTHON`, the active
virtualenv, or `python3` — add `BO_PIP_BREAK=1` when that interpreter
refuses pip operations (PEP 668). The bo binary must be on `PATH` for
pybo to spawn its daemon; an installed `bo` is enough.

Or build from the checkout directly:

```console
$ cargo build --release          # the bo CLI/daemon
$ cd py && uv sync               # pybo into .venv
$ uv run pytest                  # pybo's test suite
```

The Python host finds the daemon binary by itself (this checkout's
`target/…/bo`, then `PATH`), so a script needs no setup beyond an import.

Set `BO_BACKEND=silent` for deterministic headless sessions and CI;
without an audio device the daemon falls back to silence with a note.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
