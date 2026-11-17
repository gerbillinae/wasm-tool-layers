//! The native player as a core's Player port (tool-ports).

use std::sync::Arc;
use std::time::Duration;

use tool_ports::{Clip, LocalFuture, Player};

use crate::player::CommandPlayer;

/// The command-line player as the core's Player port.
pub struct NativePlayer(pub Arc<CommandPlayer>);

impl NativePlayer {
    pub fn new() -> NativePlayer {
        NativePlayer(Arc::new(CommandPlayer::new()))
    }
}

impl Default for NativePlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Player for NativePlayer {
    fn available(&self) -> bool {
        self.0.available()
    }
    fn play(&self, data: Vec<u8>, mime: &str) -> Result<Box<dyn Clip>, String> {
        let id = self.0.play(data, mime)?;
        Ok(Box::new(NativeClip { player: self.0.clone(), id }))
    }
}

struct NativeClip {
    player: Arc<CommandPlayer>,
    id: u64,
}

impl Clip for NativeClip {
    fn finished(&self) -> LocalFuture {
        let (player, id) = (self.player.clone(), self.id);
        // The player is a child process; look every 50 ms.
        Box::pin(async move {
            while !player.finished(id) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    }
    fn stop(&self) {
        self.player.stop(self.id);
    }
}
