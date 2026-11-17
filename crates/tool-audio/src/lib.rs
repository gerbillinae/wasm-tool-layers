//! Sound on the local machine's speakers, a capability standard WASI
//! doesn't have, as one implementation wrapped for each place it's needed:
//!
//! - [`player`]: the command-line player (afplay, mpv or ffplay);
//! - [`native`]: it as a core's `Player` port, for native builds;
//! - [`host`] (feature `host`): it as the `local:audio/player` WIT interface
//!   (in this repo's `wit/deps/local-audio`), for a tool-specific WASM host;
//! - `stub/`: a component implementing the interface as unavailable, to
//!   compose into a component so hosts without audio can run it.

pub mod native;
pub mod player;

#[cfg(feature = "host")]
pub mod host;
