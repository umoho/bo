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
| `put uri[,from-to] [trk[@at]]` | `Insert { uri, from, to?, at, track }` | `Inserted { track, clip, landed }` | done |
| `play` / `pause` / `resume` / `seek t` / `stop` / `apply` | same-name commands | `Played` / `Paused{at}` / … / `Applied` | done |
| `take trk clip`        | `Remove { track, id }` | `Removed { track, clip, landed }` | new |
| `move trk clip trk2@at2` | `Move { from_track, id, to_track, at }` | `Moved { … }` | new |
| `route trk bus`        | `Route { track, bus }` (name → group id in engine) | `Routed { … }` | new |
| `set <path> <value>`   | `Set { path, value }` — state zone, patch | `Set { path, value, landed }` | new |
| `render file [range]`  | `Render { file?, range?, measure?, mono? }` | `Rendered { duration, stats? }` | new |
| `probe uri`            | `Probe { uri }` | `Probed { length, channels }` | new |
| `ls` / `at t`          | `Get { path }` | tree JSON | new |
| `check`                | revisit: `Check` over sources | — | design |
| `save`/`load`          | snapshot: `Get` whole tree ↔ `Set`/structure replay | — | design |

## Set / Get — the arrangement as a tree

Reading is one `Get(path)` returning JSON: the whole tree (`""`), a
subtree, or a leaf. Durations are canonical timecode text inside tree
values; ids are stable and present on every clip.

```
master        { volume }
transport     { state, playhead, duration }        # read-only
track.N       { name, volume, pan, muted, clips: [ {id, uri, from, to,
                                                      at, gain, fades, …} ] }
bus.N         { name, volume, muted }
```

`Set` writes only the **state zone** (strips, clip params, names):

- leaf path + scalar: validated/clamped, lands via the transport's live/pending
  semantics;
- interior state node + object: **patch** — missing keys untouched, unknown
  keys refused, each key lands independently;
- structure (`track[]`, `track.N.clips`, `bus[]` membership) has **no set
  entry**: it is edited by verbs (`Insert`, `Remove`, `Move`, `Route`) only.

Keys/paths are a small grammar (`master`, `track.N.volume`, `clip.T.C.gain_control`,
`bus.N.muted` …) shared by text, typed client and daemon — defined once in
core, rendered by the text layer.

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

1. text put/transport → `Command`; delete `put_command`/`play_command`/…
   execution bodies; daemon serves through `Session`; replies rendered from
   `Outcome`. Semantics already covered by engine/exec tests.
2. `Remove`, `Move`, `Route` commands + their text; semantics tests in
   engine, thin grammar tests in text.
3. tree `Get`/`Set` + state-zone rules; `ls`/`at`/`set` text become
   rendering of the tree.
4. `Render`, `Probe`; then revisit `check`/`save`/`load` as snapshot
   round-trips of the tree.
5. Close the engine: `pub use session::Session;` only; delete
   `player_mut` and the transitional exports.

## Version policy

Breaking surface changes bump the minor (0.2.x). The text grammar is
replanned freely within 0.2; when it stabilizes, 0.3 would only carry
additive text/command changes.
