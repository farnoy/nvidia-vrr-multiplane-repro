//! Minimal atomic KMS repro: with VRR enabled, a primary plane flips on a fixed schedule
//! below maximum refresh while a secondary plane (overlay or cursor) updates every K frames.
//! Each frame records the commit time and flip-complete event timestamp. Linear dumb buffers,
//! no modeset, no GPU rendering.

use drm_sys::*;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

const USAGE: &str = "\
Usage: drm-plane-bench [options]
  --suite                run the 6-case ~42 s test suite and print a Markdown
                         report (recommended)
  --list                 list detected GPUs, active displays and VRR capability
                         (safe to run inside a graphical session)
  --device PATH          DRM device (default: auto-detect, preferring vrr_capable=1)
  --connector NAME       e.g. HDMI-A-1, DP-1 (default: first connected active CRTC)
  --vrr on|off|keep      VRR_ENABLED for the run (default on); restored at exit
  --pace-hz HZ           primary flip rate cap (default: 83% of refresh in --suite,
                         97.5% in single-run mode)
  --overlay-every K      update the secondary plane every K frames (default 8)
  --overlay-size WxH     overlay size (default 512x128)
  --overlay-pos X,Y      overlay position (default 0,0)
  --overlay-opaque       fully opaque overlay instead of an opaque 320x120 card on
                         transparent margins (pre-multiplied blending)
  --keep-planes          leave other planes on the CRTC (e.g. the cursor) as found
  --cursor-plane         update the cursor plane instead of an overlay plane
  --overlay-move         update the plane by moving it 16 px (CRTC_X) instead of
                         swapping its buffer, like cursor motion
  --exclusive            overlay-update frames commit only the overlay (no primary)
  --draw MODE            primary content: scene (default: visual aids), bar (32 px
                         moving vertical bar), hbar (300 px moving horizontal bar),
                         hbar-striped (hbar over 4 px vertical stripes)
  --pattern KIND         static pattern drawn once, then no primary writes: vstripes,
                         hstripes, checker, noise (random RGB) or solid
  --pattern-period N     stripe/checker period in px (default 4)
  --pattern-on N         px of foreground per period (default 3)
  --pattern-fg RRGGBB    foreground (default 30c030), --pattern-bg RRGGBB (default 101010)
  --pattern-rect X,Y,W,H region (default: the graph band at the bottom)
  --seconds S            run length (default: 7 per case in --suite, 30 single-run)
  --csv PATH             per-frame CSV output (single-run mode; default: none)

Run from a spare VT with no compositor holding DRM master (it needs SET_MASTER).
The active mode is kept; nothing is modeset. Ctrl-C stops early.";

const NONBLOCK: u32 = 0x200;
const TEST_ONLY: u32 = 0x100;
const FLIP_EVENT: u32 = 1;
const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const OBJ_PLANE: u32 = 0xeeeeeeee;
const OBJ_CRTC: u32 = 0xcccccccc;
const OBJ_CONNECTOR: u32 = 0xc0c0c0c0;
const FLIP_COMPLETE: u32 = 0x02;
const PLANE_PROPS: [&str; 10] = [
    "FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H",
];

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

fn ioctl<T>(fd: RawFd, nr: u8, data: &mut T) -> io::Result<()> {
    let request = (3u64 << 30) | ((size_of::<T>() as u64) << 16) | (100 << 8) | nr as u64;
    if unsafe { libc::ioctl(fd, request as _, data as *mut T) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn sleep_until(ns: u64) {
    let ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as _,
    };
    unsafe {
        libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut())
    };
}

fn c_name(chars: &[libc::c_char]) -> String {
    chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect()
}

/// (property name, property id, current value) of a KMS object.
fn properties(fd: RawFd, id: u32, kind: u32) -> io::Result<Vec<(String, u32, u64)>> {
    let mut q = drm_mode_obj_get_properties { obj_id: id, obj_type: kind, ..Default::default() };
    ioctl(fd, 0xb9, &mut q)?;
    let mut ids = vec![0u32; q.count_props as usize];
    let mut values = vec![0u64; q.count_props as usize];
    q.props_ptr = ids.as_mut_ptr() as u64;
    q.prop_values_ptr = values.as_mut_ptr() as u64;
    ioctl(fd, 0xb9, &mut q)?;
    ids.iter()
        .zip(&values)
        .take(q.count_props as usize)
        .map(|(&prop_id, &value)| {
            let mut p = drm_mode_get_property { prop_id, ..Default::default() };
            ioctl(fd, 0xaa, &mut p)?;
            Ok((c_name(&p.name), prop_id, value))
        })
        .collect()
}

fn find(props: &[(String, u32, u64)], name: &str) -> Option<(u32, u64)> {
    props.iter().find(|p| p.0 == name).map(|p| (p.1, p.2))
}

struct Plane {
    id: u32,
    props: Vec<(String, u32, u64)>,
}

impl Plane {
    fn prop(&self, name: &str) -> Result<u32> {
        Ok(find(&self.props, name).ok_or_else(|| format!("plane {} lacks {name}", self.id))?.0)
    }
}

struct Buffer {
    fb: u32,
    pixels: &'static mut [u32],
    stride: usize,
    width: u32,
    height: u32,
    /// Whether the red band is currently drawn; redrawn only when it changes because CPU
    /// writes to dumb buffers can be slow (uncached on some drivers).
    band: bool,
}

impl Buffer {
    fn new(fd: RawFd, width: u32, height: u32, format: u32, color: u32) -> Result<Self> {
        let mut dumb = drm_mode_create_dumb { width, height, bpp: 32, ..Default::default() };
        ioctl(fd, 0xb2, &mut dumb)?;
        let mut cmd = drm_mode_fb_cmd2 {
            width,
            height,
            pixel_format: format,
            handles: [dumb.handle, 0, 0, 0],
            pitches: [dumb.pitch, 0, 0, 0],
            ..Default::default()
        };
        ioctl(fd, 0xb8, &mut cmd)?;
        let mut map = drm_mode_map_dumb { handle: dumb.handle, ..Default::default() };
        ioctl(fd, 0xb3, &mut map)?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                dumb.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                map.offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        // Lives until process exit; the kernel releases buffers when the fd closes.
        let pixels = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u32>(), dumb.size as usize / 4) };
        pixels.fill(color);
        Ok(Self {
            fb: cmd.fb_id,
            pixels,
            stride: dumb.pitch as usize / 4,
            width,
            height,
            band: false,
        })
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        let (x, y) = (x.min(self.width), y.min(self.height));
        let (w, h) = (w.min(self.width - x) as usize, h.min(self.height - y) as usize);
        for row in y as usize..y as usize + h {
            self.pixels[row * self.stride + x as usize..][..w].fill(color);
        }
    }

    fn draw_pattern(&mut self, p: &Pattern, (x0, y0, w, h): (u32, u32, u32, u32)) {
        let (x0, y0) = (x0.min(self.width), y0.min(self.height));
        let (w, h) = (w.min(self.width - x0), h.min(self.height - y0));
        let period = p.period.max(1);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for y in y0..y0 + h {
            let row = &mut self.pixels[y as usize * self.stride..][..self.width as usize];
            for x in x0..x0 + w {
                let on = |v: u32| v % period < p.on;
                row[x as usize] = match p.kind {
                    PatternKind::VStripes => if on(x - x0) { p.fg } else { p.bg },
                    PatternKind::HStripes => if on(y - y0) { p.fg } else { p.bg },
                    PatternKind::Checker => if on(x - x0) != on(y - y0) { p.fg } else { p.bg },
                    PatternKind::Solid => p.fg,
                    PatternKind::Noise => {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        0xff00_0000 | (seed as u32 & 0x00ff_ffff)
                    }
                };
            }
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// A 32 px white bar moving 16 px per frame; the buffer last showed frame - 2.
    fn draw_bar(&mut self, frame: u64, background: u32) {
        let span = self.width.saturating_sub(32).max(1) as u64;
        for (f, color) in [(frame.saturating_sub(2), background), (frame, WHITE)] {
            let x = (f * 16 % span) as u32;
            self.fill(x, 0, 32, self.height, color);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// Full-width 300 px horizontal bar moving 12 px/frame top-to-bottom. Updates only
    /// the delta strips between `frame - 2` and `frame`. If `striped_bg` is true, restores
    /// 4-px vertical stripes behind the bar instead of solid `background`.
    fn draw_hbar(&mut self, frame: u64, background: u32, striped_bg: bool) {
        const H: u32 = 300;
        const STEP: u64 = 12;
        let h = H.min(self.height);
        let span = self.height.saturating_sub(h).max(1) as u64;
        let y = (frame * STEP % span) as u32;
        let pat = Pattern::default();
        let mut erase = |ry: u32, rh: u32| {
            if striped_bg {
                self.draw_pattern(&pat, (0, ry, self.width, rh));
            } else {
                self.fill(0, ry, self.width, rh, background);
            }
        };
        if frame <= 2 {
            self.fill(0, y, self.width, h, WHITE);
        } else {
            let prev = (frame - 2) * STEP % span;
            let prev_y = prev as u32;
            if y > prev_y && y < prev_y + h {
                let dy = y - prev_y;
                erase(prev_y, dy);
                self.fill(0, prev_y + h, self.width, dy, WHITE);
            } else {
                erase(prev_y, h);
                self.fill(0, y, self.width, h, WHITE);
            }
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Draw {
    Scene,
    Bar,
    HBar,
    HBarStriped,
    /// A configurable static pattern (see `Pattern`), drawn once, never written again.
    Pattern,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    VStripes,
    HStripes,
    Checker,
    Noise,
    Solid,
}

/// A static test pattern in a rectangle of the primary plane: `on` px of `fg` then
/// `period - on` px of `bg`, repeating (vertical stripes vary along x, horizontal along y,
/// checker along both); `Noise` is random opaque RGB per pixel.
#[derive(Clone, Copy)]
struct Pattern {
    kind: PatternKind,
    period: u32,
    on: u32,
    fg: u32,
    bg: u32,
    /// x, y, w, h; `None` means the graph's band at the bottom of the screen.
    rect: Option<(u32, u32, u32, u32)>,
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            kind: PatternKind::VStripes,
            period: 4,
            on: 3,
            fg: GREEN,
            bg: GRAPH_BACKGROUND,
            rect: None,
        }
    }
}

const BACKGROUND: u32 = 0xff20_3040;
const GRAPH_BACKGROUND: u32 = 0xff10_1010;
const RED: u32 = 0xffff_2020;
const ORANGE: u32 = 0xffff_9000;
const GREEN: u32 = 0xff30_c030;
const CYAN: u32 = 0xff20_e0e0;
const WHITE: u32 = 0xffff_ffff;
/// Frames the red band stays on after a repeated frame (~0.25 s at 117 Hz).
const FLASH_FRAMES: u64 = 30;

/// Visual aids on the primary plane: a block moving at constant speed (a repeated frame
/// is a visible hitch), a red band at the top after each repeat, and a scrolling graph of
/// flip intervals along the bottom. Graph columns: green normal, red a repeat after an
/// overlay flip, orange a repeat without one; cyan ticks mark overlay flips; the white
/// line is the 1.5-pace repeat threshold.
struct Scene {
    width: u32,
    graph_top: u32,
    graph_height: u32,
    columns: u64,
}

impl Scene {
    const COLUMN: u32 = 4;
    const BLOCK: u32 = 160;
    const BLOCK_HEIGHT: u32 = 480;
    const STEP: u64 = 24;
    const BAND: u32 = 160;

    fn new(width: u32, height: u32) -> Self {
        let graph_height = 320.min(height / 3);
        let graph_top = height - graph_height - 20;
        Self {
            width,
            graph_top,
            graph_height,
            columns: (width / Self::COLUMN) as u64,
        }
    }

    fn block_x(&self, frame: u64) -> u32 {
        (frame * Self::STEP % (self.width - Self::BLOCK).max(1) as u64) as u32
    }

    /// Draws frame `frame` into `buffer`, which last showed frame `frame - 2`. `frames[i]`
    /// is the flipped frame `i + 1`.
    fn draw(&self, buffer: &mut Buffer, frame: u64, frames: &[Frame], pace_ns: u64, flash: bool) {
        let block_top = Self::BAND + 120;
        buffer.fill(self.block_x(frame.saturating_sub(2)), block_top, Self::BLOCK, Self::BLOCK_HEIGHT, BACKGROUND);
        buffer.fill(self.block_x(frame), block_top, Self::BLOCK, Self::BLOCK_HEIGHT, WHITE);
        if buffer.band != flash {
            buffer.fill(0, 0, self.width, Self::BAND, if flash { RED } else { BACKGROUND });
            buffer.band = flash;
        }
        let bar_width = Self::COLUMN - 1;
        for k in frame.saturating_sub(2).max(2)..frame {
            let (Some(cur), Some(prev)) = (frames.get(k as usize - 1), frames.get(k.wrapping_sub(2) as usize)) else {
                continue;
            };
            let x = (k % self.columns) as u32 * Self::COLUMN;
            let interval = cur.flip_ns.saturating_sub(prev.flip_ns);
            let repeat = interval > pace_ns * 3 / 2;
            let color = match (repeat, prev.overlay_flipped) {
                (false, _) => GREEN,
                (true, true) => RED,
                (true, false) => ORANGE,
            };
            // One pace interval is 150 px tall; the repeat threshold line sits at 225 px.
            let bar = ((interval * 150 / pace_ns) as u32).min(self.graph_height);
            let bottom = self.graph_top + self.graph_height;
            buffer.fill(x, self.graph_top, Self::COLUMN, self.graph_height, GRAPH_BACKGROUND);
            buffer.fill(x, bottom - bar, bar_width, bar, color);
            buffer.fill(x, bottom - 225.min(self.graph_height), bar_width, 2, WHITE);
            if cur.overlay_flipped {
                buffer.fill(x, self.graph_top, bar_width, 12, CYAN);
            }
            // A gap ahead of the newest column shows where the graph is writing.
            buffer.fill(x + Self::COLUMN, self.graph_top, Self::COLUMN * 6, self.graph_height, BACKGROUND);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Atomic {
    objects: Vec<u32>,
    counts: Vec<u32>,
    props: Vec<u32>,
    values: Vec<u64>,
}

impl Atomic {
    fn object(&mut self, id: u32, props: &[(u32, u64)]) {
        self.objects.push(id);
        self.counts.push(props.len() as u32);
        for &(p, v) in props {
            self.props.push(p);
            self.values.push(v);
        }
    }

    fn commit(&self, fd: RawFd, flags: u32, cookie: u64) -> io::Result<()> {
        let mut raw = drm_mode_atomic {
            flags,
            count_objs: self.objects.len() as u32,
            objs_ptr: self.objects.as_ptr() as u64,
            count_props_ptr: self.counts.as_ptr() as u64,
            props_ptr: self.props.as_ptr() as u64,
            prop_values_ptr: self.values.as_ptr() as u64,
            user_data: cookie,
            ..Default::default()
        };
        ioctl(fd, 0xbc, &mut raw)
    }
}

fn plane_geometry(plane: &Plane, crtc: u32, fb: u32, w: u32, h: u32, x: u32, y: u32) -> Result<Vec<(u32, u64)>> {
    let values = [
        fb as u64,
        crtc as u64,
        0,
        0,
        (w as u64) << 16,
        (h as u64) << 16,
        x as u64,
        y as u64,
        w as u64,
        h as u64,
    ];
    PLANE_PROPS.iter().zip(values).map(|(name, v)| Ok((plane.prop(name)?, v))).collect()
}

/// Blocks until the flip event carrying `cookie` arrives; returns its CLOCK_MONOTONIC ns.
fn wait_flip(file: &mut File, cookie: u64) -> Result<Option<u64>> {
    let mut buf = [0u8; 1024];
    loop {
        let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted && STOP.load(Ordering::Relaxed) {
                return Ok(None);
            }
            return Err(e.into());
        }
        if ready == 0 {
            return Err(format!("flip event for commit {cookie} timed out (200 ms)").into());
        }
        let n = file.read(&mut buf)?;
        let mut offset = 0;
        while offset + size_of::<drm_event>() <= n {
            let header: drm_event = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            if header.type_ == FLIP_COMPLETE && offset + size_of::<drm_event_vblank>() <= n {
                let e: drm_event_vblank = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
                if e.user_data == cookie {
                    return Ok(Some(e.tv_sec as u64 * 1_000_000_000 + e.tv_usec as u64 * 1000));
                }
            }
            offset += header.length.max(8) as usize;
        }
    }
}

struct Options {
    suite: bool,
    list: bool,
    device: Option<String>,
    connector: Option<String>,
    vrr: Option<u64>,
    pace_hz: Option<f64>,
    overlay_every: u64,
    overlay_size: (u32, u32),
    overlay_pos: (u32, u32),
    overlay_opaque: bool,
    keep_planes: bool,
    /// DRM plane type updated every K frames: 0 overlay, 2 cursor.
    update_plane_type: u64,
    overlay_move: bool,
    /// Overlay-update frames commit only the overlay, not the primary.
    exclusive: bool,
    draw: Draw,
    pattern: Pattern,
    seconds: Option<f64>,
    csv: Option<String>,
}

fn parse_args() -> Result<Options> {
    let mut o = Options {
        suite: false,
        list: false,
        device: None,
        connector: None,
        vrr: Some(1),
        pace_hz: None,
        overlay_every: 8,
        overlay_size: (512, 128),
        overlay_pos: (0, 0),
        overlay_opaque: false,
        keep_planes: false,
        update_plane_type: 0,
        overlay_move: false,
        exclusive: false,
        draw: Draw::Scene,
        pattern: Pattern::default(),
        seconds: None,
        csv: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--suite" => o.suite = true,
            "--list" => o.list = true,
            "--device" => o.device = Some(value()?),
            "--connector" => o.connector = Some(value()?),
            "--vrr" => {
                o.vrr = match value()?.as_str() {
                    "on" => Some(1),
                    "off" => Some(0),
                    "keep" => None,
                    v => return Err(format!("--vrr {v}: expected on, off or keep").into()),
                }
            }
            "--pace-hz" => o.pace_hz = Some(value()?.parse()?),
            "--overlay-every" => o.overlay_every = value()?.parse::<u64>()?.max(1),
            "--overlay-size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--overlay-size expects WxH")?;
                o.overlay_size = (w.parse()?, h.parse()?);
            }
            "--overlay-pos" => {
                let v = value()?;
                let (x, y) = v.split_once(',').ok_or("--overlay-pos expects X,Y")?;
                o.overlay_pos = (x.parse()?, y.parse()?);
            }
            "--overlay-opaque" => o.overlay_opaque = true,
            "--keep-planes" => o.keep_planes = true,
            "--cursor-plane" => o.update_plane_type = 2,
            "--overlay-move" => o.overlay_move = true,
            "--exclusive" => o.exclusive = true,
            "--pattern" => {
                o.draw = Draw::Pattern;
                o.pattern.kind = match value()?.as_str() {
                    "vstripes" => PatternKind::VStripes,
                    "hstripes" => PatternKind::HStripes,
                    "checker" => PatternKind::Checker,
                    "noise" => PatternKind::Noise,
                    "solid" => PatternKind::Solid,
                    v => {
                        return Err(format!(
                            "--pattern {v}: expected vstripes, hstripes, checker, noise or solid"
                        )
                        .into())
                    }
                };
            }
            "--pattern-period" => o.pattern.period = value()?.parse()?,
            "--pattern-on" => o.pattern.on = value()?.parse()?,
            "--pattern-fg" => o.pattern.fg = 0xff00_0000 | u32::from_str_radix(value()?.trim_start_matches("0x"), 16)?,
            "--pattern-bg" => o.pattern.bg = 0xff00_0000 | u32::from_str_radix(value()?.trim_start_matches("0x"), 16)?,
            "--pattern-rect" => {
                let v = value()?;
                let n: Vec<u32> = v.split(',').map(str::parse).collect::<std::result::Result<_, _>>()?;
                let [x, y, w, h] = n[..] else {
                    return Err("--pattern-rect expects X,Y,W,H".into());
                };
                o.pattern.rect = Some((x, y, w, h));
            }
            "--draw" => {
                o.draw = match value()?.as_str() {
                    "scene" => Draw::Scene,
                    "bar" => Draw::Bar,
                    "hbar" => Draw::HBar,
                    "hbar-striped" => Draw::HBarStriped,
                    v => return Err(format!("--draw {v}: expected scene, bar, hbar or hbar-striped").into()),
                }
            }
            "--seconds" => o.seconds = Some(value()?.parse()?),
            "--csv" => o.csv = Some(value()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {arg}\n{USAGE}").into()),
        }
    }
    Ok(o)
}

struct Frame {
    overlay_flipped: bool,
    draw_ns: u64,
    commit_ns: u64,
    flip_ns: u64,
}

struct ActiveDisplay {
    device_path: String,
    file: File,
    connector_id: u32,
    connector_name: String,
    crtc: drm_mode_crtc,
    crtc_index: usize,
}

fn candidate_cards() -> Vec<String> {
    let mut cards: Vec<(u32, String)> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let idx = name.strip_prefix("card")?.parse::<u32>().ok()?;
            Some((idx, format!("/dev/dri/{name}")))
        })
        .collect();
    cards.sort_by_key(|(idx, _)| *idx);
    cards.into_iter().map(|(_, p)| p).collect()
}

fn probe_card_all(path: &str, connector_filter: Option<&str>) -> Result<Vec<(ActiveDisplay, Option<u64>)>> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let fd = file.as_raw_fd();
    for capability in [2u64, 3] {
        // UNIVERSAL_PLANES, ATOMIC
        let mut cap = drm_set_client_cap { capability, value: 1 };
        let request = (1u64 << 30) | ((size_of::<drm_set_client_cap>() as u64) << 16) | (100 << 8) | 0x0d;
        if unsafe { libc::ioctl(fd, request as _, &mut cap) } < 0 {
            return Err(format!("{path}: atomic KMS unavailable: {}", io::Error::last_os_error()).into());
        }
    }
    let mut monotonic = drm_get_cap { capability: 6, value: 0 };
    ioctl(fd, 0x0c, &mut monotonic)?;
    if monotonic.value != 1 {
        return Err(format!("{path}: DRM timestamps are not CLOCK_MONOTONIC").into());
    }

    let mut res = drm_mode_card_res::default();
    ioctl(fd, 0xa0, &mut res)?;
    let mut crtcs = vec![0u32; res.count_crtcs as usize];
    let mut connectors = vec![0u32; res.count_connectors as usize];
    res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
    res.connector_id_ptr = connectors.as_mut_ptr() as u64;
    (res.count_fbs, res.count_encoders) = (0, 0);
    ioctl(fd, 0xa0, &mut res)?;
    let types = [
        "Unknown", "VGA", "DVI-I", "DVI-D", "DVI-A", "Composite", "SVIDEO", "LVDS", "Component",
        "DIN", "DP", "HDMI-A", "HDMI-B", "TV", "eDP", "Virtual", "DSI", "DPI", "Writeback", "SPI",
        "USB",
    ];
    let mut matches = Vec::new();
    for &id in &connectors {
        let mut mode = drm_mode_modeinfo::default();
        // One-entry mode buffer: a nonzero count avoids forcing a connector reprobe.
        let mut c = drm_mode_get_connector {
            connector_id: id,
            count_modes: 1,
            modes_ptr: &mut mode as *mut _ as u64,
            ..Default::default()
        };
        ioctl(fd, 0xa7, &mut c)?;
        let name = format!("{}-{}", types.get(c.connector_type as usize).unwrap_or(&"Unknown"), c.connector_type_id);
        if c.connection != 1 || c.encoder_id == 0 || connector_filter.is_some_and(|n| n != name) {
            continue;
        }
        let mut enc = drm_mode_get_encoder { encoder_id: c.encoder_id, ..Default::default() };
        ioctl(fd, 0xa6, &mut enc)?;
        let mut crtc = drm_mode_crtc { crtc_id: enc.crtc_id, ..Default::default() };
        if enc.crtc_id == 0 || ioctl(fd, 0xa1, &mut crtc).is_err() || crtc.mode_valid == 0 {
            continue;
        }
        let crtc_index = crtcs.iter().position(|&c| c == crtc.crtc_id).ok_or("CRTC missing from resources")?;
        let vrr_cap = properties(fd, id, OBJ_CONNECTOR).ok().and_then(|p| find(&p, "vrr_capable")).map(|p| p.1);
        matches.push((id, name, crtc, crtc_index, vrr_cap));
    }
    let mut out = Vec::with_capacity(matches.len());
    for (connector_id, connector_name, crtc, crtc_index, vrr_cap) in matches {
        out.push((
            ActiveDisplay {
                device_path: path.to_string(),
                file: file.try_clone()?,
                connector_id,
                connector_name,
                crtc,
                crtc_index,
            },
            vrr_cap,
        ));
    }
    Ok(out)
}

fn open_active_display(device: Option<&str>, connector: Option<&str>) -> Result<ActiveDisplay> {
    let cards = match device {
        Some(path) => vec![path.to_string()],
        None => {
            let c = candidate_cards();
            if c.is_empty() {
                return Err("no /dev/dri/card* devices found".into());
            }
            c
        }
    };
    let mut fallback: Option<ActiveDisplay> = None;
    let mut last_err = None;
    for path in &cards {
        match probe_card_all(path, connector) {
            Ok(found) => {
                for (disp, vrr_cap) in found {
                    if vrr_cap == Some(1) {
                        return Ok(disp);
                    }
                    if fallback.is_none() {
                        fallback = Some(disp);
                    }
                }
            }
            Err(e) => {
                if device.is_some() {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    if let Some(disp) = fallback {
        return Ok(disp);
    }
    if let Some(e) = last_err {
        return Err(e);
    }
    Err(format!(
        "no connected connector with an active CRTC across {} (pass --device / --connector)",
        cards.join(", ")
    )
    .into())
}

fn list_displays(device: Option<&str>) -> Result<()> {
    let cards = match device {
        Some(path) => vec![path.to_string()],
        None => candidate_cards(),
    };
    if cards.is_empty() {
        println!("No /dev/dri/card* devices found.");
        return Ok(());
    }
    let mut total = 0usize;
    for path in &cards {
        let (gpu_desc, _) = system_metadata(path);
        match probe_card_all(path, None) {
            Ok(found) if found.is_empty() => {
                println!("{path}: {gpu_desc} — no active displays");
            }
            Ok(found) => {
                println!("{path}: {gpu_desc}");
                for (disp, vrr_cap) in found {
                    let fd = disp.file.as_raw_fd();
                    let m = disp.crtc.mode;
                    let period_ns = (m.htotal as u64 * m.vtotal as u64 * 1_000_000) / m.clock.max(1) as u64;
                    let hz = 1e9 / period_ns as f64;
                    let conn_props = properties(fd, disp.connector_id, OBJ_CONNECTOR).unwrap_or_default();
                    let (edid_name, edid_vrr) = parse_edid(fd, &conn_props);
                    let extra = match (edid_name, edid_vrr) {
                        (Some(n), Some((lo, hi))) => format!("  [{n}, EDID range {lo}-{hi} Hz]"),
                        (Some(n), None) => format!("  [{n}]"),
                        (None, Some((lo, hi))) => format!("  [EDID range {lo}-{hi} Hz]"),
                        (None, None) => String::new(),
                    };
                    println!(
                        "  {}  {}x{} @ {:.2} Hz  vrr_capable={}{}",
                        disp.connector_name,
                        m.hdisplay,
                        m.vdisplay,
                        hz,
                        vrr_cap.map(|v| v.to_string()).unwrap_or_else(|| "none".into()),
                        extra,
                    );
                    total += 1;
                }
            }
            Err(e) => {
                println!("{path}: {gpu_desc} — skipped ({e})");
            }
        }
    }
    if total == 0 {
        println!("\nNo active displays found. Set a mode in your compositor before running.");
    }
    Ok(())
}

fn cursor_size(fd: RawFd) -> (u32, u32) {
    let cap = |id: u64| -> Option<u32> {
        let mut c = drm_get_cap { capability: id, value: 0 };
        ioctl(fd, 0x0c, &mut c).ok()?;
        let v = c.value as u32;
        (32..=512).contains(&v).then_some(v)
    };
    let w = cap(8).unwrap_or(256).min(256);
    let h = cap(9).unwrap_or(256).min(256);
    (w, h)
}

/// Extract monitor name and VRR vertical range from base 128-byte EDID descriptors.
fn parse_edid(fd: RawFd, connector_props: &[(String, u32, u64)]) -> (Option<String>, Option<(u32, u32)>) {
    let Some((_, blob_id)) = find(connector_props, "EDID") else {
        return (None, None);
    };
    if blob_id == 0 {
        return (None, None);
    }
    let mut blob = drm_mode_get_blob { blob_id: blob_id as u32, ..Default::default() };
    if ioctl(fd, 0xac, &mut blob).is_err() || blob.length < 128 {
        return (None, None);
    }
    let mut data = vec![0u8; blob.length as usize];
    blob.data = data.as_mut_ptr() as u64;
    if ioctl(fd, 0xac, &mut blob).is_err() {
        return (None, None);
    }
    let (mut name, mut vrr_range) = (None, None);
    for off in [54usize, 72, 90, 108] {
        let b = &data[off..off + 18];
        if b[0..3] != [0, 0, 0] {
            continue;
        }
        match b[3] {
            0xfc => {
                let s: String = b[5..18]
                    .iter()
                    .take_while(|&&c| c != 0x0a && c != 0)
                    .map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { ' ' })
                    .collect();
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    name = Some(trimmed.to_string());
                }
            }
            0xfd => {
                let mut min_v = b[5] as u32;
                let mut max_v = b[6] as u32;
                if b[4] & 1 != 0 {
                    min_v += 255;
                }
                if b[4] & 2 != 0 {
                    max_v += 255;
                }
                if min_v > 0 && max_v >= min_v {
                    vrr_range = Some((min_v, max_v));
                }
            }
            _ => {}
        }
    }
    (name, vrr_range)
}

fn system_metadata(device_path: &str) -> (String, String) {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let card = device_path.rsplit('/').next().unwrap_or("card0");
    let uevent = std::fs::read_to_string(format!("/sys/class/drm/{card}/device/uevent")).unwrap_or_default();
    let get = |k: &str| -> Option<String> {
        uevent.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()))
    };
    let driver = get("DRIVER=").unwrap_or_else(|| "unknown".into());
    let pci_id = get("PCI_ID=").unwrap_or_else(|| "unknown".into()).to_lowercase();
    let pci_slot = get("PCI_SLOT_NAME=");

    if driver == "nvidia" {
        let model = pci_slot
            .as_deref()
            .and_then(|slot| std::fs::read_to_string(format!("/proc/driver/nvidia/gpus/{slot}/information")).ok())
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Model:").map(|v| v.trim().to_string())))
            .unwrap_or_else(|| "NVIDIA GPU".into());
        let nv_proc = std::fs::read_to_string("/proc/driver/nvidia/version").unwrap_or_default();
        let kind = if nv_proc.contains("Open Kernel Module") {
            "open"
        } else if nv_proc.contains("Kernel Module") {
            "proprietary"
        } else {
            "unknown"
        };
        let ver = std::fs::read_to_string("/sys/module/nvidia/version")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        return (format!("{model} (`{pci_id}`, driver `nvidia` `{ver}` {kind})"), kernel);
    }
    (format!("PCI `{pci_id}` (driver `{driver}`)"), kernel)
}

#[derive(Default)]
struct GroupStats {
    n: usize,
    delayed: usize,
    repeats: usize,
    dt_p50_us: u64,
    c2f_p50_us: u64,
    c2f_p95_us: u64,
    c2f_p99_us: u64,
}

fn compute_stats(frames: &[Frame], pace_ns: u64, period_ns: u64, skip_warmup_ns: u64) -> [GroupStats; 2] {
    let first_flip = frames.first().map(|f| f.flip_ns).unwrap_or(0);
    let mut dt: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut c2f: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
    let mut delayed = [0usize; 2];
    let mut repeats = [0usize; 2];
    for pair in frames.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        if skip_warmup_ns > 0 && cur.flip_ns.saturating_sub(first_flip) < skip_warmup_ns {
            continue;
        }
        let idx = usize::from(prev.overlay_flipped);
        let d = cur.flip_ns.saturating_sub(prev.flip_ns);
        let l = cur.flip_ns.saturating_sub(cur.commit_ns);
        dt[idx].push(d);
        c2f[idx].push(l);
        // Genuine VRR stall: waited >1 ms after commit AND flipped past T_min (+0.5 ms),
        // excluding normal catch-up waits when committed early inside active scanout.
        if l > 1_000_000 && d > period_ns + 500_000 {
            delayed[idx] += 1;
        }
        if d > pace_ns * 3 / 2 {
            repeats[idx] += 1;
        }
    }
    let mut build = |i: usize| -> GroupStats {
        let n = dt[i].len();
        if n == 0 {
            return GroupStats::default();
        }
        dt[i].sort_unstable();
        c2f[i].sort_unstable();
        let pct = |v: &[u64], p: usize| v[(v.len() - 1) * p / 100] / 1000;
        GroupStats {
            n,
            delayed: delayed[i],
            repeats: repeats[i],
            dt_p50_us: pct(&dt[i], 50),
            c2f_p50_us: pct(&c2f[i], 50),
            c2f_p95_us: pct(&c2f[i], 95),
            c2f_p99_us: pct(&c2f[i], 99),
        }
    };
    [build(0), build(1)]
}

fn disable_plane(plane: &Plane) -> Result<Vec<(u32, u64)>> {
    Ok(vec![(plane.prop("FB_ID")?, 0), (plane.prop("CRTC_ID")?, 0)])
}

fn configure_secondary_state(
    primary: &Plane,
    secondary: &Plane,
    crtc_id: u32,
    fb: u32,
    (x, y, w, h): (u32, u32, u32, u32),
) -> Result<Vec<(u32, u64)>> {
    let mut state = plane_geometry(secondary, crtc_id, fb, w, h, x, y)?;
    if let Some((zpos, _)) = find(&secondary.props, "zpos") {
        let primary_z = find(&primary.props, "zpos").map(|z| z.1).unwrap_or(0);
        state.push((zpos, primary_z + 1));
    }
    for (name, value) in [("rotation", 1), ("alpha", 0xffff), ("pixel blend mode", 0)] {
        if let Some((id, _)) = find(&secondary.props, name) {
            state.push((id, value));
        }
    }
    Ok(state)
}

fn run_suite(o: Options) -> Result<()> {
    let mut disp = open_active_display(o.device.as_deref(), o.connector.as_deref())?;
    let fd = disp.file.as_raw_fd();
    let m = disp.crtc.mode;
    let period_ns = (m.htotal as u64 * m.vtotal as u64 * 1_000_000) / m.clock.max(1) as u64;
    let refresh_hz = 1e9 / period_ns as f64;
    let pace_hz = o.pace_hz.unwrap_or(refresh_hz * 0.83);
    let pace_ns = (1e9 / pace_hz) as u64;
    let case_secs = o.seconds.unwrap_or(7.0);
    let (width, height) = (m.hdisplay as u32, m.vdisplay as u32);
    let conn_props = properties(fd, disp.connector_id, OBJ_CONNECTOR)?;
    let vrr_capable = find(&conn_props, "vrr_capable").map(|p| p.1);
    let (edid_name, edid_vrr) = parse_edid(fd, &conn_props);
    let crtc_props = properties(fd, disp.crtc.crtc_id, OBJ_CRTC)?;
    let vrr_prop = find(&crtc_props, "VRR_ENABLED");
    let (gpu_desc, kernel) = system_metadata(&disp.device_path);

    let mut planes = drm_mode_get_plane_res::default();
    ioctl(fd, 0xb5, &mut planes)?;
    let mut ids = vec![0u32; planes.count_planes as usize];
    planes.plane_id_ptr = ids.as_mut_ptr() as u64;
    ioctl(fd, 0xb5, &mut planes)?;
    let (mut primary, mut overlay, mut cursor, mut active) = (None, None, None, Vec::new());
    for &id in &ids {
        let mut p = drm_mode_get_plane { plane_id: id, ..Default::default() };
        ioctl(fd, 0xb6, &mut p)?;
        let mut formats = vec![0u32; p.count_format_types as usize];
        p.format_type_ptr = formats.as_mut_ptr() as u64;
        ioctl(fd, 0xb6, &mut p)?;
        if p.possible_crtcs & (1 << disp.crtc_index) == 0 || (p.crtc_id != 0 && p.crtc_id != disp.crtc.crtc_id) {
            continue;
        }
        let props = properties(fd, id, OBJ_PLANE)?;
        if p.crtc_id == disp.crtc.crtc_id {
            active.push(id);
        }
        match find(&props, "type").map(|t| t.1) {
            Some(1) if primary.is_none() && formats.contains(&XRGB8888) => primary = Some(Plane { id, props }),
            Some(0) if overlay.is_none() && formats.contains(&ARGB8888) => overlay = Some(Plane { id, props }),
            Some(2) if cursor.is_none() && formats.contains(&ARGB8888) => cursor = Some(Plane { id, props }),
            _ => {}
        }
    }
    let primary = primary.ok_or("no primary plane with XRGB8888")?;
    let others: Vec<u32> = active
        .into_iter()
        .filter(|&id| {
            id != primary.id
                && overlay.as_ref().is_none_or(|p| p.id != id)
                && cursor.as_ref().is_none_or(|p| p.id != id)
        })
        .collect();

    let request = (100u64 << 8) | 0x1e; // SET_MASTER
    if unsafe { libc::ioctl(fd, request as _) } < 0 {
        return Err(format!(
            "cannot become DRM master ({}); run from a spare VT without a compositor on this card",
            io::Error::last_os_error()
        )
        .into());
    }
    let action = libc::sigaction {
        sa_sigaction: on_signal as *const () as usize,
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    for signal in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }

    let mut game = [
        Buffer::new(fd, width, height, XRGB8888, GRAPH_BACKGROUND)?,
        Buffer::new(fd, width, height, XRGB8888, GRAPH_BACKGROUND)?,
    ];
    let (ow, oh) = (o.overlay_size.0.min(width), o.overlay_size.1.min(height));
    let (ox, oy) = (o.overlay_pos.0.min(width.saturating_sub(ow + 16)), o.overlay_pos.1.min(height.saturating_sub(oh)));
    let overlay_bufs = if overlay.is_some() {
        let mut b = [
            Buffer::new(fd, ow, oh, ARGB8888, 0xffe0_6020)?,
            Buffer::new(fd, ow, oh, ARGB8888, 0xff20_c0e0)?,
        ];
        for buf in &mut b {
            let (cw, ch) = (buf.width.min(320), buf.height.min(120));
            buf.fill(cw, 0, buf.width - cw, ch, 0);
            buf.fill(0, ch, buf.width, buf.height - ch, 0);
        }
        Some(b)
    } else {
        None
    };
    let (cw, ch) = cursor_size(fd);
    let (cx, cy) = (0u32, 0u32);
    let cursor_buf = if cursor.is_some() {
        let mut b = Buffer::new(fd, cw, ch, ARGB8888, 0)?;
        b.fill(0, 0, cw.min(32), ch.min(32), 0xffff_ffff);
        Some(b)
    } else {
        None
    };

    struct CaseSpec {
        label: &'static str,
        update_label: &'static str,
        kind: PatternKind,
        top_half_only: bool,
        use_cursor: bool,
        exclusive: bool,
    }
    let cases = [
        CaseSpec { label: "`solid` (control)", update_label: "overlay swap", kind: PatternKind::Solid, top_half_only: false, use_cursor: false, exclusive: false },
        CaseSpec { label: "`hstripes` (control)", update_label: "overlay swap", kind: PatternKind::HStripes, top_half_only: false, use_cursor: false, exclusive: false },
        CaseSpec { label: "`vstripes` (full)", update_label: "overlay swap", kind: PatternKind::VStripes, top_half_only: false, use_cursor: false, exclusive: false },
        CaseSpec { label: "`vstripes` (top half)", update_label: "overlay swap", kind: PatternKind::VStripes, top_half_only: true, use_cursor: false, exclusive: false },
        CaseSpec { label: "`vstripes` (cursor)", update_label: "cursor move", kind: PatternKind::VStripes, top_half_only: false, use_cursor: true, exclusive: false },
        CaseSpec { label: "`vstripes` (`--exclusive`)", update_label: "overlay only (1 plane)", kind: PatternKind::VStripes, top_half_only: false, use_cursor: false, exclusive: true },
    ];

    let mut next_cookie = 1u64;
    let primary_fb = primary.prop("FB_ID")?;
    let mut rows: Vec<String> = Vec::new();
    let mut vrr_set_ok = true;

    for (ci, case) in cases.iter().enumerate() {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let sec_plane = if case.use_cursor { cursor.as_ref() } else { overlay.as_ref() };
        let Some(sec) = sec_plane else {
            let reason = if case.use_cursor { "no cursor plane" } else { "no overlay plane" };
            rows.push(format!("| {} | {} | skipped ({reason}) | — | — | — |", case.label, case.update_label));
            continue;
        };

        let pat = Pattern { kind: case.kind, period: 4, on: 3, fg: GREEN, bg: GRAPH_BACKGROUND, rect: None };
        let rect = if case.top_half_only { (0, 0, width, height / 2) } else { (0, 0, width, height) };
        for buf in &mut game {
            buf.fill(0, 0, width, height, GRAPH_BACKGROUND);
            buf.draw_pattern(&pat, rect);
        }

        let mut setup = Atomic::default();
        setup.object(primary.id, &plane_geometry(&primary, disp.crtc.crtc_id, game[0].fb, width, height, 0, 0)?);
        if case.use_cursor {
            if let Some(ovl) = &overlay {
                setup.object(ovl.id, &disable_plane(ovl)?);
            }
            let cb = cursor_buf.as_ref().unwrap();
            setup.object(sec.id, &configure_secondary_state(&primary, sec, disp.crtc.crtc_id, cb.fb, (cx, cy, cw, ch))?);
        } else {
            if let Some(cur) = &cursor {
                setup.object(cur.id, &disable_plane(cur)?);
            }
            let ob = overlay_bufs.as_ref().unwrap();
            setup.object(sec.id, &configure_secondary_state(&primary, sec, disp.crtc.crtc_id, ob[0].fb, (ox, oy, ow, oh))?);
        }
        if ci == 0 {
            if !o.keep_planes {
                for &id in &others {
                    let props = properties(fd, id, OBJ_PLANE)?;
                    let (Some((fb, _)), Some((crtc_prop, _))) = (find(&props, "FB_ID"), find(&props, "CRTC_ID")) else {
                        continue;
                    };
                    setup.object(id, &[(fb, 0), (crtc_prop, 0)]);
                }
            }
            if let (Some(value), Some((id, _))) = (o.vrr, vrr_prop) {
                setup.object(disp.crtc.crtc_id, &[(id, value)]);
            } else if o.vrr.is_some() {
                vrr_set_ok = false;
            }
        }
        if let Err(e) = setup.commit(fd, TEST_ONLY, 0) {
            rows.push(format!("| {} | {} | skipped (TEST_ONLY: {e}) | — | — | — |", case.label, case.update_label));
            continue;
        }
        let c0 = next_cookie;
        next_cookie += 1;
        setup.commit(fd, NONBLOCK | FLIP_EVENT, c0)?;
        let Some(initial_flip) = wait_flip(&mut disp.file, c0)? else {
            break;
        };

        let sec_fb = sec.prop("FB_ID")?;
        let sec_x = sec.prop("CRTC_X")?;
        let mut frames: Vec<Frame> = Vec::with_capacity((case_secs * pace_hz * 1.1) as usize);
        let start = now_ns();
        let duration = (case_secs * 1e9) as u64;
        let mut next_due = initial_flip + pace_ns;
        let mut frame = 1u64;
        let mut err_msg = None;

        while now_ns() - start < duration && !STOP.load(Ordering::Relaxed) {
            let buffer = &game[(frame % 2) as usize];
            let overlay_flipped = frame.is_multiple_of(o.overlay_every);
            let mut commit = Atomic::default();
            if !(case.exclusive && overlay_flipped) {
                commit.object(primary.id, &[(primary_fb, buffer.fb as u64)]);
            }
            if overlay_flipped {
                let n = frame / o.overlay_every;
                if case.use_cursor {
                    commit.object(sec.id, &[(sec_x, cx as u64 + 16 * (n % 2))]);
                } else {
                    let ob = overlay_bufs.as_ref().unwrap();
                    commit.object(sec.id, &[(sec_fb, ob[(n % 2) as usize].fb as u64)]);
                }
            }
            sleep_until(next_due);
            let cookie = next_cookie;
            next_cookie += 1;
            let commit_ns = now_ns();
            if let Err(e) = commit.commit(fd, NONBLOCK | FLIP_EVENT, cookie) {
                err_msg = Some(format!("commit error: {e}"));
                break;
            }
            match wait_flip(&mut disp.file, cookie) {
                Ok(Some(flip_ns)) => {
                    frames.push(Frame { overlay_flipped, draw_ns: 0, commit_ns, flip_ns });
                    // Rebase after any stalled flip (>1 ms past schedule) so subsequent
                    // primary-only frames do not run in a catch-up backlog.
                    if flip_ns > next_due + 1_000_000 {
                        next_due = flip_ns + pace_ns;
                    } else {
                        next_due += pace_ns;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    err_msg = Some(e.to_string());
                    break;
                }
            }
            frame += 1;
        }

        if let Some(err) = err_msg {
            rows.push(format!("| {} | {} | error ({err}) | — | — | — |", case.label, case.update_label));
            continue;
        }
        let warmup = if case_secs >= 3.0 { 1_000_000_000 } else { 0 };
        let [pri, ovl] = compute_stats(&frames, pace_ns, period_ns, warmup);
        let tmin_ms = period_ns as f64 / 1e6;
        let dt_ms = ovl.dt_p50_us as f64 / 1000.0;
        let ratio = dt_ms / tmin_ms;
        let pct_del = if ovl.n > 0 { 100.0 * ovl.delayed as f64 / ovl.n as f64 } else { 0.0 };
        rows.push(format!(
            "| {} | {} | {:.2} ms ({:.2}×) | {:.2} / {:.2} ms | {} / {} ({:.1}%) | {:.2} ms |",
            case.label,
            case.update_label,
            dt_ms,
            ratio,
            ovl.c2f_p50_us as f64 / 1000.0,
            ovl.c2f_p95_us as f64 / 1000.0,
            ovl.delayed,
            ovl.n,
            pct_del,
            pri.c2f_p50_us as f64 / 1000.0,
        ));
    }

    // Restore: secondary planes off, VRR as found.
    let mut cleanup = Atomic::default();
    if let Some(ovl) = &overlay {
        cleanup.object(ovl.id, &disable_plane(ovl)?);
    }
    if let Some(cur) = &cursor {
        cleanup.object(cur.id, &disable_plane(cur)?);
    }
    if let (Some(_), Some((id, value))) = (o.vrr, vrr_prop) {
        cleanup.object(disp.crtc.crtc_id, &[(id, value)]);
    }
    let cookie = u64::MAX;
    if cleanup.commit(fd, NONBLOCK | FLIP_EVENT, cookie).is_ok() {
        let _ = wait_flip(&mut disp.file, cookie);
    }

    let edid_desc = match (edid_name, edid_vrr) {
        (Some(n), Some((lo, hi))) => format!(" — `{n}` (EDID `{lo}–{hi} Hz`)"),
        (Some(n), None) => format!(" — `{n}`"),
        (None, Some((lo, hi))) => format!(" (EDID `{lo}–{hi} Hz`)"),
        (None, None) => String::new(),
    };
    println!("### System");
    println!("- **GPU:** {gpu_desc}");
    println!("- **Kernel:** `{kernel}`");
    println!("- **Connector:** `{}` (`{}`){edid_desc}", disp.connector_name, disp.device_path);
    println!(
        "- **Mode:** `{width}x{height} @ {refresh_hz:.3} Hz` (`T_min = {:.2} ms`, `vrr_capable={}`, `VRR_ENABLED: {} -> {}`)",
        period_ns as f64 / 1e6,
        vrr_capable.map(|v| v.to_string()).unwrap_or_else(|| "none".into()),
        vrr_prop.map(|p| p.1.to_string()).unwrap_or_else(|| "none".into()),
        o.vrr.or(vrr_prop.map(|p| p.1)).map(|v| v.to_string()).unwrap_or_else(|| "none".into()),
    );
    println!(
        "- **Suite pace:** `{pace_hz:.2} Hz` (`{:.2} ms`, {:.0}% of refresh), plane update every {} frames, {:.0} s/case",
        pace_ns as f64 / 1e6,
        100.0 * pace_hz / refresh_hz,
        o.overlay_every,
        case_secs,
    );
    if vrr_capable == Some(0) || !vrr_set_ok {
        println!("\n> **Warning:** VRR does not appear to be enabled on this connector (`vrr_capable={vrr_capable:?}`, `VRR_ENABLED` available={vrr_set_ok}). Results may reflect fixed-refresh timing.");
    }
    println!();
    println!("| Case | Plane update | After update: `dt` p50 (×T_min) | After update: `commit→flip` p50 / p95 | Delayed (>1 ms) | Primary-only: `commit→flip` p50 |");
    println!("|---|---|---:|---:|---:|---:|");
    for r in &rows {
        println!("{r}");
    }
    Ok(())
}

fn run(o: Options) -> Result<()> {
    if o.list {
        return list_displays(o.device.as_deref());
    }
    if o.suite {
        return run_suite(o);
    }
    let mut disp = open_active_display(o.device.as_deref(), o.connector.as_deref())?;
    let fd = disp.file.as_raw_fd();
    let (connector_id, connector_name, crtc, crtc_index) =
        (disp.connector_id, disp.connector_name, disp.crtc, disp.crtc_index);
    let m = crtc.mode;
    let period_ns = (m.htotal as u64 * m.vtotal as u64 * 1_000_000) / m.clock.max(1) as u64;
    let refresh_hz = 1e9 / period_ns as f64;
    let pace_hz = o.pace_hz.unwrap_or(refresh_hz * 0.975);
    let pace_ns = (1e9 / pace_hz) as u64;
    let seconds = o.seconds.unwrap_or(30.0);
    let (width, height) = (m.hdisplay as u32, m.vdisplay as u32);
    let (ow, oh) = if o.update_plane_type == 2 {
        cursor_size(fd)
    } else {
        (o.overlay_size.0.min(width), o.overlay_size.1.min(height))
    };
    let vrr_capable = find(&properties(fd, connector_id, OBJ_CONNECTOR)?, "vrr_capable").map(|p| p.1);
    let crtc_props = properties(fd, crtc.crtc_id, OBJ_CRTC)?;
    let vrr_prop = find(&crtc_props, "VRR_ENABLED");

    // Primary and secondary planes for this CRTC.
    let mut planes = drm_mode_get_plane_res::default();
    ioctl(fd, 0xb5, &mut planes)?;
    let mut ids = vec![0u32; planes.count_planes as usize];
    planes.plane_id_ptr = ids.as_mut_ptr() as u64;
    ioctl(fd, 0xb5, &mut planes)?;
    let (mut primary, mut overlay, mut active) = (None, None, Vec::new());
    for &id in &ids {
        let mut p = drm_mode_get_plane { plane_id: id, ..Default::default() };
        ioctl(fd, 0xb6, &mut p)?;
        let mut formats = vec![0u32; p.count_format_types as usize];
        p.format_type_ptr = formats.as_mut_ptr() as u64;
        ioctl(fd, 0xb6, &mut p)?;
        if p.possible_crtcs & (1 << crtc_index) == 0 || (p.crtc_id != 0 && p.crtc_id != crtc.crtc_id) {
            continue;
        }
        let props = properties(fd, id, OBJ_PLANE)?;
        if p.crtc_id == crtc.crtc_id {
            active.push(id);
        }
        match find(&props, "type").map(|t| t.1) {
            Some(1) if primary.is_none() && formats.contains(&XRGB8888) => primary = Some(Plane { id, props }),
            Some(t) if t == o.update_plane_type && overlay.is_none() && formats.contains(&ARGB8888) => {
                overlay = Some(Plane { id, props })
            }
            _ => {}
        }
    }
    let primary = primary.ok_or("no primary plane with XRGB8888")?;
    let overlay = overlay.ok_or(if o.update_plane_type == 2 {
        "no cursor plane with ARGB8888"
    } else {
        "no overlay plane with ARGB8888"
    })?;
    let others: Vec<u32> = active.into_iter().filter(|&id| id != primary.id && id != overlay.id).collect();
    let (ox, oy) = (o.overlay_pos.0.min(width.saturating_sub(ow + 16)), o.overlay_pos.1.min(height.saturating_sub(oh)));

    println!(
        "{connector_name} (connector {connector_id}, {}), CRTC {} at {width}x{height} {refresh_hz:.3} Hz; vrr_capable={vrr_capable:?} VRR_ENABLED={:?} -> {:?}",
        disp.device_path,
        crtc.crtc_id,
        vrr_prop.map(|p| p.1),
        o.vrr.or(vrr_prop.map(|p| p.1))
    );
    println!(
        "primary plane {}, {} plane {} ({ow}x{oh} at {ox},{oy}, {}); pace {pace_hz:.3} Hz; {} every {} frames; {seconds} s",
        primary.id,
        if o.update_plane_type == 2 { "cursor" } else { "overlay" },
        overlay.id,
        if o.overlay_opaque { "opaque" } else { "transparent margins, pre-multiplied blending" },
        if o.overlay_move { "move" } else { "buffer swap" },
        o.overlay_every
    );
    println!("other planes on this CRTC {others:?}: {}", if o.keep_planes { "kept" } else { "disabled" });
    if vrr_capable == Some(0) && o.vrr == Some(1) {
        eprintln!("warning: connector reports vrr_capable=0, VRR_ENABLED=1 will have no effect");
    }

    let request = (100u64 << 8) | 0x1e; // SET_MASTER
    if unsafe { libc::ioctl(fd, request as _) } < 0 {
        return Err(format!(
            "cannot become DRM master ({}); run from a spare VT without a compositor on this card",
            io::Error::last_os_error()
        )
        .into());
    }
    let action = libc::sigaction {
        sa_sigaction: on_signal as *const () as usize,
        sa_mask: unsafe { std::mem::zeroed() },
        sa_flags: 0,
        sa_restorer: None,
    };
    for signal in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }

    let mut game = [
        Buffer::new(fd, width, height, XRGB8888, BACKGROUND)?,
        Buffer::new(fd, width, height, XRGB8888, BACKGROUND)?,
    ];
    let scene = Scene::new(width, height);
    if o.draw == Draw::Pattern {
        let rect = o.pattern.rect.unwrap_or((0, scene.graph_top, width, scene.graph_height));
        for buffer in &mut game {
            buffer.draw_pattern(&o.pattern, rect);
        }
        println!(
            "static pattern: kind={} period={} on={} fg={:06x} bg={:06x} rect={rect:?}",
            match o.pattern.kind {
                PatternKind::VStripes => "vstripes",
                PatternKind::HStripes => "hstripes",
                PatternKind::Checker => "checker",
                PatternKind::Noise => "noise",
                PatternKind::Solid => "solid",
            },
            o.pattern.period,
            o.pattern.on,
            o.pattern.fg & 0xffffff,
            o.pattern.bg & 0xffffff
        );
    }
    if o.draw == Draw::HBarStriped {
        let pat = Pattern::default();
        for buffer in &mut game {
            buffer.draw_pattern(&pat, (0, 0, width, height));
        }
    }
    let mut notification = [
        Buffer::new(fd, ow, oh, ARGB8888, 0xffe0_6020)?,
        Buffer::new(fd, ow, oh, ARGB8888, 0xff20_c0e0)?,
    ];
    if !o.overlay_opaque {
        for b in &mut notification {
            let (w, h) = (b.width, b.height);
            let (cw, ch) = (w.min(320), h.min(120));
            b.fill(cw, 0, w - cw, ch, 0);
            b.fill(0, ch, w, h - ch, 0);
        }
    }

    // First commit: full plane state plus VRR_ENABLED; later commits change FB_IDs/CRTC_X only.
    let mut setup = Atomic::default();
    setup.object(primary.id, &plane_geometry(&primary, crtc.crtc_id, game[0].fb, width, height, 0, 0)?);
    setup.object(overlay.id, &configure_secondary_state(&primary, &overlay, crtc.crtc_id, notification[0].fb, (ox, oy, ow, oh))?);
    if !o.keep_planes {
        for &id in &others {
            let props = properties(fd, id, OBJ_PLANE)?;
            let (Some((fb, _)), Some((crtc_prop, _))) = (find(&props, "FB_ID"), find(&props, "CRTC_ID")) else {
                continue;
            };
            setup.object(id, &[(fb, 0), (crtc_prop, 0)]);
        }
    }
    if let (Some(value), Some((id, _))) = (o.vrr, vrr_prop) {
        setup.object(crtc.crtc_id, &[(id, value)]);
    } else if o.vrr.is_some() {
        return Err("CRTC has no VRR_ENABLED property".into());
    }
    setup.commit(fd, TEST_ONLY, 0).map_err(|e| format!("setup TEST_ONLY rejected: {e}"))?;
    setup.commit(fd, NONBLOCK | FLIP_EVENT, 1)?;
    wait_flip(&mut disp.file, 1)?.ok_or("stopped")?;

    let (primary_fb, overlay_fb) = (primary.prop("FB_ID")?, overlay.prop("FB_ID")?);
    let overlay_crtc_x = overlay.prop("CRTC_X")?;
    let mut frames: Vec<Frame> = Vec::with_capacity((seconds * pace_hz * 1.1) as usize);
    let mut failure = None;
    let start = now_ns();
    let duration = (seconds * 1e9) as u64;
    let mut frame = 1u64;
    let mut flash_until = 0u64;
    while now_ns() - start < duration && !STOP.load(Ordering::Relaxed) {
        if let [.., prev, cur] = frames.as_slice() {
            if cur.flip_ns - prev.flip_ns > pace_ns * 3 / 2 {
                flash_until = frame + FLASH_FRAMES;
            }
        }
        let buffer = &mut game[(frame % 2) as usize];
        let draw_start = now_ns();
        match o.draw {
            Draw::Scene => scene.draw(buffer, frame, &frames, pace_ns, frame < flash_until),
            Draw::Bar => buffer.draw_bar(frame, BACKGROUND),
            Draw::HBar => buffer.draw_hbar(frame, BACKGROUND, false),
            Draw::HBarStriped => buffer.draw_hbar(frame, BACKGROUND, true),
            Draw::Pattern => {}
        }
        let draw_ns = now_ns() - draw_start;
        let overlay_flipped = frame.is_multiple_of(o.overlay_every);
        let mut commit = Atomic::default();
        if !(o.exclusive && overlay_flipped) {
            commit.object(primary.id, &[(primary_fb, buffer.fb as u64)]);
        }
        if overlay_flipped {
            let n = frame / o.overlay_every;
            if o.overlay_move {
                commit.object(overlay.id, &[(overlay_crtc_x, ox as u64 + 16 * (n % 2))]);
            } else {
                commit.object(overlay.id, &[(overlay_fb, notification[(n % 2) as usize].fb as u64)]);
            }
        }
        sleep_until(start + (frame - 1) * pace_ns);
        let cookie = frame + 1;
        let commit_ns = now_ns();
        if let Err(e) = commit.commit(fd, NONBLOCK | FLIP_EVENT, cookie) {
            failure = Some(format!("frame {frame}: commit failed: {e}"));
            break;
        }
        match wait_flip(&mut disp.file, cookie) {
            Ok(Some(flip_ns)) => frames.push(Frame { overlay_flipped, draw_ns, commit_ns, flip_ns }),
            Ok(None) => break,
            Err(e) => {
                failure = Some(format!("frame {frame}: {e}"));
                break;
            }
        }
        frame += 1;
    }

    // Restore: overlay off, VRR as found. The primary keeps our framebuffer; when the process
    // exits the kernel removes it, which turns the CRTC off until the compositor takes the
    // VT back and sets its mode again.
    let mut cleanup = Atomic::default();
    cleanup.object(overlay.id, &disable_plane(&overlay)?);
    if let (Some(_), Some((id, value))) = (o.vrr, vrr_prop) {
        cleanup.object(crtc.crtc_id, &[(id, value)]);
    }
    let cookie = u64::MAX;
    if cleanup.commit(fd, NONBLOCK | FLIP_EVENT, cookie).is_ok() {
        let _ = wait_flip(&mut disp.file, cookie);
    }

    if let Some(path) = &o.csv {
        let mut out = io::BufWriter::new(File::create(path)?);
        writeln!(out, "frame,overlay_flipped,draw_ns,commit_ns,flip_ns")?;
        for (i, f) in frames.iter().enumerate() {
            writeln!(out, "{},{},{},{},{}", i + 1, u8::from(f.overlay_flipped), f.draw_ns, f.commit_ns, f.flip_ns)?;
        }
    }
    summarize(&frames, pace_ns, period_ns);
    if let Some(f) = failure {
        return Err(f.into());
    }
    Ok(())
}

fn summarize(frames: &[Frame], pace_ns: u64, period_ns: u64) {
    if let (Some(first), Some(last)) = (frames.first(), frames.last()) {
        let secs = (last.flip_ns - first.flip_ns) as f64 / 1e9;
        println!(
            "\n{} frames, {:.2} flips/s (pace {:.2}/s, refresh {:.2} Hz)",
            frames.len(),
            (frames.len() - 1) as f64 / secs,
            1e9 / pace_ns as f64,
            1e9 / period_ns as f64
        );
        let mut draw: Vec<u64> = frames.iter().map(|f| f.draw_ns).collect();
        draw.sort_unstable();
        let (p50, p99) = (draw[(draw.len() - 1) / 2], draw[(draw.len() - 1) * 99 / 100]);
        println!("CPU drawing per frame: p50={} p99={} us", p50 / 1000, p99 / 1000);
        if p99 > pace_ns / 4 {
            println!("warning: drawing takes a large part of the frame budget; pacing may be disturbed");
        }
    }
    let [primary_only, after_overlay] = compute_stats(frames, pace_ns, period_ns, 0);
    let tmin_ms = period_ns as f64 / 1e6;
    println!("delayed = commit->flip > 1 ms & dt > T_min+0.5 ms; repeat = dt > 1.5 pace; commit->flip in us");
    for (label, g) in [("after overlay flip", &after_overlay), ("after primary-only", &primary_only)] {
        if g.n == 0 {
            println!("  {label:20} n=0");
            continue;
        }
        let dt_p50_ms = g.dt_p50_us as f64 / 1000.0;
        println!(
            "  {label:20} n={:5} delayed={:4} ({:5.1}%) repeats={:4} ({:5.1}%)  dt p50={:.2}ms ({:.2}x)  commit->flip p50={} p99={}",
            g.n,
            g.delayed,
            100.0 * g.delayed as f64 / g.n as f64,
            g.repeats,
            100.0 * g.repeats as f64 / g.n as f64,
            dt_p50_ms,
            dt_p50_ms / tmin_ms,
            g.c2f_p50_us,
            g.c2f_p99_us,
        );
    }
}

fn main() {
    match parse_args().and_then(run) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("drm-plane-bench: {e}");
            std::process::exit(1);
        }
    }
}
