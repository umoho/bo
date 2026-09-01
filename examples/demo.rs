//! Usage tour of the playlist model: `cargo run --example demo`.
//!
//! No audio, no TTY — just the data structure an agent would drive.

use std::time::Duration;

use bo::playlist::{Nav, PlayMode, Playlist, Repeat, Track};

fn main() {
    let mut pl = Playlist::seeded("late night", 20_260_901);
    pl.append_tracks([
        Track::new("music/m83_midnight_city.flac")
            .with_title("Midnight City")
            .with_artist("M83")
            .with_album("Hurry Up, We're Dreaming")
            .with_duration(Duration::from_secs(243)),
        Track::new("music/city_lights.flac")
            .with_title("City Lights")
            .with_artist("Washed Out")
            .with_duration(Duration::from_secs(212)),
        Track::new("music/afterhours.flac")
            .with_title("After Hours")
            .with_artist("The Weeknd")
            .with_duration(Duration::from_secs(1_008)),
        Track::new("https://stream.example/night-waves"),
    ]);

    println!("{pl}");

    // The agent speaks indices, ids and ranks — never string positions.
    println!("search \"city\" -> {:?}", pl.search("city"));
    let weeknd = pl.find(|t| t.artist() == Some("The Weeknd")).unwrap();

    // Transport: mode first, then step.
    pl.set_mode(PlayMode {
        shuffle: false,
        repeat: Repeat::All,
    });
    println!("advance: {}", pl.advance());
    println!("advance: {}", pl.advance());
    println!("up next: {:?}", pl.peek_next().map(Track::title));
    println!("jump to {weeknd}: {}", pl.jump_to(weeknd).unwrap());
    println!("back:      {}", pl.back());
    println!("back again:{}", pl.back());

    // Shuffle keeps the playing entry on the same slot.
    pl.set_shuffle(true);
    println!("\nshuffled play order: {:?}", pl.play_order());
    let mut steps = Vec::new();
    for _ in 0..pl.len() {
        match pl.advance() {
            Nav::Moved { index, .. } | Nav::Wrapped { index, .. } => {
                steps.push(pl.get(index).unwrap().title().to_owned())
            }
            Nav::Same { .. } => steps.push("(requeue)".into()),
            Nav::Ended => steps.push("(end)".into()),
        }
    }
    println!("one shuffled pass: {steps:?}");

    // Editing while playing: the cursor follows its track.
    println!("\n{}", pl);
    println!(
        "remove the finished opener: {:?}",
        pl.remove(0).map(|t| t.title().to_owned())
    );
    println!("{}", pl);
    println!(
        "length: {} tracks, {} known, total {:?}",
        pl.len(),
        pl.duration_known().0.as_secs(),
        pl.duration_total()
    );
}
