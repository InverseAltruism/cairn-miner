//! Cairn visual identity for the launcher: the CRT-phosphor terminal palette
//! (near-black background, monospace everything, one phosphor green + one gold
//! accent) applied to egui, plus a couple of pure formatting helpers.

use egui::{Color32, FontFamily, FontId, TextStyle};

// --- Cairn brand tokens (dark only; see the cairn design system) ---
pub const BG: Color32 = Color32::from_rgb(0x05, 0x05, 0x05);
pub const PANEL: Color32 = Color32::from_rgb(0x0a, 0x0c, 0x0b);
pub const PANEL2: Color32 = Color32::from_rgb(0x0d, 0x0f, 0x0e);
pub const LINE: Color32 = Color32::from_rgb(0x1b, 0x1f, 0x1d);
pub const LINE_HOVER: Color32 = Color32::from_rgb(0x24, 0x2a, 0x27);
pub const GREEN: Color32 = Color32::from_rgb(0x57, 0xd9, 0x77);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xd2, 0x4a);
pub const FG: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
pub const DIM: Color32 = Color32::from_rgb(0x6f, 0x6f, 0x6f);
pub const DIM2: Color32 = Color32::from_rgb(0xa6, 0xa6, 0xa6);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);

/// Install the cairn look: dark phosphor visuals + monospace text styles,
/// applied to every theme so the OS light/dark preference can't override it.
pub fn apply(ctx: &egui::Context) {
    let visuals = build_visuals();
    let text_styles: std::collections::BTreeMap<TextStyle, FontId> = [
        (TextStyle::Small, FontId::new(10.0, FontFamily::Monospace)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Monospace)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Monospace)),
        (TextStyle::Heading, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into();
    ctx.all_styles_mut(|style| {
        style.visuals = visuals.clone();
        style.text_styles = text_styles.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
    });
}

fn build_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = egui::Stroke::new(1.0, LINE);
    visuals.extreme_bg_color = PANEL2;
    visuals.faint_bg_color = PANEL2;
    visuals.override_text_color = Some(FG);
    visuals.hyperlink_color = GREEN;
    visuals.selection.bg_fill = GREEN.linear_multiply(0.35);
    visuals.selection.stroke = egui::Stroke::new(1.0, GREEN);

    // Widgets: dark inset panels with a 1px cairn hairline that warms on hover.
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, DIM2);
    w.inactive.bg_fill = PANEL2;
    w.inactive.bg_stroke = egui::Stroke::new(1.0, LINE);
    w.inactive.fg_stroke = egui::Stroke::new(1.0, FG);
    w.hovered.bg_fill = PANEL2;
    w.hovered.bg_stroke = egui::Stroke::new(1.0, LINE_HOVER);
    w.hovered.fg_stroke = egui::Stroke::new(1.0, GREEN);
    w.active.bg_fill = PANEL2;
    w.active.bg_stroke = egui::Stroke::new(1.0, GREEN);
    w.active.fg_stroke = egui::Stroke::new(1.0, GREEN);
    visuals
}

/// Format a hash rate (hashes/second) with an auto-scaled SI-ish unit, matching
/// how the pool dashboard renders rates (`22.40 MH/s`).
pub fn format_hashrate(hps: f64) -> String {
    const UNITS: [&str; 5] = ["H/s", "kH/s", "MH/s", "GH/s", "TH/s"];
    if hps <= 0.0 || !hps.is_finite() {
        return "0.00 H/s".to_string();
    }
    let mut v = hps;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

/// Split a hash rate into (number, unit) for two-tone stat-tile rendering.
pub fn split_hashrate(hps: f64) -> (String, String) {
    let s = format_hashrate(hps);
    match s.split_once(' ') {
        Some((n, u)) => (n.to_string(), u.to_string()),
        None => (s, String::new()),
    }
}

/// Human-readable elapsed time: `1d 02h 03m`, `2h 03m 04s`, `03m 04s`, `12s`.
pub fn format_duration(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h:02}h {m:02}m")
    } else if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashrate_scales_units() {
        assert_eq!(format_hashrate(0.0), "0.00 H/s");
        assert_eq!(format_hashrate(-5.0), "0.00 H/s");
        assert_eq!(format_hashrate(950.0), "950.00 H/s");
        assert_eq!(format_hashrate(22_400_000.0), "22.40 MH/s");
        assert_eq!(format_hashrate(1_500_000_000.0), "1.50 GH/s");
        assert_eq!(format_hashrate(2_493_638_959_233.0), "2.49 TH/s");
    }

    #[test]
    fn hashrate_splits_number_and_unit() {
        assert_eq!(
            split_hashrate(22_400_000.0),
            ("22.40".to_string(), "MH/s".to_string())
        );
    }

    #[test]
    fn duration_formats_by_magnitude() {
        assert_eq!(format_duration(12), "12s");
        assert_eq!(format_duration(184), "3m 04s");
        assert_eq!(format_duration(7_384), "2h 03m 04s");
        assert_eq!(format_duration(93_784), "1d 02h 03m");
    }
}
