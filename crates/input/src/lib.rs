//! Keyboard and mouse: global hotkeys and the panic key through Raw Input,
//! the feed of keys and mouse a controller sends, and the remote control
//! injector.

// Every call into Windows is in win.rs. The rest is plain code that the
// tests drive with made-up keyboard and mouse events.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod bindings;
mod feed;
mod grab;
mod hotkeys;
mod key;
mod remote;
mod tracker;
#[allow(unsafe_code)]
mod win;

pub use bindings::{Action, Bindings};
pub use feed::{Captured, Feed, Mouse};
pub use hotkeys::Hotkeys;
pub use key::{Chord, Key, Modifiers, ParseError};
pub use remote::{Remote, RemoteNumbers};
pub use tracker::{Elevation, Event, RawKey, Tracker};
pub use win::{process_elevation, this_process_elevated};
