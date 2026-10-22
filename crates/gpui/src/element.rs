//! Elements are the workhorses of GPUI. They are responsible for laying out and painting all of
//! the contents of a window. Elements form a tree and are laid out according to the web layout
//! standards as implemented by [taffy](https://github.com/DioxusLabs/taffy). Most of the time,
//! you won't need to interact with this module or these APIs directly. Elements provide their
//! own APIs and GPUI, or other element implementation, uses the APIs in this module to convert
//! that element tree into the pixels you see on the screen.
//!
//! # Element Basics
//!
//! Elements are constructed by calling [`Render::render()`] on the root view of the window,
//! which recursively constructs the element tree from the current state of the application,.
//! These elements are then laid out by Taffy, and painted to the screen according to their own
//! implementation of [`Element::paint()`]. Before the start of the next frame, the entire element
//! tree and any callbacks they have registered with GPUI are dropped and the process repeats.
//!
//! But some state is too simple and voluminous to store in every view that needs it, e.g.
//! whether a hover has been started or not. For this, GPUI provides the [`Element::PrepaintState`], associated type.
//!
//! # Implementing your own elements
//!
//! Elements are intended to be the low level, imperative API to GPUI. They are responsible for upholding,
//! or breaking, GPUI's features as they deem necessary. As an example, most GPUI elements are expected
//! to stay in the bounds that their parent element gives them. But with [`Window::with_content_mask`],
//! you can ignore this restriction and paint anywhere inside of the window's bounds. This is useful for overlays
//! and popups and anything else that shows up 'on top' of other elements.
//! With great power, comes great responsibility.
//!
//! However, most of the time, you won't need to implement your own elements. GPUI provides a number of
//! elements that should cover most common use cases out of the box and it's recommended that you use those
//! to construct `components`, using the [`RenderOnce`] trait and the `#[derive(IntoElement)]` macro. Only implement
//! elements when you need to take manual control of the layout and painting process, such as when using
//! your own custom layout algorithm or rendering a code editor.

use crate::{
    A11yRelation, A11ySubtreeBuilder, App, ArenaBox, AvailableSpace, Bounds, Context, ContentMask,
    DispatchNodeId, Edges, ElementId, FocusHandle, InspectorElementId, IsZero, LayoutId, Pixels, Point,
    SharedString, Size, Style, Visibility, Window, util::FluentBuilder, window::with_element_arena,
};
use collections::FxHasher;
use std::{
    any::Any,
    fmt::{self, Debug, Display},
    hash::{Hash, Hasher},
    mem,
    ops::Deref,
    panic,
    sync::Arc,
};

/// Where an element that escapes the clip and scroll of its ancestors is drawn instead.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Escape {
    /// Drawn with the content mask and element offset of the nearest positioned ancestor's
    /// children: ancestors between the two neither clip nor scroll it. The window if there is no
    /// positioned ancestor.
    ContainingBlock,
    /// Clipped by the window only, and scrolled by nothing.
    Viewport,
}

/// Implemented by types that participate in laying out and painting the contents of a window.
/// Elements form a tree and are laid out according to web-based layout rules, as implemented by Taffy.
/// You can create custom elements by implementing this trait, see the module-level documentation
/// for more details.
pub trait Element: 'static + IntoElement {
    /// The type of state returned from [`Element::request_layout`]. A mutable reference to this state is subsequently
    /// provided to [`Element::prepaint`] and [`Element::paint`].
    type RequestLayoutState: 'static;

    /// The type of state returned from [`Element::prepaint`]. A mutable reference to this state is subsequently
    /// provided to [`Element::paint`].
    type PrepaintState: 'static;

    /// If this element has a unique identifier, return it here. This is used to track elements across frames, and
    /// will cause a GlobalElementId to be passed to the request_layout, prepaint, and paint methods.
    ///
    /// The global id can in turn be used to access state that's connected to an element with the same id across
    /// frames. This id must be unique among children of the first containing element with an id.
    fn id(&self) -> Option<ElementId>;

    /// Source location where this element was constructed, used to disambiguate elements in the
    /// inspector and navigate to their source code.
    fn source_location(&self) -> Option<&'static panic::Location<'static>>;

    /// Before an element can be painted, we need to know where it's going to be and how big it is.
    /// Use this method to request a layout from Taffy and initialize the element's state.
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState);

    /// After laying out an element, we need to commit its bounds to the current frame for hitbox
    /// purposes. The state argument is the same state that was returned from [`Element::request_layout()`].
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState;

    /// How far this element is moved from where layout put it, for `position: sticky`. Its
    /// children and its own paint move with it. Called in prepaint with the laid-out bounds.
    fn prepaint_offset(&self, _layout_bounds: Bounds<Pixels>, _window: &Window) -> Point<Pixels> {
        Point::default()
    }

    /// Whether this element is drawn without the content mask and element offset it inherits, like
    /// CSS `position: absolute` and `position: fixed`, which only the overflow and scroll of their
    /// containing block apply to. The element's bounds, hitboxes and paint all follow it.
    fn escapes_to(&self) -> Option<Escape> {
        None
    }

    /// The `z-index` of this element if it is drawn in the stacking order of the nearest
    /// stacking context (see [`crate::Div::stacking_context`]) instead of in its tree position, as
    /// a CSS positioned element is: its parent hands it over to that context once it is
    /// prepainted. `None` keeps it in the tree.
    fn lift_z(&self) -> Option<i32> {
        None
    }

    /// Once layout has been completed, this method will be called to paint the element to the screen.
    /// The state argument is the same state that was returned from [`Element::request_layout()`].
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    );

    /// How far past its laid-out box this element and everything below it can draw, or be hit.
    /// An element that returns `Some` may be culled: when the box and the scrollable overflow of
    /// the whole subtree lie outside the content mask, inflated by this, the parent prepaints
    /// and paints it through [`Element::prepaint_culled`] and [`Element::paint_culled`] instead
    /// of [`Element::prepaint`] and [`Element::paint`]. `None`, the default, is never culled.
    ///
    /// The caller vouches for the whole subtree: nothing in it draws or is hit outside what this
    /// says (so no transform, and no box that escapes its clip or is placed by a sticky offset),
    /// and nothing in it needs a hitbox, a listener or a paint of its own to behave, since
    /// the subtree is outside the mask and has none of those. See [`Window::is_culling`].
    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        None
    }

    /// [`Element::prepaint`] for an element in a culled subtree, called while
    /// [`Window::is_culling`]. Everything below the content mask is empty: nothing drawn is seen
    /// and nothing inserted can be hit. The element still owes what layout readers see of it:
    /// its bounds, the sizes and scroll metrics it reports, and its place in the tab order (see
    /// [`Element::paint_culled`]). The default does a full prepaint, which is correct and not
    /// faster.
    ///
    /// The accessibility tree is built as for any element: a culled subtree keeps its nodes. An
    /// element whose [`Element::a11y_synthetic_children`] reads its prepaint state has to return
    /// what [`Element::prepaint`] would while [`Window::is_a11y_active`].
    fn prepaint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.prepaint(id, inspector_id, bounds, request_layout, window, cx)
    }

    /// [`Element::paint`] for an element in a culled subtree. What paint registers that is not
    /// drawing is still owed: tab stops, and what listeners publish from paint. The default does
    /// a full paint, which is correct and not faster.
    fn paint_culled(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint(id, inspector_id, bounds, request_layout, prepaint, window, cx)
    }

    /// Returns the accessible role for this element, if any.
    /// Elements that return `None` are not included in the accessibility tree.
    ///
    /// Note: inclusion in accessibility tree requires non-`None` [`id`][Element::id].
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    fn a11y_role(&self) -> Option<accesskit::Role> {
        None
    }

    /// The `visibility` this element declares in its own style. While the visibility it ends up
    /// with is [`Visibility::Hidden`], the element is left out of the accessibility tree.
    fn declared_visibility(&self) -> Option<Visibility> {
        None
    }

    /// Write accessibility properties to the given node.
    /// Called only when `a11y_role()` returns `Some`.
    ///
    /// See the [accessibility guide](crate::_accessibility) for an overview.
    fn write_a11y_info(&self, _node: &mut accesskit::Node) {}

    /// Relations of this element's node to other nodes, named by author id.
    /// Resolved when the frame is finalized, so the targets may be anywhere in
    /// the tree. Read only when `a11y_role()` returns `Some`.
    fn a11y_relations(&self) -> &[(A11yRelation, Vec<SharedString>)] {
        &[]
    }

    /// Add synthetic child nodes to an [`Element`] that has an
    /// [`.id()`][Element::id] and a [`.role()`][Element::a11y_role].
    ///
    /// Some elements may want to inject accessibility nodes that do not
    /// correspond to any GPUI element. For example, a custom text field element
    /// may want to inject synthetic child nodes for the text content.
    ///
    /// See [Synthetic children](crate::_accessibility#synthetic-children) in
    /// the accessibility guide for more detail.
    fn a11y_synthetic_children(
        &mut self,
        _prepaint: &mut Self::PrepaintState,
        _builder: &mut A11ySubtreeBuilder,
    ) {
    }

    /// Convert this element into a dynamically-typed [`AnyElement`].
    fn into_any(self) -> AnyElement {
        AnyElement::new(self)
    }
}

/// Implemented by any type that can be converted into an element.
pub trait IntoElement: Sized {
    /// The specific type of element into which the implementing type is converted.
    /// Useful for converting other types into elements automatically, like Strings
    type Element: Element;

    /// Convert self into a type that implements [`Element`].
    fn into_element(self) -> Self::Element;

    /// Convert self into a dynamically-typed [`AnyElement`].
    fn into_any_element(self) -> AnyElement {
        self.into_element().into_any()
    }
}

impl<T: IntoElement> FluentBuilder for T {}

/// An object that can be drawn to the screen. This is the trait that distinguishes "views" from
/// other entities. Views are `Entity`'s which `impl Render` and drawn to the screen.
pub trait Render: 'static + Sized {
    /// Render this view into an element tree.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement;
}

impl Render for Empty {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// You can derive [`IntoElement`] on any type that implements this trait.
/// It is used to construct reusable `components` out of plain data. Think of
/// components as a recipe for a certain pattern of elements. RenderOnce allows
/// you to invoke this pattern, without breaking the fluent builder pattern of
/// the element APIs.
pub trait RenderOnce: 'static {
    /// Render this component into an element tree. Note that this method
    /// takes ownership of self, as compared to [`Render::render()`] method
    /// which takes a mutable reference.
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement;
}

/// This is a helper trait to provide a uniform interface for constructing elements that
/// can accept any number of any kind of child elements
pub trait ParentElement {
    /// Extend this element's children with the given child elements.
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>);

    /// Add a single child element to this element.
    fn child(mut self, child: impl IntoElement) -> Self
    where
        Self: Sized,
    {
        self.extend(std::iter::once(child.into_element().into_any()));
        self
    }

    /// Add multiple child elements to this element.
    fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self
    where
        Self: Sized,
    {
        self.extend(children.into_iter().map(|child| child.into_any_element()));
        self
    }
}

/// A globally unique identifier for an element, used to track state across frames.
///
/// Element state maps look a `GlobalElementId` up several times per element per frame, so the hash of
/// the id path is computed once at construction and every later hash just replays it.
#[derive(Clone, Debug)]
pub struct GlobalElementId {
    pub(crate) ids: Arc<[ElementId]>,
    hash: u64,
}

impl GlobalElementId {
    pub(crate) fn new(ids: &[ElementId]) -> Self {
        let mut hasher = FxHasher::default();
        ids.hash(&mut hasher);
        Self {
            ids: Arc::from(ids),
            hash: hasher.finish(),
        }
    }
}

impl GlobalElementId {
    /// A number that is the same for an element in every frame it is in, and almost always
    /// different between elements.
    pub(crate) fn scope_hash(&self) -> u64 {
        self.hash
    }
}

impl Default for GlobalElementId {
    fn default() -> Self {
        Self::new(&[])
    }
}

impl Deref for GlobalElementId {
    type Target = Arc<[ElementId]>;

    fn deref(&self) -> &Self::Target {
        &self.ids
    }
}

impl PartialEq for GlobalElementId {
    fn eq(&self, other: &Self) -> bool {
        if self.hash != other.hash {
            return false;
        }
        if Arc::ptr_eq(&self.ids, &other.ids) {
            return true;
        }
        #[cfg(any(test, feature = "test-support"))]
        FULL_PATH_COMPARES.with(|compares| compares.set(compares.get() + 1));
        self.ids == other.ids
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// How many times two ids that did not share their path were compared element by element.
    pub(crate) static FULL_PATH_COMPARES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Eq for GlobalElementId {}

impl Hash for GlobalElementId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl Display for GlobalElementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, element_id) in self.ids.iter().enumerate() {
            if i > 0 {
                write!(f, ".")?;
            }
            write!(f, "{}", element_id)?;
        }
        Ok(())
    }
}

impl GlobalElementId {
    pub(crate) fn accesskit_node_id(&self) -> accesskit::NodeId {
        let mut hasher = std::hash::DefaultHasher::default();
        self.ids.hash(&mut hasher);
        accesskit::NodeId(hasher.finish())
    }
}

trait ElementObject {
    fn inner_element(&mut self) -> &mut dyn Any;

    fn request_layout(&mut self, window: &mut Window, cx: &mut App) -> LayoutId;

    fn escapes_to(&self) -> Option<Escape>;

    fn lift_z(&self) -> Option<i32>;

    fn cull_outset(&self) -> Option<Edges<Pixels>>;

    fn prepaint(&mut self, window: &mut Window, cx: &mut App);

    fn paint(&mut self, window: &mut Window, cx: &mut App);

    fn layout_as_root(
        &mut self,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Size<Pixels>;
}

/// A wrapper around an implementer of [`Element`] that allows it to be drawn in a window.
pub struct Drawable<E: Element> {
    /// The drawn element.
    pub element: E,
    phase: ElementDrawPhase<E::RequestLayoutState, E::PrepaintState>,
}

/// Whether an element was prepainted as part of a culled subtree, see [`Element::cull_outset`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Culled {
    No,
    /// The top of the culled subtree, which empties the content mask for everything below it.
    Root,
    Inside,
}

#[derive(Default)]
enum ElementDrawPhase<RequestLayoutState, PrepaintState> {
    #[default]
    Start,
    RequestLayout {
        layout_id: LayoutId,
        global_id: Option<GlobalElementId>,
        inspector_id: Option<InspectorElementId>,
        request_layout: RequestLayoutState,
    },
    LayoutComputed {
        layout_id: LayoutId,
        global_id: Option<GlobalElementId>,
        inspector_id: Option<InspectorElementId>,
        available_space: Size<AvailableSpace>,
        request_layout: RequestLayoutState,
    },
    Prepaint {
        node_id: DispatchNodeId,
        global_id: Option<GlobalElementId>,
        inspector_id: Option<InspectorElementId>,
        bounds: Bounds<Pixels>,
        /// The content mask an escaping element paints with.
        escaped_mask: Option<(ContentMask<Pixels>, bool)>,
        culled: Culled,
        request_layout: RequestLayoutState,
        prepaint: PrepaintState,
    },
    Painted,
}

/// A wrapper around an implementer of [`Element`] that allows it to be drawn in a window.
impl<E: Element> Drawable<E> {
    /// A retained root is bookkeeping for what it replays or builds, which are counted as
    /// elements of their own, see [`crate::FrameWorkCounters`].
    fn is_retained_wrapper() -> bool {
        std::any::TypeId::of::<E>() == std::any::TypeId::of::<crate::Retained>()
    }

    pub(crate) fn new(element: E) -> Self {
        Drawable {
            element,
            phase: ElementDrawPhase::Start,
        }
    }

    fn request_layout(&mut self, window: &mut Window, cx: &mut App) -> LayoutId {
        match mem::take(&mut self.phase) {
            ElementDrawPhase::Start => {
                let global_id = self.element.id().map(|element_id| {
                    window.element_id_stack.push(element_id);
                    GlobalElementId::new(&window.element_id_stack)
                });

                let inspector_id;
                #[cfg(any(feature = "inspector", debug_assertions))]
                {
                    inspector_id = self.element.source_location().map(|source| {
                        let path = crate::InspectorElementPath {
                            global_id: GlobalElementId::new(&window.element_id_stack),
                            source_location: source,
                        };
                        window.build_inspector_element_id(path)
                    });
                }
                #[cfg(not(any(feature = "inspector", debug_assertions)))]
                {
                    inspector_id = None;
                }

                let outer_scope = global_id
                    .as_ref()
                    .map(|global_id| mem::replace(&mut window.layout_scope, global_id.scope_hash()));
                let (layout_id, request_layout) = self.element.request_layout(
                    global_id.as_ref(),
                    inspector_id.as_ref(),
                    window,
                    cx,
                );
                if let Some(outer_scope) = outer_scope {
                    window.layout_scope = outer_scope;
                }

                if global_id.is_some() {
                    window.element_id_stack.pop();
                }

                self.phase = ElementDrawPhase::RequestLayout {
                    layout_id,
                    global_id,
                    inspector_id,
                    request_layout,
                };
                layout_id
            }
            _ => panic!("must call request_layout only once"),
        }
    }

    pub(crate) fn prepaint(&mut self, window: &mut Window, cx: &mut App) {
        match mem::take(&mut self.phase) {
            ElementDrawPhase::RequestLayout {
                layout_id,
                global_id,
                inspector_id,
                mut request_layout,
            }
            | ElementDrawPhase::LayoutComputed {
                layout_id,
                global_id,
                inspector_id,
                mut request_layout,
                ..
            } => {
                if let Some(element_id) = self.element.id() {
                    window.element_id_stack.push(element_id);
                    debug_assert_eq!(&*global_id.as_ref().unwrap().ids, &*window.element_id_stack);
                }

                let culled = window.take_culled();
                if culled == Culled::Root {
                    window.content_mask_stack.push(ContentMask::default());
                }

                // Inside a culled subtree nothing draws or is hit, so only the offset an
                // escaping box is placed by matters: its bounds are read.
                let escaped = self.element.escapes_to().map(|escape| {
                    let derived = window.escape_frame_is_in_recording(escape);
                    let (mask, offset) = window.escape_frame(escape);
                    (mask, offset, derived)
                });
                if let Some((mask, offset, derived)) = escaped {
                    if culled == Culled::No {
                        window.note_escape(layout_id, &mask, derived);
                        window.content_mask_stack.push(mask);
                        window.begin_escaped_prepaint(derived);
                    }
                    window.element_offset_stack.push(offset);
                }

                let mut bounds = window.layout_bounds(layout_id);
                let sticky_offset = self.element.prepaint_offset(bounds, window);
                if !sticky_offset.is_zero() {
                    let shifted = window.element_offset() + sticky_offset;
                    window.element_offset_stack.push(shifted);
                    bounds = window.layout_bounds(layout_id);
                }
                let mut pushed_a11y_node = false;
                let hidden = self.element.declared_visibility().unwrap_or(window.visibility())
                    == Visibility::Hidden;
                if window.a11y.is_active() && !hidden {
                    if let Some(global_id) = global_id.as_ref() {
                        if let Some(role) = self.element.a11y_role() {
                            let node_id = global_id.accesskit_node_id();
                            let mut node = accesskit::Node::new(role);
                            let scale = window.scale_factor();
                            node.set_bounds(accesskit::Rect {
                                x0: (bounds.origin.x.0 * scale) as f64,
                                y0: (bounds.origin.y.0 * scale) as f64,
                                x1: ((bounds.origin.x.0 + bounds.size.width.0) * scale) as f64,
                                y1: ((bounds.origin.y.0 + bounds.size.height.0) * scale) as f64,
                            });
                            self.element.write_a11y_info(&mut node);
                            window.a11y.set_node_bounds(node_id, bounds);
                            pushed_a11y_node = window.a11y.nodes.push(node_id, node);
                            if pushed_a11y_node {
                                for (relation, author_ids) in self.element.a11y_relations() {
                                    window
                                        .a11y
                                        .nodes
                                        .defer_relation(node_id, *relation, author_ids);
                                }
                            }
                            #[cfg(debug_assertions)]
                            if pushed_a11y_node {
                                let view = window
                                    .a11y
                                    .view_type_names
                                    .get(&window.current_view())
                                    .copied();
                                let source_location = self.element.source_location();
                                window.a11y.nodes.record_node_info(
                                    node_id,
                                    crate::window::a11y::debug::NodeDebugInfo {
                                        synthetic: false,
                                        view,
                                        element_id: global_id.ids.last().map(|id| format!("{id:?}")),
                                        source_location,
                                    },
                                );
                            }
                        }
                    }
                }

                let node_id = window.next_frame.dispatch_tree.push_node();
                let outer_scope = global_id
                    .as_ref()
                    .map(|global_id| mem::replace(&mut window.layout_scope, global_id.scope_hash()));
                if !Self::is_retained_wrapper() {
                    match culled {
                        Culled::No => window.retained.work.prepainted += 1,
                        Culled::Root | Culled::Inside => window.retained.work.culled_prepainted += 1,
                    }
                }
                let mut prepaint = if culled == Culled::No {
                    self.element.prepaint(
                        global_id.as_ref(),
                        inspector_id.as_ref(),
                        bounds,
                        &mut request_layout,
                        window,
                        cx,
                    )
                } else {
                    self.element.prepaint_culled(
                        global_id.as_ref(),
                        inspector_id.as_ref(),
                        bounds,
                        &mut request_layout,
                        window,
                        cx,
                    )
                };
                if let Some(outer_scope) = outer_scope {
                    window.layout_scope = outer_scope;
                }
                window.next_frame.dispatch_tree.pop_node();
                if !sticky_offset.is_zero() {
                    window.element_offset_stack.pop();
                }
                if let Some((_, _, derived)) = escaped {
                    if culled == Culled::No {
                        window.end_escaped_prepaint(derived);
                        window.content_mask_stack.pop();
                    }
                    window.element_offset_stack.pop();
                }
                if culled == Culled::Root {
                    window.content_mask_stack.pop();
                }

                if pushed_a11y_node {
                    if let Some(global_id) = global_id.as_ref() {
                        #[cfg(debug_assertions)]
                        let creator = crate::window::a11y::debug::NodeCreator {
                            view: window
                                .a11y
                                .view_type_names
                                .get(&window.current_view())
                                .copied(),
                            element_id: global_id.ids.last().map(|id| format!("{id:?}")),
                            source_location: self.element.source_location(),
                        };
                        let mut builder = A11ySubtreeBuilder::new(
                            global_id.accesskit_node_id(),
                            &mut window.a11y.nodes,
                        );
                        #[cfg(debug_assertions)]
                        {
                            builder = builder.with_creator(creator);
                        }
                        self.element
                            .a11y_synthetic_children(&mut prepaint, &mut builder);
                    }
                    window.a11y.nodes.pop();
                }

                if global_id.is_some() {
                    window.element_id_stack.pop();
                }

                self.phase = ElementDrawPhase::Prepaint {
                    node_id,
                    global_id,
                    inspector_id,
                    bounds,
                    escaped_mask: escaped
                        .map(|(mask, _, derived)| (mask, derived))
                        .filter(|_| culled == Culled::No),
                    culled,
                    request_layout,
                    prepaint,
                };
            }
            _ => panic!("must call request_layout before prepaint"),
        }
    }

    pub(crate) fn paint(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> (E::RequestLayoutState, E::PrepaintState) {
        match mem::take(&mut self.phase) {
            ElementDrawPhase::Prepaint {
                node_id,
                global_id,
                inspector_id,
                bounds,
                escaped_mask,
                culled,
                mut request_layout,
                mut prepaint,
                ..
            } => {
                if let Some(element_id) = self.element.id() {
                    window.element_id_stack.push(element_id);
                    debug_assert_eq!(&*global_id.as_ref().unwrap().ids, &*window.element_id_stack);
                }

                window.next_frame.dispatch_tree.set_active_node(node_id);
                if !Self::is_retained_wrapper() {
                    match culled {
                        Culled::No => window.retained.work.painted += 1,
                        Culled::Root | Culled::Inside => window.retained.work.culled_painted += 1,
                    }
                }
                let element = &mut self.element;
                let mut paint = |window: &mut Window| {
                    if culled == Culled::No {
                        element.paint(
                            global_id.as_ref(),
                            inspector_id.as_ref(),
                            bounds,
                            &mut request_layout,
                            &mut prepaint,
                            window,
                            cx,
                        )
                    } else {
                        element.paint_culled(
                            global_id.as_ref(),
                            inspector_id.as_ref(),
                            bounds,
                            &mut request_layout,
                            &mut prepaint,
                            window,
                            cx,
                        )
                    }
                };
                match escaped_mask {
                    Some((mask, derived)) => window.with_escaped_content_mask(mask, derived, paint),
                    None if culled == Culled::Root => {
                        window.content_mask_stack.push(ContentMask::default());
                        paint(window);
                        window.content_mask_stack.pop();
                    }
                    None => paint(window),
                }

                if global_id.is_some() {
                    window.element_id_stack.pop();
                }

                self.phase = ElementDrawPhase::Painted;
                (request_layout, prepaint)
            }
            _ => panic!("must call prepaint before paint"),
        }
    }

    pub(crate) fn layout_as_root(
        &mut self,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Size<Pixels> {
        if matches!(&self.phase, ElementDrawPhase::Start) {
            self.request_layout(window, cx);
        }

        let layout_id = match mem::take(&mut self.phase) {
            ElementDrawPhase::RequestLayout {
                layout_id,
                global_id,
                inspector_id,
                request_layout,
            } => {
                window.compute_layout(layout_id, available_space, cx);
                self.phase = ElementDrawPhase::LayoutComputed {
                    layout_id,
                    global_id,
                    inspector_id,
                    available_space,
                    request_layout,
                };
                layout_id
            }
            ElementDrawPhase::LayoutComputed {
                layout_id,
                global_id,
                inspector_id,
                available_space: prev_available_space,
                request_layout,
            } => {
                if available_space != prev_available_space {
                    window.compute_layout(layout_id, available_space, cx);
                }
                self.phase = ElementDrawPhase::LayoutComputed {
                    layout_id,
                    global_id,
                    inspector_id,
                    available_space,
                    request_layout,
                };
                layout_id
            }
            _ => panic!("cannot measure after painting"),
        };

        window.layout_bounds(layout_id).size
    }
}

impl<E> ElementObject for Drawable<E>
where
    E: Element,
    E::RequestLayoutState: 'static,
{
    fn inner_element(&mut self) -> &mut dyn Any {
        &mut self.element
    }

    #[inline]
    fn request_layout(&mut self, window: &mut Window, cx: &mut App) -> LayoutId {
        Drawable::request_layout(self, window, cx)
    }

    #[inline]
    fn prepaint(&mut self, window: &mut Window, cx: &mut App) {
        Drawable::prepaint(self, window, cx);
    }

    #[inline]
    fn paint(&mut self, window: &mut Window, cx: &mut App) {
        Drawable::paint(self, window, cx);
    }

    fn escapes_to(&self) -> Option<Escape> {
        self.element.escapes_to()
    }

    fn lift_z(&self) -> Option<i32> {
        self.element.lift_z()
    }

    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        self.element.cull_outset()
    }

    #[inline]
    fn layout_as_root(
        &mut self,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Size<Pixels> {
        Drawable::layout_as_root(self, available_space, window, cx)
    }
}

/// A dynamically typed element that can be used to store any element type.
pub struct AnyElement(ArenaBox<dyn ElementObject>);

impl AnyElement {
    pub(crate) fn new<E>(element: E) -> Self
    where
        E: 'static + Element,
        E::RequestLayoutState: Any,
    {
        let element = with_element_arena(|arena| arena.alloc(|| Drawable::new(element)))
            .map(|element| element as &mut dyn ElementObject);
        AnyElement(element)
    }

    /// Attempt to downcast a reference to the boxed element to a specific type.
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.0.inner_element().downcast_mut::<T>()
    }

    /// What the element stored in this `AnyElement` escapes to. A wrapper that places something
    /// by its child's bounds forwards this from [`Element::escapes_to`].
    pub fn escapes_to(&self) -> Option<Escape> {
        self.0.escapes_to()
    }

    /// What the element stored in this `AnyElement` lifts to. A wrapper that places something by
    /// its child's bounds forwards this from [`Element::lift_z`].
    pub fn lift_z(&self) -> Option<i32> {
        self.0.lift_z()
    }

    /// What the element stored in this `AnyElement` says it can be culled by, see
    /// [`Element::cull_outset`]. A wrapper that places something by its child's bounds does not
    /// forward this: it is never culled.
    pub fn cull_outset(&self) -> Option<Edges<Pixels>> {
        self.0.cull_outset()
    }

    /// Request the layout ID of the element stored in this `AnyElement`.
    /// Used for laying out child elements in a parent element.
    pub fn request_layout(&mut self, window: &mut Window, cx: &mut App) -> LayoutId {
        self.0.request_layout(window, cx)
    }

    /// Prepares the element to be painted by storing its bounds, giving it a chance to draw hitboxes and
    /// request autoscroll before the final paint pass is confirmed.
    pub fn prepaint(&mut self, window: &mut Window, cx: &mut App) -> Option<FocusHandle> {
        let focus_assigned = window.next_frame.focus.is_some();

        self.0.prepaint(window, cx);

        if !focus_assigned && let Some(focus_id) = window.next_frame.focus {
            return FocusHandle::for_id(focus_id, &cx.focus_handles);
        }

        None
    }

    /// Prepaints the element stored in this `AnyElement` as the top of a culled subtree: it and
    /// everything below it publish what layout readers see through [`Element::prepaint_culled`],
    /// under an empty content mask, and [`AnyElement::paint`] then does the same.
    pub fn prepaint_culled(&mut self, window: &mut Window, cx: &mut App) {
        window.begin_culling();
        self.0.prepaint(window, cx);
        window.end_culling();
    }

    /// Paints the element stored in this `AnyElement`.
    pub fn paint(&mut self, window: &mut Window, cx: &mut App) {
        self.0.paint(window, cx);
    }

    /// Performs layout for this element within the given available space and returns its size.
    pub fn layout_as_root(
        &mut self,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Size<Pixels> {
        self.0.layout_as_root(available_space, window, cx)
    }

    /// Prepaints this element at the given absolute origin. The element is a layout root, so it is
    /// the containing block of the absolutely positioned boxes that have no positioned ancestor
    /// inside it, and a stacking context of its own: nothing inside it is lifted into a context
    /// outside.
    /// If any element in the subtree beneath this element is focused, its FocusHandle is returned.
    pub fn prepaint_at(
        &mut self,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<FocusHandle> {
        window.with_absolute_element_offset(origin, |window| {
            window.with_lift_barrier(|window| {
                window.with_positioned_frame(|window| self.prepaint(window, cx))
            })
        })
    }

    /// [`Self::prepaint_at`] for an element placed at an origin that layout has not snapped to a
    /// device pixel, such as an atomic inline in its line: the element is painted at the device
    /// pixel the origin rounds half up to and reports bounds from the origin itself, see
    /// [`Window::with_unsnapped_element_origin`].
    pub fn prepaint_at_unsnapped(
        &mut self,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<FocusHandle> {
        window.with_unsnapped_element_origin(origin, |window| {
            window.with_lift_barrier(|window| {
                window.with_positioned_frame(|window| self.prepaint(window, cx))
            })
        })
    }

    /// Performs layout on this element in the available space, then prepaints it at the given absolute origin.
    /// If any element in the subtree beneath this element is focused, its FocusHandle is returned.
    pub fn prepaint_as_root(
        &mut self,
        origin: Point<Pixels>,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<FocusHandle> {
        self.layout_as_root(available_space, window, cx);
        window.with_absolute_element_offset(origin, |window| {
            window.with_lift_barrier(|window| {
                window.with_positioned_frame(|window| self.prepaint(window, cx))
            })
        })
    }
}

impl Element for AnyElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static panic::Location<'static>> {
        None
    }

    // `ParentElement::child` wraps an `AnyElement` in another one, and the parent asks the outer
    // one.
    fn lift_z(&self) -> Option<i32> {
        AnyElement::lift_z(self)
    }

    // The wrapper does not escape, but the element inside may, and its parent asks the wrapper.
    fn cull_outset(&self) -> Option<Edges<Pixels>> {
        if AnyElement::escapes_to(self).is_some() {
            return None;
        }
        AnyElement::cull_outset(self)
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.paint(window, cx);
    }
}

impl IntoElement for AnyElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }

    fn into_any_element(self) -> AnyElement {
        self
    }
}

/// The empty element, which renders nothing.
pub struct Empty;

impl IntoElement for Empty {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Empty {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    display: crate::Display::None,
                    ..Default::default()
                },
                None,
                cx,
            ),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    use super::*;
    use crate::{
        AppContext as _, Context, IntoElement, Render, Styled as _, TestAppContext, canvas, div,
    };

    fn path(names: &[&'static str]) -> Vec<ElementId> {
        names.iter().map(|name| ElementId::from(*name)).collect()
    }

    fn hash_of(id: &GlobalElementId) -> u64 {
        let mut hasher = FxHasher::default();
        id.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn global_ids_of_the_same_path_are_equal_and_hash_alike() {
        let a = GlobalElementId::new(&path(&["root", "list", "row"]));
        let b = GlobalElementId::new(&path(&["root", "list", "row"]));
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    #[test]
    fn global_ids_of_different_paths_differ() {
        let id = GlobalElementId::new(&path(&["a", "b"]));
        for other in [
            path(&["b", "a"]),
            path(&["a"]),
            path(&["a", "b", "c"]),
            path(&["a", "c"]),
            path(&[]),
        ] {
            assert_ne!(id, GlobalElementId::new(&other), "{other:?}");
        }
    }

    #[test]
    fn global_ids_with_a_colliding_hash_still_compare_their_paths() {
        let a = GlobalElementId::new(&path(&["a"]));
        let b = GlobalElementId {
            ids: Arc::from(path(&["b"])),
            hash: a.hash,
        };
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn the_default_global_id_is_the_empty_path() {
        assert_eq!(GlobalElementId::default(), GlobalElementId::new(&[]));
    }

    type Counts = Rc<RefCell<HashMap<String, Vec<usize>>>>;

    struct CountsStatePerPath {
        paths: Vec<Vec<&'static str>>,
        counts: Counts,
    }

    impl Render for CountsStatePerPath {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let paths = self.paths.clone();
            let counts = self.counts.clone();
            div().size_full().child(canvas(
                move |_, window, _| {
                    for path in &paths {
                        touch_state(window, path, path, &counts);
                    }
                },
                |_, _, _, _| {},
            ))
        }
    }

    // Each visit increments the element state stored at the path and records the value it saw.
    fn touch_state(
        window: &mut Window,
        full: &[&'static str],
        rest: &[&'static str],
        counts: &Counts,
    ) {
        match rest.split_first() {
            Some((last, [])) => window.with_global_id(ElementId::from(*last), |id, window| {
                window.with_element_state::<usize, _>(id, |state, _| {
                    let next = state.map_or(1, |state| state + 1);
                    counts
                        .borrow_mut()
                        .entry(full.join("."))
                        .or_default()
                        .push(next);
                    ((), next)
                })
            }),
            Some((first, rest)) => window.with_id(ElementId::from(*first), |window| {
                touch_state(window, full, rest, counts)
            }),
            None => unreachable!(),
        }
    }

    fn draw_frames(paths: Vec<Vec<&'static str>>, frames: usize) -> HashMap<String, Vec<usize>> {
        let mut cx = TestAppContext::single();
        let counts = Counts::default();
        let window = cx.add_window({
            let counts = counts.clone();
            move |_, _| CountsStatePerPath { paths, counts }
        });
        for _ in 1..frames {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        }
        let counts = counts.borrow().clone();
        counts
    }

    #[test]
    fn element_state_follows_its_path_across_frames() {
        let counts = draw_frames(
            vec![vec!["a", "b"], vec!["a", "c"], vec!["b", "a"], vec!["a"]],
            3,
        );
        for path in ["a.b", "a.c", "b.a", "a"] {
            assert_eq!(counts[path], vec![1, 2, 3], "{path}");
        }
    }

    #[test]
    fn element_state_of_a_repeated_path_continues_within_the_frame() {
        let counts = draw_frames(vec![vec!["a", "b"], vec!["a", "b"]], 2);
        assert_eq!(counts["a.b"], vec![1, 2, 3, 4]);
    }
}
