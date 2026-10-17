//! CSS `text-box-trim` and `text-box-edge`: cutting the space a first or last line
//! box has above and below its glyphs out of the box that holds the text.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Pixels, RowMetrics};

/// Where the space a trim keeps starts above the baseline, as in CSS `text-box-edge`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextBoxOver {
    /// The ascent of the font: only the half-leading is cut.
    #[default]
    Text,
    /// The cap height of the font.
    Cap,
    /// The x-height of the font.
    Ex,
}

/// Where the space a trim keeps ends below the baseline, as in CSS `text-box-edge`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextBoxUnder {
    /// The descent of the font: only the half-leading is cut.
    #[default]
    Text,
    /// The baseline itself.
    Alphabetic,
}

/// The over and under edge a `text-box-trim` cuts to. The default is CSS's `auto`,
/// which behaves as `text`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TextBoxEdge {
    /// The edge above the baseline.
    pub over: TextBoxOver,
    /// The edge below the baseline.
    pub under: TextBoxUnder,
}

/// Which ends of a text `text-box-trim` cuts. Not inherited, unlike [`TextBoxEdge`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextBoxTrim {
    /// Cut above the first line.
    pub start: bool,
    /// Cut below the last line.
    pub end: bool,
}

impl TextBoxTrim {
    /// Whether neither end is cut.
    pub fn is_none(self) -> bool {
        !self.start && !self.end
    }
}

/// The metrics of the font of the root inline box, at its size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextBoxMetrics {
    /// The distance from the baseline up to the top of the font's glyph area.
    pub ascent: Pixels,
    /// The distance from the baseline down to the bottom of the font's glyph area,
    /// as a positive number.
    pub descent: Pixels,
    /// The height of a capital letter.
    pub cap_height: Pixels,
    /// The height of a lowercase x.
    pub x_height: Pixels,
}

impl TextBoxEdge {
    fn over_distance(self, metrics: &TextBoxMetrics) -> Pixels {
        match self.over {
            TextBoxOver::Text => metrics.ascent,
            TextBoxOver::Cap => metrics.cap_height,
            TextBoxOver::Ex => metrics.x_height,
        }
    }

    fn under_distance(self, metrics: &TextBoxMetrics) -> Pixels {
        match self.under {
            TextBoxUnder::Text => metrics.descent,
            TextBoxUnder::Alphabetic => Pixels::ZERO,
        }
    }
}

impl TextBoxTrim {
    /// The space cut above the first row and below the last row: from the top of the
    /// row to `edge` over its baseline, and from `edge` under the baseline to the
    /// bottom of the row. `None` rows (no text) cut nothing.
    pub fn amounts(
        self,
        edge: TextBoxEdge,
        first: Option<RowMetrics>,
        last: Option<RowMetrics>,
        metrics: &TextBoxMetrics,
    ) -> (Pixels, Pixels) {
        let top = match first {
            Some(row) if self.start => row.baseline - edge.over_distance(metrics),
            _ => Pixels::ZERO,
        };
        let bottom = match last {
            Some(row) if self.end => (row.height - row.baseline) - edge.under_distance(metrics),
            _ => Pixels::ZERO,
        };
        (top, bottom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    /// A 16px font: ascent 12, descent 4, cap height 10, x-height 7.
    fn metrics() -> TextBoxMetrics {
        TextBoxMetrics {
            ascent: px(12.),
            descent: px(4.),
            cap_height: px(10.),
            x_height: px(7.),
        }
    }

    /// A row of line height 24 around that font: half-leading 4, baseline 16.
    fn row() -> RowMetrics {
        RowMetrics {
            height: px(24.),
            baseline: px(16.),
        }
    }

    const BOTH: TextBoxTrim = TextBoxTrim {
        start: true,
        end: true,
    };

    fn cut(trim: TextBoxTrim, edge: TextBoxEdge) -> (f32, f32) {
        let (top, bottom) = trim.amounts(edge, Some(row()), Some(row()), &metrics());
        (top.into(), bottom.into())
    }

    #[test]
    fn the_text_edge_cuts_only_the_half_leading() {
        assert_eq!(cut(BOTH, TextBoxEdge::default()), (4., 4.));
    }

    #[test]
    fn the_cap_alphabetic_edge_cuts_down_to_the_cap_height_and_the_baseline() {
        let edge = TextBoxEdge {
            over: TextBoxOver::Cap,
            under: TextBoxUnder::Alphabetic,
        };
        // 24 - 6 - 8 = 10, the cap height.
        assert_eq!(cut(BOTH, edge), (6., 8.));
    }

    #[test]
    fn the_ex_edge_cuts_down_to_the_x_height() {
        let edge = TextBoxEdge {
            over: TextBoxOver::Ex,
            under: TextBoxUnder::Alphabetic,
        };
        assert_eq!(cut(BOTH, edge), (9., 8.));
    }

    #[test]
    fn an_end_that_is_not_trimmed_cuts_nothing() {
        let edge = TextBoxEdge {
            over: TextBoxOver::Cap,
            under: TextBoxUnder::Alphabetic,
        };
        let start = TextBoxTrim {
            start: true,
            end: false,
        };
        let end = TextBoxTrim {
            start: false,
            end: true,
        };
        assert_eq!(cut(start, edge), (6., 0.));
        assert_eq!(cut(end, edge), (0., 8.));
    }

    #[test]
    fn no_text_cuts_nothing() {
        let amounts = BOTH.amounts(TextBoxEdge::default(), None, None, &metrics());
        assert_eq!(amounts, (Pixels::ZERO, Pixels::ZERO));
    }

    #[test]
    fn a_line_height_under_the_content_area_cuts_a_negative_half_leading() {
        // Line height 12 around a content area of 16: half-leading -2, baseline 10.
        let tight = RowMetrics {
            height: px(12.),
            baseline: px(10.),
        };
        let (top, bottom) =
            BOTH.amounts(TextBoxEdge::default(), Some(tight), Some(tight), &metrics());
        assert_eq!((f32::from(top), f32::from(bottom)), (-2., -2.));
    }
}
