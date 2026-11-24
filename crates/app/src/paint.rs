//! Drawing the window. Each part checks the clip first, so a small change repaints little.

use gannet_core::listing::{Kind, SortKey, Stat};
use gannet_core::places::PlaceKind;
use gannet_core::theme::blend;
use gannet_core::time;

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::app::{ActiveDrag, App, Dialog, DragGhost, Edit, FileDrag, Hit, Pane, icon_for};
use crate::canvas::{Canvas, Rect};
use crate::field::Field;
use crate::format;
use crate::icons::Icon;
use crate::rename_many::{Focus, RenameMany};

impl App {
    pub fn paint(&mut self, c: &mut Canvas) {
        if self.viewer {
            return self.paint_preview(c);
        }
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
        let (b, pad, icon) = (self.px(36.0), self.px(6.0), self.px(16.0));
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
                    self.icons.draw(c, Icon::Crumb, x + w + (gap - self.px(10.0)) / 2, (l.toolbar.h - self.px(10.0)) / 2, self.px(10.0), co.muted);
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
            // An empty box says how far it reaches.
            if f.text.is_empty() {
                let hint = if self.search_below { "Search here and below" } else { "Filter this folder" };
                let by = self.baseline(r.y, r.h);
                self.text.draw_fitted(c, hint, (r.x + self.px(10.0)) as f32, by, (r.w - self.px(18.0)) as f32, co.muted);
            }
        }
    }

    pub(crate) fn paint_field(&mut self, c: &mut Canvas, r: Rect, f: &Field, focused: bool) {
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

    /// A pane's scroll bar thumb, with its right edge at `right`.
    fn paint_scrollbar(&mut self, c: &mut Canvas, pane: Pane, right: i32) {
        let area = self.pane_rect(pane);
        let (ty, th) = self.scrollbar_thumb(pane);
        let w = self.px(5.0);
        let co = self.colors;
        let color = if self.hover == Hit::Scrollbar(pane) { co.muted } else { co.border };
        c.rrect(Rect::new(right - w - self.px(3.0), area.y + ty, w, th), w / 2, color);
    }

    fn paint_sidebar(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.sidebar, co.surface);
        c.fill(Rect::new(l.sidebar.right() - 1, l.sidebar.y, 1, l.sidebar.h), co.border);
        // Places scrolled out of view mustn't spill over the toolbar or the status bar.
        Self::with_clip(c, l.sidebar, |c| self.paint_places(c));
        if self.scrollbar_visible(Pane::Sidebar) {
            self.paint_scrollbar(c, Pane::Sidebar, l.sidebar.right() - 1);
        }
    }

    fn paint_places(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        // The scroll bar's room, when there is one, comes off the rows' right.
        let room = self.side_bar_room();
        let row = self.px(30.0);
        let icon = self.px(16.0);
        // Worked out while the places are still here: they're moved out while they're drawn.
        let ejects: Vec<Option<Rect>> = (0..self.places.len()).map(|i| self.eject_rect(i)).collect();
        let tops: Vec<i32> = (0..self.places.len()).map(|i| self.place_top(i)).collect();
        let heading = self.px(crate::app::SIDEBAR_HEADING);
        for (h, label) in self.sidebar_headings() {
            let by = self.baseline(tops[h] - heading, heading);
            self.text.draw_fitted(c, label, (l.sidebar.x + self.px(14.0)) as f32, by, (l.sidebar.w - self.px(28.0) - room) as f32, co.muted);
        }
        // A drag that can't go to the Trash shows it dimmed, like a button that can't be pressed.
        let trash_off = self.trash_unavailable();
        let carried = self.carried_pin().map(std::path::Path::to_path_buf);
        let places = std::mem::take(&mut self.places);
        for (i, p) in places.iter().enumerate() {
            let y = tops[i];
            let r = Rect::new(l.sidebar.x + self.px(6.0), y, l.sidebar.w - self.px(13.0) - room, row);
            let here = p.path == self.dir;
            if self.is_drop_target(Hit::Place(i)) {
                c.rrect(r, self.px(5.0), blend(co.accent, co.surface, 0.45));
            } else if here {
                c.rrect(r, self.px(5.0), co.selection);
            } else if self.hover == Hit::Place(i) {
                c.rrect(r, self.px(5.0), co.hover);
            }
            let ic = match p.kind {
                PlaceKind::Disk if p.volume.as_ref().is_some_and(|v| v.locked) => Icon::Lock,
                PlaceKind::Home => Icon::Home,
                PlaceKind::Recent => Icon::Recent,
                PlaceKind::Trash => Icon::Trash,
                PlaceKind::Disk => Icon::Disk,
                PlaceKind::Documents => Icon::Document,
                PlaceKind::Downloads => Icon::Download,
                PlaceKind::Music => Icon::Audio,
                PlaceKind::Pictures => Icon::Image,
                PlaceKind::Videos => Icon::Video,
                PlaceKind::Pinned => Icon::Folder,
            };
            // A pin being carried elsewhere is dimmed where it was, like the Trash a drag can't use.
            let off = (trash_off && p.kind == PlaceKind::Trash) || (p.kind == PlaceKind::Pinned && carried.as_ref() == Some(&p.path));
            self.icons.draw(c, ic, r.x + self.px(8.0), y + (row - icon) / 2, icon, if here { co.on_selection } else if off { co.border } else { co.muted });
            let tx = (r.x + self.px(32.0)) as f32;
            // A drive that's busy, or not mounted yet, is quieter than the rest.
            let waiting = p.volume.as_ref().is_some_and(|v| v.mount.is_none() || self.drives_busy.contains(&v.block));
            let color = if here { co.on_selection } else if off { co.border } else if waiting { co.muted } else { co.foreground };
            let eject = ejects[i];
            let right = eject.map_or(r.right() - self.px(6.0), |e| e.x);
            let max = right as f32 - tx;
            let by = self.baseline(y, row);
            self.text.draw_fitted(c, &p.label, tx, by, max, color);
            if let Some(e) = eject {
                if self.hover == Hit::Eject(i) {
                    c.rrect(Rect::new(e.x + self.px(2.0), e.y + self.px(3.0), e.w - self.px(4.0), e.h - self.px(6.0)), self.px(4.0), if here { co.accent } else { co.border });
                }
                let s = self.px(16.0);
                self.icons.draw(c, Icon::Eject, e.x + (e.w - s) / 2, e.y + (e.h - s) / 2, s, if here { co.on_selection } else { co.muted });
            }
        }
        self.places = places;
        // Where a pin or a dragged folder would land among the pins.
        if let Some(y) = self.pin_bar() {
            let h = self.px(2.0).max(2);
            c.rrect(Rect::new(l.sidebar.x + self.px(8.0), y - h / 2, l.sidebar.w - self.px(17.0) - room, h), h / 2, co.accent);
        }
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
        let recent = self.listing.as_ref().is_some_and(|l| l.is_recent());
        let search = self.listing.as_ref().is_some_and(|l| l.search().is_some());
        if let Some((ox, _)) = self.origin_column(l.header, size_x).filter(|_| trash || recent || search) {
            self.text.draw_str(c, if trash { "Original location" } else { "Location" }, ox as f32, y, co.muted);
        }
        let sort = self.view_sort();
        let date = if trash {
            "Deleted"
        } else if recent {
            "Used"
        } else {
            "Modified"
        };
        let cols = [(SortKey::Name, "Name", name_x), (SortKey::Size, "Size", size_x), (SortKey::Modified, date, date_x)];
        for (key, label, x) in cols {
            let on = sort.key == key;
            let color = if on || self.hover == Hit::Header(key) { co.foreground } else { co.muted };
            // Size figures sit to the right of their column, so its label does too.
            let w = self.text.measure(label) as i32;
            let x = if key == SortKey::Size { date_x - self.px(16.0) - w - if on { arrow } else { 0 } } else { x };
            self.text.draw_str(c, label, x as f32, y, color);
            if on {
                let ic = if sort.descending { Icon::ArrowDown } else { Icon::ArrowUp };
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
            } else if self.loading && self.hold_paint.is_none() && self.search_query().is_some() {
                self.paint_center(c, area, "Searching…", co.muted);
            }
            return;
        };
        if self.rows.is_empty() {
            let searching = self.search_query().is_some();
            if searching && self.loading {
                self.paint_center(c, area, "Searching…", co.muted);
            } else if !self.loading {
                let msg = if searching {
                    format!("Nothing found in {} or the folders in it", self.crumb_label(&self.dir))
                } else if !self.filter.is_empty() && self.can_search() {
                    "Nothing here matches. Ctrl+F searches the folders inside too".to_string()
                } else if !self.filter.is_empty() {
                    "Nothing matches the filter".to_string()
                } else if listing.is_trash() {
                    "The Trash is empty".to_string()
                } else if listing.is_recent() {
                    "No recent files".to_string()
                } else {
                    "This folder is empty".to_string()
                };
                self.paint_center(c, area, &msg, co.muted);
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
        let origin = self.origin_column(area, size_x).filter(|_| listing.is_trash() || listing.is_recent() || listing.search().is_some());
        let name_max = (origin.map_or(size_x, |(ox, _)| ox) - self.px(10.0) - name_x) as f32;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let icon = self.px(16.0);
        let now = time::now();
        let hover_row = if let Hit::Row(r) = self.hover { Some(r) } else { None };
        let thumb_room = if self.scrollbar_visible(Pane::List) { self.px(12.0) } else { 0 };
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
                // A thumbnail in the icon's place, centred where the icon would be.
                if let Some(pic) = self.thumbnail(&listing, id) {
                    let (cx, cy) = (area.x + self.px(14.0) + icon / 2, y + row_h / 2);
                    let (w, h) = (pic.w as i32, pic.h as i32);
                    c.image(&pic.rgba, w, h, Rect::new(cx - w / 2, cy - h / 2, w, h));
                } else {
                    self.icons.draw(c, icon_for(kind, name), area.x + self.px(14.0), y + (row_h - icon) / 2, icon, tint);
                }
                let shown = String::from_utf8_lossy(name);
                let by = y + ((row_h as f32 + self.text.ascent - self.text.descent) / 2.0).round() as i32;
                // Recent's and the Trash's rows are each somewhere of their own.
                let is_cut = !self.cut_paths.is_empty() && {
                    let here = listing.path(id);
                    self.cut_paths.contains(&here)
                };
                // Search results can have several rows of one name.
                if let Edit::Rename { name: editing, field, path, .. } = &self.edit
                    && editing.as_slice() == name
                    && (listing.search().is_none() || *path == listing.path(id))
                {
                    let f = field.clone();
                    let box_h = (row_h - self.px(2.0)).max(1);
                    self.paint_field(c, Rect::new(name_x - self.px(4.0), y + 1, name_max as i32 + self.px(8.0), box_h), &f, true);
                } else {
                    self.text.draw_fitted(c, &shown, name_x as f32, by, name_max, if is_cut { dim } else { fg });
                }
                if let Some((ox, ow)) = origin
                    && let Some(folder) = listing.location(id)
                {
                    let shown = if listing.search().is_some() { self.search_location(folder) } else { crate::app::home_location(folder, home.as_deref()) };
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
                self.paint_scrollbar(c, Pane::List, area.right());
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
        let r = self.dialog_rect_for(&d);
        c.rrect(r, self.px(8.0), co.surface);
        c.frame(r, 1, co.border);
        if let Dialog::RenameMany(rm) = &d {
            self.paint_rename_many(c, rm);
            // Enter renames from the fields, so Rename shows as the default there, unless
            // something stops it.
            let at = if let Focus::Button(i) = rm.focus { i } else { 0 };
            self.paint_dialog_buttons(c, &d, at, rm.focus != Focus::Button(1) && !rm.can_rename());
            self.dialog = Some(d);
            return;
        }
        let (title, line2) = self.dialog_text(&d);
        let at = App::dialog_default(&d);
        let x = (r.x + self.px(16.0)) as f32;
        let max = (r.w - self.px(32.0)) as f32;
        let row = self.px(26.0);
        let top = r.y + self.px(12.0);
        let (y1, y2) = (self.baseline(top, row), self.baseline(top + row, row));
        self.text.draw_fitted(c, &title, x, y1, max, co.foreground);
        let line2_color = if matches!(&d, Dialog::Unlock { error: Some(_), waiting: false, .. }) { co.danger } else { co.muted };
        self.text.draw_fitted(c, &line2, x, y2, max, line2_color);
        if let Dialog::Unlock { field, .. } = &d {
            // The passphrase shows as a dot for each character, never as itself.
            let fr = self.dialog_check_rect();
            c.rrect(fr, self.px(5.0), co.base);
            c.frame(fr, 1, co.accent);
            let n = field.text.chars().count();
            let before = field.text[..field.caret].chars().count();
            let dot = self.text.measure("•");
            let fx = (fr.x + self.px(8.0)) as f32;
            let by = self.baseline(fr.y, fr.h);
            self.text.draw_fitted(c, &"•".repeat(n), fx, by, (fr.w - self.px(16.0)) as f32, co.foreground);
            let caret_x = (fx + dot * before as f32).min((fr.right() - self.px(8.0)) as f32) as i32;
            c.fill(Rect::new(caret_x, fr.y + self.px(5.0), self.px(1.0).max(1), fr.h - self.px(10.0)), co.foreground);
        }
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
        self.paint_dialog_buttons(c, &d, at, false);
        self.dialog = Some(d);
    }

    /// The dialog's buttons, with the one at `at` focused. `first_off` shows the first one can't
    /// be pressed now.
    fn paint_dialog_buttons(&mut self, c: &mut Canvas, d: &Dialog, at: usize, first_off: bool) {
        let co = self.colors;
        let buttons = App::dialog_buttons(d);
        let rects = self.dialog_button_rects(d);
        for (i, (b, br)) in buttons.iter().zip(&rects).enumerate() {
            let off = first_off && i == 0;
            let focused = i == at && !off;
            let bg = if focused { if b.danger { co.danger } else { co.accent } } else { co.hover };
            c.rrect(*br, self.px(5.0), bg);
            let fg = if off { co.muted } else if focused { co.on_accent } else if b.danger { co.danger } else { co.foreground };
            let w = self.text.measure(b.label);
            let by = self.baseline(br.y, br.h);
            self.text.draw_str(c, b.label, br.x as f32 + (br.w as f32 - w) / 2.0, by, fg);
        }
    }

    /// The rename dialog: its title, Find and Replace, each old name beside its new one, and the
    /// line saying what will happen or what's stopping it.
    fn paint_rename_many(&mut self, c: &mut Canvas, d: &RenameMany) {
        let co = self.colors;
        let r = self.rename_rect(d.names.len());
        let l = self.rename_layout(d.names.len());
        let x = (r.x + self.px(16.0)) as f32;
        let max = (r.w - self.px(32.0)) as f32;
        self.text.draw_fitted(c, &d.title(), x, self.baseline(r.y + self.px(12.0), self.px(26.0)), max, co.foreground);
        for (label, field, focus) in [("Find", &l.find, Focus::Find), ("Replace with", &l.replace, Focus::Replace)] {
            self.text.draw_fitted(c, label, x, self.baseline(field.y, field.h), (field.x - r.x) as f32 - self.px(24.0) as f32, co.muted);
            let f = if focus == Focus::Find { &d.find } else { &d.replace };
            self.paint_field(c, *field, f, d.focus == focus);
        }
        // The names in two columns, old on the left, new on the right after an arrow.
        let half = l.headings.w / 2;
        let new_x = l.headings.x + half + self.px(12.0);
        let col_w = (half - self.px(20.0)) as f32;
        let hy = self.baseline(l.headings.y, l.headings.h);
        self.text.draw_fitted(c, "Old name", l.headings.x as f32, hy, col_w, co.muted);
        self.text.draw_fitted(c, "New name", new_x as f32, hy, col_w, co.muted);
        c.fill(Rect::new(l.headings.x, l.headings.bottom() - 1, l.headings.w, 1), co.border);
        let shown = (l.list.h / l.row_h).max(1) as usize;
        let arrow_x = (l.headings.x + half - self.px(4.0)) as f32;
        for (k, i) in (d.scroll..d.names.len().min(d.scroll + shown)).enumerate() {
            let top = l.list.y + k as i32 * l.row_h;
            let by = self.baseline(top, l.row_h);
            let (problem, same) = (d.problems[i].is_some(), d.names[i] == d.new[i]);
            if problem {
                c.fill(Rect::new(l.list.x, top, l.list.w, l.row_h), blend(co.surface, co.danger, 0.14));
            }
            self.text.draw_fitted(c, &String::from_utf8_lossy(&d.names[i]), l.list.x as f32, by, col_w, if same { co.muted } else { co.foreground });
            self.text.draw_str(c, "→", arrow_x, by, co.muted);
            let color = if problem { co.danger } else if same { co.muted } else { co.foreground };
            self.text.draw_fitted(c, &String::from_utf8_lossy(&d.new[i]), new_x as f32, by, col_w, color);
        }
        if d.names.len() > shown {
            // Where the rows shown are in the whole list.
            let track = Rect::new(l.list.right() - self.px(3.0), l.list.y, self.px(3.0), l.list.h);
            let thumb_h = ((track.h as usize * shown / d.names.len()) as i32).max(self.px(16.0)).min(track.h);
            let thumb_y = track.y + ((track.h - thumb_h).max(0) as usize * d.scroll / (d.names.len() - shown).max(1)) as i32;
            c.rrect(Rect::new(track.x, thumb_y, track.w, thumb_h), self.px(1.5), co.muted);
        }
        let (line, problem) = d.summary();
        self.text.draw_fitted(c, &line, l.summary.x as f32, self.baseline(l.summary.y, l.summary.h), l.summary.w as f32, if problem { co.danger } else { co.muted });
    }

    fn paint_status(&mut self, c: &mut Canvas) {
        if self.chooser.is_some() {
            return self.paint_chooser_bar(c);
        }
        let l = self.layout();
        let co = self.colors;
        c.fill(l.status, co.surface);
        c.fill(Rect::new(0, l.status.y, l.status.w, 1), co.border);
        let msg = self.status_text();
        let y = self.baseline(l.status.y, l.status.h);
        self.text.draw_fitted(c, &msg, self.px(12.0) as f32, y, (l.status.w - self.px(24.0)) as f32, co.muted);
    }

    pub(crate) fn status_text(&self) -> String {
        let total = self.rows.len();
        let found = self.listing.as_ref().and_then(|l| l.search()).filter(|_| self.search_query().is_some());
        let mut s = match found {
            Some(f) if f.truncated => format!("The first {} found. Type more to narrow it", format::count(total, "item", "items").replace(" items", "").replace(" item", "")),
            Some(_) if self.loading => format!("{} found so far, searching…", format::count(total, "item", "items")),
            Some(_) => format!("{} found in {} and the folders in it", format::count(total, "item", "items"), self.crumb_label(&self.dir)),
            None if self.search_query().is_some() => "Searching…".to_string(),
            None => format::count(total, "item", "items"),
        };
        if found.is_none() && self.loading && self.listing.as_ref().is_some_and(|l| !l.complete) {
            s.push_str(" so far, still reading…");
        }
        if found.is_none() && !self.filter.is_empty() && self.search_query().is_none() {
            s.push_str(" matching the filter");
        }
        if let Some(n) = found.map(|f| f.unreadable).filter(|&n| n > 0) {
            s.push_str(&format!(" · {} couldn't be read", format::count(n, "folder", "folders")));
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

