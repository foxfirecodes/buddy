//! Phase colors and marks shared by the native client and its secondary windows.
use eframe::egui::{
    self, Color32, FontId, Id, Painter, Pos2, Rect, RichText, Shape, Stroke, Ui, pos2, vec2,
};

pub const INK: Color32 = Color32::from_rgb(0x10, 0x10, 0x11);
pub const PANEL: Color32 = Color32::from_rgb(0x19, 0x19, 0x1a);
pub const LINE: Color32 = Color32::from_rgb(0x34, 0x34, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xf2, 0xee, 0xe7);
pub const MUTED: Color32 = Color32::from_rgb(0xaa, 0xa8, 0xa5);
pub const ORANGE: Color32 = Color32::from_rgb(0xff, 0x68, 0x1e);
pub const ACCENT_INK: Color32 = Color32::from_rgb(0x17, 0x10, 0x0c);

pub fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(16.));
        s.text_styles.insert(Button, FontId::proportional(15.));
        s.text_styles.insert(Heading, FontId::proportional(29.));
        s.text_styles.insert(Monospace, FontId::monospace(14.));
        s.spacing.button_padding = vec2(12., 6.);
        s.spacing.item_spacing = vec2(8., 8.);
        s.spacing.interact_size.y = 32.;
        let v = &mut s.visuals;
        v.panel_fill = INK;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1_f32, LINE);
        v.faint_bg_color = PANEL;
        v.extreme_bg_color = Color32::from_rgb(0x14, 0x14, 0x15);
        v.text_edit_bg_color = Some(v.extreme_bg_color);
        v.code_bg_color = v.extreme_bg_color;
        v.weak_text_color = Some(MUTED);
        v.hyperlink_color = ORANGE;
        v.warn_fg_color = ORANGE;
        v.error_fg_color = Color32::from_rgb(0xff, 0x98, 0x79);
        v.selection.bg_fill = Color32::from_rgb(0x52, 0x30, 0x22);
        v.selection.stroke = Stroke::new(1.5_f32, ORANGE);
        v.text_cursor.stroke = Stroke::new(2_f32, ORANGE);
        v.window_corner_radius = 7.into();
        v.menu_corner_radius = 5.into();
        let control = Color32::from_rgb(0x25, 0x25, 0x27);
        let hover = Color32::from_rgb(0x32, 0x2b, 0x27);
        let widgets = &mut v.widgets;
        for (state, fill, border) in [
            (&mut widgets.noninteractive, PANEL, LINE),
            (&mut widgets.inactive, control, LINE),
            (
                &mut widgets.hovered,
                hover,
                Color32::from_rgb(0xa8, 0x72, 0x57),
            ),
            (&mut widgets.active, hover, ORANGE),
            (&mut widgets.open, hover, ORANGE),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = Stroke::new(1_f32, border);
            state.fg_stroke = Stroke::new(1.5_f32, TEXT);
            state.corner_radius = 5.into();
        }
    });
    // Decode the approved wordmark once per GUI context, including on high-DPI displays.
    let art = eframe::icon_data::from_png_bytes(include_bytes!("../assets/phase-brand.png"))
        .expect("embedded Phase wordmark");
    let texture = ctx.load_texture(
        "Phase wordmark",
        egui::ColorImage::from_rgba_unmultiplied(
            [art.width as usize, art.height as usize],
            &art.rgba,
        ),
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|data| data.insert_temp(Id::new("phase-brand"), texture));
}

pub fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/phase-icon.png"))
        .expect("embedded Phase icon")
}

pub fn brand(ui: &mut Ui) {
    let texture = ui
        .ctx()
        .data(|data| data.get_temp::<egui::TextureHandle>(Id::new("phase-brand")));
    if let Some(texture) = texture {
        ui.add(egui::Image::new((texture.id(), vec2(148.5, 33.))).alt_text("Buddy"));
    }
}

pub fn primary(label: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label).strong().color(ACCENT_INK)).fill(ORANGE)
}

pub fn panel() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1_f32, LINE))
        .corner_radius(7)
        .inner_margin(14)
}

/// An empty track is useful during phases that have no measurable byte total.
pub fn traces(painter: &Painter, rect: Rect, progress: Option<f32>) {
    if !rect.is_positive() {
        return;
    }
    let track = painter.with_clip_rect(rect);
    let end = rect.left() + rect.width() * progress.unwrap_or(0.).clamp(0., 1.);
    let lit = track.with_clip_rect(Rect::from_min_max(rect.min, pos2(end, rect.bottom())));
    let x = rect.left() + rect.width() * 0.43;
    let rise = (rect.height() * 0.37).min(rect.width() * 0.2);
    let pitch = (rect.height() * 0.15).min(3.8);
    for i in -1..=1 {
        let y = rect.center().y + rise * 0.5 + i as f32 * pitch;
        let points = [
            pos2(rect.left(), y),
            pos2(x, y),
            pos2(x + rise * 1.3, y - rise),
            pos2(rect.right(), y - rise),
        ];
        for pair in points.windows(2) {
            track.line_segment([pair[0], pair[1]], Stroke::new(1.6_f32, LINE));
            if progress.is_some() {
                lit.line_segment([pair[0], pair[1]], Stroke::new(1.6_f32, ORANGE));
            }
        }
    }
}

/// The ID fixes the disk's angle and label for this game in tiles and banners.
pub fn disk(painter: &Painter, rect: Rect, id: &str) {
    let p = painter.with_clip_rect(rect);
    p.rect_filled(rect, 7, Color32::from_rgb(0x24, 0x21, 0x20));
    let seed = id
        .chars()
        .fold(5381_u32, |hash, c| hash.wrapping_mul(33) ^ c as u32);
    let side = (rect.width() * 0.4).min(rect.height() * 1.18);
    let center = rect.min + rect.size() * vec2(0.74, 0.54);
    let angle = egui::emath::Rot2::from_angle(((seed % 13) as f32 - 6.).to_radians());
    let point =
        |x: f32, y: f32| -> Pos2 { center + angle * (vec2(x - 50., y - 50.) * (side / 100.)) };
    let plate = |x, y, w, h, color| {
        p.add(Shape::convex_polygon(
            vec![
                point(x, y),
                point(x + w, y),
                point(x + w, y + h),
                point(x, y + h),
            ],
            color,
            Stroke::NONE,
        ));
    };
    let shell = [
        point(1., 1.),
        point(84., 1.),
        point(99., 16.),
        point(99., 98.),
        point(1., 98.),
    ];
    p.add(Shape::convex_polygon(
        shell.to_vec(),
        Color32::from_rgb(0x34, 0x31, 0x31),
        Stroke::new(1_f32, Color32::from_rgb(0x70, 0x67, 0x61)),
    ));
    let slot = Color32::from_gray(23);
    plate(21., 1., 54., 34., slot);
    plate(24., 1., 47., 31., Color32::from_rgb(0x77, 0x72, 0x6e));
    plate(53., 4., 10., 23., slot);
    plate(10., 49., 80., 44., Color32::from_rgb(0xc3, 0xb9, 0xa7));
    plate(10., 48., 80., 7., Color32::from_rgb(0x30, 0x2e, 0x2b));
    let bend = (25 + ((seed >> 8) % 38)) as f32;
    let step = if seed & 1 == 1 { -11. } else { 11. };
    for i in 0..3 {
        let y = 67. + i as f32 * 5.;
        p.add(Shape::line(
            vec![
                point(10., y),
                point(bend, y),
                point(bend + 15., y + step),
                point(90., y + step),
            ],
            Stroke::new((side * 0.02).max(1.), Color32::from_rgb(0xa7, 0x47, 0x20)),
        ));
    }
    plate(4., 88., 4., 5., slot);
    plate(92., 88., 4., 5., slot);
}
