//! Usage tour of the playlist model: `cargo run --example demo`.
//!
//! No audio, no TTS backend, no TTY — just the data structure an agent drives.
//! Note that music and speech are the same kind of row here: an entry only has
//! to say how it becomes sound (an address to open, or a script to speak), and
//! everything else is an opaque tag.

use std::time::Duration;

use bo::playlist::{Entry, Meta, Nav, PlayMode, Playlist, Repeat};

fn main() {
    let mut pl = Playlist::seeded("evening", 20_260_901);

    // Audio: an address is enough to queue and play.
    pl.push(Entry::new("https://stream.example/night-waves"));
    pl.extend([
        Entry::new("music/m83_hurry_up.flac").with_meta(
            Meta::labeled("Midnight City")
                .with_tag("artist", "M83")
                .with_tag("lossless", "yes")
                .with_duration(Duration::from_secs(243)),
        ),
        Entry::address("music/washed_out.flac")
            .with_label("City Lights")
            .with_tag("artist", "Washed Out")
            .with_duration(Duration::from_secs(212)),
    ]);

    // Speech: a script, with no audio yet. It still queues, lists, searches and
    // takes part in shuffle/cursor arithmetic like anything else.
    pl.push(
        Entry::speak("Take the 6:40 ferry to Makansytrand.\nLeave by 6:10.")
            .with_label("Ferry reminder")
            .with_tag("voice", "nova")
            .with_tag("rate", "1.05"),
    );

    println!("{pl}");
    println!("waiting to be synthesized: {:?}", pl.needs_synthesis());
    println!("no metadata at all:        {:?}", pl.untagged());
    println!("by artist (facet): {:?}", pl.facet("artist"));
    println!("\nsearch \"6:10\"     -> {:?}", pl.search("6:10"));
    println!("search \"ferry\"    -> {:?}", pl.search("ferry"));
    println!("search \"lossless\" -> {:?}", pl.search("lossless"));

    // The TTS worker finishes: the script becomes an address in place, and the
    // id, index, tags and cursor all survive.
    let pending = pl.needs_synthesis();
    let Some(&slot) = pending.first() else {
        println!("\nnothing to synthesize");
        return;
    };
    let id = pl.get(slot).unwrap().id();
    let changed = pl
        .get_mut(slot)
        .unwrap()
        .resolved_at("/tmp/bo/cache/ferry-1.wav");
    println!(
        "\nresolved entry {id} in place: {changed}, now opens {:?}",
        pl.get(slot).unwrap().uri()
    );
    println!("script kept? {:?}", pl.get(slot).unwrap().text());
    println!(
        "still tagged with a voice: {:?}",
        pl.get(slot).unwrap().tag("voice")
    );
    println!("needs synthesis now: {:?}", pl.needs_synthesis());

    // Transport: mode first, then step. Playback works the same for a file and
    // for a script that has just been rendered.
    pl.set_mode(PlayMode {
        shuffle: false,
        repeat: Repeat::All,
    });
    println!("\nadvance: {}", pl.advance());
    println!("advance: {}", pl.advance());
    println!("up next: {:?}", pl.peek_next().map(Entry::label));
    let ferry = pl.index_of(id).unwrap();
    println!("jump to {id}: {}", pl.jump_to(ferry).unwrap());
    println!("back:       {}", pl.back());
    println!("requeue:    {}", pl.back());

    // Shuffle pins the playing entry to its slot.
    pl.set_shuffle(true);
    println!("\nshuffled play order: {:?}", pl.play_order());
    let mut pass = Vec::new();
    for _ in 0..pl.len() {
        let step = match pl.advance() {
            Nav::Moved { index, .. } | Nav::Wrapped { index, .. } => {
                format!(
                    "{} ({})",
                    pl.get(index).unwrap().label(),
                    kind(pl.get(index).unwrap())
                )
            }
            Nav::Same { .. } => "(requeue)".into(),
            Nav::Ended => "(end)".into(),
        };
        pass.push(step);
    }
    println!("one shuffled pass: {pass:?}");
    println!("\n{pl}");
}

fn kind(entry: &Entry) -> &'static str {
    if entry.is_speech() { "speech" } else { "audio" }
}
