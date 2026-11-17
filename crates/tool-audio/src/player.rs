//! Plays clips with a command-line player. Native only: one
//! implementation, wrapped as the core's Player port natively and as the
//! local:audio/player WIT interface by a tool-specific WASM host.

use std::sync::Mutex;

/// Plays clips with a command-line player (afplay on macOS, else mpv or
/// ffplay; ANKI_CARDS_PLAYER overrides), from temporary files. The Go tool
/// played files straight from Anki's media folder; the core now fetches the
/// audio through AnkiConnect, so it arrives as bytes.
pub struct CommandPlayer {
    command: Option<Vec<String>>,
    clips: Mutex<std::collections::HashMap<u64, (std::process::Child, std::path::PathBuf)>>,
    next: std::sync::atomic::AtomicU64,
}

impl Default for CommandPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPlayer {
    pub fn new() -> Self {
        let on_path = |c: &str| {
            std::env::var_os("PATH")
                .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(c).is_file()))
        };
        let command = match std::env::var("ANKI_CARDS_PLAYER") {
            Ok(s) if !s.trim().is_empty() => Some(s.split_whitespace().map(String::from).collect()),
            _ => [
                vec!["afplay"],
                vec!["mpv", "--no-video", "--really-quiet"],
                vec!["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet"],
            ]
            .into_iter()
            .find(|c| on_path(c[0]))
            .map(|c| c.into_iter().map(String::from).collect()),
        };
        CommandPlayer { command, clips: Mutex::default(), next: Default::default() }
    }
}

impl CommandPlayer {
    pub fn available(&self) -> bool {
        self.command.is_some()
    }

    pub fn play(&self, data: Vec<u8>, mime: &str) -> Result<u64, String> {
        let cmd = self.command.as_ref().ok_or("no audio player found: install mpv or ffplay, or set ANKI_CARDS_PLAYER")?;
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ext = if mime == "audio/mpeg" { "mp3" } else { "audio" };
        let path = std::env::temp_dir().join(format!("anki-cards-{}-{id}.{ext}", std::process::id()));
        std::fs::write(&path, data).map_err(|e| e.to_string())?;
        let child = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("{} couldn't start: {e}", cmd[0]))?;
        self.clips.lock().unwrap().insert(id, (child, path));
        Ok(id)
    }

    pub fn finished(&self, clip: u64) -> bool {
        let mut clips = self.clips.lock().unwrap();
        let Some((child, _)) = clips.get_mut(&clip) else { return true };
        if matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        if let Some((_, path)) = clips.remove(&clip) {
            let _ = std::fs::remove_file(path);
        }
        true
    }

    pub fn stop(&self, clip: u64) {
        if let Some((mut child, path)) = self.clips.lock().unwrap().remove(&clip) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(path);
        }
    }
}
