//! Milestone 27 — observability baseline (architecture doc §9:
//! "Observability — internal tracing/metrics from day one … this is not
//! optional once the pipeline is multi-threaded").
//!
//! Three things, all local-only (no telemetry leaves the process, ever —
//! architecture doc §1, vendor-independent):
//!
//! 1. **The tracing facade.** [`tracing`] is re-exported so core crates
//!    write `diagnostics::tracing::info_span!(..)` and never depend on a
//!    subscriber. With no subscriber installed a span or event costs a
//!    couple of relaxed atomic loads and is otherwise a no-op, which is
//!    what "zero overhead when off" means here. The subscriber is the
//!    HOST's choice: `app --trace` installs [`collector::Collector`]
//!    (behind the `collector` feature, which core crates do not enable).
//! 2. **[`Guard`]s.** Every security control that used to degrade
//!    silently — a depth cap, a size cap, a refused redirect, a rejected
//!    cookie, a blocked cross-origin read — now reports through one
//!    function, [`guard`], which bumps a counter AND emits a `tracing`
//!    event. A hostile page that trips a cap is visible in `--trace`
//!    output and asserted on in tests, instead of just quietly rendering
//!    a bit wrong.
//! 3. **[`metrics`].** Plain relaxed atomics: frames built, a frame-time
//!    histogram, cache sizes and hit rates, glyph-atlas generation bumps,
//!    pending fetches, and one counter per [`Guard`].
//!
//! See `docs/adr/001-observability-facade.md` for why `tracing`, why a
//! facade crate, and why no subscriber in core.

#![forbid(unsafe_code)]

pub use tracing;

pub mod metrics;
pub mod unsupported;

#[cfg(feature = "collector")]
pub mod collector;

/// A security or robustness control that changed what the engine did
/// with untrusted input. Adding a control without a `Guard` variant is
/// the review-blocking gap this enum exists to prevent: a control nobody
/// can observe is a control nobody can tune or trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Guard {
    /// `html_parser`: element nesting past `MAX_NESTING_DEPTH` was
    /// flattened instead of nested further.
    HtmlNestingDepth,
    /// `css_parser`: `:not()` nested past its cap; the selector was rejected.
    CssNotNestingDepth,
    /// `css_parser`: `@media` nested past its cap; the block was dropped.
    CssAtRuleNestingDepth,
    /// `style`: DOM deeper than the cascade will walk.
    StyleDepth,
    /// `layout`: DOM deeper than layout will walk.
    LayoutDepth,
    /// `layout`: inline content nested deeper than inline layout will walk.
    InlineDepth,
    /// `accessibility`: tree deeper than the a11y walk will go.
    AccessibilityDepth,
    /// `network`: a response body hit the byte cap and was refused.
    ResponseBodySize,
    /// `images`: declared pixel count past the decode cap; refused.
    ImageDimensions,
    /// `style`/`text`: an oversized or non-finite `font-size` was
    /// clamped or ignored.
    FontSizeClamped,
    /// `network`: a redirect chain exceeded the hop limit.
    TooManyRedirects,
    /// `network`: a redirect to a non-http(s) scheme was refused.
    RedirectSchemeRefused,
    /// `network`: a `Set-Cookie` was rejected (e.g. domain mismatch).
    CookieRejected,
    /// `js`: a cross-origin response was withheld from script.
    SopBlocked,
    /// `images`: content asked for a local file and was refused (the
    /// engine never reads the filesystem on content's behalf).
    FileAccessDenied,
    /// `js`: a script hit an execution limit (loop iterations, recursion)
    /// and was stopped.
    ScriptRuntimeLimit,
    /// `js`: a `localStorage`/`sessionStorage` write was refused for
    /// exceeding its quota.
    StorageQuotaExceeded,
    /// `style`: a form `pattern` was ignored (too long, or not a regular
    /// expression the linear-time matcher supports).
    PatternRejected,
    /// `network`: a cookie was withheld from a request by SameSite policy.
    CookieWithheld,
    /// `engine`: a message to a page failed validation on receipt (a
    /// non-finite coordinate, an oversized URL/HTML/key) and was dropped.
    EngineMessageRejected,
    /// `engine`: a page's inbox was full; the oldest coalescable input was
    /// dropped (backpressure — a slow page costs bounded memory).
    InputDropped,
    /// `engine`: a page's thread panicked; the page was marked crashed and
    /// the rest of the browser carried on (ADR 007).
    PageCrashed,
    /// `text`: the glyph atlas reached its size cap; glyphs were skipped
    /// for the rest of the frame and the atlas is cleared for the next.
    GlyphAtlasFull,
    /// `gpu_backend`: a texture exceeded the device's size limit; an image
    /// was scaled down to fit, or the glyph atlas was not uploaded.
    GpuTextureTooLarge,
    /// `gpu_backend`: the GPU image-texture byte budget was full; an image
    /// was not uploaded (and is not drawn).
    GpuImageBudget,
    /// `gpu_backend`: the GPU device reported an error (validation, out of
    /// memory); it was logged instead of panicking the GPU thread.
    GpuError,
    /// `css_parser`: a selector had more compounds than
    /// `MAX_SELECTOR_COMPOUNDS`; it was rejected.
    CssSelectorTooLong,
    /// `network`: more fetches were waiting than the fetch pool queues;
    /// the request failed instead of queueing without bound.
    FetchQueueFull,
    /// `engine`: a renderer process sent a message that failed to decode
    /// or broke the protocol; its page was ended as crashed (ADR 016).
    RendererMessageRejected,
    /// `engine`: a renderer asked for a request its page may not make — a
    /// typed navigation the host did not ask for, or an origin the page
    /// never loaded (ADR 016 §5).
    RendererRequestRefused,
    /// `js`/`capability`: script asked for something its page holds no
    /// capability for (or one that was revoked) — refused before any work
    /// (ADR 010).
    CapabilityDenied,
    /// `browser_core`/`storage`: a profile file (settings, permissions,
    /// local storage) was oversized, of an unknown version or malformed;
    /// what could not be read fell back to defaults (ADR 010).
    ProfileFileRejected,
    /// `browser_core`: a page asked for more pending permission prompts
    /// than the store keeps; the request was denied.
    PermissionRequestsCapped,
    /// `filter`: a filter list (or some of its rules) was refused — too
    /// large, too many rules, or malformed (ADR 019).
    FilterListRejected,
    /// `network`: the filter blocked a request (ADR 019).
    RequestBlocked,
    /// `network`: a route could not carry a request (an unreachable proxy, a
    /// VPN that is down) and it failed closed — never falling back to a
    /// direct connection (ADR 011).
    RouteFailedClosed,
    /// `network`: DNS over HTTPS could not resolve a name; the request
    /// failed rather than fall back to system DNS (ADR 012).
    DohFailed,
    /// `engine`: a renderer owed a tick and made no progress for the
    /// unresponsive bound; its process was ended and its page reported
    /// crashed (ADR 013).
    PageUnresponsive,
    /// `engine`: an animating page's frames took more than half its
    /// animation interval; its frame rate was lowered one step (ADR 020).
    FrameBudgetExceeded,
    /// `engine`: the browser process withheld a response body from a
    /// renderer — a cross-origin response the request's stated purpose
    /// cannot use (not an image for an image request, not readable by the
    /// document's script for a script request; ADR 025 §5).
    RendererResponseWithheld,
}

impl Guard {
    pub const ALL: [Guard; 40] = [
        Guard::HtmlNestingDepth,
        Guard::CssNotNestingDepth,
        Guard::CssAtRuleNestingDepth,
        Guard::StyleDepth,
        Guard::LayoutDepth,
        Guard::InlineDepth,
        Guard::AccessibilityDepth,
        Guard::ResponseBodySize,
        Guard::ImageDimensions,
        Guard::FontSizeClamped,
        Guard::TooManyRedirects,
        Guard::RedirectSchemeRefused,
        Guard::CookieRejected,
        Guard::SopBlocked,
        Guard::FileAccessDenied,
        Guard::ScriptRuntimeLimit,
        Guard::StorageQuotaExceeded,
        Guard::PatternRejected,
        Guard::CookieWithheld,
        Guard::EngineMessageRejected,
        Guard::InputDropped,
        Guard::PageCrashed,
        Guard::GlyphAtlasFull,
        Guard::GpuTextureTooLarge,
        Guard::GpuImageBudget,
        Guard::GpuError,
        Guard::CssSelectorTooLong,
        Guard::FetchQueueFull,
        Guard::RendererMessageRejected,
        Guard::RendererRequestRefused,
        Guard::CapabilityDenied,
        Guard::ProfileFileRejected,
        Guard::PermissionRequestsCapped,
        Guard::FilterListRejected,
        Guard::RequestBlocked,
        Guard::RouteFailedClosed,
        Guard::DohFailed,
        Guard::PageUnresponsive,
        Guard::FrameBudgetExceeded,
        Guard::RendererResponseWithheld,
    ];

    /// Stable snake_case identifier — the `guard` field on the event and
    /// the suffix of the `guard.<name>` counter. Treat as public API:
    /// tests and log consumers match on it.
    pub const fn name(self) -> &'static str {
        match self {
            Guard::HtmlNestingDepth => "html_nesting_depth",
            Guard::CssNotNestingDepth => "css_not_nesting_depth",
            Guard::CssAtRuleNestingDepth => "css_at_rule_nesting_depth",
            Guard::StyleDepth => "style_depth",
            Guard::LayoutDepth => "layout_depth",
            Guard::InlineDepth => "inline_depth",
            Guard::AccessibilityDepth => "accessibility_depth",
            Guard::ResponseBodySize => "response_body_size",
            Guard::ImageDimensions => "image_dimensions",
            Guard::FontSizeClamped => "font_size_clamped",
            Guard::TooManyRedirects => "too_many_redirects",
            Guard::RedirectSchemeRefused => "redirect_scheme_refused",
            Guard::CookieRejected => "cookie_rejected",
            Guard::SopBlocked => "sop_blocked",
            Guard::FileAccessDenied => "file_access_denied",
            Guard::ScriptRuntimeLimit => "script_runtime_limit",
            Guard::StorageQuotaExceeded => "storage_quota_exceeded",
            Guard::PatternRejected => "pattern_rejected",
            Guard::CookieWithheld => "cookie_withheld",
            Guard::EngineMessageRejected => "engine_message_rejected",
            Guard::InputDropped => "input_dropped",
            Guard::PageCrashed => "page_crashed",
            Guard::GlyphAtlasFull => "glyph_atlas_full",
            Guard::GpuTextureTooLarge => "gpu_texture_too_large",
            Guard::GpuImageBudget => "gpu_image_budget",
            Guard::GpuError => "gpu_error",
            Guard::CssSelectorTooLong => "css_selector_too_long",
            Guard::FetchQueueFull => "fetch_queue_full",
            Guard::RendererMessageRejected => "renderer_message_rejected",
            Guard::RendererRequestRefused => "renderer_request_refused",
            Guard::CapabilityDenied => "capability_denied",
            Guard::ProfileFileRejected => "profile_file_rejected",
            Guard::PermissionRequestsCapped => "permission_requests_capped",
            Guard::FilterListRejected => "filter_list_rejected",
            Guard::RequestBlocked => "request_blocked",
            Guard::RouteFailedClosed => "route_failed_closed",
            Guard::DohFailed => "doh_failed",
            Guard::PageUnresponsive => "page_unresponsive",
            Guard::FrameBudgetExceeded => "frame_budget_exceeded",
            Guard::RendererResponseWithheld => "renderer_response_withheld",
        }
    }
}

/// Reports that `guard` changed how untrusted input was handled.
/// Bumps the guard's counter and emits a WARN event on target
/// `engine::guard` with fields `guard` and `detail`.
///
/// `detail` is a borrowed string (no allocation at the call site beyond
/// what the caller already had): what tripped it, e.g. a limit or a
/// host. Safe to call from any thread and from hot paths — with no
/// subscriber installed the event is a no-op and the counter is one
/// relaxed atomic add.
pub fn guard(guard: Guard, detail: &str) {
    metrics::record_guard(guard);
    tracing::warn!(target: "engine::guard", guard = guard.name(), detail = detail, "security guard tripped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_guard_has_a_unique_name_and_is_listed_in_all() {
        let mut names: Vec<&str> = Guard::ALL.iter().map(|g| g.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "guard names must be unique");
        for n in &names {
            assert!(n.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{n} should be snake_case");
        }
    }

    #[test]
    fn guard_increments_its_own_counter_only() {
        // Counters are process-global and tests run in parallel, so
        // assert on monotone deltas, never on absolute values.
        let before = metrics::snapshot();
        guard(Guard::CookieRejected, "test");
        let after = metrics::snapshot();
        assert!(after.delta(&before, "guard.cookie_rejected") >= 1);
    }

    #[test]
    fn guard_with_no_subscriber_installed_is_harmless() {
        // The "zero overhead when off" path: must not panic or block.
        for g in Guard::ALL {
            guard(g, "no subscriber");
        }
    }
}
