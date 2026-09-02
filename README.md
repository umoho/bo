# bo

> An audio editor, mixer, and player for agents. One command at a time — build tracks, place clips, tune the mix, then play it live or render it to a file. Not a DAW yet, but this is the shape one would grow from.

[English](README.md) | [中文](README.zh-CN.md)

**bo** is a command-driven audio editor, mixer, and player. You describe a session — clips placed on stacked tracks, each clip a slice of an audio source — with `put` commands, tune it with `set`, and hear the result with `play` (through your sound device) or `render` (offline, to a wav file). There is no project file: the session lives in a long-running daemon, reached over a Unix socket, and can be written out as a script (`save`) and rebuilt from it (`load`).

Each `bo ...` invocation is one action: place a clip, remove one, move the playhead, duck a track, start or stop playback. The daemon is spawned on demand and cleans up after itself, so a session is a conversation, not a project.

## What bo is — and isn't

**bo** is an editing, mixing, and playback tool for agents: slicing sources, placing clips on a timeline, stacking tracks, adjusting gain and mute — auditioning a section while you work, playing a finished arrangement out in full, or rendering it to a file.

What it is **not** — yet — is a DAW: no pan, no effects, no fades or automation, no project files. But the data model underneath — `Source` → `Clip` → `Track`, a timeline, a transport — is exactly the spine a CLI DAW is built on. The roadmap is to grow DAW operations onto that spine, not to replace it.

## Features

- **Arrangement as data** — tracks and clips live in the daemon's memory, not in files. `save`/`load` serialize them as the very commands that built them.
- **Built for agents** — one command per invocation, machine-readable replies, stable exit codes: 2 for misuse, 1 for a refused operation.
- **Play it live, or render it offline** — audition from the playhead mid-edit, play a finished arrangement out end to end (rodio), or mix the whole arrangement — or just a range — to a wav file.
- **Silent fallback** — with no audio device the daemon still runs; set `BO_BACKEND=silent` for deterministic, headless tests and CI.
- **Self-cleaning** — the daemon exits and removes its socket when playback finishes, on `stop`, or after `BO_IDLE_TIMEOUT` seconds of silence (default 600, `0` disables).
- **Slicing, not files** — `uri@at:from-to` places any slice of a source anywhere on the timeline; no trimming, no copies.

## Install

Requires **Rust 1.85 or newer** (edition 2024).

```console
$ cargo build --release
$ target/release/bo --help
```

or install from this checkout:

```console
$ cargo install --path .
```

The name `bo` is already taken on crates.io, so `cargo install bo` installs an unrelated crate. Build from source, or use a release binary instead.

## Quick start

A two-track session — a voice over a bed of music:

```console
$ bo put bed.wav:00:00:00-00:00:30          # 30 s of a bed, on a fresh track
ok: track 0 clip #0 bed.wav @ 00:00:00.000
$ bo put voice.wav:00:00:00-00:00:30 1      # voice on track 1
ok: track 1 clip #0 voice.wav @ 00:00:00.000
$ bo play
session: 2 tracks | 2 clips | ends 00:00:30.000 | backend rodio
playing from 00:00:00.000
$ bo set track.0.volume 0.4   # duck the bed under the voice
track 0 volume 0.40
$ bo apply                    # make the change audible now
apply: rebuilt from 00:00:00.000
$ bo stop                     # end the session; the daemon cleans up
stopped
```

## License

BSD 3-Clause. See [LICENSE](LICENSE).
