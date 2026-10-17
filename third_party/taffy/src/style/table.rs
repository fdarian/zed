//! Style types for Table layout
use crate::geometry::Size;
use crate::{CoreStyle, Style};

/// Which algorithm sizes the columns of a table
///
/// See <https://developer.mozilla.org/en-US/docs/Web/CSS/table-layout>
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum TableLayout {
    /// Columns are sized from the content of all their cells
    #[default]
    Auto,
    /// Columns are sized from the `<col>` nodes and the cells of the first row, which needs a definite table width.
    /// A table with an `auto` width is laid out as `Auto`.
    Fixed,
}

#[cfg(feature = "parse")]
crate::util::parse::impl_parse_for_keyword_enum!(TableLayout,
    "auto" => Auto,
    "fixed" => Fixed,
);

/// What kind of row group a node is. The first header group is placed before all other row groups and the
/// first footer group after them, wherever they are in the children of the table.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum RowGroupKind {
    /// `display: table-header-group`
    Header,
    /// `display: table-row-group`
    #[default]
    Body,
    /// `display: table-footer-group`
    Footer,
}

/// The side of the table a caption is placed on
///
/// See <https://developer.mozilla.org/en-US/docs/Web/CSS/caption-side>
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum CaptionSide {
    /// Above the rows
    #[default]
    Top,
    /// Below the rows
    Bottom,
}

#[cfg(feature = "parse")]
crate::util::parse::impl_parse_for_keyword_enum!(CaptionSide,
    "top" => Top,
    "bottom" => Bottom,
);

/// How the content of a table cell is aligned in the block axis within the row
///
/// The other values of CSS `vertical-align` (`sub`, `super`, `text-top`, `text-bottom`, lengths and percentages)
/// behave as `baseline` on a table cell, so they map to [`VerticalAlign::Baseline`].
///
/// See <https://developer.mozilla.org/en-US/docs/Web/CSS/vertical-align>
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum VerticalAlign {
    /// The first baseline of the cell is aligned with the baseline of the row
    #[default]
    Baseline,
    /// The content starts at the top of the cell
    Top,
    /// The content is centred in the cell
    Middle,
    /// The content ends at the bottom of the cell
    Bottom,
}

#[cfg(feature = "parse")]
crate::util::parse::impl_parse_for_keyword_enum!(VerticalAlign,
    "baseline" => Baseline,
    "top" => Top,
    "middle" => Middle,
    "bottom" => Bottom,
);

/// What part of a table a node is. Only the children of a table, and the children of its row groups and rows,
/// are looked at.
///
/// The structure is repaired while laying out, without creating nodes:
///   - cells that are children of a table or row group form an implicit row, a run of them one row
///   - a child of a table, row group or row that is not a table part acts as a cell of its own, as does a
///     child of a row that is not a cell. Unlike CSS, neighbouring ones do not share an anonymous cell, so their
///     margins and block flow do not combine.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum TableRole {
    /// Not a table part
    #[default]
    None,
    /// `display: table-row-group`, `table-header-group` or `table-footer-group`
    RowGroup(RowGroupKind),
    /// `display: table-row`
    Row,
    /// `display: table-cell`. A span of 0 stretches the cell to the last row (`rowspan`) or is 1 (`colspan`).
    Cell {
        /// How many columns the cell covers (`colspan`)
        col_span: u16,
        /// How many rows the cell covers (`rowspan`)
        row_span: u16,
    },
    /// `display: table-caption`
    Caption(CaptionSide),
    /// `display: table-column`. Takes no space itself, its `size.width` is the width of the columns it spans.
    Column {
        /// How many columns the node stands for (`span`)
        span: u16,
    },
    /// `display: table-column-group`. When it has `Column` children they define its columns, otherwise `span` does.
    ColumnGroup {
        /// How many columns the node stands for when it has no column children (`span`)
        span: u16,
    },
}

impl TableRole {
    /// A cell covering one row and one column
    pub const CELL: TableRole = TableRole::Cell { col_span: 1, row_span: 1 };
}

/// The set of styles required for a Table layout container
pub trait TableContainerStyle: CoreStyle {
    /// Which algorithm sizes the columns
    #[inline(always)]
    fn table_layout(&self) -> TableLayout {
        Style::<Self::CustomIdent>::DEFAULT.table_layout
    }

    /// The gap between cells and between cells and the edge of the table. Ignored (taken as 0) when
    /// [`border_collapse`](Self::border_collapse) is set.
    #[inline(always)]
    fn border_spacing(&self) -> Size<f32> {
        Style::<Self::CustomIdent>::DEFAULT.border_spacing
    }

    /// Whether the borders of the cells and the table are collapsed. Spacing is then 0, and the border widths of the
    /// table and its cells are taken to be already the half widths of the collapsed borders.
    #[inline(always)]
    fn border_collapse(&self) -> bool {
        Style::<Self::CustomIdent>::DEFAULT.border_collapse
    }
}

/// The set of styles required for a part of a table (child of a table, row group or row)
pub trait TableItemStyle: CoreStyle {
    /// What part of the table the node is
    #[inline(always)]
    fn table_role(&self) -> TableRole {
        Style::<Self::CustomIdent>::DEFAULT.table_role
    }

    /// How the content of a cell is aligned in its row
    #[inline(always)]
    fn vertical_align(&self) -> VerticalAlign {
        Style::<Self::CustomIdent>::DEFAULT.vertical_align
    }
}
