# Command redesign (0.2)

The command surface is being replanned. This is the design the text CLI,
the typed client and the daemon all converge on; 0.2 is the breaking marker.

## Principles

1. **One executor.** The engine is the only place a command runs:
   `Session::exec(Command) -> Result<Outcome, Error>`. Nothing above it
   edits an arrangement directly.
2. **Commands speak the model.** Payloads are uris, `Duration`s (whole
   milliseconds on the wire), track/clip indices, bus ids — never
   client-side conveniences (parsed windows, `track@pos` sugar, display
   strings).
3. **Text is a translator, not a second implementation.** A text command
   maps its grammar onto a `Command` (or a query) and renders the typed
   reply back into canonical text. No text layer may hold execution logic.
4. **Semantics are tested at the engine.** The behaviour specs live in
   engine/exec tests; the text layer keeps thin grammar/rendering tests.
5. **Reading is a command too.** Arrangement state is reachable through
   `Get` (a tree), not by touching engine objects.

## Where execution lives

```
core::command   Command / Outcome / Reply / data   (model units, serde, ms)
engine::Session exec + lifecycle (backend choice, clock)   — the only public engine item
client::Bo      typed sugar -> Command over Connection
daemon (bin)    Session host: serve lines -> translate -> exec -> render
text CLI (bin)  same grammar -> same Command (thin)
```

## Command catalog

Text grammar on the left is the *planned* surface; existing `bo` text
grammar is being replanned onto it.

| text (planned)        | Command | Outcome | status |
|---|---|---|---|
| `put …` | `Insert { uri, from, to?, on: OnTrack }` | `Inserted { track, clip, landed }` | done |
| `play` / `pause` / `resume` / `seek t` / `stop` / `apply` | same-name commands | `Played` / `Paused{at}` / … / `Applied` | done |
| `take …`        | `Remove { track, clip: ClipHere }` | `Removed { track, clip, landed }` | done |
| `move …` | `Move { track, clip: ClipHere, to: OnTrack }` | `Moved { from_track, to_track, clip, landed }` | done |
| `route …`        | `Route { track, bus: RouteBus }` (Master/Group/New) | `Routed { track, bus, landed }` | done |
| `ls` / `at t`          | `Get { path }` | `Outcome::Tree(JSON)` | done |
| `set <path> <value>`   | `Set { path, patcher }` — state zone, deep patch | `Outcome::Set(Set{ path, patched, landed })` | done |
| ~~`probe uri`~~ | removed — measurement is upstream tooling (Python / ffmpeg); engine still measures internally for open-end inserts | |
| `render file [range]`  | `Render { file, from?, to? }` | `Rendered { file, duration_ms }` | done (range/measure/mono next) |
| ~~`check`~~ | removed — source verification is upstream tooling too | |
| `save`/`load` | `{version, history: [Command], playhead}` — daemon logs, load stages atomically | `Outcome::Snapshot/Loaded` | done |
| `check <file>`  | dry-run validation of a `.bo` snapshot | `Outcome::Checked` / `Error::Check` | done |

## Set / Get — the arrangement as a tree

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
  `Route`) only, so a `clips` key in a track patch is refused.

Paths are the tree's own (`master.volume`, `track.N`, `track.N.clips.M`,
`track.N.clips.M.gain_control.rate`, `bus.N.muted`) — shared by text, typed
client and daemon, defined once in core. A clip is addressed by its index
in the track's ordered list; its stable id rides inside the object. (Dotted
paths split on `.`, so curve keyframes whose timecodes carry decimals are
patched by object merge at the control path, never as a deeper path.)

Writable today: master volume; `track.N.{name,volume,pan,muted}`;
`bus.N.{name,volume,muted}`; `track.N.clips.M.{gain, fade_in, fade_out,
pan, pan_control, gain_control}` (pan/controls deep as above).

## Replies

Every reply is produced from the typed `Outcome`/`Error`/tree, never
hand-written twice. Shapes stay stable:

- status line `ok:` / `err:`;
- timecodes `HH:MM:SS.fff`, gains two decimals;
- one `note:` line when an edit waits for the next `apply`;
- a clip is one signature line `clip #{id} 'uri' from-to @ at` (+ non-defaults).

The old text grammar's properties (`set track.N.*`, `bo set` listing) is
replaced by the tree (`Get`/`Set`), which also gives the typed client and
Python one shared read/write vocabulary.

## Migration

Per-command fold, each step green:

1. text put/transport → `Command`; delete execution bodies; daemon serves
   through `Session`; replies rendered from `Outcome`. (done)
2. `Remove`, `Move`, `Route` commands + their text; semantics tests in
   engine. (done)
3. tree `Get`/`Set` with deep patch; `ls`/`at`/`set` text become rendering
   of the tree. (engine done; the text surface is still the old one)
4. `Probe` dropped (upstream tooling); `Render` (file + trim/measure/mono)
   done; `save`/`load`/`check` done as versioned command-log snapshots.
5. **The CLI switches to the new API** (next): cli.rs becomes a thin
   translator — parse text -> Command, go through bo::client::Bo, render
   replies from Outcome/tree. The daemon speaks only the typed wire; the
   legacy text dispatch, reply grammar and their tests are deleted.
6. Close the engine: `pub use session::Session;` only; delete
   `player_mut` and the transitional exports.

## Version policy

Breaking surface changes bump the minor (0.2.x). The text grammar is
replanned freely within 0.2; when it stabilizes, 0.3 would only carry
additive text/command changes.
