//! Drawing the window. Each part checks the clip first, so a small change repaints little.

use gannet_core::listing::{Kind, SortKey, Stat};
use gannet_core::places::PlaceKind;
use gannet_core::theme::blend;
use gannet_core::time;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::app::{ActiveDrag, App, Dialog, DragGhost, Edit, FileDrag, Hit, icon_for};
use crate::canvas::{Canvas, Rect};
use crate::field::Field;
use crate::format;
use crate::icons::Icon;

impl App {
    pub fn paint(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let clip = c.clip();
        if clip.intersects(l.list) {
            self.paint_list(c);
        }
        if clip.intersects(l.header) {
            self.paint_header(c);
        }
        if clip.intersects(l.sidebar) {
            self.paint_sidebar(c);
        }
        if clip.intersects(l.toolbar) {
            self.paint_toolbar(c);
        }
        if clip.intersects(l.notices) {
            self.paint_notices(c);
        }
        if clip.intersects(l.status) {
            self.paint_status(c);
        }
        if let Some(FileDrag::Band(b)) = &self.file_drag
            && !b.rect.is_empty()
        {
            let accent = self.colors.accent;
            c.tint(b.rect, accent, 48);
            c.frame(b.rect, 1, accent);
        }
        if self.menu.is_some() {
            self.paint_menu(c);
        }
        if self.preview.is_some() {
            self.paint_preview(c);
        }
        if self.dialog.is_some() {
            self.paint_dialog(c);
        }
        self.paint_drag_label(c);
    }

    /// The target a drag is over, if it's this row or place.
    fn is_drop_target(&self, hit: Hit) -> bool {
        matches!(&self.file_drag, Some(FileDrag::Active(ActiveDrag { target: Some(t), .. })) if t.hit == hit) || self.incoming.as_ref().and_then(|i| i.target.as_ref()).is_some_and(|t| t.hit == hit)
    }

    /// What a drag carries: its row, as it looks in the list, with a stack and a count if
    /// it's several.
    pub(crate) fn paint_ghost(&mut self, c: &mut Canvas, g: &DragGhost, r: Rect) {
        let co = self.colors;
        let radius = self.px(5.0);
        let stack = if g.count > 1 { self.px(4.0) } else { 0 };
        let row = Rect::new(r.x, r.y, r.w - stack, r.h - stack);
        if stack > 0 {
            let behind = Rect::new(r.x + stack, r.y + stack, row.w, row.h);
            c.rrect(behind, radius, co.border);
        }
        c.rrect(row, radius, co.accent);
        let inset = self.px(1.0);
        c.rrect(Rect::new(row.x + inset, row.y + inset, row.w - inset * 2, row.h - inset * 2), radius, co.selection);
        let icon = self.px(16.0);
        let tint = if g.kind.is_dirlike() { co.on_selection } else { blend(co.on_selection, co.selection, 0.25) };
        self.icons.draw(c, icon_for(g.kind, &g.name), row.x + self.px(10.0), row.y + (row.h - icon) / 2, icon, tint);
        let badge = g.count.to_string();
        let badge_w = if g.count > 1 { self.text.measure(&badge).ceil() as i32 + self.px(12.0) } else { 0 };
        let tx = (row.x + self.px(34.0)) as f32;
        let right = row.right() - self.px(8.0) - if badge_w > 0 { badge_w + self.px(6.0) } else { 0 };
        let by = self.baseline(row.y, row.h);
        let name = String::from_utf8_lossy(&g.name).into_owned();
        self.text.draw_fitted(c, &name, tx, by, right as f32 - tx, co.on_selection);
        if badge_w > 0 {
            let h = self.px(18.0);
            let pill = Rect::new(row.right() - self.px(6.0) - badge_w, row.y + (row.h - h) / 2, badge_w, h);
            c.rrect(pill, h / 2, co.accent);
            self.text.draw_str(c, &badge, (pill.x + self.px(6.0)) as f32, self.baseline(pill.y, pill.h), co.on_accent);
        }
    }

    fn paint_drag_label(&mut self, c: &mut Canvas) {
        if let Some(FileDrag::Active(d)) = &self.file_drag
            && !d.ghost_at.is_empty()
        {
            let (ghost, at) = (d.ghost.clone(), d.ghost_at);
            self.paint_ghost(c, &ghost, at);
        }
        let (label, r) = if let Some(FileDrag::Active(d)) = &self.file_drag {
            (self.drag_label(d), d.label)
        } else if let Some(i) = &self.incoming
            && let Some(t) = &i.target
        {
            (self.incoming_label(i, t), i.label)
        } else {
            return;
        };
        if r.is_empty() {
            return;
        }
        let co = self.colors;
        let radius = self.px(5.0);
        c.rrect(r, radius, co.accent);
        let (x, by, max) = ((r.x + self.px(8.0)) as f32, self.baseline(r.y, r.h), (r.w - self.px(16.0)) as f32);
        self.text.draw_fitted(c, &label, x, by, max, co.on_accent);
    }

    /// The baseline that centers a line of text in a row.
    pub(crate) fn baseline(&self, top: i32, h: i32) -> i32 {
        top + ((h as f32 + self.text.ascent - self.text.descent) / 2.0).round() as i32
    }

    pub(crate) fn with_clip(c: &mut Canvas, r: Rect, f: impl FnOnce(&mut Canvas)) {
        let old = c.clip();
        c.set_clip(old.intersect(r));
        if !c.clip().is_empty() {
            f(c);
        }
        c.set_clip(old);
    }

    fn paint_toolbar(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.toolbar, co.surface);
        c.fill(Rect::new(0, l.toolbar.bottom() - 1, l.toolbar.w, 1), co.border);
        let (b, pad, icon) = (self.px(36.0), self.px(6.0), self.px(18.0));
        let buttons = [(Hit::Back, Icon::Back, self.history.can_go_back()), (Hit::Forward, Icon::Forward, self.history.can_go_forward()), (Hit::Up, Icon::Up, self.up_target().is_some())];
        for (i, (hit, ic, on)) in buttons.into_iter().enumerate() {
            let x = pad + i as i32 * b;
            if on && self.hover == hit {
                c.rrect(Rect::new(x + 2, self.px(6.0), b - 4, l.toolbar.h - self.px(12.0)), self.px(5.0), co.hover);
            }
            self.icons.draw(c, ic, x + (b - icon) / 2, (l.toolbar.h - icon) / 2, icon, if on { co.foreground } else { co.border });
        }
        let y = self.baseline(0, l.toolbar.h);
        if let Edit::Path(f) = &self.edit {
            let f = f.clone();
            let r = self.path_box();
            self.paint_field(c, r, &f, true);
        } else {
            let crumbs = self.crumbs();
            let last = crumbs.len().saturating_sub(1);
            let gap = self.px(14.0);
            for (i, (x, w, path)) in crumbs.into_iter().enumerate() {
                if self.hover == Hit::Crumb(i) {
                    c.rrect(Rect::new(x, self.px(8.0), w, l.toolbar.h - self.px(16.0)), self.px(5.0), co.hover);
                }
                let label = self.crumb_label(&path);
                self.text.draw_str(c, &label, (x + self.px(6.0)) as f32, y, if i == last { co.foreground } else { co.muted });
                if i != last {
                    self.icons.draw(c, Icon::Forward, x + w + (gap - self.px(10.0)) / 2, (l.toolbar.h - self.px(10.0)) / 2, self.px(10.0), co.muted);
                }
            }
        }
        if let Some(r) = self.empty_trash_rect() {
            let on = self.can_empty_trash();
            if on && self.hover == Hit::EmptyTrash {
                c.rrect(r, self.px(5.0), co.hover);
            }
            c.frame(r, 1, co.border);
            let w = self.text.measure("Empty Trash");
            let by = self.baseline(r.y, r.h);
            self.text.draw_str(c, "Empty Trash", r.x as f32 + (r.w as f32 - w) / 2.0, by, if on { co.danger } else { co.muted });
        }
        if let Some(r) = self.filter_rect() {
            let focused = matches!(self.edit, Edit::Filter(_));
            let f = match &self.edit {
                Edit::Filter(f) => f.clone(),
                _ => Field::new(&self.filter),
            };
            self.paint_field(c, r, &f, focused);
        }
    }

    fn paint_field(&mut self, c: &mut Canvas, r: Rect, f: &Field, focused: bool) {
        let co = self.colors;
        c.rrect(r, self.px(5.0), co.base);
        c.frame(r, 1, if focused { co.accent } else { co.border });
        let inner = Rect::new(r.x + self.px(8.0), r.y, r.w - self.px(16.0), r.h);
        let y = self.baseline(r.y, r.h);
        let caret_x = self.text.measure(&f.text[..f.caret]);
        // Keep the caret in view when the text is longer than the box.
        let shift = (caret_x - inner.w as f32 + 2.0).max(0.0);
        let x0 = inner.x as f32 - shift;
        let (a, b) = f.selection();
        let (sa, sb) = (self.text.measure(&f.text[..a]), self.text.measure(&f.text[..b]));
        let line = self.text.line_height() as i32;
        let line_y = r.y + (r.h - line) / 2;
        let text = &mut self.text;
        Self::with_clip(c, inner, |c| {
            if focused && a != b {
                c.fill(Rect::new((x0 + sa) as i32, line_y, (sb - sa) as i32, line), co.selection);
            }
            text.draw_str(c, &f.text, x0, y, co.foreground);
            if focused {
                c.fill(Rect::new((x0 + caret_x) as i32, line_y, 1, line), co.foreground);
            }
        });
    }

    fn paint_sidebar(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.sidebar, co.surface);
        c.fill(Rect::new(l.sidebar.right() - 1, l.sidebar.y, 1, l.sidebar.h), co.border);
        let row = self.px(30.0);
        let icon = self.px(16.0);
        let top = l.sidebar.y + self.px(8.0);
        let places = std::mem::take(&mut self.places);
        for (i, p) in places.iter().enumerate() {
            let y = top + i as i32 * row;
            let r = Rect::new(l.sidebar.x + self.px(6.0), y, l.sidebar.w - self.px(13.0), row);
            let here = p.path == self.dir;
            if self.is_drop_target(Hit::Place(i)) {
                c.rrect(r, self.px(5.0), blend(co.accent, co.surface, 0.45));
            } else if here {
                c.rrect(r, self.px(5.0), co.selection);
            } else if self.hover == Hit::Place(i) {
                c.rrect(r, self.px(5.0), co.hover);
            }
            let ic = match p.kind {
                PlaceKind::Home => Icon::Home,
                PlaceKind::Trash => Icon::Trash,
                PlaceKind::Root => Icon::Computer,
                PlaceKind::Disk => Icon::Disk,
                PlaceKind::Documents => Icon::Document,
                PlaceKind::Downloads => Icon::ArrowDown,
                PlaceKind::Music => Icon::Audio,
                PlaceKind::Pictures => Icon::Image,
                PlaceKind::Videos => Icon::Video,
            };
            self.icons.draw(c, ic, r.x + self.px(8.0), y + (row - icon) / 2, icon, if here { co.on_selection } else { co.muted });
            let tx = (r.x + self.px(32.0)) as f32;
            let color = if here { co.on_selection } else { co.foreground };
            let max = (r.right() - self.px(6.0)) as f32 - tx;
            let by = self.baseline(y, row);
            self.text.draw_fitted(c, &p.label, tx, by, max, color);
        }
        self.places = places;
    }

    fn paint_header(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.header, co.base);
        c.fill(Rect::new(l.header.x, l.header.bottom() - 1, l.header.w, 1), co.border);
        let date_x = l.header.right() - self.px(14.0) - l.col_date;
        let size_x = date_x - l.col_size;
        let name_x = l.header.x + self.px(14.0);
        let y = self.baseline(l.header.y, l.header.h);
        let arrow = self.px(14.0);
        let trash = self.listing.as_ref().is_some_and(|l| l.is_trash());
        if let Some((ox, _)) = self.origin_column(l.header, size_x).filter(|_| trash) {
            self.text.draw_str(c, "Original location", ox as f32, y, co.muted);
        }
        let cols = [(SortKey::Name, "Name", name_x), (SortKey::Size, "Size", size_x), (SortKey::Modified, if trash { "Deleted" } else { "Modified" }, date_x)];
        for (key, label, x) in cols {
            let on = self.sort.key == key;
            let color = if on || self.hover == Hit::Header(key) { co.foreground } else { co.muted };
            // Size figures sit to the right of their column, so its label does too.
            let w = self.text.measure(label) as i32;
            let x = if key == SortKey::Size { date_x - self.px(16.0) - w - if on { arrow } else { 0 } } else { x };
            self.text.draw_str(c, label, x as f32, y, color);
            if on {
                let ic = if self.sort.descending { Icon::ArrowDown } else { Icon::ArrowUp };
                self.icons.draw(c, ic, x + w + self.px(3.0), l.header.y + (l.header.h - arrow) / 2, arrow, co.muted);
            }
        }
    }

    fn paint_list(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        let area = l.list;
        c.fill(area.intersect(c.clip()), co.base);
        if let (Edit::NewFolder(f), true) = (&self.edit, self.rows.is_empty()) {
            let f = f.clone();
            let name_x = area.x + self.px(38.0);
            let r = Rect::new(name_x - self.px(4.0), area.y + 1, self.px(300.0), l.row_h - self.px(2.0));
            self.paint_field(c, r, &f, true);
            return;
        }
        let Some(listing) = self.listing.clone() else {
            if let Some(e) = self.load_error.clone() {
                self.paint_center(c, area, &format!("Can't open this folder: {e}"), co.danger);
            }
            return;
        };
        if self.rows.is_empty() {
            if !self.loading {
                let msg = if !self.filter.is_empty() {
                    "Nothing matches the filter"
                } else if listing.is_trash() {
                    "The Trash is empty"
                } else {
                    "This folder is empty"
                };
                self.paint_center(c, area, msg, co.muted);
            }
            return;
        }
        let row_h = l.row_h;
        let first = (self.scroll / row_h as f64) as usize;
        let offset = (self.scroll - first as f64 * row_h as f64) as i32;
        let clip = c.clip().intersect(area);
        let date_x = area.right() - self.px(14.0) - l.col_date;
        let size_x = date_x - l.col_size;
        let name_x = area.x + self.px(38.0);
        let origin = self.origin_column(area, size_x).filter(|_| listing.is_trash());
        let name_max = (origin.map_or(size_x, |(ox, _)| ox) - self.px(10.0) - name_x) as f32;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let icon = self.px(16.0);
        let now = time::now();
        let hover_row = if let Hit::Row(r) = self.hover { Some(r) } else { None };
        let thumb_room = if self.scrollbar_visible() { self.px(12.0) } else { 0 };
        Self::with_clip(c, area, |c| {
            let visible = (area.h + offset) / row_h.max(1) + 1;
            for vis in 0..visible as usize {
                let row = first + vis;
                let Some(&id) = self.rows.get(row) else { break };
                let y = area.y + vis as i32 * row_h - offset;
                let r = Rect::new(area.x, y, area.w, row_h);
                if !r.intersects(clip) {
                    continue;
                }
                let selected = self.is_selected(id);
                if self.is_drop_target(Hit::Row(row)) {
                    c.fill(r, blend(co.accent, co.base, 0.45));
                } else if selected {
                    c.fill(r, co.selection);
                } else if hover_row == Some(row) {
                    c.fill(r, co.hover);
                }
                if self.cursor_shown && row == self.cursor {
                    c.fill(Rect::new(r.x, r.y, self.px(3.0), r.h), co.accent);
                }
                let fg = if selected { co.on_selection } else { co.foreground };
                let dim = if selected { blend(co.on_selection, co.selection, 0.25) } else { co.muted };
                let name = listing.name(id);
                let kind = listing.kind(id);
                let tint = if kind.is_dirlike() { if selected { co.on_selection } else { co.accent } } else { dim };
                self.icons.draw(c, icon_for(kind, name), area.x + self.px(14.0), y + (row_h - icon) / 2, icon, tint);
                let shown = String::from_utf8_lossy(name);
                let by = y + ((row_h as f32 + self.text.ascent - self.text.descent) / 2.0).round() as i32;
                let is_cut = !self.cut_paths.is_empty() && self.cut_paths.iter().any(|p| p.parent() == Some(listing.dir.as_path()) && p.file_name() == Some(listing.os_name(id)));
                if let Edit::Rename { name: editing, field, .. } = &self.edit
                    && editing.as_slice() == name
                {
                    let f = field.clone();
                    let box_h = (row_h - self.px(2.0)).max(1);
                    self.paint_field(c, Rect::new(name_x - self.px(4.0), y + 1, name_max as i32 + self.px(8.0), box_h), &f, true);
                } else {
                    self.text.draw_fitted(c, &shown, name_x as f32, by, name_max, if is_cut { dim } else { fg });
                }
                if let Some((ox, ow)) = origin
                    && let Some(folder) = listing.original(id).and_then(Path::parent)
                {
                    let shown = match home.as_deref().and_then(|h| folder.strip_prefix(h).ok()) {
                        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                        Some(rest) => format!("~/{}", rest.display()),
                        None => folder.display().to_string(),
                    };
                    self.text.draw_fitted(c, &shown, ox as f32, by, ow as f32, dim);
                }
                match listing.stat(id) {
                    Stat::Known { size, mtime } => {
                        if !matches!(kind, Kind::Dir | Kind::LinkDir) {
                            let s = format::size(size);
                            let w = self.text.measure(&s);
                            self.text.draw_str(c, &s, (date_x - self.px(16.0)) as f32 - w, by, dim);
                        }
                        let d = format::date(mtime, now);
                        self.text.draw_str(c, &d, date_x as f32, by, dim);
                    }
                    Stat::Failed => self.text.draw_str(c, "?", date_x as f32, by, dim),
                    Stat::Unknown => {}
                }
            }
            if let Edit::NewFolder(f) = &self.edit {
                let f = f.clone();
                let r = Rect::new(area.x, area.y, area.w, row_h);
                c.fill(r, co.hover);
                self.icons.draw(c, crate::icons::Icon::Folder, area.x + self.px(14.0), area.y + (row_h - icon) / 2, icon, co.accent);
                self.paint_field(c, Rect::new(name_x - self.px(4.0), area.y + 1, name_max as i32 + self.px(8.0), row_h - self.px(2.0)), &f, true);
            }
            if thumb_room > 0 {
                let (ty, th) = self.scrollbar_thumb();
                let w = self.px(5.0);
                let color = if self.hover == Hit::Scrollbar { co.muted } else { co.border };
                c.rrect(Rect::new(area.right() - w - self.px(3.0), area.y + ty, w, th), w / 2, color);
            }
        });
    }

    /// Where the Trash view's "Original location" column goes, between the names and sizes.
    pub(crate) fn origin_column(&self, area: Rect, size_x: i32) -> Option<(i32, i32)> {
        let name_x = area.x + self.px(38.0);
        let room = size_x - self.px(10.0) - name_x;
        let w = (room * 2 / 5).min(self.px(320.0));
        (w >= self.px(60.0)).then(|| (size_x - self.px(10.0) - w, w))
    }

    pub(crate) fn paint_center(&mut self, c: &mut Canvas, area: Rect, msg: &str, color: u32) {
        let w = self.text.measure(msg);
        let x = area.x as f32 + ((area.w as f32 - w) / 2.0).max(self.px(12.0) as f32);
        let y = self.baseline(area.y + area.h / 3, self.px(30.0));
        self.text.draw_fitted(c, msg, x, y, (area.w - self.px(24.0)) as f32, color);
    }

    fn paint_notices(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        let row = self.px(30.0);
        let shown_jobs: Vec<_> = self.visible_jobs().take(4).map(|j| (j.job.label.clone(), j.job.progress.clone(), j.job.is_cancelled())).collect();
        let job_rows = shown_jobs.len();
        for (i, (label, p, stopping)) in shown_jobs.into_iter().enumerate() {
            let y = l.notices.y + i as i32 * row;
            let r = Rect::new(l.notices.x, y, l.notices.w, row);
            c.fill(r, co.surface);
            c.fill(Rect::new(r.x, r.y, r.w, 1), co.border);
            let (total, done) = (p.total_bytes.load(Ordering::Relaxed), p.done_bytes.load(Ordering::Relaxed));
            let (items, done_items) = (p.total_items.load(Ordering::Relaxed), p.done_items.load(Ordering::Relaxed));
            let scanning = p.scanning.load(Ordering::Relaxed);
            let fraction = if total > 0 { done as f32 / total as f32 } else if items > 0 { done_items as f32 / items as f32 } else { 0.0 };
            let bar = Rect::new(r.right() - self.px(240.0), y + (row - self.px(6.0)) / 2, self.px(200.0), self.px(6.0));
            let text_max = (bar.x - self.px(12.0) - r.x - self.px(14.0)) as f32;
            let detail = if stopping {
                format!("{label}: stopping…")
            } else if scanning {
                format!("{label}: measuring…")
            } else if total > 0 {
                format!("{label}: {} of {}", format::size(done), format::size(total))
            } else {
                label
            };
            let by = self.baseline(y, row);
            self.text.draw_fitted(c, &detail, (r.x + self.px(14.0)) as f32, by, text_max, co.foreground);
            c.rrect(bar, bar.h / 2, co.hover);
            let fill = ((bar.w as f32 * fraction.clamp(0.0, 1.0)) as i32).max(if scanning { 0 } else { bar.h });
            c.rrect(Rect::new(bar.x, bar.y, fill, bar.h), bar.h / 2, co.accent);
            let s = self.px(12.0);
            let color = if self.hover == Hit::JobCancel(i) { co.foreground } else { co.muted };
            self.icons.draw(c, Icon::Close, r.right() - self.px(22.0), y + (row - s) / 2, s, color);
        }
        let count = self.notices.len().min(4);
        let shown: Vec<String> = self.notices.iter().take(count).cloned().collect();
        for (i, msg) in shown.iter().enumerate() {
            let y = l.notices.y + (job_rows + i) as i32 * row;
            let r = Rect::new(l.notices.x, y, l.notices.w, row);
            c.fill(r, blend(co.base, co.danger, 0.18));
            c.fill(Rect::new(r.x, r.y, self.px(3.0), r.h), co.danger);
            let by = self.baseline(y, row);
            let more = self.notices.len() - count;
            let line = if more > 0 && i + 1 == count { format!("{msg} (and {more} more)") } else { msg.clone() };
            self.text.draw_fitted(c, &line, (r.x + self.px(14.0)) as f32, by, (r.w - self.px(52.0)) as f32, co.foreground);
            let s = self.px(12.0);
            let color = if self.hover == Hit::NoticeClose(i) { co.foreground } else { co.muted };
            self.icons.draw(c, Icon::Close, r.right() - self.px(22.0), y + (row - s) / 2, s, color);
        }
    }

    fn paint_dialog(&mut self, c: &mut Canvas) {
        let Some(d) = self.dialog.take() else { return };
        let co = self.colors;
        let r = self.dialog_rect();
        c.rrect(r, self.px(8.0), co.surface);
        c.frame(r, 1, co.border);
        let name = |p: &std::path::Path| p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        let (title, line2, at) = match &d {
            Dialog::Conflict { conflict, at, .. } => {
                let t = if conflict.both_folders { format!("A folder named “{}” is already here", name(&conflict.target)) } else { format!("“{}” is already here", name(&conflict.target)) };
                let body = if conflict.both_folders {
                    "Merge puts the contents together and asks about any clashes."
                } else if conflict.can_replace {
                    "Replace overwrites it. Keep both gives the new one another name."
                } else {
                    "One's a file and the other's a folder, so it can't be replaced."
                };
                (t, body.to_string(), *at)
            }
            Dialog::ConfirmDelete { paths, at, empty: true } => {
                let t = match paths.len() {
                    1 => "Delete the one item in the Trash for good?".to_string(),
                    n => format!("Delete all {n} items in the Trash for good?"),
                };
                // A disk that didn't answer keeps its items, and the question shouldn't hide that.
                let more = if self.load_error.is_some() { " A disk that couldn't be read keeps its items." } else { "" };
                (t, format!("This can't be undone.{more}"), *at)
            }
            Dialog::ConfirmDelete { paths, at, .. } => {
                let t = match paths.as_slice() {
                    [one] => format!("Delete “{}” for good?", name(one)),
                    many => format!("Delete {} for good?", format::count(many.len(), "item", "items")),
                };
                (t, "This can't be undone.".to_string(), *at)
            }
            Dialog::ConfirmQuit { at } => ("Operations are still running".to_string(), "Closing stops them. Whatever's finished stays finished.".to_string(), *at),
        };
        let x = (r.x + self.px(16.0)) as f32;
        let max = (r.w - self.px(32.0)) as f32;
        let row = self.px(26.0);
        let top = r.y + self.px(12.0);
        let (y1, y2) = (self.baseline(top, row), self.baseline(top + row, row));
        self.text.draw_fitted(c, &title, x, y1, max, co.foreground);
        self.text.draw_fitted(c, &line2, x, y2, max, co.muted);
        if let Dialog::Conflict { apply_to_all, .. } = &d {
            let cr = self.dialog_check_rect();
            let s = self.px(14.0);
            let bx = Rect::new(cr.x, cr.y + (cr.h - s) / 2, s, s);
            if *apply_to_all {
                c.rrect(bx, self.px(3.0), co.accent);
            }
            c.frame(bx, 1, if *apply_to_all { co.accent } else { co.muted });
            let by = self.baseline(cr.y, cr.h);
            self.text.draw_str(c, "Do the same for the rest (A)", (cr.x + s + self.px(8.0)) as f32, by, co.foreground);
        }
        let buttons = App::dialog_buttons(&d);
        let rects = self.dialog_button_rects(&d);
        for (i, (b, br)) in buttons.iter().zip(&rects).enumerate() {
            let focused = i == at;
            let bg = if focused { if b.danger { co.danger } else { co.accent } } else { co.hover };
            c.rrect(*br, self.px(5.0), bg);
            let fg = if focused { co.on_accent } else if b.danger { co.danger } else { co.foreground };
            let w = self.text.measure(b.label);
            let by = self.baseline(br.y, br.h);
            self.text.draw_str(c, b.label, br.x as f32 + (br.w as f32 - w) / 2.0, by, fg);
        }
        self.dialog = Some(d);
    }

    fn paint_status(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.status, co.surface);
        c.fill(Rect::new(0, l.status.y, l.status.w, 1), co.border);
        let msg = self.status_text();
        let y = self.baseline(l.status.y, l.status.h);
        self.text.draw_fitted(c, &msg, self.px(12.0) as f32, y, (l.status.w - self.px(24.0)) as f32, co.muted);
    }

    fn status_text(&mut self) -> String {
        let total = self.rows.len();
        let mut s = format::count(total, "item", "items");
        if self.loading && self.listing.as_ref().is_some_and(|l| !l.complete) {
            s.push_str(" so far, still reading…");
        }
        if !self.filter.is_empty() {
            s.push_str(" matching the filter");
        }
        if self.sel_count > 0 {
            s.push_str(&format!(" · {} selected", format::count(self.sel_count, "item", "items").replace(" items", "").replace(" item", "")));
            if let Some(l) = &self.listing
                && self.sel_count <= 2000
            {
                let (mut bytes, mut known) = (0u64, true);
                for id in self.selected_ids() {
                    if l.kind(id).is_dirlike() {
                        continue;
                    }
                    match l.stat(id) {
                        Stat::Known { size, .. } => bytes += size,
                        _ => known = false,
                    }
                }
                if known && bytes > 0 {
                    s.push_str(&format!(", {}", format::size(bytes)));
                }
            }
        }
        if let Some(e) = self.listing.as_ref().and(self.load_error.as_ref()) {
            s.push_str(&format!(" · couldn't read it all: {e}"));
        }
        s
    }

    fn paint_menu(&mut self, c: &mut Canvas) {
        let Some(m) = self.menu.take() else { return };
        let co = self.colors;
        let r = self.menu_rect(&m);
        c.rrect(r, self.px(6.0), co.surface);
        c.frame(r, 1, co.border);
        let row = self.px(28.0);
        let top = r.y + self.px(4.0);
        for (i, item) in m.items.iter().enumerate() {
            let y = top + i as i32 * row;
            if i == m.at && item.enabled {
                c.rrect(Rect::new(r.x + self.px(4.0), y, r.w - self.px(8.0), row), self.px(4.0), co.selection);
            }
            let fg = if !item.enabled {
                co.border
            } else if i == m.at {
                co.on_selection
            } else {
                co.foreground
            };
            let by = self.baseline(y, row);
            self.text.draw_str(c, item.label, (r.x + self.px(12.0)) as f32, by, fg);
            let w = self.text.measure(item.keys);
            self.text.draw_str(c, item.keys, (r.right() - self.px(12.0)) as f32 - w, by, if item.enabled { co.muted } else { co.border });
        }
        self.menu = Some(m);
    }
}

