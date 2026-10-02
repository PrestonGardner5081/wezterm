//! Client side decorations in the style of a GTK3/Adwaita headerbar
//! (eg: GNOME Terminal): a tall titlebar with rounded top corners, a
//! centered bold title and round window buttons on the right.
//!
//! The titlebar colors and font come from `window_frame` in the config,
//! which is also what the fancy tab bar uses, so the titlebar and the
//! tab bar below it blend together.
//!
//! Like GTK, the resize borders are invisible margins outside of the
//! window geometry, so that maximizing and tiling are flush with the
//! screen edges and the window outline is just the header + content.
//!
//! The structure follows smithay-client-toolkit's FallbackFrame.

use std::mem;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::ConfigHandle;
use smithay_client_toolkit::reexports::csd_frame::{
    CursorIcon, DecorationsFrame, FrameAction, FrameClick, ResizeEdge, WindowManagerCapabilities,
    WindowState,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::subcompositor::SubcompositorState;
use tiny_skia::{
    Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, PixmapRef, Stroke,
    Transform,
};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Proxy, QueueHandle};
use wezterm_color_types::SrgbaTuple;
use wezterm_font::{FontConfiguration, FontMetrics, GlyphInfo, RasterizedGlyph};

use super::state::WaylandState;

/// Height of the headerbar, in surface-local pixels (Adwaita: 46)
const HEADER_SIZE: u32 = 46;

/// Width of the invisible resize margin outside of the window geometry
const BORDER_SIZE: u32 = 10;

/// How far along an edge the corner resize zone extends
const CORNER_SIZE: u32 = 24;

/// Radius of the rounded top corners of the headerbar (GTK3 Adwaita: 8)
const CORNER_RADIUS: f32 = 8.;

/// Diameter of the round window buttons
const BUTTON_SIZE: u32 = 24;

/// Gap between the window buttons
const BUTTON_SPACING: u32 = 10;

/// Padding between the right edge and the rightmost button
const BUTTON_PADDING: u32 = 11;

/// Size of the area in which the button icons are drawn
const ICON_SIZE: f32 = 16.;

/// Width of the icon strokes
const ICON_STROKE: f32 = 1.5;

/// Two clicks on the header within this time toggle maximize
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);

const HEADER: usize = 0;
const TOP_BORDER: usize = 1;
const RIGHT_BORDER: usize = 2;
const BOTTOM_BORDER: usize = 3;
const LEFT_BORDER: usize = 4;

pub struct HeaderBarFrame {
    parent: WlSurface,
    state: WindowState,
    wm_capabilities: WindowManagerCapabilities,
    resizable: bool,
    dirty: bool,
    mouse_location: Location,
    mouse_coords: (i32, i32),
    /// The button that the pointer was pressed on, if any
    pressed_button: Option<UIButton>,
    last_header_press: Option<Instant>,
    /// When `None` the frame is hidden
    render_data: Option<FrameRenderData>,
    should_sync: bool,
    scale_factor: f64,
    queue_handle: QueueHandle<WaylandState>,
    pool: SlotPool,
    subcompositor: Arc<SubcompositorState>,
    /// Buttons, from right to left
    buttons: Vec<UIButton>,
    config: ConfigHandle,
    title: String,
    shaped_title: Option<ShapedTitle>,
    /// Font configuration for the title, at the dpi of the frame buffers
    font_config: Option<(usize, Rc<FontConfiguration>)>,
}

struct ShapedTitle {
    title: String,
    dpi: usize,
    active: bool,
    metrics: FontMetrics,
    glyphs: Vec<ShapedGlyph>,
    width: f32,
}

struct ShapedGlyph {
    info: GlyphInfo,
    glyph: RasterizedGlyph,
}

impl HeaderBarFrame {
    pub fn new(
        parent: &WlSurface,
        shm: &Shm,
        subcompositor: Arc<SubcompositorState>,
        queue_handle: QueueHandle<WaylandState>,
        config: ConfigHandle,
    ) -> anyhow::Result<Self> {
        let pool = SlotPool::new(1, shm)?;
        let render_data = Some(FrameRenderData::new(parent, &subcompositor, &queue_handle));
        let wm_capabilities = WindowManagerCapabilities::all();
        Ok(Self {
            parent: parent.clone(),
            state: WindowState::empty(),
            wm_capabilities,
            resizable: true,
            dirty: true,
            mouse_location: Location::None,
            mouse_coords: (0, 0),
            pressed_button: None,
            last_header_press: None,
            render_data,
            should_sync: true,
            scale_factor: 1.,
            queue_handle,
            pool,
            subcompositor,
            buttons: Self::supported_buttons(wm_capabilities),
            config,
            title: String::new(),
            shaped_title: None,
            font_config: None,
        })
    }

    pub fn set_config(&mut self, config: ConfigHandle) {
        self.config = config;
        self.font_config.take();
        self.shaped_title.take();
        self.dirty = true;
    }

    fn supported_buttons(wm_capabilities: WindowManagerCapabilities) -> Vec<UIButton> {
        let mut buttons = vec![UIButton::Close];
        if wm_capabilities.contains(WindowManagerCapabilities::MAXIMIZE) {
            buttons.push(UIButton::Maximize);
        }
        if wm_capabilities.contains(WindowManagerCapabilities::MINIMIZE) {
            buttons.push(UIButton::Minimize);
        }
        buttons
    }

    /// Whether the window is snapped to screen edges; in that case
    /// the corners are square and there is nothing to resize past.
    fn is_maximized(&self) -> bool {
        self.state.contains(WindowState::MAXIMIZED)
    }

    fn is_square(&self) -> bool {
        self.state
            .intersects(WindowState::MAXIMIZED | WindowState::TILED)
    }

    fn header_width(&self) -> u32 {
        self.render_data
            .as_ref()
            .map(|r| r.parts[HEADER].width)
            .unwrap_or(0)
    }

    /// Left edge (in surface coords) of the button at index idx
    /// (0 is the rightmost)
    fn button_x(width: u32, idx: usize) -> i32 {
        width as i32
            - BUTTON_PADDING as i32
            - BUTTON_SIZE as i32
            - idx as i32 * (BUTTON_SIZE + BUTTON_SPACING) as i32
    }

    fn button_y() -> i32 {
        (HEADER_SIZE as i32 - BUTTON_SIZE as i32) / 2
    }

    fn find_button(&self, x: f64, y: f64) -> Location {
        let width = self.header_width();
        let top = Self::button_y() as f64;
        for (idx, &button) in self.buttons.iter().enumerate() {
            let left = Self::button_x(width, idx) as f64;
            // Be a bit generous with the hit area: half the spacing on
            // each side and the full header height.
            let slop = (BUTTON_SPACING / 2) as f64;
            if x >= left - slop
                && x <= left + BUTTON_SIZE as f64 + slop
                && y >= top - slop
                && y <= top + BUTTON_SIZE as f64 + slop
            {
                return Location::Button(button);
            }
        }
        Location::Head
    }

    fn precise_location(&self, part: usize, x: f64, y: f64) -> Location {
        let parts = &self.render_data.as_ref().unwrap().parts;
        let corner = (BORDER_SIZE + CORNER_SIZE) as f64;
        match part {
            HEADER => self.find_button(x, y),
            TOP_BORDER => {
                let width = parts[TOP_BORDER].width as f64;
                if x <= corner {
                    Location::TopLeft
                } else if x >= width - corner {
                    Location::TopRight
                } else {
                    Location::Top
                }
            }
            BOTTOM_BORDER => {
                let width = parts[BOTTOM_BORDER].width as f64;
                if x <= corner {
                    Location::BottomLeft
                } else if x >= width - corner {
                    Location::BottomRight
                } else {
                    Location::Bottom
                }
            }
            LEFT_BORDER | RIGHT_BORDER => {
                let height = parts[part].height as f64;
                let left = part == LEFT_BORDER;
                if y <= CORNER_SIZE as f64 {
                    if left {
                        Location::TopLeft
                    } else {
                        Location::TopRight
                    }
                } else if y >= height - CORNER_SIZE as f64 {
                    if left {
                        Location::BottomLeft
                    } else {
                        Location::BottomRight
                    }
                } else if left {
                    Location::Left
                } else {
                    Location::Right
                }
            }
            _ => Location::None,
        }
    }

    fn part_index_for_surface(&self, surface_id: &ObjectId) -> Option<usize> {
        self.render_data
            .as_ref()?
            .parts
            .iter()
            .position(|part| &part.surface.id() == surface_id)
    }

    fn colors(&self) -> (SrgbaTuple, SrgbaTuple) {
        let frame = &self.config.window_frame;
        if self.state.contains(WindowState::ACTIVATED) {
            (*frame.active_titlebar_bg, *frame.active_titlebar_fg)
        } else {
            (*frame.inactive_titlebar_bg, *frame.inactive_titlebar_fg)
        }
    }

    fn font_config(&mut self, dpi: usize) -> Option<Rc<FontConfiguration>> {
        if let Some((existing_dpi, font_config)) = self.font_config.as_ref() {
            if *existing_dpi == dpi {
                return Some(Rc::clone(font_config));
            }
        }
        match FontConfiguration::new(Some(self.config.clone()), dpi) {
            Ok(font_config) => {
                let font_config = Rc::new(font_config);
                self.font_config.replace((dpi, Rc::clone(&font_config)));
                Some(font_config)
            }
            Err(err) => {
                log::error!("header frame: failed to set up title font: {err:#}");
                None
            }
        }
    }

    fn reshape_title(&mut self, dpi: usize, fg: SrgbaTuple) -> Option<()> {
        let active = self.state.contains(WindowState::ACTIVATED);
        if self.title.is_empty() {
            self.shaped_title.take();
            return Some(());
        }
        if let Some(existing) = self.shaped_title.as_ref() {
            if existing.title == self.title && existing.dpi == dpi && existing.active == active {
                return Some(());
            }
        }

        let font_config = self.font_config(dpi)?;
        let font = font_config.title_font().ok()?;
        let metrics = font.metrics();
        let infos = font
            .shape(
                &self.title,
                || {},
                |_| {},
                None,
                wezterm_bidi::Direction::LeftToRight,
                None,
                None,
            )
            .ok()?;

        let mut glyphs = vec![];
        let mut width = 0.;
        for info in infos {
            width += info.x_advance.get() as f32;
            if let Ok(mut glyph) = font.rasterize_glyph(info.glyph_pos, info.font_idx) {
                if !glyph.has_color {
                    // Monochrome glyphs are coverage masks; tint them with
                    // the title color (premultiplied RGBA).
                    for p in glyph.data.chunks_exact_mut(4) {
                        let a = p[3] as f32 / 255.;
                        p[0] = (fg.0 * a * 255.) as u8;
                        p[1] = (fg.1 * a * 255.) as u8;
                        p[2] = (fg.2 * a * 255.) as u8;
                    }
                }
                glyphs.push(ShapedGlyph { info, glyph });
            }
        }

        self.shaped_title.replace(ShapedTitle {
            title: self.title.clone(),
            dpi,
            active,
            metrics,
            glyphs,
            width,
        });
        Some(())
    }

    /// Paint the header into pixmap, which is width x HEADER_SIZE
    /// surface pixels at the given integer scale.
    fn paint_header(&mut self, pixmap: &mut Pixmap, width: u32, scale: f32) {
        let (bg, fg) = self.colors();
        let ts = Transform::from_scale(scale, scale);

        // Background, with rounded top corners unless snapped
        let radius = if self.is_square() { 0. } else { CORNER_RADIUS };
        let w = width as f32;
        let h = HEADER_SIZE as f32;
        if let Some(path) = top_rounded_rect(w, h, radius) {
            pixmap.fill_path(&path, &paint(bg, 1.), FillRule::Winding, ts, None);
        }

        // Title, centered in the window but kept clear of the buttons
        let dpi = (crate::DEFAULT_DPI * scale as f64) as usize;
        self.reshape_title(dpi, fg);
        if let Some(shaped) = self.shaped_title.as_ref() {
            let buttons_left = Self::button_x(width, self.buttons.len().saturating_sub(1)) as f32
                - BUTTON_SPACING as f32;
            let limit = buttons_left * scale;
            let title_width = shaped.width;
            let min_x = (BUTTON_PADDING as f32) * scale;
            let mut x = ((w * scale - title_width) / 2.).max(min_x);
            if x + title_width > limit {
                x = (limit - title_width).max(min_x);
            }
            let cell_height = shaped.metrics.cell_height.get() as f32;
            let baseline = ((h * scale - cell_height) / 2.)
                + cell_height
                + shaped.metrics.descender.get() as f32;

            let paint = PixmapPaint::default();
            for item in &shaped.glyphs {
                let advance = item.info.x_advance.get() as f32;
                if x + advance > limit {
                    break;
                }
                if let Some(data) = PixmapRef::from_bytes(
                    &item.glyph.data,
                    item.glyph.width as u32,
                    item.glyph.height as u32,
                ) {
                    pixmap.draw_pixmap(
                        (x + (item.info.x_offset.get() + item.glyph.bearing_x.get()) as f32)
                            .round() as i32,
                        (baseline
                            - (item.glyph.bearing_y.get() + item.info.y_offset.get()) as f32)
                            .round() as i32,
                        data,
                        &paint,
                        Transform::identity(),
                        None,
                    );
                }
                x += advance;
            }
        }

        // Buttons: a faint round background that brightens on hover
        // and press, with a thin symbolic icon on top.
        let active = self.state.contains(WindowState::ACTIVATED);
        for (idx, &button) in self.buttons.iter().enumerate() {
            let left = Self::button_x(width, idx) as f32;
            let top = Self::button_y() as f32;
            let hovered = self.mouse_location == Location::Button(button);
            let pressed = hovered && self.pressed_button == Some(button);
            let alpha = if pressed {
                0.30
            } else if hovered {
                0.15
            } else if active {
                0.10
            } else {
                0.05
            };
            let r = BUTTON_SIZE as f32 / 2.;
            if let Some(circle) = PathBuilder::from_circle(left + r, top + r, r) {
                pixmap.fill_path(&circle, &paint(fg, alpha), FillRule::Winding, ts, None);
            }

            let icon_left = left + (BUTTON_SIZE as f32 - ICON_SIZE) / 2.;
            let icon_top = top + (BUTTON_SIZE as f32 - ICON_SIZE) / 2.;
            let icon = match button {
                UIButton::Maximize if self.is_maximized() => Icon::Restore,
                UIButton::Maximize => Icon::Maximize,
                UIButton::Minimize => Icon::Minimize,
                UIButton::Close => Icon::Close,
            };
            draw_icon(
                pixmap,
                icon,
                Transform::from_translate(icon_left, icon_top).post_scale(scale, scale),
                fg,
            );
        }
    }
}

impl DecorationsFrame for HeaderBarFrame {
    fn set_scaling_factor(&mut self, scale_factor: f64) {
        if self.scale_factor != scale_factor {
            self.scale_factor = scale_factor;
            self.dirty = true;
            self.should_sync = true;
        }
    }

    fn on_click(
        &mut self,
        _timestamp: Duration,
        click: FrameClick,
        pressed: bool,
    ) -> Option<FrameAction> {
        if click == FrameClick::Alternate {
            return if pressed
                && self.mouse_location == Location::Head
                && self
                    .wm_capabilities
                    .contains(WindowManagerCapabilities::WINDOW_MENU)
            {
                // Relative to the base surface; the header sits above it
                Some(FrameAction::ShowMenu(
                    self.mouse_coords.0,
                    self.mouse_coords.1 - HEADER_SIZE as i32,
                ))
            } else {
                None
            };
        }

        let resize = pressed && self.resizable;
        match self.mouse_location {
            Location::Head if pressed => {
                let now = Instant::now();
                let double = self
                    .last_header_press
                    .map(|prior| now.duration_since(prior) <= DOUBLE_CLICK_TIME)
                    .unwrap_or(false);
                if double {
                    self.last_header_press = None;
                    if !self
                        .wm_capabilities
                        .contains(WindowManagerCapabilities::MAXIMIZE)
                    {
                        None
                    } else if self.is_maximized() {
                        Some(FrameAction::UnMaximize)
                    } else {
                        Some(FrameAction::Maximize)
                    }
                } else {
                    self.last_header_press = Some(now);
                    Some(FrameAction::Move)
                }
            }
            Location::Button(button) => {
                if pressed {
                    self.pressed_button = Some(button);
                    self.dirty = true;
                    None
                } else {
                    let was_pressed = self.pressed_button.take() == Some(button);
                    self.dirty = true;
                    if !was_pressed {
                        return None;
                    }
                    match button {
                        UIButton::Close => Some(FrameAction::Close),
                        UIButton::Minimize => Some(FrameAction::Minimize),
                        UIButton::Maximize if self.is_maximized() => {
                            Some(FrameAction::UnMaximize)
                        }
                        UIButton::Maximize => Some(FrameAction::Maximize),
                    }
                }
            }
            Location::Top if resize => Some(FrameAction::Resize(ResizeEdge::Top)),
            Location::TopLeft if resize => Some(FrameAction::Resize(ResizeEdge::TopLeft)),
            Location::Left if resize => Some(FrameAction::Resize(ResizeEdge::Left)),
            Location::BottomLeft if resize => Some(FrameAction::Resize(ResizeEdge::BottomLeft)),
            Location::Bottom if resize => Some(FrameAction::Resize(ResizeEdge::Bottom)),
            Location::BottomRight if resize => Some(FrameAction::Resize(ResizeEdge::BottomRight)),
            Location::Right if resize => Some(FrameAction::Resize(ResizeEdge::Right)),
            Location::TopRight if resize => Some(FrameAction::Resize(ResizeEdge::TopRight)),
            _ => {
                if !pressed && self.pressed_button.take().is_some() {
                    self.dirty = true;
                }
                None
            }
        }
    }

    fn click_point_moved(
        &mut self,
        _timestamp: Duration,
        surface_id: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<CursorIcon> {
        let part = self.part_index_for_surface(surface_id)?;
        let old_location = self.mouse_location;
        self.mouse_coords = (x as i32, y as i32);
        self.mouse_location = self.precise_location(part, x, y);

        // Repaint the header when the hovered button changes
        self.dirty |= (matches!(old_location, Location::Button(_))
            || matches!(self.mouse_location, Location::Button(_)))
            && old_location != self.mouse_location;

        Some(match self.mouse_location {
            Location::Top => CursorIcon::NResize,
            Location::TopRight => CursorIcon::NeResize,
            Location::Right => CursorIcon::EResize,
            Location::BottomRight => CursorIcon::SeResize,
            Location::Bottom => CursorIcon::SResize,
            Location::BottomLeft => CursorIcon::SwResize,
            Location::Left => CursorIcon::WResize,
            Location::TopLeft => CursorIcon::NwResize,
            _ => CursorIcon::Default,
        })
    }

    fn click_point_left(&mut self) {
        if matches!(self.mouse_location, Location::Button(_)) || self.pressed_button.is_some() {
            self.dirty = true;
        }
        self.mouse_location = Location::None;
        self.pressed_button = None;
    }

    fn set_hidden(&mut self, hidden: bool) {
        if self.is_hidden() == hidden {
            return;
        }
        if hidden {
            self.render_data = None;
        } else {
            let _ = self.pool.resize(1);
            self.render_data = Some(FrameRenderData::new(
                &self.parent,
                &self.subcompositor,
                &self.queue_handle,
            ));
            self.dirty = true;
            self.should_sync = true;
        }
    }

    fn set_resizable(&mut self, resizable: bool) {
        self.resizable = resizable;
    }

    fn update_state(&mut self, state: WindowState) {
        let difference = self.state.symmetric_difference(state);
        self.state = state;
        self.dirty |= difference.intersects(
            WindowState::ACTIVATED
                | WindowState::FULLSCREEN
                | WindowState::MAXIMIZED
                | WindowState::TILED,
        );
    }

    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        let parts = &mut self
            .render_data
            .as_mut()
            .expect("trying to resize hidden frame")
            .parts;

        let width = width.get();
        let height = height.get();

        parts[HEADER].width = width;

        parts[TOP_BORDER].width = width + 2 * BORDER_SIZE;

        parts[BOTTOM_BORDER].width = width + 2 * BORDER_SIZE;
        parts[BOTTOM_BORDER].pos.1 = height as i32;

        parts[LEFT_BORDER].height = height + HEADER_SIZE;

        parts[RIGHT_BORDER].height = parts[LEFT_BORDER].height;
        parts[RIGHT_BORDER].pos.0 = width as i32;

        self.dirty = true;
        self.should_sync = true;
    }

    // The window geometry is the header + content; the resize borders
    // lie outside of it.
    fn subtract_borders(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        if self.state.contains(WindowState::FULLSCREEN) || self.render_data.is_none() {
            (Some(width), Some(height))
        } else {
            (
                Some(width),
                NonZeroU32::new(height.get().saturating_sub(HEADER_SIZE)),
            )
        }
    }

    fn add_borders(&self, width: u32, height: u32) -> (u32, u32) {
        if self.state.contains(WindowState::FULLSCREEN) || self.render_data.is_none() {
            (width, height)
        } else {
            (width, height + HEADER_SIZE)
        }
    }

    fn is_hidden(&self) -> bool {
        self.render_data.is_none()
    }

    fn location(&self) -> (i32, i32) {
        if self.state.contains(WindowState::FULLSCREEN) || self.is_hidden() {
            (0, 0)
        } else {
            self.render_data.as_ref().unwrap().parts[HEADER].pos
        }
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn draw(&mut self) -> bool {
        if self.render_data.is_none() {
            return false;
        }

        self.dirty = false;
        let should_sync = mem::take(&mut self.should_sync);

        let fullscreen = self.state.contains(WindowState::FULLSCREEN);
        // GTK-style integer buffer scale; the compositor scales it
        // to a fractional output scale.
        let scale = self.scale_factor.ceil().max(1.) as u32;

        for idx in 0..5 {
            let (width, height) = {
                let part = &self.render_data.as_ref().unwrap().parts[idx];
                (part.width, part.height)
            };

            // Nothing to show when fullscreen, and no resize margins
            // when maximized.
            let unmapped = fullscreen || (idx != HEADER && self.is_maximized());
            if unmapped || width == 0 || height == 0 {
                let part = &self.render_data.as_ref().unwrap().parts[idx];
                part.surface.attach(None, 0, 0);
                part.surface.commit();
                continue;
            }

            let mut pixmap = match Pixmap::new(width * scale, height * scale) {
                Some(p) => p,
                None => continue,
            };
            // The resize margins stay fully transparent; they only
            // need to exist to receive pointer input.
            if idx == HEADER {
                self.paint_header(&mut pixmap, width, scale as f32);
            }

            let (buffer, canvas) = match self.pool.create_buffer(
                (width * scale) as i32,
                (height * scale) as i32,
                (width * scale * 4) as i32,
                wl_shm::Format::Argb8888,
            ) {
                Ok(b) => b,
                Err(err) => {
                    log::error!("header frame: failed to allocate buffer: {err:#}");
                    continue;
                }
            };

            // tiny-skia is premultiplied RGBA; wl_shm Argb8888 is
            // premultiplied BGRA in memory on little endian.
            for (dst, src) in canvas
                .chunks_exact_mut(4)
                .zip(pixmap.data().chunks_exact(4))
            {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = src[3];
            }

            let part = &self.render_data.as_ref().unwrap().parts[idx];
            part.surface.set_buffer_scale(scale as i32);
            if should_sync {
                part.subsurface.set_sync();
            } else {
                part.subsurface.set_desync();
            }
            part.subsurface.set_position(part.pos.0, part.pos.1);

            if let Err(err) = buffer.attach_to(&part.surface) {
                log::error!("header frame: failed to attach buffer: {err:#}");
                continue;
            }
            if part.surface.version() >= 4 {
                part.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            } else {
                part.surface.damage(0, 0, i32::MAX, i32::MAX);
            }
            part.surface.commit();
        }

        should_sync
    }

    fn update_wm_capabilities(&mut self, capabilities: WindowManagerCapabilities) {
        self.dirty |= self.wm_capabilities != capabilities;
        self.wm_capabilities = capabilities;
        self.buttons = Self::supported_buttons(capabilities);
    }

    fn set_title(&mut self, title: impl Into<String>) {
        let title = title.into();
        if title != self.title {
            self.title = title;
            self.dirty = true;
        }
    }
}

fn paint(color: SrgbaTuple, alpha: f32) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(
        Color::from_rgba(color.0, color.1, color.2, (color.3 * alpha).clamp(0., 1.))
            .unwrap_or(Color::BLACK),
    );
    paint
}

/// A w x h rectangle with its top two corners rounded by radius
fn top_rounded_rect(w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    if radius <= 0. {
        pb.push_rect(tiny_skia::Rect::from_xywh(0., 0., w, h)?);
        return pb.finish();
    }
    // Control point distance approximating a quarter circle with a cubic
    let k = radius * 0.552_284_8;
    pb.move_to(0., h);
    pb.line_to(0., radius);
    pb.cubic_to(0., radius - k, radius - k, 0., radius, 0.);
    pb.line_to(w - radius, 0.);
    pb.cubic_to(w - radius + k, 0., w, radius - k, w, radius);
    pb.line_to(w, h);
    pb.close();
    pb.finish()
}

#[derive(Debug, Clone, Copy)]
enum Icon {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// Draw a symbolic icon designed on a 16x16 grid; ts maps that grid
/// into the pixmap.
fn draw_icon(pixmap: &mut Pixmap, icon: Icon, ts: Transform, color: SrgbaTuple) {
    let mut pb = PathBuilder::new();
    match icon {
        Icon::Minimize => {
            pb.move_to(4.5, 11.5);
            pb.line_to(11.5, 11.5);
        }
        Icon::Maximize => {
            if let Some(rect) = tiny_skia::Rect::from_ltrb(4.5, 4.5, 11.5, 11.5) {
                pb.push_rect(rect);
            }
        }
        Icon::Restore => {
            // Front window, and the corner of the one behind it
            if let Some(rect) = tiny_skia::Rect::from_ltrb(4.5, 6.5, 9.5, 11.5) {
                pb.push_rect(rect);
            }
            pb.move_to(6.5, 4.5);
            pb.line_to(11.5, 4.5);
            pb.line_to(11.5, 9.5);
        }
        Icon::Close => {
            pb.move_to(5., 5.);
            pb.line_to(11., 11.);
            pb.move_to(11., 5.);
            pb.line_to(5., 11.);
        }
    }
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width: ICON_STROKE,
            line_cap: LineCap::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint(color, 1.), &stroke, ts, None);
    }
}

struct FrameRenderData {
    parts: [FramePart; 5],
}

impl FrameRenderData {
    fn new(
        parent: &WlSurface,
        subcompositor: &SubcompositorState,
        queue_handle: &QueueHandle<WaylandState>,
    ) -> Self {
        let border = BORDER_SIZE as i32;
        let header = HEADER_SIZE as i32;
        let part = |width, height, pos| {
            FramePart::new(
                subcompositor.create_subsurface(parent.clone(), queue_handle),
                width,
                height,
                pos,
            )
        };
        Self {
            parts: [
                // Header
                part(0, HEADER_SIZE, (0, -header)),
                // Top border
                part(0, BORDER_SIZE, (-border, -(header + border))),
                // Right border
                part(BORDER_SIZE, 0, (0, -header)),
                // Bottom border
                part(0, BORDER_SIZE, (-border, 0)),
                // Left border
                part(BORDER_SIZE, 0, (-border, -header)),
            ],
        }
    }
}

struct FramePart {
    subsurface: WlSubsurface,
    surface: WlSurface,
    /// Size in surface-local pixels
    width: u32,
    height: u32,
    /// Position relative to the parent surface
    pos: (i32, i32),
}

impl FramePart {
    fn new(surfaces: (WlSubsurface, WlSurface), width: u32, height: u32, pos: (i32, i32)) -> Self {
        let (subsurface, surface) = surfaces;
        subsurface.set_sync();
        Self {
            surface,
            subsurface,
            width,
            height,
            pos,
        }
    }
}

impl Drop for FramePart {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Location {
    None,
    Head,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    TopLeft,
    Button(UIButton),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum UIButton {
    Minimize,
    Maximize,
    Close,
}
