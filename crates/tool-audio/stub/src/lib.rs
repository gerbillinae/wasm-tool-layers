//! local:audio/player with no audio: `available` is false and `play` fails
//! with `unavailable`. Composed into a component that imports the player,
//! it leaves a component any host can run, minus sound.

wit_bindgen::generate!({ world: "player-stub", path: "../../../wit/deps/local-audio" });

use exports::local::audio::player::{Error, Guest, GuestPlayback, Playback};

struct Stub;

impl Guest for Stub {
    type Playback = NoClip;

    fn available() -> bool {
        false
    }

    fn play(_data: Vec<u8>, _mime: String) -> Result<Playback, Error> {
        Err(Error::Unavailable)
    }
}

/// Never made: play always fails.
struct NoClip;

impl GuestPlayback for NoClip {
    async fn finished(&self) {}
    fn stop(&self) {}
}

export!(Stub);
