//! Launch splash for the interactive terminal: the banner as an "ember
//! wordmark". The letters of "siGit" are the blank cells in the art and the
//! digits are a flame streaming off behind them, so the word faces right and
//! the trail runs to its left. This is the same animation the siGit app plays
//! at launch (`src/components/splash` in sigit-app); keep the timeline and the
//! art in sync with it.
//!
//! Everything here is a pure function of elapsed time, so the frames can be
//! tested without a terminal. The splash is not decoration: it stays up until
//! the startup checks in `startup.rs` have reported, and never less than
//! [`MIN_SHOW`].

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// The siGit Code banner. The blank cells in rows 2-11 are the letters; the
/// digits are the flame. Mirrored in sigit-app's `bannerArt.ts`.
pub(crate) const BANNER_ART: &str = "\
77777777777777777777777777777777777777777777777777777777777777777777777777777777777777777777
77777777322222222222222222222222222222223777389969902208431358831999699051111177777777777777
1111111125555555555555555555555511113222311159    5002         088    3081771691111111111111
1111111111111111111111111111131136841   1482853332007    05    9043332891    400811111111111
1111111111111111111111111111111201        109    304    40     00    79      100041111111111
333333255555555555555555555552392   102   503    90    7000000005    903    0000023333333333
333333245454545454545454545433381    7600000    302    61    780    109    20009533333333333
3333333333333333333333333333333402      7001    08    761    202    902    90003333333333333
2222255555555555555555555555250899901    49    304    403    08    108    300042222222222222
2222222222222222222222222222269   106    03    901    06    505    402    000052222222222222
2222255555555555555555555555299        708    1002          80     00      90852222222222222
55555555555555555555555555555560953258000866660000051140866908666600008966900065555555555555
88888888888888888888888888888888888888888888888888888888888888888888888888888888888888888888";

/// The splash never leaves before this, even when every check is instant.
pub(crate) const MIN_SHOW: Duration = Duration::from_millis(1400);
/// Contract and fade at the end.
const EXIT: Duration = Duration::from_millis(350);
/// A check that never reports must not hold the terminal hostage.
pub(crate) const HARD_CAP: Duration = Duration::from_millis(4000);

// Graphite canvas and one ember ramp, the same stops as the app's design tokens
// (`--surface-950`, `--surface-600`, `--brand-600`, `--accent`, `--brand-400`,
// `--ink-strong`, `--ink-muted`).
type Rgb = [u8; 3];
const BG: Rgb = [0x12, 0x14, 0x17];
const RAMP: [Rgb; 4] = [
    [0x2d, 0x32, 0x38],
    [0xe8, 0x5d, 0x28],
    [0xfe, 0x6d, 0x36],
    [0xff, 0x8f, 0x5a],
];
pub(crate) const INK: Rgb = [0xf3, 0xf4, 0xf6];
pub(crate) const MUTED: Rgb = [0x6b, 0x72, 0x80];
pub(crate) const EMBER: Rgb = RAMP[2];

/// Heat per digit. 0 and 9 are the densest in the art, 7 and 1 the thinnest.
const HEAT: [f32; 10] = [1.0, 0.2, 0.45, 0.55, 0.5, 0.4, 0.65, 0.15, 0.8, 1.0];

#[derive(Clone, Copy)]
struct Cell {
    x: u16,
    y: u16,
    letter: bool,
    heat: f32,
}

/// The banner parsed into cells.
pub(crate) struct Banner {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    letter_min_x: u16,
    letter_max_x: u16,
}

impl Banner {
    pub(crate) fn parse() -> Self {
        let lines: Vec<&str> = BANNER_ART.lines().collect();
        let cols = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let mut cells = Vec::new();
        let (mut letter_min_x, mut letter_max_x) = (cols, 0);
        for (y, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            for x in 0..cols {
                match chars.get(x as usize).copied().unwrap_or(' ') {
                    ' ' => {
                        if !(2..=11).contains(&y) {
                            continue;
                        }
                        cells.push(Cell {
                            x,
                            y: y as u16,
                            letter: true,
                            heat: 0.0,
                        });
                        letter_min_x = letter_min_x.min(x);
                        letter_max_x = letter_max_x.max(x);
                    }
                    ch => {
                        let heat = ch.to_digit(10).map_or(0.5, |d| HEAT[d as usize]);
                        cells.push(Cell {
                            x,
                            y: y as u16,
                            letter: false,
                            heat,
                        });
                    }
                }
            }
        }
        Self {
            cols,
            rows: lines.len() as u16,
            cells,
            letter_min_x,
            letter_max_x,
        }
    }

    /// First column to draw for a terminal `width` cells wide. A narrow
    /// terminal keeps the word and a short tail and drops the rest of the
    /// trail. `None` when even the word does not fit.
    fn start_col(&self, width: u16) -> Option<u16> {
        let word = self.letter_max_x - self.letter_min_x + 1;
        if width < word + 2 {
            return None;
        }
        if width >= self.cols {
            return Some(0);
        }
        // Keep a margin of flame on the right of the word, then spend the rest
        // on the trail.
        let right_edge = (self.letter_max_x + 1 + 4).min(self.cols);
        Some(right_edge.saturating_sub(width).min(self.letter_min_x))
    }

    /// Cells the wordmark occupies in a terminal of `width`, or `None` when it
    /// is too narrow to draw it.
    pub(crate) fn size(&self, width: u16) -> Option<(u16, u16)> {
        let start = self.start_col(width)?;
        Some(((self.cols - start).min(width), self.rows))
    }
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn ease_out(v: f32) -> f32 {
    1.0 - (1.0 - v).powi(3)
}

/// Cheap value noise: a sin-hash of the cell and time, in 0..1.
fn noise(x: u16, y: u16, t_ms: f32) -> f32 {
    let s = (x as f32 * 12.9898 + y as f32 * 78.233 + t_ms * 0.009).sin() * 43758.547;
    s - s.floor()
}

fn mix(a: Rgb, b: Rgb, k: f32) -> Rgb {
    let k = clamp01(k);
    [0, 1, 2].map(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * k).round() as u8)
}

fn ramp(heat: f32) -> Rgb {
    let f = clamp01(heat) * (RAMP.len() - 1) as f32;
    let i = (f.floor() as usize).min(RAMP.len() - 2);
    mix(RAMP[i], RAMP[i + 1], f - i as f32)
}

/// Whether the terminal can show the ember ramp as 24-bit colour.
pub(crate) fn truecolor_supported() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}

/// An `Rgb` as a ratatui colour: exact on a truecolor terminal, the nearest
/// cell of the 256-colour cube otherwise.
pub(crate) fn color(rgb: Rgb, truecolor: bool) -> Color {
    if truecolor {
        return Color::Rgb(rgb[0], rgb[1], rgb[2]);
    }
    let level = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
    Color::Indexed(16 + 36 * level(rgb[0]) + 6 * level(rgb[1]) + level(rgb[2]))
}

/// `rgb` faded toward the canvas by `1 - k`, as a ratatui colour.
pub(crate) fn fade(rgb: Rgb, k: f32, truecolor: bool) -> Color {
    color(mix(BG, rgb, k), truecolor)
}

/// One frame of the wordmark. `t` is time since the splash started and `exit`
/// the exit progress, 0 to 1. `still` freezes the flicker for the static frame
/// used under reduced motion.
pub(crate) struct Wordmark<'a> {
    pub banner: &'a Banner,
    pub t: Duration,
    pub exit: f32,
    pub still: bool,
    pub truecolor: bool,
}

impl Widget for Wordmark<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let b = self.banner;
        let Some(start) = b.start_col(area.width) else {
            return;
        };
        let visible = (b.cols - start).min(area.width);
        let ox = area.x + (area.width - visible) / 2;
        let t = self.t.as_secs_f32() * 1000.0;
        let e = clamp01(self.exit);

        // 1. Grid: every cell fades in as a faint dot.
        let grid = clamp01(t / 300.0);
        // 2. Ignition: the front sweeps from the right edge to the left.
        let sweep = clamp01((t - 250.0) / 750.0);
        // 3. Word: letters resolve left to right, then settle one cell right.
        let settle = (1.0 - ease_out(clamp01((t - 600.0) / 500.0))).round() as i32;
        // 4. Hold: the flame keeps breathing, quieter.
        let breathe = if t > 1100.0 { 0.4 } else { 1.0 };
        let span = (b.letter_max_x - b.letter_min_x).max(1) as f32;
        let mid = (b.letter_min_x + b.letter_max_x) as f32 / 2.0;

        let mut put = |x: i32, y: u16, glyph: &str, rgb: Rgb| {
            let (min, max) = (ox as i32, (ox + visible) as i32);
            if x < min || x >= max || y >= area.height {
                return;
            }
            if let Some(cell) = buf.cell_mut((x as u16, area.y + y)) {
                cell.set_symbol(glyph)
                    .set_style(Style::default().fg(color(rgb, self.truecolor)));
            }
        };

        for c in &b.cells {
            if c.x < start || c.x - start >= visible {
                continue;
            }
            let gx = ox as i32 + (c.x - start) as i32;

            if c.letter {
                put(gx, c.y, "\u{b7}", mix(BG, RAMP[0], 0.9 * grid * (1.0 - e)));
                let reveal =
                    clamp01((t - (600.0 + (c.x - b.letter_min_x) as f32 / span * 350.0)) / 150.0);
                if reveal > 0.0 {
                    put(
                        gx - settle,
                        c.y,
                        "\u{2588}",
                        mix(BG, INK, reveal * (1.0 - e)),
                    );
                }
                continue;
            }

            let d = (b.cols - 1 - c.x) as f32 / (b.cols - 1) as f32;
            let lit = clamp01((sweep * 1.25 - d) * 5.0);
            // Flicker is strongest on the trail, weakest beside the word.
            let away = clamp01((b.letter_min_x as f32 - c.x as f32) / 25.0);
            let amp = (0.12 + 0.38 * away) * breathe;
            let trail = if c.x < b.letter_min_x { 0.7 } else { 1.0 };
            let n = if self.still { 0.5 } else { noise(c.x, c.y, t) };
            let heat = clamp01(c.heat * trail * lit + (n - 0.5) * 2.0 * amp * lit);

            if lit <= 0.0 {
                put(gx, c.y, "\u{b7}", mix(BG, RAMP[0], 0.9 * grid * (1.0 - e)));
                continue;
            }
            // Exit: the flame contracts toward the word and dims.
            let shift = ((mid - c.x as f32) * 0.3 * ease_out(e)).round() as i32;
            let glyph = if heat < 0.35 {
                "\u{2591}"
            } else if heat < 0.65 {
                "\u{2592}"
            } else {
                "\u{2593}"
            };
            put(
                gx + shift,
                c.y,
                glyph,
                mix(BG, ramp(heat), lit * (0.55 + 0.45 * heat) * (1.0 - e)),
            );
        }
    }
}

/// Where the splash is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Running,
    Done,
}

/// The splash's clock: when it started, whether it has begun to leave, and
/// whether the user asked it to go.
pub(crate) struct Splash {
    start: Instant,
    exit_start: Option<Instant>,
    skipped: bool,
    reduced: bool,
    pub(crate) banner: Banner,
    pub(crate) truecolor: bool,
}

impl Splash {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            start: now,
            exit_start: None,
            skipped: false,
            reduced: reduced_motion(),
            banner: Banner::parse(),
            truecolor: truecolor_supported(),
        }
    }

    /// A key press: leave now, whether or not the checks are in.
    pub(crate) fn skip(&mut self) {
        self.skipped = true;
    }

    pub(crate) fn reduced(&self) -> bool {
        self.reduced
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.start)
    }

    /// Exit progress, 0 while running and 1 once gone.
    pub(crate) fn exit_progress(&self, now: Instant) -> f32 {
        match self.exit_start {
            None => 0.0,
            Some(_) if self.reduced => 1.0,
            Some(at) => {
                clamp01(now.saturating_duration_since(at).as_secs_f32() / EXIT.as_secs_f32())
            }
        }
    }

    /// Advance the clock. `ready` is true once every startup check has
    /// reported. The splash starts leaving when it is ready and the minimum
    /// time has passed, when the user skips, or at the hard cap.
    pub(crate) fn update(&mut self, now: Instant, ready: bool) -> Phase {
        let elapsed = self.elapsed(now);
        let min = if self.reduced {
            Duration::ZERO
        } else {
            MIN_SHOW
        };
        if self.exit_start.is_none()
            && (self.skipped || (ready && elapsed >= min) || elapsed >= HARD_CAP)
        {
            self.exit_start = Some(now);
        }
        if self.exit_progress(now) >= 1.0 {
            Phase::Done
        } else {
            Phase::Running
        }
    }
}

/// `SIGIT_REDUCE_MOTION` set to anything but `0` or empty: draw the finished
/// frame and leave as soon as the checks are in.
fn reduced_motion() -> bool {
    std::env::var("SIGIT_REDUCE_MOTION").is_ok_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(banner: &Banner, width: u16, ms: u64, exit: f32) -> Buffer {
        let area = Rect::new(0, 0, width, banner.rows);
        let mut buf = Buffer::empty(area);
        Wordmark {
            banner,
            t: Duration::from_millis(ms),
            exit,
            still: false,
            truecolor: true,
        }
        .render(area, &mut buf);
        buf
    }

    fn lit(buf: &Buffer, x0: u16, x1: u16) -> usize {
        (0..buf.area.height)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buf[(x, y)].symbol(), "\u{2591}" | "\u{2592}" | "\u{2593}"))
            .count()
    }

    #[test]
    fn banner_is_the_documented_grid() {
        let b = Banner::parse();
        assert_eq!((b.cols, b.rows), (92, 13));
        assert_eq!((b.letter_min_x, b.letter_max_x), (31, 76));
        assert!(
            b.cells
                .iter()
                .filter(|c| c.letter)
                .all(|c| (2..=11).contains(&c.y))
        );
    }

    #[test]
    fn ignition_runs_from_right_to_left() {
        let b = Banner::parse();
        let buf = frame(&b, 92, 500, 0.0);
        assert!(lit(&buf, 66, 92) > lit(&buf, 0, 26) * 3);
    }

    #[test]
    fn letters_are_ink_once_revealed_and_absent_before() {
        let b = Banner::parse();
        let ink = |buf: &Buffer| {
            (0..92)
                .flat_map(|x| (0..13).map(move |y| (x, y)))
                .filter(|&(x, y)| buf[(x, y)].symbol() == "\u{2588}")
                .count()
        };
        assert_eq!(ink(&frame(&b, 92, 400, 0.0)), 0);
        let letters = b.cells.iter().filter(|c| c.letter).count();
        let settled = ink(&frame(&b, 92, 1600, 0.0));
        assert!(settled >= letters - 13, "{settled} of {letters}");
    }

    #[test]
    fn exit_removes_everything() {
        let b = Banner::parse();
        let buf = frame(&b, 92, 1600, 1.0);
        let bg = color(BG, true);
        for y in 0..13 {
            for x in 0..92 {
                let c = &buf[(x, y)];
                if c.symbol() != " " {
                    assert_eq!(c.fg, bg, "({x},{y}) {:?}", c.symbol());
                }
            }
        }
    }

    #[test]
    fn narrow_terminals_crop_the_trail_and_tiny_ones_drop_the_art() {
        let b = Banner::parse();
        assert_eq!(b.start_col(92), Some(0));
        assert_eq!(b.start_col(120), Some(0));
        let start = b.start_col(80).unwrap();
        assert!(start > 0 && start <= b.letter_min_x);
        // The word still fits.
        assert!(b.letter_max_x - start < 80);
        assert_eq!(b.size(50), Some((50, 13)));
        assert_eq!(b.start_col(40), None);
    }

    #[test]
    fn frames_do_not_write_outside_the_area() {
        let b = Banner::parse();
        for width in [48, 60, 80, 92, 140] {
            let area = Rect::new(3, 2, width, 13);
            let mut buf = Buffer::empty(Rect::new(0, 0, width + 10, 20));
            Wordmark {
                banner: &b,
                t: Duration::from_millis(1500),
                exit: 0.0,
                still: false,
                truecolor: false,
            }
            .render(area, &mut buf);
            for y in 0..20 {
                for x in 0..width + 10 {
                    if !(area.x..area.x + area.width).contains(&x)
                        || !(area.y..area.y + 13).contains(&y)
                    {
                        assert_eq!(buf[(x, y)].symbol(), " ", "({x},{y})");
                    }
                }
            }
        }
    }

    fn splash(reduced: bool) -> (Splash, Instant) {
        let t0 = Instant::now();
        let mut s = Splash::new(t0);
        s.reduced = reduced;
        (s, t0)
    }

    #[test]
    fn stays_for_the_minimum_even_when_ready() {
        let (mut s, t0) = splash(false);
        assert_eq!(
            s.update(t0 + Duration::from_millis(1000), true),
            Phase::Running
        );
        assert_eq!(s.exit_progress(t0 + Duration::from_millis(1000)), 0.0);
        let at = t0 + MIN_SHOW;
        assert_eq!(s.update(at, true), Phase::Running);
        assert_eq!(s.update(at + EXIT, true), Phase::Done);
    }

    #[test]
    fn waits_for_checks_then_gives_up_at_the_cap() {
        let (mut s, t0) = splash(false);
        assert_eq!(
            s.update(t0 + Duration::from_millis(3900), false),
            Phase::Running
        );
        assert_eq!(s.exit_progress(t0 + Duration::from_millis(3900)), 0.0);
        let at = t0 + HARD_CAP;
        s.update(at, false);
        assert_eq!(s.update(at + EXIT, false), Phase::Done);
    }

    #[test]
    fn a_key_press_leaves_without_waiting() {
        let (mut s, t0) = splash(false);
        s.skip();
        s.update(t0 + Duration::from_millis(100), false);
        assert_eq!(
            s.update(t0 + Duration::from_millis(450), false),
            Phase::Done
        );
    }

    #[test]
    fn reduced_motion_leaves_as_soon_as_the_checks_are_in() {
        let (mut s, t0) = splash(true);
        assert_eq!(
            s.update(t0 + Duration::from_millis(10), false),
            Phase::Running
        );
        assert_eq!(s.update(t0 + Duration::from_millis(20), true), Phase::Done);
    }

    #[test]
    fn colour_falls_back_to_the_256_cube() {
        assert_eq!(
            color([0xfe, 0x6d, 0x36], true),
            Color::Rgb(0xfe, 0x6d, 0x36)
        );
        assert_eq!(color([0, 0, 0], false), Color::Indexed(16));
        assert_eq!(color([255, 255, 255], false), Color::Indexed(231));
    }
}
