//! Drawing on a `u32` XRGB pixel buffer, always through a clip rectangle.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn intersect(&self, o: Rect) -> Rect {
        let (x, y) = (self.x.max(o.x), self.y.max(o.y));
        Rect {
            x,
            y,
            w: (self.right().min(o.right()) - x).max(0),
            h: (self.bottom().min(o.bottom()) - y).max(0),
        }
    }

    pub fn union(&self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return *self;
        }
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Rect {
            x,
            y,
            w: self.right().max(o.right()) - x,
            h: self.bottom().max(o.bottom()) - y,
        }
    }

    pub fn intersects(&self, o: Rect) -> bool {
        !self.intersect(o).is_empty()
    }
}

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    pub w: i32,
    pub h: i32,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(px: &'a mut [u32], w: i32, h: i32) -> Self {
        debug_assert!(px.len() >= (w * h) as usize);
        Self {
            px,
            w,
            h,
            clip: Rect::new(0, 0, w, h),
        }
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Sets the clip, always within the buffer.
    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(Rect::new(0, 0, self.w, self.h));
    }

    pub fn fill(&mut self, r: Rect, color: u32) {
        let r = r.intersect(self.clip);
        if r.is_empty() {
            return;
        }
        for y in r.y..r.bottom() {
            let start = (y * self.w + r.x) as usize;
            self.px[start..start + r.w as usize].fill(color);
        }
    }

    /// A see-through rectangle over what's there. `a` is 0 to 255.
    pub fn tint(&mut self, r: Rect, color: u32, a: u32) {
        let r = r.intersect(self.clip);
        if r.is_empty() {
            return;
        }
        for y in r.y..r.bottom() {
            let start = (y * self.w + r.x) as usize;
            for p in &mut self.px[start..start + r.w as usize] {
                *p = blend(*p, color, a);
            }
        }
    }

    /// A filled rectangle with rounded corners, antialiased.
    pub fn rrect(&mut self, r: Rect, radius: i32, color: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        if radius == 0 {
            self.fill(r, color);
            return;
        }
        self.fill(Rect::new(r.x, r.y + radius, r.w, r.h - radius * 2), color);
        for i in 0..radius {
            // Distance from the corner's centre, sampled at the pixel centre.
            let dy = radius as f32 - i as f32 - 0.5;
            let reach = ((radius * radius) as f32 - dy * dy).max(0.0).sqrt();
            let inset = radius as f32 - reach;
            let solid = inset.ceil() as i32;
            let cover = (solid as f32 - inset).clamp(0.0, 1.0);
            for y in [r.y + i, r.bottom() - 1 - i] {
                self.fill(Rect::new(r.x + solid, y, r.w - solid * 2, 1), color);
                if solid > 0 && cover > 0.0 {
                    let a = (cover * 255.0) as u32;
                    self.blend_px(r.x + solid - 1, y, color, a);
                    self.blend_px(r.right() - solid, y, color, a);
                }
            }
        }
    }

    fn blend_px(&mut self, x: i32, y: i32, color: u32, a: u32) {
        if self.clip.contains(x, y) {
            let p = &mut self.px[(y * self.w + x) as usize];
            *p = blend(*p, color, a);
        }
    }

    /// Outline of a rectangle, one `t` thick, drawn inside it.
    pub fn frame(&mut self, r: Rect, t: i32, color: u32) {
        self.fill(Rect::new(r.x, r.y, r.w, t), color);
        self.fill(Rect::new(r.x, r.bottom() - t, r.w, t), color);
        self.fill(Rect::new(r.x, r.y + t, t, r.h - t * 2), color);
        self.fill(Rect::new(r.right() - t, r.y + t, t, r.h - t * 2), color);
    }

    /// Draws an 8-bit coverage mask in a solid color.
    pub fn mask(&mut self, mask: &[u8], mw: i32, mh: i32, x: i32, y: i32, color: u32) {
        let area = Rect::new(x, y, mw, mh).intersect(self.clip);
        if area.is_empty() {
            return;
        }
        let row_len = area.w as usize;
        for row in area.y..area.bottom() {
            let m = (row - y) as usize * mw as usize + (area.x - x) as usize;
            let d = (row * self.w + area.x) as usize;
            let dst = &mut self.px[d..d + row_len];
            let src = &mask[m..m + row_len];
            for (p, &a) in dst.iter_mut().zip(src) {
                if a == 255 {
                    *p = color;
                } else if a != 0 {
                    *p = blend(*p, color, a as u32);
                }
            }
        }
    }

    /// Draws RGBA pixels (straight alpha), as color emoji come.
    pub fn rgba(&mut self, data: &[u8], mw: i32, mh: i32, x: i32, y: i32) {
        let area = Rect::new(x, y, mw, mh).intersect(self.clip);
        for row in area.y..area.bottom() {
            for col in area.x..area.right() {
                let m = ((row - y) * mw + (col - x)) as usize * 4;
                let c = (data[m] as u32) << 16 | (data[m + 1] as u32) << 8 | data[m + 2] as u32;
                let p = &mut self.px[(row * self.w + col) as usize];
                *p = blend(*p, c, data[m + 3] as u32);
            }
        }
    }

    /// Draws pixels already in this canvas's format, as video frames come. A plain copy when
    /// they're the right size, and the nearest pixel if not.
    pub fn xrgb(&mut self, data: &[u32], sw: i32, sh: i32, dest: Rect) {
        let area = dest.intersect(self.clip);
        if area.is_empty() || sw <= 0 || sh <= 0 || data.len() < sw as usize * sh as usize {
            return;
        }
        let stride = sw as usize;
        if (dest.w, dest.h) == (sw, sh) {
            for row in area.y..area.bottom() {
                let from = (row - dest.y) as usize * stride + (area.x - dest.x) as usize;
                let to = (row * self.w + area.x) as usize;
                self.px[to..to + area.w as usize]
                    .copy_from_slice(&data[from..from + area.w as usize]);
            }
            return;
        }
        let sample = |at: i32, from: i32, to: i32| {
            ((at as i64 * 2 + 1) * from as i64 / (to as i64 * 2)) as usize
        };
        let columns: Vec<usize> = (area.x..area.right())
            .map(|x| sample(x - dest.x, sw, dest.w))
            .collect();
        for row in area.y..area.bottom() {
            let line = &data[sample(row - dest.y, sh, dest.h) * stride..][..stride];
            let to = (row * self.w + area.x) as usize;
            for (p, &column) in self.px[to..to + area.w as usize].iter_mut().zip(&columns) {
                *p = line[column];
            }
        }
    }

    /// Draws RGBA pixels (straight alpha) stretched to fill `dest`, taking the nearest pixel.
    /// Costs what's visible, however big the image is.
    pub fn image(&mut self, data: &[u8], sw: i32, sh: i32, dest: Rect) {
        let area = dest.intersect(self.clip);
        if area.is_empty() || sw <= 0 || sh <= 0 || data.len() < sw as usize * sh as usize * 4 {
            return;
        }
        let sample = |at: i32, from: i32, to: i32| {
            ((at as i64 * 2 + 1) * from as i64 / (to as i64 * 2)) as usize
        };
        let columns: Vec<usize> = (area.x..area.right())
            .map(|x| sample(x - dest.x, sw, dest.w) * 4)
            .collect();
        let stride = sw as usize * 4;
        for row in area.y..area.bottom() {
            let line = &data[sample(row - dest.y, sh, dest.h) * stride..][..stride];
            let d = (row * self.w + area.x) as usize;
            for (p, &column) in self.px[d..d + area.w as usize].iter_mut().zip(&columns) {
                let [r, g, b, a] = [
                    line[column],
                    line[column + 1],
                    line[column + 2],
                    line[column + 3],
                ];
                let color = (r as u32) << 16 | (g as u32) << 8 | b as u32;
                match a {
                    255 => *p = color,
                    0 => {}
                    a => *p = blend(*p, color, a as u32),
                }
            }
        }
    }
}

/// Blends two XRGB colors. `a` is 0 to 255.
#[inline]
pub fn blend(dst: u32, src: u32, a: u32) -> u32 {
    // Widened so weights run 0 to 256 and the shift is exact at both ends.
    let a = a + (a >> 7);
    let ia = 256 - a;
    let rb = (((src & 0xff00ff) * a + (dst & 0xff00ff) * ia) >> 8) & 0xff00ff;
    let g = (((src & 0x00ff00) * a + (dst & 0x00ff00) * ia) >> 8) & 0x00ff00;
    rb | g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects_intersect_and_union() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(Rect::new(5, 5, 10, 10)), Rect::new(5, 5, 5, 5));
        assert!(a.intersect(Rect::new(20, 0, 5, 5)).is_empty());
        assert_eq!(a.union(Rect::new(20, 0, 5, 5)), Rect::new(0, 0, 25, 10));
        assert_eq!(Rect::default().union(a), a);
    }

    #[test]
    fn drawing_stays_inside_the_clip_and_buffer() {
        let mut px = vec![0u32; 100];
        let mut c = Canvas::new(&mut px, 10, 10);
        c.set_clip(Rect::new(2, 2, 2, 2));
        c.fill(Rect::new(-100, -100, 1000, 1000), 1);
        c.mask(&[255; 9], 3, 3, 3, 3, 2);
        c.rrect(Rect::new(0, 0, 10, 10), 3, 3);
        c.frame(Rect::new(-5, -5, 500, 500), 1, 4);
        c.rgba(&[255; 36], 3, 3, 8, 8);
        for y in 0..10 {
            for x in 0..10 {
                let inside = (2..4).contains(&x) && (2..4).contains(&y);
                assert_eq!(px[y * 10 + x] != 0, inside, "{x},{y}");
            }
        }
    }

    #[test]
    fn set_clip_cant_escape_the_buffer() {
        let mut px = vec![0u32; 16];
        let mut c = Canvas::new(&mut px, 4, 4);
        c.set_clip(Rect::new(-5, -5, 100, 100));
        c.fill(Rect::new(-5, -5, 100, 100), 7);
        assert!(px.iter().all(|&p| p == 7));
    }

    #[test]
    fn blend_endpoints_are_exact_enough() {
        assert_eq!(blend(0x123456, 0xabcdef, 0), 0x123456);
        assert_eq!(blend(0x123456, 0xabcdef, 255), 0xabcdef);
        assert_eq!(blend(0x000000, 0xffffff, 128) & 0xff, 128);
        assert_eq!(blend(0xffffff, 0x000000, 0), 0xffffff);
    }

    #[test]
    fn rounded_rect_fills_the_middle_and_cuts_corners() {
        let mut px = vec![0u32; 400];
        let mut c = Canvas::new(&mut px, 20, 20);
        c.rrect(Rect::new(0, 0, 20, 20), 6, 0xffffff);
        assert_eq!(px[0], 0);
        assert_eq!(px[10 * 20 + 10], 0xffffff);
        assert_eq!(px[10], 0xffffff);
    }

    #[test]
    fn images_scale_clip_and_keep_their_alpha() {
        let mut px = vec![0x10_u32; 16];
        let mut c = Canvas::new(&mut px, 4, 4);
        // Red, then half-clear white: doubled, and with the top row and right edge clipped.
        let data = [255, 0, 0, 255, 255, 255, 255, 0];
        c.set_clip(Rect::new(0, 1, 3, 3));
        c.image(&data, 2, 1, Rect::new(0, 0, 4, 2));
        assert_eq!(px[0], 0x10, "above the clip");
        assert_eq!(px[4], 0xff0000);
        assert_eq!(px[5], 0xff0000);
        assert_eq!(px[6], 0x10, "fully clear pixels leave what's under them");
        assert_eq!(px[8], 0x10, "below the image");
    }

    #[test]
    fn images_refuse_a_buffer_smaller_than_they_claim() {
        let mut px = vec![0_u32; 4];
        let mut c = Canvas::new(&mut px, 2, 2);
        c.image(&[255; 4], 2, 2, Rect::new(0, 0, 2, 2));
        c.image(&[255; 4], 0, 0, Rect::new(0, 0, 2, 2));
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn video_frames_copy_scale_clip_and_refuse_short_buffers() {
        let frame = [1_u32, 2, 3, 4];
        let mut px = vec![0_u32; 16];
        let mut c = Canvas::new(&mut px, 4, 4);
        c.xrgb(&frame, 2, 2, Rect::new(1, 1, 2, 2));
        assert_eq!(px[5..7], [1, 2]);
        assert_eq!(px[9..11], [3, 4]);
        let mut px = vec![0_u32; 16];
        let mut c = Canvas::new(&mut px, 4, 4);
        c.xrgb(&frame, 2, 2, Rect::new(0, 0, 4, 4));
        assert_eq!(px, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
        let mut px = vec![0_u32; 4];
        let mut c = Canvas::new(&mut px, 2, 2);
        c.set_clip(Rect::new(1, 1, 1, 1));
        c.xrgb(&frame, 2, 2, Rect::new(-1, -1, 2, 2));
        c.xrgb(&frame, 2, 2, Rect::new(0, 0, 2, 2));
        assert_eq!(px, [0, 0, 0, 4]);
        let mut px = vec![0_u32; 4];
        let mut c = Canvas::new(&mut px, 2, 2);
        c.xrgb(&frame[..3], 2, 2, Rect::new(0, 0, 2, 2));
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn images_far_off_the_buffer_draw_nothing() {
        let mut px = vec![0_u32; 4];
        let mut c = Canvas::new(&mut px, 2, 2);
        c.image(&[255; 4], 1, 1, Rect::new(-500, -500, 100, 100));
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn zero_or_negative_sizes_draw_nothing() {
        let mut px = vec![0u32; 16];
        let mut c = Canvas::new(&mut px, 4, 4);
        c.fill(Rect::new(1, 1, 0, 3), 1);
        c.fill(Rect::new(1, 1, -2, 3), 1);
        c.rrect(Rect::new(1, 1, 0, 0), 4, 1);
        c.mask(&[], 0, 0, 1, 1, 1);
        assert!(px.iter().all(|&p| p == 0));
    }
}
