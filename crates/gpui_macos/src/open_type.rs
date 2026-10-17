#![allow(unused, non_upper_case_globals)]

use cocoa::appkit::CGFloat;
use core_foundation::{
    array::{
        CFArray, CFArrayAppendArray, CFArrayAppendValue, CFArrayCreateMutable, CFArrayGetCount,
        CFArrayGetValueAtIndex, CFArrayRef, CFMutableArrayRef, kCFTypeArrayCallBacks,
    },
    base::{CFRelease, CFType, TCFType, kCFAllocatorDefault},
    dictionary::{
        CFDictionaryCreate, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    },
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_foundation_sys::locale::CFLocaleCopyPreferredLanguages;
use core_graphics::{display::CFDictionary, geometry::CGAffineTransform};
use core_text::font_descriptor::{
    TraitAccessors, kCTFontFamilyNameAttribute, kCTFontItalicTrait, kCTFontSlantTrait,
    kCTFontTraitsAttribute, kCTFontWeightTrait, kCTFontWidthTrait,
};
use core_text::{
    font::{CTFont, CTFontRef, cascade_list_for_languages},
    font_descriptor::{
        CTFontDescriptor, CTFontDescriptorCopyAttributes, CTFontDescriptorCreateCopyWithFeature,
        CTFontDescriptorCreateWithAttributes, CTFontDescriptorCreateWithNameAndSize,
        CTFontDescriptorRef, kCTFontCascadeListAttribute, kCTFontFeatureSettingsAttribute,
        kCTFontVariationAttribute,
    },
};
use font_kit::font::Font as FontKitFont;
use gpui::{FontFallbacks, FontFeatures};
use std::ptr;

pub fn apply_features_and_fallbacks(
    font: &mut FontKitFont,
    features: &FontFeatures,
    fallbacks: Option<&FontFallbacks>,
) -> anyhow::Result<()> {
    unsafe {
        let mut keys = vec![kCTFontFeatureSettingsAttribute];
        let mut values = vec![generate_feature_array(features)];
        if let Some(fallbacks) = fallbacks
            && !fallbacks.fallback_list().is_empty()
        {
            keys.push(kCTFontCascadeListAttribute);
            values.push(generate_fallback_array(fallbacks, font));
        }
        let attrs = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr() as _,
            values.as_ptr() as _,
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );

        for value in &values {
            CFRelease(*value as _);
        }

        let new_descriptor = CTFontDescriptorCreateWithAttributes(attrs);
        CFRelease(attrs as _);
        let new_descriptor = CTFontDescriptor::wrap_under_create_rule(new_descriptor);
        let new_font = CTFontCreateCopyWithAttributes(
            font.native_font().as_concrete_TypeRef(),
            0.0,
            std::ptr::null(),
            new_descriptor.as_concrete_TypeRef(),
        );
        let new_font = CTFont::wrap_under_create_rule(new_font);
        *font = font_kit::font::Font::from_native_font(&new_font);

        Ok(())
    }
}

fn generate_feature_array(features: &FontFeatures) -> CFMutableArrayRef {
    unsafe {
        let feature_array = CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);
        for (tag, value) in features.tag_value_list() {
            let keys = [kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue];
            let values = [
                CFString::new(tag).as_CFTypeRef(),
                CFNumber::from(*value as i32).as_CFTypeRef(),
            ];
            let dict = CFDictionaryCreate(
                kCFAllocatorDefault,
                &keys as *const _ as _,
                &values as *const _ as _,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            values.into_iter().for_each(|value| CFRelease(value));
            CFArrayAppendValue(feature_array, dict as _);
            CFRelease(dict as _);
        }
        feature_array
    }
}

fn generate_fallback_array(fallbacks: &FontFallbacks, font: &mut FontKitFont) -> CFMutableArrayRef {
    unsafe {
        let symbolic_traits = font.native_font().symbolic_traits();
        let all_traits = font.native_font().all_traits();

        let fallback_array = CFArrayCreateMutable(kCFAllocatorDefault, 0, &kCFTypeArrayCallBacks);
        for user_fallback in fallbacks.fallback_list() {
            let name = CFString::from(user_fallback.as_str());

            let traits_keys = [kCTFontWeightTrait, kCTFontSlantTrait];
            let weight_value = CFNumber::from(all_traits.normalized_weight());
            let slant_value = CFNumber::from(if (symbolic_traits & kCTFontItalicTrait) != 0 {
                1.0
            } else {
                0.0
            });
            let traits_values = [weight_value.as_CFTypeRef(), slant_value.as_CFTypeRef()];
            let traits = CFDictionaryCreate(
                kCFAllocatorDefault,
                &traits_keys as *const _ as _,
                &traits_values as *const _ as _,
                traits_keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            drop(weight_value);
            drop(slant_value);

            let attr_keys = [kCTFontFamilyNameAttribute, kCTFontTraitsAttribute];
            let attr_values = [name.as_CFTypeRef(), traits as _];
            let attrs = CFDictionaryCreate(
                kCFAllocatorDefault,
                &attr_keys as *const _ as _,
                &attr_values as *const _ as _,
                attr_keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            CFRelease(traits as _);

            let fallback_desc = CTFontDescriptorCreateWithAttributes(attrs);
            CFRelease(attrs as _);

            CFArrayAppendValue(fallback_array, fallback_desc as _);
            CFRelease(fallback_desc as _);
        }

        let font_ref = font.native_font().as_concrete_TypeRef();
        append_system_fallbacks(fallback_array, font_ref);
        fallback_array
    }
}

fn append_system_fallbacks(fallback_array: CFMutableArrayRef, font_ref: CTFontRef) {
    unsafe {
        let preferred_languages: CFArray<CFString> =
            CFArray::wrap_under_create_rule(CFLocaleCopyPreferredLanguages());

        let default_fallbacks = CTFontCopyDefaultCascadeListForLanguages(
            font_ref,
            preferred_languages.as_concrete_TypeRef(),
        );
        let default_fallbacks: CFArray<CTFontDescriptor> =
            CFArray::wrap_under_create_rule(default_fallbacks);

        for desc in default_fallbacks
            .iter()
            .filter(|desc| desc.font_path().is_some())
        {
            CFArrayAppendValue(fallback_array, desc.as_concrete_TypeRef() as _);
        }
    }
}

/// An axis of a variable font.
#[derive(Clone, Copy, Debug)]
pub struct VariationAxis {
    pub tag: u32,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

/// The axes of `font`, none for a font that is not variable.
pub fn variation_axes(font: &FontKitFont) -> Vec<VariationAxis> {
    let Some(axes) = font.native_font().get_variation_axes() else {
        return Vec::new();
    };
    let number = |axis: &CFDictionary<CFString, CFType>, key: CFStringRef| {
        axis.find(unsafe { CFString::wrap_under_get_rule(key) })
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_f64())
    };
    axes.iter()
        .filter_map(|axis| unsafe {
            Some(VariationAxis {
                tag: number(&axis, kCTFontVariationAxisIdentifierKey)? as u32,
                min: number(&axis, kCTFontVariationAxisMinimumValueKey)? as f32,
                max: number(&axis, kCTFontVariationAxisMaximumValueKey)? as f32,
                default: number(&axis, kCTFontVariationAxisDefaultValueKey)? as f32,
            })
        })
        .collect()
}

/// What `font` has set on each of its `axes`.
pub fn current_variations(font: &FontKitFont, axes: &[VariationAxis]) -> Vec<(u32, f32)> {
    let dictionary = unsafe {
        let dictionary = CTFontCopyVariation(font.native_font().as_concrete_TypeRef());
        // A font with no variation set has none, and then every axis is at its default.
        (!dictionary.is_null())
            .then(|| CFDictionary::<CFType, CFType>::wrap_under_create_rule(dictionary))
    };
    axes.iter()
        .map(|axis| {
            let set = dictionary.as_ref().and_then(|dictionary| {
                dictionary
                    .find(CFNumber::from(axis.tag as i64).as_CFType())
                    .and_then(|value| value.downcast::<CFNumber>())
                    .and_then(|value| value.to_f64())
            });
            (axis.tag, set.map_or(axis.default, |value| value as f32))
        })
        .collect()
}

/// Replaces `font` with a copy set to `variations`, a value per axis in `axes`.
/// The axes not named stay as `font` has them.
///
/// CoreText drops an axis set to its default from the copy's variation, and then
/// the copy has that axis as the face it was made from does: asking a
/// semibold instance for `wght` 400 gives a semibold. A value at the default is
/// moved one step off it, which is below what any rasteriser can show.
pub fn apply_variations(font: &mut FontKitFont, variations: &[(u32, f32)], axes: &[VariationAxis]) {
    let values = CFDictionary::from_CFType_pairs(
        &variations
            .iter()
            .map(|(tag, value)| {
                let default = axes
                    .iter()
                    .find(|axis| axis.tag == *tag)
                    .map(|axis| (axis.default, axis.max));
                let value = match default {
                    Some((default, max)) if *value == default => {
                        if default < max {
                            value.next_up()
                        } else {
                            value.next_down()
                        }
                    }
                    _ => *value,
                };
                (CFNumber::from(*tag as i64), CFNumber::from(value as f64))
            })
            .collect::<Vec<_>>(),
    );
    let attributes = CFDictionary::from_CFType_pairs(&[(
        unsafe { CFString::wrap_under_get_rule(kCTFontVariationAttribute) },
        values.as_CFType(),
    )]);
    let descriptor = unsafe {
        CTFontDescriptor::wrap_under_create_rule(CTFontDescriptorCreateWithAttributes(
            attributes.as_concrete_TypeRef(),
        ))
    };
    let new_font = unsafe {
        CTFont::wrap_under_create_rule(CTFontCreateCopyWithAttributes(
            font.native_font().as_concrete_TypeRef(),
            0.0,
            std::ptr::null(),
            descriptor.as_concrete_TypeRef(),
        ))
    };
    *font = unsafe { FontKitFont::from_native_font(&new_font) };
}

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    static kCTFontOpenTypeFeatureTag: CFStringRef;
    static kCTFontOpenTypeFeatureValue: CFStringRef;
    static kCTFontVariationAxisIdentifierKey: CFStringRef;
    static kCTFontVariationAxisMinimumValueKey: CFStringRef;
    static kCTFontVariationAxisMaximumValueKey: CFStringRef;
    static kCTFontVariationAxisDefaultValueKey: CFStringRef;

    fn CTFontCopyVariation(font: CTFontRef) -> core_foundation::dictionary::CFDictionaryRef;

    fn CTFontCreateCopyWithAttributes(
        font: CTFontRef,
        size: CGFloat,
        matrix: *const CGAffineTransform,
        attributes: CTFontDescriptorRef,
    ) -> CTFontRef;
    fn CTFontCopyDefaultCascadeListForLanguages(
        font: CTFontRef,
        languagePrefList: CFArrayRef,
    ) -> CFArrayRef;
}
