//! Debug: what kind of frame the platform is about to draw, for the `GPUI_GPU_TIMES` log of the
//! Metal renderer (the GPU time of each presented frame beside how the frame was made).

use std::cell::Cell;

use crate::{PatchCounters, ScrollPatchCounters};

#[derive(Default, Clone, Copy)]
struct Snapshot {
    patched_frames: u64,
    thumb_repaints: u64,
    scroll_patched: u64,
}

thread_local! {
    static KIND: Cell<&'static str> = const { Cell::new("whole") };
    static DREW: Cell<bool> = const { Cell::new(false) };
    static LAST: Cell<Snapshot> = const {
        Cell::new(Snapshot { patched_frames: 0, thumb_repaints: 0, scroll_patched: 0 })
    };
}

/// Whether `GPUI_GPU_TIMES` is set.
pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_GPU_TIMES").is_some())
}

/// How the frame the platform is drawing now was made: `whole`, `patch`, `scroll`, `patch+scroll`,
/// `thumb`, or `present` (nothing was drawn since the last presentation).
pub fn kind() -> &'static str {
    KIND.with(Cell::get)
}

pub(crate) fn note_draw() {
    DREW.with(|drew| drew.set(true));
}

pub(crate) fn note_present(patch: PatchCounters, scroll: &ScrollPatchCounters) {
    let last = LAST.with(Cell::get);
    let patched = patch.patched_frames > last.patched_frames;
    let thumb = patch.thumb_repaints > last.thumb_repaints;
    let scrolled = scroll.patched > last.scroll_patched;
    let drew = DREW.with(|drew| drew.replace(false));
    let kind = match (patched, scrolled, thumb, drew) {
        (true, true, _, _) => "patch+scroll",
        (true, false, _, _) => "patch",
        (false, true, _, _) => "scroll",
        (false, false, true, _) => "thumb",
        (false, false, false, true) => "whole",
        (false, false, false, false) => "present",
    };
    KIND.with(|cell| cell.set(kind));
    LAST.with(|cell| {
        cell.set(Snapshot {
            patched_frames: patch.patched_frames,
            thumb_repaints: patch.thumb_repaints,
            scroll_patched: scroll.patched,
        })
    });
}
