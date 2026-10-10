//! Query language.
//!
//! Plain words are fuzzy tokens (all must match). Modifiers: `'x` substring,
//! `^x` prefix, `x$` suffix, `^x$` / `=x` exact name, `!x` / `-x` exclude
//! (substring). A word containing `/` is split into tokens. Filters:
//!
//! * `ext:pdf,md`
//! * `type:file|dir|link|app|image|video|audio|doc|code|archive|font`
//! * `size:>5mb`, `size:<1k`, `size:1mb..10mb`, `size:5mb` (= at least 5 MB);
//!   units b/k/kb/m/mb/g/gb/t/tb (×1000) and kib/mib/gib/tib (×1024)
//! * `mtime:<7d` (modified within 7 days), `mtime:>1y` (older than a year),
//!   `mtime:3d` (= within 3 days), `mtime:2d..7d`, `mtime:2026-01-15`
//!   (that local day), `mtime:>2026-01-01`, `mtime:2026-01-01..2026-02-01`,
//!   `mtime:today`, `mtime:yesterday`; units s/m(in)/h/d/w/mo/y.
//!   All arithmetic saturates – `mtime:>500y` cannot wrap around.
//! * `in:/abs/path` or `in:~/dir` – restrict to a folder subtree
//! * `limit:N`

use crate::matcher::{Pattern, fold_all};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Mode {
    Fuzzy,
    Substring,
    Prefix,
    Suffix,
    Exact,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub mode: Mode,
    pub pat: Pattern,
}

impl Token {
    pub fn text(&self) -> &[u8] {
        &self.pat.text
    }

    /// Score against a folded name (`orig` = original case, same length).
    #[inline]
    pub fn score(&self, name: &[u8], orig: &[u8]) -> Option<i32> {
        self.score_sig(name, orig, crate::matcher::bigram_sig(name))
    }

    #[inline]
    pub fn score_sig(&self, name: &[u8], orig: &[u8], sig: crate::matcher::Sig) -> Option<i32> {
        self.score_opt(name, orig, sig, true)
    }

    /// `typos = false` scores clean (non-typo) matches only.
    #[inline]
    pub fn score_opt(&self, name: &[u8], orig: &[u8], sig: crate::matcher::Sig, typos: bool) -> Option<i32> {
        use crate::matcher::*;
        let t = &self.pat.text[..];
        match self.mode {
            Mode::Fuzzy if typos => self.pat.score_sig(name, orig, sig),
            Mode::Fuzzy => self.pat.clean_score(name, orig),
            Mode::Exact => (name == t).then_some(EXACT),
            Mode::Prefix => {
                if name == t {
                    Some(EXACT)
                } else if name.starts_with(t) {
                    Some(PREFIX - prefix_pen(name.len() - t.len()))
                } else {
                    None
                }
            }
            Mode::Suffix => name
                .ends_with(t)
                .then(|| if name == t { EXACT } else { WORD - ((name.len() - t.len()) as i32).min(40) }),
            Mode::Substring => {
                if name == t {
                    Some(EXACT)
                } else {
                    self.pat.find(name).map(|p| {
                        if p == 0 {
                            PREFIX - prefix_pen(name.len() - t.len())
                        } else {
                            SUBSTR - ((name.len() - t.len()) as i32).min(40) - (p as i32).min(20)
                        }
                    })
                }
            }
        }
    }
}

pub const TYPES: &[(&str, &[&str])] = &[
    (
        "image",
        &[
            "png", "jpg", "jpeg", "gif", "heic", "heif", "webp", "tiff", "tif", "bmp", "svg", "raw", "cr2", "cr3", "nef", "arw", "dng", "psd", "ico", "icns",
            "avif", "jxl",
        ],
    ),
    (
        "video",
        &["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpg", "mpeg", "3gp", "hevc"],
    ),
    (
        "audio",
        &[
            "mp3", "m4a", "aac", "wav", "flac", "aiff", "aif", "ogg", "opus", "alac", "caf", "mid", "midi", "m4r",
        ],
    ),
    (
        "doc",
        &[
            "pdf", "doc", "docx", "pages", "txt", "md", "rtf", "odt", "key", "ppt", "pptx", "numbers", "xls", "xlsx", "csv", "epub", "tex",
        ],
    ),
    (
        "code",
        &[
            "rs", "c", "h", "cc", "cpp", "hpp", "m", "mm", "swift", "go", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "java", "kt", "rb", "php", "cs", "sh",
            "zsh", "bash", "fish", "lua", "sql", "html", "css", "scss", "json", "yaml", "yml", "toml", "xml", "vue", "svelte", "zig", "nim", "hs", "ml", "ex",
            "exs", "erl", "clj", "dart", "r", "jl", "metal", "glsl", "wgsl", "proto", "graphql", "nix",
        ],
    ),
    (
        "archive",
        &["zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "dmg", "pkg", "iso", "zst", "lz4", "xip"],
    ),
    ("font", &["ttf", "otf", "woff", "woff2", "ttc", "dfont"]),
];

/// Entry kinds a query accepts (bit per `walk::KIND_*`).
pub const KINDS_ALL: u8 = 0b1111;

#[derive(Clone, Debug)]
pub struct Query {
    pub tokens: Vec<Token>,
    /// Excluded substrings (folded).
    pub negs: Vec<Vec<u8>>,
    /// Folded extensions without the dot; empty = any.
    pub exts: Vec<Vec<u8>>,
    pub kinds: u8,
    /// Inclusive byte range.
    pub size: (u64, u64),
    /// Inclusive mtime range (Unix seconds).
    pub mtime: (i64, i64),
    pub scope: Option<Vec<u8>>,
    pub limit: usize,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            tokens: Vec::new(),
            negs: Vec::new(),
            exts: Vec::new(),
            kinds: KINDS_ALL,
            size: (0, u64::MAX),
            mtime: (i64::MIN, i64::MAX),
            scope: None,
            limit: 50,
        }
    }
}

/// Wall clock + local UTC offset, injectable for tests.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub now: i64,
    pub utc_offset: i64,
}

impl Clock {
    pub fn system() -> Clock {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Clock {
            now,
            utc_offset: local_offset(now),
        }
    }
}

#[cfg(unix)]
fn local_offset(now: i64) -> i64 {
    let t = now as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}
#[cfg(not(unix))]
fn local_offset(_: i64) -> i64 {
    0
}

impl Query {
    pub fn parse(s: &str) -> Result<Query, String> {
        Self::parse_with(s, Clock::system(), std::env::var("HOME").ok().as_deref())
    }

    pub fn parse_with(s: &str, clock: Clock, home: Option<&str>) -> Result<Query, String> {
        let mut q = Query::default();
        for word in s.split_whitespace() {
            if let Some((k, v)) = word.split_once(':')
                && !k.is_empty()
                && q.filter(&k.to_ascii_lowercase(), v, clock, home)?
            {
                continue;
            }
            q.push_word(word);
        }
        Ok(q)
    }

    fn push_word(&mut self, w: &str) {
        if let Some(rest) = w.strip_prefix('!').or_else(|| w.strip_prefix('-').filter(|r| !r.is_empty())) {
            if !rest.is_empty() {
                self.negs.push(fold_all(rest.as_bytes()));
            }
            return;
        }
        for piece in w.split('/').filter(|p| !p.is_empty()) {
            self.push_token(piece);
        }
    }

    pub fn push_token(&mut self, w: &str) {
        let (mode, text) = if let Some(r) = w.strip_prefix('=') {
            (Mode::Exact, r)
        } else if let Some(r) = w.strip_prefix('^') {
            match r.strip_suffix('$') {
                Some(r2) => (Mode::Exact, r2),
                None => (Mode::Prefix, r),
            }
        } else if let Some(r) = w.strip_prefix('\'') {
            (Mode::Substring, r)
        } else if let Some(r) = w.strip_suffix('$') {
            (Mode::Suffix, r)
        } else {
            (Mode::Fuzzy, w)
        };
        if text.is_empty() {
            return;
        }
        let folded = fold_all(text.as_bytes());
        self.tokens.push(Token {
            mode,
            pat: Pattern::new(&folded, mode == Mode::Fuzzy),
        });
    }

    /// Returns Ok(false) if `k` is not a filter (the word is then a token).
    fn filter(&mut self, k: &str, v: &str, clock: Clock, home: Option<&str>) -> Result<bool, String> {
        use crate::walk::*;
        match k {
            "ext" => {
                self.exts
                    .extend(v.split(',').map(|e| fold_all(e.trim_start_matches('.').as_bytes())).filter(|e| !e.is_empty()));
            }
            "type" | "kind" => {
                let v = v.to_ascii_lowercase();
                match v.as_str() {
                    "file" | "f" => self.kinds = 1 << KIND_FILE,
                    "dir" | "d" | "folder" => self.kinds = 1 << KIND_DIR,
                    "link" | "symlink" | "l" => self.kinds = 1 << KIND_LINK,
                    "app" => {
                        self.kinds = (1 << KIND_DIR) | (1 << KIND_LINK);
                        self.exts.push(b"app".to_vec());
                    }
                    g => {
                        let Some((_, exts)) = TYPES.iter().find(|(n, _)| *n == g) else {
                            return Err(format!("unknown type: {v}"));
                        };
                        self.kinds = 1 << KIND_FILE;
                        self.exts.extend(exts.iter().map(|e| e.as_bytes().to_vec()));
                    }
                }
            }
            "size" => self.size = parse_size_range(v)?,
            "mtime" | "modified" | "dm" => self.mtime = parse_mtime(v, clock)?,
            "in" | "under" => {
                let mut p = v.to_string();
                if let Some(rest) = p.strip_prefix('~') {
                    p = format!("{}{}", home.ok_or("in:~ needs $HOME")?, rest);
                }
                while p.len() > 1 && p.ends_with('/') {
                    p.pop();
                }
                self.scope = Some(p.into_bytes());
            }
            "limit" => self.limit = v.parse::<usize>().map_err(|_| format!("bad limit: {v}"))?.clamp(1, 100_000),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn positive(&self) -> &[Token] {
        &self.tokens
    }

    /// True if any name-level predicate exists.
    pub fn is_empty_name_query(&self) -> bool {
        self.tokens.is_empty() && self.negs.is_empty() && self.exts.is_empty()
    }
}

// ---------- size ----------

fn split_range(v: &str) -> (Option<&str>, Option<&str>, &'static str) {
    if let Some((a, b)) = v.split_once("..") {
        return (Some(a).filter(|s| !s.is_empty()), Some(b).filter(|s| !s.is_empty()), "..");
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(r) = v.strip_prefix(op) {
            return (Some(r), None, op);
        }
    }
    (Some(v), None, "")
}

pub fn parse_size(v: &str) -> Result<u64, String> {
    let v = v.trim().to_ascii_lowercase();
    let split = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let mult: f64 = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1048576.0,
        "gib" => 1073741824.0,
        "tib" => 1099511627776.0,
        _ => return Err(format!("bad size unit: {unit}")),
    };
    let n: f64 = num.parse().map_err(|_| format!("bad size: {v}"))?;
    let bytes = n * mult;
    Ok(if bytes >= u64::MAX as f64 { u64::MAX } else { bytes.max(0.0) as u64 })
}

pub fn parse_size_range(v: &str) -> Result<(u64, u64), String> {
    let (a, b, op) = split_range(v);
    Ok(match op {
        ".." => (a.map(parse_size).transpose()?.unwrap_or(0), b.map(parse_size).transpose()?.unwrap_or(u64::MAX)),
        ">" => (parse_size(a.unwrap_or(""))?.saturating_add(1), u64::MAX),
        ">=" | "" => (parse_size(a.unwrap_or(""))?, u64::MAX),
        "<" => (0, parse_size(a.unwrap_or(""))?.saturating_sub(1)),
        "<=" => (0, parse_size(a.unwrap_or(""))?),
        "=" => {
            let s = parse_size(a.unwrap_or(""))?;
            (s, s)
        }
        _ => unreachable!(),
    })
}

// ---------- mtime ----------

/// Duration in seconds; saturates instead of overflowing.
pub fn parse_duration(v: &str) -> Result<i64, String> {
    let v = v.trim().to_ascii_lowercase();
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let (num, unit) = v.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("bad duration: {v}"))?;
    let mult: u64 = match unit {
        "s" | "sec" => 1,
        "m" | "min" => 60,
        "h" | "hr" => 3600,
        "" | "d" | "day" | "days" => 86_400,
        "w" | "wk" => 7 * 86_400,
        "mo" | "mon" => 30 * 86_400,
        "y" | "yr" => 365 * 86_400,
        _ => return Err(format!("bad duration unit: {unit}")),
    };
    Ok(n.saturating_mul(mult).min(i64::MAX as u64) as i64)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` → [start, end) of that local day, in Unix seconds.
fn parse_date(v: &str, clock: Clock) -> Option<(i64, i64)> {
    let mut it = v.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next().map_or(Some(1), |s| s.parse().ok())?;
    let d: i64 = it.next().map_or(Some(1), |s| s.parse().ok())?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1..=9999).contains(&y) {
        return None;
    }
    let start = days_from_civil(y, m, d) * 86_400 - clock.utc_offset;
    Some((start, start + 86_400))
}

fn is_date(v: &str) -> bool {
    v.len() >= 8 && v.as_bytes()[4] == b'-' && v[..4].bytes().all(|b| b.is_ascii_digit())
}

fn day_of(clock: Clock, days_back: i64) -> (i64, i64) {
    let local = clock.now + clock.utc_offset;
    let start = local.div_euclid(86_400) * 86_400 - clock.utc_offset - days_back * 86_400;
    (start, start + 86_400)
}

/// Inclusive mtime range for an `mtime:` value.
pub fn parse_mtime(v: &str, clock: Clock) -> Result<(i64, i64), String> {
    let lv = v.to_ascii_lowercase();
    match lv.as_str() {
        "today" => {
            let (a, b) = day_of(clock, 0);
            return Ok((a, b - 1));
        }
        "yesterday" => {
            let (a, b) = day_of(clock, 1);
            return Ok((a, b - 1));
        }
        _ => {}
    }
    let (a, b, op) = split_range(&lv);
    let date = |s: &str| parse_date(s, clock).ok_or_else(|| format!("bad date: {s}"));
    // Age → timestamp: now - age, saturating.
    let ago = |s: &str| parse_duration(s).map(|d| clock.now.saturating_sub(d));
    let any = |s: Option<&str>| s.is_some_and(is_date);
    if any(a) || any(b) {
        return Ok(match op {
            ".." => (
                a.map(date).transpose()?.map_or(i64::MIN, |r| r.0),
                b.map(date).transpose()?.map_or(i64::MAX, |r| r.1 - 1),
            ),
            ">" => (date(a.unwrap())?.1, i64::MAX),
            ">=" => (date(a.unwrap())?.0, i64::MAX),
            "<" => (i64::MIN, date(a.unwrap())?.0 - 1),
            "<=" => (i64::MIN, date(a.unwrap())?.1 - 1),
            "" | "=" => {
                let (s, e) = date(a.unwrap())?;
                (s, e - 1)
            }
            _ => unreachable!(),
        });
    }
    Ok(match op {
        // Ages: `<7d` / bare `7d` = modified within the last 7 days.
        "<" | "<=" | "" | "=" => (ago(a.unwrap_or(""))?, i64::MAX),
        ">" | ">=" => (i64::MIN, ago(a.unwrap_or(""))?),
        ".." => {
            // `2d..7d`: age between 2 and 7 days.
            let young = a.map(ago).transpose()?.unwrap_or(clock.now);
            let old = b.map(ago).transpose()?.unwrap_or(i64::MIN);
            (old.min(young), old.max(young))
        }
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const CLOCK: Clock = Clock {
        now: 1_790_000_000,
        utc_offset: 7 * 3600,
    };

    fn q(s: &str) -> Query {
        Query::parse_with(s, CLOCK, Some("/Users/me")).unwrap()
    }

    #[test]
    fn tokens_and_modes() {
        let x = q("foo ^bar baz$ 'qux =exact ^whole$ !neg -neg2 src/main");
        let modes: Vec<_> = x.tokens.iter().map(|t| (t.mode, String::from_utf8_lossy(t.text()).into_owned())).collect();
        assert_eq!(
            modes,
            vec![
                (Mode::Fuzzy, "foo".into()),
                (Mode::Prefix, "bar".into()),
                (Mode::Suffix, "baz".into()),
                (Mode::Substring, "qux".into()),
                (Mode::Exact, "exact".into()),
                (Mode::Exact, "whole".into()),
                (Mode::Fuzzy, "src".into()),
                (Mode::Fuzzy, "main".into())
            ]
        );
        assert_eq!(x.negs, vec![b"neg".to_vec(), b"neg2".to_vec()]);
    }

    #[test]
    fn unknown_filter_is_a_token() {
        let x = q("c:foo");
        assert_eq!(x.tokens.len(), 1);
        assert_eq!(x.tokens[0].text(), b"c:foo");
    }

    #[test]
    fn sizes() {
        assert_eq!(q("size:>5mb").size, (5_000_001, u64::MAX));
        assert_eq!(q("size:<1k").size, (0, 999));
        assert_eq!(q("size:1mib..2mib").size, (1 << 20, 2 << 20));
        assert_eq!(q("size:5mb").size, (5_000_000, u64::MAX));
        assert_eq!(q("size:>99999999tb").size.1, u64::MAX);
        assert!(Query::parse_with("size:5xb", CLOCK, None).is_err());
    }

    #[test]
    fn mtime_bare_is_a_window_not_a_second() {
        // fsearch bug: `mtime:3d` matched a 1-second window.
        let (lo, hi) = q("mtime:3d").mtime;
        assert_eq!(lo, CLOCK.now - 3 * 86_400);
        assert_eq!(hi, i64::MAX);
        assert_eq!(q("mtime:<3d").mtime, (lo, hi));
    }

    #[test]
    fn mtime_huge_ages_saturate() {
        // fsearch bug: `mtime:>140y` wrapped through a u32 cast.
        let (lo, hi) = q("mtime:>140y").mtime;
        assert_eq!(lo, i64::MIN);
        assert_eq!(hi, CLOCK.now - 140 * 365 * 86_400);
        assert!(hi < 0, "140 years ago is before 1970");
        let (_, hi) = q("mtime:>99999999999999y").mtime;
        assert!(hi < CLOCK.now);
        let (lo, _) = q("mtime:<99999999999999y").mtime;
        assert!(lo <= 0);
    }

    #[test]
    fn mtime_ranges_and_dates() {
        let (lo, hi) = q("mtime:2d..7d").mtime;
        assert_eq!((lo, hi), (CLOCK.now - 7 * 86_400, CLOCK.now - 2 * 86_400));
        // 2026-01-15 local (UTC+7) = 2026-01-14T17:00Z
        let (lo, hi) = q("mtime:2026-01-15").mtime;
        assert_eq!(lo, 1_768_410_000);
        assert_eq!(hi, lo + 86_399);
        let (lo, hi) = q("mtime:>2026-01-15").mtime;
        assert_eq!((lo, hi), (1_768_410_000 + 86_400, i64::MAX));
        let (lo, hi) = q("mtime:2026-01-01..2026-02-01").mtime;
        assert!(lo < hi);
        let (lo, hi) = q("mtime:today").mtime;
        assert!(lo <= CLOCK.now && CLOCK.now <= hi);
    }

    #[test]
    fn types_and_ext() {
        let x = q("type:image ext:.PDF");
        assert_eq!(x.kinds, 1 << crate::walk::KIND_FILE);
        assert!(x.exts.iter().any(|e| e == b"pdf"));
        assert!(x.exts.iter().any(|e| e == b"heic"));
        assert!(Query::parse_with("type:nope", CLOCK, None).is_err());
    }

    #[test]
    fn scope() {
        assert_eq!(q("in:~/Documents/ x").scope.as_deref(), Some(&b"/Users/me/Documents"[..]));
    }
}
