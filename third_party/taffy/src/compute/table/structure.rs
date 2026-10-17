//! The table grid: which row group, row, column and cell every child of a table is
use crate::LayoutTableContainer;
use crate::style::{BoxGenerationMode, CaptionSide, CoreStyle, Position, RowGroupKind, TableItemStyle, TableRole};
use crate::tree::NodeId;
use crate::util::sys::Vec;

/// A vector of `len` copies of `value`
pub(super) fn alloc_vec<T: Clone>(len: usize, value: T) -> Vec<T> {
    let mut vec = Vec::with_capacity(len);
    vec.resize(len, value);
    vec
}

/// CSS (HTML) caps on the number of columns and rows a cell spans
const MAX_COL_SPAN: usize = 1000;
/// CSS (HTML) caps on the number of columns and rows a cell spans
const MAX_ROW_SPAN: usize = 65534;

/// A cell, placed in the grid
pub(super) struct Cell {
    /// The node of the cell
    pub node: NodeId,
    /// Index into [`Structure::rows`] of the first row of the cell
    pub row: usize,
    /// Index of the first column of the cell
    pub col: usize,
    /// How many columns the cell covers
    pub col_span: usize,
    /// How many columns the cell was asked to cover, which can be more than it covers once the columns that nothing
    /// starts in are merged away
    pub requested_col_span: usize,
    /// How many rows the cell covers
    pub row_span: usize,
}

/// A row. Rows are in layout order, header group first and footer group last
pub(super) struct Row {
    /// The node of the row. `None` for an implicit row
    pub node: Option<NodeId>,
    /// Index into [`Structure::cells`] of the cells that start in this row, in source order
    pub cells: Vec<usize>,
}

/// A row group. Sections are in layout order, header group first and footer group last
pub(super) struct Section {
    /// The node of the row group. `None` for the rows that are children of the table itself
    pub node: Option<NodeId>,
    /// Index into [`Structure::rows`] of the first row
    pub first_row: usize,
    /// How many rows the section has
    pub row_count: usize,
    /// Whether it is a body group: not the header group or the footer group, which do not grow when the table is
    /// taller than its rows
    pub is_body: bool,
}

/// A `Column` node, or a column of a `ColumnGroup` node
pub(super) struct ColumnNode {
    /// The node
    pub node: NodeId,
    /// Index of the first column the node stands for
    pub first_col: usize,
    /// How many columns the node stands for
    pub span: usize,
    /// Index into [`Structure::column_groups`] of the group the node is in, if it is a child of one
    pub group: Option<usize>,
    /// Whether the node has a width, which makes its columns exist even if no cell starts in them
    pub has_width: bool,
}

/// A `ColumnGroup` node
pub(super) struct ColumnGroup {
    /// The node
    pub node: NodeId,
    /// Index of the first column of the group
    pub first_col: usize,
    /// How many columns the group has
    pub span: usize,
    /// Whether the group has a width and no column children, which makes its columns exist even if no cell starts in
    /// them
    pub has_width: bool,
}

/// A caption and the side of the table it is on
pub(super) struct Caption {
    /// The node
    pub node: NodeId,
    /// Which side it is on
    pub side: CaptionSide,
}

/// What the children of a table, row groups and rows turn out to be
pub(super) struct Structure {
    /// All the cells, in source order
    pub cells: Vec<Cell>,
    /// All the rows, in layout order
    pub rows: Vec<Row>,
    /// All the row groups, in layout order
    pub sections: Vec<Section>,
    /// The `Column` nodes that define a column: those that are children of the table or of a column group
    pub column_nodes: Vec<ColumnNode>,
    /// The `ColumnGroup` nodes
    pub column_groups: Vec<ColumnGroup>,
    /// How many columns the table has
    pub col_count: usize,
    /// The captions, in source order
    pub captions: Vec<Caption>,
    /// Absolutely positioned children of the table
    pub absolute: Vec<NodeId>,
    /// Nodes that generate no box, anywhere in the table
    pub hidden: Vec<NodeId>,
}

/// A child of a row group (or of the table, for the rows of the anonymous group) before cells are placed
enum RowSource {
    /// A `Row` node
    Row(NodeId),
    /// Nodes that stand for the cells of an implicit row
    Implicit(Vec<NodeId>),
}

/// A row group before cells are placed
struct SectionSource {
    /// `None` for the anonymous group of the rows that are children of the table
    node: Option<NodeId>,
    /// The kind of the group, which decides where it goes
    kind: RowGroupKind,
    /// The rows
    rows: Vec<RowSource>,
}

impl Structure {
    /// Reads the children of the table and the children of those, and places the cells in the grid
    pub(super) fn build<Tree: LayoutTableContainer>(tree: &Tree, table: NodeId) -> Structure {
        let mut structure = Structure {
            cells: Vec::new(),
            rows: Vec::new(),
            sections: Vec::new(),
            column_nodes: Vec::new(),
            column_groups: Vec::new(),
            col_count: 0,
            captions: Vec::new(),
            absolute: Vec::new(),
            hidden: Vec::new(),
        };
        let mut sources: Vec<SectionSource> = Vec::new();
        let mut explicit_columns = 0;
        // Cells that are children of the table, collected into one implicit row until another kind of child comes
        let mut implicit_run: Vec<NodeId> = Vec::new();

        for child in tree.child_ids(table) {
            let style = tree.get_table_item_style(child);
            if style.box_generation_mode() == BoxGenerationMode::None {
                structure.hidden.push(child);
                continue;
            }
            if style.position() == Position::Absolute {
                structure.absolute.push(child);
                continue;
            }
            let role = style.table_role();
            let has_width = !style.size().width.is_auto();
            drop(style);

            if !matches!(role, TableRole::None | TableRole::Cell { .. }) {
                structure.flush_implicit_run(&mut sources, &mut implicit_run);
            }
            match role {
                TableRole::None | TableRole::Cell { .. } => implicit_run.push(child),
                TableRole::Caption(side) => structure.captions.push(Caption { node: child, side }),
                TableRole::Column { span } => {
                    let span = span.max(1) as usize;
                    structure.column_nodes.push(ColumnNode {
                        node: child,
                        first_col: explicit_columns,
                        span,
                        group: None,
                        has_width,
                    });
                    explicit_columns += span;
                }
                TableRole::ColumnGroup { span } => {
                    explicit_columns += structure.add_column_group(tree, child, span, explicit_columns, has_width);
                }
                TableRole::Row => {
                    let section = anonymous_section(&mut sources);
                    sources[section].rows.push(RowSource::Row(child));
                }
                TableRole::RowGroup(kind) => {
                    let mut group = SectionSource { node: Some(child), kind, rows: Vec::new() };
                    structure.read_group(tree, child, &mut group);
                    sources.push(group);
                }
            }
        }
        structure.flush_implicit_run(&mut sources, &mut implicit_run);

        structure.col_count = explicit_columns;
        structure.place(tree, sources);
        structure
    }

    /// Merges every column that no cell starts in, and that was not given a width, into the column before it, the way
    /// browsers do: a cell that spans columns nothing else uses does not make the table wider, and `<col>`s past the
    /// last cell are dropped.
    pub(super) fn merge_empty_columns(&mut self) {
        let raw_count = self.col_count;
        let mut keep = alloc_vec(raw_count, false);
        for cell in &self.cells {
            keep[cell.col] = true;
        }
        for node in self.column_nodes.iter().filter(|node| node.has_width) {
            keep[node.first_col..node.first_col + node.span].fill(true);
        }
        for group in self.column_groups.iter().filter(|group| group.has_width) {
            keep[group.first_col..group.first_col + group.span].fill(true);
        }

        // The index of the column each raw column ends up in, `None` before the first column that is kept
        let mut merged_into: Vec<Option<usize>> = Vec::with_capacity(raw_count);
        let mut kept = 0;
        for &is_kept in &keep {
            kept += is_kept as usize;
            merged_into.push(kept.checked_sub(1));
        }
        for cell in &mut self.cells {
            let first = merged_into[cell.col].expect("a column with a cell is kept");
            let last = merged_into[cell.col + cell.col_span - 1].expect("it ends where it starts or after");
            cell.col = first;
            cell.col_span = last - first + 1;
        }
        // A node whose columns are all merged away has no width: it is a span of 0 at the column it merged into
        let remap = |first_col: usize, span: usize| -> (usize, usize) {
            let mut kept_columns = (first_col..first_col + span).filter(|raw| keep[*raw]);
            match kept_columns.next() {
                Some(first) => {
                    let last = kept_columns.last().unwrap_or(first);
                    let first = merged_into[first].expect("kept");
                    (first, merged_into[last].expect("kept") - first + 1)
                }
                None => (merged_into[first_col].unwrap_or(0), 0),
            }
        };
        for node in &mut self.column_nodes {
            (node.first_col, node.span) = remap(node.first_col, node.span);
        }
        for group in &mut self.column_groups {
            (group.first_col, group.span) = remap(group.first_col, group.span);
        }
        self.col_count = kept;
    }

    /// Turns the cells collected from the children of the table into an implicit row
    fn flush_implicit_run(&mut self, sources: &mut Vec<SectionSource>, run: &mut Vec<NodeId>) {
        if run.is_empty() {
            return;
        }
        let section = anonymous_section(sources);
        sources[section].rows.push(RowSource::Implicit(core::mem::take(run)));
    }

    /// Reads a `ColumnGroup`. Returns how many columns it has
    fn add_column_group<Tree: LayoutTableContainer>(
        &mut self,
        tree: &Tree,
        group_node: NodeId,
        span: u16,
        first_col: usize,
        has_width: bool,
    ) -> usize {
        let group = self.column_groups.len();
        let mut columns = 0;
        let mut has_column_children = false;
        for child in tree.child_ids(group_node) {
            let style = tree.get_table_item_style(child);
            if style.box_generation_mode() == BoxGenerationMode::None {
                self.hidden.push(child);
                continue;
            }
            if let TableRole::Column { span } = style.table_role() {
                let span = span.max(1) as usize;
                self.column_nodes.push(ColumnNode {
                    node: child,
                    first_col: first_col + columns,
                    span,
                    group: Some(group),
                    has_width: !style.size().width.is_auto(),
                });
                columns += span;
                has_column_children = true;
            }
        }
        // Column children define the columns of the group, `span` only counts when there are none.
        if columns == 0 {
            columns = span.max(1) as usize;
        }
        self.column_groups.push(ColumnGroup {
            node: group_node,
            first_col,
            span: columns,
            has_width: has_width && !has_column_children,
        });
        columns
    }

    /// Reads the children of a row group
    fn read_group<Tree: LayoutTableContainer>(&mut self, tree: &Tree, group_node: NodeId, group: &mut SectionSource) {
        let mut implicit_run: Vec<NodeId> = Vec::new();
        for child in tree.child_ids(group_node) {
            let style = tree.get_table_item_style(child);
            if style.box_generation_mode() == BoxGenerationMode::None {
                self.hidden.push(child);
                continue;
            }
            let role = style.table_role();
            drop(style);
            if let TableRole::Row = role {
                if !implicit_run.is_empty() {
                    group.rows.push(RowSource::Implicit(core::mem::take(&mut implicit_run)));
                }
                group.rows.push(RowSource::Row(child));
            } else {
                implicit_run.push(child);
            }
        }
        if !implicit_run.is_empty() {
            group.rows.push(RowSource::Implicit(implicit_run));
        }
    }

    /// Orders the groups, and places every cell in the grid
    fn place<Tree: LayoutTableContainer>(&mut self, tree: &Tree, sources: Vec<SectionSource>) {
        // The first header group goes first and the first footer group last, any other header or footer group is
        // a body group where it is.
        let header = sources.iter().position(|source| source.node.is_some() && source.kind == RowGroupKind::Header);
        let footer = sources.iter().position(|source| source.node.is_some() && source.kind == RowGroupKind::Footer);
        let mut order: Vec<usize> = Vec::with_capacity(sources.len());
        order.extend(header);
        order.extend((0..sources.len()).filter(|index| Some(*index) != header && Some(*index) != footer));
        order.extend(footer);

        let mut sources: Vec<Option<SectionSource>> = sources.into_iter().map(Some).collect();
        for index in order {
            let source = sources[index].take().expect("every group is placed once");
            let is_body = Some(index) != header && Some(index) != footer;
            self.place_section(tree, source, is_body);
        }
    }

    /// Places the cells of the rows of one group, honouring the cells of earlier rows that span into later ones
    fn place_section<Tree: LayoutTableContainer>(&mut self, tree: &Tree, source: SectionSource, is_body: bool) {
        let first_row = self.rows.len();
        let row_count = source.rows.len();
        // For every column the first row (relative to the group) it is free again
        let mut busy_until: Vec<usize> = Vec::new();

        for (row_in_section, row_source) in source.rows.into_iter().enumerate() {
            let (row_node, children) = match row_source {
                RowSource::Row(node) => {
                    let mut children = Vec::new();
                    for child in tree.child_ids(node) {
                        if tree.get_table_item_style(child).box_generation_mode() == BoxGenerationMode::None {
                            self.hidden.push(child);
                        } else {
                            children.push(child);
                        }
                    }
                    (Some(node), children)
                }
                RowSource::Implicit(children) => (None, children),
            };

            let row = self.rows.len();
            let mut cells = Vec::with_capacity(children.len());
            let mut col = 0;
            for child in children {
                let (col_span, row_span) = match tree.get_table_item_style(child).table_role() {
                    TableRole::Cell { col_span, row_span } => {
                        let col_span = (col_span as usize).clamp(1, MAX_COL_SPAN);
                        // A `rowspan` of 0 reaches the end of the group
                        let row_span = if row_span == 0 { MAX_ROW_SPAN } else { row_span as usize };
                        (col_span, row_span.min(row_count - row_in_section))
                    }
                    _ => (1, 1),
                };
                while col < busy_until.len() && busy_until[col] > row_in_section {
                    col += 1;
                }
                if busy_until.len() < col + col_span {
                    busy_until.resize(col + col_span, 0);
                }
                for busy in &mut busy_until[col..col + col_span] {
                    *busy = row_in_section + row_span;
                }
                cells.push(self.cells.len());
                self.cells.push(Cell { node: child, row, col, col_span, requested_col_span: col_span, row_span });
                self.col_count = self.col_count.max(col + col_span);
                col += col_span;
            }
            self.rows.push(Row { node: row_node, cells });
        }
        self.sections.push(Section { node: source.node, first_row, row_count, is_body });
    }
}

/// The group the rows that are children of the table go into: one anonymous group per run of them
fn anonymous_section(sources: &mut Vec<SectionSource>) -> usize {
    if sources.last().is_none_or(|last| last.node.is_some()) {
        sources.push(SectionSource { node: None, kind: RowGroupKind::Body, rows: Vec::new() });
    }
    sources.len() - 1
}
