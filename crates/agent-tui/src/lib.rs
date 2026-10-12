//! The terminal UI for `smoll`: a screen for the loop, not a second loop.
//!
//! Everything the CLI can do with `smoll task`, this does with the same [`Agent`]:
//! the same provider, the same twenty-four tools, the same sandbox policy, the
//! same approval modes, the same session record on disk. What it adds is a
//! picture that keeps its panes in place while a run goes, and a keyboard that
//! answers a question without anyone having to find the line it scrolled past.
//!
//! The shape of it:
//!
//! ```text
//!  smoll 0.1.0  provider mock branch master mode approve-edits ctx~1200/4096 …
//! ┌ transcript ─────────────────┐┌ plan ─────────────┐
//! │ › rename the helper         ││ Goal: rename      │
//! │ │ used in two places        ││ [x] 1. read it    │
//! ├ tool log ───────────────────┤├ asking about ─────┤
//! │ ok   fs_read_file src/…     ││ @@ -1 +1 @@       │
//! └─────────────────────────────┘│ +helper()         │
//!  › █                           └───────────────────┘
//!  /help · /theme <name> · /rollback · /clear · /quit — q quits
//! ```
//!
//! Three rules hold it together, and each is a test somewhere in this crate:
//!
//! - **The run is on another task.** The loop does filesystem work and waits on a
//!   model; the screen must keep drawing and keep taking keys through both. They
//!   talk only through [`crate::hub::Hub`], which is why the loop needs to know
//!   nothing about any of this.
//! - **The terminal comes back.** Raw mode and the alternate screen are undone on
//!   every path out — a clean quit, an error, a panic mid-draw. A shell left in
//!   raw mode is a worse outcome than any run this screen could show.
//! - **A question nobody answered is a no.** Both ways it can happen — the screen
//!   closed, or a key never came — the answer is refusal, in the same words the
//!   CLI's terminal approver uses.

mod app;
mod hub;
mod render;
mod state;
mod theme;

pub use app::{has_a_screen, press, run, Effect, Error, Report};
pub use hub::{Hub, Msg};
pub use render::{draw, draw_pane, fit, rows_for, Edge, Row};
pub use state::{Header, Kind, Line, Phase, Screen, Status};
pub use theme::{Palette, Theme};
