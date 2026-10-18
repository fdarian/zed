use crate::{BackgroundExecutor, Task};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
    task,
    time::Duration,
};

/// A helper trait for building complex objects with imperative conditionals in a fluent style.
pub trait FluentBuilder {
    /// Imperatively modify self with the given closure.
    fn map<U>(self, f: impl FnOnce(Self) -> U) -> U
    where
        Self: Sized,
    {
        f(self)
    }

    /// Conditionally modify self with the given closure.
    fn when(self, condition: bool, then: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        self.map(|this| if condition { then(this) } else { this })
    }

    /// Conditionally modify self with the given closure.
    fn when_else(
        self,
        condition: bool,
        then: impl FnOnce(Self) -> Self,
        else_fn: impl FnOnce(Self) -> Self,
    ) -> Self
    where
        Self: Sized,
    {
        self.map(|this| if condition { then(this) } else { else_fn(this) })
    }

    /// Conditionally unwrap and modify self with the given closure, if the given option is Some.
    fn when_some<T>(self, option: Option<T>, then: impl FnOnce(Self, T) -> Self) -> Self
    where
        Self: Sized,
    {
        self.map(|this| {
            if let Some(value) = option {
                then(this, value)
            } else {
                this
            }
        })
    }
    /// Conditionally unwrap and modify self with the given closure, if the given option is None.
    fn when_none<T>(self, option: &Option<T>, then: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        self.map(|this| if option.is_some() { this } else { then(this) })
    }
}

/// Extensions for Future types that provide additional combinators and utilities.
pub trait FutureExt {
    /// Requires a Future to complete before the specified duration has elapsed.
    /// Similar to tokio::timeout.
    fn with_timeout(self, timeout: Duration, executor: &BackgroundExecutor) -> WithTimeout<Self>
    where
        Self: Sized;
}

impl<T: Future> FutureExt for T {
    fn with_timeout(self, timeout: Duration, executor: &BackgroundExecutor) -> WithTimeout<Self>
    where
        Self: Sized,
    {
        WithTimeout {
            future: self,
            timer: executor.timer(timeout),
        }
    }
}

#[pin_project::pin_project]
pub struct WithTimeout<T> {
    #[pin]
    future: T,
    #[pin]
    timer: Task<()>,
}

#[derive(Debug, thiserror::Error)]
#[error("Timed out before future resolved")]
/// Error returned by with_timeout when the timeout duration elapsed before the future resolved
pub struct Timeout;

impl<T: Future> Future for WithTimeout<T> {
    type Output = Result<T::Output, Timeout>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> task::Poll<Self::Output> {
        let this = self.project();

        if let task::Poll::Ready(output) = this.future.poll(cx) {
            task::Poll::Ready(Ok(output))
        } else if this.timer.poll(cx).is_ready() {
            task::Poll::Ready(Err(Timeout))
        } else {
            task::Poll::Pending
        }
    }
}

/// Increment the given atomic counter if it is not zero.
/// Return the new value of the counter.
pub(crate) fn atomic_incr_if_not_zero(counter: &AtomicUsize) -> usize {
    let mut loaded = counter.load(SeqCst);
    loop {
        if loaded == 0 {
            return 0;
        }
        match counter.compare_exchange_weak(loaded, loaded + 1, SeqCst, SeqCst) {
            Ok(x) => return x + 1,
            Err(actual) => loaded = actual,
        }
    }
}

/// Rounds to the nearest integer with 0.5 ties toward zero.
#[inline]
pub(crate) fn round_half_toward_zero(value: f32) -> f32 {
    (value.abs() - 0.5).ceil().copysign(value)
}

#[inline]
pub(crate) fn round_half_toward_zero_f64(value: f64) -> f64 {
    (value.abs() - 0.5).ceil().copysign(value)
}

#[inline]
pub(crate) fn round_to_device_pixel(logical: f32, scale_factor: f32) -> f32 {
    round_half_toward_zero(logical * scale_factor)
}

/// Rounds to the nearest integer with 0.5 ties toward negative infinity.
///
/// Unlike [`round_half_toward_zero`] this commutes with whole-number shifts,
/// `round_half_down(v + n) == round_half_down(v) + n`, also across zero: a subtree painted `n`
/// device pixels further along snaps to the same pixels, `n` further along. It agrees with
/// [`round_half_toward_zero`] on every non-negative value.
#[inline]
pub(crate) fn round_half_down(value: f32) -> f32 {
    // `+ 0.0` turns the -0.0 that `ceil` gives for (-0.5, 0.5] into 0.0.
    (value - 0.5).ceil() + 0.0
}

/// [`round_half_down`] of a logical length in device pixels. Used where what is painted must
/// keep its shape when it is moved by whole device pixels; [`round_to_device_pixel`] is the
/// layout's rounding.
#[inline]
pub(crate) fn round_to_device_pixel_down(logical: f32, scale_factor: f32) -> f32 {
    round_half_down(logical * scale_factor)
}

/// Splits a glyph origin in device pixels into the whole pixel it is painted at and the
/// subpixel variant (in `0..variants`) it is rasterized with, so that
/// `whole + variant / variants` is the origin rounded to the nearest `1 / variants`.
/// Shifting the origin by a whole number shifts `whole` by it and leaves the variant alone.
#[inline]
pub(crate) fn split_glyph_origin(origin: f32, variants: u8) -> (f32, u8) {
    let quantized = round_half_down(origin * variants as f32) / variants as f32;
    let whole = quantized.floor();
    (whole, ((quantized - whole) * variants as f32) as u8)
}

#[inline]
pub(crate) fn round_stroke_to_device_pixel(logical: f32, scale_factor: f32) -> f32 {
    if logical == 0.0 {
        0.0
    } else {
        round_to_device_pixel(logical.max(0.0), scale_factor).max(1.0)
    }
}

#[inline]
pub(crate) fn floor_to_device_pixel(logical: f32, scale_factor: f32) -> f32 {
    (logical * scale_factor).floor()
}

/// Chrome's `LayoutUnit`: boxes are laid out in 1/64 of a logical pixel and painted on device
/// pixels. Text takes the unit above its advance and the one nearest its height.
pub(crate) const LAYOUT_UNIT: f32 = 1. / 64.;

/// How far text may stand over the width it is laid out in and still fit, in logical pixels. A
/// width measured up to a layout unit crosses layout in device pixels and comes back as a float.
const FIT_SLACK: f32 = 1. / 1024.;

/// The first multiple of [`LAYOUT_UNIT`] at or above `logical`, for a size that must hold its
/// content: a width the text is laid out in must not wrap the text. A sum of advances a hair above
/// a multiple of the unit is that multiple.
#[inline]
pub(crate) fn ceil_to_layout_unit(logical: f32) -> f32 {
    (logical / LAYOUT_UNIT - FIT_SLACK / LAYOUT_UNIT).ceil() * LAYOUT_UNIT
}

/// The multiple of [`LAYOUT_UNIT`] nearest to `logical`, for a size that is a measure of the font.
#[inline]
pub(crate) fn round_to_layout_unit(logical: f32) -> f32 {
    (logical / LAYOUT_UNIT).round() * LAYOUT_UNIT
}

/// Whether text `width` wide stands over `available`: the slack is for the float that a width
/// measured by [`ceil_to_layout_unit`] comes back as.
#[inline]
pub(crate) fn overflows(width: crate::Pixels, available: crate::Pixels) -> bool {
    width.0 > available.0 + FIT_SLACK
}

#[inline]
pub(crate) fn ceil_to_device_pixel(logical: f32, scale_factor: f32) -> f32 {
    (logical * scale_factor).ceil()
}

#[cfg(test)]
mod tests {
    use crate::TestAppContext;

    use super::*;

    #[test]
    fn test_round_half_toward_zero() {
        // Midpoint ties go toward zero
        assert_eq!(round_half_toward_zero(0.5), 0.0);
        assert_eq!(round_half_toward_zero(1.5), 1.0);
        assert_eq!(round_half_toward_zero(2.5), 2.0);
        assert_eq!(round_half_toward_zero(-0.5), 0.0);
        assert_eq!(round_half_toward_zero(-1.5), -1.0);
        assert_eq!(round_half_toward_zero(-2.5), -2.0);

        // Non-midpoint values round to nearest
        assert_eq!(round_half_toward_zero(1.5001), 2.0);
        assert_eq!(round_half_toward_zero(1.4999), 1.0);
        assert_eq!(round_half_toward_zero(-1.5001), -2.0);
        assert_eq!(round_half_toward_zero(-1.4999), -1.0);

        // Integers are unchanged
        assert_eq!(round_half_toward_zero(0.0), 0.0);
        assert_eq!(round_half_toward_zero(3.0), 3.0);
        assert_eq!(round_half_toward_zero(-3.0), -3.0);
    }

    #[test]
    fn test_device_pixel_helpers() {
        // Snap uses half-toward-zero: 1.0 * 1.5 = 1.5 ties toward 1.0.
        assert_eq!(round_to_device_pixel(1.0, 1.5), 1.0);
        // Below the tie rounds down, above rounds up.
        assert_eq!(round_to_device_pixel(0.3, 2.0), 1.0);
        assert_eq!(round_to_device_pixel(1.4, 1.0), 1.0);
        assert_eq!(round_to_device_pixel(1.6, 1.0), 2.0);

        // Stroke uses snap, but clamps non-zero input up to at least 1dp.
        assert_eq!(round_stroke_to_device_pixel(0.0, 1.0), 0.0);
        assert_eq!(round_stroke_to_device_pixel(0.4, 1.0), 1.0);
        assert_eq!(round_stroke_to_device_pixel(0.5, 1.0), 1.0);
        assert_eq!(round_stroke_to_device_pixel(1.0, 1.5), 1.0);
        assert_eq!(round_stroke_to_device_pixel(1.6, 1.0), 2.0);

        // Cover's near edge floors, far edge ceils. Together they form a strict superset.
        assert_eq!(floor_to_device_pixel(0.3, 2.0), 0.0);
        assert_eq!(ceil_to_device_pixel(0.3, 2.0), 1.0);
        assert_eq!(floor_to_device_pixel(2.1, 1.0), 2.0);
        assert_eq!(ceil_to_device_pixel(2.1, 1.0), 3.0);

        // Integer device-pixel inputs are stable under all three.
        assert_eq!(round_to_device_pixel(2.0, 2.0), 4.0);
        assert_eq!(floor_to_device_pixel(2.0, 2.0), 4.0);
        assert_eq!(ceil_to_device_pixel(2.0, 2.0), 4.0);
    }

    #[test]
    fn test_round_half_toward_zero_f64() {
        assert_eq!(round_half_toward_zero_f64(0.5), 0.0);
        assert_eq!(round_half_toward_zero_f64(-0.5), 0.0);
        assert_eq!(round_half_toward_zero_f64(1.5), 1.0);
        assert_eq!(round_half_toward_zero_f64(-1.5), -1.0);
        assert_eq!(round_half_toward_zero_f64(2.5001), 3.0);
    }

    /// The values the rounding is checked at: every multiple of 1/64 up to 2048, the
    /// neighbourhood of every half up to 70000, and a spread of arbitrary bit patterns.
    fn non_negative_values() -> Vec<f32> {
        let mut values: Vec<f32> = (0..2048 * 64).map(|step| step as f32 / 64.).collect();
        for whole in 0..70_000u32 {
            let tie = whole as f32 + 0.5;
            let mut bits = tie.to_bits() - 3;
            for _ in 0..7 {
                values.push(f32::from_bits(bits));
                bits += 1;
            }
        }
        let mut state = 0x9e37_79b9_u32;
        for _ in 0..200_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // Up to 2^24, where f32 stops holding fractions.
            values.push(f32::from_bits(state & 0x7fff_ffff) % 16_777_216.);
        }
        values.retain(|value| value.is_finite() && *value >= 0.);
        values
    }

    #[test]
    fn rounding_half_down_is_the_rounding_toward_zero_for_non_negative_values() {
        for value in non_negative_values() {
            assert_eq!(
                round_half_down(value).to_bits(),
                round_half_toward_zero(value).to_bits(),
                "{value}"
            );
            for scale_factor in [1., 2., 4., 1.5, 3.] {
                assert_eq!(
                    round_to_device_pixel_down(value, scale_factor).to_bits(),
                    round_to_device_pixel(value, scale_factor).to_bits(),
                    "{value} at {scale_factor}"
                );
            }
        }
    }

    #[test]
    fn rounding_half_down_ties_go_down() {
        for (value, expected) in [
            (-2.5, -3.),
            (-1.5, -2.),
            (-0.5, -1.),
            (0.5, 0.),
            (1.5, 1.),
            (-0.4999, 0.),
            (-0.5001, -1.),
            (0.5001, 1.),
        ] {
            assert_eq!(round_half_down(value), expected, "{value}");
        }
        assert!(round_half_down(-0.25).is_sign_positive());
    }

    #[test]
    fn rounding_half_down_commutes_with_whole_shifts() {
        for step in -64 * 16..64 * 16 {
            let value = step as f32 / 64.;
            for shift in -300..300 {
                let shift = shift as f32;
                assert_eq!(
                    round_half_down(value + shift),
                    round_half_down(value) + shift,
                    "{value} + {shift}"
                );
            }
        }
    }

    fn split_glyph_origin_toward_zero(origin: f32, variants: u8) -> (f32, u8) {
        let quantized = round_half_toward_zero(origin * variants as f32) / variants as f32;
        (quantized.trunc(), (quantized.fract() * variants as f32) as u8)
    }

    #[test]
    fn splitting_a_glyph_origin_is_what_it_was_for_non_negative_values() {
        for value in non_negative_values() {
            for variants in [1, 4] {
                let (whole, variant) = split_glyph_origin(value, variants);
                let (expected_whole, expected_variant) =
                    split_glyph_origin_toward_zero(value, variants);
                assert_eq!(whole.to_bits(), expected_whole.to_bits(), "{value}");
                assert_eq!(variant, expected_variant, "{value}");
            }
        }
    }

    #[test]
    fn splitting_a_glyph_origin_commutes_with_whole_shifts() {
        for variants in [1, 4] {
            for step in -64 * 16..64 * 16 {
                let value = step as f32 / 64.;
                let (whole, variant) = split_glyph_origin(value, variants);
                assert!(variant < variants);
                for shift in -300..300 {
                    let shifted = split_glyph_origin(value + shift as f32, variants);
                    assert_eq!(shifted, (whole + shift as f32, variant), "{value} + {shift}");
                }
            }
        }
    }

    #[gpui::test]
    async fn test_with_timeout(cx: &mut TestAppContext) {
        Task::ready(())
            .with_timeout(Duration::from_secs(1), &cx.executor())
            .await
            .expect("Timeout should be noop");

        let long_duration = Duration::from_secs(6000);
        let short_duration = Duration::from_secs(1);
        cx.executor()
            .timer(long_duration)
            .with_timeout(short_duration, &cx.executor())
            .await
            .expect_err("timeout should have triggered");

        let fut = cx
            .executor()
            .timer(long_duration)
            .with_timeout(short_duration, &cx.executor());
        cx.executor().advance_clock(short_duration * 2);
        futures::FutureExt::now_or_never(fut)
            .unwrap_or_else(|| panic!("timeout should have triggered"))
            .expect_err("timeout");
    }
}
