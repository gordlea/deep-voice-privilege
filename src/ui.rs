//! Interactive Ratatui front end: live controls plus spectrum, scope and level displays.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, BorderType, Chart, Dataset, GraphType, LineGauge, Paragraph, Sparkline};
use ratatui::{DefaultTerminal, Frame};
use ringbuf::traits::Consumer;

use crate::analysis::{self, FFT_SIZE, Spectrum};
use crate::audio::Engine;
use crate::params::{GATE_OFF_DB, Knob, Params, Preset};

const FRAME: Duration = Duration::from_millis(33);
const SPECTRUM_LO_HZ: f32 = 63.0;
const SPECTRUM_HI_HZ: f32 = 6300.0;
const SPECTRUM_BANDS: usize = 160;
const SPECTRUM_FLOOR_DB: f32 = -90.0;
/// How fast spectrum bars fall back after a peak, per frame.
const SPECTRUM_DECAY_DB: f32 = 2.0;
const METER_FLOOR_DB: f32 = -60.0;
const SCOPE_SAMPLES: usize = 1024;
const LEVEL_HISTORY: usize = 240;
/// Frames a pitch reading is held after the voice stops, so the readout doesn't flicker.
const PITCH_HOLD_FRAMES: u32 = 15;
/// `+`/`-` always adjust pitch by this much, whichever control is selected.
const PITCH_HOTKEY_STEP: f32 = 0.5;
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 22;

pub fn run(engine: Engine) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = App::new(engine).run(&mut terminal);
    ratatui::restore();
    result
}

/// Semantic colors. With `NO_COLOR` set, everything falls back to the terminal default and
/// selection relies on reverse video and bold.
struct Theme {
    accent: Style,
    input: Style,
    muted: Style,
    good: Style,
    warn: Style,
    bad: Style,
    selected: Style,
}

impl Theme {
    fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        if no_color {
            let plain = Style::default();
            return Self {
                accent: plain.add_modifier(Modifier::BOLD),
                input: plain.add_modifier(Modifier::DIM),
                muted: plain.add_modifier(Modifier::DIM),
                good: plain,
                warn: plain.add_modifier(Modifier::BOLD),
                bad: plain.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                selected: plain.add_modifier(Modifier::REVERSED | Modifier::BOLD),
            };
        }
        Self {
            accent: Style::default().fg(Color::Magenta),
            input: Style::default().fg(Color::Cyan),
            muted: Style::default().fg(Color::DarkGray),
            good: Style::default().fg(Color::Green),
            warn: Style::default().fg(Color::Yellow),
            bad: Style::default().fg(Color::Red),
            selected: Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        }
    }
}

struct PitchReadout {
    hz: Option<f32>,
    missing: u32,
}

impl PitchReadout {
    fn new() -> Self {
        Self { hz: None, missing: 0 }
    }

    fn update(&mut self, detected: Option<f32>) {
        match (detected, self.hz) {
            // Smooth only small changes; a big jump is a new note and should show at once.
            (Some(new), Some(old)) if (new / old - 1.0).abs() < 0.1 => {
                self.hz = Some(old + (new - old) * 0.4);
                self.missing = 0;
            }
            (Some(new), _) => {
                self.hz = Some(new);
                self.missing = 0;
            }
            (None, _) => {
                self.missing += 1;
                if self.missing > PITCH_HOLD_FRAMES {
                    self.hz = None;
                }
            }
        }
    }
}

struct App {
    engine: Engine,
    theme: Theme,
    selected: usize,
    quit: bool,

    in_history: VecDeque<f32>,
    out_history: VecDeque<f32>,
    frame_in: Vec<f32>,
    frame_out: Vec<f32>,

    spectrum: Spectrum,
    in_bands: Vec<f32>,
    out_bands: Vec<f32>,
    in_db: f32,
    out_db: f32,
    out_level_history: VecDeque<u64>,
    in_pitch: PitchReadout,
    out_pitch: PitchReadout,
}

impl App {
    fn new(engine: Engine) -> Self {
        let rate = engine.sample_rate;
        Self {
            engine,
            theme: Theme::detect(),
            selected: 0,
            quit: false,
            in_history: VecDeque::from(vec![0.0; FFT_SIZE]),
            out_history: VecDeque::from(vec![0.0; FFT_SIZE]),
            frame_in: Vec::new(),
            frame_out: Vec::new(),
            spectrum: Spectrum::new(rate),
            in_bands: vec![SPECTRUM_FLOOR_DB; SPECTRUM_BANDS],
            out_bands: vec![SPECTRUM_FLOOR_DB; SPECTRUM_BANDS],
            in_db: METER_FLOOR_DB,
            out_db: METER_FLOOR_DB,
            out_level_history: VecDeque::from(vec![0; LEVEL_HISTORY]),
            in_pitch: PitchReadout::new(),
            out_pitch: PitchReadout::new(),
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut next_frame = Instant::now();
        while !self.quit {
            self.analyze();
            terminal.draw(|f| self.draw(f))?;

            next_frame += FRAME;
            // Handle every key that arrives before the next frame is due.
            loop {
                let timeout = next_frame.saturating_duration_since(Instant::now());
                if !event::poll(timeout)? {
                    break;
                }
                if let Event::Key(key) = event::read()? {
                    self.on_key(key);
                }
                if self.quit {
                    break;
                }
            }
            // If we fell behind (e.g. the terminal was suspended), don't try to catch up.
            next_frame = next_frame.max(Instant::now());
        }
        Ok(())
    }

    fn params(&self) -> &Params {
        &self.engine.params
    }

    fn on_key(&mut self, key: KeyEvent) {
        let held = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
        let press = key.kind == KeyEventKind::Press;
        if !held {
            return;
        }
        let knob = Knob::ALL[self.selected];
        let fine = key.modifiers.contains(KeyModifiers::SHIFT);
        let step = if fine { knob.spec().fine_step } else { knob.spec().step };

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc if press => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + Knob::ALL.len() - 1) % Knob::ALL.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.selected = (self.selected + 1) % Knob::ALL.len();
            }
            KeyCode::Left | KeyCode::Char('h') => self.params().nudge(knob, -step),
            KeyCode::Right | KeyCode::Char('l') => self.params().nudge(knob, step),
            KeyCode::Char('H') => self.params().nudge(knob, -knob.spec().fine_step),
            KeyCode::Char('L') => self.params().nudge(knob, knob.spec().fine_step),
            KeyCode::Char('+') | KeyCode::Char('=') => self.params().nudge(Knob::Pitch, PITCH_HOTKEY_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.params().nudge(Knob::Pitch, -PITCH_HOTKEY_STEP),
            KeyCode::Char('r') if press => self.params().set(knob, knob.spec().default),
            KeyCode::Char('R') if press => {
                for k in Knob::ALL {
                    self.params().set(k, k.spec().default);
                }
            }
            KeyCode::Char(' ') if press => Params::toggle(&self.engine.params.bypass),
            KeyCode::Char('m') if press => Params::toggle(&self.engine.params.mute),
            KeyCode::Char('a') if press => Params::toggle(&self.engine.params.psola),
            KeyCode::Char(c @ '1'..='9') if press => {
                if let Some(preset) = Preset::ALL.get(c as usize - '1' as usize) {
                    preset.apply(self.params());
                }
            }
            _ => {}
        }
    }

    /// Pulls new audio from the engine and updates every derived display value.
    fn analyze(&mut self) {
        self.frame_in.clear();
        self.frame_out.clear();
        for (x, y) in self.engine.viz.pop_iter() {
            self.frame_in.push(x);
            self.frame_out.push(y);
        }
        push_history(&mut self.in_history, &self.frame_in, FFT_SIZE);
        push_history(&mut self.out_history, &self.frame_out, FFT_SIZE);

        if !self.frame_in.is_empty() {
            self.in_db = analysis::to_db(analysis::rms(&self.frame_in)).max(METER_FLOOR_DB);
            self.out_db = analysis::to_db(analysis::rms(&self.frame_out)).max(METER_FLOOR_DB);
        }
        self.out_level_history.pop_front();
        self.out_level_history.push_back((self.out_db - METER_FLOOR_DB) as u64);

        let rate = self.engine.sample_rate;
        let input = self.in_history.make_contiguous();
        let new_in = self.spectrum.bands(input, SPECTRUM_LO_HZ, SPECTRUM_HI_HZ, SPECTRUM_BANDS);
        self.in_pitch.update(analysis::detect_pitch(input, rate));
        let output = self.out_history.make_contiguous();
        let new_out = self.spectrum.bands(output, SPECTRUM_LO_HZ, SPECTRUM_HI_HZ, SPECTRUM_BANDS);
        self.out_pitch.update(analysis::detect_pitch(output, rate));

        decay_into(&mut self.in_bands, &new_in);
        decay_into(&mut self.out_bands, &new_out);
    }

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            f.render_widget(
                Paragraph::new(format!(
                    "Terminal too small for deep voice (needs {MIN_WIDTH}x{MIN_HEIGHT}).\nResize, or press q to quit. Audio is still running."
                )),
                area,
            );
            return;
        }

        let [header, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(area);
        self.draw_header(f, header);
        self.draw_footer(f, footer);

        // Wide: controls on the left, visuals on the right. Narrow: stack and drop the scope.
        if body.width >= 80 {
            let [left, right] = Layout::horizontal([Constraint::Length(40), Constraint::Min(0)]).areas(body);
            let [controls, levels, health] = Layout::vertical([
                Constraint::Length(controls_height(body.height, 6 + 5)),
                Constraint::Min(6),
                Constraint::Length(5),
            ])
            .areas(left);
            let [spectrum, scope] =
                Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(right);
            self.draw_controls(f, controls);
            self.draw_levels(f, levels);
            self.draw_health(f, health);
            self.draw_spectrum(f, spectrum);
            self.draw_scope(f, scope);
        } else {
            let [controls, levels, spectrum] = Layout::vertical([
                Constraint::Length(controls_height(body.height, 8 + 4)),
                Constraint::Length(8),
                Constraint::Min(0),
            ])
            .areas(body);
            self.draw_controls(f, controls);
            self.draw_levels(f, levels);
            self.draw_spectrum(f, spectrum);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let p = self.params();
        let (state, style) = if p.mute.load(Ordering::Relaxed) {
            ("✕ MUTED", t.bad)
        } else if p.bypass.load(Ordering::Relaxed) {
            ("◌ BYPASS", t.warn)
        } else {
            ("● LIVE", t.good)
        };
        let e = &self.engine;
        let left = Line::from(vec![
            Span::styled(" deep voice ", t.accent.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    " {} → {} · {:.1} kHz · {}",
                    e.input_name,
                    e.output_name,
                    e.sample_rate as f32 / 1000.0,
                    p.engine_name()
                ),
                t.muted,
            ),
        ]);
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(11)]).areas(area);
        f.render_widget(left, l);
        f.render_widget(Line::styled(format!("{state} "), style.add_modifier(Modifier::BOLD)).right_aligned(), r);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        // (key, action, priority): lowest priority is dropped first when the footer won't fit.
        let mut keys = vec![
            ("↑↓", "select", 6),
            ("←→", "adjust", 6),
            ("⇧", "fine", 1),
            ("+/-", "pitch", 5),
            ("r/R", "reset", 2),
            ("1-3", "preset", 3),
            ("a", "engine", 2),
            ("space", "bypass", 3),
            ("m", "mute", 4),
            ("q", "quit", 7),
        ];
        let width = |keys: &[(&str, &str, u8)]| -> usize {
            keys.iter().map(|(k, d, _)| k.chars().count() + d.len() + 3).sum()
        };
        while width(&keys) > area.width as usize && keys.len() > 1 {
            let lowest = (0..keys.len()).min_by_key(|&i| keys[i].2).unwrap_or(0);
            keys.remove(lowest);
        }
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, d, _)| [Span::styled(format!(" {k}"), t.accent), Span::styled(format!(" {d} "), t.muted)])
            .collect();
        f.render_widget(Line::from(spans), area);
    }

    fn draw_controls(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let block = Block::bordered().border_type(BorderType::Rounded).title(" Controls ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        // label(7) + value(9) + gaps leave the rest for the slider.
        let slider_width = inner.width.saturating_sub(2 + 7 + 1 + 1 + 9) as usize;
        let mut lines: Vec<Line> = Knob::ALL
            .iter()
            .enumerate()
            .map(|(i, &knob)| {
                let spec = knob.spec();
                let v = self.params().get(knob);
                let selected = i == self.selected;
                let style = if selected { t.selected } else { Style::default() };
                let pos = (v - spec.min) / (spec.max - spec.min);
                Line::from(vec![
                    Span::styled(if selected { "▸ " } else { "  " }, style),
                    Span::styled(format!("{:<7}", spec.label), style),
                    Span::raw(" "),
                    Span::styled(slider(pos, slider_width), if selected { t.accent } else { t.muted }),
                    Span::raw(" "),
                    Span::styled(format!("{:>9}", knob.format(v)), style),
                ])
            })
            .collect();
        if inner.height as usize >= Knob::ALL.len() + 2 {
            let knob = Knob::ALL[self.selected];
            lines.push(Line::default());
            lines.push(Line::styled(format!("  {}", knob.spec().help), t.muted));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_levels(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let block = Block::bordered().border_type(BorderType::Rounded).title(" Levels ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

        for (row, label, db, style) in [(rows[0], "mic", self.in_db, t.input), (rows[1], "out", self.out_db, t.accent)]
        {
            let ratio = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0) as f64;
            let fill = if db > -3.0 {
                t.bad
            } else if db > -12.0 {
                t.warn
            } else {
                style
            };
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio)
                    .label(Span::styled(format!("{label} {db:>4.0} dB "), style))
                    .filled_symbol("█")
                    .unfilled_symbol("░")
                    .filled_style(fill)
                    .unfilled_style(t.muted),
                row,
            );
        }

        let fmt_pitch = |p: &PitchReadout| match p.hz {
            Some(hz) => format!("{hz:>4.0} Hz {:<3}", analysis::note_name(hz)),
            None => format!("{:>4} Hz {:<3}", "—", ""),
        };
        f.render_widget(
            Line::from(vec![
                Span::styled("voice ", t.muted),
                Span::styled(fmt_pitch(&self.in_pitch), t.input),
                Span::styled(" → ", t.muted),
                Span::styled(fmt_pitch(&self.out_pitch), t.accent),
            ]),
            rows[2],
        );

        let gate_db = self.params().get(Knob::Gate);
        let (gate, gate_style) = if gate_db <= GATE_OFF_DB {
            ("off", t.muted)
        } else if self.engine.stats.gate_open.load(Ordering::Relaxed) {
            ("open", t.good)
        } else {
            ("closed", t.warn)
        };
        f.render_widget(
            Line::from(vec![Span::styled("gate  ", t.muted), Span::styled(gate, gate_style)]),
            rows[3],
        );

        let width = rows[4].width as usize;
        let history: Vec<u64> = self.out_level_history.iter().rev().take(width).rev().copied().collect();
        f.render_widget(
            Sparkline::default().data(&history).max(-METER_FLOOR_DB as u64).style(t.accent),
            rows[4],
        );
    }

    fn draw_health(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let [dropped, underruns, resyncs, overloads] = self.engine.stats.glitches();
        let total = dropped + underruns + resyncs + overloads;
        let block = Block::bordered().border_type(BorderType::Rounded).title(" Health ");
        let style = if total > 0 { t.warn } else { t.muted };
        let counts = [
            Line::styled(format!("dropped {dropped:<6} underrun {underruns}"), style),
            Line::styled(format!("resync  {resyncs:<6} overload {overloads}"), style),
        ];
        let error = match self.engine.stats.last_error.lock().ok().and_then(|e| e.clone()) {
            Some(e) => Line::styled(e, t.bad),
            None => Line::styled("no stream errors", t.muted),
        };
        let lines: Vec<Line> = counts.into_iter().chain([error]).collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_spectrum(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let to_points = |bands: &[f32]| -> Vec<(f64, f64)> {
            bands.iter().enumerate().map(|(i, &db)| (i as f64, db as f64)).collect()
        };
        let in_points = to_points(&self.in_bands);
        let out_points = to_points(&self.out_bands);
        let in_marker = pitch_marker(self.in_pitch.hz);
        let out_marker = pitch_marker(self.out_pitch.hz);

        let mut datasets = vec![
            Dataset::default()
                .name("mic")
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.input)
                .data(&in_points),
            Dataset::default()
                .name("out")
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.accent)
                .data(&out_points),
        ];
        for (marker, style) in [(&in_marker, t.input), (&out_marker, t.accent)] {
            if !marker.is_empty() {
                datasets.push(Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).style(style).data(marker));
            }
        }

        let chart = Chart::new(datasets)
            .block(Block::bordered().border_type(BorderType::Rounded).title(" Spectrum · Hz "))
            .x_axis(
                Axis::default()
                    .bounds([0.0, SPECTRUM_BANDS as f64 - 1.0])
                    .labels(["63", "200", "630", "2k", "6.3k"])
                    .style(t.muted),
            )
            .y_axis(
                Axis::default()
                    .bounds([SPECTRUM_FLOOR_DB as f64, 0.0])
                    .labels(["-90", "-45", "0 dB"])
                    .style(t.muted),
            );
        f.render_widget(chart, area);
    }

    fn draw_scope(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let to_points = |history: &VecDeque<f32>| -> (Vec<(f64, f64)>, f32) {
            let start = trigger_point(history, SCOPE_SAMPLES);
            let points: Vec<(f64, f64)> = history
                .range(start..start + SCOPE_SAMPLES)
                .enumerate()
                .map(|(i, &y)| (i as f64, y as f64))
                .collect();
            let peak = points.iter().fold(0.0f64, |m, p| m.max(p.1.abs())) as f32;
            (points, peak)
        };
        let (in_points, in_peak) = to_points(&self.in_history);
        let (out_points, out_peak) = to_points(&self.out_history);
        // Auto-scale so quiet speech is still visible, with a floor so silence stays flat.
        let scale = (in_peak.max(out_peak) * 1.2).max(0.05) as f64;
        let ms = SCOPE_SAMPLES as f32 * 1000.0 / self.engine.sample_rate as f32;

        let chart = Chart::new(vec![
            Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).style(t.input).data(&in_points),
            Dataset::default().marker(Marker::Braille).graph_type(GraphType::Line).style(t.accent).data(&out_points),
        ])
        .block(Block::bordered().border_type(BorderType::Rounded).title(format!(" Scope · {ms:.0} ms ")))
        .x_axis(Axis::default().bounds([0.0, SCOPE_SAMPLES as f64]))
        .y_axis(Axis::default().bounds([-scale, scale]));
        f.render_widget(chart, area);
    }
}

/// Rows for the controls panel: every knob plus borders, and the help line when the other
/// panels (`others` rows) still fit beside it.
fn controls_height(body_height: u16, others: u16) -> u16 {
    let knobs = Knob::ALL.len() as u16;
    if body_height >= knobs + 4 + others { knobs + 4 } else { knobs + 2 }
}

fn push_history(history: &mut VecDeque<f32>, samples: &[f32], len: usize) {
    history.extend(samples);
    let excess = history.len().saturating_sub(len);
    history.drain(..excess);
}

/// Bars jump up immediately and fall back slowly, like a hardware analyzer.
fn decay_into(display: &mut [f32], fresh: &[f32]) {
    for (d, &n) in display.iter_mut().zip(fresh) {
        *d = n.max(*d - SPECTRUM_DECAY_DB).max(SPECTRUM_FLOOR_DB);
    }
}

/// Finds a rising zero crossing so the scope trace holds still on periodic sounds.
fn trigger_point(history: &VecDeque<f32>, window: usize) -> usize {
    let latest = history.len() - window;
    let earliest = latest.saturating_sub(window);
    (earliest + 1..=latest)
        .rev()
        .find(|&i| history[i - 1] < 0.0 && history[i] >= 0.0)
        .unwrap_or(latest)
}

/// A vertical line on the spectrum at the detected fundamental.
fn pitch_marker(hz: Option<f32>) -> Vec<(f64, f64)> {
    let Some(hz) = hz.filter(|&hz| (SPECTRUM_LO_HZ..SPECTRUM_HI_HZ).contains(&hz)) else {
        return Vec::new();
    };
    let x = (hz / SPECTRUM_LO_HZ).ln() / (SPECTRUM_HI_HZ / SPECTRUM_LO_HZ).ln() * SPECTRUM_BANDS as f32;
    vec![(x as f64, SPECTRUM_FLOOR_DB as f64), (x as f64, 0.0)]
}

fn slider(pos: f32, width: usize) -> String {
    if width < 2 {
        return String::new();
    }
    let knob = ((pos.clamp(0.0, 1.0) * (width - 1) as f32).round()) as usize;
    (0..width).map(|i| if i == knob { '●' } else if i < knob { '━' } else { '─' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_positions() {
        assert_eq!(slider(0.0, 5), "●────");
        assert_eq!(slider(0.5, 5), "━━●──");
        assert_eq!(slider(1.0, 5), "━━━━●");
    }

    #[test]
    fn trigger_finds_rising_crossing() {
        let mut h: VecDeque<f32> = (0..4096).map(|i| (i as f32 * 0.05).sin()).collect();
        h.make_contiguous();
        let i = trigger_point(&h, 1024);
        assert!(h[i - 1] < 0.0 && h[i] >= 0.0);
        assert!(i + 1024 <= h.len());
    }

    #[test]
    fn pitch_hold_and_smoothing() {
        let mut p = PitchReadout::new();
        p.update(Some(100.0));
        p.update(Some(105.0));
        assert!((p.hz.unwrap() - 102.0).abs() < 0.01);
        p.update(Some(200.0));
        assert_eq!(p.hz, Some(200.0));
        for _ in 0..PITCH_HOLD_FRAMES {
            p.update(None);
        }
        assert_eq!(p.hz, Some(200.0));
        p.update(None);
        assert_eq!(p.hz, None);
    }
}
