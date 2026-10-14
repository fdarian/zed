//! The full CSS grid track vocabulary: sizes, `minmax()`, `fit-content()`,
//! `repeat()` (including `auto-fill` and `auto-fit`), line names and named areas.
//!
//! These mirror taffy's types so they can derive `JsonSchema` and `Refineable`
//! support like the rest of [`crate::Style`]. [`crate::GridTemplate`] stays the
//! shorthand for the common `repeat(n, minmax(0, 1fr))`.

use crate::SharedString;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One track sizing function: the `<track-breadth>` of a bound of `minmax()`.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub enum GridTrackSize {
    /// A length in pixels
    Length(f32),
    /// A fraction of the grid container, `0.5` for `50%`
    Percent(f32),
    /// A share of the free space. Only valid as the maximum of a track.
    Fr(f32),
    /// `auto`
    Auto,
    /// `min-content`
    MinContent,
    /// `max-content`
    MaxContent,
    /// `fit-content(<pixels>)`. Only valid as the maximum of a track.
    FitContentLength(f32),
    /// `fit-content(<percentage>)`. Only valid as the maximum of a track.
    FitContentPercent(f32),
}

/// A `<track-size>`: `minmax(min, max)`, or a single size standing for both bounds
/// as CSS defines it (`auto` is `minmax(auto, auto)`, a length is `minmax(len, len)`).
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GridTrack {
    /// The minimum sizing function
    pub min: GridTrackSize,
    /// The maximum sizing function
    pub max: GridTrackSize,
}

/// How many times a `repeat()` repeats its tracks.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
pub enum GridRepetition {
    /// `repeat(<n>, ...)`
    Count(u16),
    /// `repeat(auto-fill, ...)`
    AutoFill,
    /// `repeat(auto-fit, ...)`
    AutoFit,
}

/// One entry of a `grid-template-columns` or `grid-template-rows` list.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub enum GridTemplateEntry {
    /// A single track
    Track(GridTrack),
    /// A `repeat()`
    Repeat {
        /// How often the tracks repeat
        repetition: GridRepetition,
        /// The repeated tracks
        tracks: Vec<GridTrack>,
        /// Line names inside the repeat: one set before each track and one after the last
        line_names: Vec<Vec<SharedString>>,
    },
}

/// A parsed `grid-template-columns` / `grid-template-rows`.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GridTemplateTracks {
    /// The tracks, in order
    pub entries: Vec<GridTemplateEntry>,
    /// Names of the lines between entries: one set before each entry and one after
    /// the last, so it has `entries.len() + 1` sets (empty when a line has no name).
    pub line_names: Vec<Vec<SharedString>>,
}

/// A named rectangle of `grid-template-areas`, in 1-based grid line numbers.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GridTemplateArea {
    /// The area name
    pub name: SharedString,
    /// Row line the area starts at
    pub row_start: u16,
    /// Row line the area ends at
    pub row_end: u16,
    /// Column line the area starts at
    pub column_start: u16,
    /// Column line the area ends at
    pub column_end: u16,
}

/// A parsed `grid-template-areas`.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GridTemplateAreas {
    /// The named areas
    pub areas: Vec<GridTemplateArea>,
    /// Rows in the template, counting rows of `.` cells
    pub row_count: u16,
    /// Columns in the template, counting columns of `.` cells
    pub column_count: u16,
}

/// How auto-placed grid items fill the grid.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub enum GridAutoFlow {
    /// Fill each row in turn
    #[default]
    Row,
    /// Fill each column in turn
    Column,
    /// `row dense`
    RowDense,
    /// `column dense`
    ColumnDense,
}

impl From<GridAutoFlow> for taffy::GridAutoFlow {
    fn from(flow: GridAutoFlow) -> Self {
        match flow {
            GridAutoFlow::Row => Self::Row,
            GridAutoFlow::Column => Self::Column,
            GridAutoFlow::RowDense => Self::RowDense,
            GridAutoFlow::ColumnDense => Self::ColumnDense,
        }
    }
}
