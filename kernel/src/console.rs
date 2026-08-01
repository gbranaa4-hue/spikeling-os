//! MILESTONE 7: a real text console rendered on the framebuffer,
//! combining Milestone 2 (pixel output) and Milestone 6 (keyboard
//! input) -- every keystroke decoded by keyboard.rs gets drawn here
//! live, so typing is actually visible on screen, not just captured
//! into a string read back over serial.
//!
//! MILESTONE 23: real graphics primitives (pixel/line/rectangle) on
//! top of the same framebuffer the text console already owns --
//! drawing addresses raw pixel coordinates directly rather than the
//! text cursor's x_pos/y_pos, so shapes and text can coexist on
//! screen without either one disturbing the other.
//!
//! MILESTONE 29: exposes the framebuffer's real pixel dimensions so
//! mouse.rs's drawing mode can clamp the mouse's unbounded relative-
//! movement accumulator to actual on-screen coordinates before handing
//! them to draw_line/draw_pixel.

use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, RasterizedChar, get_raster, get_raster_width};
use spin::Mutex;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const CHAR_RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;
const BACKUP_CHAR: char = '?';
const LINE_SPACING: usize = 2;
const LETTER_SPACING: usize = 0;
const BORDER_PADDING: usize = 4;

fn get_char_raster(c: char) -> RasterizedChar {
    get_raster(c, FONT_WEIGHT, CHAR_RASTER_HEIGHT)
        .unwrap_or_else(|| get_raster(BACKUP_CHAR, FONT_WEIGHT, CHAR_RASTER_HEIGHT).expect("backup char must render"))
}

pub struct Console {
    framebuffer: &'static mut [u8],
    info: FrameBufferInfo,
    x_pos: usize,
    y_pos: usize,
}

impl Console {
    pub fn new(framebuffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        let mut console = Self {
            framebuffer,
            info,
            x_pos: 0,
            y_pos: 0,
        };
        console.clear();
        console
    }

    pub fn clear(&mut self) {
        self.x_pos = BORDER_PADDING;
        self.y_pos = BORDER_PADDING;
        self.framebuffer.fill(0);
    }

    fn newline(&mut self) {
        self.y_pos += CHAR_RASTER_HEIGHT.val() + LINE_SPACING;
        self.carriage_return();
    }

    fn carriage_return(&mut self) {
        self.x_pos = BORDER_PADDING;
    }

    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.carriage_return(),
            c => {
                let new_xpos = self.x_pos + get_raster_width(FONT_WEIGHT, CHAR_RASTER_HEIGHT);
                if new_xpos >= self.info.width {
                    self.newline();
                }
                let new_ypos = self.y_pos + CHAR_RASTER_HEIGHT.val() + BORDER_PADDING;
                if new_ypos >= self.info.height {
                    self.clear();
                }
                self.write_rendered_char(get_char_raster(c));
            }
        }
    }

    fn write_rendered_char(&mut self, rendered_char: RasterizedChar) {
        self.draw_rendered_char(&rendered_char);
        self.x_pos += rendered_char.width() + LETTER_SPACING;
    }

    fn draw_rendered_char(&mut self, rendered_char: &RasterizedChar) {
        for (y, row) in rendered_char.raster().iter().enumerate() {
            for (x, byte) in row.iter().enumerate() {
                self.write_pixel(self.x_pos + x, self.y_pos + y, *byte);
            }
        }
    }

    /// MILESTONE 8: moves back one character cell and paints over it
    /// with a blank glyph -- doesn't track wrapping across a newline
    /// (backspacing past a line boundary is out of scope here), a
    /// deliberate simplification, not an oversight.
    pub fn backspace(&mut self) {
        let char_width = get_raster_width(FONT_WEIGHT, CHAR_RASTER_HEIGHT);
        if self.x_pos >= BORDER_PADDING + char_width {
            self.x_pos -= char_width;
            self.draw_rendered_char(&get_char_raster(' '));
        }
    }

    fn write_pixel(&mut self, x: usize, y: usize, intensity: u8) {
        let pixel_offset = y * self.info.stride + x;
        let bpp = self.info.bytes_per_pixel;
        let byte_offset = pixel_offset * bpp;
        if byte_offset + bpp > self.framebuffer.len() {
            return;
        }
        match self.info.pixel_format {
            PixelFormat::Rgb | PixelFormat::Bgr => {
                // both channel orders are fine here since it's a pure
                // white-on-black grayscale render (equal R/G/B) -- the
                // BGR-vs-RGB swap that mattered for Milestone 2's
                // colored gradient makes no visible difference when
                // every channel carries the same intensity value
                self.framebuffer[byte_offset] = intensity;
                self.framebuffer[byte_offset + 1] = intensity;
                self.framebuffer[byte_offset + 2] = intensity;
            }
            PixelFormat::U8 => {
                self.framebuffer[byte_offset] = intensity;
            }
            _ => {}
        }
    }

    /// MILESTONE 23: sets a single pixel at raw framebuffer coordinates,
    /// independent of the text cursor -- reuses write_pixel's existing
    /// bounds check and pixel-format branching rather than duplicating
    /// them, matching the text renderer's 0=off/255=on convention.
    pub fn draw_pixel(&mut self, x: usize, y: usize, on: bool) {
        self.write_pixel(x, y, if on { 255 } else { 0 });
    }

    /// MILESTONE 23: Bresenham's line algorithm, the standard integer-only
    /// way to rasterize an arbitrary line without floating point -- works
    /// for any slope/direction since the coordinates are widened to isize
    /// for the signed step arithmetic, then only used as usize once each
    /// point is known to be non-negative.
    pub fn draw_line(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        let (mut x0, mut y0) = (x0 as isize, y0 as isize);
        let (x1, y1) = (x1 as isize, y1 as isize);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.draw_pixel(x0 as usize, y0 as usize, true);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// MILESTONE 23: outline (unfilled) or filled rectangle, both built
    /// out of draw_line/draw_pixel rather than a separate framebuffer
    /// write path.
    pub fn draw_rect(&mut self, x: usize, y: usize, width: usize, height: usize, filled: bool) {
        if width == 0 || height == 0 {
            return;
        }
        let x1 = x + width - 1;
        let y1 = y + height - 1;
        if filled {
            for row in y..=y1 {
                self.draw_line(x, row, x1, row);
            }
        } else {
            self.draw_line(x, y, x1, y);
            self.draw_line(x, y1, x1, y1);
            self.draw_line(x, y, x, y1);
            self.draw_line(x1, y, x1, y1);
        }
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

pub fn init(framebuffer: &'static mut [u8], info: FrameBufferInfo) {
    *CONSOLE.lock() = Some(Console::new(framebuffer, info));
}

pub fn write_char(c: char) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.write_char(c);
    }
}

pub fn write_str(s: &str) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        for c in s.chars() {
            console.write_char(c);
        }
    }
}

pub fn backspace() {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.backspace();
    }
}

pub fn clear_screen() {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.clear();
    }
}

/// MILESTONE 29: the framebuffer's real (width, height) in pixels, or
/// None before the console is initialized -- lets callers outside this
/// module (mouse.rs's drawing mode) clamp coordinates to the real
/// screen instead of duplicating FrameBufferInfo lookup logic.
pub fn dimensions() -> Option<(usize, usize)> {
    CONSOLE.lock().as_ref().map(|c| (c.info.width, c.info.height))
}

pub fn draw_pixel(x: usize, y: usize, on: bool) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.draw_pixel(x, y, on);
    }
}

pub fn draw_line(x0: usize, y0: usize, x1: usize, y1: usize) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.draw_line(x0, y0, x1, y1);
    }
}

pub fn draw_rect(x: usize, y: usize, width: usize, height: usize, filled: bool) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.draw_rect(x, y, width, height, filled);
    }
}
