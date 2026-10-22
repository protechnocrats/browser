//! Desktop window shell (Linux/macOS/Windows via `winit`). Owns the
//! window and the event loop on the UI thread, and a
//! `gpu_backend::GpuRenderer` on a dedicated **GPU thread** (Milestone 30,
//! ADR 006). It is a pure presenter: it forwards normalised input and size
//! changes to a [`ShellHost`], asks it to advance, and hands the newest
//! `Scene` to the GPU thread without waiting for it. It never sees a DOM, a
//! layout tree or a script, and the host never draws (ADR 005).

use gpu_backend::GpuRenderer;
use rendering::{Renderer, Scene};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;
use winit::{
    event::{ElementState, Event, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    keyboard::{Key, NamedKey},
    window::WindowBuilder,
};

/// Wakes the shell's event loop from any thread — handed to the host
/// (`ShellHost::set_waker`) so a page thread that finished a frame can say
/// "come and collect it" instead of the loop polling.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// The latest-wins hand-off from the UI thread to the GPU thread
/// (ADR 006 §4): the UI thread never waits for the GPU, and the GPU thread
/// only ever draws the newest scene — an unpresented older one is replaced,
/// never queued.
#[derive(Default)]
struct RenderSlot {
    scene: Option<Scene>,
    size: Option<(u32, u32)>,
    redraw: bool,
    shutdown: bool,
}

#[derive(Default)]
struct RenderMailbox {
    slot: Mutex<RenderSlot>,
    ready: Condvar,
}

impl RenderMailbox {
    fn with_slot(&self, change: impl FnOnce(&mut RenderSlot)) {
        change(&mut self.slot.lock().unwrap_or_else(PoisonError::into_inner));
        self.ready.notify_one();
    }

    fn wait(&self) -> RenderSlot {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        while slot.scene.is_none() && slot.size.is_none() && !slot.redraw && !slot.shutdown {
            slot = self.ready.wait(slot).unwrap_or_else(PoisonError::into_inner);
        }
        std::mem::take(&mut *slot)
    }
}

/// The GPU thread: owns the `wgpu` device and surface, and draws whatever
/// the UI thread last handed over. Page threads never reach it (they live
/// in `engine`, which may not depend on `wgpu`).
fn spawn_gpu_thread(mut renderer: GpuRenderer, mailbox: Arc<RenderMailbox>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("gpu".to_string()).spawn(move || {
        let mut last: Option<Scene> = None;
        loop {
            let job = mailbox.wait();
            if job.shutdown {
                return;
            }
            let resized = job.size.is_some();
            if let Some((width, height)) = job.size {
                renderer.resize(width, height);
            }
            let new_scene = job.scene.is_some();
            if job.scene.is_some() {
                last = job.scene;
            }
            if new_scene || resized || job.redraw {
                if let Some(scene) = &last {
                    // Milestone 27: GPU upload + draw + present.
                    let _gpu = diagnostics::tracing::info_span!("gpu_render").entered();
                    renderer.render(scene);
                }
            }
        }
    })
}

/// Milestone 15: the real input this shell forwards into the engine.
/// Deliberately minimal — mouse position/click, scroll delta, and a
/// single resolved key string per keypress — matching the milestone's
/// explicit scope (bubbling-phase click/keydown/scroll only, not the
/// full DOM event/pointer-event surface).
pub enum InputEvent {
    /// Cursor moved to this position (viewport pixels, top-left
    /// origin — same space `LayoutBox`/hit-testing use). Tracked so a
    /// click can be paired with a position (`winit`'s click event
    /// itself carries no position).
    MouseMoved { x: f32, y: f32 },
    /// Left mouse button pressed, at the last known cursor position.
    /// Only the press is forwarded (not release) — sufficient for
    /// "clicking" in the DOM `click`-event sense this milestone scopes
    /// to; drag/press-and-hold interactions are separate, deferred
    /// work (see milestone-15's "Out of scope" list: drag-and-drop).
    MouseClicked { x: f32, y: f32 },
    /// The left button came up (M4's gap: it ends `:active`).
    MouseReleased { x: f32, y: f32 },
    /// Scroll amount in CSS pixels, already normalised by
    /// [`ScrollSettings::normalize`]: positive `delta_y` means "further
    /// down the document", whatever the device (wheel lines or trackpad
    /// pixels) and whatever the user's direction preference.
    Scrolled { delta_x: f32, delta_y: f32 },
    /// A key was pressed, resolved to a string matching the shape of
    /// the real DOM `KeyboardEvent.key` values where possible
    /// (`winit`'s `NamedKey` variants are named to correspond closely
    /// — "Enter", "Backspace", "ArrowLeft", etc.); for a plain
    /// character key, the actual typed character (accounting for
    /// shift/layout). An approximation, not full DOM spec compliance
    /// for every named key value. `ctrl`: whether Control (Command on
    /// macOS) was held — the host's browser shortcuts (Milestone 36).
    KeyPressed { key: String, ctrl: bool },
}

/// How the shell turns wheel and trackpad motion into the engine's
/// normalised scroll deltas (CSS pixels, positive = further down the
/// document). The engine has no scroll preference of its own (ADR 005):
/// this is the one place natural-versus-traditional scrolling and speed
/// are decided. (Since Milestone 36 the host fills it from the profile's
/// settings.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollSettings {
    /// Reverse the direction the platform reports. `winit` already applies
    /// the operating system's own natural-scrolling setting, so this is an
    /// override on top of it, off by default.
    pub invert: bool,
    /// Multiplier on every delta; clamped to 0.1–10 when applied.
    pub speed: f32,
    /// CSS pixels per wheel "line" (mice that report lines, not pixels).
    pub line_height: f32,
}

impl Default for ScrollSettings {
    fn default() -> Self {
        ScrollSettings { invert: false, speed: 1.0, line_height: 20.0 }
    }
}

impl ScrollSettings {
    /// `winit`'s delta → the engine's. `winit`: positive `y` means the
    /// content should move DOWN (the user scrolled UP, towards the top).
    /// Engine: positive `dy` means move further down the document. Hence
    /// the negation.
    pub fn normalize(&self, delta: MouseScrollDelta) -> (f32, f32) {
        let (x, y) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x * self.line_height, y * self.line_height),
            MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
        };
        let direction = if self.invert { 1.0 } else { -1.0 };
        let speed = if self.speed.is_finite() { self.speed.clamp(0.1, 10.0) } else { 1.0 };
        (x * direction * speed, y * direction * speed)
    }
}

/// Whether the window can be seen, from the two signals platforms give:
/// covered (`Occluded`) and minimised (a zero size). Each change returns the
/// new visibility when it changed.
#[derive(Debug, Default)]
struct WindowVisibility {
    occluded: bool,
    minimised: bool,
}

impl WindowVisibility {
    fn visible(&self) -> bool {
        !self.occluded && !self.minimised
    }

    fn occluded(&mut self, occluded: bool) -> Option<bool> {
        let before = self.visible();
        self.occluded = occluded;
        (self.visible() != before).then_some(self.visible())
    }

    fn minimised(&mut self, minimised: bool) -> Option<bool> {
        let before = self.visible();
        self.minimised = minimised;
        (self.visible() != before).then_some(self.visible())
    }
}

/// What the host (`app`, wrapping the engine) gives back from each
/// [`ShellHost::update`].
pub struct HostUpdate {
    /// A newer scene to present, if one was finished.
    pub scene: Option<Scene>,
    /// When to call `update` again if no input arrives; `None` = sleep until
    /// something happens.
    pub wake_at: Option<Instant>,
}

/// The host side of the window: receives size changes and input, and is
/// asked to advance and hand over finished scenes. It never draws and the
/// shell never runs page code (ADR 005): the shell only forwards, waits and
/// presents.
pub trait ShellHost {
    /// Called once before the loop starts: `waker` wakes the event loop from
    /// any thread (the host hands it to the engine's page threads).
    fn set_waker(&mut self, _waker: Waker) {}
    /// The drawable area is now `width` x `height` pixels.
    fn resized(&mut self, width: f32, height: f32);
    /// A user input, already normalised (see [`ScrollSettings`]).
    fn input(&mut self, event: InputEvent);
    /// Whether anyone can see the window: `false` once it is minimised or
    /// fully covered (where the platform says so), `true` again when shown
    /// (M38b's gap; ADR 020). Called on changes only.
    fn window_visible(&mut self, _visible: bool) {}
    /// Advance to `now`.
    fn update(&mut self, now: Instant) -> HostUpdate;
}

/// Runs the window until it is closed: forwards input and size changes to
/// `host`, calls `host.update` once per event-loop iteration and whenever
/// the host asked to be woken, and presents the newest scene the host
/// handed over. Idles (`ControlFlow::Wait`) when the host has nothing
/// scheduled — an idle page costs no CPU.
///
/// Returns `Err` — never panics — when the platform cannot provide what a
/// window needs (no display, no GPU adapter, the event loop failing).
pub fn run(title: &str, initial_width: u32, initial_height: u32, scroll: ScrollSettings, mut host: impl ShellHost + 'static) -> Result<(), ShellError> {
    let event_loop = EventLoopBuilder::<()>::with_user_event()
        .build()
        .map_err(|e| ShellError(format!("could not start the event loop: {e}")))?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    host.set_waker(Arc::new(move || {
        // A send only fails once the loop has exited — nothing to wake.
        let _ = proxy.send_event(());
    }));

    let window = Arc::new(
        WindowBuilder::new()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(initial_width, initial_height))
            .build(&event_loop)
            .map_err(|e| ShellError(format!("could not create the window: {e}")))?,
    );

    let renderer = pollster::block_on(GpuRenderer::new(window.clone(), initial_width, initial_height))
        .map_err(|e| ShellError(e.to_string()))?;
    let render_mailbox = Arc::new(RenderMailbox::default());
    let mut gpu_thread = Some(
        spawn_gpu_thread(renderer, Arc::clone(&render_mailbox)).map_err(|e| ShellError(format!("could not start the GPU thread: {e}")))?,
    );
    let initial = window.inner_size();
    host.resized(initial.width.max(1) as f32, initial.height.max(1) as f32);
    let mut cursor_position = (0.0f32, 0.0f32);
    let mut ctrl = false;
    // M38b's gap: the window is a visibility source. Covered (winit's
    // `Occluded`, where the platform reports it) or minimised (a zero
    // size) hides it.
    let mut visibility = WindowVisibility::default();

    event_loop
        .run(move |event, elwt| match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::Resized(new_size) => {
                    if new_size.width > 0 && new_size.height > 0 {
                        render_mailbox.with_slot(|slot| slot.size = Some((new_size.width, new_size.height)));
                        host.resized(new_size.width as f32, new_size.height as f32);
                    }
                    if let Some(visible) = visibility.minimised(new_size.width == 0 || new_size.height == 0) {
                        host.window_visible(visible);
                    }
                }
                WindowEvent::Occluded(occluded) => {
                    if let Some(visible) = visibility.occluded(occluded) {
                        host.window_visible(visible);
                    }
                }
                // The compositor wants the window redrawn (exposed, restored):
                // the GPU thread redraws the scene it already has.
                WindowEvent::RedrawRequested => render_mailbox.with_slot(|slot| slot.redraw = true),
                WindowEvent::CursorMoved { position, .. } => {
                    cursor_position = (position.x as f32, position.y as f32);
                    host.input(InputEvent::MouseMoved { x: cursor_position.0, y: cursor_position.1 });
                }
                WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                    host.input(InputEvent::MouseClicked { x: cursor_position.0, y: cursor_position.1 });
                }
                WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                    host.input(InputEvent::MouseReleased { x: cursor_position.0, y: cursor_position.1 });
                }
                // Milestone 23: a touch-down is a click at its location.
                // Gestures (scroll, pinch) need their own model and are
                // not forwarded; untested here (no touchscreen).
                WindowEvent::Touch(touch) => {
                    let (x, y) = (touch.location.x as f32, touch.location.y as f32);
                    match touch.phase {
                        TouchPhase::Started => host.input(InputEvent::MouseClicked { x, y }),
                        TouchPhase::Ended | TouchPhase::Cancelled => host.input(InputEvent::MouseReleased { x, y }),
                        TouchPhase::Moved => {}
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let (delta_x, delta_y) = scroll.normalize(delta);
                    host.input(InputEvent::Scrolled { delta_x, delta_y });
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    let state = modifiers.state();
                    ctrl = if cfg!(target_os = "macos") { state.super_key() } else { state.control_key() };
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if event.state == ElementState::Pressed {
                        let key = match &event.logical_key {
                            Key::Character(s) => s.to_string(),
                            Key::Named(named) => named_key_to_string(*named),
                            _ => String::new(),
                        };
                        if !key.is_empty() {
                            host.input(InputEvent::KeyPressed { key, ctrl });
                        }
                    }
                }
                _ => {}
            },
            // After every batch of events, and whenever a `WaitUntil`
            // deadline passes: let the host advance, present what it
            // finished, and sleep until it next needs to run.
            Event::AboutToWait => {
                // The GPU thread ended on its own: it panicked (a driver or
                // device failure). Without it nothing can be shown.
                if gpu_thread.as_ref().is_some_and(|t| t.is_finished()) {
                    eprintln!("error: the GPU thread stopped; closing the window");
                    elwt.exit();
                    return;
                }
                let now = Instant::now();
                let update = host.update(now);
                if let Some(scene) = update.scene {
                    render_mailbox.with_slot(|slot| slot.scene = Some(scene));
                }
                match update.wake_at {
                    Some(at) if at > now => elwt.set_control_flow(ControlFlow::WaitUntil(at)),
                    Some(_) => elwt.set_control_flow(ControlFlow::Poll),
                    None => elwt.set_control_flow(ControlFlow::Wait),
                }
            }
            // A waker call (a page thread published): nothing to do here —
            // `AboutToWait` follows and collects it.
            Event::UserEvent(()) => {}
            Event::LoopExiting => {
                render_mailbox.with_slot(|slot| slot.shutdown = true);
                if let Some(thread) = gpu_thread.take() {
                    let _ = thread.join();
                }
            }
            _ => {}
        })
        .map_err(|e| ShellError(format!("the event loop exited with an error: {e}")))
}

/// Why the desktop shell could not run. Carries a message fit to show a
/// user; see [`run`].
#[derive(Debug)]
pub struct ShellError(pub String);

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ShellError {}

/// Maps the most common `NamedKey` variants to the string real
/// `KeyboardEvent.key` values use — not exhaustive (winit's `NamedKey`
/// has dozens of variants for keys real web content essentially never
/// listens for — media keys, IME controls, etc.); unmapped keys
/// return an empty string, filtered out by the caller before ever
/// reaching `handle_input`.
fn named_key_to_string(key: NamedKey) -> String {
    match key {
        NamedKey::Enter => "Enter",
        NamedKey::Tab => "Tab",
        NamedKey::Space => " ",
        NamedKey::Backspace => "Backspace",
        NamedKey::Delete => "Delete",
        NamedKey::Escape => "Escape",
        NamedKey::ArrowLeft => "ArrowLeft",
        NamedKey::ArrowRight => "ArrowRight",
        NamedKey::ArrowUp => "ArrowUp",
        NamedKey::ArrowDown => "ArrowDown",
        NamedKey::Home => "Home",
        NamedKey::End => "End",
        NamedKey::Shift => "Shift",
        NamedKey::Control => "Control",
        NamedKey::Alt => "Alt",
        _ => "",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    /// M38b's gap: covered or minimised hides the window; each signal is
    /// reported once, on a change, and both must clear to show it again.
    #[test]
    fn covered_or_minimised_hides_the_window() {
        let mut v = WindowVisibility::default();
        assert_eq!(v.occluded(false), None, "no change");
        assert_eq!(v.occluded(true), Some(false));
        assert_eq!(v.minimised(true), None, "already hidden");
        assert_eq!(v.occluded(false), None, "still minimised");
        assert_eq!(v.minimised(false), Some(true));
    }

    #[test]
    fn wheel_up_moves_towards_the_top_of_the_document() {
        // winit: positive y = content moves down = the user scrolled up.
        let s = ScrollSettings::default();
        assert_eq!(s.normalize(MouseScrollDelta::LineDelta(0.0, 1.0)), (-0.0, -20.0));
        assert_eq!(s.normalize(MouseScrollDelta::LineDelta(0.0, -2.0)), (-0.0, 40.0));
        assert_eq!(s.normalize(MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, -7.0))), (-3.0, 7.0));
    }

    #[test]
    fn invert_and_speed_are_the_shells_transform() {
        let s = ScrollSettings { invert: true, speed: 2.0, line_height: 10.0 };
        assert_eq!(s.normalize(MouseScrollDelta::LineDelta(0.0, 1.0)), (0.0, 20.0));
        let wild = ScrollSettings { invert: false, speed: f32::NAN, line_height: 10.0 };
        assert_eq!(wild.normalize(MouseScrollDelta::LineDelta(0.0, 1.0)).1, -10.0, "a non-finite speed falls back to 1");
        let fast = ScrollSettings { invert: false, speed: 1000.0, line_height: 10.0 };
        assert_eq!(fast.normalize(MouseScrollDelta::LineDelta(0.0, 1.0)).1, -100.0, "speed is clamped to 10x");
    }
}
