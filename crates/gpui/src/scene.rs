// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Anchor, AtlasTextureId, AtlasTile, Background, BackgroundTile, Bounds, ContentMask, CornerRadii, Corners, Edges,
    GroupEffect, Hsla, MAX_GROUP_EXTENT, Pixels, Point, Radians, ResolvedFilter, ScaledPixels, Size,
    bounds_tree::BoundsTree, filter_margin, point, visual_bounds,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    mem,
    ops::{Add, Range, Sub},
    slice,
};

#[allow(non_camel_case_types, unused)]
#[expect(missing_docs)]
pub type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

#[expect(missing_docs)]
pub type DrawOrder = u32;

/// A boolean stored as a `u32` so that GPU-facing structs contain no
/// compiler-inserted padding bytes, which would be undefined behavior to
/// reinterpret as `&[u8]` when writing instance buffers. Guaranteed to be
/// `0` or `1` by construction; shaders read it as a `u32`/`uint`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PaddedBool32(u32);

impl From<bool> for PaddedBool32 {
    fn from(value: bool) -> Self {
        PaddedBool32(value as u32)
    }
}

#[derive(Default)]
#[expect(missing_docs)]
pub struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<DrawOrder>,
    pub shadows: Vec<Shadow>,
    pub quads: Vec<Quad>,
    pub paths: Vec<Path<ScaledPixels>>,
    pub underlines: Vec<Underline>,
    pub monochrome_sprites: Vec<MonochromeSprite>,
    pub subpixel_sprites: Vec<SubpixelSprite>,
    pub polychrome_sprites: Vec<PolychromeSprite>,
    pub surfaces: Vec<PaintSurface>,
    /// Offscreen effect groups, each a scene of its own composited into this one.
    pub groups: Vec<EffectGroup>,
    effect_groups_enabled: bool,
    /// A scene nested in a group has no paint operations of its own: the root records them all.
    is_sub_scene: bool,
    group_stack: Vec<GroupBuilder>,
    /// Without effect groups, the opacities of the groups being drawn flat into the scene.
    flat_opacities: Vec<f32>,
    text_clips: Vec<TextClip>,
    next_group_id: u64,
    hidden: bool,
}

/// A group under construction: its content goes to its own scene until the group ends.
struct GroupBuilder {
    id: u64,
    effect: GroupEffect,
    content_mask: ContentMask<ScaledPixels>,
    reserved_order: Option<DrawOrder>,
    scene: Scene,
}

/// Glyphs painted while a text clip is open are also kept here, for `background-clip: text`.
struct TextClip {
    scene: Scene,
    group_id: Option<u64>,
}

/// A group of primitives drawn on their own and composited into the scene they are in, which is
/// how opacity, filters, masks and clip paths apply to the group as a whole.
#[expect(missing_docs)]
pub struct EffectGroup {
    pub id: u64,
    pub order: DrawOrder,
    /// Where the group paints in the enclosing scene: its content plus what the filters add,
    /// in whole device pixels. The group's own scene has its origin here.
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub effect: GroupEffect,
    pub scene: Scene,
    /// The glyphs the group is multiplied by, for `background-clip: text`.
    pub text_mask: Option<Scene>,
}

#[expect(missing_docs)]
impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
        self.groups.clear();
        self.group_stack.clear();
        self.flat_opacities.clear();
        self.text_clips.clear();
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    /// Whether the renderer composites offscreen groups. Without it, a group is drawn straight
    /// into the scene with only its opacity applied, one primitive at a time.
    pub fn set_effect_groups_enabled(&mut self, enabled: bool) {
        self.effect_groups_enabled = enabled;
    }

    pub fn effect_groups_enabled(&self) -> bool {
        self.effect_groups_enabled
    }

    fn sub_scene() -> Scene {
        Scene {
            is_sub_scene: true,
            ..Scene::default()
        }
    }

    fn record(&mut self, operation: PaintOperation) {
        if !self.is_sub_scene {
            self.paint_operations.push(operation);
        }
    }

    /// The scene primitives currently go to: the innermost open group's, or this one.
    fn current_mut(&mut self) -> &mut Scene {
        if self.group_stack.is_empty() {
            self
        } else {
            let group = self.group_stack.last_mut();
            &mut group.expect("the stack is not empty").scene
        }
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        self.record(PaintOperation::StartLayer(bounds));
        let current = self.current_mut();
        let order = current.primitive_bounds.insert(bounds);
        current.layer_stack.push(order);
    }

    pub fn pop_layer(&mut self) {
        self.record(PaintOperation::EndLayer);
        self.current_mut().layer_stack.pop();
    }

    /// A scene to record the operations of a painter in, instead of this one: it renders as this
    /// one would, and is hidden when this one is.
    pub(crate) fn recorder(&self) -> Scene {
        Scene {
            effect_groups_enabled: self.effect_groups_enabled,
            hidden: self.hidden,
            ..Scene::default()
        }
    }

    /// The operations that were recorded, to be put into another scene.
    pub(crate) fn into_operations(self) -> Vec<PaintOperation> {
        self.paint_operations
    }

    /// Marks the start of what is painted under a content mask that replaces the one around it,
    /// see [`PaintOperation::Replaced`].
    pub(crate) fn begin_replaced(&mut self, derived: bool) {
        self.record(PaintOperation::Replaced { derived });
    }

    pub(crate) fn end_replaced(&mut self) {
        self.record(PaintOperation::EndReplaced);
    }

    /// While hidden, primitives are dropped (`visibility: hidden`): the element keeps its box in
    /// the layout, and effect groups and layers still open and close around nothing.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        self.insert(primitive.into(), true);
    }

    /// Inserts an image. Unlike the sprite of an emoji, which is a glyph, it is not part of
    /// what a text clip masks by.
    pub fn insert_image(&mut self, sprite: PolychromeSprite) {
        self.insert(sprite.into(), false);
    }

    fn insert(&mut self, primitive: Primitive, glyph: bool) {
        if self.hidden {
            return;
        }
        if primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds)
            .is_empty()
        {
            return;
        }
        match (&primitive, glyph) {
            (Primitive::PolychromeSprite(sprite), false) => {
                self.record(PaintOperation::Image(*sprite))
            }
            _ => self.record(PaintOperation::Primitive(primitive.clone())),
        }
        self.route(primitive, glyph);
    }

    /// Puts an already recorded primitive where it is drawn: opacity of a flat group, the open text
    /// clips, and the scene of the innermost open group.
    fn route(&mut self, primitive: Primitive, glyph: bool) {
        let mut primitive = primitive;
        if !self.effect_groups_enabled {
            let opacity: f32 = self.flat_opacities.iter().product();
            if opacity < 1. {
                primitive.apply_opacity(opacity);
            }
        }
        if glyph && let Some(glyph) = primitive.as_glyph_mask() {
            for clip in &mut self.text_clips {
                clip.scene.insert_primitive_local(glyph.clone());
            }
        }
        self.current_mut().insert_primitive_local(primitive);
    }

    /// Inserts a quad. One with no background is split around its empty interior, so that the
    /// shader does not shade every transparent pixel inside a large outline. The split is made
    /// here and not by the painter because the pieces are cut out of the quad's content mask,
    /// which a recording that is put elsewhere only knows once it is.
    pub(crate) fn insert_quad(&mut self, quad: Quad) {
        if self.hidden || quad.bounds.intersect(&quad.content_mask.bounds).is_empty() {
            return;
        }
        self.record(PaintOperation::Quad(quad));
        if !quad.background.is_transparent() {
            self.route(quad.into(), true);
            return;
        }
        let outer_bounds = quad.bounds;
        let inner_bounds = largest_border_interior(&quad);

        // A strip narrows the mask to a different rectangle, which would drop the mask's rounded
        // corners, so a rounded mask paints the whole quad.
        if inner_bounds.is_empty() || quad.content_mask.corner_radii != Corners::default() {
            self.route(quad.into(), true);
            return;
        }

        let strips = [
            // Top
            Bounds::from_corners(
                outer_bounds.origin,
                point(outer_bounds.right(), inner_bounds.top()),
            ),
            // Bottom
            Bounds::from_corners(
                point(outer_bounds.left(), inner_bounds.bottom()),
                outer_bounds.bottom_right(),
            ),
            // Left
            Bounds::from_corners(
                point(outer_bounds.left(), inner_bounds.top()),
                inner_bounds.bottom_left(),
            ),
            // Right
            Bounds::from_corners(
                inner_bounds.top_right(),
                point(outer_bounds.right(), inner_bounds.bottom()),
            ),
        ];

        for strip in strips {
            let content_mask_bounds = quad.content_mask.bounds.intersect(&strip);
            if !content_mask_bounds.is_empty() {
                let piece = Quad {
                    content_mask: ContentMask {
                        bounds: content_mask_bounds,
                        corner_radii: Corners::default(),
                        corner_radii_y: Corners::default(),
                    },
                    ..quad
                };
                self.route(piece.into(), true);
            }
        }
    }

    /// Stores the primitive in this scene with an order, without recording or routing it.
    fn insert_primitive_local(&mut self, mut primitive: Primitive) {
        let clipped_bounds = primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return;
        }

        let order = self
            .layer_stack
            .last()
            .copied()
            .unwrap_or_else(|| self.primitive_bounds.insert(clipped_bounds));
        match &mut primitive {
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(*shadow);
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(*quad);
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(*underline);
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(*sprite);
            }
            Primitive::SubpixelSprite(sprite) => {
                sprite.order = order;
                self.subpixel_sprites.push(*sprite);
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(*sprite);
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
        }
    }

    /// How many primitives and groups this scene holds.
    fn item_count(&self) -> usize {
        self.shadows.len()
            + self.quads.len()
            + self.paths.len()
            + self.underlines.len()
            + self.monochrome_sprites.len()
            + self.subpixel_sprites.len()
            + self.polychrome_sprites.len()
            + self.surfaces.len()
            + self.groups.len()
    }

    /// The area this scene paints, each primitive clipped to its content mask.
    fn content_bounds(&self) -> Option<Bounds<ScaledPixels>> {
        let mut union: Option<Bounds<ScaledPixels>> = None;
        let mut add = |bounds: Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>| {
            let clipped = bounds.intersect(&mask.bounds);
            if !clipped.is_empty() {
                union = Some(union.map_or(clipped, |union| union.union(&clipped)));
            }
        };
        for shadow in &self.shadows {
            add(shadow.bounds, &shadow.content_mask);
        }
        for quad in &self.quads {
            add(quad.bounds, &quad.content_mask);
        }
        for path in &self.paths {
            add(path.bounds, &path.content_mask);
        }
        for underline in &self.underlines {
            add(underline.bounds, &underline.content_mask);
        }
        for sprite in &self.monochrome_sprites {
            add(sprite.bounds, &sprite.content_mask);
        }
        for sprite in &self.subpixel_sprites {
            add(sprite.bounds, &sprite.content_mask);
        }
        for sprite in &self.polychrome_sprites {
            add(sprite.bounds, &sprite.content_mask);
        }
        for surface in &self.surfaces {
            add(surface.bounds, &surface.content_mask);
        }
        for group in &self.groups {
            add(group.bounds, &group.content_mask);
        }
        union
    }

    /// The only primitive of a scene that holds exactly one and no groups.
    fn take_only_primitive(&mut self) -> Option<Primitive> {
        // A surface can't be faded in place: its pixels are the platform's to draw.
        if self.item_count() != 1 || !self.groups.is_empty() || !self.surfaces.is_empty() {
            return None;
        }
        if let Some(shadow) = self.shadows.pop() {
            return Some(Primitive::Shadow(shadow));
        }
        if let Some(quad) = self.quads.pop() {
            return Some(Primitive::Quad(quad));
        }
        if let Some(path) = self.paths.pop() {
            return Some(Primitive::Path(path));
        }
        if let Some(underline) = self.underlines.pop() {
            return Some(Primitive::Underline(underline));
        }
        if let Some(sprite) = self.monochrome_sprites.pop() {
            return Some(Primitive::MonochromeSprite(sprite));
        }
        if let Some(sprite) = self.subpixel_sprites.pop() {
            return Some(Primitive::SubpixelSprite(sprite));
        }
        if let Some(sprite) = self.polychrome_sprites.pop() {
            return Some(Primitive::PolychromeSprite(sprite));
        }
        None
    }

    /// Moves everything in the scene, so a group's content can be drawn into a texture that
    /// starts at the group's origin.
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        for shadow in &mut self.shadows {
            shadow.translate(delta);
        }
        for quad in &mut self.quads {
            quad.translate(delta);
        }
        for path in &mut self.paths {
            path.translate(delta);
        }
        for underline in &mut self.underlines {
            underline.translate(delta);
        }
        for sprite in &mut self.monochrome_sprites {
            sprite.translate(delta);
        }
        for sprite in &mut self.subpixel_sprites {
            sprite.translate(delta);
        }
        for sprite in &mut self.polychrome_sprites {
            sprite.translate(delta);
        }
        for surface in &mut self.surfaces {
            surface.translate(delta);
        }
        for group in &mut self.groups {
            group.bounds.origin = group.bounds.origin + delta;
            translate_mask(&mut group.content_mask, delta);
        }
    }

    /// Opens a group. Until [`Scene::end_group`], primitives go to a scene of their own that is
    /// composited with `effect` into this one. `reserved` fixes where the group sits in the
    /// stacking order up front rather than by what it ends up covering, for a group that must
    /// stay below what is painted after it.
    pub fn start_group(
        &mut self,
        effect: GroupEffect,
        content_mask: ContentMask<ScaledPixels>,
        reserved: Option<Bounds<ScaledPixels>>,
    ) {
        self.record(PaintOperation::StartGroup {
            effect: effect.clone(),
            content_mask,
            reserved,
        });
        if !self.effect_groups_enabled {
            self.flat_opacities.push(effect.opacity);
            return;
        }
        let reserved_order = reserved.map(|bounds| {
            let current = self.current_mut();
            current
                .layer_stack
                .last()
                .copied()
                .unwrap_or_else(|| current.primitive_bounds.insert(bounds))
        });
        let id = self.next_group_id;
        self.next_group_id += 1;
        self.group_stack.push(GroupBuilder {
            id,
            effect,
            content_mask,
            reserved_order,
            scene: Scene::sub_scene(),
        });
    }

    /// Lets the open groups composite where `escaped` is, for a box that escaped the clip of
    /// the elements between it and its containing block. The group's own primitives keep the mask
    /// they were painted with, so only what the filters spread of the clipped part is let past
    /// the clip.
    pub fn widen_groups(&mut self, escaped: ContentMask<ScaledPixels>) {
        self.record(PaintOperation::WidenGroups(escaped));
        for builder in &mut self.group_stack {
            let union = builder.content_mask.bounds.union(&escaped.bounds);
            if union != builder.content_mask.bounds {
                builder.content_mask = ContentMask {
                    bounds: union,
                    corner_radii: Corners::default(),
                    corner_radii_y: Corners::default(),
                };
            }
        }
    }

    pub fn end_group(&mut self) {
        self.record(PaintOperation::EndGroup);
        if !self.effect_groups_enabled {
            self.flat_opacities.pop();
            return;
        }
        let Some(builder) = self.group_stack.pop() else {
            return;
        };
        let GroupBuilder {
            id,
            mut effect,
            content_mask,
            reserved_order,
            mut scene,
        } = builder;

        // Nothing of a fully faded group shows, whatever it filters or sits over.
        if effect.opacity <= 0. {
            return;
        }
        let has_backdrop = !effect.backdrop_filters.is_empty();
        if scene.item_count() == 0 && !has_backdrop {
            return;
        }
        if effect.is_opacity_only() && reserved_order.is_none() {
            if let Some(mut only) = scene.take_only_primitive() {
                only.apply_opacity(effect.opacity);
                self.current_mut().insert_primitive_local(only);
                return;
            }
        }

        let mut visual = scene
            .content_bounds()
            .map(|content| visual_bounds(content, &effect.filters));
        if has_backdrop || effect.text_mask {
            // Only the part of the element's box inside the content mask is ever composited, and
            // the box can be arbitrarily large.
            let reference = visual_bounds(effect.reference.intersect(&content_mask.bounds), &[]);
            if !reference.is_empty() {
                visual = Some(visual.map_or(reference, |visual| visual.union(&reference)));
            }
        }
        let Some(visual) = visual else {
            return;
        };
        let Some(visual) = limit_group_bounds(visual, &content_mask, &effect.filters) else {
            return;
        };

        let delta = Point::new(ScaledPixels(-visual.origin.x.0), ScaledPixels(-visual.origin.y.0));
        scene.translate(delta);
        scene.finish();
        effect.translate(delta);
        if let Some(layers) = &mut effect.mask {
            let local = Bounds::new(point(ScaledPixels(0.), ScaledPixels(0.)), visual.size);
            // The sprites keep their content mask: it is the rounded clip of the layer.
            for quad in layers.iter_mut().flat_map(|layer| &mut layer.quads) {
                quad.content_mask = ContentMask {
                    bounds: local,
                    corner_radii: Corners::default(),
                    corner_radii_y: Corners::default(),
                };
            }
        }

        if effect.text_mask {
            if let Some(clip) = self
                .text_clips
                .iter_mut()
                .rev()
                .find(|clip| clip.group_id.is_none())
            {
                clip.group_id = Some(id);
            }
        }

        self.current_mut().insert_group_local(
            EffectGroup {
                id,
                order: 0,
                bounds: visual,
                content_mask,
                effect,
                scene,
                text_mask: None,
            },
            reserved_order,
        );
    }

    fn insert_group_local(&mut self, mut group: EffectGroup, reserved_order: Option<DrawOrder>) {
        let clipped = group.bounds.intersect(&group.content_mask.bounds);
        if clipped.is_empty() {
            return;
        }
        group.order = reserved_order.unwrap_or_else(|| {
            self.layer_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.primitive_bounds.insert(clipped))
        });
        self.groups.push(group);
    }

    /// Starts keeping a copy of every glyph painted from now until [`Scene::end_text_clip`]. The
    /// next group opened with a text mask is multiplied by the glyphs' alpha.
    pub fn start_text_clip(&mut self) {
        self.record(PaintOperation::StartTextClip);
        if self.effect_groups_enabled {
            self.text_clips.push(TextClip {
                scene: Scene::sub_scene(),
                group_id: None,
            });
        }
    }

    pub fn end_text_clip(&mut self) {
        self.record(PaintOperation::EndTextClip);
        if !self.effect_groups_enabled {
            return;
        }
        let Some(clip) = self.text_clips.pop() else {
            return;
        };
        let Some(group_id) = clip.group_id else {
            return;
        };
        let mut glyphs = clip.scene;
        let current = self.current_mut();
        if let Some(group) = current.groups.iter_mut().rev().find(|group| group.id == group_id) {
            glyphs.translate(Point::new(
                ScaledPixels(-group.bounds.origin.x.0),
                ScaledPixels(-group.bounds.origin.y.0),
            ));
            glyphs.finish();
            group.text_mask = Some(glyphs);
        }
    }

    /// The quads of this scene and of the groups inside it, in this scene's coordinates.
    pub fn quads_with_groups(&self) -> Vec<Quad> {
        let mut quads = self.quads.clone();
        for group in &self.groups {
            for mut quad in group.scene.quads_with_groups() {
                quad.translate(group.bounds.origin);
                quads.push(quad);
            }
        }
        quads
    }

    /// The image sprites of this scene and of the groups inside it, in this scene's coordinates.
    pub fn polychrome_sprites_with_groups(&self) -> Vec<PolychromeSprite> {
        let mut sprites = self.polychrome_sprites.clone();
        for group in &self.groups {
            for mut sprite in group.scene.polychrome_sprites_with_groups() {
                sprite.translate(group.bounds.origin);
                sprites.push(sprite);
            }
        }
        sprites
    }

    /// The underlines of this scene and of the groups inside it, in this scene's coordinates.
    pub fn underlines_with_groups(&self) -> Vec<Underline> {
        let mut underlines = self.underlines.clone();
        for group in &self.groups {
            for mut underline in group.scene.underlines_with_groups() {
                underline.translate(group.bounds.origin);
                underlines.push(underline);
            }
        }
        underlines
    }

    /// The shadows of this scene and of the groups inside it, in this scene's coordinates.
    pub fn shadows_with_groups(&self) -> Vec<Shadow> {
        let mut shadows = self.shadows.clone();
        for group in &self.groups {
            for mut shadow in group.scene.shadows_with_groups() {
                shadow.translate(group.bounds.origin);
                shadows.push(shadow);
            }
        }
        shadows
    }

    /// The operations in `range`, as text without their draw order, for the comparison of two
    /// frames that painted them at different places in the order. `loose` leaves out whether a
    /// replaced content mask was cut from the masks inside the recording, which a recording
    /// put in a region of its own has and a build of the same subtree does not.
    pub(crate) fn describe_operations(&self, range: Range<usize>, loose: bool) -> String {
        self.describe_operations_with(range, true, loose)
    }

    /// The operations of the whole scene that are drawn: all but the marks that only say how a
    /// recording of them is put under another content mask.
    pub(crate) fn describe_drawn(&self) -> String {
        self.describe_operations_with(0..self.paint_operations.len(), false, false)
    }

    fn describe_operations_with(&self, range: Range<usize>, marks: bool, loose: bool) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        for operation in self.paint_operations.get(range).unwrap_or_default() {
            if !marks && matches!(operation, PaintOperation::Replaced { .. } | PaintOperation::EndReplaced) {
                continue;
            }
            match operation {
                PaintOperation::Primitive(Primitive::Shadow(shadow)) => write!(out, "{shadow:?}"),
                PaintOperation::Primitive(Primitive::Quad(quad)) | PaintOperation::Quad(quad) => {
                    write!(out, "{quad:?}")
                }
                PaintOperation::Primitive(Primitive::Path(path)) => write!(out, "{path:?}"),
                PaintOperation::Primitive(Primitive::Underline(underline)) => write!(out, "{underline:?}"),
                PaintOperation::Primitive(Primitive::MonochromeSprite(sprite)) => write!(out, "{sprite:?}"),
                PaintOperation::Primitive(Primitive::SubpixelSprite(sprite)) => write!(out, "{sprite:?}"),
                PaintOperation::Primitive(Primitive::PolychromeSprite(sprite)) | PaintOperation::Image(sprite) => {
                    write!(out, "{sprite:?}")
                }
                PaintOperation::Primitive(Primitive::Surface(surface)) => write!(out, "{surface:?}"),
                PaintOperation::StartLayer(bounds) => write!(out, "layer {bounds:?}"),
                PaintOperation::EndLayer => write!(out, "end layer"),
                PaintOperation::StartGroup { content_mask, reserved, .. } => {
                    write!(out, "group {content_mask:?} {reserved:?}")
                }
                PaintOperation::EndGroup => write!(out, "end group"),
                PaintOperation::WidenGroups(mask) => write!(out, "widen {mask:?}"),
                PaintOperation::StartTextClip => write!(out, "text clip"),
                PaintOperation::EndTextClip => write!(out, "end text clip"),
                PaintOperation::Replaced { .. } if loose => write!(out, "replaced"),
                PaintOperation::Replaced { derived } => write!(out, "replaced {derived}"),
                PaintOperation::EndReplaced => write!(out, "end replaced"),
            }
            .ok();
            out.push('\n');
        }
        out
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        // The recorded operations already left out whatever was hidden when they were recorded.
        let hidden = mem::take(&mut self.hidden);
        for operation in &prev_scene.paint_operations[range] {
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::Quad(quad) => self.insert_quad(*quad),
                PaintOperation::Image(sprite) => self.insert_image(*sprite),
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
                PaintOperation::StartGroup {
                    effect,
                    content_mask,
                    reserved,
                } => self.start_group(effect.clone(), *content_mask, *reserved),
                PaintOperation::EndGroup => self.end_group(),
                PaintOperation::WidenGroups(escaped) => self.widen_groups(*escaped),
                PaintOperation::StartTextClip => self.start_text_clip(),
                PaintOperation::EndTextClip => self.end_text_clip(),
                PaintOperation::Replaced { derived } => self.begin_replaced(*derived),
                PaintOperation::EndReplaced => self.end_replaced(),
            }
        }
        self.hidden = hidden;
    }

    /// Puts recorded `operations` into this scene `delta` away from where they were recorded,
    /// clipped by `clip`, the content mask around the place they are put in, which they were
    /// recorded without: their masks hold the region they were recorded in. What this gives is
    /// what painting at the new place under `clip` gives, when a painter's decisions do not
    /// depend on where the clip is: see [`Window::replay_key`].
    pub(crate) fn replay_translated(
        &mut self,
        operations: &[PaintOperation],
        delta: Point<ScaledPixels>,
        clip: &ReplayClip,
    ) {
        // The recorded operations already left out whatever was hidden when they were recorded.
        let hidden = mem::take(&mut self.hidden);
        // The masks that replaced the one around them in the recording are not cut by the one
        // around the place the recording is put, unless they were cut from the masks inside it.
        // Whether what is put now is cut by `clip`: inside a mask that replaced the one around
        // it, until one that was cut from the masks inside the recording (a box handed to a
        // stacking context inside it, painted under the mask it was handed over with) replaces
        // that in turn.
        let mut clipping: Vec<bool> = Vec::new();
        // Whether each layer open was skipped, as a painter skips one that is clipped away.
        let mut layers: Vec<bool> = Vec::new();
        for operation in operations {
            let clipped = clipping.last().copied().unwrap_or(true);
            match operation {
                PaintOperation::Primitive(primitive) => {
                    let mut primitive = primitive.clone();
                    primitive.translate_replayed(delta);
                    if clipped {
                        primitive.clip_to(clip);
                    }
                    self.insert_primitive(primitive);
                }
                PaintOperation::Quad(quad) => {
                    let mut quad = *quad;
                    quad.translate(delta);
                    if clipped {
                        clip_mask(&mut quad.content_mask, &clip.snapped);
                    }
                    self.insert_quad(quad);
                }
                PaintOperation::Image(sprite) => {
                    let mut sprite = *sprite;
                    sprite.translate(delta);
                    if clipped {
                        clip_mask(&mut sprite.content_mask, &clip.snapped);
                    }
                    self.insert_image(sprite);
                }
                PaintOperation::StartLayer(bounds) => {
                    let mut bounds = *bounds;
                    translate_bounds(&mut bounds, delta);
                    if clipped {
                        bounds = bounds.intersect(&clip.snapped);
                    }
                    let skipped = bounds.is_empty();
                    layers.push(skipped);
                    if !skipped {
                        self.push_layer(bounds);
                    }
                }
                PaintOperation::EndLayer => {
                    if layers.pop() == Some(false) {
                        self.pop_layer();
                    }
                }
                PaintOperation::StartGroup {
                    effect,
                    content_mask,
                    reserved,
                } => {
                    let mut effect = effect.clone();
                    // The group of an exact clip has no box of its own: its effect is a default.
                    if !effect.exact_clip {
                        effect.translate(delta);
                    }
                    let mut content_mask = *content_mask;
                    translate_mask(&mut content_mask, delta);
                    if clipped {
                        clip_mask(&mut content_mask, &clip.snapped);
                    }
                    let reserved = reserved.map(|mut bounds| {
                        translate_bounds(&mut bounds, delta);
                        bounds
                    });
                    self.start_group(effect, content_mask, reserved);
                }
                PaintOperation::EndGroup => self.end_group(),
                PaintOperation::WidenGroups(escaped) => {
                    let mut escaped = *escaped;
                    translate_mask(&mut escaped, delta);
                    if clipped {
                        clip_mask(&mut escaped, &clip.snapped);
                    }
                    self.widen_groups(escaped);
                }
                PaintOperation::StartTextClip => self.start_text_clip(),
                PaintOperation::EndTextClip => self.end_text_clip(),
                PaintOperation::Replaced { derived } => {
                    clipping.push(*derived);
                    self.begin_replaced(*derived);
                }
                PaintOperation::EndReplaced => {
                    clipping.pop();
                    self.end_replaced();
                }
            }
        }
        self.hidden = hidden;
    }

    pub fn finish(&mut self) {
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.subpixel_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.surfaces.sort_by_key(|surface| surface.order);
        self.groups.sort_by_key(|group| group.order);
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub fn batches(&self) -> impl Iterator<Item = PrimitiveBatch> + '_ {
        BatchIterator {
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            subpixel_sprites_start: 0,
            subpixel_sprites_iter: self.subpixel_sprites.iter().peekable(),
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
            groups_start: 0,
            groups_iter: self.groups.iter().peekable(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    SubpixelSprite,
    PolychromeSprite,
    Surface,
    EffectGroup,
}

#[derive(Clone)]
pub(crate) enum PaintOperation {
    Primitive(Primitive),
    /// A quad before a border-only one is split around its empty interior, which depends on the
    /// content mask it ends up with: see [`Scene::insert_quad`].
    Quad(Quad),
    Image(PolychromeSprite),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
    StartGroup {
        effect: GroupEffect,
        content_mask: ContentMask<ScaledPixels>,
        reserved: Option<Bounds<ScaledPixels>>,
    },
    EndGroup,
    WidenGroups(ContentMask<ScaledPixels>),
    StartTextClip,
    EndTextClip,
    /// What follows, until the matching [`PaintOperation::EndReplaced`], was painted under a
    /// content mask that replaced the one around it. `derived` says that it was cut from the
    /// masks inside the subtree that painted it, and not an absolute one such as the viewport's.
    /// A recording put under another content mask cuts the first by that mask and not the second.
    Replaced {
        derived: bool,
    },
    EndReplaced,
}

fn largest_border_interior(quad: &Quad) -> Bounds<ScaledPixels> {
    let horizontal = &quad.corner_radii;
    let vertical = CornerRadii {
        x: quad.corner_radii,
        y: quad.corner_radii_y,
    }
    .explicit_y();
    let widths = &quad.border_widths;
    // How far in from each edge a rounded corner reaches: the vertical radius from the top and
    // bottom, the horizontal one from the left and right.
    let edge_radii = Edges {
        top: vertical.top_left.max(vertical.top_right),
        right: horizontal.top_right.max(horizontal.bottom_right),
        bottom: vertical.bottom_left.max(vertical.bottom_right),
        left: horizontal.top_left.max(horizontal.bottom_left),
    };

    let antialias_inset = point(ScaledPixels(1.0), ScaledPixels(1.0));
    let inset_bounds = |top_left_inset, bottom_right_inset| {
        Bounds::from_corners(
            quad.bounds.origin + top_left_inset + antialias_inset,
            quad.bounds.bottom_right() - bottom_right_inset - antialias_inset,
        )
    };

    // Rounded corners need only be excluded on one axis. Either candidate
    // is empty of border pixels, so use the larger interior.
    let horizontal_band = inset_bounds(
        point(widths.left, widths.top.max(edge_radii.top)),
        point(widths.right, widths.bottom.max(edge_radii.bottom)),
    );
    let vertical_band = inset_bounds(
        point(widths.left.max(edge_radii.left), widths.top),
        point(widths.right.max(edge_radii.right), widths.bottom),
    );

    let area = |bounds: &Bounds<ScaledPixels>| {
        bounds.size.width.0.max(0.) * bounds.size.height.0.max(0.)
    };
    if area(&horizontal_band) >= area(&vertical_band) {
        horizontal_band
    } else {
        vertical_band
    }
}

/// Keeps a group's bounds to what can show: what the content mask lets through, plus what the
/// filters spread before it is cut, and never beyond what a texture can hold.
fn limit_group_bounds(
    bounds: Bounds<ScaledPixels>,
    content_mask: &ContentMask<ScaledPixels>,
    filters: &[ResolvedFilter],
) -> Option<Bounds<ScaledPixels>> {
    let margin = filter_margin(filters);
    let reach = content_mask.bounds.extend(Edges {
        top: ScaledPixels(margin.top),
        right: ScaledPixels(margin.right),
        bottom: ScaledPixels(margin.bottom),
        left: ScaledPixels(margin.left),
    });
    let mut limited = bounds.intersect(&reach);
    if limited.is_empty() {
        return None;
    }
    limited.size.width = ScaledPixels(limited.size.width.0.min(MAX_GROUP_EXTENT));
    limited.size.height = ScaledPixels(limited.size.height.0.min(MAX_GROUP_EXTENT));
    Some(limited)
}

#[derive(Clone)]
#[expect(missing_docs)]
pub enum Primitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

#[expect(missing_docs)]
impl Primitive {
    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::SubpixelSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    /// Scales the primitive's alpha, as a group's opacity does when it is drawn flat.
    pub fn apply_opacity(&mut self, opacity: f32) {
        match self {
            Primitive::Shadow(shadow) => shadow.color = shadow.color.opacity(opacity),
            Primitive::Quad(quad) => {
                quad.background = quad.background.opacity(opacity);
                quad.border_colors = quad.border_colors.map(|color| color.opacity(opacity));
                quad.border_inner_colors =
                    quad.border_inner_colors.map(|color| color.opacity(opacity));
            }
            Primitive::Path(path) => path.color = path.color.opacity(opacity),
            Primitive::Underline(underline) => {
                underline.color = underline.color.opacity(opacity)
            }
            Primitive::MonochromeSprite(sprite) => sprite.color = sprite.color.opacity(opacity),
            Primitive::SubpixelSprite(sprite) => sprite.color = sprite.color.opacity(opacity),
            Primitive::PolychromeSprite(sprite) => sprite.opacity *= opacity,
            Primitive::Surface(_) => {}
        }
    }

    /// A glyph as it counts for a text clip: opaque, so it masks by its shape alone.
    fn as_glyph_mask(&self) -> Option<Primitive> {
        match self {
            Primitive::MonochromeSprite(sprite) => {
                let mut sprite = *sprite;
                sprite.color = Hsla {
                    h: 0.,
                    s: 0.,
                    l: 1.,
                    a: 1.,
                };
                Some(Primitive::MonochromeSprite(sprite))
            }
            Primitive::SubpixelSprite(sprite) => {
                let mut sprite = *sprite;
                sprite.color = Hsla {
                    h: 0.,
                    s: 0.,
                    l: 1.,
                    a: 1.,
                };
                Some(Primitive::SubpixelSprite(sprite))
            }
            Primitive::PolychromeSprite(sprite) => Some(Primitive::PolychromeSprite(*sprite)),
            _ => None,
        }
    }

    /// Moves the primitive by `delta`, the way a painter that painted it there would have put it
    /// where it ends up on screen: a sprite's bounds move, and its transformation changes so that
    /// what the transformation does to the moved bounds is the old place moved by `delta`.
    fn translate_replayed(&mut self, delta: Point<ScaledPixels>) {
        match self {
            Primitive::Shadow(shadow) => shadow.translate(delta),
            Primitive::Quad(quad) => quad.translate(delta),
            Primitive::Path(path) => path.translate(delta),
            Primitive::Underline(underline) => underline.translate(delta),
            Primitive::MonochromeSprite(sprite) => {
                translate_bounds(&mut sprite.bounds, delta);
                translate_transformation(&mut sprite.transformation, delta);
                translate_mask(&mut sprite.content_mask, delta);
            }
            Primitive::SubpixelSprite(sprite) => {
                translate_bounds(&mut sprite.bounds, delta);
                translate_transformation(&mut sprite.transformation, delta);
                translate_mask(&mut sprite.content_mask, delta);
            }
            Primitive::PolychromeSprite(sprite) => sprite.translate(delta),
            Primitive::Surface(surface) => surface.translate(delta),
        }
    }

    /// Cuts the primitive by `clip`, as painting under it would have.
    fn clip_to(&mut self, clip: &ReplayClip) {
        match self {
            Primitive::Path(path) => clip_mask(&mut path.content_mask, &clip.plain),
            Primitive::Shadow(shadow) => clip_mask(&mut shadow.content_mask, &clip.snapped),
            Primitive::Quad(quad) => clip_mask(&mut quad.content_mask, &clip.snapped),
            Primitive::Underline(underline) => {
                clip_mask(&mut underline.content_mask, &clip.snapped)
            }
            Primitive::MonochromeSprite(sprite) => {
                clip_mask(&mut sprite.content_mask, &clip.snapped)
            }
            Primitive::SubpixelSprite(sprite) => {
                clip_mask(&mut sprite.content_mask, &clip.snapped)
            }
            Primitive::PolychromeSprite(sprite) => {
                clip_mask(&mut sprite.content_mask, &clip.snapped)
            }
            Primitive::Surface(surface) => clip_mask(&mut surface.content_mask, &clip.snapped),
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::SubpixelSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}


/// The content mask around the place a recording is put in, in device pixels, as the painters
/// that made the recording would have cut it: `snapped` is what most of them cut by (the mask
/// grown to whole pixels), and `plain` what a path is cut by (the mask scaled).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplayClip {
    pub(crate) snapped: Bounds<ScaledPixels>,
    pub(crate) plain: Bounds<ScaledPixels>,
}

/// Cuts `mask` by the plain rectangle `clip`, as [`ContentMask::intersect`] does: a rounded corner
/// stays where the result still has the corner.
fn clip_mask(mask: &mut ContentMask<ScaledPixels>, clip: &Bounds<ScaledPixels>) {
    let bounds = mask.bounds.intersect(clip);
    let keep = |anchor: Anchor| !bounds.is_empty() && mask.bounds.corner(anchor) == bounds.corner(anchor);
    let radii = |corners: &Corners<ScaledPixels>| Corners {
        top_left: if keep(Anchor::TopLeft) { corners.top_left } else { ScaledPixels(0.) },
        top_right: if keep(Anchor::TopRight) { corners.top_right } else { ScaledPixels(0.) },
        bottom_right: if keep(Anchor::BottomRight) {
            corners.bottom_right
        } else {
            ScaledPixels(0.)
        },
        bottom_left: if keep(Anchor::BottomLeft) { corners.bottom_left } else { ScaledPixels(0.) },
    };
    let corner_radii = radii(&mask.corner_radii);
    let corner_radii_y = radii(&mask.corner_radii_y);
    *mask = ContentMask {
        bounds,
        corner_radii,
        corner_radii_y,
    };
}

/// Moves what `transformation` draws by `delta` when its bounds were moved by `delta` too: the
/// shader draws `rotation_scale * bounds + translation`.
fn translate_transformation(transformation: &mut TransformationMatrix, delta: Point<ScaledPixels>) {
    let [[a, b], [c, d]] = transformation.rotation_scale;
    transformation.translation[0] += (1. - a) * delta.x.0 - b * delta.y.0;
    transformation.translation[1] += -c * delta.x.0 + (1. - d) * delta.y.0;
}

fn translate_bounds(bounds: &mut Bounds<ScaledPixels>, delta: Point<ScaledPixels>) {
    bounds.origin = bounds.origin + delta;
}

fn translate_mask(mask: &mut ContentMask<ScaledPixels>, delta: Point<ScaledPixels>) {
    translate_bounds(&mut mask.bounds, delta);
}

#[expect(missing_docs)]
impl Quad {
    pub fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_mask(&mut self.content_mask, delta);
        if self.background_tile.width > 0. {
            self.background_tile.origin_x += delta.x.0;
            self.background_tile.origin_y += delta.y.0;
        }
    }
}

impl Shadow {
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_bounds(&mut self.element_bounds, delta);
        translate_mask(&mut self.content_mask, delta);
    }
}

impl Underline {
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_mask(&mut self.content_mask, delta);
    }
}

impl MonochromeSprite {
    /// Moves through the transformation rather than the bounds, which is where a sprite's
    /// position ends up whatever the rest of the matrix does.
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        self.transformation.translation[0] += delta.x.0;
        self.transformation.translation[1] += delta.y.0;
        translate_mask(&mut self.content_mask, delta);
    }
}

impl SubpixelSprite {
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        self.transformation.translation[0] += delta.x.0;
        self.transformation.translation[1] += delta.y.0;
        translate_mask(&mut self.content_mask, delta);
    }
}

impl PolychromeSprite {
    pub(crate) fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_mask(&mut self.content_mask, delta);
    }
}

impl PaintSurface {
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_mask(&mut self.content_mask, delta);
    }
}

impl Path<ScaledPixels> {
    fn translate(&mut self, delta: Point<ScaledPixels>) {
        translate_bounds(&mut self.bounds, delta);
        translate_mask(&mut self.content_mask, delta);
        for vertex in &mut self.vertices {
            vertex.xy_position = vertex.xy_position + delta;
            translate_mask(&mut vertex.content_mask, delta);
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    subpixel_sprites_start: usize,
    subpixel_sprites_iter: Peekable<slice::Iter<'a, SubpixelSprite>>,
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
    groups_start: usize,
    groups_iter: Peekable<slice::Iter<'a, EffectGroup>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.subpixel_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::SubpixelSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
            (
                self.groups_iter.peek().map(|group| group.order),
                PrimitiveKind::EffectGroup,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(shadows_start..shadows_end))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(quads_start..quads_end))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(paths_start..paths_end))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(underlines_start..underlines_end))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::SubpixelSprite => {
                let texture_id = self.subpixel_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.subpixel_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.subpixel_sprites_iter.next();
                while self
                    .subpixel_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.subpixel_sprites_start = sprites_end;
                Some(PrimitiveBatch::SubpixelSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(surfaces_start..surfaces_end))
            }
            PrimitiveKind::EffectGroup => {
                let groups_start = self.groups_start;
                let mut groups_end = groups_start + 1;
                self.groups_iter.next();
                while self
                    .groups_iter
                    .next_if(|group| (group.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    groups_end += 1;
                }
                self.groups_start = groups_end;
                Some(PrimitiveBatch::EffectGroups(groups_start..groups_end))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
#[allow(missing_docs)]
pub enum PrimitiveBatch {
    Shadows(Range<usize>),
    Quads(Range<usize>),
    Paths(Range<usize>),
    Underlines(Range<usize>),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SubpixelSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    Surfaces(Range<usize>),
    EffectGroups(Range<usize>),
}

impl PrimitiveBatch {
    #[expect(missing_docs)]
    pub fn label(&self) -> String {
        match self {
            Self::Shadows(range) => format!("shadows ({})", range.len()),
            Self::Quads(range) => format!("quads ({})", range.len()),
            Self::Paths(range) => format!("paths ({})", range.len()),
            Self::Underlines(range) => format!("underlines ({})", range.len()),
            Self::MonochromeSprites { texture_id, range } => {
                format!(
                    "monochrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::SubpixelSprites { texture_id, range } => {
                format!(
                    "subpixel sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::PolychromeSprites { texture_id, range } => {
                format!(
                    "polychrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::Surfaces(range) => format!("surfaces ({})", range.len()),
            Self::EffectGroups(range) => format!("effect groups ({})", range.len()),
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Quad {
    pub order: DrawOrder,
    /// What the shaders draw on each side. `Inset` and `Outset` are solid sides with shaded
    /// colors, so they never appear here (see `border_style::resolve_edge`).
    pub border_styles: Edges<BorderStyle>,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_colors: Edges<Hsla>,
    /// The color of the inner half of a `Groove` or `Ridge` side, whose `border_colors` entry
    /// is the outer half. Equal to `border_colors` for every other style.
    pub border_inner_colors: Edges<Hsla>,
    pub corner_radii: Corners<ScaledPixels>,
    /// The vertical radii of the corners; zero for the same as `corner_radii`.
    pub corner_radii_y: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
    pub background_tile: BackgroundTile,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    /// A `SHAPE_*` code of `text_decoration.rs`.
    pub style: u32,
    /// For the dotted and dashed shapes: the length of a mark and the distance between
    /// the starts of two marks.
    pub mark_length: ScaledPixels,
    pub mark_period: ScaledPixels,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    /// The vertical radii of the corners; zero for the same as `corner_radii`.
    pub corner_radii_y: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub element_bounds: Bounds<ScaledPixels>,
    pub element_corner_radii: Corners<ScaledPixels>,
    /// The vertical radii of the element's corners; zero for the same as `element_corner_radii`.
    pub element_corner_radii_y: Corners<ScaledPixels>,
    /// 0 = drop shadow (rendered outside the element), 1 = inset shadow (rendered inside).
    pub inset: u32,
    pub pad: u32, // align to 8 bytes
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of one side of a border, as CSS `border-style` names them. `none` and `hidden`
/// are a width of zero, not a style.
///
/// `shaders.metal`, `shaders.wgsl` and `shaders.hlsl` repeat the numbers of `Dashed` to
/// `Ridge`, so change them together.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// Dashes of two border widths with a gap of one.
    Dashed = 1,
    /// Round dots, one border width across, two apart.
    Dotted = 2,
    /// Two lines with a gap between them, a third of the border width each.
    Double = 3,
    /// The outer half is shaded like an `Inset` border and the inner half like an `Outset` one.
    Groove = 4,
    /// The outer half is shaded like an `Outset` border and the inner half like an `Inset` one.
    Ridge = 5,
    /// The top and left sides are darker than the color, the bottom and right ones lighter.
    Inset = 6,
    /// The bottom and right sides are darker than the color, the top and left ones lighter.
    Outset = 7,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(mut self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct SubpixelSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
}

impl From<SubpixelSprite> for Primitive {
    fn from(sprite: SubpixelSprite) -> Self {
        Primitive::SubpixelSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PolychromeSprite {
    pub order: DrawOrder,
    pub nearest_neighbor: PaddedBool32,
    pub grayscale: PaddedBool32,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    /// The vertical radii of the corners; zero for the same as `corner_radii`.
    pub corner_radii_y: Corners<ScaledPixels>,
    pub tile: AtlasTile,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone)]
#[allow(missing_docs)]
pub struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(target_os = "macos")]
    pub image_buffer: core_video::pixel_buffer::CVPixelBuffer,
    /// Type-erased GPU texture (`Arc<wgpu::Texture>`). Ported from gpui-ce
    /// ([#39](https://github.com/gpui-ce/gpui-ce/commit/6d043b22e477)).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub texture: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub texture_size: Size<crate::DevicePixels>,
}

impl std::fmt::Debug for PaintSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PaintSurface");
        debug
            .field("order", &self.order)
            .field("bounds", &self.bounds)
            .field("content_mask", &self.content_mask);
        #[cfg(target_os = "macos")]
        debug.field("image_buffer", &self.image_buffer);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        debug.field("texture_size", &self.texture_size);
        debug.finish_non_exhaustive()
    }
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[expect(missing_docs)]
pub struct PathId(pub usize);

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug)]
#[expect(missing_docs)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub id: PathId,
    pub order: DrawOrder,
    pub bounds: Bounds<P>,
    pub content_mask: ContentMask<P>,
    pub vertices: Vec<PathVertex<P>>,
    pub color: Background,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            contour_count: 0,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    #[expect(missing_docs)]
    pub fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub xy_position: Point<P>,
    pub st_position: Point<f32>,
    pub content_mask: ContentMask<P>,
}

#[expect(missing_docs)]
impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AtlasTextureKind, GroupEffect, ResolvedFilter, TileId, TransformationMatrix, red, size,
    };

    const WINDOW: f32 = 400.;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn mask() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: bounds(0., 0., WINDOW, WINDOW),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
        }
    }

    fn quad(x: f32, y: f32, width: f32, height: f32) -> Quad {
        Quad {
            bounds: bounds(x, y, width, height),
            content_mask: mask(),
            background: red().into(),
            ..Quad::default()
        }
    }

    fn glyph(x: f32, y: f32) -> MonochromeSprite {
        MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: bounds(x, y, 8., 8.),
            content_mask: mask(),
            color: Hsla {
                h: 0.,
                s: 0.,
                l: 0.,
                a: 0.,
            },
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                tile_id: TileId(0),
                padding: 0,
                bounds: Bounds::default(),
            },
            transformation: TransformationMatrix::unit(),
        }
    }

    fn scene() -> Scene {
        let mut scene = Scene::default();
        scene.set_effect_groups_enabled(true);
        scene
    }

    fn fade(opacity: f32) -> GroupEffect {
        GroupEffect {
            opacity,
            ..GroupEffect::default()
        }
    }

    fn blurred() -> GroupEffect {
        GroupEffect {
            filters: vec![ResolvedFilter::Blur(2.)],
            ..GroupEffect::default()
        }
    }

    #[test]
    fn a_group_keeps_its_primitives_in_a_scene_of_its_own() {
        let mut scene = scene();
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(quad(20., 30., 10., 10.));
        scene.insert_primitive(quad(25., 35., 10., 10.));
        scene.end_group();
        scene.finish();

        assert!(scene.quads.is_empty());
        assert_eq!(scene.groups.len(), 1);
        let group = &scene.groups[0];
        assert_eq!(group.bounds, bounds(20., 30., 15., 15.));
        // The group's content is drawn from its own origin.
        let inner: Vec<_> = group.scene.quads.iter().map(|quad| quad.bounds.origin).collect();
        assert_eq!(
            inner,
            vec![
                point(ScaledPixels(0.), ScaledPixels(0.)),
                point(ScaledPixels(5.), ScaledPixels(5.))
            ]
        );
        // Overlapping content is ordered inside the group, not against the rest of the scene.
        assert!(group.scene.quads[1].order > group.scene.quads[0].order);
    }

    #[test]
    fn filters_grow_the_group_beyond_its_content() {
        let mut scene = scene();
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(50., 50., 20., 20.));
        scene.end_group();
        // Two sigma of 2 reach 6 pixels each way.
        assert_eq!(scene.groups[0].bounds, bounds(44., 44., 32., 32.));
        assert_eq!(
            scene.groups[0].scene.quads[0].bounds.origin,
            point(ScaledPixels(6.), ScaledPixels(6.))
        );
    }

    #[test]
    fn a_fade_over_a_single_primitive_needs_no_group() {
        let mut scene = scene();
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.end_group();

        assert!(scene.groups.is_empty());
        assert_eq!(scene.quads.len(), 1);
        assert_eq!(scene.quads[0].bounds, bounds(10., 10., 20., 20.));
        assert_eq!(scene.quads[0].background.as_solid().unwrap().a, 0.5);
    }

    #[test]
    fn a_group_kept_for_its_clip_stays_a_group_around_a_single_primitive() {
        let mut scene = scene();
        let clip = GroupEffect {
            exact_clip: true,
            ..GroupEffect::default()
        };
        let mut inner = quad(10., 10., 20., 20.);
        inner.content_mask.bounds = bounds(0., 0., 300., 300.);
        scene.start_group(clip, narrow(), None);
        scene.insert_primitive(inner);
        scene.end_group();

        assert!(scene.quads.is_empty());
        assert_eq!(scene.groups.len(), 1);
        // The group composites under the mask it started with, the primitive keeps its own.
        assert_eq!(scene.groups[0].content_mask, narrow());
        assert_eq!(
            scene.groups[0].scene.quads[0].content_mask.bounds,
            bounds(-10., -10., 300., 300.)
        );
    }

    #[test]
    fn two_overlapping_primitives_make_a_real_group() {
        let mut scene = scene();
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.insert_primitive(quad(15., 15., 20., 20.));
        scene.end_group();
        assert_eq!(scene.groups.len(), 1);
        // Each primitive keeps its own alpha: the fade is applied to the composite.
        assert_eq!(scene.groups[0].scene.quads[0].background.as_solid().unwrap().a, 1.);
    }

    #[test]
    fn empty_groups_paint_nothing_unless_they_filter_the_backdrop() {
        let mut scene = scene();
        scene.start_group(blurred(), mask(), None);
        scene.end_group();
        assert!(scene.groups.is_empty());

        let backdrop = GroupEffect {
            backdrop_filters: vec![ResolvedFilter::Blur(2.)],
            reference: bounds(10., 10., 50., 50.),
            ..GroupEffect::default()
        };
        scene.start_group(backdrop, mask(), None);
        scene.end_group();
        assert_eq!(scene.groups.len(), 1);
        assert_eq!(scene.groups[0].bounds, bounds(10., 10., 50., 50.));
    }

    #[test]
    fn a_group_is_ordered_by_what_it_covers() {
        let mut scene = scene();
        scene.insert_primitive(quad(0., 0., 100., 100.));
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(40., 40., 20., 20.));
        scene.end_group();
        scene.insert_primitive(quad(45., 45., 10., 10.));
        scene.finish();

        let below = scene.quads.iter().find(|quad| quad.bounds.size.width.0 == 100.).unwrap();
        let above = scene.quads.iter().find(|quad| quad.bounds.size.width.0 == 10.).unwrap();
        let group = &scene.groups[0];
        assert!(below.order < group.order);
        assert!(group.order < above.order);
    }

    #[test]
    fn a_reserved_group_stays_below_what_is_painted_after_it() {
        let mut scene = scene();
        let region = bounds(0., 0., 100., 100.);
        scene.start_group(blurred(), mask(), Some(region));
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.end_group();
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.finish();

        assert!(scene.groups[0].order < scene.quads[0].order);
    }

    #[test]
    fn nested_groups_composite_into_the_group_around_them() {
        let mut scene = scene();
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(100., 100., 40., 40.));
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(110., 110., 10., 10.));
        scene.insert_primitive(quad(112., 112., 10., 10.));
        scene.end_group();
        scene.end_group();

        assert_eq!(scene.groups.len(), 1);
        let outer = &scene.groups[0];
        assert_eq!(outer.scene.groups.len(), 1);
        assert_eq!(outer.scene.quads.len(), 1);
        let inner = &outer.scene.groups[0];
        assert_eq!(inner.scene.quads.len(), 2);
        // The inner group's position is relative to the outer group's origin.
        assert_eq!(
            inner.bounds.origin.x.0 + outer.bounds.origin.x.0,
            110. - 6.
        );
        // The outer group covers the inner one and the margin around both.
        assert_eq!(outer.bounds, bounds(94., 94., 52., 52.));
    }

    #[test]
    fn replaying_operations_rebuilds_the_same_groups() {
        let mut previous = scene();
        previous.insert_primitive(quad(0., 0., 10., 10.));
        let start = previous.len();
        previous.start_group(blurred(), mask(), None);
        previous.insert_primitive(quad(40., 40., 20., 20.));
        previous.insert_primitive(quad(45., 45., 20., 20.));
        previous.end_group();
        let end = previous.len();
        previous.finish();

        let mut next = scene();
        next.replay(start..end, &previous);
        next.finish();

        assert!(next.quads.is_empty());
        assert_eq!(next.groups.len(), 1);
        assert_eq!(next.groups[0].bounds, previous.groups[0].bounds);
        assert_eq!(next.groups[0].scene.quads.len(), 2);
        assert_eq!(next.len(), end - start);
    }

    fn narrow() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: bounds(0., 0., 50., 50.),
            ..mask()
        }
    }

    fn painted_around_an_escaped_quad(scene: &mut Scene) {
        scene.start_group(fade(0.5), narrow(), None);
        let mut inside = quad(10., 10., 20., 20.);
        inside.content_mask = narrow();
        scene.insert_primitive(inside);
        scene.widen_groups(mask());
        scene.insert_primitive(quad(100., 100., 20., 20.));
        scene.end_group();
    }

    #[test]
    fn a_group_widens_to_the_mask_of_an_escaped_primitive() {
        let mut scene = scene();
        painted_around_an_escaped_quad(&mut scene);
        scene.finish();

        assert_eq!(scene.groups[0].bounds, bounds(10., 10., 110., 110.));
        assert_eq!(scene.groups[0].content_mask.bounds, mask().bounds);
    }

    #[test]
    fn a_group_that_is_not_widened_stays_cut_to_its_start_mask() {
        let mut scene = scene();
        scene.start_group(fade(0.5), narrow(), None);
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.insert_primitive(quad(100., 100., 20., 20.));
        scene.end_group();

        assert_eq!(scene.groups[0].bounds, bounds(10., 10., 40., 40.));
        assert_eq!(scene.groups[0].content_mask, narrow());
    }

    #[test]
    fn a_replayed_group_is_widened_too() {
        let mut previous = scene();
        painted_around_an_escaped_quad(&mut previous);
        let end = previous.len();
        previous.finish();

        let mut next = scene();
        next.replay(0..end, &previous);
        next.finish();

        assert_eq!(next.groups[0].bounds, previous.groups[0].bounds);
        assert_eq!(next.groups[0].content_mask, previous.groups[0].content_mask);
    }

    #[test]
    fn without_group_support_the_opacity_is_applied_per_primitive() {
        let mut scene = Scene::default();
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(quad(0., 0., 10., 10.));
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(quad(20., 0., 10., 10.));
        scene.end_group();
        scene.end_group();
        scene.insert_primitive(quad(40., 0., 10., 10.));

        assert!(scene.groups.is_empty());
        let alphas: Vec<f32> = scene
            .quads
            .iter()
            .map(|quad| quad.background.as_solid().unwrap().a)
            .collect();
        assert_eq!(alphas, vec![0.5, 0.25, 1.]);
    }

    #[test]
    fn text_clips_keep_a_copy_of_the_glyphs_for_the_group_before_them() {
        let mut scene = scene();
        let region = bounds(0., 0., 100., 50.);
        scene.start_text_clip();
        let effect = GroupEffect {
            text_mask: true,
            reference: region,
            ..GroupEffect::default()
        };
        scene.start_group(effect, mask(), Some(region));
        scene.insert_primitive(quad(0., 0., 100., 50.));
        scene.end_group();
        scene.insert_primitive(glyph(10., 10.));
        scene.insert_primitive(glyph(20., 10.));
        scene.end_text_clip();
        // Glyphs painted once the clip is over don't count.
        scene.insert_primitive(glyph(30., 10.));
        scene.finish();

        assert_eq!(scene.monochrome_sprites.len(), 3, "the glyphs still paint normally");
        let mask_scene = scene.groups[0].text_mask.as_ref().expect("the group has a text mask");
        assert_eq!(mask_scene.monochrome_sprites.len(), 2);
        // Opaque, whatever colour the text has.
        assert_eq!(mask_scene.monochrome_sprites[0].color.a, 1.);
    }

    #[test]
    fn a_text_clip_masks_by_an_emoji_but_not_by_an_image() {
        let mut scene = scene();
        let region = bounds(0., 0., 100., 50.);
        scene.start_text_clip();
        let effect = GroupEffect {
            text_mask: true,
            reference: region,
            ..GroupEffect::default()
        };
        scene.start_group(effect, mask(), Some(region));
        scene.insert_primitive(quad(0., 0., 100., 50.));
        scene.end_group();
        let sprite = PolychromeSprite {
            order: 0,
            nearest_neighbor: false.into(),
            grayscale: false.into(),
            opacity: 1.,
            bounds: bounds(10., 10., 8., 8.),
            content_mask: mask(),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
            tile: glyph(0., 0.).tile,
        };
        scene.insert_primitive(sprite);
        scene.insert_image(sprite);
        scene.end_text_clip();
        scene.finish();

        assert_eq!(scene.polychrome_sprites.len(), 2, "both paint");
        let mask_scene = scene.groups[0].text_mask.as_ref().expect("the group has a text mask");
        assert_eq!(mask_scene.polychrome_sprites.len(), 1);
    }

    #[test]
    fn groups_are_batched_in_order_with_the_primitives() {
        let mut scene = scene();
        scene.insert_primitive(quad(0., 0., 100., 100.));
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(40., 40., 20., 20.));
        scene.end_group();
        scene.insert_primitive(quad(45., 45., 10., 10.));
        scene.finish();

        let batches: Vec<String> = scene.batches().map(|batch| batch.label()).collect();
        assert_eq!(
            batches,
            vec!["quads (1)", "effect groups (1)", "quads (1)"]
        );
    }

    #[test]
    fn painted_quads_include_the_ones_inside_groups() {
        let mut scene = scene();
        scene.start_group(blurred(), mask(), None);
        scene.insert_primitive(quad(100., 120., 20., 20.));
        scene.end_group();
        let quads = scene.quads_with_groups();
        assert_eq!(quads.len(), 1);
        assert_eq!(quads[0].bounds, bounds(100., 120., 20., 20.));
    }

    #[test]
    fn a_hidden_scene_drops_primitives_and_replays_what_was_recorded() {
        let mut recorded = scene();
        recorded.insert_primitive(quad(0., 0., 10., 10.));
        recorded.set_hidden(true);
        recorded.insert_primitive(quad(20., 0., 10., 10.));
        recorded.set_hidden(false);
        recorded.insert_primitive(quad(40., 0., 10., 10.));
        assert_eq!(recorded.quads.len(), 2);

        let mut replayed = scene();
        replayed.set_hidden(true);
        replayed.replay(0..recorded.len(), &recorded);
        assert!(replayed.hidden, "replay leaves the flag as it found it");
        replayed.set_hidden(false);
        assert_eq!(replayed.quads.len(), 2);
    }

    #[test]
    fn a_huge_backdrop_box_is_cut_to_the_content_mask() {
        let mut scene = scene();
        let backdrop = GroupEffect {
            backdrop_filters: vec![ResolvedFilter::Blur(crate::MAX_BLUR_SIGMA)],
            reference: bounds(-1.0e7, -1.0e7, 2.0e7, 2.0e7),
            ..GroupEffect::default()
        };
        scene.start_group(backdrop, mask(), None);
        scene.end_group();
        assert_eq!(scene.groups.len(), 1);
        assert_eq!(scene.groups[0].bounds, bounds(0., 0., WINDOW, WINDOW));
    }

    #[test]
    fn a_huge_blur_only_reaches_a_bounded_margin() {
        let mut scene = scene();
        let effect = GroupEffect {
            filters: vec![crate::FilterFunction::Blur(crate::px(1.0e9)).resolve(1.)],
            ..GroupEffect::default()
        };
        scene.start_group(effect, mask(), None);
        scene.insert_primitive(quad(0., 0., WINDOW, WINDOW));
        scene.end_group();
        let bounds = scene.groups[0].bounds;
        let reach = 3. * crate::MAX_BLUR_SIGMA;
        assert_eq!(bounds.origin.x.0, -reach);
        assert_eq!(bounds.size.width.0, WINDOW + 2. * reach);
        assert!(bounds.size.height.0 <= crate::MAX_GROUP_EXTENT);
    }

    #[test]
    fn a_group_never_exceeds_the_largest_texture() {
        let mut scene = scene();
        let wide = ContentMask {
            bounds: bounds(0., 0., 1.0e6, 1.0e6),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
        };
        let mut huge = quad(0., 0., 1.0e6, 1.0e6);
        huge.content_mask = wide;
        scene.start_group(blurred(), wide, None);
        scene.insert_primitive(huge);
        scene.end_group();
        let bounds = scene.groups[0].bounds;
        assert_eq!(bounds.size.width.0, crate::MAX_GROUP_EXTENT);
        assert_eq!(bounds.size.height.0, crate::MAX_GROUP_EXTENT);
    }

    #[test]
    fn a_fully_faded_group_paints_nothing_even_over_a_backdrop() {
        let mut scene = scene();
        let effect = GroupEffect {
            opacity: 0.,
            backdrop_filters: vec![ResolvedFilter::Blur(2.)],
            reference: bounds(10., 10., 50., 50.),
            ..GroupEffect::default()
        };
        scene.start_group(effect, mask(), None);
        scene.insert_primitive(quad(10., 10., 20., 20.));
        scene.insert_primitive(quad(15., 15., 20., 20.));
        scene.end_group();
        scene.finish();
        assert!(scene.groups.is_empty());
        assert!(scene.quads.is_empty());
    }

    fn clip(x: f32, y: f32, width: f32, height: f32) -> ReplayClip {
        ReplayClip {
            snapped: bounds(x, y, width, height),
            plain: bounds(x, y, width, height),
        }
    }

    fn masked(content_mask: ContentMask<ScaledPixels>, shift: f32, clip: &Bounds<ScaledPixels>) -> ContentMask<ScaledPixels> {
        let mut mask = content_mask;
        translate_mask(&mut mask, point(ScaledPixels(0.), ScaledPixels(shift)));
        clip_mask(&mut mask, clip);
        mask
    }

    fn describe(scene: &Scene) -> String {
        scene.describe_operations(0..scene.len(), false)
    }

    #[test]
    fn a_translated_replay_is_the_scene_painted_where_it_lands() {
        let region = ContentMask {
            bounds: bounds(0., 0., WINDOW, 3. * WINDOW),
            ..mask()
        };
        let mut recorded = scene();
        let mut rows = [quad(10., 10., 50., 20.), quad(10., 120., 50., 20.), quad(10., 700., 50., 20.)];
        for row in &mut rows {
            row.content_mask = region;
            recorded.insert_quad(*row);
        }
        recorded.push_layer(bounds(0., 100., 100., 100.));
        let mut sprite = glyph(20., 130.);
        sprite.content_mask = region;
        recorded.insert_primitive(sprite);
        recorded.pop_layer();
        recorded.push_layer(bounds(0., 800., 100., 100.));
        recorded.pop_layer();

        // The viewport slid up by 100 px: what was at 120 is at 20.
        let viewport = clip(0., 0., WINDOW, 150.);
        let mut replayed = scene();
        replayed.replay_translated(
            &recorded.paint_operations,
            point(ScaledPixels(0.), ScaledPixels(-100.)),
            &viewport,
        );

        let mut painted = scene();
        for row in &rows {
            let mut row = *row;
            row.bounds.origin.y.0 -= 100.;
            row.content_mask = masked(region, -100., &viewport.snapped);
            painted.insert_quad(row);
        }
        painted.push_layer(bounds(0., 0., 100., 100.));
        let mut sprite = glyph(20., 30.);
        sprite.content_mask = masked(region, -100., &viewport.snapped);
        painted.insert_primitive(sprite);
        painted.pop_layer();

        assert_eq!(describe(&replayed), describe(&painted));
        assert_eq!(replayed.quads.len(), 1, "the rows at 10 and 700 are clipped away");
        replayed.finish();
        painted.finish();
        let orders = |scene: &Scene| scene.quads.iter().map(|quad| quad.order).collect::<Vec<_>>();
        assert_eq!(orders(&replayed), orders(&painted));
    }

    #[test]
    fn a_translated_sprite_keeps_its_place_under_its_transformation() {
        let rotated = TransformationMatrix::unit().rotate(crate::Radians(0.5)).scale(size(2., 3.));
        let draws_at = |sprite: &MonochromeSprite| {
            let origin = sprite.bounds.origin;
            sprite.transformation.apply(Point::new(origin.x.0.into(), origin.y.0.into()).map(|v: crate::Pixels| v))
        };
        let mut recorded = scene();
        let mut sprite = glyph(10., 10.);
        sprite.transformation = rotated;
        recorded.insert_primitive(sprite);

        let delta = point(ScaledPixels(7.), ScaledPixels(-4.));
        let mut replayed = scene();
        replayed.replay_translated(&recorded.paint_operations, delta, &clip(-100., -100., 1000., 1000.));
        let moved = replayed.monochrome_sprites[0];
        let before = draws_at(&sprite);
        let after = draws_at(&moved);
        assert!((after.x.0 - before.x.0 - delta.x.0).abs() < 1e-4);
        assert!((after.y.0 - before.y.0 - delta.y.0).abs() < 1e-4);
    }

    #[test]
    fn a_translated_replay_clips_inside_the_exact_group_by_its_own_mask_only() {
        let inner = ContentMask {
            bounds: bounds(0., 0., 500., 500.),
            ..mask()
        };
        let mut recorded = scene();
        recorded.start_group(
            GroupEffect {
                exact_clip: true,
                ..GroupEffect::default()
            },
            mask(),
            None,
        );
        recorded.begin_replaced(false);
        let mut inside = quad(10., 10., 20., 20.);
        inside.content_mask = inner;
        recorded.insert_quad(inside);
        recorded.end_replaced();
        recorded.end_group();

        let mut replayed = scene();
        replayed.replay_translated(
            &recorded.paint_operations,
            point(ScaledPixels(0.), ScaledPixels(0.)),
            &clip(0., 0., 100., 100.),
        );
        assert_eq!(replayed.groups.len(), 1);
        assert_eq!(replayed.groups[0].content_mask.bounds, bounds(0., 0., 100., 100.));
        assert_eq!(replayed.groups[0].scene.quads[0].content_mask.bounds.size.width.0 > 0., true);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_fade_over_a_lone_surface_keeps_the_group() {
        use core_video::pixel_buffer::CVPixelBuffer;
        let image_buffer = CVPixelBuffer::new(
            core_video::pixel_buffer::kCVPixelFormatType_32BGRA,
            4,
            4,
            None,
        )
        .unwrap();
        let mut scene = scene();
        scene.start_group(fade(0.5), mask(), None);
        scene.insert_primitive(PaintSurface {
            order: 0,
            bounds: bounds(10., 10., 20., 20.),
            content_mask: mask(),
            image_buffer,
        });
        scene.end_group();
        assert_eq!(scene.groups.len(), 1);
        assert!(scene.surfaces.is_empty());
        assert_eq!(scene.groups[0].scene.surfaces.len(), 1);
    }
}
