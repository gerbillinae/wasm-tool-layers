//! The host side: `local:audio/player` for a tool-specific WASM host built
//! on tool-host. Add [`add_to_linker`] to the host's imports, keep an
//! [`Audio`] as the extension state, and offer a grant flag (e.g. `--audio`).

use tool_host::wasmtime::component::{Accessor, HasSelf, Resource};

use crate::player::CommandPlayer;

mod bindings {
    tool_host::wasmtime::component::bindgen!({
        world: "local:audio/player-host",
        path: "../../wit",
        with: { "local:audio/player.playback": super::Playback },
        wasmtime_crate: tool_host::wasmtime,
    });
}



/// A clip, as the component holds it: its rep is the player's clip id.
pub struct Playback;

/// The extension's state: the same player the native build uses.
pub struct Audio {
    pub granted: bool,
    pub player: CommandPlayer,
}

impl Host for Audio {
    fn available(&mut self) -> bool {
        self.granted && self.player.available()
    }

    fn play(&mut self, data: Vec<u8>, mime: String) -> Result<Resource<Playback>, Error> {
        if !self.granted {
            return Err(Error::Unavailable);
        }
        let id = self.player.play(data, &mime).map_err(Error::Device)?;
        Ok(Resource::new_own(id as u32))
    }
}

/// `finished` is async (WASI 0.3): it resolves when the clip ends, while
/// the component's other tasks and calls carry on.
impl<T> HostPlaybackWithStore<T> for HasSelf<Audio> {
    async fn finished(a: &Accessor<T, Self>, clip: Resource<Playback>) {
        let id = clip.rep() as u64;
        // The player is a child process; look every 50 ms.
        while !a.with(|mut s| s.get().player.finished(id)) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

impl HostPlayback for Audio {
    fn stop(&mut self, clip: Resource<Playback>) {
        self.player.stop(clip.rep() as u64)
    }

    fn drop(&mut self, clip: Resource<Playback>) -> tool_host::wasmtime::Result<()> {
        // A dropped clip that's still playing is stopped, so no sound
        // outlives its handle.
        self.player.stop(clip.rep() as u64);
        Ok(())
    }
}


use bindings::local::audio::player::{Error, Host, HostPlayback, HostPlaybackWithStore};

impl Audio {
    /// The extension state; `granted` is whether the user gave the grant.
    pub fn new(granted: bool) -> Audio {
        Audio { granted, player: CommandPlayer::new() }
    }
}

/// Adds local:audio/player to a tool-host linker, with the extension state
/// at `get`.
pub fn add_to_linker<T: Send + 'static>(
    linker: &mut tool_host::wasmtime::component::Linker<T>,
    get: fn(&mut T) -> &mut Audio,
) -> tool_host::wasmtime::Result<()> {
    bindings::local::audio::player::add_to_linker::<T, HasSelf<Audio>>(linker, get)
}
