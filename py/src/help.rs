//! pybo's built-in help: `pybo.help(query)` returns one usage page.
//!
//! The agent-facing documentation lives here, as a table of topics; each
//! page is tutorial prose — synopsis, semantics, a copy-pasteable example,
//! common mistakes, related pages — kept deliberately separate from the
//! signatures themselves (which live on the Rust items). `help()` with no
//! query returns the overview plus an index; a query is matched by alias,
//! then by substring scoring, and an ambiguous query lists the candidates.

use pyo3::prelude::*;

/// One help page.
pub(crate) struct Topic {
    /// The canonical name (`pybo.help("put")`).
    pub key: &'static str,
    /// Other ways to ask for it.
    pub aliases: &'static [&'static str],
    /// One line for the overview index.
    pub one_line: &'static str,
    /// The page body.
    pub body: &'static str,
}

/// Everything `pybo.help` can answer. Order is the index order.
pub(crate) static TOPICS: &[Topic] = &[
    Topic {
        key: "overview",
        aliases: &["intro", "what", "start", "帮助", "总览"],
        one_line: "what pybo is, and the index of every help page.",
        body: r#"topic: overview

pybo is the Python face of bo, an audio editing and mixing engine for
agents. The arrangement lives in a daemon process; pybo edits it over a
Unix socket. The engine is the only executor — pybo is a thin client.

Working model
  - one Bo = one socket = one shared arrangement (clients on the same
    socket see each other's edits)
  - Bo() uses the shared default socket; Bo(socket=...) isolates a session
  - the daemon spawns on demand; 'stop' ends it (see topic: daemon)
  - times: input accepts seconds or text like '1:02.5'; replies and the
    tree carry whole milliseconds (see topic: timecode)
  - structure is edited by verbs (put/take/move/route/reset); state is
    read and patched through get/set (see topic: get, set)

Index (help("<key>"))
  overview    what pybo is, and the index of every help page
  recipe      one runnable end-to-end session, copy and adapt
  daemon      the session process: sockets, lifecycle, cleanup
  timecode    the one time dialect (seconds/text in, HH:MM:SS.fff out)
  trim        slice a source into clip material
  track       where a clip lands: Track(n), .at(t), Track.fresh()
  put         place a clip
  take        remove a clip (by id or by the time it covers)
  move        move a clip between tracks or positions
  route       route a track's output into a bus
  get         read the arrangement as a tree (JSON dict)
  set         deep-patch the state zone (leaf or object merge)
  apply       land edits a running mix could not take
  render      mix the arrangement to a wav file
  transport   play/pause/resume/seek/stop and their replies
  snapshot    save/load/check the session as a versioned script
  reset       clear the session
  replies     what verb replies look like (ms, landed)
  errors      BoError vs ValueError vs TypeError
  cli         the shell front end: bo play|pause|resume|seek|stop|load
"#,
    },
    Topic {
        key: "recipe",
        aliases: &["example", "quickstart", "demo", "配方", "示例"],
        one_line: "one runnable end-to-end session, copy and adapt.",
        body: r#"topic: recipe

A complete session: two tracks, one group bus, a patch, a render, a
snapshot; then audition from the shell and stop.

  import pybo
  bo = pybo.Bo()                            # the shared daemon
  bo.put("voice.wav", pybo.Track(0).at("0:00"))
  bo.put(pybo.trim("bed.wav", "0:00-2:00"), pybo.Track(1).at("0:00"))
  bo.route(on=1, bus="music")               # group bus under the master
  bo.route(on=0, bus="music")
  bo.set("track.1.volume", 0.35)            # duck the bed
  bo.set("bus.0.volume", 0.8)
  bo.get("")                                # read the whole tree
  bo.render("mix.wav")
  bo.save("show.bo")                        # for a later session
  bo.stop()

Restore and audition from the shell (the CLI has no arrangement verbs):

  bo load show.bo
  bo play
  bo stop

Notes
  - the two whole-source puts ("voice.wav", "bed.wav") probe the files at
    the session — they must exist. Closed trim(...) spans never touch disk
    until play/render.
  - render needs the source files to exist too.
  - keep the sources until the session is done; deleting a file a clip or
    a queue still references is what breaks playback.
"#,
    },
    Topic {
        key: "daemon",
        aliases: &["socket", "process", "session", "shared", "后台", "守护"],
        one_line: "the session process: sockets, lifecycle, cleanup.",
        body: r#"topic: daemon

The arrangement lives in a daemon, not in your process. pybo is a client:
every verb is a typed command over a Unix socket to the daemon, which runs
the engine.

  - Bo() speaks to $TMPDIR/bo/daemon.sock; Bo(socket="/path/x.sock")
    isolates a session. Everything on one socket shares one arrangement.
  - the daemon is spawned on demand by the first request and found as
    follows: $BO_DAEMON, the installed 'bo' on PATH, or a checkout's
    target/debug/bo. An installed bo binary is enough.
  - it exits and removes its socket when playback finishes, on 'stop',
    or after BO_IDLE_TIMEOUT seconds idle (default 600, 0 disables).
  - BO_BACKEND=silent forces a headless backend (tests, CI, no device).

Notes
  - a Unix socket path is ~104 bytes long; when a test creates its own
    socket, keep the directory short (pytest's long tmp_path names can
    overflow it and the daemon's bind fails).
  - relative uris and render paths resolve against the caller's cwd, not
    the daemon's.
"#,
    },
    Topic {
        key: "timecode",
        aliases: &["time", "duration", "ms", "seconds", "时间"],
        one_line: "the one time dialect (seconds/text in, HH:MM:SS.fff out).",
        body: r#"topic: timecode

One dialect everywhere. A time is accepted as seconds or lenient text and
formats canonically as HH:MM:SS.fff; on the wire and in every reply dict
and tree it is whole milliseconds.

  pybo.Timecode("1.23").ms       # 1230   ("1.23" = 1.23 seconds)
  pybo.Timecode(90).ms           # 90000  (an int is seconds too)
  str(pybo.Timecode("1:02.5"))   # '00:01:02.500'
  pybo.Timecode.from_ms(1230)    # ms -> Timecode (format tree values)

Every verb that takes a moment also accepts seconds floats and lenient
text directly:
  Track(0).at("0:30")  bo.seek("0:05")  trim(..., start=0.5, end="1:00")

Notes
  - text forms: SS, MM:SS, HH:MM:SS, optional .fff fraction; a bare field
    is seconds ("3.2", "0.005").
  - the tree and replies carry ms ints, not text: read
    tree["track"][0]["clips"][0]["at"] and format with
    pybo.Timecode.from_ms(...).
"#,
    },
    Topic {
        key: "trim",
        aliases: &["slice", "window", "material", "range", "区间", "切片"],
        one_line: "slice a source into clip material.",
        body: r#"topic: trim

trim(uri, ...) builds the material a put places: a from..to window of the
source. The whole source is the default; a range text or start/end moments
select a window.

  pybo.trim("voice.wav")                # whole source (open end -> probed)
  pybo.trim("voice.wav", "0:30-1:00")   # closed span
  pybo.trim("voice.wav", "0:30-")       # to the source's end
  pybo.trim("voice.wav", start="0:30", end=90)

Semantics
  - a closed span touches no disk until the clip plays or renders; an
    open end (whole source, 'from-') is probed at the session, so the
    file must exist.
  - put also accepts a bare uri string (the whole source).
  - the returned value reads uri/from_ms/to_ms; pass it straight to put.

Mistakes
  - trim(uri, range, start=...) — a range and start/end are mutually
    exclusive (ValueError).
  - an 'end' before 'start' is refused (ValueError).
"#,
    },
    Topic {
        key: "track",
        aliases: &["placement", "destination", "where", "on", "轨道"],
        one_line: "where a clip lands: Track(n), .at(t), Track.fresh().",
        body: r#"topic: track

Track describes where a put (or a move) lands: an existing track — created
on demand up to its index — or a fresh one.

  pybo.Track(0)                 # track 0 at the playhead
  pybo.Track(2).at("1:00")      # track 2 at 1:00
  pybo.Track.fresh()            # a new track at the playhead
  pybo.Track.fresh(at="0:05")   # a new track at 0:05

Omitting the destination entirely on put means a fresh track at the
playhead:
  bo.put("voice.wav")           # == bo.put(..., pybo.Track.fresh())

Mistakes
  - bo.put(x, 0) is a TypeError: destinations are Track objects, not bare
    indices.
  - Track(0) alone means "at the playhead"; to be explicit about the
    moment use Track(0).at("0:00").
"#,
    },
    Topic {
        key: "put",
        aliases: &["place", "insert", "放", "素材"],
        one_line: "place a clip.",
        body: r#"topic: put

put(material, dest=None) places one clip and returns
{track, clip: {id, uri, at, from, to, gain}, landed}.

  bo.put("voice.wav", pybo.Track(0).at("0:00"))          # whole source
  bo.put(pybo.trim("voice.wav", "0:30-1:00"),
         pybo.Track(0).at("1:00"))                       # a slice

Semantics
  - material: a uri string (whole source, probed at the session) or a
    trim(...) slice; dest: a Track placement (see topic: track).
  - clip.id is the stable per-track id — keep it to take/move the clip.
  - landed is "live" (a running mix took it) or "pending" (see apply).
  - the track is grown to fit an explicit index.

Mistakes
  - a whole-source put of a file that cannot be measured is refused
    (BoError, "cannot measure ..."). Give it a closed trim(...), or make
    sure the file exists.
  - overlapping the same track is refused (BoError); half-open spans may
    butt-join (one ends exactly where the next starts).
"#,
    },
    Topic {
        key: "take",
        aliases: &["remove", "delete", "删", "取走"],
        one_line: "remove a clip (by id or by the time it covers).",
        body: r#"topic: take

take(on, clip_id=None, at=None) removes one clip from a track. Address it
by its stable clip_id (what put returned) or by the moment it covers —
exactly one of the two. Returns {track, clip, landed}.

  r = bo.put("voice.wav", pybo.Track(0).at("0:00"))
  bo.take(on=0, clip_id=r["clip"]["id"])
  bo.take(on=0, at="0:00.500")        # the clip covering that moment

Semantics
  - ids are stable and never reused while the clip lives; removing one
    clip does not renumber the others.
  - landed "pending" when a running mix cannot take the edit — it lands at
    the next apply.
  - removing clips is structure: use the verbs, never set().

Mistakes
  - give clip_id or at, not both, and not neither (ValueError).
  - a time nothing covers is a BoError ("no clip").
"#,
    },
    Topic {
        key: "move",
        aliases: &["reposition", "relocate", "移"],
        one_line: "move a clip between tracks or positions.",
        body: r#"topic: move

move(on, to, clip_id=None, at=None) moves one clip — addressed by its
stable clip_id or the at it covers — to a destination Track placement.
Returns {from_track, to_track, clip, landed}.

  bo.move(on=0, clip_id=3, to=pybo.Track(1).at("0:00"))
  bo.move(on=0, at="0:12", to=pybo.Track(0).at("1:00"))

Semantics
  - the clip keeps its content (gain, fades); its id survives when the
    destination does not already carry one.
  - atomic: a move into an occupied span is refused whole (BoError) and
    leaves both tracks untouched.
  - a destination Track index is created on demand.

Mistakes
  - `to` is required (ValueError if missing).
  - address by clip_id or at — one of the two, not both.
"#,
    },
    Topic {
        key: "route",
        aliases: &["bus", "group", "groupbus", "路由", "母线"],
        one_line: "route a track's output into a bus.",
        body: r#"topic: route

route(on, bus) directs a track's output into a group bus — several tracks
share one strip (volume, mute) before the master hears them. bus is
"master" (back out), a group bus id (int), or a name.

  bo.route(on=1, bus="music")      # first mention creates bus 'music'
  bo.route(on=0, bus="music")      # a matching name joins that bus
  bo.route(on=0, bus=0)            # or join by its id
  bo.route(on=0, bus="master")     # route back out (the bus stays)

Semantics
  - routing is structure: it lands at the next apply when something is
    playing.
  - bus names are unique; 'master' is reserved for the master bus.
  - read the bus table with get("bus"): {name, volume, muted, members}.
  - a group strip is patched like any state: set("bus.0.volume", 0.4).

Mistakes
  - routing a track that does not exist is a BoError.
"#,
    },
    Topic {
        key: "get",
        aliases: &["read", "tree", "ls", "status", "读", "树"],
        one_line: "read the arrangement as a tree (JSON dict).",
        body: r#"topic: get

get(path="") reads the arrangement as a dict — the whole tree, a subtree,
or a leaf. Durations are whole milliseconds; clip ids are stable.

  tree = bo.get("")
  # { master: {volume}, transport: {state, playhead, duration},
  #   track: [ {name, volume, pan, muted, clips: [...]} ],
  #   bus:  [ {name, volume, muted, members} ] }
  # clip node: {id, uri, from, to, at, gain, fade_in, fade_out, pan,
  #             pan_control, gain_control}

  bo.get("track.0")                 # one track's node
  bo.get("track.0.clips.0")         # one clip (its index, not its id)
  bo.get("transport.state")         # 'stopped' | 'playing' | 'paused'

Semantics
  - reading is a command; nothing is cached client-side.
  - a clip is addressed by its index in the track's ordered list; the
    stable id rides inside the node.

Mistakes
  - a path that leads nowhere is a BoError.
"#,
    },
    Topic {
        key: "set",
        aliases: &["patch", "write", "statezone", "改", "设置"],
        one_line: "deep-patch the state zone (leaf or object merge).",
        body: r#"topic: set

set(path, value) patches the state zone — strips, clip parameters,
controls, names. A leaf takes a scalar; an interior node takes an object
that merges (missing keys untouched).

  bo.set("master.volume", 0.9)
  bo.set("track.0.volume", 0.4)
  bo.set("track.0", {"muted": True, "name": "bed"})     # merge
  bo.set("track.0.clips.0.gain", 0.7)
  bo.set("bus.0.volume", 0.5)

Writable today
  master.volume
  track.N.{name, volume, pan, muted}
  bus.N.{name, volume, muted}
  track.N.clips.M.{gain, fade_in, fade_out, pan,
                   pan_control, gain_control}

Semantics
  - the clip index M is the track's ordered list position (see get); the
    stable id is inside the node.
  - fade_in/fade_out are whole milliseconds on the wire: set(..., 500),
    not a text form.
  - a control input is replaced by a source object (curve/lfo/sidechain)
    or cleared by None; set("track.0.clips.0.pan_control", None).
  - structure is NOT writable here: track[]/bus[] membership and clip
    membership belong to the verbs (put/take/move/route/reset). A "clips"
    key in a track patch is refused.
  - returns {path, patched, landed}.

Mistakes
  - a leaf expects one typed value; handing it an object (or vice versa)
    is refused.
  - fading in "1:00" text? No — durations are ms ints in the patch.
"#,
    },
    Topic {
        key: "apply",
        aliases: &["land", "pending", "commit", "生效"],
        one_line: "land edits a running mix could not take.",
        body: r#"topic: apply

Edits land as they are made: a gain, a pan or a mute goes into the running
mix immediately, and a clip placed past a track's queue joins it. What a
running graph cannot express — a clip taken or moved, a re-route, a bus
strip change — waits: the verb's reply says landed "pending". apply makes
them audible, rebuilding the graph from where the audio really is.

  bo.apply()     # {"kind": "rebuilt" | "live" | "nothing pending" | ...}

Semantics
  - with nothing playing, pending edits land at the next play; apply
    answers "not playing" and changes nothing.
  - a structural edit supersedes the small edits queued behind it — one
    rebuild takes all of it.

Notes
  - you only need apply when something is already playing and you made a
    structural edit. Stopped sessions land everything at play.
"#,
    },
    Topic {
        key: "render",
        aliases: &["mix", "export", "wav", "out", "渲染", "导出"],
        one_line: "mix the arrangement to a wav file.",
        body: r#"topic: render

render(file, trim=None, measure=False, mono=False) mixes the arrangement
to a wav file, offline. Returns {file, duration_ms, stats}.

  bo.render("mix.wav")
  bo.render("mix.wav", trim="0:30-1:00")     # only a span of the mix
  bo.render("mix.wav", measure=True)         # also levels (stats)
  bo.render("mono.wav", mono=True)           # (L+R)/2 fold

Semantics
  - a trim range is of the arrangement: 'from-to' or 'from-'.
  - measured stats describe the exact stream written: peak/true peak/RMS
    and, for spans of 3s+, EBU R128 loudness.
  - the mix is built exactly like live playback — in-points, fades,
    control sources, buses included.
  - the file is written by the engine (the daemon); relative paths
    resolve against your cwd.

Mistakes
  - sources must exist at render time: the file is decoded then.
"#,
    },
    Topic {
        key: "transport",
        aliases: &["play", "pause", "resume", "seek", "stop", "播放", "传输"],
        one_line: "play/pause/resume/seek/stop and their replies.",
        body: r#"topic: transport

The transport verbs move playback. A running playhead is real time at the
daemon; the silent backend (BO_BACKEND=silent) has no clock, and a host
advances it by wall time.

  bo.play()       # -> {tracks, clips, end, playhead}; refused when empty
  bo.pause()      # -> {at}
  bo.resume()     # -> {at}
  bo.seek("0:05") # move the playhead; a running transport re-plans
  bo.stop()       # stop, rewind, and end the daemon session

Semantics
  - seek while playing re-plans the graph from the new position and takes
    pending edits with it.
  - stop ends the daemon session: the daemon removes its socket and
    exits. Every further verb spawns a fresh session — reload a snapshot
    (see topic: snapshot).
  - pause/resume keep the position.

Mistakes
  - play on an empty arrangement is refused ("no clips").
  - in a test, give play a long arrangement: a short one finishes and the
    daemon exits under you.
"#,
    },
    Topic {
        key: "snapshot",
        aliases: &["save", "load", "check", "persist", "restore", "快照", "存档"],
        one_line: "save/load/check the session as a versioned script.",
        body: r#"topic: snapshot

A snapshot is the session's own command history plus its playhead: a
versioned, replayable script (.bo file, JSON).

  bo.save("show.bo")     # -> {version, playhead, commands}
  bo.load("show.bo")     # replace the session, atomically
  bo.check("show.bo")    # validate a file without touching the session

Semantics
  - load stages the history on a silent session first: a failing snapshot
    leaves the current session untouched.
  - only arrangement commands are recorded — transport, queries and
    renders are not.
  - reset clears the session and its history.
  - versions this build does not read are refused.

Notes
  - this is how a session survives its daemon and how you hand an
    arrangement to the shell front end: save from pybo, then
    'bo load show.bo' from a terminal (see topic: cli).
"#,
    },
    Topic {
        key: "reset",
        aliases: &["clear", "wipe", "fresh", "清空"],
        one_line: "clear the session.",
        body: r#"topic: reset

reset() drops every track and group bus and stops the transport: the
session is back to its fresh state (and its command history is forgotten,
so a reset is not replayed into a snapshot).

  bo.reset()

Semantics
  - transport resets with the swap: state stopped, playhead zero.
  - the audio backend survives a reset.
"#,
    },
    Topic {
        key: "replies",
        aliases: &["dict", "shape", "return", "landed", "返回值"],
        one_line: "what verb replies look like (ms, landed).",
        body: r#"topic: replies

Verbs return plain dicts. Times inside are whole milliseconds; landed says
how an edit reached the sound.

  put/take/move:  {track|from_track|to_track, clip, landed}
                  clip: {id, uri, at, from, to, gain}   (ms ints)
  route:          {track, bus: "master" | {"group": id}, landed}
  set:            {path, patched, landed}
  get:            the tree/subtree/leaf itself (see topic: get)
  play:           {tracks, clips, end, playhead}
  pause/resume:   {at}
  apply:          "nothing pending" | {"kind": "live", ...} | ...
  save:           {version, playhead, commands}

landed
  "live"    a running mix took the edit as it was made
  "pending" nothing is playing, or the graph cannot take it — it lands at
            the next apply/play (see topic: apply)

Format ms as time text:
  str(pybo.Timecode.from_ms(tree["track"][0]["clips"][0]["at"]))
"#,
    },
    Topic {
        key: "errors",
        aliases: &["exception", "raise", "error", "异常"],
        one_line: "BoError vs ValueError vs TypeError.",
        body: r#"topic: errors

  BoError     a refused operation: the daemon or the engine said no. The
              message is the reason (an overlap names the conflicting
              clip and the next free start; a whole-source put that cannot
              be measured says so). Covers transport trouble too.
  ValueError  an argument was wrong: a bad timecode, a range with 'to'
              before 'start', trim with both a range and start/end.
  TypeError   a value had the wrong kind: put(x, 0), route(on, 3.5).

Semantics
  - a refused verb leaves the session exactly as it was (engine edits are
    atomic).
  - check returns cleanly; BoError only when it finds problems.
"#,
    },
    Topic {
        key: "cli",
        aliases: &["shell", "terminal", "command", "命令行"],
        one_line: "the shell front end: bo play|pause|resume|seek|stop|load.",
        body: r#"topic: cli

The shell front end is deliberately small — arrangement editing is code,
not text. The CLI keeps what a terminal is for: auditioning and restore.

  bo load show.bo          # restore a snapshot saved by pybo
  bo play                  # -> ok: N tracks, N clips, ends ..., playing from ...
  bo pause | resume | seek <t> | stop

Semantics
  - every CLI verb is the same typed wire pybo uses; they share a daemon
    session on one socket.
  - stop ends the daemon session and removes its socket.
  - exit codes: 0 ok, 1 refused, 2 usage.
  - 'bo daemon' is the hidden subcommand clients spawn; 'bo --socket
    PATH' points at another socket.

Mistakes
  - bo put / bo set / bo ls are gone. Use pybo (this module) for
    arrangement verbs; 'bo help' only shows the six CLI verbs.
"#,
    },
];

/// The overview: architecture + index (help with no query, or an unknown
/// one, points here).
fn overview() -> String {
    let topic = &TOPICS[0];
    topic.body.to_string()
}

/// An exact page by key (keys double as aliases).
fn page(key: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|t| t.key == key)
}

/// Stopwords a help query carries without meaning (fuzzy queries are
/// prose, not code).
fn stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an" | "the" | "to" | "on" | "at" | "in" | "of" | "for" | "with" | "and" | "or"
            | "from" | "how" | "what" | "do" | "does" | "is" | "are" | "i" | "my" | "me"
            | "can" | "could" | "want" | "please" | "tell" | "it" | "this" | "that" | "where"
    )
}

/// The words of a query: lowercase, ASCII punctuation dropped (so "put",
/// "place a clip" and "放素材" all reduce to comparable tokens).
fn words(query: &str) -> Vec<String> {
    query
        .chars()
        .map(|c| {
            if c.is_ascii_punctuation() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|w| !stopword(w) && !w.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

enum Match {
    Page(&'static Topic),
    Candidates(Vec<&'static Topic>),
    None,
}

/// Match a query: an exact alias (or key) token wins strongly; otherwise
/// tokens score by containing an alias (or being contained in one). A tie
/// lists the candidates.
fn match_query(q: &str) -> Match {
    let tokens = words(q);
    if tokens.is_empty() {
        if let Some(topic) = TOPICS.iter().find(|t| t.key == q || t.aliases.contains(&q)) {
            return Match::Page(topic);
        }
        return Match::None;
    }
    let mut best: Vec<&Topic> = Vec::new();
    let mut best_score = 0usize;
    for topic in TOPICS {
        let mut score = 0usize;
        for token in &tokens {
            let exact = topic.aliases.iter().any(|a| a == token) || topic.key == token;
            if exact {
                score += 4;
                continue;
            }
            for alias in topic.aliases {
                if alias.len() >= 3 && (token.contains(alias) || alias.contains(token)) {
                    score += 1;
                    break;
                }
            }
        }
        match score.cmp(&best_score) {
            std::cmp::Ordering::Greater => {
                best.clear();
                best.push(topic);
                best_score = score;
            }
            std::cmp::Ordering::Equal if score > 0 => best.push(topic),
            _ => {}
        }
    }
    match best.as_slice() {
        [] => Match::None,
        [topic] => Match::Page(topic),
        candidates => Match::Candidates(candidates.to_vec()),
    }
}

fn index_listing() -> String {
    TOPICS[1..]
        .iter()
        .map(|t| format!("  {:<10} {}", t.key, t.one_line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `pybo.help(query=None)` — one usage page.
///
/// With no query, or an empty one, returns the overview (architecture and
/// the index of every page). A query is matched by alias first, then by
/// substring scoring; an ambiguous query lists its candidates; an unknown
/// one returns the overview with the index.
#[pyfunction]
#[pyo3(signature = (query=None))]
pub(crate) fn help(query: Option<&str>) -> String {
    let Some(query) = query else {
        return overview();
    };
    if query.trim().is_empty() {
        return overview();
    }
    match match_query(query) {
        Match::Page(topic) => format!("{}\n", topic.body.trim_end()),
        Match::Candidates(candidates) => {
            let list = candidates
                .iter()
                .map(|t| format!("  {:<10} {}", t.key, t.one_line))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "'{query}' matches several topics — pick one:\n\n{list}\n\npybo.help('<key>') opens it."
            )
        }
        Match::None => {
            let list = index_listing();
            format!(
                "no help topic for '{query}'.\n\nWhat pybo can tell you about:\n\n{list}\n\npybo.help('<key>') opens a page."
            )
        }
    }
}
