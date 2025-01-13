// Copyright 2020 the Xilem Authors and the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Tools and infrastructure for testing widgets.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

use cursor_icon::CursorIcon;
use dpi::LogicalSize;
use image::{DynamicImage, ImageReader, Rgba, RgbaImage};
use tracing::debug;
use vello::util::{block_on_wgpu, RenderContext};
use vello::RendererOptions;
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, ImageCopyBuffer,
    TextureDescriptor, TextureFormat, TextureUsages,
};
use winit::event::Ime;

use crate::action::Action;
use crate::dpi::{LogicalPosition, PhysicalPosition, PhysicalSize};
use crate::event::{PointerButton, PointerEvent, PointerState, TextEvent, WindowEvent};
use crate::passes::anim::run_update_anim_pass;
use crate::render_root::{RenderRoot, RenderRootOptions, RenderRootSignal, WindowSizePolicy};
use crate::testing::screenshots::get_image_diff;
use crate::testing::snapshot_utils::get_cargo_workspace;
use crate::tracing_backend::try_init_test_tracing;
use crate::widget::{WidgetMut, WidgetRef};
use crate::{Color, Handled, Point, Size, Vec2, Widget, WidgetId};

/// A safe headless environment to test widgets in.
///
/// `TestHarness` is a type that simulates a [`RenderRoot`] for testing.
///
/// ## Workflow
///
/// One of the main goals of Masonry is to provide primitives that allow application
/// developers to test their app in a convenient and intuitive way.
/// The basic testing workflow is as follows:
///
/// - Create a harness with some widget.
/// - Send events to the widget as if you were a user interacting with a window.
///   (Rewrite passes are handled automatically.)
/// - Check that the state of the widget graph matches what you expect.
///
/// You can do that last part in a few different ways.
/// You can get a [`WidgetRef`] to a specific widget through methods like [`try_get_widget`](Self::try_get_widget).
/// [`WidgetRef`] implements `Debug`, so you can check the state of an entire tree with something like the [`insta`] crate.
///
/// You can also render the widget tree directly with the [`render`](Self::render) method.
/// Masonry also provides the [`assert_render_snapshot`] macro, which performs snapshot testing on the
/// rendered widget tree automatically.
///
/// ## Fidelity
///
/// `TestHarness` tries to act like the normal masonry environment. It will run the same passes as the normal app after every user event and animation.
///
/// Animations can be simulated with the [`animate_ms`](Self::animate_ms) method.
///
/// One minor difference is that paint only happens when the user explicitly calls rendering
/// methods, whereas in a normal applications you could reasonably expect multiple paint calls
/// between eg any two clicks.
///
/// ## Example
///
/// ```
/// use insta::assert_debug_snapshot;
///
/// use masonry::PointerButton;
/// use masonry::widget::Button;
/// use masonry::Action;
/// use masonry::assert_render_snapshot;
/// use masonry::testing::widget_ids;
/// use masonry::testing::TestHarness;
/// use masonry::testing::TestWidgetExt;
/// use masonry::theme::PRIMARY_LIGHT;
///
/// # /*
/// #[test]
/// # */
/// fn simple_button() {
///     let [button_id] = widget_ids();
///     let widget = Button::new("Hello").with_id(button_id);
///
///     let mut harness = TestHarness::create(widget);
///
///     # if false {
///     assert_debug_snapshot!(harness.root_widget());
///     assert_render_snapshot!(harness, "hello");
///     # }
///
///     assert_eq!(harness.pop_action(), None);
///
///     harness.mouse_click_on(button_id);
///     assert_eq!(
///         harness.pop_action(),
///         Some((Action::ButtonPressed(PointerButton::Primary), button_id))
///     );
/// }
///
/// # simple_button();
/// ```
///
/// [`assert_render_snapshot`]: crate::assert_render_snapshot
/// [`insta`]: https://docs.rs/insta/latest/insta/
pub struct TestHarness {
    render_root: RenderRoot,
    mouse_state: PointerState,
    window_size: PhysicalSize<u32>,
    background_color: Color,
    action_queue: VecDeque<(Action, WidgetId)>,
    has_ime_session: bool,
    ime_rect: (LogicalPosition<f64>, LogicalSize<f64>),
    title: String,
}

/// Parameters for creating a [`TestHarness`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TestHarnessParams {
    /// The size of the virtual window the harness renders into for snapshot testing.
    /// Defaults to [`Self::DEFAULT_SIZE`].
    pub window_size: Size,
    /// The background color of the virtual window.
    /// Defaults to [`Self::DEFAULT_BACKGROUND_COLOR`].
    pub background_color: Color,
    /// The scale factor widgets are rendered at.
    /// Defaults to 1.0.
    pub scale_factor: f64,
}

/// Assert a snapshot of a rendered frame of your app.
///
/// This macro takes a test harness and a name, renders the current state of the app,
/// and stores the render as a PNG next to the text, in a `./screenshots/` folder.
///
/// If a screenshot already exists, the rendered value is compared against this screenshot.
/// The assert passes if both are equal; otherwise, a diff file is created.
/// If the test is run again and the new rendered value matches the old screenshot, the diff file is deleted.
///
/// If a screenshot doesn't exist, the assert will fail; the new screenshot is stored as
/// `./screenshots/<test_name>.new.png`, and must be renamed before the assert will pass.
#[macro_export]
macro_rules! assert_render_snapshot {
    ($test_harness:expr, $name:expr) => {
        $test_harness.check_render_snapshot(
            env!("CARGO_MANIFEST_DIR"),
            file!(),
            module_path!(),
            $name,
        )
    };
}

impl TestHarnessParams {
    /// Default canvas size for tests.
    pub const DEFAULT_SIZE: Size = Size::new(400., 400.);

    /// Default background color for tests.
    pub const DEFAULT_BACKGROUND_COLOR: Color = Color::from_rgba8(0x29, 0x29, 0x29, 0xff);
}

impl Default for TestHarnessParams {
    fn default() -> Self {
        Self {
            window_size: Self::DEFAULT_SIZE,
            background_color: Self::DEFAULT_BACKGROUND_COLOR,
            scale_factor: 1.0,
        }
    }
}

impl TestHarness {
    /// Builds harness with given root widget.
    ///
    /// Window size will be [`TestHarnessParams::DEFAULT_SIZE`].
    /// Background color will be [`TestHarnessParams::DEFAULT_BACKGROUND_COLOR`].
    pub fn create(root_widget: impl Widget) -> Self {
        Self::create_with(root_widget, TestHarnessParams::default())
    }

    /// Builds harness with given root widget and window size.
    pub fn create_with_size(root_widget: impl Widget, window_size: Size) -> Self {
        Self::create_with(
            root_widget,
            TestHarnessParams {
                window_size,
                ..Default::default()
            },
        )
    }

    /// Builds harness with given root widget and additional parameters.
    pub fn create_with(root_widget: impl Widget, params: TestHarnessParams) -> Self {
        let mouse_state = PointerState::empty();
        let window_size = PhysicalSize::new(
            params.window_size.width as _,
            params.window_size.height as _,
        );

        // If there is no default tracing subscriber, we set our own. If one has
        // already been set, we get an error which we swallow.
        // Having a default subscriber is helpful for tests; swallowing errors means
        // we don't panic if the user has already set one, or a test creates multiple
        // harnesses.
        let _ = try_init_test_tracing();

        const ROBOTO: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/fonts/roboto/Roboto-Regular.ttf"
        ));
        let data = ROBOTO.to_vec();

        let mut harness = Self {
            render_root: RenderRoot::new(
                root_widget,
                RenderRootOptions {
                    use_system_fonts: false,
                    size_policy: WindowSizePolicy::User,
                    scale_factor: params.scale_factor,
                    test_font: Some(data),
                },
            ),
            mouse_state,
            window_size,
            background_color: params.background_color,
            action_queue: VecDeque::new(),
            has_ime_session: false,
            ime_rect: Default::default(),
            title: String::new(),
        };
        harness.process_window_event(WindowEvent::Resize(window_size));

        harness
    }

    // --- MARK: PROCESS EVENTS ---

    /// Send a [`WindowEvent`] to the simulated window.
    ///
    /// This will run [rewrite passes](crate::doc::doc_05_pass_system#rewrite-passes) after the event is processed.
    pub fn process_window_event(&mut self, event: WindowEvent) -> Handled {
        let handled = self.render_root.handle_window_event(event);
        self.process_signals();
        handled
    }

    /// Send a [`PointerEvent`] to the simulated window.
    ///
    /// This will run [rewrite passes](crate::doc::doc_05_pass_system#rewrite-passes) after the event is processed.
    pub fn process_pointer_event(&mut self, event: PointerEvent) -> Handled {
        let handled = self.render_root.handle_pointer_event(event);
        self.process_signals();
        handled
    }

    /// Send a [`TextEvent`] to the simulated window.
    ///
    /// This will run [rewrite passes](crate::doc::doc_05_pass_system#rewrite-passes) after the event is processed.
    pub fn process_text_event(&mut self, event: TextEvent) -> Handled {
        let handled = self.render_root.handle_text_event(event);
        self.process_signals();
        handled
    }

    fn process_signals(&mut self) {
        while let Some(signal) = self.render_root.pop_signal() {
            match signal {
                RenderRootSignal::Action(action, widget_id) => {
                    self.action_queue.push_back((action, widget_id));
                }
                RenderRootSignal::StartIme => {
                    self.has_ime_session = true;
                }
                RenderRootSignal::EndIme => {
                    self.has_ime_session = false;
                }
                RenderRootSignal::ImeMoved(position, size) => {
                    self.ime_rect = (position, size);
                }
                RenderRootSignal::RequestRedraw => (),
                RenderRootSignal::RequestAnimFrame => (),
                RenderRootSignal::TakeFocus => (),
                RenderRootSignal::SetCursor(_) => (),
                RenderRootSignal::SetSize(physical_size) => {
                    self.window_size = physical_size;
                    self.process_window_event(WindowEvent::Resize(physical_size));
                }
                RenderRootSignal::SetTitle(title) => {
                    self.title = title;
                }
                RenderRootSignal::DragWindow => (),
                RenderRootSignal::DragResizeWindow(_) => (),
                RenderRootSignal::ToggleMaximized => (),
                RenderRootSignal::Minimize => (),
                RenderRootSignal::Exit => (),
                RenderRootSignal::ShowWindowMenu(_) => (),
            }
        }
    }

    // --- MARK: RENDER ---
    // TODO - We add way too many dependencies in this code
    // TODO - Should be async?
    /// Create a bitmap (an array of pixels), paint the window and return the bitmap as an 8-bits-per-channel RGB image.
    pub fn render(&mut self) -> RgbaImage {
        let (scene, _tree_update) = self.render_root.redraw();
        if std::env::var("SKIP_RENDER_TESTS").is_ok_and(|it| !it.is_empty()) {
            return RgbaImage::from_pixel(1, 1, Rgba([255, 255, 255, 255]));
        }
        // TODO: Cache/share the context
        let mut context = RenderContext::new();
        let device_id =
            pollster::block_on(context.device(None)).expect("No compatible device found");
        let device_handle = &mut context.devices[device_id];
        let device = &device_handle.device;
        let queue = &device_handle.queue;
        let mut renderer = vello::Renderer::new(
            device,
            RendererOptions {
                surface_format: None,
                // TODO - Examine this value
                use_cpu: true,
                num_init_threads: NonZeroUsize::new(1),
                // TODO - Examine this value
                antialiasing_support: vello::AaSupport::area_only(),
            },
        )
        .expect("Got non-Send/Sync error from creating renderer");

        // TODO - fix window_size
        let (width, height) = (self.window_size.width, self.window_size.height);
        let render_params = vello::RenderParams {
            base_color: self.background_color,
            width,
            height,
            antialiasing_method: vello::AaConfig::Area,
        };

        let size = Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("Target texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::STORAGE_BINDING | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        renderer
            .render_to_texture(device, queue, &scene, &view, &render_params)
            .expect("Got non-Send/Sync error from rendering");
        let padded_byte_width = (width * 4).next_multiple_of(256);
        let buffer_size = padded_byte_width as u64 * height as u64;
        let buffer = device.create_buffer(&BufferDescriptor {
            label: Some("val"),
            size: buffer_size,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("Copy out buffer"),
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_byte_width),
                    rows_per_image: None,
                },
            },
            size,
        );

        queue.submit([encoder.finish()]);
        let buf_slice = buffer.slice(..);

        let (sender, receiver) = futures_intrusive::channel::shared::oneshot_channel();
        buf_slice.map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());
        let recv_result = block_on_wgpu(device, receiver.receive()).expect("channel was closed");
        recv_result.expect("failed to map buffer");

        let data = buf_slice.get_mapped_range();
        let mut result_unpadded =
            Vec::<u8>::with_capacity((width * height * 4).try_into().unwrap());
        for row in 0..height {
            let start = (row * padded_byte_width).try_into().unwrap();
            result_unpadded.extend(&data[start..start + (width * 4) as usize]);
        }

        RgbaImage::from_vec(width, height, result_unpadded).expect("failed to create image")
    }

    // --- MARK: EVENT HELPERS ---

    /// Move an internal mouse state, and send a [`PointerMove`](PointerEvent::PointerMove) event to the window.
    pub fn mouse_move(&mut self, pos: impl Into<Point>) {
        // FIXME - Account for scaling
        let pos = pos.into();
        let pos = PhysicalPosition::new(pos.x, pos.y);
        self.mouse_state.physical_position = pos;

        debug!("Harness mouse moved to {}, {}", pos.x, pos.y);

        // TODO: may want to support testing with non-unity scale factors.
        let scale_factor = 1.0;
        self.mouse_state.position = pos.to_logical(scale_factor);

        self.process_pointer_event(PointerEvent::PointerMove(self.mouse_state.clone()));
    }

    /// Send a [`PointerDown`](PointerEvent::PointerDown) event to the window.
    pub fn mouse_button_press(&mut self, button: PointerButton) {
        self.mouse_state.buttons.insert(button);
        self.process_pointer_event(PointerEvent::PointerDown(button, self.mouse_state.clone()));
    }

    /// Send a [`PointerUp`](PointerEvent::PointerUp) event to the window.
    pub fn mouse_button_release(&mut self, button: PointerButton) {
        self.mouse_state.buttons.remove(button);
        self.process_pointer_event(PointerEvent::PointerUp(button, self.mouse_state.clone()));
    }

    /// Send a [`MouseWheel`](PointerEvent::MouseWheel) event to the window.
    pub fn mouse_wheel(&mut self, wheel_delta: Vec2) {
        let pixel_delta = LogicalPosition::new(wheel_delta.x, wheel_delta.y);
        self.process_pointer_event(PointerEvent::MouseWheel(
            pixel_delta,
            self.mouse_state.clone(),
        ));
    }

    /// Send events that lead to a given widget being clicked.
    ///
    /// Combines [`mouse_move`](Self::mouse_move), [`mouse_button_press`](Self::mouse_button_press), and [`mouse_button_release`](Self::mouse_button_release).
    ///
    /// ## Panics
    ///
    /// - If the widget is not found in the tree.
    /// - If the widget is stashed.
    /// - If the widget doesn't accept pointer events.
    /// - If the widget is scrolled out of view.
    #[track_caller]
    pub fn mouse_click_on(&mut self, id: WidgetId) {
        self.mouse_move_to(id);
        self.mouse_button_press(PointerButton::Primary);
        self.mouse_button_release(PointerButton::Primary);
    }

    /// Use [`mouse_move`](Self::mouse_move) to set the internal mouse pos to the center of the given widget.
    ///
    /// ## Panics
    ///
    /// - If the widget is not found in the tree.
    /// - If the widget is stashed.
    /// - If the widget doesn't accept pointer events.
    /// - If the widget is scrolled out of view.
    #[track_caller]
    pub fn mouse_move_to(&mut self, id: WidgetId) {
        let widget = self.get_widget(id);
        let widget_rect = widget.ctx().window_layout_rect();
        let widget_center = widget_rect.center();

        if !widget.ctx().accepts_pointer_interaction() {
            panic!("Widget {id} doesn't accept pointer events");
        }
        if widget.ctx().is_disabled() {
            panic!("Widget {id} is disabled");
        }
        if self
            .render_root
            .get_root_widget()
            .find_widget_at_pos(widget_center)
            .map(|w| w.id())
            != Some(id)
        {
            panic!("Widget {id} is not visible");
        }

        self.mouse_move(widget_center);
    }

    // TODO - Handle complicated IME
    // TODO - Mock Winit keyboard events
    /// Send a [`TextEvent`] for each character in the given string.
    pub fn keyboard_type_chars(&mut self, text: &str) {
        // For each character
        for c in text.split("").filter(|s| !s.is_empty()) {
            let event = TextEvent::Ime(Ime::Commit(c.to_string()));
            self.render_root.handle_text_event(event);
        }
    }

    /// Sets the [focused widget](crate::doc::doc_06_masonry_concepts#text-focus).
    ///
    /// ## Panics
    ///
    /// If the widget is not found in the tree or can't be focused.
    #[track_caller]
    pub fn focus_on(&mut self, id: Option<WidgetId>) {
        if let Some(id) = id {
            let arena = &self.render_root.widget_arena;
            let Some(state) = arena.states.find(id) else {
                panic!("Cannot focus widget {id}: widget not found in tree");
            };
            if state.item.is_stashed {
                panic!("Cannot focus widget {id}: widget is stashed");
            }
            if state.item.is_disabled {
                panic!("Cannot focus widget {id}: widget is disabled");
            }
        }
        self.render_root.global_state.next_focused_widget = id;
        self.render_root.run_rewrite_passes();
        self.process_signals();
    }

    /// Run an animation pass on the widget tree.
    pub fn animate_ms(&mut self, ms: u64) {
        run_update_anim_pass(&mut self.render_root, ms * 1_000_000);
        self.render_root.run_rewrite_passes();
        self.process_signals();
    }

    // --- MARK: GETTERS ---

    /// Return a [`WidgetRef`] to the root widget.
    pub fn root_widget(&self) -> WidgetRef<'_, dyn Widget> {
        self.render_root.get_root_widget()
    }

    /// Return a [`WidgetRef`] to the widget with the given id.
    ///
    /// ## Panics
    ///
    /// Panics if no Widget with this id can be found.
    #[track_caller]
    pub fn get_widget(&self, id: WidgetId) -> WidgetRef<'_, dyn Widget> {
        self.render_root
            .get_widget(id)
            .unwrap_or_else(|| panic!("could not find widget {}", id))
    }

    /// Try to return a [`WidgetRef`] to the widget with the given id.
    pub fn try_get_widget(&self, id: WidgetId) -> Option<WidgetRef<'_, dyn Widget>> {
        self.render_root.get_widget(id)
    }

    // TODO - Link to focus definition in tutorial
    /// Return a [`WidgetRef`] to the [focused widget](crate::doc::doc_06_masonry_concepts#text-focus).
    pub fn focused_widget(&self) -> Option<WidgetRef<'_, dyn Widget>> {
        self.root_widget()
            .find_widget_by_id(self.render_root.global_state.focused_widget?)
    }

    /// Return a [`WidgetRef`] to the widget which [captures pointer events](crate::doc::doc_06_masonry_concepts#pointer-capture).
    pub fn pointer_capture_target(&self) -> Option<WidgetRef<'_, dyn Widget>> {
        self.render_root
            .get_widget(self.render_root.global_state.pointer_capture_target?)
    }

    /// Return the id of the widget which [captures pointer events](crate::doc::doc_06_masonry_concepts#pointer-capture).
    // TODO - This is kinda redundant with the above
    pub fn pointer_capture_target_id(&self) -> Option<WidgetId> {
        self.render_root.global_state.pointer_capture_target
    }

    /// Call the provided visitor on every widget in the widget tree.
    pub fn inspect_widgets(&mut self, f: impl Fn(WidgetRef<'_, dyn Widget>) + 'static) {
        fn inspect(
            widget: WidgetRef<'_, dyn Widget>,
            f: &(impl Fn(WidgetRef<'_, dyn Widget>) + 'static),
        ) {
            f(widget);
            for child in widget.children() {
                inspect(child, f);
            }
        }

        inspect(self.root_widget(), &f);
    }

    /// Get a [`WidgetMut`] to the root widget.
    ///
    /// Because of how `WidgetMut` works, it can only be passed to a user-provided callback.
    pub fn edit_root_widget<R>(
        &mut self,
        f: impl FnOnce(WidgetMut<'_, Box<dyn Widget>>) -> R,
    ) -> R {
        self.render_root.edit_root_widget(f)
    }

    /// Get a [`WidgetMut`] to a specific widget.
    ///
    /// Because of how `WidgetMut` works, it can only be passed to a user-provided callback.
    pub fn edit_widget<R>(
        &mut self,
        id: WidgetId,
        f: impl FnOnce(WidgetMut<'_, Box<dyn Widget>>) -> R,
    ) -> R {
        self.render_root.edit_widget(id, f)
    }

    /// Pop the oldest [`Action`] emitted by the widget tree.
    pub fn pop_action(&mut self) -> Option<(Action, WidgetId)> {
        self.action_queue.pop_front()
    }

    /// Return the app's current cursor icon.
    ///
    /// The cursor icon is the icon that would be displayed to indicate the mouse
    /// position in a visual environment.
    pub fn cursor_icon(&self) -> CursorIcon {
        self.render_root.cursor_icon()
    }

    /// Return whether the app has an IME session in progress.
    ///
    /// This usually means that a widget which [accepts text input](Widget::accepts_text_input) is focused.
    pub fn has_ime_session(&self) -> bool {
        self.has_ime_session
    }

    /// Return the rectangle of the IME session.
    ///
    /// This is usually the layout rectangle of the focused widget.
    pub fn ime_rect(&self) -> (LogicalPosition<f64>, LogicalSize<f64>) {
        self.ime_rect
    }

    /// Return the size of the simulated window.
    pub fn window_size(&self) -> PhysicalSize<u32> {
        self.window_size
    }

    /// Return the title of the simulated window.
    pub fn title(&self) -> std::string::String {
        self.title.clone()
    }

    // --- MARK: SNAPSHOT ---

    /// Method used by [`assert_render_snapshot`]. Use the macro instead.
    ///
    /// Renders the current Widget tree to a pixmap, and compares the pixmap against the
    /// snapshot stored in `./screenshots/module_path__test_name.png`.
    ///
    /// * `manifest_dir`: directory where `Cargo.toml` can be found.
    /// * `test_file_path`: file path the current test is in.
    /// * `test_module_path`: import path of the module the current test is in.
    /// * `test_name`: arbitrary name; second argument of [`assert_render_snapshot`].
    #[doc(hidden)]
    #[track_caller]
    pub fn check_render_snapshot(
        &mut self,
        manifest_dir: &str,
        test_file_path: &str,
        test_module_path: &str,
        test_name: &str,
    ) {
        if std::env::var("SKIP_RENDER_TESTS").is_ok_and(|it| !it.is_empty()) {
            // We still redraw to get some coverage in the paint code.
            let _ = self.render_root.redraw();

            return;
        }

        let new_image: DynamicImage = self.render().into();

        let workspace_path = get_cargo_workspace(manifest_dir);
        let test_file_path_abs = workspace_path.join(test_file_path);
        let folder_path = test_file_path_abs.parent().unwrap();

        let screenshots_folder = folder_path.join("screenshots");
        std::fs::create_dir_all(&screenshots_folder).unwrap();

        let module_str = test_module_path.replace("::", "__");

        let reference_path = screenshots_folder.join(format!("{module_str}__{test_name}.png"));
        let new_path = screenshots_folder.join(format!("{module_str}__{test_name}.new.png"));
        let diff_path = screenshots_folder.join(format!("{module_str}__{test_name}.diff.png"));

        // TODO: If this file is corrupted, it could be an lfs bandwidth/installation issue.
        // Have a warning for that case (i.e. differentiation between not-found and invalid format)
        // and a environment variable to ignore the test in that case.
        if let Ok(reference_file) = ImageReader::open(&reference_path) {
            let ref_image = reference_file.decode().unwrap().to_rgb8();

            if let Some(diff_image) = get_image_diff(&ref_image, &new_image.to_rgb8()) {
                if std::env::var_os("MASONRY_TEST_BLESS").is_some_and(|it| !it.is_empty()) {
                    let _ = std::fs::remove_file(&new_path);
                    let _ = std::fs::remove_file(&diff_path);
                    new_image.save(&reference_path).unwrap();
                } else {
                    new_image.save(&new_path).unwrap();
                    diff_image.save(&diff_path).unwrap();
                    panic!("Snapshot test '{test_name}' failed: Images are different");
                }
            } else {
                // Remove the vestigial new and diff images
                let _ = std::fs::remove_file(&new_path);
                let _ = std::fs::remove_file(&diff_path);
            }
        } else {
            // Remove '<test_name>.new.png' file if it exists
            let _ = std::fs::remove_file(&new_path);
            new_image.save(&new_path).unwrap();
            panic!("Snapshot test '{test_name}' failed: No reference file");
        }
    }
}
