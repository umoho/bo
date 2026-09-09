"""Thin grammar and plumbing tests for pybo — every arrangement verb is
already exercised at the engine and on the typed wire; here we prove the
Python surface builds the right calls and renders the replies."""

import math
import os
import shutil
import struct
import tempfile
import wave

import pytest

import pybo


def write_wav(path, seconds=1.0, rate=44_100):
    w = wave.open(str(path), "w")
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    frames = b"".join(
        struct.pack("<h", int(12_000 * math.sin(2 * math.pi * 440 * i / rate)))
        for i in range(int(rate * seconds))
    )
    w.writeframes(frames)
    w.close()
    return str(path)


@pytest.fixture()
def bo(tmp_path):
    """A client on its own daemon (spawned on first use); the session ends
    with the test.

    The socket lives in a short `tempfile` dir, not pytest's tmp_path: a
    Unix socket path is limited to ~104 bytes, and a long test name would
    push it over and make the daemon's bind fail."""
    home = tempfile.mkdtemp(prefix="bo")
    client = pybo.Bo(socket=os.path.join(home, "s.sock"))
    yield client
    try:
        client.stop()
    except pybo.BoError:
        pass
    shutil.rmtree(home, ignore_errors=True)


# --------------------------------------------------------------------------
# The time dialect
# --------------------------------------------------------------------------


def test_timecode_speaks_seconds_and_text():
    assert pybo.Timecode("1.23").ms == 1230
    assert pybo.Timecode(1.23).ms == 1230
    assert pybo.Timecode(90).ms == 90_000  # an int is seconds too
    assert str(pybo.Timecode("1.23")) == "00:00:01.230"
    assert str(pybo.Timecode("1:02.5")) == "00:01:02.500"
    assert str(pybo.Timecode("00:01:00")) == "00:01:00.000"
    assert pybo.Timecode.from_ms(1230).ms == 1230
    assert pybo.Timecode("1:00").seconds == 60.0
    assert pybo.Timecode("1:00") == pybo.Timecode(60)
    with pytest.raises(ValueError):
        pybo.Timecode("bogus")
    with pytest.raises(ValueError):
        pybo.Timecode(-1)


def test_trim_builds_a_source_window():
    whole = pybo.trim("voice.wav")
    assert (whole.uri, whole.from_ms, whole.to_ms) == ("voice.wav", 0, None)
    closed = pybo.trim("voice.wav", "0:30-1:00")
    assert (closed.from_ms, closed.to_ms) == (30_000, 60_000)
    tail = pybo.trim("voice.wav", "0:30-")
    assert (tail.from_ms, tail.to_ms) == (30_000, None)
    kw = pybo.trim("voice.wav", start="0:30", to=90)
    assert (kw.from_ms, kw.to_ms) == (30_000, 90_000)
    with pytest.raises(ValueError):
        pybo.trim("voice.wav", "0:30-1:00", start="0:00")  # range or kw, not both
    with pytest.raises(ValueError):
        pybo.trim("voice.wav", "1:00-0:30")  # to before from
    with pytest.raises(ValueError):
        pybo.trim("voice.wav", start="1:00", to="0:30")


def test_track_placements_read_back():
    assert pybo.Track(0).index == 0
    at = pybo.Track(2).at("1:00")
    assert at.index == 2
    assert at.at_ms == 60_000
    assert "Track(2).at('00:01:00.000')" in repr(at)
    fresh = pybo.Track.fresh(at="0:05")
    assert fresh.index is None
    assert fresh.at_ms == 5_000
    assert "Track.fresh()" in repr(fresh)


# --------------------------------------------------------------------------
# Arrangement verbs over the daemon
# --------------------------------------------------------------------------


def test_put_get_and_set_round_trip(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    r = bo.put(src, pybo.Track(0).at("0:00"))
    assert r["track"] == 0
    assert r["clip"]["uri"].endswith("bed.wav")
    clip_id = r["clip"]["id"]

    # A whole source is probed: the clip's out-point is its real length.
    assert r["clip"]["to"] == 1000

    tree = bo.get("")
    assert tree["track"][0]["clips"][0]["id"] == clip_id

    r = bo.set("track.0.volume", 0.4)
    assert r["landed"] in ("live", "pending")
    assert bo.get("track.0.volume") == pytest.approx(0.4)
    bo.set("track.0", {"muted": True, "name": "bed"})
    assert bo.get("track.0.muted") is True
    assert bo.get("track.0.name") == "bed"


def test_trim_put_butt_joins_and_take_by_id(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    first = bo.put(pybo.trim(src, "0-0.5"), pybo.Track(0).at("0:00"))
    second = bo.put(pybo.trim(src, "0-0.5"), pybo.Track(0).at("0:00.500"))
    assert first["clip"]["id"] != second["clip"]["id"]

    # Remove the second clip by its stable id.
    taken = bo.take(on=0, clip_id=second["clip"]["id"])
    assert taken["clip"]["id"] == second["clip"]["id"]
    assert len(bo.get("track.0")["clips"]) == 1

    # Address by the moment the clip covers.
    taken = bo.take(on=0, at="0:00.200")
    assert len(bo.get("track.0")["clips"]) == 0
    with pytest.raises(pybo.BoError):
        bo.take(on=0, at="0:00.200")  # nothing covers it now


def test_move_repositions_between_tracks(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    bo.put(src, pybo.Track(0).at("0:00"))
    other = bo.put(pybo.trim(src, "0-0.5"), pybo.Track(0).at("1:00"))
    moved = bo.move(on=0, clip_id=other["clip"]["id"], to=pybo.Track(1).at("0:00"))
    assert moved["from_track"] == 0 and moved["to_track"] == 1
    assert moved["clip"]["id"] == other["clip"]["id"], "the move keeps the id"
    assert len(bo.get("track.0")["clips"]) == 1
    assert len(bo.get("track.1")["clips"]) == 1


def test_route_joins_a_named_bus_by_id_or_name(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    bo.put(src, pybo.Track(0).at("0:00"))
    bo.put(pybo.trim(src, "0-0.5"), pybo.Track(1).at("0:00"))
    created = bo.route(on=0, bus="music")
    assert created["bus"] == {"group": 0}
    joined = bo.route(on=1, bus="music")  # resolved to the existing bus
    assert joined["bus"] == {"group": 0}
    assert bo.get("bus.0")["members"] == 2
    # Back out by the master.
    assert bo.route(on=1, bus="master")["bus"] == "master"
    assert bo.get("bus.0")["members"] == 1


def test_a_refused_put_raises(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    bo.put(pybo.trim(src, "0-0.5"), pybo.Track(0).at("0:00"))
    with pytest.raises(pybo.BoError) as err:
        bo.put(pybo.trim(src, "0-0.5"), pybo.Track(0).at("0:00.200"))
    assert "overlap" in str(err.value) or "refused" in str(err.value)


# --------------------------------------------------------------------------
# Transport
# --------------------------------------------------------------------------


def test_transport_drives_the_state_machine(bo, tmp_path):
    # A long arrangement: the daemon must not finish playback mid-test.
    src = write_wav(tmp_path / "bed.wav", seconds=30)
    bo.put(src, pybo.Track(0).at("0:00"))
    played = bo.play()
    assert played["clips"] == 1 and played["end"] == 30_000
    bo.seek("0:00.500")
    paused = bo.pause()
    assert paused["at"] == 500
    resumed = bo.resume()
    assert resumed["at"] == 500
    bo.stop()
    assert bo.get("transport.state") == "stopped"
    assert bo.get("transport.playhead") == 0


# --------------------------------------------------------------------------
# Snapshots
# --------------------------------------------------------------------------


def test_save_load_reset_and_check(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    bo.put(src, pybo.Track(0).at("0:00"))
    bo.set("track.0.volume", 0.4)
    snap = str(tmp_path / "show.bo")
    saved = bo.save(snap)
    assert saved["commands"] == 2

    bo.reset()
    assert len(bo.get("")["track"]) == 0

    bo.load(snap)
    tree = bo.get("")
    assert len(tree["track"][0]["clips"]) == 1
    assert tree["track"][0]["volume"] == pytest.approx(0.4)

    bo.check(snap)


def test_render_writes_a_wav(bo, tmp_path):
    src = write_wav(tmp_path / "bed.wav")
    bo.put(src, pybo.Track(0).at("0:00"))
    out = str(tmp_path / "mix.wav")
    rendered = bo.render(out)
    assert rendered["duration_ms"] == 1000
    assert os.path.isfile(out) and os.path.getsize(out) > 100
