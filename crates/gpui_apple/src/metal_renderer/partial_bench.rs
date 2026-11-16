//! A controlled GPU-time microbenchmark of the variants of a partial redraw on a scene shaped like the mailbox. Run with
//! `cargo test -p gpui_apple --release --lib -- --ignored --nocapture partial_redraw_gpu_cost`.
//!
//! Every variant draws the same scene into the same persistent target; only the damage differs. The variants run
//! round-robin (rotating their place in the round) so they share one clock state, one command buffer at a time; each is
//! read as `GPUEndTime - GPUStartTime`.

use super::{InstanceBufferPool, MetalRenderer};
use gpui::{
    AtlasTile, Bounds, ContentMask, Corners, DevicePixels, FrameDamage, MonochromeSprite, Quad, ScaledPixels, Scene,
    Shift, point, size,
};
use objc::{msg_send, sel, sel_impl};
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc};

const WIDTH: i32 = 2560;
const HEIGHT: i32 = 1600;
const WARMUP_ROUNDS: usize = 60;
const ROUNDS: usize = 600;

/// Command buffers in flight besides the one being waited for (`BENCH_IN_FLIGHT`, 0 unless set). Keep it 0: consecutive
/// buffers overlap on the GPU when more than one is queued, and each is charged for the other's tail (measured: a copy-only
/// frame reads 0.08 ms alone and 0.2 ms with 3 in flight). `BENCH_FILLER=1` queues a whole frame ahead of each measured
/// one to keep the clocks up, which has the same blur: it adds ~0.45 ms to every variant.
fn in_flight_limit() -> usize {
    std::env::var("BENCH_IN_FLIGHT").map_or(0, |value| value.parse().expect("BENCH_IN_FLIGHT is a number"))
}

fn scaled(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(width), ScaledPixels(height)),
    )
}

fn device(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
    Bounds::new(point(DevicePixels(x), DevicePixels(y)), size(DevicePixels(width), DevicePixels(height)))
}

fn mask(bounds: Bounds<ScaledPixels>) -> ContentMask<ScaledPixels> {
    ContentMask { bounds, corner_radii: Corners::default(), corner_radii_y: Corners::default() }
}

fn filled(bounds: Bounds<ScaledPixels>, color: gpui::Hsla, radius: f32, clip: Bounds<ScaledPixels>) -> Quad {
    Quad {
        bounds,
        content_mask: mask(clip),
        background: color.into(),
        corner_radii: Corners::all(ScaledPixels(radius)),
        ..Quad::default()
    }
}

fn glyph_tiles(renderer: &MetalRenderer) -> Vec<AtlasTile> {
    let atlas: Arc<dyn gpui::PlatformAtlas> = renderer.sprite_atlas().clone();
    (0..48)
        .map(|index| {
            let bytes: Vec<u8> =
                (0..16 * 16u32).map(|texel| ((texel * 7 + index * 13) % 256) as u8).collect();
            atlas
                .get_or_insert_with(
                    &gpui::AtlasKey::Svg(gpui::RenderSvgParams {
                        path: format!("bench-glyph-{index}").into(),
                        size: size(DevicePixels(16), DevicePixels(16)),
                    }),
                    &mut || Ok(Some((size(DevicePixels(16), DevicePixels(16)), bytes.clone().into()))),
                )
                .expect("the atlas takes the tile")
                .expect("the tile exists")
        })
        .collect()
}

/// A line of `count` glyphs 12 px apart, 14 x 18 each.
fn text_line(scene: &mut Scene, tiles: &[AtlasTile], x: f32, y: f32, count: usize, clip: Bounds<ScaledPixels>, seed: usize) {
    for glyph in 0..count {
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: scaled(x + 12. * glyph as f32, y, 14., 18.),
            content_mask: mask(clip),
            color: gpui::hsla(0., 0., 0.9, 1.),
            tile: tiles[(seed + glyph * 5) % tiles.len()].clone(),
            transformation: gpui::TransformationMatrix::unit(),
            local_clip: ContentMask::unbounded(),
            clip_transformation: gpui::TransformationMatrix::unit(),
        });
    }
}

const LIST: (f32, f32, f32, f32) = (438., 166., 842., 1434.);

fn mailbox_scene(renderer: &MetalRenderer) -> Scene {
    let tiles = glyph_tiles(renderer);
    let window = scaled(0., 0., WIDTH as f32, HEIGHT as f32);
    let list = scaled(LIST.0, LIST.1, LIST.2, LIST.3);
    let mut scene = Scene::default();
    scene.insert_primitive(filled(window, gpui::hsla(0.6, 0.2, 0.12, 1.), 0., window));
    // The sidebar: 30 items of an icon, a label of ~24 glyphs and a badge.
    scene.insert_primitive(filled(scaled(0., 0., 438., 1600.), gpui::hsla(0.6, 0.2, 0.15, 1.), 0., window));
    for item in 0..30 {
        let top = 80. + item as f32 * 44.;
        scene.insert_primitive(filled(scaled(20., top, 398., 36.), gpui::hsla(0.6, 0.1, 0.2, 1.), 8., window));
        scene.insert_primitive(filled(scaled(32., top + 8., 20., 20.), gpui::hsla(0.1, 0.8, 0.6, 1.), 4., window));
        text_line(&mut scene, &tiles, 64., top + 9., 24, window, item);
        scene.insert_primitive(filled(scaled(380., top + 10., 24., 16.), gpui::hsla(0.0, 0.8, 0.5, 1.), 8., window));
    }
    // The detail pane: a header, then 18 paragraphs of 4 lines of 55 glyphs each.
    scene.insert_primitive(filled(scaled(1280., 0., 1280., 1600.), gpui::hsla(0.6, 0.2, 0.14, 1.), 0., window));
    for paragraph in 0..18 {
        for line in 0..4 {
            text_line(
                &mut scene,
                &tiles,
                1320.,
                100. + paragraph as f32 * 80. + line as f32 * 20.,
                55,
                window,
                paragraph * 4 + line,
            );
        }
    }
    // The list: a backdrop and 20 rows of 72 px, each with a card, an avatar, an unread dot and 3 lines of text.
    scene.insert_primitive(filled(list, gpui::hsla(0.6, 0.2, 0.18, 1.), 0., list));
    for row in 0..20 {
        let top = LIST.1 - 36. + row as f32 * 72.;
        scene.insert_primitive(filled(scaled(LIST.0 + 8., top + 2., LIST.2 - 16., 68.), gpui::hsla(0.6, 0.15, 0.22, 1.), 10., list));
        scene.insert_primitive(filled(scaled(LIST.0 + 24., top + 14., 44., 44.), gpui::hsla(row as f32 / 20., 0.7, 0.5, 1.), 22., list));
        scene.insert_primitive(filled(scaled(LIST.0 + 800., top + 10., 10., 10.), gpui::hsla(0.58, 0.9, 0.6, 1.), 5., list));
        text_line(&mut scene, &tiles, LIST.0 + 84., top + 8., 18, list, row);
        text_line(&mut scene, &tiles, LIST.0 + 84., top + 26., 30, list, row + 3);
        text_line(&mut scene, &tiles, LIST.0 + 84., top + 44., 28, list, row + 9);
    }
    // The overlay thumb of the list's scrollbar.
    scene.insert_primitive(filled(scaled(1266., 300., 8., 200.), gpui::hsla(0., 0., 0.8, 0.6), 4., window));
    scene.finish();
    scene
}

struct Variant {
    name: &'static str,
    damage: Option<FrameDamage>,
    samples: Vec<f64>,
}

fn percentile(sorted: &[f64], at: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * at).round() as usize]
}

#[test]
#[ignore]
fn partial_redraw_gpu_cost() {
    let mut renderer = MetalRenderer::new_headless(Arc::new(Mutex::new(InstanceBufferPool::default())));
    let scene = mailbox_scene(&renderer);
    println!(
        "scene: {} quads, {} mono sprites, {} paths, {} shadows, {} underlines, {} poly sprites",
        scene.quads.len(),
        scene.monochrome_sprites.len(),
        scene.paths.len(),
        scene.shadows.len(),
        scene.underlines.len(),
        scene.polychrome_sprites.len(),
    );
    let window = size(DevicePixels(WIDTH), DevicePixels(HEIGHT));

    let list = device(LIST.0 as i32, LIST.1 as i32, LIST.2 as i32, LIST.3 as i32);
    let shift = Shift { region: list, by: point(DevicePixels(0), DevicePixels(60)) };
    let strip = device(438, 166, 842, 60);
    let thumbs = [device(1266, 300, 14, 200), device(1266, 500, 14, 200)];
    let mut variants = vec![
        Variant { name: "A list rect, no shift (today)", damage: Some(FrameDamage::rect(list)), samples: vec![] },
        Variant { name: "B shift 60 + strip", damage: Some(FrameDamage::from_parts(Some(shift), &[strip])), samples: vec![] },
        Variant {
            name: "C shift 60 + strip + 2 thumb rects",
            damage: Some(FrameDamage::from_parts(Some(shift), &[strip, thumbs[0], thumbs[1]])),
            samples: vec![],
        },
        Variant { name: "D empty damage (copy only)", damage: Some(FrameDamage::from_parts(None, &[])), samples: vec![] },
        Variant { name: "E whole frame", damage: None, samples: vec![] },
        Variant { name: "F strip rect, no shift", damage: Some(FrameDamage::rect(strip)), samples: vec![] },
        Variant {
            name: "G shift 60, no rects",
            damage: Some(FrameDamage::from_parts(Some(shift), &[])),
            samples: vec![],
        },
        Variant {
            name: "H 3 rects (strip + thumbs), no shift",
            damage: Some(FrameDamage::from_parts(None, &[strip, thumbs[0], thumbs[1]])),
            samples: vec![],
        },
    ];

    let descriptor = metal::TextureDescriptor::new();
    descriptor.set_width(WIDTH as u64);
    descriptor.set_height(HEIGHT as u64);
    descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    descriptor.set_storage_mode(metal::MTLStorageMode::Private);
    descriptor.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
    let destination = renderer.device.new_texture(&descriptor);

    // The first frame fills the persistent target, as a window's first frame does.
    let first = renderer
        .render_frame_in(&scene, super::OrderRange::ALL, &destination, window, Some(None))
        .expect("the first frame is drawn");
    first.commit();
    first.wait_until_completed();

    let mut in_flight: VecDeque<(usize, metal::CommandBuffer)> = VecDeque::new();
    let gpu_ms = |command_buffer: &metal::CommandBuffer| -> f64 {
        let start: f64 = unsafe { msg_send![&**command_buffer, GPUStartTime] };
        let end: f64 = unsafe { msg_send![&**command_buffer, GPUEndTime] };
        (end - start) * 1e3
    };
    let in_flight_limit = in_flight_limit();
    let filler = std::env::var_os("BENCH_FILLER").is_some();
    for round in 0..WARMUP_ROUNDS + ROUNDS {
        // The variants rotate their place in the round, so none is always charged for the one before it.
        for step in 0..variants.len() {
            let index = (step + round) % variants.len();
            let damage = variants[index].damage.clone();
            if filler {
                // Queued ahead of the measured frame so the GPU is busy when it starts, which keeps its clocks up.
                let busy = renderer
                    .render_frame_in(&scene, super::OrderRange::ALL, &destination, window, Some(None))
                    .expect("the filler is drawn");
                busy.commit();
            }
            let command_buffer = renderer
                .render_frame_in(&scene, super::OrderRange::ALL, &destination, window, Some(damage))
                .expect("the frame is drawn");
            command_buffer.commit();
            // The index is the variant, or usize::MAX for a frame of the warm-up.
            in_flight.push_back((if round < WARMUP_ROUNDS { usize::MAX } else { index }, command_buffer));
            if in_flight.len() > in_flight_limit {
                let (done, command_buffer) = in_flight.pop_front().expect("one is in flight");
                command_buffer.wait_until_completed();
                if done != usize::MAX {
                    variants[done].samples.push(gpu_ms(&command_buffer));
                }
            }
        }
    }
    while let Some((done, command_buffer)) = in_flight.pop_front() {
        command_buffer.wait_until_completed();
        if done != usize::MAX {
            variants[done].samples.push(gpu_ms(&command_buffer));
        }
    }

    println!("{:<40} {:>8} {:>8} {:>8} {:>8}  (ms GPU, n={ROUNDS})", "variant", "median", "p10", "p90", "mean");
    for variant in &mut variants {
        variant.samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let mean = variant.samples.iter().sum::<f64>() / variant.samples.len() as f64;
        println!(
            "{:<40} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
            variant.name,
            percentile(&variant.samples, 0.5),
            percentile(&variant.samples, 0.1),
            percentile(&variant.samples, 0.9),
            mean,
        );
    }
}
