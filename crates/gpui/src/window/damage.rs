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
//! A change that no site says is seen anyway: every rebuild of the rendered scene is a `clear`
//! (see [`Scene::revision`]), the ledger knows the revision it was told of, and a rebuild it was not
//! told of makes the damage the whole picture. So does anything in the scene whose pixels are not
//! drawn inside a box: an effect group, which reads and writes what is around it, a native surface
//! and a surface of video frames.

use crate::scene::Extent;
use crate::{Bounds, DevicePixels, PaintOperation, ScaledPixels, Scene, Size, point, size};

/// How far past the area of an operation the damage reaches, in device pixels: antialiased edges
/// are inside the bounds the shaders draw in, and this is for whatever rounding is left.
const MARGIN: f32 = 2.;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Damage {
    /// Not known: all of the picture.
    Full,
    /// Inside this area, or nowhere.
    Area(Option<Bounds<ScaledPixels>>),
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
            *damage = Some(damage.map_or(area, |damage| damage.union(&area)));
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

    /// The damage the platform is to draw, in device pixels inside a viewport of `viewport` (`None`
    /// is all of it, an empty rectangle none of it), and the damage that is left, which is none.
    pub(crate) fn take(&mut self, scene: &Scene, viewport: Size<DevicePixels>) -> Option<Bounds<DevicePixels>> {
        let damage = std::mem::replace(&mut self.damage, Damage::Area(None));
        if self.accounted != scene.revision() || scene.has_unboxed_content() {
            self.accounted = scene.revision();
            return None;
        }
        match damage {
            Damage::Full => None,
            Damage::Area(None) => Some(Bounds::default()),
            Damage::Area(Some(area)) => Some(to_device(area, viewport)),
        }
    }
}

fn to_device(area: Bounds<ScaledPixels>, viewport: Size<DevicePixels>) -> Bounds<DevicePixels> {
    let left = (area.left().0 - MARGIN).floor().max(0.);
    let top = (area.top().0 - MARGIN).floor().max(0.);
    let right = (area.right().0 + MARGIN).ceil().min(viewport.width.0 as f32);
    let bottom = (area.bottom().0 + MARGIN).ceil().min(viewport.height.0 as f32);
    if right <= left || bottom <= top {
        return Bounds::default();
    }
    Bounds {
        origin: point(DevicePixels(left as i32), DevicePixels(top as i32)),
        size: size(DevicePixels((right - left) as i32), DevicePixels((bottom - top) as i32)),
    }
}

impl crate::Window {
    /// The damage of the scene shown, taken for a presentation; see [`DamageLedger::take`].
    pub(crate) fn take_damage(&mut self) -> Option<Bounds<DevicePixels>> {
        let viewport = size(
            DevicePixels((self.viewport_size.width.0 * self.scale_factor).ceil() as i32),
            DevicePixels((self.viewport_size.height.0 * self.scale_factor).ceil() as i32),
        );
        self.damage.take(&self.rendered_frame.scene, viewport)
    }
}

/// What a frame drawn with [`PlatformWindow::draw_with_damage`] was told and held, for tests of the
/// damage: each primitive of the scene as text without its draw order (which a scene made again
/// changes for the ones that did not) and the area it draws.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct DamageDraw {
    /// The damage the platform was given, `None` for all of the window.
    pub damage: Option<Bounds<DevicePixels>>,
    /// The primitives of the scene, by what they paint.
    pub primitives: Vec<(String, Bounds<ScaledPixels>)>,
}

#[cfg(any(test, feature = "test-support"))]
impl DamageDraw {
    pub(crate) fn record(scene: &Scene, damage: Option<Bounds<DevicePixels>>) -> Self {
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

    /// Whether `area` is inside the damage.
    pub fn covers(&self, area: &Bounds<ScaledPixels>) -> bool {
        let Some(damage) = self.damage else { return true };
        let (left, top) = (damage.origin.x.0 as f32, damage.origin.y.0 as f32);
        let (right, bottom) = (left + damage.size.width.0 as f32, top + damage.size.height.0 as f32);
        area.left().0 >= left && area.top().0 >= top && area.right().0 <= right && area.bottom().0 <= bottom
    }
}
