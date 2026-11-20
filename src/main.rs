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
  --regression           run the --suite cases plus 5 regression cases (overlay
                         move, overlay move --exclusive, cursor swap, cursor-only
                         idle, VRR off control); ~77 s, same Markdown report
  --dual-head            run 4 cases updating two active CRTCs of one device in the
                         same atomic commit (VRR+VRR flips, flip+cursor both ways,
                         VRR+fixed); ~28 s, Markdown report; sets VRR per case
  --connector-b NAME     second head for --dual-head (default: the next active CRTC
                         on the same device)
  --list                 list detected GPUs, active displays and VRR capability
                         (safe to run inside a graphical session)
  --device PATH          DRM device (default: auto-detect, preferring vrr_capable=1)
  --connector NAME       e.g. HDMI-A-1, DP-1 (default: first connected active CRTC;
                         head A in --dual-head)
  --vrr on|off|keep      VRR_ENABLED for the run (default on); restored at exit
  --pace-hz HZ           primary flip rate cap (default: 83% of refresh in --suite
                         and --regression, of the slower head in --dual-head;
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
  --seconds S            run length (default: 7 per case in --suite, --regression
                         and --dual-head; 30 single-run)
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

enum EventPoll {
    Read,
    TimedOut,
    Stopped,
}

/// Waits up to 200 ms for DRM events and passes each flip completion to
/// `on_flip(user_data, crtc_id, CLOCK_MONOTONIC ns)`.
fn read_flip_events(file: &mut File, mut on_flip: impl FnMut(u64, u32, u64)) -> Result<EventPoll> {
    let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut pfd, 1, 200) };
    if ready < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted && STOP.load(Ordering::Relaxed) {
            return Ok(EventPoll::Stopped);
        }
        return Err(e.into());
    }
    if ready == 0 {
        return Ok(EventPoll::TimedOut);
    }
    let mut buf = [0u8; 1024];
    let n = file.read(&mut buf)?;
    let mut offset = 0;
    while offset + size_of::<drm_event>() <= n {
        let header: drm_event = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
        if header.type_ == FLIP_COMPLETE && offset + size_of::<drm_event_vblank>() <= n {
            let e: drm_event_vblank = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            on_flip(e.user_data, e.crtc_id, e.tv_sec as u64 * 1_000_000_000 + e.tv_usec as u64 * 1000);
        }
        offset += header.length.max(8) as usize;
    }
    Ok(EventPoll::Read)
}

/// Blocks until the flip event carrying `cookie` arrives; returns its CLOCK_MONOTONIC ns.
fn wait_flip(file: &mut File, cookie: u64) -> Result<Option<u64>> {
    let mut flip = None;
    while flip.is_none() {
        let poll = read_flip_events(file, |data, _, ns| {
            if data == cookie {
                flip = Some(ns);
            }
        })?;
        match poll {
            EventPoll::Stopped => return Ok(None),
            EventPoll::TimedOut => return Err(format!("flip event for commit {cookie} timed out (200 ms)").into()),
            EventPoll::Read => {}
        }
    }
    Ok(flip)
}

/// A commit touching two CRTCs completes with one flip event per CRTC, all carrying the
/// same `cookie`. Blocks until the event of every expected CRTC arrives and returns the
/// per-CRTC CLOCK_MONOTONIC ns `[A, B]` (`None` for a CRTC not expected); `None` if stopped.
fn wait_flips_dual(
    file: &mut File,
    cookie: u64,
    crtc_a: u32,
    crtc_b: u32,
    expect_a: bool,
    expect_b: bool,
) -> Result<Option<[Option<u64>; 2]>> {
    let mut flips = [None, None];
    let missing = |flips: &[Option<u64>; 2]| [expect_a && flips[0].is_none(), expect_b && flips[1].is_none()];
    while missing(&flips).contains(&true) {
        let poll = read_flip_events(file, |data, crtc, ns| {
            if data == cookie {
                if crtc == crtc_a {
                    flips[0] = Some(ns);
                } else if crtc == crtc_b {
                    flips[1] = Some(ns);
                }
            }
        })?;
        match poll {
            EventPoll::Stopped => return Ok(None),
            EventPoll::TimedOut => {
                let crtcs: Vec<String> = missing(&flips)
                    .into_iter()
                    .zip([crtc_a, crtc_b])
                    .filter(|&(m, _)| m)
                    .map(|(_, crtc)| crtc.to_string())
                    .collect();
                return Err(format!(
                    "flip event for commit {cookie} on CRTC {} timed out (200 ms)",
                    crtcs.join(", ")
                )
                .into());
            }
            EventPoll::Read => {}
        }
    }
    Ok(Some(flips))
}

struct Options {
    suite: bool,
    regression: bool,
    dual_head: bool,
    list: bool,
    device: Option<String>,
    connector: Option<String>,
    connector_b: Option<String>,
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
        regression: false,
        dual_head: false,
        list: false,
        device: None,
        connector: None,
        connector_b: None,
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
            "--regression" => o.regression = true,
            "--dual-head" => o.dual_head = true,
            "--list" => o.list = true,
            "--device" => o.device = Some(value()?),
            "--connector" => o.connector = Some(value()?),
            "--connector-b" => o.connector_b = Some(value()?),
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
                    let period_ns = frame_period_ns(&m);
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

/// Markdown cells `dt p50 (×T_min) | commit→flip p50 / p95 | delayed / n (%)`.
fn stats_cells(g: &GroupStats, period_ns: u64) -> String {
    if g.n == 0 {
        return "no samples | — | —".into();
    }
    let dt_ms = g.dt_p50_us as f64 / 1000.0;
    format!(
        "{:.2} ms ({:.2}×) | {:.2} / {:.2} ms | {} / {} ({:.1}%)",
        dt_ms,
        dt_ms / (period_ns as f64 / 1e6),
        g.c2f_p50_us as f64 / 1000.0,
        g.c2f_p95_us as f64 / 1000.0,
        g.delayed,
        g.n,
        100.0 * g.delayed as f64 / g.n as f64,
    )
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

fn frame_period_ns(m: &drm_mode_modeinfo) -> u64 {
    (m.htotal as u64 * m.vtotal as u64 * 1_000_000) / m.clock.max(1) as u64
}

/// Planes usable on one CRTC: the first XRGB8888 primary and first ARGB8888 overlay and
/// cursor that are free or already on it (skipping `exclude`), plus every plane on it.
struct CrtcPlanes {
    primary: Plane,
    overlay: Option<Plane>,
    cursor: Option<Plane>,
    active: Vec<u32>,
}

impl CrtcPlanes {
    /// Planes currently on the CRTC other than the primary and `used`.
    fn others(&self, used: &[u32]) -> Vec<u32> {
        self.active.iter().copied().filter(|id| *id != self.primary.id && !used.contains(id)).collect()
    }

    fn ids(&self) -> Vec<u32> {
        [Some(&self.primary), self.overlay.as_ref(), self.cursor.as_ref()].into_iter().flatten().map(|p| p.id).collect()
    }
}

fn crtc_planes(fd: RawFd, crtc_id: u32, crtc_index: usize, exclude: &[u32]) -> Result<CrtcPlanes> {
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
        if exclude.contains(&id) || p.possible_crtcs & (1 << crtc_index) == 0 || (p.crtc_id != 0 && p.crtc_id != crtc_id) {
            continue;
        }
        let props = properties(fd, id, OBJ_PLANE)?;
        if p.crtc_id == crtc_id {
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
    Ok(CrtcPlanes { primary, overlay, cursor, active })
}

/// Turns off every plane in `ids` that has FB_ID and CRTC_ID properties.
fn disable_planes(fd: RawFd, ids: &[u32], commit: &mut Atomic) -> Result<()> {
    for &id in ids {
        let props = properties(fd, id, OBJ_PLANE)?;
        if let (Some((fb, _)), Some((crtc_prop, _))) = (find(&props, "FB_ID"), find(&props, "CRTC_ID")) {
            commit.object(id, &[(fb, 0), (crtc_prop, 0)]);
        }
    }
    Ok(())
}

/// Becomes DRM master and makes SIGINT/SIGTERM stop the run gracefully.
fn take_master(fd: RawFd) -> Result<()> {
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
    Ok(())
}

/// Two overlay buffers (orange, cyan): an opaque 320x120 card on transparent margins.
fn overlay_buffers(fd: RawFd, w: u32, h: u32) -> Result<[Buffer; 2]> {
    let mut b = [Buffer::new(fd, w, h, ARGB8888, 0xffe0_6020)?, Buffer::new(fd, w, h, ARGB8888, 0xff20_c0e0)?];
    for buf in &mut b {
        let (cw, ch) = (buf.width.min(320), buf.height.min(120));
        buf.fill(cw, 0, buf.width - cw, ch, 0);
        buf.fill(0, ch, buf.width, buf.height - ch, 0);
    }
    Ok(b)
}

/// A transparent cursor buffer with an opaque 32x32 `color` square in the top-left corner.
fn cursor_buffer(fd: RawFd, w: u32, h: u32, color: u32) -> Result<Buffer> {
    let mut b = Buffer::new(fd, w, h, ARGB8888, 0)?;
    b.fill(0, 0, w.min(32), h.min(32), color);
    Ok(b)
}

fn edid_desc((name, vrr): &(Option<String>, Option<(u32, u32)>)) -> String {
    match (name, vrr) {
        (Some(n), Some((lo, hi))) => format!(" — `{n}` (EDID `{lo}–{hi} Hz`)"),
        (Some(n), None) => format!(" — `{n}`"),
        (None, Some((lo, hi))) => format!(" (EDID `{lo}–{hi} Hz`)"),
        (None, None) => String::new(),
    }
}

fn opt_str(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "none".into())
}

fn run_suite(o: Options) -> Result<()> {
    let mut disp = open_active_display(o.device.as_deref(), o.connector.as_deref())?;
    let fd = disp.file.as_raw_fd();
    let m = disp.crtc.mode;
    let period_ns = frame_period_ns(&m);
    let refresh_hz = 1e9 / period_ns as f64;
    let pace_hz = o.pace_hz.unwrap_or(refresh_hz * 0.83);
    let pace_ns = (1e9 / pace_hz) as u64;
    let case_secs = o.seconds.unwrap_or(7.0);
    let (width, height) = (m.hdisplay as u32, m.vdisplay as u32);
    let conn_props = properties(fd, disp.connector_id, OBJ_CONNECTOR)?;
    let vrr_capable = find(&conn_props, "vrr_capable").map(|p| p.1);
    let edid = parse_edid(fd, &conn_props);
    let crtc_props = properties(fd, disp.crtc.crtc_id, OBJ_CRTC)?;
    let vrr_prop = find(&crtc_props, "VRR_ENABLED");
    let (gpu_desc, kernel) = system_metadata(&disp.device_path);

    let planes = crtc_planes(fd, disp.crtc.crtc_id, disp.crtc_index, &[])?;
    let others = planes.others(&planes.ids());
    let CrtcPlanes { primary, overlay, cursor, .. } = planes;

    take_master(fd)?;

    let mut game = [
        Buffer::new(fd, width, height, XRGB8888, GRAPH_BACKGROUND)?,
        Buffer::new(fd, width, height, XRGB8888, GRAPH_BACKGROUND)?,
    ];
    let (ow, oh) = (o.overlay_size.0.min(width), o.overlay_size.1.min(height));
    let (ox, oy) = (o.overlay_pos.0.min(width.saturating_sub(ow + 16)), o.overlay_pos.1.min(height.saturating_sub(oh)));
    let overlay_bufs = overlay.as_ref().map(|_| overlay_buffers(fd, ow, oh)).transpose()?;
    let (cw, ch) = cursor_size(fd);
    let (cx, cy) = (0u32, 0u32);
    let cursor_bufs = cursor
        .as_ref()
        .map(|_| -> Result<[Buffer; 2]> { Ok([cursor_buffer(fd, cw, ch, WHITE)?, cursor_buffer(fd, cw, ch, CYAN)?]) })
        .transpose()?;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Update {
        OverlaySwap,
        OverlayMove,
        CursorMove,
        CursorSwap,
    }
    struct CaseSpec {
        label: &'static str,
        update_label: &'static str,
        kind: PatternKind,
        top_half_only: bool,
        update: Update,
        /// Update frames commit only the secondary plane.
        exclusive: bool,
        /// Every frame is an update frame, so the primary never flips after setup.
        idle_primary: bool,
        /// Run with VRR_ENABLED=0 (fixed refresh) instead of the --vrr setting.
        vrr_off: bool,
    }
    let spec = |label, update_label, kind, top_half_only, update, exclusive| CaseSpec {
        label,
        update_label,
        kind,
        top_half_only,
        update,
        exclusive,
        idle_primary: false,
        vrr_off: false,
    };
    let mut cases = vec![
        spec("`solid` (control)", "overlay swap", PatternKind::Solid, false, Update::OverlaySwap, false),
        spec("`hstripes` (control)", "overlay swap", PatternKind::HStripes, false, Update::OverlaySwap, false),
        spec("`vstripes` (full)", "overlay swap", PatternKind::VStripes, false, Update::OverlaySwap, false),
        spec("`vstripes` (top half)", "overlay swap", PatternKind::VStripes, true, Update::OverlaySwap, false),
        spec("`vstripes` (cursor)", "cursor move", PatternKind::VStripes, false, Update::CursorMove, false),
        spec("`vstripes` (`--exclusive`)", "overlay only (1 plane)", PatternKind::VStripes, false, Update::OverlaySwap, true),
    ];
    if o.regression {
        cases.extend([
            spec("`vstripes` (overlay move)", "overlay move", PatternKind::VStripes, false, Update::OverlayMove, false),
            spec("`vstripes` (overlay move `--exclusive`)", "overlay move only (1 plane)", PatternKind::VStripes, false, Update::OverlayMove, true),
            spec("`vstripes` (cursor swap)", "cursor swap", PatternKind::VStripes, false, Update::CursorSwap, false),
            CaseSpec {
                idle_primary: true,
                ..spec("`vstripes` (cursor-only idle)", "cursor move only, every frame", PatternKind::VStripes, false, Update::CursorMove, true)
            },
            CaseSpec {
                vrr_off: true,
                ..spec("`vstripes` (VRR off control, `VRR=0`)", "overlay swap (fixed refresh)", PatternKind::VStripes, false, Update::OverlaySwap, false)
            },
        ]);
    }

    let mut next_cookie = 1u64;
    let primary_fb = primary.prop("FB_ID")?;
    let mut rows: Vec<String> = Vec::new();
    let mut vrr_set_ok = true;
    // VRR_ENABLED currently applied to the CRTC.
    let mut applied_vrr = vrr_prop.map(|p| p.1);

    for (ci, case) in cases.iter().enumerate() {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let use_cursor = matches!(case.update, Update::CursorMove | Update::CursorSwap);
        let sec_plane = if use_cursor { cursor.as_ref() } else { overlay.as_ref() };
        let Some(sec) = sec_plane else {
            let reason = if use_cursor { "no cursor plane" } else { "no overlay plane" };
            rows.push(format!("| {} | {} | skipped ({reason}) | — | — | — |", case.label, case.update_label));
            continue;
        };
        if case.vrr_off && vrr_prop.is_none() {
            rows.push(format!("| {} | {} | skipped (no `VRR_ENABLED` property) | — | — | — |", case.label, case.update_label));
            continue;
        }
        let case_vrr = if case.vrr_off { Some(0) } else { o.vrr.or(vrr_prop.map(|p| p.1)) };

        let pat = Pattern { kind: case.kind, period: 4, on: 3, fg: GREEN, bg: GRAPH_BACKGROUND, rect: None };
        let rect = if case.top_half_only { (0, 0, width, height / 2) } else { (0, 0, width, height) };
        for buf in &mut game {
            buf.fill(0, 0, width, height, GRAPH_BACKGROUND);
            buf.draw_pattern(&pat, rect);
        }

        let mut setup = Atomic::default();
        setup.object(primary.id, &plane_geometry(&primary, disp.crtc.crtc_id, game[0].fb, width, height, 0, 0)?);
        if use_cursor {
            if let Some(ovl) = &overlay {
                setup.object(ovl.id, &disable_plane(ovl)?);
            }
            let cb = &cursor_bufs.as_ref().unwrap()[0];
            setup.object(sec.id, &configure_secondary_state(&primary, sec, disp.crtc.crtc_id, cb.fb, (cx, cy, cw, ch))?);
        } else {
            if let Some(cur) = &cursor {
                setup.object(cur.id, &disable_plane(cur)?);
            }
            let ob = overlay_bufs.as_ref().unwrap();
            setup.object(sec.id, &configure_secondary_state(&primary, sec, disp.crtc.crtc_id, ob[0].fb, (ox, oy, ow, oh))?);
        }
        if ci == 0 && !o.keep_planes {
            disable_planes(fd, &others, &mut setup)?;
        }
        if let (Some(value), Some((id, _))) = (case_vrr, vrr_prop) {
            if (ci == 0 && o.vrr.is_some()) || applied_vrr != Some(value) {
                setup.object(disp.crtc.crtc_id, &[(id, value)]);
            }
        } else if ci == 0 && o.vrr.is_some() {
            vrr_set_ok = false;
        }
        if let Err(e) = setup.commit(fd, TEST_ONLY, 0) {
            rows.push(format!("| {} | {} | skipped (TEST_ONLY: {e}) | — | — | — |", case.label, case.update_label));
            continue;
        }
        let c0 = next_cookie;
        next_cookie += 1;
        setup.commit(fd, NONBLOCK | FLIP_EVENT, c0)?;
        if vrr_prop.is_some() && case_vrr.is_some() {
            applied_vrr = case_vrr;
        }
        let Some(initial_flip) = wait_flip(&mut disp.file, c0)? else {
            break;
        };

        let sec_fb = sec.prop("FB_ID")?;
        let sec_x = sec.prop("CRTC_X")?;
        let every = if case.idle_primary { 1 } else { o.overlay_every };
        let mut frames: Vec<Frame> = Vec::with_capacity((case_secs * pace_hz * 1.1) as usize);
        let start = now_ns();
        let duration = (case_secs * 1e9) as u64;
        let mut next_due = initial_flip + pace_ns;
        let mut frame = 1u64;
        let mut err_msg = None;

        while now_ns() - start < duration && !STOP.load(Ordering::Relaxed) {
            let buffer = &game[(frame % 2) as usize];
            let overlay_flipped = frame.is_multiple_of(every);
            let mut commit = Atomic::default();
            if !(case.exclusive && overlay_flipped) {
                commit.object(primary.id, &[(primary_fb, buffer.fb as u64)]);
            }
            if overlay_flipped {
                let n = frame / every;
                let i = (n % 2) as usize;
                let update = match case.update {
                    Update::OverlaySwap => (sec_fb, overlay_bufs.as_ref().unwrap()[i].fb as u64),
                    Update::CursorSwap => (sec_fb, cursor_bufs.as_ref().unwrap()[i].fb as u64),
                    Update::OverlayMove => (sec_x, ox as u64 + 16 * i as u64),
                    Update::CursorMove => (sec_x, cx as u64 + 16 * i as u64),
                };
                commit.object(sec.id, &[update]);
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
        let primary_only = if pri.n == 0 { "—".to_string() } else { format!("{:.2} ms", pri.c2f_p50_us as f64 / 1000.0) };
        rows.push(format!("| {} | {} | {} | {primary_only} |", case.label, case.update_label, stats_cells(&ovl, period_ns)));
    }

    // Restore: secondary planes off, VRR as found.
    let mut cleanup = Atomic::default();
    if let Some(ovl) = &overlay {
        cleanup.object(ovl.id, &disable_plane(ovl)?);
    }
    if let Some(cur) = &cursor {
        cleanup.object(cur.id, &disable_plane(cur)?);
    }
    if let Some((id, value)) = vrr_prop {
        if o.vrr.is_some() || applied_vrr != Some(value) {
            cleanup.object(disp.crtc.crtc_id, &[(id, value)]);
        }
    }
    let cookie = u64::MAX;
    if cleanup.commit(fd, NONBLOCK | FLIP_EVENT, cookie).is_ok() {
        let _ = wait_flip(&mut disp.file, cookie);
    }

    println!("### System");
    println!("- **GPU:** {gpu_desc}");
    println!("- **Kernel:** `{kernel}`");
    println!("- **Connector:** `{}` (`{}`){}", disp.connector_name, disp.device_path, edid_desc(&edid));
    println!(
        "- **Mode:** `{width}x{height} @ {refresh_hz:.3} Hz` (`T_min = {:.2} ms`, `vrr_capable={}`, `VRR_ENABLED: {} -> {}`)",
        period_ns as f64 / 1e6,
        opt_str(vrr_capable),
        opt_str(vrr_prop.map(|p| p.1)),
        opt_str(o.vrr.or(vrr_prop.map(|p| p.1))),
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
    if o.regression {
        println!();
        println!("- `cursor-only idle`: after setup every commit moves only the cursor (the primary never flips), so there are no primary-only samples (`—`); a flip-event timeout shows as `error (...)`.");
        println!("- `VRR=0` row: fixed refresh at the same pace; `Delayed` there counts ordinary waits for the next fixed vblank, not VRR stalls. `VRR_ENABLED` is restored afterwards.");
    }
    Ok(())
}

/// Two connected connectors driven by distinct active CRTCs of one DRM device: head A is
/// `connector_a` (default: the first, preferring vrr_capable=1), head B is `connector_b`
/// (default: the next active CRTC on the same device).
fn open_dual_displays(
    device: Option<&str>,
    connector_a: Option<&str>,
    connector_b: Option<&str>,
) -> Result<[ActiveDisplay; 2]> {
    let cards = match device {
        Some(path) => vec![path.to_string()],
        None => candidate_cards(),
    };
    let mut notes = Vec::new();
    for path in &cards {
        let found = match probe_card_all(path, None) {
            Ok(found) => found,
            Err(e) if device.is_some() => return Err(e),
            Err(e) => {
                notes.push(format!("{path}: {e}"));
                continue;
            }
        };
        let named = |d: &ActiveDisplay, name: Option<&str>| name.is_none_or(|n| d.connector_name == n);
        let a = found
            .iter()
            .position(|(d, vrr)| named(d, connector_a) && *vrr == Some(1))
            .or_else(|| found.iter().position(|(d, _)| named(d, connector_a)));
        let b = a.and_then(|a| {
            let crtc_a = found[a].0.crtc.crtc_id;
            found.iter().position(|(d, _)| named(d, connector_b) && d.crtc.crtc_id != crtc_a)
        });
        if let (Some(a), Some(b)) = (a, b) {
            // a != b (distinct CRTCs); remove the higher index first so the lower stays valid.
            let mut found = found;
            let high = found.swap_remove(a.max(b)).0;
            let low = found.swap_remove(a.min(b)).0;
            return Ok(if a < b { [low, high] } else { [high, low] });
        }
        let names: Vec<String> = found.iter().map(|(d, _)| format!("{} (CRTC {})", d.connector_name, d.crtc.crtc_id)).collect();
        notes.push(format!("{path}: {} active [{}]", found.len(), names.join(", ")));
    }
    let wanted = format!("A={}, B={}", connector_a.unwrap_or("auto"), connector_b.unwrap_or("auto"));
    let found = if notes.is_empty() { "no /dev/dri/card* devices".to_string() } else { notes.join("; ") };
    Err(format!(
        "--dual-head needs two connected connectors on distinct active CRTCs of one DRM device ({wanted}); found: {found}. \
         Enable a second display in the compositor first, or pick heads with --device / --connector / --connector-b"
    )
    .into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Primary flip every frame plus an overlay buffer swap every K frames.
    Flip,
    /// Static primary; a cursor CRTC_X move every K frames is the head's only update.
    Cursor,
}

impl Role {
    fn label(self, vrr: u64) -> String {
        match self {
            Role::Flip => format!("`VRR={vrr}` primary flip + overlay swap"),
            Role::Cursor => format!("`VRR={vrr}` static primary, cursor move"),
        }
    }
}

/// One head's per-frame commit content: an optional property set every frame (primary
/// flip) and one set every K frames (secondary plane update), each alternating two values.
struct HeadPlan {
    every_frame: Option<(u32, u32, [u64; 2])>,
    on_update: (u32, u32, [u64; 2]),
}

impl HeadPlan {
    /// Adds this head's state for `frame` to `commit`; returns whether the head is part of it.
    fn add(&self, commit: &mut Atomic, frame: u64, every: u64) -> bool {
        if let Some((plane, prop, values)) = self.every_frame {
            commit.object(plane, &[(prop, values[(frame % 2) as usize])]);
        }
        let update = frame.is_multiple_of(every);
        if update {
            let (plane, prop, values) = self.on_update;
            commit.object(plane, &[(prop, values[(frame / every % 2) as usize])]);
        }
        update || self.every_frame.is_some()
    }
}

/// A head of the dual-head run with its planes, full-screen `vstripes` primary buffers and
/// secondary plane buffers.
struct Head {
    disp: ActiveDisplay,
    period_ns: u64,
    size: (u32, u32),
    vrr_capable: Option<u64>,
    /// VRR_ENABLED (property id, value at start); restored at exit.
    vrr_prop: Option<(u32, u64)>,
    edid: (Option<String>, Option<(u32, u32)>),
    primary: Plane,
    game: [Buffer; 2],
    overlay: Option<(Plane, [Buffer; 2])>,
    overlay_rect: (u32, u32, u32, u32),
    cursor: Option<(Plane, Buffer)>,
    cursor_size: (u32, u32),
    others: Vec<u32>,
}

impl Head {
    fn new(fd: RawFd, disp: ActiveDisplay, exclude: &[u32], o: &Options) -> Result<Self> {
        let m = disp.crtc.mode;
        let (w, h) = (m.hdisplay as u32, m.vdisplay as u32);
        let conn_props = properties(fd, disp.connector_id, OBJ_CONNECTOR)?;
        let vrr_prop = find(&properties(fd, disp.crtc.crtc_id, OBJ_CRTC)?, "VRR_ENABLED");
        let planes = crtc_planes(fd, disp.crtc.crtc_id, disp.crtc_index, exclude)
            .map_err(|e| format!("{} (CRTC {}): {e}", disp.connector_name, disp.crtc.crtc_id))?;
        let others = planes.others(&planes.ids());
        let mut game = [
            Buffer::new(fd, w, h, XRGB8888, GRAPH_BACKGROUND)?,
            Buffer::new(fd, w, h, XRGB8888, GRAPH_BACKGROUND)?,
        ];
        let pat = Pattern { kind: PatternKind::VStripes, period: 4, on: 3, fg: GREEN, bg: GRAPH_BACKGROUND, rect: None };
        for buf in &mut game {
            buf.draw_pattern(&pat, (0, 0, w, h));
        }
        let (ow, oh) = (o.overlay_size.0.min(w), o.overlay_size.1.min(h));
        let overlay_rect = (o.overlay_pos.0.min(w.saturating_sub(ow + 16)), o.overlay_pos.1.min(h.saturating_sub(oh)), ow, oh);
        let cursor_size = cursor_size(fd);
        let overlay = planes.overlay.map(|p| -> Result<_> { Ok((p, overlay_buffers(fd, ow, oh)?)) }).transpose()?;
        let cursor = planes
            .cursor
            .map(|p| -> Result<_> { Ok((p, cursor_buffer(fd, cursor_size.0, cursor_size.1, WHITE)?)) })
            .transpose()?;
        Ok(Self {
            period_ns: frame_period_ns(&m),
            size: (w, h),
            vrr_capable: find(&conn_props, "vrr_capable").map(|p| p.1),
            vrr_prop,
            edid: parse_edid(fd, &conn_props),
            primary: planes.primary,
            game,
            overlay,
            overlay_rect,
            cursor,
            cursor_size,
            others,
            disp,
        })
    }

    fn crtc_id(&self) -> u32 {
        self.disp.crtc.crtc_id
    }

    fn plane_ids(&self) -> Vec<u32> {
        let overlay = self.overlay.as_ref().map(|(p, _)| p.id);
        let cursor = self.cursor.as_ref().map(|(p, _)| p.id);
        [Some(self.primary.id), overlay, cursor].into_iter().flatten().collect()
    }

    /// Adds this head's full plane state for `role` and VRR_ENABLED=`vrr` to `commit`.
    fn setup(&self, role: Role, vrr: u64, commit: &mut Atomic) -> Result<HeadPlan> {
        let crtc = self.crtc_id();
        let (w, h) = self.size;
        let missing = |what: &str| format!("`{}` has no {what} plane", self.disp.connector_name);
        commit.object(self.primary.id, &plane_geometry(&self.primary, crtc, self.game[0].fb, w, h, 0, 0)?);
        let plan = match role {
            Role::Flip => {
                let (ovl, bufs) = self.overlay.as_ref().ok_or_else(|| missing("overlay"))?;
                if let Some((cur, _)) = &self.cursor {
                    commit.object(cur.id, &disable_plane(cur)?);
                }
                commit.object(ovl.id, &configure_secondary_state(&self.primary, ovl, crtc, bufs[0].fb, self.overlay_rect)?);
                HeadPlan {
                    every_frame: Some((
                        self.primary.id,
                        self.primary.prop("FB_ID")?,
                        [self.game[0].fb as u64, self.game[1].fb as u64],
                    )),
                    on_update: (ovl.id, ovl.prop("FB_ID")?, [bufs[0].fb as u64, bufs[1].fb as u64]),
                }
            }
            Role::Cursor => {
                let (cur, buf) = self.cursor.as_ref().ok_or_else(|| missing("cursor"))?;
                if let Some((ovl, _)) = &self.overlay {
                    commit.object(ovl.id, &disable_plane(ovl)?);
                }
                let (cw, ch) = self.cursor_size;
                commit.object(cur.id, &configure_secondary_state(&self.primary, cur, crtc, buf.fb, (0, 0, cw, ch))?);
                HeadPlan { every_frame: None, on_update: (cur.id, cur.prop("CRTC_X")?, [0, 16]) }
            }
        };
        if let Some((id, _)) = self.vrr_prop {
            commit.object(crtc, &[(id, vrr)]);
        }
        Ok(plan)
    }

    /// Adds secondary planes off and VRR_ENABLED as found to `commit`.
    fn restore(&self, commit: &mut Atomic) -> Result<()> {
        for plane in [self.overlay.as_ref().map(|(p, _)| p), self.cursor.as_ref().map(|(p, _)| p)].into_iter().flatten() {
            commit.object(plane.id, &disable_plane(plane)?);
        }
        if let Some((id, value)) = self.vrr_prop {
            commit.object(self.crtc_id(), &[(id, value)]);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        let m = self.disp.crtc.mode;
        format!(
            "`{}` (`{}`, CRTC {}){} — `{}x{} @ {:.3} Hz` (`T_min = {:.2} ms`, `vrr_capable={}`, `VRR_ENABLED` at start: {}, restored at exit)",
            self.disp.connector_name,
            self.disp.device_path,
            self.crtc_id(),
            edid_desc(&self.edid),
            m.hdisplay,
            m.vdisplay,
            1e9 / self.period_ns as f64,
            self.period_ns as f64 / 1e6,
            opt_str(self.vrr_capable),
            opt_str(self.vrr_prop.map(|p| p.1)),
        )
    }
}

fn run_dual_head(o: Options) -> Result<()> {
    let [disp_a, disp_b] = open_dual_displays(o.device.as_deref(), o.connector.as_deref(), o.connector_b.as_deref())?;
    // Both displays share one open file description: one DRM master, one event queue.
    let mut file = disp_a.file.try_clone()?;
    let fd = file.as_raw_fd();
    let (gpu_desc, kernel) = system_metadata(&disp_a.device_path);
    take_master(fd)?;
    let head_a = Head::new(fd, disp_a, &[], &o)?;
    let head_b = Head::new(fd, disp_b, &head_a.plane_ids(), &o)?;
    let heads = [head_a, head_b];
    let (crtc_a, crtc_b) = (heads[0].crtc_id(), heads[1].crtc_id());

    let slow_period_ns = heads[0].period_ns.max(heads[1].period_ns);
    let slow_refresh_hz = 1e9 / slow_period_ns as f64;
    let pace_hz = o.pace_hz.unwrap_or(slow_refresh_hz * 0.83);
    let pace_ns = (1e9 / pace_hz) as u64;
    let case_secs = o.seconds.unwrap_or(7.0);
    let every = o.overlay_every;

    struct DualCase {
        label: &'static str,
        a: Role,
        b: Role,
        vrr_b: u64,
    }
    let cases = [
        DualCase { label: "dual: both VRR flips", a: Role::Flip, b: Role::Flip, vrr_b: 1 },
        DualCase { label: "dual: VRR A flip + VRR B cursor", a: Role::Flip, b: Role::Cursor, vrr_b: 1 },
        DualCase { label: "dual: VRR A cursor + VRR B flip", a: Role::Cursor, b: Role::Flip, vrr_b: 1 },
        DualCase { label: "dual: VRR A + fixed B", a: Role::Flip, b: Role::Flip, vrr_b: 0 },
    ];

    let mut rows: Vec<String> = Vec::new();
    let mut next_cookie = 1u64;
    let mut planes_cleared = o.keep_planes;
    for case in &cases {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let prefix = format!("| {} | {} | {} |", case.label, case.a.label(1), case.b.label(case.vrr_b));
        let mut setup = Atomic::default();
        let plans = heads[0]
            .setup(case.a, 1, &mut setup)
            .and_then(|a| Ok([a, heads[1].setup(case.b, case.vrr_b, &mut setup)?]));
        let plans = match plans {
            Ok(plans) => plans,
            Err(e) => {
                rows.push(format!("{prefix} skipped ({e}) | — | — | — | — | — |"));
                continue;
            }
        };
        if !planes_cleared {
            for head in &heads {
                disable_planes(fd, &head.others, &mut setup)?;
            }
        }
        if let Err(e) = setup.commit(fd, TEST_ONLY, 0) {
            rows.push(format!("{prefix} skipped (TEST_ONLY: {e}) | — | — | — | — | — |"));
            continue;
        }
        let c0 = next_cookie;
        next_cookie += 1;
        if let Err(e) = setup.commit(fd, NONBLOCK | FLIP_EVENT, c0) {
            rows.push(format!("{prefix} error (setup commit: {e}) | — | — | — | — | — |"));
            continue;
        }
        planes_cleared = true;
        let initial = match wait_flips_dual(&mut file, c0, crtc_a, crtc_b, true, true) {
            Ok(Some(flips)) => flips.into_iter().flatten().max().unwrap_or_else(now_ns),
            Ok(None) => break,
            Err(e) => {
                rows.push(format!("{prefix} error (setup: {e}) | — | — | — | — | — |"));
                continue;
            }
        };

        let capacity = (case_secs * pace_hz * 1.1) as usize;
        let mut frames: [Vec<Frame>; 2] = [Vec::with_capacity(capacity), Vec::with_capacity(capacity)];
        let start = now_ns();
        let duration = (case_secs * 1e9) as u64;
        let mut next_due = initial + pace_ns;
        let mut frame = 1u64;
        let mut err_msg = None;
        while now_ns() - start < duration && !STOP.load(Ordering::Relaxed) {
            let mut commit = Atomic::default();
            let expect = [plans[0].add(&mut commit, frame, every), plans[1].add(&mut commit, frame, every)];
            let overlay_flipped = frame.is_multiple_of(every);
            sleep_until(next_due);
            let cookie = next_cookie;
            next_cookie += 1;
            let commit_ns = now_ns();
            if let Err(e) = commit.commit(fd, NONBLOCK | FLIP_EVENT, cookie) {
                err_msg = Some(format!("frame {frame}: commit error: {e}"));
                break;
            }
            match wait_flips_dual(&mut file, cookie, crtc_a, crtc_b, expect[0], expect[1]) {
                Ok(Some(flips)) => {
                    let mut latest = 0;
                    for (head_frames, flip) in frames.iter_mut().zip(flips) {
                        if let Some(flip_ns) = flip {
                            head_frames.push(Frame { overlay_flipped, draw_ns: 0, commit_ns, flip_ns });
                            latest = latest.max(flip_ns);
                        }
                    }
                    // Same rebase as --suite, driven by the later of the two flips.
                    if latest > next_due + 1_000_000 {
                        next_due = latest + pace_ns;
                    } else {
                        next_due += pace_ns;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    err_msg = Some(format!("frame {frame}: {e}"));
                    break;
                }
            }
            frame += 1;
        }

        if let Some(err) = err_msg {
            rows.push(format!("{prefix} error ({err}) | — | — | — | — | — |"));
            continue;
        }
        let warmup = if case_secs >= 3.0 { 1_000_000_000 } else { 0 };
        let cells: Vec<String> = heads
            .iter()
            .zip(&frames)
            .map(|(head, frames)| stats_cells(&compute_stats(frames, pace_ns, head.period_ns, warmup)[1], head.period_ns))
            .collect();
        rows.push(format!("{prefix} {} | {} |", cells[0], cells[1]));
    }

    let mut cleanup = Atomic::default();
    for head in &heads {
        head.restore(&mut cleanup)?;
    }
    let cookie = u64::MAX;
    if cleanup.commit(fd, NONBLOCK | FLIP_EVENT, cookie).is_ok() {
        let _ = wait_flips_dual(&mut file, cookie, crtc_a, crtc_b, true, true);
    }

    println!("### System");
    println!("- **GPU:** {gpu_desc}");
    println!("- **Kernel:** `{kernel}`");
    println!("- **Head A:** {}", heads[0].describe());
    println!("- **Head B:** {}", heads[1].describe());
    println!(
        "- **Dual pace:** `{pace_hz:.2} Hz` (`{:.2} ms`, {:.0}% of the slower head's refresh), plane update every {every} frames, {case_secs:.0} s/case; every frame is one `NONBLOCK | FLIP_EVENT` atomic commit spanning both CRTCs (one flip event per CRTC)",
        pace_ns as f64 / 1e6,
        100.0 * pace_hz / slow_refresh_hz,
    );
    for head in &heads {
        if head.vrr_capable == Some(0) || head.vrr_prop.is_none() {
            println!(
                "\n> **Warning:** VRR cannot be enabled on `{}` (`vrr_capable={}`, `VRR_ENABLED` available={}). Its `VRR=1` rows reflect fixed-refresh timing.",
                head.disp.connector_name,
                opt_str(head.vrr_capable),
                head.vrr_prop.is_some(),
            );
        }
    }
    println!();
    println!("| Case | Head A | Head B | A: `dt` p50 (×T_min) | A: `commit→flip` p50 / p95 | A: Delayed (>1 ms) | B: `dt` p50 (×T_min) | B: `commit→flip` p50 / p95 | B: Delayed (>1 ms) |");
    println!("|---|---|---|---:|---:|---:|---:|---:|---:|");
    for r in &rows {
        println!("{r}");
    }
    println!();
    println!("- Per-head stats cover the frame after each plane-update frame, as in `--suite`; `T_min` is that head's own.");
    println!("- A cursor head is in only every {every}th commit (cursor `CRTC_X` move, no primary flip), so its `dt` is ≈ {every} × pace.");
    println!("- `VRR=0` head: fixed refresh; `Delayed` there counts ordinary waits for the next fixed vblank, not VRR stalls.");
    Ok(())
}

fn run(o: Options) -> Result<()> {
    if o.list {
        return list_displays(o.device.as_deref());
    }
    if o.dual_head {
        return run_dual_head(o);
    }
    if o.suite || o.regression {
        return run_suite(o);
    }
    let mut disp = open_active_display(o.device.as_deref(), o.connector.as_deref())?;
    let fd = disp.file.as_raw_fd();
    let (connector_id, connector_name, crtc, crtc_index) =
        (disp.connector_id, disp.connector_name, disp.crtc, disp.crtc_index);
    let m = crtc.mode;
    let period_ns = frame_period_ns(&m);
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

    let planes = crtc_planes(fd, crtc.crtc_id, crtc_index, &[])?;
    let overlay = if o.update_plane_type == 2 {
        planes.cursor.as_ref().ok_or("no cursor plane with ARGB8888")?
    } else {
        planes.overlay.as_ref().ok_or("no overlay plane with ARGB8888")?
    };
    let others = planes.others(&[overlay.id]);
    let primary = &planes.primary;
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

    take_master(fd)?;

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
    setup.object(primary.id, &plane_geometry(primary, crtc.crtc_id, game[0].fb, width, height, 0, 0)?);
    setup.object(overlay.id, &configure_secondary_state(primary, overlay, crtc.crtc_id, notification[0].fb, (ox, oy, ow, oh))?);
    if !o.keep_planes {
        disable_planes(fd, &others, &mut setup)?;
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
    cleanup.object(overlay.id, &disable_plane(overlay)?);
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
