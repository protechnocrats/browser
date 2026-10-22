//! Starting renderer processes (ADR 016): the [`engine::RendererLauncher`]
//! a browser grants its engine. The engine never starts a process itself.

#![forbid(unsafe_code)]

pub mod fonts;
pub use fonts::system_fonts;

use engine::{RendererConnection, RendererControl, RendererLauncher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The renderer binary's file name.
#[cfg(windows)]
pub const RENDERER_BINARY: &str = "browser-renderer.exe";
/// The renderer binary's file name.
#[cfg(not(windows))]
pub const RENDERER_BINARY: &str = "browser-renderer";

/// The renderer process's `main`, for any binary that doubles as the
/// renderer (`app` launches itself with `--renderer`, the way one browser
/// executable serves every process kind): prepares, sandboxes itself, and
/// serves the browser on stdin/stdout until it is done. Never returns.
pub fn renderer_main() -> ! {
    engine::prepare_renderer();
    let report = sandbox::enter();
    if !report.active {
        eprintln!("renderer: running without a full sandbox ({})", report.detail);
    }
    let code = engine::run_renderer(std::io::stdin().lock(), std::io::stdout(), report.active, &report.detail);
    std::process::exit(code);
}

/// Whether this process was started as a renderer.
pub fn is_renderer_process() -> bool {
    std::env::args().any(|a| a == "--renderer")
}

/// Starts renderer processes (`program --renderer`).
pub struct ProcessLauncher {
    program: PathBuf,
}

impl ProcessLauncher {
    pub fn new(program: impl Into<PathBuf>) -> ProcessLauncher {
        ProcessLauncher { program: program.into() }
    }

    /// The running program itself, started as a renderer.
    pub fn current_exe() -> std::io::Result<ProcessLauncher> {
        std::env::current_exe().map(ProcessLauncher::new)
    }

    /// The renderer installed next to the running program, if it is there.
    pub fn beside_current_exe() -> Option<ProcessLauncher> {
        let program = std::env::current_exe().ok()?.parent()?.join(RENDERER_BINARY);
        program.is_file().then(|| ProcessLauncher::new(program))
    }

    pub fn program(&self) -> &Path {
        &self.program
    }
}

impl RendererLauncher for ProcessLauncher {
    fn launch(&self) -> std::io::Result<RendererConnection> {
        let mut child = Command::new(&self.program)
            .arg("--renderer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Nothing of the browser's environment (tokens, proxies, paths)
            // reaches a process that runs web content.
            .env_clear()
            .spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("the renderer's pipes were not created"));
        };
        Ok(RendererConnection { to_renderer: Box::new(stdin), from_renderer: Box::new(stdout), control: Box::new(ChildControl { child }) })
    }
}

struct ChildControl {
    child: Child,
}

impl RendererControl for ChildControl {
    fn terminate(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }

    fn exit_status(&mut self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status.to_string());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
}
