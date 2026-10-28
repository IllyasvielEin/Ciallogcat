//! Offline fallback for symbols missing from Cascadia and egui's default fonts.
//! See assets/fonts/README.md for the pinned, unmodified font and its license.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily};

const SYMBOL_FALLBACK_NAME: &str = "Noto Sans Mono";

/// Call after the application's primary fonts have been ordered.
pub(super) fn install(fonts: &mut FontDefinitions) {
    fonts.font_data.insert(
        SYMBOL_FALLBACK_NAME.to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSansMono-Regular.ttf"
        ))),
    );

    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let fallbacks = fonts.families.entry(family).or_default();
        if !fallbacks.iter().any(|name| name == SYMBOL_FALLBACK_NAME) {
            fallbacks.push(SYMBOL_FALLBACK_NAME.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Color32, FontId, epaint::text::Fonts};

    fn primary_and_default_fonts() -> FontDefinitions {
        let mut fonts = FontDefinitions::default();
        let primary = "Cascadia Next SC NF";
        fonts.font_data.insert(
            primary.to_owned(),
            Arc::new(FontData::from_static(include_bytes!(
                "../assets/fonts/CascadiaNextSCNF-Regular.ttf"
            ))),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .get_mut(&family)
                .unwrap()
                .insert(0, primary.to_owned());
        }
        fonts
    }

    #[test]
    fn install_preserves_primary_and_existing_fallback_priority() {
        let mut definitions = primary_and_default_fonts();
        let original = definitions.clone();
        install(&mut definitions);
        install(&mut definitions);

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let before = &original.families[&family];
            let after = &definitions.families[&family];
            assert_eq!(&after[..before.len()], before);
            assert_eq!(after.len(), before.len() + 1);
            assert_eq!(after.last().unwrap(), SYMBOL_FALLBACK_NAME);
        }
        for (name, data) in &original.font_data {
            assert!(Arc::ptr_eq(data, &definitions.font_data[name]));
        }
    }

    #[test]
    fn bitcoin_has_visible_glyph_without_replacing_text_at_both_scales() {
        let mut definitions = primary_and_default_fonts();
        let mut original = Fonts::new(Default::default(), definitions.clone());
        install(&mut definitions);
        let mut installed = Fonts::new(Default::default(), definitions);
        let text = "amount=0.125 ₿ 中文 →";

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let font_id = FontId::new(12.5, family);
            // Reproduce X03: the previous embedded font chain has no Bitcoin glyph.
            assert!(!original.has_glyph(&font_id, '\u{20bf}'));
            assert!(installed.has_glyph(&font_id, '\u{20bf}'));

            for scale in [1.0, 1.5] {
                let mut view = installed.with_pixels_per_point(scale);
                let galley = view.layout_no_wrap(text.to_owned(), font_id.clone(), Color32::WHITE);
                assert_eq!(galley.job.text.as_bytes(), text.as_bytes());
                let laid_out: String = galley
                    .rows
                    .iter()
                    .flat_map(|row| row.glyphs.iter().map(|glyph| glyph.chr))
                    .collect();
                assert_eq!(laid_out, text);

                let bitcoin = galley
                    .rows
                    .iter()
                    .flat_map(|row| &row.glyphs)
                    .find(|glyph| glyph.chr == '\u{20bf}')
                    .unwrap();
                assert!(bitcoin.advance_width > 0.0);
                assert!(!bitcoin.uv_rect.is_nothing());

                // A nonempty replacement box would pass a mesh-only check.
                // Compare atlas allocations with a genuinely unsupported codepoint too.
                let missing =
                    view.layout_no_wrap("\u{10ffff}".to_owned(), font_id.clone(), Color32::WHITE);
                assert_ne!(bitcoin.uv_rect, missing.rows[0].glyphs[0].uv_rect);

                // Exercise actual glyph rasterization, rather than only font registration.
                let image = view.image();
                let uv = bitcoin.uv_rect;
                assert!((uv.min[1]..uv.max[1]).any(|y| {
                    (uv.min[0]..uv.max[0]).any(|x| {
                        image.pixels[usize::from(y) * image.size[0] + usize::from(x)].a() > 0
                    })
                }));
            }
        }
    }
}
