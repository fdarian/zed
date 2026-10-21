use anyhow::anyhow;
use cocoa::appkit::CGFloat;
use collections::{HashMap, HashSet};
use core_foundation::{
    array::{CFArray, CFArrayRef},
    attributed_string::CFMutableAttributedString,
    base::{CFIndex, CFRange, CFType, TCFType},
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
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::Vector2F,
};
use smallvec::SmallVec;
use std::{borrow::Cow, char, convert::TryFrom, sync::Arc, sync::OnceLock};

use crate::open_type::{
    apply_features_and_fallbacks, apply_variations, current_variations, variation_axes,
};

#[allow(non_upper_case_globals)]
const kCGImageAlphaOnly: u32 = 7;

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
}

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

    fn layout_line(&self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, font_runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }

    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        // When font smoothing is enabled, CoreGraphics thickens glyph strokes by an amount that
        // depends on the foreground color's luminance. We replicate the logic used by CoreGraphics
        // to select between the different levels of dilation.
        if !font_smoothing_allowed_by_user() {
            return 0;
        }
        let rgba: Rgba = color.into();
        let luminance = 0.2126 * rgba.r + 0.7152 * rgba.g + 0.0722 * rgba.b;
        let level = ((4.0 * luminance) + 0.5).floor() as i32;
        level.clamp(0, 4) as u8
    }
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
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
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
                (has != value).then_some((*tag, value))
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
        self.varied_fonts.insert(key, varied);
        varied
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

            if params.font_smoothing == FontSmoothing::Antialiased {
                // CoreGraphics smooths a glyph by default, and `-webkit-font-smoothing:
                // antialiased` is the plain coverage without it, for dark and light text alike.
                cx.set_should_smooth_fonts(false);
                cx.set_gray_fill_color(0.0, 1.0);
            } else if params.dilation > 0 {
                let luminance = params.dilation as f64 * 0.25;
                cx.set_should_smooth_fonts(true);
                cx.set_gray_fill_color(luminance, 1.0);
            } else {
                cx.set_gray_fill_color(0.0, 1.0);
            }
            self.fonts[params.font_id.0]
                .native_font()
                .clone_with_font_size(f32::from(params.font_size) as CGFloat)
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
            }

            Ok((bitmap_size, bytes))
        }
    }

    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
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
                let font = &self.fonts[run.font_id.0];

                let run_font_size = run.font_size.unwrap_or(font_size);
                let line_box = self.font_metrics[run.font_id.0].line_box(run_font_size);
                let ct_font_size = if break_ligature {
                    px(f32::from(run_font_size).next_up())
                } else {
                    run_font_size
                };
                let ct_font = font.native_font().clone_with_font_size(ct_font_size.into());
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
            .map(|run| self.font_metrics[run.font_id.0].line_box(font_size));
        LineLayout {
            runs,
            font_size,
            width: typographic_bounds.width.into(),
            ascent: strut.map_or(Pixels::ZERO, |strut| strut.ascent),
            descent: strut.map_or(Pixels::ZERO, |strut| strut.descent),
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

fn font_kit_metrics_to_metrics(font: &FontKitFont) -> FontMetrics {
    let metrics = font.metrics();
    FontMetrics {
        units_per_em: metrics.units_per_em,
        ascent: metrics.ascent,
        descent: metrics.descent,
        line_gap: metrics.line_gap,
        underline_position: metrics.underline_position,
        underline_thickness: metrics.underline_thickness,
        cap_height: metrics.cap_height,
        x_height: metrics.x_height,
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
        Font, FontFeatures, FontRun, FontVariations, FontWeight, GlyphId, PlatformTextSystem,
        RenderGlyphParams, font, px,
    };
    use std::sync::Arc;

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

        let layout = fonts.layout_line(line, px(16.), &[style]);
        assert_eq!(layout.len, line.len());
        assert!(layout.runs.is_empty());

        let line = "a\u{feff}b";
        style.len = line.len();
        let layout = fonts.layout_line(line, px(16.), &[style]);
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
        let layout = fonts.layout_line(line, px(16.), font_runs);
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

        let layout = fonts.layout_line(text, px(16.), font_runs);
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

        let layout2 = fonts.layout_line(text, px(16.), font_runs_different);
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
        let layout = fonts.layout_line(text, px(16.), font_runs);
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
        let layout = fonts.layout_line(text, px(16.), font_runs);
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
        let layout = fonts.layout_line(text, px(16.), font_runs);
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

        let layout = fonts.layout_line("aaaa", px(16.), &runs);

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
        let implicit = fonts.layout_line("aaaa", px(16.), &[menlo_run(&fonts, 4, None, None)]);
        let explicit = fonts.layout_line(
            "aaaa",
            px(16.),
            &[
                menlo_run(&fonts, 2, Some(px(16.)), None),
                menlo_run(&fonts, 2, Some(px(16.)), None),
            ],
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
        f32::from(fonts.layout_line(text, px(40.), &[run]).width)
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
                    dilation: 0,
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
    /// `<family>.<weight>.<size>.normal` and its `.text`). Helvetica, Times and Courier have their
    /// ascent widened by a sixth of the content area, and the other fonts do not.
    #[test]
    fn test_the_line_box_of_a_font_is_the_one_chrome_makes() {
        let fonts = MacTextSystem::new();
        let cases: &[(&str, f32, f32, f32, f32)] = &[
            ("Helvetica Neue", 400., 20., 23., 24.),
            ("Helvetica Neue", 700., 20., 24., 25.),
            ("Helvetica Neue", 400., 24., 28., 29.),
            ("Helvetica Neue", 700., 12., 15., 15.),
            ("Helvetica", 400., 12., 14., 14.),
            ("Helvetica", 700., 20., 23., 23.),
            ("Helvetica", 400., 24., 28., 28.),
            ("Menlo", 400., 16., 19., 19.),
            ("Menlo", 700., 20., 24., 24.),
            ("Menlo", 400., 24., 28., 28.),
            ("Times", 400., 14., 17., 17.),
            ("Times", 700., 16., 18., 18.),
            ("Courier", 400., 20., 23., 23.),
            ("Courier", 700., 12., 14., 14.),
        ];
        for (family, weight, size, content, normal) in cases {
            let font = Font {
                weight: FontWeight(*weight),
                ..font(*family)
            };
            let font_id = fonts.font_id(&font).unwrap();
            let line_box = fonts.font_metrics(font_id).line_box(px(*size));
            let label = format!("{family} {weight} {size}px");
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
        );
        // The strut is the font of the first run, and a run in another font has its own box.
        assert_eq!((layout.ascent, layout.descent), (px(18.), px(5.)));
        assert_eq!(layout.runs.len(), 2);
        assert_eq!((layout.runs[0].ascent, layout.runs[0].descent), (px(18.), px(5.)));
        // Bold Helvetica Neue at 20px is 24px of content in Chrome.
        assert_eq!((layout.runs[1].ascent, layout.runs[1].descent), (px(20.), px(4.)));
    }
}
