use crate::{
    AnyWindowHandle, AtlasKey, ComposedLayer, ComposedScene, CompositionSurfaceId, NativePlacement, OrderRange,
    OverlayHitbox, PlatformSurfaceAttachment, AtlasTextureId, AtlasTile, Bounds, DevicePixels,
    DispatchEventResult, GpuSpecs, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformHeadlessRenderer, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, RequestFrameOptions, Scene, Size, TestPlatform, TileId, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
};
use collections::HashMap;
use gpui_util::ResultExt as _;
use image::RgbaImage;
use parking_lot::Mutex;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{self, Arc},
};

pub(crate) struct TestWindowState {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) handle: AnyWindowHandle,
    display: Rc<dyn PlatformDisplay>,
    pub(crate) title: Option<String>,
    pub(crate) edited: bool,
    pub(crate) document_path: Option<std::path::PathBuf>,
    platform: Weak<TestPlatform>,
    // TODO: Replace with `Rc`
    sprite_atlas: Arc<dyn PlatformAtlas>,
    renderer: Option<Box<dyn PlatformHeadlessRenderer>>,
    pub(crate) should_close_handler: Option<Box<dyn FnMut() -> bool>>,
    hit_test_window_control_callback:
        Option<Box<dyn FnMut(Point<Pixels>) -> Option<WindowControlArea>>>,
    input_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    hover_status_change_callback: Option<Box<dyn FnMut(bool)>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved_callback: Option<Box<dyn FnMut()>>,
    appearance_change_callback: Option<Box<dyn FnMut()>>,
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    frame_wake_count: Rc<Cell<usize>>,
    frame_scheduled: bool,
    frame_callback_pending: bool,
    input_handler: Option<PlatformInputHandler>,
    is_fullscreen: bool,
    appearance: WindowAppearance,
    external_drag_files: Vec<(PathBuf, bool)>,
    start_external_drag_result: bool,
    pointer_locked: bool,
    last_mouse_movement: Point<Pixels>,
    scale_factor: f32,
    plain_draws: usize,
    composition_frames: Vec<CompositionFrame>,
    native_surface_events: Rc<RefCell<Vec<NativeSurfaceEvent>>>,
    next_native_surface: usize,
}

/// A layer of a frame the window drew composed, bottom to top.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordedLayer {
    /// A GPUI layer.
    Gpui {
        /// The surface of the layer.
        surface: CompositionSurfaceId,
        /// The orders of the scene it draws.
        orders: OrderRange,
        /// Whether it is the window's own layer.
        is_base: bool,
        /// The primitives and groups of the scene in `orders`.
        primitives: usize,
        /// What the layer claims of the pointer.
        hitboxes: Vec<OverlayHitbox>,
    },
    /// A native layer.
    Native {
        /// The surface of the layer.
        surface: CompositionSurfaceId,
        /// What `platform_handle` of the surface answers: the index of its attachment, from 0.
        attachment: usize,
        /// Where it is, `None` when it is not visible.
        placement: Option<NativePlacement>,
        /// The position in the frame's layers, bottom to top.
        z: usize,
    },
}

impl RecordedLayer {
    /// Whether the layer is a native one that shows.
    pub fn is_visible_native(&self) -> bool {
        matches!(self, RecordedLayer::Native { placement: Some(_), .. })
    }
}

/// A frame drawn with `draw_composed`.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositionFrame {
    /// The layers, bottom to top.
    pub layers: Vec<RecordedLayer>,
}

impl CompositionFrame {
    /// What `composed` puts on the screen, as a platform window records it for a test.
    /// `attachment` of a native layer is what the surface's `platform_handle` answers as a
    /// `usize`: the index of the attachment on the test platform, the content view's address on
    /// AppKit.
    pub fn record(composed: &ComposedScene<'_>) -> Self {
        let layers = composed
            .layers()
            .iter()
            .enumerate()
            .map(|(z, layer)| match layer {
                ComposedLayer::Gpui(layer) => RecordedLayer::Gpui {
                    surface: layer.surface,
                    orders: layer.orders,
                    is_base: layer.is_base,
                    primitives: composed.scene().len_in(layer.orders),
                    hitboxes: layer.hitboxes.clone(),
                },
                ComposedLayer::Native(layer) => RecordedLayer::Native {
                    surface: layer.surface,
                    attachment: *layer
                        .attachment
                        .platform_handle()
                        .expect("a native attachment has a handle")
                        .downcast::<usize>()
                        .expect("a native attachment's handle is a usize"),
                    placement: layer.placement,
                    z,
                },
            })
            .collect();
        Self { layers }
    }

    /// The native layers, bottom to top.
    pub fn natives(&self) -> impl Iterator<Item = &RecordedLayer> {
        self.layers
            .iter()
            .filter(|layer| matches!(layer, RecordedLayer::Native { .. }))
    }
}

/// What happened to the native surfaces of a test window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSurfaceEvent {
    /// The attachment with this index was made.
    Created(usize),
    /// The attachment with this index was dropped, which removes its view.
    Removed(usize),
}

struct TestSurfaceAttachment {
    index: usize,
    events: Rc<RefCell<Vec<NativeSurfaceEvent>>>,
}

impl PlatformSurfaceAttachment for TestSurfaceAttachment {
    fn platform_handle(&self) -> anyhow::Result<Box<dyn Any>> {
        Ok(Box::new(self.index))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for TestSurfaceAttachment {
    fn drop(&mut self) {
        self.events.borrow_mut().push(NativeSurfaceEvent::Removed(self.index));
    }
}

#[derive(Clone)]
pub struct TestWindow(pub(crate) Rc<Mutex<TestWindowState>>);

// Test windows are not backed by a real platform window, so there is no raw
// handle to report; `NotSupported` is `raw_window_handle`'s variant for exactly this.
impl HasWindowHandle for TestWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl HasDisplayHandle for TestWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}

impl TestWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        params: WindowParams,
        platform: Weak<TestPlatform>,
        display: Rc<dyn PlatformDisplay>,
        renderer: Option<Box<dyn PlatformHeadlessRenderer>>,
    ) -> Self {
        let sprite_atlas: Arc<dyn PlatformAtlas> = match &renderer {
            Some(r) => r.sprite_atlas(),
            None => Arc::new(TestAtlas::new()),
        };
        let scale_factor = platform
            .upgrade()
            .map_or(2.0, |platform| platform.window_scale_factor.get());
        Self(Rc::new(Mutex::new(TestWindowState {
            scale_factor,
            bounds: params.bounds,
            display,
            platform,
            handle,
            sprite_atlas,
            renderer,
            title: Default::default(),
            edited: false,
            document_path: None,
            should_close_handler: None,
            hit_test_window_control_callback: None,
            input_callback: None,
            active_status_change_callback: None,
            hover_status_change_callback: None,
            resize_callback: None,
            moved_callback: None,
            appearance_change_callback: None,
            request_frame_callback: None,
            frame_wake_count: Rc::new(Cell::new(0)),
            frame_scheduled: false,
            frame_callback_pending: false,
            input_handler: None,
            is_fullscreen: false,
            appearance: WindowAppearance::Light,
            external_drag_files: Vec::new(),
            start_external_drag_result: false,
            pointer_locked: false,
            last_mouse_movement: Point::default(),
            plain_draws: 0,
            composition_frames: Vec::new(),
            native_surface_events: Rc::new(RefCell::new(Vec::new())),
            next_native_surface: 0,
        })))
    }
    /// How many frames were drawn with `draw`, which a window without native surfaces does.
    pub fn plain_draw_count(&self) -> usize {
        self.0.lock().plain_draws
    }

    /// The frames drawn with `draw_composed`, oldest first.
    pub fn composition_frames(&self) -> Vec<CompositionFrame> {
        self.0.lock().composition_frames.clone()
    }

    /// The last frame drawn with `draw_composed`.
    pub fn composition_frame(&self) -> Option<CompositionFrame> {
        self.0.lock().composition_frames.last().cloned()
    }

    /// The native surfaces made and removed, in order.
    pub fn native_surface_events(&self) -> Vec<NativeSurfaceEvent> {
        self.0.lock().native_surface_events.borrow().clone()
    }

    fn draw_scene(&self, scene: &Scene) {
        let scale_factor = self.scale_factor();
        let mut state = self.0.lock();
        state.frame_callback_pending = true;
        state.frame_scheduled = true;
        let device_size: Size<DevicePixels> = state.bounds.size.to_device_pixels(scale_factor);
        if let Some(renderer) = &mut state.renderer {
            renderer.render_scene(scene, device_size).warn_on_err();
        }
    }

    pub fn simulate_scheduled_frame(&self) -> bool {
        let callback = {
            let mut state = self.0.lock();
            if !std::mem::take(&mut state.frame_scheduled) {
                return false;
            }
            state.frame_callback_pending = false;
            state.request_frame_callback.take()
        };
        let Some(mut callback) = callback else {
            self.0.lock().frame_scheduled = true;
            return false;
        };

        callback(RequestFrameOptions::default());
        self.0.lock().request_frame_callback = Some(callback);
        true
    }

    pub fn frame_scheduled(&self) -> bool {
        self.0.lock().frame_scheduled
    }

    pub fn simulate_resize(&mut self, size: Size<Pixels>) {
        let scale_factor = self.scale_factor();
        let mut lock = self.0.lock();
        // Always update bounds, even if no callback is registered
        lock.bounds.size = size;
        let Some(mut callback) = lock.resize_callback.take() else {
            return;
        };
        drop(lock);
        callback(size, scale_factor);
        self.0.lock().resize_callback = Some(callback);
    }

    pub(crate) fn simulate_active_status_change(&self, active: bool) {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.active_status_change_callback.take() else {
            return;
        };
        drop(lock);
        callback(active);
        self.0.lock().active_status_change_callback = Some(callback);
    }

    pub fn simulate_appearance_change(&self, appearance: WindowAppearance) {
        let mut lock = self.0.lock();
        lock.appearance = appearance;
        let Some(mut callback) = lock.appearance_change_callback.take() else {
            return;
        };
        drop(lock);
        callback();
        self.0.lock().appearance_change_callback = Some(callback);
    }

    /// Returns how many times this window's frame waker has been invoked.
    pub fn frame_wake_count(&self) -> usize {
        self.0.lock().frame_wake_count.get()
    }

    /// Delivers a frame request to the window, as the platform's frame source
    /// would.
    pub fn simulate_frame_request(&self, options: RequestFrameOptions) {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.request_frame_callback.take() else {
            return;
        };
        drop(lock);
        callback(options);
        self.0.lock().request_frame_callback = Some(callback);
    }

    pub fn simulate_input(&mut self, event: PlatformInput) -> bool {
        let mut lock = self.0.lock();
        let Some(mut callback) = lock.input_callback.take() else {
            return false;
        };
        drop(lock);
        let result = callback(event);
        self.0.lock().input_callback = Some(callback);
        !result.propagate
    }

    pub fn external_drag_files(&self) -> Vec<(PathBuf, bool)> {
        self.0.lock().external_drag_files.clone()
    }

    /// Sets the value reported by `last_mouse_movement`, as a real platform does for each mouse move.
    pub fn simulate_mouse_movement(&self, movement: Point<Pixels>) {
        self.0.lock().last_mouse_movement = movement;
    }

    /// Drops the pointer lock the way a platform does on deactivation. The window only notices
    /// on its next activation change, as with a real platform.
    pub fn simulate_pointer_lock_lost(&self) {
        self.0.lock().pointer_locked = false;
    }

    pub fn set_start_external_drag_result(&self, result: bool) {
        self.0.lock().start_external_drag_result = result;
    }
}

impl PlatformWindow for TestWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.lock().bounds
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let mut lock = self.0.lock();
        lock.bounds.size = size;
    }

    fn scale_factor(&self) -> f32 {
        self.0.lock().scale_factor
    }

    fn force_scale_factor(&self, scale_factor: f32) {
        self.0.lock().scale_factor = scale_factor;
    }

    fn appearance(&self) -> WindowAppearance {
        self.0.lock().appearance
    }

    fn display(&self) -> Option<std::rc::Rc<dyn crate::PlatformDisplay>> {
        Some(self.0.lock().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> crate::Modifiers {
        crate::Modifiers::default()
    }

    fn capslock(&self) -> crate::Capslock {
        crate::Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.lock().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.lock().input_handler.take()
    }

    fn prompt(
        &self,
        _level: crate::PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        Some(
            self.0
                .lock()
                .platform
                .upgrade()
                .expect("platform dropped")
                .prompt(msg, detail, answers),
        )
    }

    fn activate(&self) {
        self.0
            .lock()
            .platform
            .upgrade()
            .unwrap()
            .set_active_window(Some(self.clone()))
    }

    fn is_active(&self) -> bool {
        false
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn set_pointer_lock(&self, locked: bool) -> bool {
        let mut state = self.0.lock();
        state.pointer_locked = locked;
        locked
    }

    fn is_pointer_locked(&self) -> bool {
        self.0.lock().pointer_locked
    }

    fn last_mouse_movement(&self) -> Point<Pixels> {
        self.0.lock().last_mouse_movement
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn supports_effect_groups(&self) -> bool {
        self.0
            .lock()
            .renderer
            .as_ref()
            .is_some_and(|renderer| renderer.supports_effect_groups())
    }

    fn set_title(&mut self, title: &str) {
        self.0.lock().title = Some(title.to_owned());
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn set_edited(&mut self, edited: bool) {
        self.0.lock().edited = edited;
    }

    fn set_document_path(&self, path: Option<&std::path::Path>) {
        self.0.lock().document_path = path.map(|p| p.to_path_buf());
    }

    fn show_character_palette(&self) {
        unimplemented!()
    }

    fn minimize(&self) {
        unimplemented!()
    }

    fn zoom(&self) {
        unimplemented!()
    }

    fn toggle_fullscreen(&self) {
        let mut lock = self.0.lock();
        lock.is_fullscreen = !lock.is_fullscreen;
    }

    fn is_fullscreen(&self) -> bool {
        self.0.lock().is_fullscreen
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        // Recording invocations (rather than delivering a frame) lets tests
        // assert the wake protocol without coupling to frame timing; tests
        // deliver frames explicitly via `simulate_frame_request`.
        let frame_wake_count = self.0.lock().frame_wake_count.clone();
        Some(Rc::new(move || {
            frame_wake_count.set(frame_wake_count.get() + 1);
        }))
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.lock().request_frame_callback = Some(callback);
    }

    fn schedule_frame(&self) {
        let mut state = self.0.lock();
        if !state.frame_callback_pending {
            state.frame_scheduled = true;
        }
    }

    fn on_input(&self, callback: Box<dyn FnMut(crate::PlatformInput) -> DispatchEventResult>) {
        self.0.lock().input_callback = Some(callback)
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().active_status_change_callback = Some(callback)
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().hover_status_change_callback = Some(callback)
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.lock().resize_callback = Some(callback)
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().moved_callback = Some(callback)
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.lock().should_close_handler = Some(callback);
    }

    fn on_close(&self, _callback: Box<dyn FnOnce()>) {}

    fn on_hit_test_window_control(
        &self,
        callback: Box<dyn FnMut(Point<Pixels>) -> Option<WindowControlArea>>,
    ) {
        self.0.lock().hit_test_window_control_callback = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().appearance_change_callback = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.0.lock().plain_draws += 1;
        self.draw_scene(scene);
    }

    fn draw_composed(&self, composed: ComposedScene<'_>) {
        self.0.lock().composition_frames.push(CompositionFrame::record(&composed));
        self.draw_scene(composed.scene());
    }

    fn create_native_surface(&self) -> anyhow::Result<Rc<dyn PlatformSurfaceAttachment>> {
        let mut state = self.0.lock();
        let index = state.next_native_surface;
        state.next_native_surface += 1;
        state.native_surface_events.borrow_mut().push(NativeSurfaceEvent::Created(index));
        Ok(Rc::new(TestSurfaceAttachment {
            index,
            events: state.native_surface_events.clone(),
        }))
    }

    fn sprite_atlas(&self) -> sync::Arc<dyn crate::PlatformAtlas> {
        self.0.lock().sprite_atlas.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    fn render_to_image(&self, scene: &Scene) -> anyhow::Result<RgbaImage> {
        let scale_factor = self.scale_factor();
        let mut state = self.0.lock();
        let size = state.bounds.size;
        if let Some(renderer) = &mut state.renderer {
            let device_size: Size<DevicePixels> = size.to_device_pixels(scale_factor);
            renderer.render_scene_to_image(scene, device_size)
        } else {
            anyhow::bail!("render_to_image not available: no HeadlessRenderer configured")
        }
    }

    fn as_test(&mut self) -> Option<&mut TestWindow> {
        Some(self)
    }

    fn composition_frame(&self) -> Option<CompositionFrame> {
        TestWindow::composition_frame(self)
    }

    fn composition_frames(&self) -> Vec<CompositionFrame> {
        TestWindow::composition_frames(self)
    }

    #[cfg(target_os = "windows")]
    fn get_raw_handle(&self) -> windows::Win32::Foundation::HWND {
        unimplemented!()
    }

    fn show_window_menu(&self, _position: Point<Pixels>) {
        unimplemented!()
    }

    fn start_window_move(&self) {}

    fn can_start_external_drag(&self) -> bool {
        true
    }

    fn start_external_drag(&self, payload: &crate::ExternalDragPayload) -> bool {
        let mut state = self.0.lock();
        match payload {
            crate::ExternalDragPayload::Files(paths) => {
                state.external_drag_files.extend_from_slice(paths.entries());
            }
        }
        state.start_external_drag_result
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }
}

pub(crate) struct TestAtlasState {
    next_id: u32,
    tiles: HashMap<AtlasKey, AtlasTile>,
}

pub(crate) struct TestAtlas(Mutex<TestAtlasState>);

impl TestAtlas {
    pub fn new() -> Self {
        TestAtlas(Mutex::new(TestAtlasState {
            next_id: 0,
            tiles: HashMap::default(),
        }))
    }
}

impl PlatformAtlas for TestAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &crate::AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<crate::DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<crate::AtlasTile>> {
        let mut state = self.0.lock();
        if let Some(&tile) = state.tiles.get(key) {
            return Ok(Some(tile));
        }
        drop(state);

        let Some((size, _)) = build()? else {
            return Ok(None);
        };

        let mut state = self.0.lock();
        state.next_id += 1;
        let texture_id = state.next_id;
        state.next_id += 1;
        let tile_id = state.next_id;

        state.tiles.insert(
            key.clone(),
            crate::AtlasTile {
                texture_id: AtlasTextureId {
                    index: texture_id,
                    kind: crate::AtlasTextureKind::Monochrome,
                },
                tile_id: TileId(tile_id),
                padding: 0,
                bounds: crate::Bounds {
                    origin: Point::default(),
                    size,
                },
            },
        );

        Ok(Some(state.tiles[key]))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.0.lock();
        state.tiles.remove(key);
    }

    fn contains(&self, key: &AtlasKey) -> bool {
        self.0.lock().tiles.contains_key(key)
    }
}
