#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // no console window in release

use chrono::{DateTime, Local, NaiveDate};
use gpui::{
    App, Application, Bounds, Context, Hsla, MouseButton, Rgba, Window, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowKind, WindowOptions, div, prelude::*, px, rgb, rgba,
    size,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const DAYS: usize = 7;
const REFRESH: Duration = Duration::from_secs(60); // local logs
const LIMITS_EVERY: Duration = Duration::from_secs(300); // usage endpoint rate-limits (429) if polled faster
const LIMITS_MAX_BACKOFF: Duration = Duration::from_secs(1800);
const TICK: Duration = Duration::from_millis(33);
const HOVER_IN: Duration = Duration::from_millis(250); // dwell before a docked widget expands
const HOVER_OUT: Duration = Duration::from_millis(350); // grace before it collapses again
const FULL_W: f32 = 232.;
const MINI_W: f32 = 96.;
const SNAP_DIST: f32 = 32.; // edges pull the window in from this far while dragging
const MARGIN: f32 = 8.; // gap between a stuck widget and the screen edge
const ORANGE: u32 = 0xe8875f;

#[derive(Clone)]
struct Limit {
    label: String,
    pct: f64,
    resets: Option<DateTime<Local>>,
}

#[derive(Default)]
struct Stats {
    days: [u64; DAYS], // tokens per day; index 0 = 6 days ago, last = today
    limits: Vec<Limit>,
    limit_err: Option<String>,
}

fn claude_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    Path::new(&home).join(".claude")
}

fn jsonl_files(dir: &Path, since: SystemTime, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            jsonl_files(&p, since, out);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since)
        {
            out.push(p);
        }
    }
}

/// Adds one log line's usage to `days`. Streaming writes the same message several
/// times (and resumed sessions copy history), so `seen` dedupes by message id.
fn add_line(line: &str, today: NaiveDate, seen: &mut HashSet<String>, days: &mut [u64; DAYS]) {
    if !line.contains("\"usage\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return };
    let msg = &v["message"];
    let (Some(id), Some(ts)) = (msg["id"].as_str(), v["timestamp"].as_str()) else { return };
    let Ok(ts) = DateTime::parse_from_rfc3339(ts) else { return };
    let age = (today - ts.with_timezone(&Local).date_naive()).num_days();
    if !(0..DAYS as i64).contains(&age) || !seen.insert(id.to_string()) {
        return;
    }
    let u = &msg["usage"];
    days[DAYS - 1 - age as usize] += ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
        .iter()
        .map(|k| u[k].as_u64().unwrap_or(0))
        .sum::<u64>();
}

fn local_usage() -> [u64; DAYS] {
    let since = SystemTime::now() - Duration::from_secs(86400 * (DAYS as u64 + 1));
    let mut files = Vec::new();
    jsonl_files(&claude_dir().join("projects"), since, &mut files);
    let today = Local::now().date_naive();
    let (mut seen, mut days) = (HashSet::new(), [0; DAYS]);
    for f in files {
        // ponytail: re-reads ~a week of logs every poll; cache per-file (mtime, len) if it gets slow
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            add_line(line, today, &mut seen, &mut days);
        }
    }
    days
}

/// Every `{utilization, resets_at}` entry: five_hour, seven_day, and per-model
/// weekly ones (seven_day_opus, seven_day_fable, ...), in that order.
fn parse_limits(body: &Value) -> Vec<Limit> {
    let Some(map) = body.as_object() else { return vec![] };
    let mut out: Vec<(u8, Limit)> = map
        .iter()
        .filter_map(|(k, l)| {
            let (rank, label) = match k.as_str() {
                "five_hour" => (0, "5h".to_string()),
                "seven_day" => (1, "Week".to_string()),
                k => {
                    let m = k.strip_prefix("seven_day_").filter(|m| !m.contains('_'))?;
                    (2, m[..1].to_uppercase() + &m[1..])
                }
            };
            let resets = l["resets_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Local));
            Some((rank, Limit { label, pct: l["utilization"].as_f64()?, resets }))
        })
        .collect();
    out.sort_by_key(|(r, _)| *r);
    out.into_iter().map(|(_, l)| l).collect()
}

/// Plan rate limits via the same (unofficial) endpoint Claude Code's /usage uses.
/// Re-reads the token every call since Claude Code refreshes it.
fn plan_limits() -> Result<Vec<Limit>, String> {
    let creds = std::fs::read_to_string(claude_dir().join(".credentials.json"))
        .map_err(|_| "no credentials (log in to Claude Code)")?;
    let creds: Value = serde_json::from_str(&creds).map_err(|e| e.to_string())?;
    let token = creds["claudeAiOauth"]["accessToken"].as_str().ok_or("no OAuth token")?;
    let body = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| e.to_string())?;
    Ok(parse_limits(&serde_json::from_str(&body).map_err(|e| e.to_string())?))
}

fn fmt_until(t: DateTime<Local>) -> String {
    let m = (t - Local::now()).num_minutes().max(0);
    match m {
        1440.. => format!("{}d {}h", m / 1440, m % 1440 / 60),
        60.. => format!("{}h {}m", m / 60, m % 60),
        _ => format!("{m}m"),
    }
}

fn level_color(pct: f64) -> Rgba {
    match pct {
        p if p >= 90.0 => rgb(0xf06a6a),
        p if p >= 70.0 => rgb(0xf0b44c),
        _ => rgb(ORANGE),
    }
}

fn bar(l: &Limit) -> gpui::Div {
    div().h(px(5.)).rounded_full().bg(rgba(0xffffff1a)).child(
        div()
            .h_full()
            .rounded_full()
            .bg(level_color(l.pct))
            .w(gpui::relative((l.pct / 100.0).clamp(0.0, 1.0) as f32)),
    )
}

fn dim() -> Hsla {
    rgb(0x9aa0aa).into()
}

/// Where a docked widget sits on one axis: against the start/end edge, or at a fixed coordinate.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Pin {
    Start,
    End,
    At(i32),
}

/// Screen rect in physical pixels: (left, top, right, bottom).
type Rect = (i32, i32, i32, i32);

/// Docks to whichever screen edges the window was dropped near (one edge, or two in a corner).
fn near_edges(w: Rect, work: Rect, dist: i32) -> Option<(Pin, Pin)> {
    // window edges lo/hi against work-area edges wlo/whi
    let pin = |lo: i32, hi: i32, wlo: i32, whi: i32| {
        if (lo - wlo).abs() <= dist {
            Some(Pin::Start)
        } else if (whi - hi).abs() <= dist {
            Some(Pin::End)
        } else {
            None
        }
    };
    let x = pin(w.0, w.2, work.0, work.2);
    let y = pin(w.1, w.3, work.1, work.3);
    (x.is_some() || y.is_some()).then(|| (x.unwrap_or(Pin::At(w.0)), y.unwrap_or(Pin::At(w.1))))
}

/// Rect of size w×h pinned per `dock`, kept inside the work area with margin `m`.
fn anchored(dock: (Pin, Pin), work: Rect, w: i32, h: i32, m: i32) -> Rect {
    let place = |p: Pin, lo: i32, hi: i32, len: i32| match p {
        Pin::Start => lo + m,
        Pin::End => hi - m - len,
        Pin::At(v) => v.clamp(lo + m, (hi - m - len).max(lo + m)),
    };
    let (x, y) = (place(dock.0, work.0, work.2, w), place(dock.1, work.1, work.3, h));
    (x, y, x + w, y + h)
}

/// Live snapping while dragging: an axis within `d` of a work-area edge (or past it)
/// sticks to that edge, so the window slides along it until pulled more than `d` away.
fn magnet(r: Rect, work: Rect, m: i32, d: i32) -> Rect {
    let axis = |lo: i32, hi: i32, wlo: i32, whi: i32| {
        if lo <= wlo + m + d {
            wlo + m
        } else if hi >= whi - m - d {
            whi - m - (hi - lo)
        } else {
            lo
        }
    };
    let (x, y) = (axis(r.0, r.2, work.0, work.2), axis(r.1, r.3, work.1, work.3));
    (x, y, x + r.2 - r.0, y + r.3 - r.1)
}

struct Widget {
    stats: Option<Stats>,
    #[cfg(windows)]
    hwnd: Option<win::HWND>,
    dock: Option<(Pin, Pin)>,
    mini: bool,
    pending: Option<Instant>, // when the cursor started asking for an expand/collapse
}

impl Widget {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            // last good limits stay on screen when a fetch fails; errors back off 5→10→20→30 min
            let (mut limits, mut err, mut next, mut backoff) = (vec![], None, Instant::now(), LIMITS_EVERY);
            loop {
                let poll = Instant::now() >= next;
                let (days, res) =
                    cx.background_executor().spawn(async move { (local_usage(), poll.then(plan_limits)) }).await;
                match res {
                    Some(Ok(l)) => (limits, err, next, backoff) = (l, None, Instant::now() + LIMITS_EVERY, LIMITS_EVERY),
                    Some(Err(e)) => {
                        (err, next) = (Some(e), Instant::now() + backoff);
                        backoff = (backoff * 2).min(LIMITS_MAX_BACKOFF);
                    }
                    None => {}
                }
                let stats = Stats { days, limits: limits.clone(), limit_err: err.clone() };
                if this.update(cx, |w, cx| { w.stats = Some(stats); cx.notify() }).is_err() {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        })
        .detach();
        // ponytail: polls the cursor for hover (GPUI's hover flag drops over drag areas); dragging itself is event-driven
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let Ok(place) = this.update_in(cx, |w, window, cx| w.tick(window, cx)) else { break };
                // move/resize only after the update returns: GPUI drops the resize event
                // if it arrives mid-update, leaving the layout at the old size
                if let Some(place) = place {
                    place();
                }
            }
        })
        .detach();
        Self {
            stats: None,
            #[cfg(windows)]
            hwnd: win::init(window, (MARGIN * window.scale_factor()) as i32, (SNAP_DIST * window.scale_factor()) as i32),
            dock: None,
            mini: false,
            pending: None,
        }
    }

    fn rows(&self) -> f32 {
        self.stats.as_ref().map_or(2, |s| s.limits.len().max(1)) as f32
    }

    fn full_h(&self) -> f32 {
        112. + 22. * self.rows()
    }

    fn mini_h(&self) -> f32 {
        16. + 5. * self.rows() + 4. * (self.rows() - 1.) // padding + bars + gaps
    }

    /// Docks when a drag ends against an edge; while docked, collapses to the bars unless hovered.
    #[cfg(windows)]
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Box<dyn FnOnce()>> {
        use std::sync::atomic::Ordering::Relaxed;
        let hwnd = self.hwnd?;
        if win::MOVING.load(Relaxed) {
            return None; // the WM_MOVING hook handles snapping live
        }
        let (cur, work) = win::rects(hwnd)?;
        let s = window.scale_factor();
        if win::DROPPED.swap(false, Relaxed) {
            // the magnet already aligned it, so "near" just means touching the margin
            self.dock = near_edges(cur, work, ((MARGIN + 2.) * s) as i32);
            self.pending = None;
        }
        let (inside, button) = win::cursor_in(cur);
        // expand on hover, but not while the button is held: that press may be a drag of the bars
        let want_mini = !inside || (self.mini && button);
        let mini = if self.dock.is_none() {
            false
        } else if want_mini == self.mini {
            self.pending = None;
            self.mini
        } else {
            let since = *self.pending.get_or_insert_with(Instant::now);
            let wait = if self.mini { HOVER_IN } else { HOVER_OUT };
            if since.elapsed() >= wait {
                self.pending = None;
                want_mini
            } else {
                self.mini
            }
        };
        let (w, h) = if mini { (MINI_W, self.mini_h()) } else { (FULL_W, self.full_h()) };
        let (w, h) = ((w * s).round() as i32, (h * s).round() as i32);
        let target = match self.dock {
            Some(d) => anchored(d, work, w, h, (MARGIN * s) as i32),
            None => (cur.0, cur.1, cur.0 + w, cur.1 + h),
        };
        if mini != self.mini {
            self.mini = mini;
            cx.notify();
        }
        (target != cur).then(|| Box::new(move || win::place(hwnd, target)) as Box<dyn FnOnce()>)
    }

    #[cfg(not(windows))]
    fn tick(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<Box<dyn FnOnce()>> {
        None // ponytail: edge docking is Windows-only
    }

    /// Docked: just the limit bars.
    fn render_mini(&self) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(4.))
            .p(px(8.))
            .window_control_area(WindowControlArea::Drag) // drag the bars to slide along the edge
            .children(self.stats.iter().flat_map(|s| &s.limits).map(|l| bar(l).w_full()))
    }

    fn render_full(&self, s: &Stats) -> impl IntoElement {
        let max = s.days.iter().copied().max().unwrap_or(0).max(1);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().flex_col().gap(px(6.)).children(s.limits.iter().map(|l| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(16.))
                    .child(div().w(px(38.)).text_color(dim()).child(l.label.clone()))
                    .child(bar(l).flex_1().h(px(6.)))
                    .child(div().w(px(30.)).flex().justify_end().child(format!("{:.0}%", l.pct)))
                    .child(
                        div()
                            .w(px(44.))
                            .flex()
                            .justify_end()
                            .text_color(dim())
                            .child(l.resets.map(fmt_until).unwrap_or_default()),
                    )
            })))
            .children(s.limit_err.as_ref().filter(|_| s.limits.is_empty()).map(|e| {
                let msg = if e.contains("429") { "rate limited, retrying in a few min".into() } else { format!("limits: {e}") };
                div().text_color(rgb(0xf06a6a)).child(msg)
            }))
            .child(div().flex().items_end().gap(px(3.)).h(px(32.)).children(
                s.days.iter().enumerate().map(|(i, d)| {
                    div()
                        .flex_1()
                        .rounded(px(3.))
                        .bg(if i == DAYS - 1 { rgb(ORANGE).into() } else { rgba(0xffffff40) })
                        .h(px(3. + 29. * *d as f32 / max as f32))
                }),
            ))
    }
}

impl Render for Widget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let root = div().size_full().bg(rgba(0x16171bb8)).text_color(rgb(0xf0f0f2)).text_xs();
        if self.mini {
            return root.child(self.render_mini()).into_any_element();
        }
        root.flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        // drag by the header only: a whole-window drag area would swallow the ✕ click
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(ORANGE))
                            .window_control_area(WindowControlArea::Drag)
                            .child("✻ Claude") // ✻ not ✳: the latter renders as a green emoji,
                    )
                    .child(
                        div()
                            .id("close")
                            .px_1()
                            .rounded_full()
                            .text_color(dim())
                            .cursor_pointer()
                            .hover(|s| s.text_color(rgb(0xffffff)).bg(rgba(0xffffff1a)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.quit())
                            .child("✕"),
                    ),
            )
            .child(match &self.stats {
                Some(s) => self.render_full(s).into_any_element(),
                None => div().text_color(dim()).child("Loading…").into_any_element(),
            })
            .into_any_element()
    }
}

#[cfg(windows)]
mod win {
    pub use windows::Win32::Foundation::HWND;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering::Relaxed};
    use windows::Win32::{
        Foundation::{LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{Dwm::*, Gdi::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    };

    // ponytail: globals, since there's exactly one window
    pub static MOVING: AtomicBool = AtomicBool::new(false);
    pub static DROPPED: AtomicBool = AtomicBool::new(false);
    static MARGIN: AtomicI32 = AtomicI32::new(0);
    static SNAP: AtomicI32 = AtomicI32::new(0);
    static GPUI_PROC: AtomicIsize = AtomicIsize::new(0);
    static GRAB: (AtomicI32, AtomicI32) = (AtomicI32::new(0), AtomicI32::new(0)); // cursor offset in the window

    /// Wraps GPUI's window procedure to snap the rect live during the native drag loop.
    unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_ENTERSIZEMOVE => unsafe {
                MOVING.store(true, Relaxed);
                let (mut pt, mut r) = (POINT::default(), RECT::default());
                let _ = GetCursorPos(&mut pt);
                let _ = GetWindowRect(h, &mut r);
                GRAB.0.store(pt.x - r.left, Relaxed);
                GRAB.1.store(pt.y - r.top, Relaxed);
            },
            WM_EXITSIZEMOVE => {
                MOVING.store(false, Relaxed);
                DROPPED.store(true, Relaxed);
            }
            WM_MOVING => unsafe {
                let r = &mut *(l.0 as *mut RECT);
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
                // the cursor's monitor, so the widget can still be dragged to another screen
                if GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
                    let w = mi.rcWork;
                    // position from the cursor, not from `r`: Windows derives `r` from the last
                    // (already snapped) rect, so snapping it would never let go of an edge
                    let (x, y) = (pt.x - GRAB.0.load(Relaxed), pt.y - GRAB.1.load(Relaxed));
                    let m = super::magnet(
                        (x, y, x + r.right - r.left, y + r.bottom - r.top),
                        (w.left, w.top, w.right, w.bottom),
                        MARGIN.load(Relaxed),
                        SNAP.load(Relaxed),
                    );
                    (r.left, r.top, r.right, r.bottom) = m;
                }
            },
            _ => {}
        }
        unsafe { CallWindowProcW(std::mem::transmute(GPUI_PROC.load(Relaxed)), h, msg, w, l) }
    }

    /// (cursor inside `r`, left button held)
    pub fn cursor_in(r: super::Rect) -> (bool, bool) {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let inside = (r.0..r.2).contains(&pt.x) && (r.1..r.3).contains(&pt.y);
            (inside, GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0)
        }
    }

    /// Topmost (GPUI's PopUp kind isn't on Windows), DWM-rounded corners (blur included),
    /// and the drag hook. `margin`/`snap` in physical px.
    // ponytail: margin/snap use the starting monitor's DPI; recompute per monitor if mixed-DPI setups look off
    pub fn init(window: &gpui::Window, margin: i32, snap: i32) -> Option<HWND> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::Win32(h) = HasWindowHandle::window_handle(window).ok()?.as_raw() else {
            return None;
        };
        let hwnd = HWND(h.hwnd.get() as _);
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as _,
                size_of_val(&pref) as u32,
            );
            MARGIN.store(margin, Relaxed);
            SNAP.store(snap, Relaxed);
            GPUI_PROC.store(SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc as *const () as isize), Relaxed);
        }
        Some(hwnd)
    }

    /// (window rect, work area of its monitor) in physical pixels.
    pub fn rects(hwnd: HWND) -> Option<(super::Rect, super::Rect)> {
        let t = |r: RECT| (r.left, r.top, r.right, r.bottom);
        unsafe {
            let mut r = RECT::default();
            GetWindowRect(hwnd, &mut r).ok()?;
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).ok().ok()?;
            Some((t(r), t(mi.rcWork)))
        }
    }

    pub fn place(hwnd: HWND, r: super::Rect) {
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), r.0, r.1, r.2 - r.0, r.3 - r.1, SWP_NOACTIVATE);
        }
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(FULL_W), px(178.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                is_resizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Widget::new(window, cx)),
        )
        .unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_line_buckets_and_dedupes() {
        let today = Local::now().date_naive();
        let ts = Local::now().to_rfc3339();
        let line = format!(
            r#"{{"timestamp":"{ts}","message":{{"id":"m1","usage":{{"input_tokens":2,"output_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000}}}}}}"#
        );
        let (mut seen, mut days) = (HashSet::new(), [0; DAYS]);
        add_line(&line, today, &mut seen, &mut days);
        add_line(&line, today, &mut seen, &mut days); // duplicate ignored
        assert_eq!(days[DAYS - 1], 1112);
        // too old → ignored
        add_line(&line.replace("m1", "m2"), today + chrono::Days::new(DAYS as u64), &mut seen, &mut days);
        assert_eq!(days.iter().sum::<u64>(), 1112);
    }

    #[test]
    fn limits_ordered_and_filtered() {
        let body: Value = serde_json::from_str(
            r#"{"seven_day_fable":{"utilization":5,"resets_at":null},"seven_day":{"utilization":24.0},
                "five_hour":{"utilization":11.0},"seven_day_opus":null,"seven_day_oauth_apps":{"utilization":1},
                "extra_usage":{"is_enabled":false}}"#,
        )
        .unwrap();
        let labels: Vec<_> = parse_limits(&body).into_iter().map(|l| l.label).collect();
        assert_eq!(labels, ["5h", "Week", "Fable"]);
    }

    #[test]
    fn edge_docking() {
        let work = (0, 0, 1920, 1040);
        let d = near_edges((1700, 900, 1900, 1030), work, 48);
        assert_eq!(d, Some((Pin::End, Pin::End))); // bottom-right corner
        assert_eq!(anchored(d.unwrap(), work, 100, 30, 8), (1812, 1002, 1912, 1032));
        let d = near_edges((1700, 400, 1910, 600), work, 48);
        assert_eq!(d, Some((Pin::End, Pin::At(400)))); // right edge, keeps its height
        assert_eq!(anchored(d.unwrap(), work, 100, 900, 8), (1812, 132, 1912, 1032)); // clamped on screen
        assert_eq!(near_edges((500, 500, 700, 700), work, 48), None);
        // magnet: near/past the right edge sticks (sliding keeps y), far away is free
        assert_eq!(magnet((1700, 300, 1900, 400), work, 8, 32), (1712, 300, 1912, 400));
        assert_eq!(magnet((1800, 300, 2000, 400), work, 8, 32), (1712, 300, 1912, 400));
        assert_eq!(magnet((1000, 300, 1200, 400), work, 8, 32), (1000, 300, 1200, 400));
        assert_eq!(magnet((-50, -50, 150, 50), work, 8, 32), (8, 8, 208, 108));
    }
}
