//! Task 83: the treemap, the sunburst and the flame graph.
//!
//! Generic over a weighted tree ([`Tree`], or anything implementing [`WeightedTree`]): the
//! Sizes view feeds it the folder totals it has (`sizes_view`); the Disk Usage window feeds
//! it a whole scan. Layouts (`layout`): squarified (the default), KDirStat's strips, Hilbert
//! and Moore. Shading (`shade`): cushions rasterized on a worker into a texture, or flat
//! tiles as one mesh, with WinDirStat's options and style presets. The widget (`widget`)
//! does hover tooltips, click to select, double-click / Enter / Ctrl+wheel to zoom in,
//! Backspace / Ctrl+wheel / the mouse's back button to zoom out, and arrow keys between
//! neighbors; the sunburst and the flame graph (`radial`) share its tree, zoom and
//! selection.

pub mod layout;
pub mod panel;
pub mod radial;
pub mod shade;
pub mod sizes_view;
#[cfg(test)]
mod tests;
pub mod tree;
pub mod widget;

pub use shade::Options;
pub use widget::keys_owned;
