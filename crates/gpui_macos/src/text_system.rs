use anyhow::anyhow;
use cocoa::appkit::CGFloat;
use collections::{HashMap, HashSet};
use core_foundation::{
    array::{CFArray, CFArrayRef},
    attributed_string::CFMutableAttributedString,
    base::{CFIndex, CFRange, CFType, TCFType},
    data::CFData,
    number::CFNumber,
    string::CFString,
};
use core_graphics::{
    base::{CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::{CGContext, CGTextDrawingMode},
    display::CGPoint,
};
use core_text::{
    font::CTFont,
    font_collection::CTFontCollectionRef,
    font_descriptor::{
        CTFontDescriptor, kCTFontSlantTrait, kCTFontSymbolicTrait, kCTFontWeightTrait,
        kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    properties::{Style as FontkitStyle, Weight as FontkitWeight},
    source::SystemSource,
    sources::mem::MemSource,
};
use gpui::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontSmoothing, FontStyle, FontVariations, FontWeight, GlyphId, Hsla, LineLayout, Pixels, PlatformTextSystem,
    RenderGlyphParams, Result, Rgba, SUBPIXEL_VARIANTS_X, ShapedGlyph, ShapedRun, SharedString,
    Size, TextRenderingMode, point, px, size, swap_rgba_pa_to_bgra,
};
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::Vector2F,
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc, sync::OnceLock};

use crate::open_type::{
    VariationAxis, apply_features_and_fallbacks, apply_variations, current_variations,
    variation_axes,
};

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;
#[allow(non_upper_case_globals)]
const kCGImageAlphaNoneSkipFirst: u32 = 6;
#[allow(non_upper_case_globals)]
const kCGBitmapByteOrder32Host: u32 = 2 << 12;

/// macOS text system using CoreText for font shaping.
pub struct MacTextSystem(RwLock<MacTextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    font_family: SharedString,
    font_features: FontFeatures,
    font_fallbacks: Option<FontFallbacks>,
}

struct MacTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    /// The metrics of each of `fonts`, by the same index.
    font_metrics: Vec<FontMetrics>,
    font_selections: HashMap<Font, FontId>,
    font_ids_by_postscript_name: HashMap<String, FontId>,
    font_ids_by_font_key: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    postscript_names_by_font_id: HashMap<FontId, String>,
    /// The faces made from another by setting variation axes, by that face and the
    /// axes set. They are not in `font_ids_by_postscript_name`: they keep the name
    /// of the face they were made from.
    varied_fonts: HashMap<(FontId, FontVariations), FontId>,
    /// What each of `varied_fonts` was asked for, to set again on a copy of it at another size.
    varied_faces: HashMap<FontId, VariedFace>,
    /// Each face at a size, by face and the bits of the size. Rasterising asks for one per
    /// glyph and shaping one per run, and making a CTFont (and setting its axes) costs more
    /// than a lookup. A CTFont is also what CoreText keeps its per-font caches on (the ASCII
    /// glyph and advance tables), so a copy made for every line has them filled for every line.
    sized_fonts: Mutex<HashMap<(FontId, u32), CTFont>>,
}

struct VariedFace {
    source: FontId,
    requested: Vec<(u32, f32)>,
    axes: Vec<VariationAxis>,
}

/// More sizes than a page of text uses; an animated `font-size` would otherwise grow
/// the cache for as long as it runs.
const SIZED_FONTS_LIMIT: usize = 256;

impl MacTextSystem {
    /// Create a new MacTextSystem.
    pub fn new() -> Self {
        Self(RwLock::new(MacTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            font_metrics: Vec::new(),
            font_selections: HashMap::default(),
            font_ids_by_postscript_name: HashMap::default(),
            font_ids_by_font_key: HashMap::default(),
            postscript_names_by_font_id: HashMap::default(),
            varied_fonts: HashMap::default(),
            varied_faces: HashMap::default(),
            sized_fonts: Mutex::new(HashMap::default()),
        }))
    }
}

impl Default for MacTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for MacTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let collection = core_text::font_collection::create_for_all_families();
        // NOTE: We intentionally avoid using `collection.get_descriptors()` here because
        // it has a memory leak bug in core-text v21.0.0. The upstream code uses
        // `wrap_under_get_rule` but `CTFontCollectionCreateMatchingFontDescriptors`
        // follows the Create Rule (caller owns the result), so it should use
        // `wrap_under_create_rule`. We call the function directly with correct memory management.
        unsafe extern "C" {
            fn CTFontCollectionCreateMatchingFontDescriptors(
                collection: CTFontCollectionRef,
            ) -> CFArrayRef;
        }
        let descriptors: Option<CFArray<CTFontDescriptor>> = unsafe {
            let array_ref =
                CTFontCollectionCreateMatchingFontDescriptors(collection.as_concrete_TypeRef());
            if array_ref.is_null() {
                None
            } else {
                Some(CFArray::wrap_under_create_rule(array_ref))
            }
        };
        let Some(descriptors) = descriptors else {
            return names;
        };
        for descriptor in descriptors.into_iter() {
            names.extend(lenient_font_attributes::family_name(&descriptor));
        }
        if let Ok(fonts_in_memory) = self.0.read().memory_source.all_families() {
            names.extend(fonts_in_memory);
        }
        names
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(font_id) = lock.font_selections.get(font) {
            Ok(*font_id)
        } else {
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let font_key = FontKey {
                font_family: font.family.clone(),
                font_features: font.features.clone(),
                font_fallbacks: font.fallbacks.clone(),
            };
            let candidates = if let Some(font_ids) = lock.font_ids_by_font_key.get(&font_key) {
                font_ids.as_slice()
            } else {
                let font_ids =
                    lock.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
                lock.font_ids_by_font_key.insert(font_key.clone(), font_ids);
                lock.font_ids_by_font_key[&font_key].as_ref()
            };

            let candidate_properties = candidates
                .iter()
                .map(|font_id| lock.fonts[font_id.0].properties())
                .collect::<SmallVec<[_; 4]>>();

            let ix = font_kit::matching::find_best_match(
                &candidate_properties,
                &font_kit::properties::Properties {
                    style: fontkit_style(font.style),
                    weight: fontkit_weight(font.weight),
                    stretch: Default::default(),
                },
            )?;

            let font_id = candidates[ix];
            let font_id = lock.varied_font(font_id, font.weight, &font.variations);
            lock.font_selections.insert(font.clone(), font_id);
            Ok(font_id)
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.read().font_metrics[font_id.0]
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(bounds_from_rect(
            self.0.read().fonts[font_id.0].typographic_bounds(glyph_id.0)?,
        ))
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        glyph_id: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(glyph_id, raster_bounds)
    }

    fn layout_line(
        &self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
        scale_factor: f32,
    ) -> LineLayout {
        self.0
            .write()
            .layout_line(text, font_size, font_runs, scale_factor)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }

    fn glyph_luminance_bucket_for_color(&self, color: Hsla) -> u8 {
        if !font_smoothing_allowed_by_user() {
            return 0;
        }
        luminance_bucket(color.into())
    }
}

/// The table Skia's mask gamma uses for text of this colour: its luminance, from each channel
/// halved and cut to 3 bits (the halving is the gamma of 1.0 CoreGraphics goes to for white text
/// from the 2.0 it has for black), in 8 steps.
fn luminance_bucket(color: Rgba) -> u8 {
    let channel = |value: f32| {
        let halved = (value * 255.0).round() as u32 / 2;
        let bits = halved >> 5;
        (bits << 5) | (bits << 2) | (bits >> 1)
    };
    let luminance = (channel(color.r) * 54 + channel(color.g) * 183 + channel(color.b) * 19) >> 8;
    (luminance >> 5) as u8
}

const LUMINANCE_BUCKETS: usize = 8;

/// Skia's mask gamma for an sRGB device, contrast 0 (`SkTMaskGamma_build_correcting_lut`): for
/// text of the bucket's luminance, the coverage a blend in the encoded space has to be given so
/// that it comes out as a blend in linear light. The destination is guessed to be the inverse of
/// the text.
fn smoothing_table(bucket: usize) -> [u8; 256] {
    fn to_linear(value: f32) -> f32 {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    fn from_linear(value: f32) -> f32 {
        if value <= 0.0031308 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        }
    }

    let bits = bucket as u32;
    let source = ((bits << 5) | (bits << 2) | (bits >> 1)) as f32 / 255.0;
    let destination = 1.0 - source;
    let linear_source = to_linear(source);
    let linear_destination = to_linear(destination);
    let mut table = [0u8; 256];
    for (coverage, entry) in table.iter_mut().enumerate() {
        let coverage = coverage as f32 / 255.0;
        let value = if (source - destination).abs() < 1.0 / 256.0 {
            255.0 * coverage
        } else {
            let linear = linear_source * coverage + (1.0 - coverage) * linear_destination;
            255.0 * (from_linear(linear) - destination) / (source - destination)
        };
        *entry = value.round().clamp(0.0, 255.0) as u8;
    }
    table
}

fn smoothing_tables() -> &'static [[u8; 256]; LUMINANCE_BUCKETS] {
    static TABLES: OnceLock<[[u8; 256]; LUMINANCE_BUCKETS]> = OnceLock::new();
    TABLES.get_or_init(|| std::array::from_fn(smoothing_table))
}

/// The coverage of one pixel CoreGraphics drew black on white with smoothing: its output has the
/// gamma of 2.0 that Skia's `sk_pow2_table` takes out, as Skia does for the A8 masks it makes
/// from smoothed text.
fn smoothed_coverage(pixel: &[u8], table: &[u8; 256]) -> u8 {
    let linear = |value: u8| 255 - ((value as u32 * value as u32 + 128) / 255);
    // The pixel is BGRX in memory.
    let luminance = (linear(pixel[2]) * 54 + linear(pixel[1]) * 183 + linear(pixel[0]) * 19) >> 8;
    table[luminance as usize]
}

fn font_smoothing_allowed_by_user() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        use core_foundation_sys::preferences::{
            CFPreferencesCopyAppValue, kCFPreferencesCurrentApplication,
        };

        let key = CFString::new("AppleFontSmoothing");
        let value_ref = unsafe {
            CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), kCFPreferencesCurrentApplication)
        };
        if value_ref.is_null() {
            return true;
        }
        let value = unsafe { CFType::wrap_under_create_rule(value_ref) };
        let Some(number) = value.downcast_into::<CFNumber>() else {
            return true;
        };
        // Only an explicit value of `0` means that font smoothing is disabled.
        number.to_i64() != Some(0)
    })
}

/// Hands `bytes` to Core Text without copying them: the `CFData` over the
/// buffer is the only copy of the file. font-kit's `Handle::from_memory` keeps
/// the bytes in an `Arc` and builds a `CFData` copy from them for the font
/// and for every `load()` of its handle.
///
/// Collections and Mac resource-fork fonts have no descriptor-from-data path
/// (font-kit unpacks them into a new buffer), so they keep font-kit's.
fn owned_font_handle(bytes: Vec<u8>) -> Result<Handle> {
    const OTTO: &[u8] = b"OTTO";
    const TRUETYPE: &[u8] = &[0, 1, 0, 0];
    if !(bytes.starts_with(OTTO) || bytes.starts_with(TRUETYPE)) {
        return Ok(Handle::from_memory(Arc::new(bytes), 0));
    }

    let descriptor = core_text::font_manager::create_font_descriptor_with_data(CFData::from_arc(
        Arc::new(bytes),
    ))
    .map_err(|()| anyhow!("Could not load a font."))?;
    let font = core_text::font::new_from_descriptor(&descriptor, 16.0);
    // SAFETY: `font` is a valid CTFont, and the handle holds its own reference.
    let font = unsafe { font_kit::loaders::core_text::Font::from_native_font(&font) };
    Ok(Handle::from_native(&font))
}

impl MacTextSystemState {
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let fonts = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(embedded_font) => {
                    let data_provider = unsafe {
                        core_graphics::data_provider::CGDataProvider::from_slice(embedded_font)
                    };
                    let font = core_graphics::font::CGFont::from_data_provider(data_provider)
                        .map_err(|()| anyhow!("Could not load an embedded font."))?;
                    let font = font_kit::loaders::core_text::Font::from_core_graphics_font(font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => owned_font_handle(bytes),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(fonts.into_iter())?;
        Ok(())
    }

    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let name = gpui::font_name_with_fallbacks(name, ".AppleSystemUIFont");

        let mut font_ids = SmallVec::new();
        let mut postscript_names_seen = HashSet::default();
        let family = self
            .memory_source
            .select_family_by_name(name)
            .or_else(|_| self.system_source.select_family_by_name(name))?;
        for font in family.fonts() {
            let mut font = font.load()?;

            apply_features_and_fallbacks(&mut font, features, fallbacks)?;
            // This block contains a precautionary fix to guard against loading fonts
            // that might cause panics due to `.unwrap()`s up the chain.
            {
                // We use the 'm' character for text measurements in various spots
                // (e.g., the editor). However, at time of writing some of those usages
                // will panic if the font has no 'm' glyph.
                //
                // Therefore, we check up front that the font has the necessary glyph.
                let has_m_glyph = font.glyph_for_char('m').is_some();

                // HACK: The 'Segoe Fluent Icons' font does not have an 'm' glyph,
                // but we need to be able to load it for rendering Windows icons in
                // the Storybook (on macOS).
                let is_segoe_fluent_icons = font.full_name() == "Segoe Fluent Icons";

                if !has_m_glyph && !is_segoe_fluent_icons {
                    // I spent far too long trying to track down why a font missing the 'm'
                    // character wasn't loading. This log statement will hopefully save
                    // someone else from suffering the same fate.
                    log::warn!(
                        "font '{}' has no 'm' character and was not loaded",
                        font.full_name()
                    );
                    continue;
                }
            }

            // We've seen a number of panics in production caused by calling font.properties()
            // which unwraps a downcast to CFNumber. This is an attempt to avoid the panic,
            // and to try and identify the incalcitrant font.
            let traits = font.native_font().all_traits();
            if unsafe {
                !(traits
                    .get(kCTFontSymbolicTrait)
                    .downcast::<CFNumber>()
                    .is_some()
                    && traits
                        .get(kCTFontWidthTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontWeightTrait)
                        .downcast::<CFNumber>()
                        .is_some()
                    && traits
                        .get(kCTFontSlantTrait)
                        .downcast::<CFNumber>()
                        .is_some())
            } {
                log::error!(
                    "Failed to read traits for font {:?} (PostScript name {:?})",
                    font.full_name(),
                    font.postscript_name(),
                );
                continue;
            }

            let Some(postscript_name) = font.postscript_name() else {
                log::warn!(
                    "font {:?} in family {:?} has no PostScript name; skipping",
                    font.full_name(),
                    name,
                );
                continue;
            };
            // Dedup is scoped to this single `load_family` call (issue #55472).
            // The same family can be reloaded later under a different `FontKey`
            // (different features/fallbacks); a global check against
            // `font_ids_by_postscript_name` would skip every already-registered
            // font and leave the second call's `font_ids` empty.
            if !postscript_names_seen.insert(postscript_name.clone()) {
                log::warn!(
                    "skipping duplicate font {:?} with PostScript name {:?} \
                     in family {:?}",
                    font.full_name(),
                    postscript_name,
                    name,
                );
                continue;
            }
            let font_id = self.push_font(font);
            font_ids.push(font_id);
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
        }
        Ok(font_ids)
    }

    fn push_font(&mut self, font: FontKitFont) -> FontId {
        let font_id = FontId(self.fonts.len());
        self.font_metrics.push(font_kit_metrics_to_metrics(&font));
        self.fonts.push(font);
        font_id
    }

    /// `font_id` with the axes it has set to `variations`, and to `weight` on `wght`
    /// unless `variations` names it, as CSS font matching does for a variable font.
    /// An axis the face does not have is ignored. `font_id` itself when that changes
    /// nothing, as for a face that already has those values.
    ///
    /// `opsz` is the exception: CoreText sets it from the point size a font is made at
    /// and again at every copy at another size, and the value it reports does not tell
    /// an explicit one from the automatic one. An explicit `opsz` is always set, and the
    /// copies at a size set it again, see `sized_font`.
    fn varied_font(
        &mut self,
        font_id: FontId,
        weight: FontWeight,
        variations: &FontVariations,
    ) -> FontId {
        let axes = variation_axes(&self.fonts[font_id.0]);
        if axes.is_empty() {
            return font_id;
        }
        let weight_tag = FontVariations::tag(*b"wght");
        let optical_size_tag = FontVariations::tag(*b"opsz");
        let automatic = [(weight_tag, weight.0)];
        let automatic = (variations.get(weight_tag).is_none()).then_some(automatic.as_slice());
        let current = current_variations(&self.fonts[font_id.0], &axes);
        let wanted = variations
            .tag_value_list()
            .iter()
            .chain(automatic.unwrap_or_default())
            .filter_map(|(tag, value)| {
                let axis = axes.iter().find(|axis| axis.tag == *tag)?;
                let value = value.clamp(axis.min, axis.max);
                let has = current.iter().find(|(current, _)| current == tag)?.1;
                (has != value || *tag == optical_size_tag).then_some((*tag, value))
            })
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            return font_id;
        }
        let wanted = FontVariations::new(wanted);
        let key = (font_id, wanted);
        if let Some(varied) = self.varied_fonts.get(&key) {
            return *varied;
        }
        let mut font = self.fonts[font_id.0].clone();
        apply_variations(&mut font, key.1.tag_value_list(), &axes);
        let varied = self.push_font(font);
        if let Some(postscript_name) = self.postscript_names_by_font_id.get(&font_id).cloned() {
            self.postscript_names_by_font_id
                .insert(varied, postscript_name);
        }
        self.varied_faces.insert(
            varied,
            VariedFace {
                source: font_id,
                requested: key.1.tag_value_list().to_vec(),
                axes,
            },
        );
        self.varied_fonts.insert(key, varied);
        varied
    }

    /// The CoreText font of `font_id` at `size`. A copy at another size gets its
    /// automatic axes for that size, so a varied face is made again from the face it
    /// came from at the size, with its axes set. It is not set again on a copy of itself:
    /// CoreText takes a value equal to what the font has as no change and gives the
    /// axis its automatic value.
    fn sized_font(&self, font_id: FontId, size: f32) -> CTFont {
        let key = (font_id, size.to_bits());
        let mut sized = self.sized_fonts.lock();
        if let Some(cached) = sized.get(&key) {
            return cached.clone();
        }
        let copy = match self.varied_faces.get(&font_id) {
            None => self.fonts[font_id.0]
                .native_font()
                .clone_with_font_size(size as CGFloat),
            Some(face) => {
                let source = self.fonts[face.source.0].native_font();
                let mut copy = unsafe {
                    FontKitFont::from_native_font(&source.clone_with_font_size(size as CGFloat))
                };
                apply_variations(&mut copy, &face.requested, &face.axes);
                copy.native_font()
            }
        };
        if sized.len() >= SIZED_FONTS_LIMIT {
            sized.clear();
        }
        sized.insert(key, copy.clone());
        copy
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(size_from_vector2f(
            self.fonts[font_id.0].advance(glyph_id.0)?,
        ))
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts[font_id.0].glyph_for_char(ch).map(GlyphId)
    }

    fn id_for_native_font(&mut self, requested_font: CTFont) -> FontId {
        let postscript_name = requested_font.postscript_name();
        if let Some(font_id) = self.font_ids_by_postscript_name.get(&postscript_name) {
            *font_id
        } else {
            let font_id = self.push_font(font_kit::font::Font::from_core_graphics_font(
                requested_font.copy_to_CGFont(),
            ));
            self.font_ids_by_postscript_name
                .insert(postscript_name.clone(), font_id);
            self.postscript_names_by_font_id
                .insert(font_id, postscript_name);
            font_id
        }
    }

    fn is_emoji(&self, font_id: FontId) -> bool {
        self.postscript_names_by_font_id
            .get(&font_id)
            .is_some_and(|postscript_name| {
                postscript_name == "AppleColorEmoji" || postscript_name == ".AppleColorEmojiUI"
            })
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let scale = Transform2F::from_scale(params.scale_factor);
        let bounds: Bounds<DevicePixels> = bounds_from_rect_i(font.raster_bounds(
            params.glyph_id.0,
            params.font_size.into(),
            scale,
            HintingOptions::None,
            font_kit::canvas::RasterizationOptions::GrayscaleAa,
        )?);

        // Expand the bounds by 1 pixel on each side to give CG room for anti-aliasing.
        Ok(bounds.dilate(DevicePixels(1)))
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        } else {
            // Add an extra pixel when the subpixel variant isn't zero to make room for anti-aliasing.
            let mut bitmap_size = glyph_bounds.size;
            if params.subpixel_variant.x > 0 {
                bitmap_size.width += DevicePixels(1);
            }
            if params.subpixel_variant.y > 0 {
                bitmap_size.height += DevicePixels(1);
            }
            let bitmap_size = bitmap_size;

            // Skia makes the mask of smoothed text from CoreGraphics' black on white output, whatever
            // the colour of the text, and CoreGraphics only smooths into an opaque context.
            let smoothed = params.font_smoothing == FontSmoothing::Auto
                && !params.is_emoji
                && font_smoothing_allowed_by_user();

            let mut bytes;
            let cx;
            if params.is_emoji {
                bytes = vec![0; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaPremultipliedLast,
                );
            } else if smoothed {
                bytes = vec![0xff; bitmap_size.width.0 as usize * 4 * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize * 4,
                    &CGColorSpace::create_device_rgb(),
                    kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Host,
                );
            } else {
                bytes = vec![0; bitmap_size.width.0 as usize * bitmap_size.height.0 as usize];
                cx = CGContext::create_bitmap_context(
                    Some(bytes.as_mut_ptr() as *mut _),
                    bitmap_size.width.0 as usize,
                    bitmap_size.height.0 as usize,
                    8,
                    bitmap_size.width.0 as usize,
                    &CGColorSpace::create_device_gray(),
                    kCGImageAlphaOnly,
                );
            }

            // Move the origin to bottom left and account for scaling, this
            // makes drawing text consistent with the font-kit's raster_bounds.
            cx.translate(
                -glyph_bounds.origin.x.0 as CGFloat,
                (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
            );
            cx.scale(
                params.scale_factor as CGFloat,
                params.scale_factor as CGFloat,
            );

            let subpixel_shift = params
                .subpixel_variant
                .map(|v| v as f32 / SUBPIXEL_VARIANTS_X as f32);
            cx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
            cx.set_allows_antialiasing(true);
            cx.set_should_antialias(true);
            cx.set_allows_font_subpixel_positioning(true);
            cx.set_should_subpixel_position_fonts(true);
            cx.set_allows_font_subpixel_quantization(false);
            cx.set_should_subpixel_quantize_fonts(false);

            cx.set_gray_fill_color(0.0, 1.0);
            if params.font_smoothing == FontSmoothing::Antialiased {
                // CoreGraphics smooths a glyph by default, and `-webkit-font-smoothing:
                // antialiased` is the plain coverage without it, for dark and light text alike.
                cx.set_should_smooth_fonts(false);
            } else if smoothed {
                cx.set_should_smooth_fonts(true);
            }
            self.sized_font(params.font_id, f32::from(params.font_size))
                .draw_glyphs(
                    &[params.glyph_id.0 as CGGlyph],
                    &[CGPoint::new(
                        (subpixel_shift.x / params.scale_factor) as CGFloat,
                        (subpixel_shift.y / params.scale_factor) as CGFloat,
                    )],
                    cx,
                );

            if params.is_emoji {
                // Convert from RGBA with premultiplied alpha to BGRA with straight alpha.
                for pixel in bytes.chunks_exact_mut(4) {
                    swap_rgba_pa_to_bgra(pixel);
                }
            } else if smoothed {
                let table = smoothing_tables()
                    .get(params.luminance_bucket as usize)
                    .ok_or_else(|| {
                        anyhow!("luminance bucket {} is out of range", params.luminance_bucket)
                    })?;
                bytes = bytes
                    .chunks_exact(4)
                    .map(|pixel| smoothed_coverage(pixel, table))
                    .collect();
            }

            Ok((bitmap_size, bytes))
        }
    }

    fn layout_line(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
        scale_factor: f32,
    ) -> LineLayout {
        // Construct the attributed string, converting UTF8 ranges to UTF16 ranges.
        let mut string = CFMutableAttributedString::new();
        let mut input_runs = Vec::<InputRun>::with_capacity(font_runs.len());

        {
            let mut text = text;
            let mut break_ligature = true;
            for run in font_runs {
                let text_run;
                (text_run, text) = text.split_at(run.len);

                let utf16_start = string.char_len(); // insert at end of string
                // note: replace_str may silently ignore codepoints it dislikes (e.g., BOM at start of string)
                string.replace_str(&CFString::new(text_run), CFRange::init(utf16_start, 0));
                let utf16_end = string.char_len();

                let length = utf16_end - utf16_start;
                let cf_range = CFRange::init(utf16_start, length);

                let run_font_size = run.font_size.unwrap_or(font_size);
                let line_box = self.font_metrics[run.font_id.0].line_box(run_font_size, scale_factor);
                let ct_font_size = if break_ligature {
                    px(f32::from(run_font_size).next_up())
                } else {
                    run_font_size
                };
                let ct_font = self.sized_font(run.font_id, f32::from(ct_font_size));
                input_runs.push(InputRun {
                    font_id: run.font_id,
                    postscript_name: ct_font.postscript_name(),
                    utf16_start,
                    font_size: run_font_size,
                    ascent: line_box.ascent,
                    descent: line_box.descent,
                    line_height: run.line_height,
                });
                unsafe {
                    string.set_attribute(cf_range, kCTFontAttributeName, &ct_font);
                }
                break_ligature = !break_ligature;
            }
        }
        // Retrieve the glyphs from the shaped line, converting UTF16 offsets to UTF8 offsets.
        let line = CTLine::new_with_attributed_string(string.as_concrete_TypeRef());
        let glyph_runs = line.glyph_runs();
        let mut runs = <Vec<ShapedRun>>::with_capacity(glyph_runs.len() as usize);
        let mut ix_converter = StringIndexConverter::new(text);
        for run in glyph_runs.into_iter() {
            let attributes = run.attributes().unwrap();
            let font = unsafe {
                attributes
                    .get(kCTFontAttributeName)
                    .downcast::<CTFont>()
                    .unwrap()
            };

            // The CTFont's point size is perturbed by the ligature hack and its font can be
            // a fallback, so the run's size comes from the input run it was shaped from.
            // The hack also keeps a CTRun inside one input run.
            let Some(first_utf16_ix) = run.string_indices().first().copied() else {
                continue;
            };
            let input_run = &input_runs
                [input_runs.partition_point(|input| input.utf16_start <= first_utf16_ix) - 1];

            // A face with features set has the PostScript name of the one it was made from,
            // so the name cannot tell which of them shaped the run. When CoreText kept the
            // font it was given, the run is in the font it asked for.
            let font_id = if input_run.postscript_name == font.postscript_name() {
                input_run.font_id
            } else {
                self.id_for_native_font(font)
            };

            let glyphs = match runs.last_mut() {
                Some(run)
                    if run.font_id == font_id
                        && run.font_size == input_run.font_size
                        && run.line_height == input_run.line_height =>
                {
                    &mut run.glyphs
                }
                _ => {
                    runs.push(ShapedRun {
                        font_id,
                        font_size: input_run.font_size,
                        ascent: input_run.ascent,
                        descent: input_run.descent,
                        line_height: input_run.line_height,
                        glyphs: Vec::with_capacity(run.glyph_count().try_into().unwrap_or(0)),
                        inline: Default::default(),
                    });
                    &mut runs.last_mut().unwrap().glyphs
                }
            };
            for ((&glyph_id, position), &glyph_utf16_ix) in run
                .glyphs()
                .iter()
                .zip(run.positions().iter())
                .zip(run.string_indices().iter())
            {
                let glyph_utf16_ix = usize::try_from(glyph_utf16_ix).unwrap();
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(glyph_id as u32),
                    position: point(position.x as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
            }
        }
        let typographic_bounds = line.get_typographic_bounds();
        // The strut is the font of the element the text is in, which the caller sets when
        // it knows it. Without it the font of the first run stands for it.
        let strut = font_runs
            .first()
            .map(|run| self.font_metrics[run.font_id.0].line_box(font_size, scale_factor));
        LineLayout {
            runs,
            font_size,
            width: typographic_bounds.width.into(),
            ascent: strut.map_or(Pixels::ZERO, |strut| strut.ascent),
            descent: strut.map_or(Pixels::ZERO, |strut| strut.descent),
            scale_factor,
            len: text.len(),
            boxes: Vec::new(),
        }
    }
}

/// What the shaper needs to know about an input `FontRun` once the glyphs come back.
struct InputRun {
    font_id: FontId,
    /// The name CoreText gives the font the run is asked to be shaped in.
    postscript_name: String,
    utf16_start: CFIndex,
    font_size: Pixels,
    ascent: Pixels,
    descent: Pixels,
    line_height: Option<Pixels>,
}

#[derive(Debug, Clone)]
struct StringIndexConverter<'a> {
    text: &'a str,
    /// Index in UTF-8 bytes
    utf8_ix: usize,
    /// Index in UTF-16 code units
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

/// The x-height and cap height an OS/2 table (version 2 and later) gives, in font units.
/// Skia's `SkScalerContext_Mac::generateFontMetrics` takes both from the table of the
/// font when it has them, nonzero and below two ems, and Chrome measures `ex` and a
/// `text-box-edge` of `ex` or `cap` from Skia's numbers. CoreText's `CTFontGetXHeight`
/// is not the table's for every font (Inter Variable: 1056 where the table has 1118
/// of 2048), and for a variable font it follows the variations, where the table is
/// that of the default instance and Chrome's heights stay put.
fn os2_heights(table: &[u8], units_per_em: u32) -> (Option<f32>, Option<f32>) {
    const X_HEIGHT: usize = 86;
    const CAP_HEIGHT: usize = 88;
    let read = |offset: usize| -> Option<i16> {
        let bytes = table.get(offset..offset + 2)?;
        Some(i16::from_be_bytes([bytes[0], bytes[1]]))
    };
    let version = read(0).unwrap_or(0);
    if version < 2 {
        return (None, None);
    }
    let height = |offset: usize| {
        read(offset)
            .filter(|height| *height > 0 && u32::from(height.unsigned_abs()) < 2 * units_per_em)
            .map(f32::from)
    };
    (height(X_HEIGHT), height(CAP_HEIGHT))
}

fn font_kit_metrics_to_metrics(font: &FontKitFont) -> FontMetrics {
    let metrics = font.metrics();
    let (os2_x_height, os2_cap_height) = font
        .load_font_table(u32::from_be_bytes(*b"OS/2"))
        .map(|table| os2_heights(&table, metrics.units_per_em))
        .unwrap_or((None, None));
    FontMetrics {
        units_per_em: metrics.units_per_em,
        ascent: metrics.ascent,
        descent: metrics.descent,
        line_gap: metrics.line_gap,
        underline_position: metrics.underline_position,
        underline_thickness: metrics.underline_thickness,
        cap_height: os2_cap_height.unwrap_or(metrics.cap_height),
        x_height: os2_x_height.unwrap_or(metrics.x_height),
        bounding_box: bounds_from_rect(metrics.bounding_box),
        // Blink's list, matched on the family name exactly: "Helvetica Neue" is not one.
        widens_ascent: matches!(
            font.family_name().as_str(),
            "Times" | "Helvetica" | "Courier"
        ),
    }
}

fn bounds_from_rect(rect: RectF) -> Bounds<f32> {
    Bounds {
        origin: point(rect.origin_x(), rect.origin_y()),
        size: size(rect.width(), rect.height()),
    }
}

fn bounds_from_rect_i(rect: RectI) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
        size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
    }
}

// impl From<Vector2I> for Size<DevicePixels> {
//     fn from(value: Vector2I) -> Self {
//         size(value.x().into(), value.y().into())
//     }
// }

// impl From<RectI> for Bounds<i32> {
//     fn from(rect: RectI) -> Self {
//         Bounds {
//             origin: point(rect.origin_x(), rect.origin_y()),
//             size: size(rect.width(), rect.height()),
//         }
//     }
// }

// impl From<Point<u32>> for Vector2I {
//     fn from(size: Point<u32>) -> Self {
//         Vector2I::new(size.x as i32, size.y as i32)
//     }
// }

fn size_from_vector2f(vec: Vector2F) -> Size<f32> {
    size(vec.x(), vec.y())
}

fn fontkit_weight(value: FontWeight) -> FontkitWeight {
    FontkitWeight(value.0)
}

fn fontkit_style(style: FontStyle) -> FontkitStyle {
    match style {
        FontStyle::Normal => FontkitStyle::Normal,
        FontStyle::Italic => FontkitStyle::Italic,
        FontStyle::Oblique => FontkitStyle::Oblique,
    }
}

// Some fonts may have no attributes despite `core_text` requiring them (and panicking).
// This is the same version as `core_text` has without `expect` calls.
mod lenient_font_attributes {
    use core_foundation::{
        base::{CFRetain, CFType, TCFType},
        string::{CFString, CFStringRef},
    };
    use core_text::font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttribute, kCTFontFamilyNameAttribute,
    };

    pub fn family_name(descriptor: &CTFontDescriptor) -> Option<String> {
        unsafe { get_string_attribute(descriptor, kCTFontFamilyNameAttribute) }
    }

    fn get_string_attribute(
        descriptor: &CTFontDescriptor,
        attribute: CFStringRef,
    ) -> Option<String> {
        unsafe {
            let value = CTFontDescriptorCopyAttribute(descriptor.as_concrete_TypeRef(), attribute);
            if value.is_null() {
                return None;
            }

            let value = CFType::wrap_under_create_rule(value);
            assert!(value.instance_of::<CFString>());
            let s = wrap_under_get_rule(value.as_CFTypeRef() as CFStringRef);
            Some(s.to_string())
        }
    }

    unsafe fn wrap_under_get_rule(reference: CFStringRef) -> CFString {
        unsafe {
            assert!(!reference.is_null(), "Attempted to create a NULL object.");
            let reference = CFRetain(reference as *const ::std::os::raw::c_void) as CFStringRef;
            TCFType::wrap_under_create_rule(reference)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::MacTextSystem;
    use gpui::{
        Font, FontFeatures, FontRun, FontSmoothing, FontVariations, FontWeight, GlyphId,
        PlatformTextSystem, RenderGlyphParams, font, px,
    };
    use std::sync::Arc;

    #[test]
    fn test_luminance_bucket_of_text_color() {
        let bucket = |hex: u32| {
            super::luminance_bucket(gpui::rgb(hex))
        };
        assert_eq!(bucket(0x000000), 0);
        assert_eq!(bucket(0xffffff), 3);
        // Halved first: #a1a1a1 is 80, in the bucket of 64 to 95.
        assert_eq!(bucket(0xa1a1a1), 2);
        // The channels are cut to 3 bits one by one before they are weighed: pure green is
        // 127 >> 5 = 3, scaled to 109, and 109 * 183 >> 8 is 77.
        assert_eq!(bucket(0x00ff00), 2);
        assert_eq!(bucket(0x0000ff), 0);
    }

    /// The tables of Skia's `SkTMaskGamma_build_correcting_lut` for an sRGB device and contrast 0,
    /// at a few coverages, worked out from its formula.
    #[test]
    fn test_smoothing_tables_are_skias_mask_gamma() {
        const COVERAGES: [usize; 7] = [0, 32, 64, 128, 192, 224, 255];
        const TABLES: [[u8; 7]; 8] = [
            [0, 15, 31, 68, 119, 157, 255],
            [0, 17, 36, 79, 138, 180, 255],
            [0, 21, 44, 96, 161, 203, 255],
            [0, 27, 56, 116, 183, 218, 255],
            [0, 38, 73, 140, 200, 228, 255],
            [0, 54, 95, 160, 212, 234, 255],
            [0, 76, 118, 176, 220, 238, 255],
            [0, 99, 137, 188, 225, 241, 255],
        ];
        for (bucket, expected) in TABLES.iter().enumerate() {
            let table = &super::smoothing_tables()[bucket];
            let entries = COVERAGES.map(|coverage| table[coverage]);
            assert_eq!(&entries, expected, "bucket {bucket}");
            assert!(table.windows(2).all(|pair| pair[0] <= pair[1]), "bucket {bucket}");
        }
    }

    #[test]
    fn test_smoothed_coverage_takes_out_the_gamma_of_coregraphics() {
        let table = &super::smoothing_tables()[0];
        let coverage = |value: u8| super::smoothed_coverage(&[value, value, value, 0xff], table);
        assert_eq!(coverage(255), 0);
        assert_eq!(coverage(0), 255);
        // 128 * 128 / 255 is 64 of linear value, so 191 of coverage, through the table of bucket 0.
        assert_eq!(coverage(128), table[191]);
    }

    #[test]
    fn test_layout_line_bom_char() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let line = "\u{feff}";
        let mut style = FontRun {
            font_id,
            len: line.len(),
            font_size: None,
            line_height: None,
        };

        let layout = fonts.layout_line(line, px(16.), &[style], 1.);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style], 1.);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b

        let line = "\u{feff}ab";
        let font_runs = &[
            FontRun {
                len: "\u{feff}".len(),
                font_id,
                font_size: None,
                line_height: None,
            },
            FontRun {
                len: "ab".len(),
                font_id,
                font_size: None,
                line_height: None,
            },
        ];
        let layout = fonts.layout_line(line, px(16.), font_runs, 1.);
        assert_eq!(layout.len, line.len());
        assert_eq!(layout.runs.len(), 1);
        assert_eq!(layout.runs[0].glyphs.len(), 2);
        // There's no glyph for \u{feff}
        assert_eq!(layout.runs[0].glyphs[0].id, GlyphId(68u32)); // a
        assert_eq!(layout.runs[0].glyphs[1].id, GlyphId(69u32)); // b
    }

    #[test]
    fn test_layout_line_zwnj_insertion() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello world";
        let font_runs = &[
            FontRun {
                font_id,
                len: 5,
                font_size: None,
                line_height: None,
            }, // "hello"
            FontRun {
                font_id,
                len: 6,
                font_size: None,
                line_height: None,
            }, // " world"
        ];

        let layout = fonts.layout_line(text, px(16.), font_runs, 1.);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with different font runs - should not insert ZWNJ
        let font_id2 = fonts.font_id(&font("Times")).unwrap_or(font_id);
        let font_runs_different = &[
            FontRun {
                font_id,
                len: 5,
                font_size: None,
                line_height: None,
            }, // "hello"
            // " world"
            FontRun {
                font_id: font_id2,
                len: 6,
                font_size: None,
                line_height: None,
            },
        ];

        let layout2 = fonts.layout_line(text, px(16.), font_runs_different, 1.);
        assert_eq!(layout2.len, text.len());

        for run in &layout2.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }
    }

    #[test]
    fn test_layout_line_zwnj_edge_cases() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();

        let text = "hello";
        let font_runs = &[FontRun {
            font_id,
            len: 5,
            font_size: None,
            line_height: None,
        }];
        let layout = fonts.layout_line(text, px(16.), font_runs, 1.);
        assert_eq!(layout.len, text.len());

        let text = "abc";
        let font_runs = &[
            FontRun {
                font_id,
                len: 1,
                font_size: None,
                line_height: None,
            }, // "a"
            FontRun {
                font_id,
                len: 1,
                font_size: None,
                line_height: None,
            }, // "b"
            FontRun {
                font_id,
                len: 1,
                font_size: None,
                line_height: None,
            }, // "c"
        ];
        let layout = fonts.layout_line(text, px(16.), font_runs, 1.);
        assert_eq!(layout.len, text.len());

        for run in &layout.runs {
            for glyph in &run.glyphs {
                assert!(
                    glyph.index < text.len(),
                    "Glyph index {} is out of bounds for text length {}",
                    glyph.index,
                    text.len()
                );
            }
        }

        // Test with empty text
        let text = "";
        let font_runs = &[];
        let layout = fonts.layout_line(text, px(16.), font_runs, 1.);
        assert_eq!(layout.len, 0);
        assert!(layout.runs.is_empty());
    }

    fn menlo_run(
        fonts: &MacTextSystem,
        len: usize,
        font_size: Option<gpui::Pixels>,
        line_height: Option<gpui::Pixels>,
    ) -> FontRun {
        FontRun {
            font_id: fonts.font_id(&font("Menlo")).unwrap(),
            len,
            font_size,
            line_height,
        }
    }

    #[test]
    fn test_a_run_is_shaped_at_its_own_font_size() {
        let fonts = MacTextSystem::new();
        let runs = [
            menlo_run(&fonts, 2, None, None),
            menlo_run(&fonts, 2, Some(px(32.)), None),
        ];

        let layout = fonts.layout_line("aaaa", px(16.), &runs, 1.);

        assert_eq!(layout.runs.len(), 2);
        assert_eq!(layout.runs[0].font_size, px(16.));
        assert_eq!(layout.runs[1].font_size, px(32.));

        let first_advance =
            layout.runs[0].glyphs[1].position.x - layout.runs[0].glyphs[0].position.x;
        let second_advance =
            layout.runs[1].glyphs[1].position.x - layout.runs[1].glyphs[0].position.x;
        assert!((f32::from(second_advance) - 2. * f32::from(first_advance)).abs() < 0.01);

        assert!(
            (f32::from(layout.runs[1].ascent) - 2. * f32::from(layout.runs[0].ascent)).abs() < 0.01
        );
        assert!(
            (f32::from(layout.runs[1].descent) - 2. * f32::from(layout.runs[0].descent)).abs()
                < 0.01
        );
        assert_eq!(layout.ascent, layout.runs[0].ascent);
        assert_eq!(layout.descent, layout.runs[0].descent);
        assert_eq!(layout.font_size, px(16.));
    }

    #[test]
    fn test_a_run_of_the_line_size_is_the_same_as_one_without_a_size() {
        let fonts = MacTextSystem::new();
        let implicit = fonts.layout_line("aaaa", px(16.), &[menlo_run(&fonts, 4, None, None)], 1.);
        let explicit = fonts.layout_line(
            "aaaa",
            px(16.),
            &[
                menlo_run(&fonts, 2, Some(px(16.)), None),
                menlo_run(&fonts, 2, Some(px(16.)), None),
            ],
            1.,
        );

        assert_eq!(explicit.runs.len(), 1);
        assert_eq!(explicit.runs[0].font_size, px(16.));
        assert_eq!(explicit.width, implicit.width);
    }

    #[test]
    fn test_runs_of_the_same_font_merge_only_with_the_same_line_height() {
        let fonts = MacTextSystem::new();

        let same = fonts.layout_line(
            "aaaa",
            px(16.),
            &[
                menlo_run(&fonts, 2, None, Some(px(20.))),
                menlo_run(&fonts, 2, None, Some(px(20.))),
            ],
            1.,
        );
        assert_eq!(same.runs.len(), 1);
        assert_eq!(same.runs[0].line_height, Some(px(20.)));

        let different = fonts.layout_line(
            "aaaa",
            px(16.),
            &[
                menlo_run(&fonts, 2, None, Some(px(20.))),
                menlo_run(&fonts, 2, None, Some(px(40.))),
            ],
            1.,
        );
        assert_eq!(different.runs.len(), 2);
        assert_eq!(different.runs[0].line_height, Some(px(20.)));
        assert_eq!(different.runs[1].line_height, Some(px(40.)));
        assert_eq!(different.runs[0].glyphs.len(), 2);
        assert_eq!(different.runs[1].glyphs.len(), 2);
    }

    /// The shaper never sees the placeholder of an atomic inline, so the glyphs after it
    /// follow its advance, whatever CoreText would have made of U+FFFC.
    #[test]
    fn test_an_atomic_inline_takes_its_advance_from_the_caller() {
        use gpui::{
            AtomicExtent, InlineBox, InlineContent, TextLayoutOptions, TextRun, TextSystem,
            VerticalAlign, WindowTextSystem,
        };
        use std::sync::Arc;

        let text_system = WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(
            MacTextSystem::new(),
        ))));
        let run = |len| TextRun {
            len,
            font: font("Menlo"),
            ..Default::default()
        };
        let text = "ab\u{FFFC}cd";
        let lines = |inline: &[InlineBox]| {
            text_system
                .shape_text_inline(
                    text.into(),
                    px(20.),
                    &[run(2), run(3), run(2)],
                    None,
                    None,
                    TextLayoutOptions::default(),
                    &[],
                    inline,
                    px(20.),
                )
                .unwrap()
        };
        let atomic = InlineBox {
            range: 2..5,
            parent: None,
            align: VerticalAlign::Baseline,
            line_height: None,
            wraps: None,
            key: None,
            content: InlineContent::Atomic(AtomicExtent {
                advance: px(30.),
                ascent: px(25.),
                descent: px(3.),
            }),
        };
        let lines = lines(&[atomic]);
        let layout: &gpui::WrappedLineLayout = &lines[0];
        let x = |index| f32::from(layout.unwrapped_layout.x_for_index(index));

        let two_characters = x(2);
        assert!(two_characters > 20. && two_characters < 30., "{two_characters}");
        assert!((x(5) - x(2) - 30.).abs() < 0.01);
        assert!((x(8) - x(5) - two_characters).abs() < 0.01);
        assert!((f32::from(layout.width()) - x(8)).abs() < 0.01);

        // Nothing was shaped for the placeholder: its glyph is the one of the box.
        let object_runs: Vec<_> = layout
            .runs()
            .iter()
            .filter(|run| run.inline.object)
            .collect();
        assert_eq!(object_runs.len(), 1);
        assert_eq!(object_runs[0].glyphs.len(), 1);
        assert_eq!(object_runs[0].glyphs[0].index, 2);

        // 25px above the baseline and the strut below it.
        let row = layout.rows(px(20.))[0];
        assert_eq!(row.baseline, px(25.));
        let position = layout.position_for_index(2, px(20.)).unwrap();
        assert_eq!(f32::from(position.x), x(2));
        assert_eq!(position.y, px(0.));
    }

    fn weight_variations(weight: f32) -> FontVariations {
        FontVariations::new([(FontVariations::tag(*b"wght"), weight)])
    }

    /// The system font is variable on every macOS this builds for, and its named
    /// instances each carry every axis.
    fn system_font(weight: f32, variations: FontVariations) -> Font {
        Font {
            weight: FontWeight(weight),
            variations,
            ..font(".SystemUIFont")
        }
    }

    fn width(fonts: &MacTextSystem, font: &Font, text: &str) -> f32 {
        let font_id = fonts.font_id(font).unwrap();
        let run = FontRun {
            font_id,
            len: text.len(),
            font_size: None,
            line_height: None,
        };
        f32::from(fonts.layout_line(text, px(40.), &[run], 1.).width)
    }

    #[test]
    fn test_a_wght_variation_changes_the_advances() {
        let fonts = MacTextSystem::new();
        let at = |wght| width(&fonts, &system_font(400., weight_variations(wght)), "Wave");
        let (light, regular, heavy) = (at(100.), at(400.), at(900.));
        assert!(
            light < regular && regular < heavy,
            "{light} {regular} {heavy}"
        );
        assert!(at(650.) > at(600.) && at(650.) < at(700.));
        // 400 is the default of the axis, which the regular face already has.
        assert_eq!(
            fonts
                .font_id(&system_font(400., weight_variations(400.)))
                .unwrap(),
            fonts
                .font_id(&system_font(400., FontVariations::default()))
                .unwrap()
        );
    }

    #[test]
    fn test_font_weight_is_the_wght_of_a_variable_font() {
        let fonts = MacTextSystem::new();
        let at = |weight| {
            width(
                &fonts,
                &system_font(weight, FontVariations::default()),
                "Wave",
            )
        };
        // The faces of the family are named instances; 650 is none of them.
        assert!(
            at(600.) < at(650.) && at(650.) < at(700.),
            "{} {} {}",
            at(600.),
            at(650.),
            at(700.)
        );
        // `font-variation-settings` wins over `font-weight`.
        assert_eq!(
            width(&fonts, &system_font(700., weight_variations(400.)), "Wave"),
            at(400.)
        );
        // An instance whose weight is asked for exactly is used as it is.
        assert_eq!(
            fonts
                .font_id(&system_font(700., FontVariations::default()))
                .unwrap(),
            fonts
                .font_id(&system_font(700., weight_variations(700.)))
                .unwrap()
        );
    }

    #[test]
    fn test_an_axis_the_font_does_not_have_is_ignored() {
        let fonts = MacTextSystem::new();
        let unknown = FontVariations::new([(FontVariations::tag(*b"ZZZZ"), 5.)]);
        assert_eq!(
            fonts.font_id(&system_font(400., unknown)).unwrap(),
            fonts
                .font_id(&system_font(400., FontVariations::default()))
                .unwrap()
        );
        // A font that is not variable at all.
        let helvetica = Font {
            variations: weight_variations(900.),
            ..font("Helvetica")
        };
        assert_eq!(
            fonts.font_id(&helvetica).unwrap(),
            fonts.font_id(&font("Helvetica")).unwrap()
        );
    }

    #[test]
    fn test_runs_of_one_family_keep_their_own_variations() {
        let fonts = MacTextSystem::new();
        let light = fonts
            .font_id(&system_font(400., weight_variations(100.)))
            .unwrap();
        let heavy = fonts
            .font_id(&system_font(400., weight_variations(900.)))
            .unwrap();
        assert_ne!(light, heavy);

        let layout = fonts.layout_line(
            "WW",
            px(40.),
            &[
                FontRun {
                    font_id: light,
                    len: 1,
                    font_size: None,
                    line_height: None,
                },
                FontRun {
                    font_id: heavy,
                    len: 1,
                    font_size: None,
                    line_height: None,
                },
            ],
            1.,
        );
        // They share a PostScript name, and each run still comes back in its own face.
        assert_eq!(
            layout
                .runs
                .iter()
                .map(|run| run.font_id)
                .collect::<Vec<_>>(),
            [light, heavy]
        );
        let glyph = |id| fonts.glyph_for_char(id, 'W').unwrap();
        assert_ne!(
            fonts.advance(light, glyph(light)).unwrap(),
            fonts.advance(heavy, glyph(heavy)).unwrap()
        );
        let alone = fonts.layout_line(
            "W",
            px(40.),
            &[FontRun {
                font_id: light,
                len: 1,
                font_size: None,
                line_height: None,
            }],
            1.,
        );
        assert_eq!(layout.runs[1].glyphs[0].position.x, alone.width);

        let bounds = |font_id| {
            fonts
                .glyph_raster_bounds(&RenderGlyphParams {
                    font_id,
                    glyph_id: glyph(font_id),
                    font_size: px(40.),
                    subpixel_variant: Default::default(),
                    scale_factor: 1.,
                    is_emoji: false,
                    subpixel_rendering: false,
                    luminance_bucket: 0,
                    font_smoothing: FontSmoothing::Auto,
                })
                .unwrap()
                .size
                .width
        };
        assert!(bounds(light) < bounds(heavy));
    }

    #[test]
    fn test_runs_of_one_family_keep_their_own_features() {
        let fonts = MacTextSystem::new();
        let tabular = Font {
            features: FontFeatures(Arc::new(vec![("tnum".into(), 1)])),
            ..font(".SystemUIFont")
        };
        let plain = font(".SystemUIFont");
        let (tabular_id, plain_id) = (
            fonts.font_id(&tabular).unwrap(),
            fonts.font_id(&plain).unwrap(),
        );
        assert_ne!(tabular_id, plain_id);
        let layout = fonts.layout_line(
            "11",
            px(40.),
            &[
                FontRun {
                    font_id: tabular_id,
                    len: 1,
                    font_size: None,
                    line_height: None,
                },
                FontRun {
                    font_id: plain_id,
                    len: 1,
                    font_size: None,
                    line_height: None,
                },
            ],
            1.,
        );
        assert_eq!(
            layout
                .runs
                .iter()
                .map(|run| run.font_id)
                .collect::<Vec<_>>(),
            [tabular_id, plain_id]
        );
    }

    #[test]
    fn test_variations_compose_with_features() {
        let fonts = MacTextSystem::new();
        let tabular = |wght| Font {
            features: FontFeatures(Arc::new(vec![("tnum".into(), 1)])),
            ..system_font(400., weight_variations(wght))
        };
        let digits = |wght, text| width(&fonts, &tabular(wght), text);
        assert!((digits(700., "1111") - digits(700., "0000")).abs() < 0.01);
        assert!(digits(700., "1111") > digits(400., "1111"));
    }

    /// The content area and the line `line-height: normal` makes, in Chrome on macOS, of
    /// `Hxg` in a font at a size (`fixtures/line-box-oracle.json` of gpuix:
    /// `<family>.<weight>.<size>.normal` and its `.text`; `line-box-oracle-x2.json` on a display of
    /// device scale 2). Helvetica, Times and Courier have their ascent widened by a sixth of the
    /// content area, and the other fonts do not. Chrome zooms the font to the display before it
    /// measures it, so at scale 2 each metric is a whole device pixel: Helvetica Neue 20px is 23.5
    /// CSS pixels of content where it is 23 at scale 1, and Times 14px is 16 where it is 17.
    #[test]
    fn test_the_line_box_of_a_font_is_the_one_chrome_makes() {
        let fonts = MacTextSystem::new();
        // (family, weight, size, content and normal at scale 1, content and normal at scale 2)
        let cases: &[(&str, f32, f32, (f32, f32), (f32, f32))] = &[
            ("Helvetica Neue", 400., 20., (23., 24.), (23.5, 24.)),
            ("Helvetica Neue", 700., 20., (24., 25.), (24., 24.5)),
            ("Helvetica Neue", 400., 24., (28., 29.), (28., 28.5)),
            ("Helvetica Neue", 700., 12., (15., 15.), (14., 14.5)),
            ("Helvetica", 400., 12., (14., 14.), (14., 14.)),
            ("Helvetica", 700., 20., (23., 23.), (23., 23.)),
            ("Helvetica", 400., 24., (28., 28.), (27.5, 27.5)),
            ("Menlo", 400., 16., (19., 19.), (19., 19.)),
            ("Menlo", 700., 20., (24., 24.), (23., 23.)),
            ("Menlo", 400., 24., (28., 28.), (28., 28.)),
            ("Times", 400., 14., (17., 17.), (16., 16.)),
            ("Times", 700., 16., (18., 18.), (18.5, 18.5)),
            ("Courier", 400., 20., (23., 23.), (23., 23.)),
            ("Courier", 700., 12., (14., 14.), (14., 14.)),
        ];
        for (family, weight, size, at_1, at_2) in cases {
            let font = Font {
                weight: FontWeight(*weight),
                ..font(*family)
            };
            let font_id = fonts.font_id(&font).unwrap();
            for (scale, (content, normal)) in [(1., at_1), (2., at_2)] {
                let line_box = fonts.font_metrics(font_id).line_box(px(*size), scale);
                let label = format!("{family} {weight} {size}px at scale {scale}");
                assert_eq!(
                    f32::from(line_box.ascent + line_box.descent),
                    *content,
                    "{label}: the content area"
                );
                assert_eq!(
                    f32::from(line_box.normal_line_height()),
                    *normal,
                    "{label}: line-height: normal"
                );
            }
        }
    }

    #[test]
    fn test_only_helvetica_times_and_courier_widen_their_ascent() {
        let fonts = MacTextSystem::new();
        let widens = |family: &str| {
            fonts
                .font_metrics(fonts.font_id(&font(family)).unwrap())
                .widens_ascent
        };
        assert!(widens("Helvetica"));
        assert!(widens("Times"));
        assert!(widens("Courier"));
        assert!(!widens("Helvetica Neue"));
        assert!(!widens("Menlo"));
        assert!(!widens("Courier New"));
    }

    #[test]
    fn test_a_run_is_shaped_with_the_line_box_of_its_font_at_its_size() {
        let fonts = MacTextSystem::new();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        let bold = fonts
            .font_id(&Font {
                weight: FontWeight::BOLD,
                ..font("Helvetica Neue")
            })
            .unwrap();
        let run = |font_id, len, font_size| FontRun {
            font_id,
            len,
            font_size,
            line_height: None,
        };
        let layout = fonts.layout_line(
            "aabb",
            px(20.),
            &[run(helvetica, 2, None), run(bold, 2, None)],
            1.,
        );
        // The strut is the font of the first run, and a run in another font has its own box.
        assert_eq!((layout.ascent, layout.descent), (px(18.), px(5.)));
        assert_eq!(layout.runs.len(), 2);
        assert_eq!((layout.runs[0].ascent, layout.runs[0].descent), (px(18.), px(5.)));
        // Bold Helvetica Neue at 20px is 24px of content in Chrome.
        assert_eq!((layout.runs[1].ascent, layout.runs[1].descent), (px(20.), px(4.)));
        assert_eq!(layout.scale_factor, 1.);
    }

    /// The same runs shaped for a display of scale 2 are measured in device pixels, as Chrome does
    /// (Helvetica 20px: 37 and 9 device pixels, and a sixth of them more, where it is 18 and 5 CSS
    /// pixels at scale 1), and the layout says which scale it is for.
    #[test]
    fn test_a_run_is_shaped_with_the_line_box_of_its_font_in_device_pixels() {
        let fonts = MacTextSystem::new();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        let bold = fonts
            .font_id(&Font {
                weight: FontWeight::BOLD,
                ..font("Helvetica Neue")
            })
            .unwrap();
        let run = |font_id, len| FontRun {
            font_id,
            len,
            font_size: None,
            line_height: None,
        };
        let layout = fonts.layout_line("aabb", px(20.), &[run(helvetica, 2), run(bold, 2)], 2.);
        assert_eq!(layout.scale_factor, 2.);
        // Helvetica 20px at scale 2: 31 + 9 device pixels, the ascent widened by 6.
        assert_eq!((layout.ascent, layout.descent), (px(18.5), px(4.5)));
        assert_eq!((layout.runs[0].ascent, layout.runs[0].descent), (px(18.5), px(4.5)));
        // Bold Helvetica Neue 20px is 24 of content at scale 2 as well (24.5 of normal height).
        assert_eq!(
            f32::from(layout.runs[1].ascent + layout.runs[1].descent),
            24.
        );
    }

    /// Inter Variable cut down to "Hamburgefonstiv 0123": an `opsz` axis of 14 to 32, and a
    /// font that is not the UI font, whose optical size CoreText always sets from the size.
    /// The file is the fixture of gpuix's font-smoothing test, so the test needs the checkout
    /// this crate is a submodule of.
    const INTER_SUBSET: &str =
        "../../../packages/react/src/__tests__/fixtures/inter-subset.ttf";

    /// What Chrome lays "Hamburgefonstiv 0123" out as in `fixtures/inter-subset.ttf`, by
    /// font size, `wght` and `opsz` (`font-smoothing-oracle.json`, scale 1).
    const CHROME_ADVANCES: [(f32, f32, f32, f32); 8] = [
        (14., 400., 14., 149.484375),
        (14., 500., 16., 150.484375),
        (14., 550., 18., 150.53125),
        (14., 700., 25., 150.96875),
        (24., 400., 14., 256.265625),
        (24., 500., 16., 257.96875),
        (24., 550., 18., 258.0625),
        (24., 700., 25., 258.796875),
    ];

    /// The cap height and x-height of `fixtures/inter-subset.ttf` are the OS/2 table's, as Skia
    /// takes them: 1490 and 1118 of 2048 units, where CoreText says 1056 for the x-height (and
    /// 1097 at `wght` 700). Chrome's label trimmed to the cap or the x-height is that tall in
    /// every instance of the font (`x-height-oracle.json`, scale 1, 1/64 px).
    #[test]
    fn test_the_cap_height_and_x_height_are_the_os2_tables_in_every_instance() {
        let fonts = MacTextSystem::new();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(INTER_SUBSET);
        let Ok(file) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not in this checkout", path.display());
            return;
        };
        fonts.add_fonts(vec![file.into()]).unwrap();
        let instances = [
            (400., vec![]),
            (700., vec![]),
            (
                550.,
                vec![
                    (FontVariations::tag(*b"wght"), 550.),
                    (FontVariations::tag(*b"opsz"), 18.),
                ],
            ),
            (
                900.,
                vec![
                    (FontVariations::tag(*b"wght"), 900.),
                    (FontVariations::tag(*b"opsz"), 32.),
                ],
            ),
        ];
        // (size, x-height, cap height) in Chrome
        let chrome = [(12., 6.546875, 8.734375), (13., 7.09375, 9.453125)];
        for (weight, variations) in instances {
            let font_id = fonts
                .font_id(&Font {
                    weight: FontWeight(weight),
                    variations: FontVariations::new(variations),
                    ..font("Inter Variable")
                })
                .unwrap();
            let metrics = fonts.font_metrics(font_id);
            assert_eq!(metrics.x_height, 1118., "wght {weight}: x-height in units");
            assert_eq!(metrics.cap_height, 1490., "wght {weight}: cap height in units");
            for (size, x_height, cap_height) in chrome {
                let x = f32::from(metrics.x_height(px(size)));
                let cap = f32::from(metrics.cap_height(px(size)));
                assert!((x - x_height).abs() <= 1. / 64., "{size}px: x-height {x}, Chrome {x_height}");
                assert!((cap - cap_height).abs() <= 1. / 64., "{size}px: cap {cap}, Chrome {cap_height}");
            }
        }
    }

    #[test]
    fn test_heights_come_from_an_os2_table_of_version_2_with_sane_values() {
        let table = |version: u16, x_height: i16, cap_height: i16, len: usize| {
            let mut table = vec![0; len];
            table[..2].copy_from_slice(&version.to_be_bytes());
            if len >= 90 {
                table[86..88].copy_from_slice(&x_height.to_be_bytes());
                table[88..90].copy_from_slice(&cap_height.to_be_bytes());
            }
            table
        };
        assert_eq!(super::os2_heights(&table(4, 1118, 1490, 96), 2048), (Some(1118.), Some(1490.)));
        assert_eq!(super::os2_heights(&table(2, 1118, 1490, 90), 2048), (Some(1118.), Some(1490.)));
        assert_eq!(super::os2_heights(&table(1, 1118, 1490, 96), 2048), (None, None));
        assert_eq!(super::os2_heights(&table(4, 1118, 1490, 88), 2048), (None, None));
        assert_eq!(super::os2_heights(&table(4, 0, 1490, 96), 2048), (None, Some(1490.)));
        assert_eq!(super::os2_heights(&table(4, 1118, -5, 96), 2048), (Some(1118.), None));
        assert_eq!(super::os2_heights(&table(4, 4096, 4095, 96), 2048), (None, Some(4095.)));
    }

    #[test]
    fn test_an_explicit_opsz_is_the_one_the_text_is_drawn_with() {
        let fonts = MacTextSystem::new();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(INTER_SUBSET);
        let Ok(file) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not in this checkout", path.display());
            return;
        };
        fonts.add_fonts(vec![file.into()]).unwrap();
        let inter = |wght, opsz| Font {
            weight: FontWeight(wght),
            variations: FontVariations::new([
                (FontVariations::tag(*b"wght"), wght),
                (FontVariations::tag(*b"opsz"), opsz),
            ]),
            ..font("Inter Variable")
        };
        for (size, wght, opsz, chrome) in CHROME_ADVANCES {
            let font_id = fonts.font_id(&inter(wght, opsz)).unwrap();
            let font_id = fonts.font_id(&inter(wght, opsz)).unwrap();
            let run = FontRun {
                font_id,
                len: "Hamburgefonstiv 0123".len(),
                font_size: None,
                line_height: None,
            };
            let advance =
                f32::from(fonts.layout_line("Hamburgefonstiv 0123", px(size), &[run], 1.).width);
            assert!(
                (advance - chrome).abs() < 0.02,
                "{size}px wght {wght} opsz {opsz}: {advance}, Chrome {chrome}"
            );
        }
    }

    #[test]
    fn test_a_registered_single_font_is_held_by_core_text_alone() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(INTER_SUBSET);
        let Ok(file) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not in this checkout", path.display());
            return;
        };
        assert!(matches!(
            super::owned_font_handle(file).unwrap(),
            font_kit::handle::Handle::Native { .. }
        ));
        // A collection has no descriptor-from-data path, so font-kit unpacks it.
        let collection = b"ttcf\0\x01\0\0\0\0\0\0".to_vec();
        assert!(matches!(
            super::owned_font_handle(collection).unwrap(),
            font_kit::handle::Handle::Memory { .. }
        ));
    }
}
