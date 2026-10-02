use chrono::{DateTime, Local, NaiveDate};
use gpui::{
    App, Application, Bounds, Context, Hsla, Window, WindowBounds, WindowControlArea, WindowKind,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const DAYS: usize = 7;
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Default, Clone, Copy)]
struct Day {
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

impl Day {
    fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

#[derive(Default)]
struct Limit {
    pct: f64,
    resets: Option<DateTime<Local>>,
}

#[derive(Default)]
struct Stats {
    days: [Day; DAYS], // index 0 = 6 days ago, last = today
    five_hour: Option<Limit>,
    seven_day: Option<Limit>,
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
fn add_line(line: &str, today: NaiveDate, seen: &mut HashSet<String>, days: &mut [Day; DAYS]) {
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
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let d = &mut days[DAYS - 1 - age as usize];
    d.input += n("input_tokens");
    d.output += n("output_tokens");
    d.cache_write += n("cache_creation_input_tokens");
    d.cache_read += n("cache_read_input_tokens");
}

fn local_usage() -> [Day; DAYS] {
    let since = SystemTime::now() - Duration::from_secs(86400 * (DAYS as u64 + 1));
    let mut files = Vec::new();
    jsonl_files(&claude_dir().join("projects"), since, &mut files);
    let today = Local::now().date_naive();
    let (mut seen, mut days) = (HashSet::new(), [Day::default(); DAYS]);
    for f in files {
        // ponytail: re-reads ~a week of logs every poll; cache per-file (mtime, len) if it gets slow
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            add_line(line, today, &mut seen, &mut days);
        }
    }
    days
}

/// Plan rate limits via the same (unofficial) endpoint Claude Code's /usage uses.
/// Re-reads the token every call since Claude Code refreshes it.
fn plan_limits() -> Result<(Option<Limit>, Option<Limit>), String> {
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
    let body: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let limit = |k: &str| {
        let l = &body[k];
        Some(Limit {
            pct: l["utilization"].as_f64()?,
            resets: l["resets_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Local)),
        })
    };
    Ok((limit("five_hour"), limit("seven_day")))
}

fn fetch() -> Stats {
    let mut s = Stats { days: local_usage(), ..Default::default() };
    match plan_limits() {
        Ok((a, b)) => (s.five_hour, s.seven_day) = (a, b),
        Err(e) => s.limit_err = Some(e),
    }
    s
}

fn fmt_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

struct Widget {
    stats: Option<Stats>,
}

impl Widget {
    fn new(cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                let stats = cx.background_executor().spawn(async { fetch() }).await;
                if this.update(cx, |w, cx| { w.stats = Some(stats); cx.notify() }).is_err() {
                    break;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        })
        .detach();
        Self { stats: None }
    }
}

fn dim() -> Hsla {
    rgb(0x8a8f98).into()
}

fn limit_row(label: &str, l: &Option<Limit>) -> impl IntoElement {
    let (pct, resets) = match l {
        Some(l) => (l.pct, l.resets.map(|t| t.format(" · resets %a %H:%M").to_string()).unwrap_or_default()),
        None => (0.0, " · n/a".into()),
    };
    let color = match pct {
        p if p >= 90.0 => rgb(0xe5534b),
        p if p >= 70.0 => rgb(0xe0a63a),
        _ => rgb(0xd97757),
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .justify_between()
                .child(format!("{label}{resets}"))
                .child(div().text_color(dim()).child(format!("{pct:.0}%"))),
        )
        .child(
            div().h(px(6.)).w_full().rounded_full().bg(rgb(0x2b2d31)).child(
                div().h_full().rounded_full().bg(color).w(gpui::relative((pct / 100.0).clamp(0.0, 1.0) as f32)),
            ),
        )
}

impl Render for Widget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .bg(rgb(0x1b1c1f))
            .border_1()
            .border_color(rgb(0x33353a))
            .text_color(rgb(0xe6e6e6))
            .text_xs()
            .window_control_area(WindowControlArea::Drag)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(div().text_sm().text_color(rgb(0xd97757)).child("Claude usage"))
                    .child(
                        div()
                            .id("close")
                            .px_1()
                            .text_color(dim())
                            .hover(|s| s.text_color(rgb(0xffffff)))
                            .window_control_area(WindowControlArea::Close)
                            .child("✕"),
                    ),
            );
        let Some(s) = &self.stats else {
            return root.child(div().text_color(dim()).child("Loading…"));
        };

        let today = s.days[DAYS - 1];
        let week = s.days.iter().fold(Day::default(), |a, d| Day {
            input: a.input + d.input,
            output: a.output + d.output,
            cache_write: a.cache_write + d.cache_write,
            cache_read: a.cache_read + d.cache_read,
        });
        let max = s.days.iter().map(Day::total).max().unwrap_or(0).max(1);
        let tok_col = |label: &str, d: Day| {
            div()
                .flex()
                .flex_col()
                .child(div().text_color(dim()).child(label.to_string()))
                .child(div().text_lg().child(fmt_tokens(d.total())))
                .child(div().text_color(dim()).child(format!("{} out", fmt_tokens(d.output))))
        };
        let today_date = Local::now().date_naive();

        root.child(limit_row("5-hour", &s.five_hour))
            .child(limit_row("Weekly", &s.seven_day))
            .children(s.limit_err.as_ref().map(|e| div().text_color(rgb(0xe5534b)).child(format!("limits: {e}"))))
            .child(div().flex().gap_6().child(tok_col("Today", today)).child(tok_col("7 days", week)))
            .child(
                div().flex().items_end().gap_1().h(px(56.)).children(s.days.iter().enumerate().map(|(i, d)| {
                    let date = today_date - chrono::Days::new((DAYS - 1 - i) as u64);
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_end()
                        .h_full()
                        .gap_1()
                        .child(
                            div()
                                .w_full()
                                .rounded_sm()
                                .bg(if i == DAYS - 1 { rgb(0xd97757) } else { rgb(0x5a5e66) })
                                .h(px(2. + 38. * d.total() as f32 / max as f32)),
                        )
                        .child(div().text_color(dim()).child(date.format("%a").to_string()))
                })),
            )
    }
}

/// GPUI's Windows PopUp has no topmost flag, so set it directly.
#[cfg(windows)]
fn make_topmost(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }
    const HWND_TOPMOST: isize = -1;
    const SWP_NOSIZE_NOMOVE_NOACTIVATE: u32 = 0x1 | 0x2 | 0x10;
    if let Ok(h) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(h) = h.as_raw()
    {
        unsafe { SetWindowPos(h.hwnd.get(), HWND_TOPMOST, 0, 0, 0, 0, SWP_NOSIZE_NOMOVE_NOACTIVATE) };
    }
}

#[cfg(not(windows))]
fn make_topmost(_: &Window) {} // PopUp kind is already topmost on macOS/Linux

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(300.), px(290.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| {
                make_topmost(window);
                cx.new(Widget::new)
            },
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
        let (mut seen, mut days) = (HashSet::new(), [Day::default(); DAYS]);
        add_line(&line, today, &mut seen, &mut days);
        add_line(&line, today, &mut seen, &mut days); // duplicate ignored
        assert_eq!(days[DAYS - 1].total(), 1112);
        assert_eq!(days[DAYS - 1].output, 10);
        // too old → ignored
        add_line(&line.replace("m1", "m2"), today + chrono::Days::new(DAYS as u64), &mut seen, &mut days);
        assert_eq!(days.iter().map(Day::total).sum::<u64>(), 1112);
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }
}
