//! Terminal drawing: truecolor ANSI, full-frame redraws in live mode, and an
//! asciinema v2 cast of everything written (for rendering a video / PNG).

use std::io::Write;
use std::time::{Duration, Instant};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";

pub fn rgb(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}
pub fn white() -> String {
    rgb(235, 235, 240)
}
pub fn grey() -> String {
    rgb(115, 115, 125)
}
pub fn green() -> String {
    rgb(120, 220, 140)
}
pub fn red() -> String {
    rgb(255, 120, 110)
}
pub fn yellow() -> String {
    rgb(240, 198, 116)
}
pub fn blue() -> String {
    rgb(120, 170, 255)
}

/// Visible width (ANSI escapes stripped, one column per char).
pub fn vis(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            n += 1;
        }
    }
    n
}

pub fn pad(s: &str, w: usize) -> String {
    let v = vis(s);
    if v >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - v)) }
}

pub fn rpad(s: &str, w: usize) -> String {
    let v = vis(s);
    if v >= w { s.to_string() } else { format!("{}{s}", " ".repeat(w - v)) }
}

pub fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(w.saturating_sub(1)).collect::<String>())
    }
}

pub fn ms(us: f64) -> String {
    let m = us / 1000.0;
    if !m.is_finite() {
        "–".into()
    } else if m < 0.1 {
        format!("{:.0} µs", m * 1000.0)
    } else if m < 1.0 {
        format!("{m:.2} ms")
    } else if m < 10.0 {
        format!("{m:.1} ms")
    } else if m < 1000.0 {
        format!("{m:.0} ms")
    } else {
        format!("{:.1} s", m / 1000.0)
    }
}

pub fn spark(v: &[f64], n: usize) -> String {
    const B: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail = &v[v.len().saturating_sub(n)..];
    let top = tail.iter().cloned().fold(1e-9, f64::max);
    tail.iter().map(|&t| B[((t / top) * 7.0).round().clamp(0.0, 7.0) as usize]).collect()
}

pub struct Term {
    pub live: bool,
    pub color: bool,
    pub w: usize,
    pub h: usize,
    cast: Option<(std::io::BufWriter<std::fs::File>, Instant)>,
    last: Instant,
    pub fps: f64,
}

impl Term {
    pub fn new(live: bool, color: bool, w: usize, h: usize, cast: Option<&str>) -> Term {
        let cast = cast.map(|p| {
            let mut f = std::io::BufWriter::new(std::fs::File::create(p).expect("cast file"));
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            writeln!(
                f,
                "{}",
                serde_json::json!({"version": 2, "width": w, "height": h, "timestamp": ts, "env": {"TERM": "xterm-256color"}})
            )
            .unwrap();
            (f, Instant::now())
        });
        Term {
            live,
            color,
            w,
            h,
            cast,
            last: Instant::now() - Duration::from_secs(1),
            fps: 30.0,
        }
    }

    pub fn out(&mut self, s: &str) {
        let shown = if self.color { s.to_string() } else { strip(s) };
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(shown.as_bytes());
        let _ = o.flush();
        if let Some((f, t0)) = self.cast.as_mut() {
            let _ = writeln!(f, "{}", serde_json::json!([t0.elapsed().as_secs_f64(), "o", s]));
        }
    }

    pub fn marker(&mut self, label: &str) {
        if let Some((f, t0)) = self.cast.as_mut() {
            let _ = writeln!(f, "{}", serde_json::json!([t0.elapsed().as_secs_f64(), "m", label]));
            let _ = f.flush();
        }
    }

    /// True when the next live frame is due.
    pub fn due(&mut self) -> bool {
        if self.last.elapsed().as_secs_f64() >= 1.0 / self.fps {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }

    pub fn frame(&mut self, lines: &[String]) {
        let mut s = String::from("\x1b[H");
        for l in lines.iter().take(self.h.saturating_sub(1)) {
            s.push_str(l);
            s.push_str("\x1b[K\r\n");
        }
        s.push_str("\x1b[J");
        self.out(&s);
    }

    pub fn start(&mut self) {
        if self.live {
            self.out("\x1b[?25l\x1b[H\x1b[2J");
        }
    }

    pub fn finish(&mut self) {
        if self.live {
            self.out("\x1b[?25h");
        }
        if let Some((f, _)) = self.cast.as_mut() {
            let _ = f.flush();
        }
    }
}

pub fn strip(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            o.push(c);
        }
    }
    o
}
