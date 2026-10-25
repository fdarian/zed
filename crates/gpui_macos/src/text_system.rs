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
    geometry::{CGAffineTransform, CGSize},
};
use core_text::{
    font::CTFont,
    font_collection::CTFontCollectionRef,
    font_descriptor::{
        CTFontDescriptor, kCTFontOrientationDefault, kCTFontSlantTrait, kCTFontSymbolicTrait,
        kCTFontWeightTrait, kCTFontWidthTrait,
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
    transform2d::{Matrix2x2F, Transform2F},
    vector::Vector2F,
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc, sync::OnceLock};

use crate::open_type::{
    VariationAxis, apply_features_and_fallbacks, apply_variations, copy_for_advances,
    current_variations, font_table_tags, variation_axes,
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
    /// Where Chrome takes the advances of each of `fonts` from, by the same index.
    advance_sources: Vec<AdvanceSource>,
    /// The copies of a face that give its advances, by face, size and scale factor
    /// (see [`MacTextSystemState::advance_font`]).
    advance_fonts: Mutex<HashMap<(FontId, u32, u32), CTFont>>,
}

/// Where Chrome gets the advances of a font from. CoreText has its own, and they are the same as
/// Chrome's for a font without a `trak` table. A font with one is advanced differently by Blink
/// (`HarfBuzzFace`, `harfbuzz_face.cc`) and by Skia (`SkScalerContext_mac_ct.cpp`), which CoreText
/// only does when the font is made as they make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdvanceSource {
    /// The advances CoreText gives.
    CoreText,
    /// A `trak` table and no `sbix` (PingFang): HarfBuzz tracks the advances itself, at the CSS size.
    Tracked,
    /// A `trak` table and an `sbix` (Apple Color Emoji): Skia's advances, which have no tracking,
    /// at the device size.
    Untracked,
}

impl AdvanceSource {
    fn of(font: &CTFont) -> Self {
        let tables = font_table_tags(font);
        let has = |tag: &[u8; 4]| tables.contains(&u32::from_be_bytes(*tag));
        match (has(b"trak"), has(b"sbix")) {
            (false, _) => Self::CoreText,
            (true, false) => Self::Tracked,
            (true, true) => Self::Untracked,
        }
    }
}

/// The advances of a face as Chrome measures them: from `font`, in units of which a pixel is
/// `per_pixel`.
struct AdvanceFont {
    font: CTFont,
    per_pixel: f64,
}

impl AdvanceFont {
    fn advances(&self, glyphs: &[CGGlyph]) -> Vec<f64> {
        let mut advances = vec![CGSize::new(0., 0.); glyphs.len()];
        // SAFETY: one advance out for each glyph in.
        unsafe {
            self.font.get_advances_for_glyphs(
                kCTFontOrientationDefault,
                glyphs.as_ptr(),
                advances.as_mut_ptr(),
                glyphs.len() as CFIndex,
            );
        }
        advances
            .iter()
            .map(|advance| advance.width / self.per_pixel)
            .collect()
    }
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
            advance_sources: Vec::new(),
            advance_fonts: Mutex::new(HashMap::default()),
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

    fn glyph_advance(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
        scale_factor: f32,
    ) -> Result<f64> {
        self.0
            .read()
            .glyph_advance(font_id, glyph_id, font_size, scale_factor)
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
        self.advance_sources
            .push(AdvanceSource::of(&font.native_font()));
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

    /// The font that gives the advances of `font_id` at `size` logical pixels, as Chrome measures
    /// them on a display of `scale_factor` device pixels to a pixel, and `None` for a face whose
    /// advances are CoreText's own (see [`AdvanceSource`]).
    ///
    /// HarfBuzz tracks a font with a `trak` table by the size it is told, which is the CSS size, and
    /// CoreText only does so for a copy that has the optical size set. Skia measures a copy at the
    /// device size with its tracking turned off.
    fn advance_font(&self, font_id: FontId, size: f32, scale_factor: f32) -> Option<AdvanceFont> {
        let (copy_size, tracking, per_pixel) = match self.advance_sources[font_id.0] {
            AdvanceSource::CoreText => return None,
            AdvanceSource::Tracked => (f64::from(size), true, 1.),
            AdvanceSource::Untracked => (
                f64::from(size) * f64::from(scale_factor),
                false,
                f64::from(scale_factor),
            ),
        };
        let key = (font_id, size.to_bits(), scale_factor.to_bits());
        let mut fonts = self.advance_fonts.lock();
        let font = match fonts.get(&key) {
            Some(font) => font.clone(),
            None => {
                let font =
                    copy_for_advances(&self.sized_font(font_id, size), copy_size, copy_size, tracking);
                if fonts.len() >= SIZED_FONTS_LIMIT {
                    fonts.clear();
                }
                fonts.insert(key, font.clone());
                font
            }
        };
        Some(AdvanceFont { font, per_pixel })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(size_from_vector2f(
            self.fonts[font_id.0].advance(glyph_id.0)?,
        ))
    }

    fn glyph_advance(
        &self,
        font_id: FontId,
        glyph_id: GlyphId,
        font_size: Pixels,
        scale_factor: f32,
    ) -> Result<f64> {
        if let Some(advance_font) = self.advance_font(font_id, f32::from(font_size), scale_factor) {
            return Ok(advance_font.advances(&[glyph_id.0 as CGGlyph])[0]);
        }
        let font = self.sized_font(font_id, f32::from(font_size));
        let mut advance = CGSize::new(0., 0.);
        // SAFETY: one glyph in, one advance out.
        unsafe {
            font.get_advances_for_glyphs(
                kCTFontOrientationDefault,
                &(glyph_id.0 as u16),
                &mut advance,
                1,
            );
        }
        Ok(advance.width)
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

    /// Whether `params` is a glyph of an emoji font that Chrome sets at the device size, with the
    /// advance of that size: it is drawn at that size too, which is not the logical size scaled. Apple
    /// Color Emoji draws a glyph larger for its em at a small size than at a large one (an emoji
    /// is 16 wide at 13 px and 24 at 26 px), so a glyph drawn at 13 px scaled by 2 is wider than its
    /// advance at 26 px.
    fn draws_at_device_size(&self, params: &RenderGlyphParams) -> bool {
        params.is_emoji && self.advance_sources[params.font_id.0] == AdvanceSource::Untracked
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = &self.fonts[params.font_id.0];
        let (size, factor) = if self.draws_at_device_size(params) {
            (f32::from(params.font_size) * params.scale_factor, 1.)
        } else {
            (f32::from(params.font_size), params.scale_factor)
        };
        let scale = if params.transform.is_identity() {
            Transform2F::from_scale(factor)
        } else {
            let linear = params.transform.linear();
            Transform2F {
                matrix: Matrix2x2F::row_major(
                    linear[0][0] * factor,
                    linear[0][1] * factor,
                    linear[1][0] * factor,
                    linear[1][1] * factor,
                ),
                vector: Vector2F::zero(),
            }
        };
        let bounds: Bounds<DevicePixels> = bounds_from_rect_i(font.raster_bounds(
            params.glyph_id.0,
            size,
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

            let device_font = self
                .draws_at_device_size(params)
                .then(|| {
                    self.advance_font(
                        params.font_id,
                        f32::from(params.font_size),
                        params.scale_factor,
                    )
                })
                .flatten();
            // A glyph drawn at the device size is in device pixels, with no scale.
            let scale = if device_font.is_some() { 1. } else { params.scale_factor };
            let subpixel_shift = params
                .subpixel_variant
                .map(|v| v as f32 / SUBPIXEL_VARIANTS_X as f32);
            let draw_at = if params.transform.is_identity() {
                // Move the origin to bottom left and account for scaling, this
                // makes drawing text consistent with the font-kit's raster_bounds.
                cx.translate(
                    -glyph_bounds.origin.x.0 as CGFloat,
                    (glyph_bounds.origin.y.0 + glyph_bounds.size.height.0) as CGFloat,
                );
                cx.scale(scale as CGFloat, scale as CGFloat);
                CGPoint::new(
                    (subpixel_shift.x / scale) as CGFloat,
                    (subpixel_shift.y / scale) as CGFloat,
                )
            } else {
                // The glyph is drawn through the transform around its origin, which is where
                // the subpixel shift (in device pixels, along the baseline of the window) puts it
                // before the transform applies. CoreGraphics' y axis points up where the
                // transform's points down, hence the signs.
                cx.translate(
                    (subpixel_shift.x - glyph_bounds.origin.x.0 as f32) as CGFloat,
                    (glyph_bounds.origin.y.0 as f32 + bitmap_size.height.0 as f32 - subpixel_shift.y) as CGFloat,
                );
                let linear = params.transform.linear();
                let factor = scale;
                cx.concat_ctm(CGAffineTransform::new(
                    (linear[0][0] * factor) as CGFloat,
                    (-linear[1][0] * factor) as CGFloat,
                    (-linear[0][1] * factor) as CGFloat,
                    (linear[1][1] * factor) as CGFloat,
                    0.,
                    0.,
                ));
                CGPoint::new(0., 0.)
            };
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
            let font = match device_font {
                Some(device_font) => device_font.font,
                None => self.sized_font(params.font_id, f32::from(params.font_size)),
            };
            font.draw_glyphs(&[params.glyph_id.0 as CGGlyph], &[draw_at], cx);

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
        // How far Chrome's advances have moved the glyphs so far, in logical pixels.
        let mut shift = 0.;
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
                self.id_for_native_font(font.clone())
            };

            // The glyphs CoreText advanced are moved to where Chrome's advances put them. The
            // kerning CoreText applied to a pair stays.
            let glyph_ids = run.glyphs();
            let moves = self
                .advance_font(font_id, f32::from(input_run.font_size), scale_factor)
                .map(|advance_font| {
                    let mut coretext = vec![CGSize::new(0., 0.); glyph_ids.len()];
                    // SAFETY: one advance out for each glyph in.
                    unsafe {
                        font.get_advances_for_glyphs(
                            kCTFontOrientationDefault,
                            glyph_ids.as_ptr(),
                            coretext.as_mut_ptr(),
                            glyph_ids.len() as CFIndex,
                        );
                    }
                    advance_font
                        .advances(&glyph_ids)
                        .into_iter()
                        .zip(coretext)
                        .map(|(chrome, coretext)| chrome - coretext.width)
                        .collect::<Vec<f64>>()
                });
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
            for (glyph_ix, ((&glyph_id, position), &glyph_utf16_ix)) in glyph_ids
                .iter()
                .zip(run.positions().iter())
                .zip(run.string_indices().iter())
                .enumerate()
            {
                let glyph_utf16_ix = usize::try_from(glyph_utf16_ix).unwrap();
                if ix_converter.utf16_ix > glyph_utf16_ix {
                    // We cannot reuse current index converter, as it can only seek forward. Restart the search.
                    ix_converter = StringIndexConverter::new(text);
                }
                ix_converter.advance_to_utf16_ix(glyph_utf16_ix);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(glyph_id as u32),
                    position: point((position.x + shift) as f32, position.y as f32).map(px),
                    index: ix_converter.utf8_ix,
                    is_emoji: self.is_emoji(font_id),
                });
                if let Some(moves) = &moves {
                    shift += moves[glyph_ix];
                }
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
            width: (typographic_bounds.width + shift).into(),
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
        legacy_kerning: font
            .load_font_table(u32::from_be_bytes(*b"GPOS"))
            .is_none(),
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
        // Each of the three runs is measured on its own, in layout units rounded up.
        assert!((f32::from(layout.width()) - x(8)).abs() < 3. / 64.);

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

    /// A row that breaks at a soft hyphen is as wide as its text and the hyphen Blink adds, which
    /// the layout knows the font and glyph of; the soft hyphen is not shaped to anything visible.
    #[test]
    fn test_a_row_that_breaks_at_a_soft_hyphen_has_a_hyphen_of_the_font_of_its_text() {
        use gpui::{TextLayoutOptions, TextRun, TextSystem, WindowTextSystem};
        use std::sync::Arc;

        let fonts = Arc::new(MacTextSystem::new());
        let text_system = WindowTextSystem::new(Arc::new(TextSystem::new(fonts.clone())));
        let text = "abc\u{AD}defghijkl";
        let run = TextRun {
            len: text.len(),
            font: font("Helvetica Neue"),
            ..Default::default()
        };
        let lines = |wrap_width: Option<f32>| {
            text_system
                .shape_text_inline(
                    text.into(),
                    px(16.),
                    &[run.clone()],
                    wrap_width.map(px),
                    None,
                    TextLayoutOptions::default(),
                    &[],
                    &[],
                    px(20.),
                )
                .unwrap()
        };
        let whole = lines(None);
        let whole: &gpui::WrappedLineLayout = &whole[0];
        let x = |index| f32::from(whole.unwrapped_layout.x_for_index(index));
        assert!(
            whole.runs().iter().flat_map(|run| &run.glyphs).all(|glyph| glyph.index != 3),
            "the soft hyphen is not shaped to a glyph"
        );

        // "abc" and a hyphen fit, the whole word does not, and the rest of it fills a row.
        let wrapped = lines(Some(x(14) - x(5) + 2.));
        let layout: &gpui::WrappedLineLayout = &wrapped[0];
        assert_eq!(layout.wrap_boundaries().len(), 1);
        let hyphen = layout.hyphen_of_row(0).expect("the row breaks at the soft hyphen");
        assert_eq!(layout.hyphen_of_row(1), None);
        assert_eq!(hyphen.font_id, layout.runs()[0].font_id);
        assert_eq!(hyphen.font_size, px(16.));
        let expected = fonts
            .glyph_for_char(hyphen.font_id, '\u{2010}')
            .or_else(|| fonts.glyph_for_char(hyphen.font_id, '-'))
            .unwrap();
        assert_eq!(hyphen.glyph_id, expected);
        assert!(hyphen.advance > px(3.) && hyphen.advance < px(10.), "{:?}", hyphen.advance);

        let widths: Vec<f32> = layout.line_widths().map(f32::from).collect();
        let hyphen_width = f32::from(hyphen.advance);
        assert!(
            (widths[0] - (x(3) + hyphen_width)).abs() < 3. / 64.,
            "{widths:?} abc is {} and the hyphen {hyphen_width}",
            x(3)
        );
        assert!(layout.hyphen_width(0) >= hyphen.advance);
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
                    transform: gpui::GlyphTransform::IDENTITY,
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

    /// Helvetica and Times kern through a legacy `kern` table, which Chrome's HarfBuzz splits
    /// between the two glyphs of a pair: a word after a kerned space starts half a kern later
    /// than CoreText puts it, and a word before one ends half a kern earlier. The widths are
    /// headless Chrome's, in 64ths of a pixel (`text-measure-oracle.json` of gpuix at scale 1).
    #[test]
    fn test_a_legacy_kern_is_split_between_its_glyphs_as_chrome_measures_it() {
        let platform: Arc<dyn PlatformTextSystem> = Arc::new(MacTextSystem::new());
        for family in ["Helvetica", "Times"] {
            let font_id = platform.font_id(&font(family)).unwrap();
            assert!(platform.font_metrics(font_id).legacy_kerning, "{family}");
        }
        let text_system = gpui::WindowTextSystem::new(Arc::new(gpui::TextSystem::new(platform)));
        let hanging = gpui::TextLayoutOptions {
            hang_trailing_spaces: true,
            ..Default::default()
        };
        for (family, size, text, min_content, width) in [
            ("Helvetica", 13., "a To", 871., 1558.),
            ("Helvetica", 13., "a AV", 1025., 1697.),
            ("Helvetica", 16., "a Wa", 1499., 2353.),
            ("Times", 13., "a Ta", 813., 1383.),
            ("Times", 16., "bA b", 1224., 1964.),
        ] {
            let text: gpui::SharedString = text.to_string().into();
            let run = gpui::TextRun {
                len: text.len(),
                font: font(family),
                ..Default::default()
            };
            let measured = text_system
                .min_content_width(text.clone(), px(size), &[run.clone()], hanging, &[])
                .unwrap();
            assert_eq!(measured, px(min_content / 64.), "{family} {size} {text:?} at min-content");
            let lines = text_system
                .shape_text(text.clone(), px(size), &[run], None, None)
                .unwrap();
            assert_eq!(lines[0].size(px(20.)).width, px(width / 64.), "{family} {size} {text:?}");
        }
    }

    /// Blink measures every text node as an item of its own and shapes the nodes of one font
    /// together, so the edge between two is ceiled twice and a kern pair across it is split
    /// between them. The widths are headless Chrome's, in 64ths of a pixel (`nodes` of
    /// `text-measure-oracle.json` of gpuix at scale 1), the nodes of a text separated by `|`.
    #[test]
    fn test_each_text_node_is_an_item_and_a_kern_across_two_is_split() {
        let platform: Arc<dyn PlatformTextSystem> = Arc::new(MacTextSystem::new());
        let text_system = gpui::WindowTextSystem::new(Arc::new(gpui::TextSystem::new(platform)));
        let hanging = gpui::TextLayoutOptions {
            hang_trailing_spaces: true,
            ..Default::default()
        };
        // The same text in one node and in two is two layouts, though the runs are alike.
        let quick: gpui::SharedString = "Quick brown fox jumps".into();
        let run = |len| gpui::TextRun {
            len,
            font: font("Helvetica Neue"),
            ..Default::default()
        };
        let width_of = |runs: &[gpui::TextRun]| {
            let lines = text_system.shape_text(quick.clone(), px(13.), runs, None, None).unwrap();
            lines[0].size(px(20.)).width
        };
        assert_eq!(width_of(&[run(21)]), px(8600. / 64.));
        assert_eq!(width_of(&[run(12), run(9)]), px(8601. / 64.));
        for (family, size, nodes, min_content, width) in [
            ("Helvetica Neue", 13., "Quick brown |fox jumps", 2326., 8601.),
            ("Helvetica Neue", 13., "Fract|ional wid|ths", 3668., 6349.),
            ("Helvetica", 13., "a |To", 872., 1559.),
            ("Helvetica", 13., "a| To", 872., 1559.),
            ("Helvetica", 13., "T|o To", 880., 1975.),
            ("Helvetica", 13., "To |To", 879., 1975.),
            ("Helvetica", 16., "a |Wa", 1499., 2353.),
            ("Times", 13., "a |Ta", 813., 1383.),
            ("Times", 16., "bA| b", 1224., 1964.),
            ("Times", 16., "b|A b", 1224., 1964.),
        ] {
            let text: gpui::SharedString = nodes.replace('|', "").into();
            let runs: Vec<_> = nodes
                .split('|')
                .map(|node| gpui::TextRun {
                    len: node.len(),
                    font: font(family),
                    ..Default::default()
                })
                .collect();
            let measured = text_system
                .min_content_width(text.clone(), px(size), &runs, hanging, &[])
                .unwrap();
            assert_eq!(measured, px(min_content / 64.), "{family} {size} {nodes:?} at min-content");
            let lines = text_system.shape_text(text, px(size), &runs, None, None).unwrap();
            assert_eq!(lines[0].size(px(20.)).width, px(width / 64.), "{family} {size} {nodes:?}");
        }
    }

    /// A real CoreText glyph turned 30 degrees: rasterized through the matrix and drawn upright it is
    /// as heavy as the glyph resampled through the matrix, and sharper.
    #[test]
    fn test_a_glyph_rasterized_through_a_rotation_is_sharper_than_one_resampled_through_it() {
        use gpui::{
            AtlasKey, ContentMask, DevicePixels, GlyphTransform, MonochromeSprite, PlatformHeadlessRenderer,
            ScaledPixels, Scene, TransformationMatrix, point,
        };
        use gpui_apple::metal_renderer::MetalHeadlessRenderer;

        const SIDE: i32 = 120;
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica")).unwrap();
        let glyph_id = fonts.glyph_for_char(font_id, 'W').unwrap();
        let radians = 30f32.to_radians();
        let linear = [[radians.cos(), -radians.sin()], [radians.sin(), radians.cos()]];
        let scale_factor = 2.;
        let params = |transform| RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: px(24.),
            subpixel_variant: Default::default(),
            scale_factor,
            is_emoji: false,
            subpixel_rendering: false,
            luminance_bucket: 0,
            font_smoothing: FontSmoothing::Antialiased,
            transform,
        };

        let coverage = |transform: GlyphTransform, drawn_through: Option<TransformationMatrix>| {
            let mut renderer = MetalHeadlessRenderer::new();
            let atlas = renderer.sprite_atlas();
            let params = params(transform);
            let raster_bounds = fonts.glyph_raster_bounds(&params).unwrap();
            let tile = atlas
                .get_or_insert_with(&AtlasKey::Glyph(params.clone()), &mut || {
                    let (size, bytes) = fonts.rasterize_glyph(&params, raster_bounds)?;
                    Ok(Some((size, bytes.into())))
                })
                .unwrap()
                .unwrap();
            let origin = point(ScaledPixels(SIDE as f32 / 2.), ScaledPixels(SIDE as f32 / 2.));
            let mut scene = Scene::default();
            scene.insert_primitive(MonochromeSprite {
                order: 0,
                pad: 0,
                bounds: gpui::Bounds {
                    origin: match drawn_through {
                        // Upright in its own space, which the matrix turns around the glyph's origin.
                        Some(_) => point(
                            ScaledPixels(raster_bounds.origin.x.0 as f32),
                            ScaledPixels(raster_bounds.origin.y.0 as f32),
                        ),
                        None => origin
                            + point(
                                ScaledPixels(raster_bounds.origin.x.0 as f32),
                                ScaledPixels(raster_bounds.origin.y.0 as f32),
                            ),
                    },
                    size: tile.bounds.size.map(Into::into),
                },
                content_mask: ContentMask {
                    bounds: gpui::Bounds {
                        origin: point(ScaledPixels(0.), ScaledPixels(0.)),
                        size: gpui::size(ScaledPixels(SIDE as f32), ScaledPixels(SIDE as f32)),
                    },
                    ..Default::default()
                },
                color: gpui::red(),
                tile,
                transformation: drawn_through.unwrap_or_else(TransformationMatrix::unit),
                local_clip: ContentMask::unbounded(),
                clip_transformation: TransformationMatrix::unit(),
            });
            scene.finish();
            let image = renderer
                .render_scene_to_image(&scene, gpui::size(DevicePixels(SIDE), DevicePixels(SIDE)))
                .unwrap();
            // Coverage is the red of red text over black.
            let grid: Vec<Vec<f32>> = (0..SIDE as u32)
                .map(|y| (0..SIDE as u32).map(|x| image.get_pixel(x, y).0[0] as f32 / 255.).collect())
                .collect();
            let (mut ink, mut squares, mut edge, mut centroid) = (0f32, 0f32, 0f32, (0f32, 0f32));
            for (y, row) in grid.iter().enumerate() {
                for (x, c) in row.iter().enumerate() {
                    ink += c;
                    centroid.0 += c * x as f32;
                    centroid.1 += c * y as f32;
                    squares += c * c;
                    if let Some(next) = row.get(x + 1) {
                        edge += (next - c).powi(2);
                    }
                    if let Some(below) = grid.get(y + 1) {
                        edge += (below[x] - c).powi(2);
                    }
                }
            }
            (ink, squares / ink, edge / ink, (centroid.0 / ink, centroid.1 / ink))
        };

        let turn = TransformationMatrix::affine(linear, [SIDE as f32 / 2., SIDE as f32 / 2.]);
        // The scale factor is in the raster of either; the matrix works on device pixels.
        let (exact_ink, exact_solid, exact_edge, exact_centroid) =
            coverage(GlyphTransform::quantize(linear), None);
        let (resampled_ink, resampled_solid, resampled_edge, resampled_centroid) =
            coverage(GlyphTransform::IDENTITY, Some(turn));
        println!("exact {exact_ink} {exact_solid} {exact_edge}; resampled {resampled_ink} {resampled_solid} {resampled_edge}");
        assert!(exact_ink > 10., "the glyph is drawn: {exact_ink}");
        assert!(
            (exact_centroid.0 - resampled_centroid.0).abs() < 0.75 && (exact_centroid.1 - resampled_centroid.1).abs() < 0.75,
            "the same place and turn: {exact_centroid:?} against {resampled_centroid:?}"
        );
        assert!(
            (exact_ink / resampled_ink - 1.).abs() < 0.04,
            "same weight: {exact_ink} against {resampled_ink}"
        );
        assert!(exact_solid > resampled_solid * 1.03, "more of the ink at full coverage: {exact_solid} against {resampled_solid}");
        assert!(exact_edge > resampled_edge * 1.05, "steeper edges: {exact_edge} against {resampled_edge}");
    }

    /// Blink shapes the end of a line again where it breaks between two kerned letters
    /// (`ShapingLineBreaker`): the letter before the break loses its kern, which moves the break of a cut
    /// word or of `break-all` and widens the line by it, in Helvetica and Times (a `kern` table that
    /// HarfBuzz splits between the glyphs) alike. The lines and their widths are headless Chrome's
    /// at scale 1: each row is the text, the width of the box and for each line its text and its width, of which the
    /// first is checked.
    #[test]
    fn test_a_line_that_breaks_between_kerned_letters_is_shaped_again_at_its_end() {
        use gpui::{OverflowWrap, TextLayoutOptions, TextRun, TextSystem, WindowTextSystem, WordBreak};
        use std::sync::Arc;

        let text_system = WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(MacTextSystem::new()))));
        let long = "AVATARTOWERWAYWARDTYPOGRAPHYVALVEFAVOURITEWATERYTAVERNVOYAGE";
        let cut = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Anywhere,
            ..Default::default()
        };
        let break_all = TextLayoutOptions {
            word_break: WordBreak::BreakAll,
            ..Default::default()
        };
        let rows: [(&str, &str, TextLayoutOptions, f32, &[(&str, f32)]); 5] = [
            (
                "Helvetica",
                long,
                cut,
                40.9375,
                &[
                    ("AVAT", 38.25), ("ART", 31.71875), ("OWE", 38.21875), ("RWA", 36.46875), ("YWA", 35.859375),
                    ("RDT", 32.890625), ("YPO", 33.796875), ("GRA", 34.671875), ("PHY", 32.90625),
                    ("VALV", 38.5625), ("EFAV", 39.734375), ("OURI", 40.), ("TEW", 35.546875), ("ATE", 29.9375),
                    ("RYT", 31.71875), ("AVE", 30.84375), ("RNV", 33.78125), ("OYA", 32.609375), ("GE", 23.125),
                ],
            ),
            (
                "Helvetica",
                long,
                break_all,
                62.953125,
                &[
                    ("AVATAR", 59.296875), ("TOWER", 59.265625), ("WAYWA", 59.875), ("RDTYP", 54.234375),
                    ("OGRAP", 57.796875), ("HYVALV", 60.78125), ("EFAVO", 52.1875), ("URITE", 48.),
                    ("WATER", 56.015625), ("YTAVER", 61.65625), ("NVOYA", 54.84375), ("GE", 23.125),
                ],
            ),
            (
                "Times",
                long,
                cut,
                121.40625,
                &[
                    ("AVATARTOWER", 115.140625), ("WAYWARDTYP", 112.3125), ("OGRAPHYVALV", 118.25),
                    ("EFAVOURITEW", 112.296875), ("ATERYTAVERN", 112.234375), ("VOYAGE", 65.78125),
                ],
            ),
            (
                "Times",
                long,
                break_all,
                59.3125,
                &[
                    ("AVATA", 48.828125), ("RTOWE", 55.640625), ("RWAY", 44.765625), ("WARDT", 56.890625),
                    ("YPOGR", 54.234375), ("APHYV", 55.125), ("ALVEF", 50.09375), ("AVOUR", 54.828125),
                    ("ITEWAT", 57.765625), ("ERYTA", 51.1875), ("VERNV", 55.109375), ("OYAGE", 54.21875),
                ],
            ),
            (
                "Helvetica",
                "ToYoAVTaWaTyVaAyWoAwYaTeVeWe",
                cut,
                31.875,
                &[
                    ("ToY", 27.578125), ("oAV", 29.0625), ("Ta", 16.90625), ("Wa", 23.421875), ("TyV", 27.578125),
                    ("aAy", 27.296875), ("Wo", 23.71875), ("Aw", 21.953125), ("YaT", 28.171875),
                    ("eVe", 27.59375), ("We", 23.71875),
                ],
            ),
        ];
        for (family, text, options, wrap_width, expected) in rows {
            let lines = text_system
                .shape_text_inline(
                    text.to_string().into(),
                    px(16.),
                    &[TextRun {
                        len: text.len(),
                        font: font(family),
                        ..Default::default()
                    }],
                    Some(px(wrap_width)),
                    None,
                    options,
                    &[],
                    &[],
                    px(20.),
                )
                .unwrap();
            let layout: &gpui::WrappedLineLayout = &lines[0];
            let mut starts = vec![0];
            starts.extend(layout.wrap_boundaries().iter().map(|boundary| {
                layout.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
            }));
            starts.push(text.len());
            let rows: Vec<&str> = starts.windows(2).map(|bytes| &text[bytes[0]..bytes[1]]).collect();
            let expected_rows: Vec<&str> = expected.iter().map(|(row, _)| *row).collect();
            assert_eq!(rows, expected_rows, "{family} {wrap_width}");
            // The first row, which starts at no kern, ends where the line was shaped again.
            let (row, chrome) = expected[0];
            let width = layout.line_widths().next().unwrap();
            assert!(
                (f32::from(width) - chrome).abs() <= 1.5 / 64.,
                "{family} {wrap_width}: {row:?} is {width:?}, Chrome {chrome}"
            );
        }
    }

    /// Blink measures the min-content width of a text from the positions of its glyphs and does not
    /// shape the end of a line again, so a letter that is cut from a kerned one is as wide as it is
    /// in the text: Chrome's `width: min-content` is 14.8125 for "WA" cut between its letters, where
    /// the W on its own is 15.109375.
    #[test]
    fn test_min_content_does_not_shape_the_end_of_a_line_again() {
        use gpui::{OverflowWrap, TextLayoutOptions, TextRun, TextSystem, WindowTextSystem};
        use std::sync::Arc;

        let text_system = WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(MacTextSystem::new()))));
        let cut = TextLayoutOptions {
            overflow_wrap: OverflowWrap::Anywhere,
            ..Default::default()
        };
        for (text, chrome) in [("WA", 14.8125), ("AVATAR", 11.546875)] {
            let run = TextRun {
                len: text.len(),
                font: font("Helvetica"),
                ..Default::default()
            };
            let measured = text_system
                .min_content_width(text.to_string().into(), px(16.), &[run], cut, &[])
                .unwrap();
            assert_eq!(measured, px(chrome), "{text}");
        }
    }

    /// The advances Chrome sets a font with a `trak` table with: HarfBuzz tracks PingFang at the CSS size, and Skia
    /// advances Apple Color Emoji at the device size with its tracking off, where CoreText advances both its own way
    /// (13 and 17 at 13px). The numbers are Chrome's own (`fallback-advance-oracle.json`, 1/64 apart).
    #[test]
    fn test_a_font_with_a_trak_table_is_advanced_as_chrome_advances_it() {
        use super::AdvanceSource;

        let fonts = MacTextSystem::new();
        let id = |family: &str| fonts.font_id(&font(family)).unwrap();
        let source = |family: &str| {
            let font_id = id(family);
            fonts.0.read().advance_sources[font_id.0]
        };
        assert_eq!(source("PingFang SC"), AdvanceSource::Tracked);
        // Apple Color Emoji cannot be asked for by name, only fallen back to.
        let emoji = fonts
            .layout_line(
                "😀",
                px(13.),
                &[FontRun { len: 4, font_id: id("Helvetica Neue"), font_size: None, line_height: None }],
                1.,
            )
            .runs[0]
            .font_id;
        assert_eq!(fonts.0.read().advance_sources[emoji.0], AdvanceSource::Untracked);
        for family in ["Helvetica Neue", "Menlo", "Hiragino Sans", "Apple SD Gothic Neo"] {
            assert_eq!(source(family), AdvanceSource::CoreText, "{family}");
        }

        let width = |family: &str, text: &str, size: f32, scale_factor: f32| {
            let run = FontRun { len: text.len(), font_id: id(family), font_size: None, line_height: None };
            f32::from(fonts.layout_line(text, px(size), &[run], scale_factor).width)
        };
        let close = |actual: f32, expected: f32, what: &str| {
            assert!((actual - expected).abs() <= 1. / 64., "{what}: {actual}, Chrome {expected}");
        };
        for scale_factor in [1., 2.] {
            // PingFang's tracking is by the CSS size, whatever the device size is, in the font asked for
            // and in the one CoreText falls back to.
            for family in ["Helvetica Neue", "Menlo", "PingFang SC"] {
                close(width(family, "中文测试", 13., scale_factor), 4. * 13.26, family);
                close(width(family, "中", 16., scale_factor), 16.32, family);
                close(width(family, "中", 24., scale_factor), 24.32, family);
            }
            // Hangul and kana are not tracked.
            close(width("Helvetica Neue", "한국어", 13., scale_factor), 3. * 11.245, "Hangul");
            close(width("Helvetica Neue", "こんにちは", 13., scale_factor), 5. * 13., "kana");
        }
        // An emoji is 17 wide at 13px to CoreText and 16 to Chrome, 13 at a device size of 26 (an em).
        for (family, size, scale_factor, expected) in [
            ("Helvetica Neue", 13., 1., 16.),
            ("Helvetica Neue", 13., 2., 13.),
            ("Menlo", 16., 1., 20.),
            ("Menlo", 16., 2., 16.),
            ("Helvetica Neue", 24., 1., 24.),
            ("Helvetica Neue", 10., 1., 13.),
        ] {
            close(width(family, "😀", size, scale_factor), expected, &format!("{family} {size} x{scale_factor}"));
        }
    }

    /// The glyphs after a glyph of a font that is advanced as Chrome does stand after its advance, the width is the sum,
    /// and `glyph_advance`, which the line's measure is made from, agrees with both.
    #[test]
    fn test_the_glyphs_after_a_fallback_glyph_move_with_its_advance() {
        let fonts = MacTextSystem::new();
        let font_id = fonts.font_id(&font("Helvetica Neue")).unwrap();
        for scale_factor in [1., 2.] {
            let text = "a中b😀c";
            let run = FontRun { len: text.len(), font_id, font_size: None, line_height: None };
            let layout = fonts.layout_line(text, px(13.), &[run], scale_factor);
            let glyphs: Vec<_> = layout
                .runs
                .iter()
                .flat_map(|run| run.glyphs.iter().map(move |glyph| (run.font_id, glyph)))
                .collect();
            let mut x = 0.;
            for (ix, (run_font, glyph)) in glyphs.iter().enumerate() {
                let at = f32::from(glyph.position.x);
                assert!((at - x).abs() < 1e-3, "glyph {ix}: {at} at {x}");
                x += fonts.glyph_advance(*run_font, glyph.id, px(13.), scale_factor).unwrap() as f32;
            }
            let width = f32::from(layout.width);
            assert!((width - x).abs() < 1e-3, "{width} and {x}");
        }
    }
}
