//! The damage of a picture: the part of what a window shows that differs from the scene the platform
//! drew last, so that a platform that can (Metal) draws that part and keeps the rest. Every window
//! has it, and there is no switch.
//!
//! The rendered scene is the picture. A frame built whole replaces it, so it damages all of it. A
//! patch, a scroll translate and a repaint of a scrollbar's thumb write over the scene they follow,
//! and each says what it wrote over before it does: the area of every operation that was and the one
//! that is, which is where a pixel can differ. The damage is a rectangle, the union of those areas,
//! and it is kept until a presentation takes it.
//!
//! A scroll translate can say more: that what the picture shows inside a rectangle is what it showed,
//! moved (a [`Shift`]). Then the platform copies those pixels and draws only what is not a copy:
//! the areas that are written over, the areas where what was wrong in the picture before moves to,
//! and what scrolled in. The ledger keeps the areas apart while it holds a shift, and adds up the
//! shifts of one region that come before a presentation; a shift it cannot keep (another region,
//! nothing left of the region to copy, or so much to draw that the copy does not pay) is given up
//! for the rectangle that holds everything, as if the picture was not shifted.
//!
//! A change that no site says is seen anyway: every rebuild of the rendered scene is a `clear`
//! (see [`Scene::revision`]), the ledger knows the revision it was told of, and a rebuild it was not
//! told of makes the damage the whole picture. So does anything in the scene whose pixels are not
//! drawn inside a box: an effect group, which reads and writes what is around it, a native surface
//! and a surface of video frames.

use crate::scene::Extent;
use crate::{Bounds, DevicePixels, PaintOperation, Point, ScaledPixels, Scene, Size, point, size};
use smallvec::SmallVec;

/// How far past the area of an operation the damage reaches, in device pixels: antialiased edges
/// are inside the bounds the shaders draw in, and this is for whatever rounding is left.
const MARGIN: i32 = 2;

/// How many rectangles a frame is drawn in, at most, when part of it is a shifted copy of the
/// picture before: each is a pass over the scene, and past this the copy does not pay for them.
const MAX_RECTS: usize = 4;

/// How much of the shifted region the rectangles drawn can be, at most, for the copy to be kept.
const MAX_DRAWN_SHARE: f32 = 0.6;

/// How many separate areas the ledger holds beside a shift before it holds their bounds instead,
/// so that a frame that changed many rows does not grow the list without end.
const MAX_RESIDUAL: usize = 16;

/// Pixels of the picture before that the picture now holds again, moved: a scrolled list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shift {
    /// Where the pixels are moved in: whole pixels, never beyond the window.
    pub region: Bounds<DevicePixels>,
    /// How far they are moved.
    pub by: Point<DevicePixels>,
}

impl Shift {
    /// Where the picture now equals the picture before, moved by `by` (except inside the rectangles
    /// of the damage that carries this shift): `region` and the pixels that `region` moved by `by`
    /// has in it.
    pub fn kept(&self) -> Bounds<DevicePixels> {
        IntRect::of(self.region).intersect(&IntRect::of(self.region).moved(self.by.x.0, self.by.y.0)).bounds()
    }
}

/// What a platform is told to draw of a frame: `None` in its place is all of the picture.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameDamage {
    /// Pixels the picture before holds, moved (see [`Shift`]), so that the platform copies them
    /// instead of drawing them: every pixel of [`Shift::kept`] is the pixel of the picture before
    /// that is `by` away, except inside `rects`.
    pub shift: Option<Shift>,
    /// The parts of the scene to draw, in device pixels inside the window, that do not overlap. None
    /// at all is a picture that did not change.
    pub rects: SmallVec<[Bounds<DevicePixels>; MAX_RECTS]>,
}

impl FrameDamage {
    /// The damage of one rectangle, none of it if the rectangle is empty.
    pub fn rect(area: Bounds<DevicePixels>) -> Self {
        let mut rects = SmallVec::new();
        if !IntRect::of(area).is_empty() {
            rects.push(area);
        }
        Self { shift: None, rects }
    }

    /// A damage of the parts given, as they are, for the tests of a platform.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_parts(shift: Option<Shift>, rects: &[Bounds<DevicePixels>]) -> Self {
        Self { shift, rects: rects.iter().copied().collect() }
    }

    /// The smallest rectangle that holds every rectangle drawn (empty if none is), not the copy.
    pub fn bounds(&self) -> Bounds<DevicePixels> {
        self.rects
            .iter()
            .map(|area| IntRect::of(*area))
            .reduce(|one, other| one.union(&other))
            .map_or_else(Bounds::default, |area| area.bounds())
    }
}

/// A rectangle of whole pixels as its four edges, which is what the damage is made of.
#[derive(Clone, Copy, Debug, PartialEq)]
struct IntRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl IntRect {
    fn of(area: Bounds<DevicePixels>) -> Self {
        Self {
            left: area.origin.x.0,
            top: area.origin.y.0,
            right: area.origin.x.0 + area.size.width.0,
            bottom: area.origin.y.0 + area.size.height.0,
        }
    }

    fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    fn intersect(&self, other: &Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }

    fn intersects(&self, other: &Self) -> bool {
        !self.intersect(other).is_empty()
    }

    fn union(&self, other: &Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    fn moved(&self, x: i32, y: i32) -> Self {
        Self { left: self.left + x, top: self.top + y, right: self.right + x, bottom: self.bottom + y }
    }

    fn grown(&self, by: i32) -> Self {
        Self { left: self.left - by, top: self.top - by, right: self.right + by, bottom: self.bottom + by }
    }

    fn area(&self) -> i64 {
        i64::from(self.right - self.left) * i64::from(self.bottom - self.top)
    }

    /// An empty rectangle is the default bounds, whatever edges it has.
    fn bounds(&self) -> Bounds<DevicePixels> {
        if self.is_empty() {
            return Bounds::default();
        }
        Bounds {
            origin: point(DevicePixels(self.left), DevicePixels(self.top)),
            size: size(DevicePixels(self.right - self.left), DevicePixels(self.bottom - self.top)),
        }
    }

    /// The area as the damage has it: past the area by [`MARGIN`], inside `window`, none if that is empty.
    fn of_area(area: Bounds<ScaledPixels>, window: &Self) -> Option<Self> {
        let reached = Self {
            left: area.left().0.floor() as i32,
            top: area.top().0.floor() as i32,
            right: area.right().0.ceil() as i32,
            bottom: area.bottom().0.ceil() as i32,
        };
        reached.within(window)
    }

    /// This rectangle past its area by [`MARGIN`], inside `window`, none if that is empty.
    fn within(&self, window: &Self) -> Option<Self> {
        let reached = self.grown(MARGIN).intersect(window);
        (!reached.is_empty()).then_some(reached)
    }

    fn to_scaled(&self) -> Bounds<ScaledPixels> {
        Bounds {
            origin: point(ScaledPixels(self.left as f32), ScaledPixels(self.top as f32)),
            size: size(ScaledPixels((self.right - self.left) as f32), ScaledPixels((self.bottom - self.top) as f32)),
        }
    }
}

/// The rectangle a list scrolls in (its clip) as whole pixels, from both sides: `inner` holds only
/// pixels the clip covers whole, which a copy may move, and `outer` every pixel it covers in part.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Region {
    inner: IntRect,
    outer: IntRect,
}

impl Region {
    /// None if no whole pixel is inside `clip`.
    fn of(clip: Bounds<ScaledPixels>) -> Option<Self> {
        let inner = IntRect {
            left: clip.left().0.ceil() as i32,
            top: clip.top().0.ceil() as i32,
            right: clip.right().0.floor() as i32,
            bottom: clip.bottom().0.floor() as i32,
        };
        let outer = IntRect {
            left: clip.left().0.floor() as i32,
            top: clip.top().0.floor() as i32,
            right: clip.right().0.ceil() as i32,
            bottom: clip.bottom().0.ceil() as i32,
        };
        (!inner.is_empty()).then_some(Self { inner, outer })
    }
}

/// [`Region`] as the areas a scroll translate tests operations against.
pub(crate) struct ShiftRegion {
    /// The pixels the clip covers whole: where a copy moves pixels.
    pub(crate) inner: Bounds<ScaledPixels>,
    /// The pixels the clip covers in part too.
    pub(crate) outer: Bounds<ScaledPixels>,
}

impl ShiftRegion {
    /// None if no whole pixel is inside `clip`.
    pub(crate) fn of(clip: Bounds<ScaledPixels>) -> Option<Self> {
        Region::of(clip).map(|region| Self { inner: region.inner.to_scaled(), outer: region.outer.to_scaled() })
    }
}

/// A shift of the picture that no presentation has shown yet.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingShift {
    region: Region,
    by: (i32, i32),
}

/// What differs from the picture the platform drew last, apart from the shift if there is one.
#[derive(Clone, Debug, Default, PartialEq)]
struct Area {
    shift: Option<PendingShift>,
    /// Where the picture can differ from the picture before and, with a shift, from that moved: the
    /// areas of what changed, one rectangle that holds all of them if there is no shift, since
    /// without one a rectangle per area is what a platform has no use for.
    residual: SmallVec<[Bounds<ScaledPixels>; 4]>,
}

#[derive(Clone, Debug, PartialEq)]
enum Damage {
    /// Not known: all of the picture.
    Full,
    /// Inside this area, or nowhere.
    Area(Area),
}

impl Area {
    fn add(&mut self, area: Bounds<ScaledPixels>) {
        if self.shift.is_none() {
            match self.residual.first_mut() {
                Some(held) => *held = held.union(&area),
                None => self.residual.push(area),
            }
        } else {
            self.add_apart(area);
        }
    }

    /// Adds `area` to the list, as one rectangle with every one of the list that it overlaps.
    fn add_apart(&mut self, mut area: Bounds<ScaledPixels>) {
        while let Some(at) = self.residual.iter().position(|held| held.intersects(&area)) {
            area = area.union(&self.residual.swap_remove(at));
        }
        self.residual.push(area);
        if self.residual.len() > MAX_RESIDUAL {
            let all = self.residual.iter().copied().reduce(|one, other| one.union(&other)).expect("the list is not empty");
            self.residual.clear();
            self.residual.push(all);
        }
    }

    fn translated(area: &Bounds<ScaledPixels>, by: (i32, i32)) -> Bounds<ScaledPixels> {
        Bounds { origin: area.origin + point(ScaledPixels(by.0 as f32), ScaledPixels(by.1 as f32)), size: area.size }
    }

    /// The pixels of `clip` move by `by`.
    fn add_shift(&mut self, clip: Bounds<ScaledPixels>, by: Point<DevicePixels>) {
        let by = (by.x.0, by.y.0);
        if by == (0, 0) {
            return;
        }
        match (self.shift, Region::of(clip)) {
            (None, Some(region)) => {
                // What was wrong in the picture before is wrong where it moves to.
                let held = std::mem::take(&mut self.residual);
                for area in held {
                    self.add_apart(area);
                    self.add_apart(Self::translated(&area, by));
                }
                self.shift = Some(PendingShift { region, by });
            }
            (Some(pending), Some(region)) if pending.region == region => {
                for area in self.residual.iter_mut() {
                    *area = Self::translated(area, by);
                }
                self.shift = Some(PendingShift { region, by: (pending.by.0 + by.0, pending.by.1 + by.1) });
            }
            _ => {
                self.keep_the_bounds();
                self.add(clip);
            }
        }
    }

    /// Gives up the shift: the damage is what a frame with no shift would have, the rectangle that
    /// holds the pixels it covered and everything else.
    fn keep_the_bounds(&mut self) {
        let Some(pending) = self.shift.take() else { return };
        let all = self.residual.drain(..).fold(pending.region.outer.to_scaled(), |all, area| all.union(&area));
        self.residual.push(all);
    }

    fn frame_damage(&self, viewport: Size<DevicePixels>) -> FrameDamage {
        let window = IntRect { left: 0, top: 0, right: viewport.width.0, bottom: viewport.height.0 };
        match self.shift.filter(|pending| pending.by != (0, 0)) {
            Some(pending) => self.shifted(pending, &window).unwrap_or_else(|| {
                let mut plain = self.clone();
                plain.keep_the_bounds();
                plain.frame_damage(viewport)
            }),
            None => FrameDamage { shift: None, rects: Self::apart(self.residual.iter().filter_map(|area| IntRect::of_area(*area, &window))) },
        }
    }

    /// The rectangles of `areas` where those that overlap are one.
    fn apart(areas: impl Iterator<Item = IntRect>) -> SmallVec<[Bounds<DevicePixels>; MAX_RECTS]> {
        let mut apart: SmallVec<[IntRect; MAX_RECTS]> = SmallVec::new();
        for mut area in areas {
            while let Some(at) = apart.iter().position(|held| held.intersects(&area)) {
                area = area.union(&apart.swap_remove(at));
            }
            apart.push(area);
        }
        apart.sort_by_key(|area| (area.top, area.left));
        apart.iter().map(IntRect::bounds).collect()
    }

    /// None where the shift does not pay: nothing of the region stays, or what is left to draw is
    /// in too many rectangles or most of the region.
    fn shifted(&self, pending: PendingShift, window: &IntRect) -> Option<FrameDamage> {
        let inner = pending.region.inner.intersect(window);
        let outer = pending.region.outer.intersect(window);
        let kept = inner.intersect(&inner.moved(pending.by.0, pending.by.1));
        if kept.is_empty() {
            return None;
        }
        // What is in the region and not kept: what scrolled in, and the part of the clip that is not
        // a whole pixel. These are exact pixel geometry, so unlike the areas of primitives they get
        // no margin: the margin is for antialiasing around a primitive, and the copy is not one.
        let exposed = [
            IntRect { left: outer.left, top: outer.top, right: outer.right, bottom: kept.top },
            IntRect { left: outer.left, top: kept.bottom, right: outer.right, bottom: outer.bottom },
            IntRect { left: outer.left, top: kept.top, right: kept.left, bottom: kept.bottom },
            IntRect { left: kept.right, top: kept.top, right: outer.right, bottom: kept.bottom },
        ];
        let rects = Self::apart(
            self.residual
                .iter()
                .filter_map(|area| IntRect::of_area(*area, window))
                .chain(exposed.iter().copied().filter(|band| !band.is_empty())),
        );
        let drawn: i64 = rects.iter().map(|area| IntRect::of(*area).area()).sum();
        if rects.len() > MAX_RECTS || drawn as f32 > MAX_DRAWN_SHARE * outer.area() as f32 {
            return None;
        }
        Some(FrameDamage { shift: Some(Shift { region: inner.bounds(), by: point(DevicePixels(pending.by.0), DevicePixels(pending.by.1)) }), rects })
    }
}

pub(crate) struct DamageLedger {
    damage: Damage,
    /// The revision of the rendered scene that every change up to is in `damage`.
    accounted: u64,
}

impl DamageLedger {
    pub(crate) fn new() -> Self {
        Self { damage: Damage::Full, accounted: 0 }
    }

    /// A frame built whole is the rendered scene now: all of the picture differs from the last.
    pub(crate) fn frame_built(&mut self, scene: &Scene) {
        self.damage = Damage::Full;
        self.accounted = scene.revision();
    }

    /// The picture changes in a way that no scene says (an image the atlas holds was written over).
    pub(crate) fn everything(&mut self) {
        self.damage = Damage::Full;
    }

    /// Called before a change to the rendered scene: a rebuild that was not accounted for makes the
    /// damage all of the picture.
    pub(crate) fn begin(&mut self, scene: &Scene) {
        if self.accounted != scene.revision() {
            self.damage = Damage::Full;
        }
    }

    /// Called after a change to the rendered scene whose damage was added.
    pub(crate) fn end(&mut self, scene: &Scene) {
        self.accounted = scene.revision();
    }

    pub(crate) fn add_area(&mut self, area: Bounds<ScaledPixels>) {
        if let Damage::Area(damage) = &mut self.damage {
            damage.add(area);
        }
    }

    /// The pixels inside `clip` (a rectangle in device pixels) are the pixels of the picture before
    /// moved by `by`, except where areas are added to the damage after this. Areas added before it
    /// are in the picture before and move with it. A shift of a region that is not the one a shift
    /// before it moved, or of one that holds no whole pixel, is the area of its clip.
    pub(crate) fn add_shift(&mut self, clip: Bounds<ScaledPixels>, by: Point<DevicePixels>) {
        if let Damage::Area(damage) = &mut self.damage {
            damage.add_shift(clip, by);
        }
    }

    /// `marks` is whether a mark is part of what is written: one that is the only thing that changed
    /// can change how primitives it does not touch are ordered, so it is all of the picture; the marks
    /// of a run that is written whole stand around primitives that are in the damage themselves.
    fn add_extent(&mut self, extent: Extent, marks: bool) {
        match extent {
            Extent::Nowhere => {}
            Extent::Area(area) => self.add_area(area),
            Extent::Mark if !marks => {}
            Extent::Mark | Extent::Unknown => self.damage = Damage::Full,
        }
    }

    /// An operation that is written over another: what the first painted and what the second paints.
    pub(crate) fn add_change(&mut self, old: &PaintOperation, new: &PaintOperation) {
        self.add_extent(old.extent(), true);
        self.add_extent(new.extent(), true);
    }

    /// Operations that are put in the place of others, which is where all of them were and are.
    pub(crate) fn add_operations(&mut self, operations: &[PaintOperation]) {
        for operation in operations {
            if self.damage == Damage::Full {
                return;
            }
            self.add_extent(operation.extent(), false);
        }
    }

    /// Extents that are put in the place of others, as [`Self::add_operations`] does for their operations.
    pub(crate) fn add_extents(&mut self, extents: impl IntoIterator<Item = Extent>) {
        for extent in extents {
            if self.damage == Damage::Full {
                return;
            }
            self.add_extent(extent, false);
        }
    }

    /// The damage the platform is to draw, in device pixels inside a viewport of `viewport` (`None`
    /// is all of it), and the damage that is left, which is none.
    pub(crate) fn take(&mut self, scene: &Scene, viewport: Size<DevicePixels>) -> Option<FrameDamage> {
        let damage = std::mem::replace(&mut self.damage, Damage::Area(Area::default()));
        if self.accounted != scene.revision() || scene.has_unboxed_content() {
            self.accounted = scene.revision();
            return None;
        }
        match damage {
            Damage::Full => None,
            Damage::Area(area) => Some(area.frame_damage(viewport)),
        }
    }
}

/// The damage the ledger gives for a chain of shifts and areas, for the tests of a platform that
/// draws a shift: it adds them as a window does, in the order they are called.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct DamageSteps(Area);

#[cfg(any(test, feature = "test-support"))]
impl DamageSteps {
    /// A list scrolls by `by` inside `clip`.
    pub fn shift(&mut self, clip: Bounds<ScaledPixels>, by: Point<DevicePixels>) -> &mut Self {
        self.0.add_shift(clip, by);
        self
    }

    /// Something is drawn again inside `area`.
    pub fn area(&mut self, area: Bounds<ScaledPixels>) -> &mut Self {
        self.0.add(area);
        self
    }

    /// The damage of a window of `viewport`.
    pub fn finish(&self, viewport: Size<DevicePixels>) -> FrameDamage {
        self.0.frame_damage(viewport)
    }
}

impl crate::Window {
    /// The damage of the scene shown, taken for a presentation; see [`DamageLedger::take`].
    pub(crate) fn take_damage(&mut self) -> Option<FrameDamage> {
        let viewport = size(
            DevicePixels((self.viewport_size.width.0 * self.scale_factor).ceil() as i32),
            DevicePixels((self.viewport_size.height.0 * self.scale_factor).ceil() as i32),
        );
        let damage = self.damage.take(&self.rendered_frame.scene, viewport);
        self.scroll_patch.note_presented(damage.as_ref());
        damage
    }
}

/// What a frame drawn with [`PlatformWindow::draw_with_damage`] was told and held, for tests of the
/// damage: each primitive of the scene as text without its draw order (which a scene made again
/// changes for the ones that did not) and the area it draws.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct DamageDraw {
    /// The damage the platform was given, `None` for all of the window.
    pub damage: Option<FrameDamage>,
    /// The primitives of the scene, by what they paint.
    pub primitives: Vec<(String, Bounds<ScaledPixels>)>,
}

#[cfg(any(test, feature = "test-support"))]
impl DamageDraw {
    pub(crate) fn record(scene: &Scene, damage: Option<FrameDamage>) -> Self {
        let mut primitives = Vec::new();
        macro_rules! take {
            ($($field:ident => $kind:ident),*) => {$(
                for primitive in &scene.$field {
                    let mut copy = primitive.clone();
                    copy.order = 0;
                    let drawn = crate::Primitive::$kind(primitive.clone()).drawn_bounds().intersect(&primitive.content_mask.bounds);
                    primitives.push((format!("{copy:?}"), drawn));
                }
            )*};
        }
        take!(shadows => Shadow, quads => Quad, underlines => Underline, monochrome_sprites => MonochromeSprite,
            subpixel_sprites => SubpixelSprite, polychrome_sprites => PolychromeSprite);
        Self { damage, primitives }
    }

    /// The primitives that this frame has and `before` did not, or the other way around: where the
    /// pictures of the two can differ.
    pub fn changed_areas(&self, before: &DamageDraw) -> Vec<Bounds<ScaledPixels>> {
        fn minus(from: &[(String, Bounds<ScaledPixels>)], what: &[(String, Bounds<ScaledPixels>)]) -> Vec<Bounds<ScaledPixels>> {
            let mut left: Vec<&(String, Bounds<ScaledPixels>)> = what.iter().collect();
            let mut areas = Vec::new();
            for item in from {
                match left.iter().position(|other| other.0 == item.0) {
                    Some(at) => {
                        left.swap_remove(at);
                    }
                    None => areas.push(item.1),
                }
            }
            areas
        }
        let mut areas = minus(&self.primitives, &before.primitives);
        areas.extend(minus(&before.primitives, &self.primitives));
        areas
    }

    /// Whether `area` is inside the rectangles drawn (the shifted copy is not counted, which says nothing
    /// of the pixels that changed).
    pub fn covers(&self, area: &Bounds<ScaledPixels>) -> bool {
        let Some(damage) = &self.damage else { return true };
        let (left, top, right, bottom) = (area.left().0, area.top().0, area.right().0, area.bottom().0);
        damage.rects.iter().any(|rect| {
            let rect = IntRect::of(*rect);
            left >= rect.left as f32 && top >= rect.top as f32 && right <= rect.right as f32 && bottom <= rect.bottom as f32
        })
    }
}
