# Embedded fonts

Fonts are compiled into the GUI executable; runtime downloads and system font
installation are not required. Both font licenses and this notice accompany the
GUI ZIP, tar.gz and DEB packages. CLI-only builds do not embed these fonts.

## Cascadia Next SC NF (primary)

- Embedded file: `CascadiaNextSCNF-Regular.ttf`
- Version: v1.0.1
- Source: <https://github.com/LiLittleCat/Cascadia-Next-Nerd-Font>
- Release archive SHA-256: `03bfe7040816e56473fd0f9f89ceaa14d9e26ba750967a750c2f6aba6c79b3e7`
- Embedded font SHA-256: `4308f91de984282871f09604ebdc01adfe8fbdc6b30cc668e3fb0c9a0e914009`
- License: SIL Open Font License 1.1; see [LICENSE-OFL.txt](LICENSE-OFL.txt).

## Noto Sans Mono (symbol fallback)

- Embedded file: `NotoSansMono-Regular.ttf`
- Font version: 2.007, Regular, upstream hinted TTF (ttfautohint v1.8)
- Size: 512,836 bytes (approximately 501 KiB)
- Source repository: <https://github.com/notofonts/noto-fonts>
- Fixed source commit: `ffebf8c1ee449e544955a7e813c54f9b73848eac`
- Original file: [hinted/ttf/NotoSansMono/NotoSansMono-Regular.ttf](https://raw.githubusercontent.com/notofonts/noto-fonts/ffebf8c1ee449e544955a7e813c54f9b73848eac/hinted/ttf/NotoSansMono/NotoSansMono-Regular.ttf)
- Source and embedded font SHA-256: `d9e2b23d19f8230be7146f409a52b1d23117e635e28f2e2892cf91b7382f325b`
- Font copyright (from its name table): Copyright 2015-2021 Google LLC. All Rights Reserved.
- License: SIL Open Font License 1.1; see [LICENSE-NOTO-OFL.txt](LICENSE-NOTO-OFL.txt).
  This includes the font's copyright notice followed by the unchanged upstream
  [LICENSE](https://raw.githubusercontent.com/notofonts/noto-fonts/ffebf8c1ee449e544955a7e813c54f9b73848eac/LICENSE)
  (upstream license SHA-256: `0dab92d0544f7b233403f14b84a663bdbfa746982eda629e7f4f9ffe1b036feb`).

This is the complete, unmodified upstream font, with its original names and
metadata. There is no subsetting, conversion or font-generation dependency.
The modest original size avoids maintaining a derived font and its build tools.
To reproduce the embedded asset, download the immutable URL above and verify
the SHA-256; the resulting bytes must match the checked-in TTF exactly.

The font includes the Currency Symbols block U+20A0–U+20BF, including the Bitcoin
sign `₿` (U+20BF) missing from Cascadia and the current egui defaults. It is appended
after the existing fonts in both the monospace and proportional families, keeping
Cascadia first and preserving egui's defaults. Only glyph selection changes; log
text, Unicode codepoints and copy content must remain intact.

`src/font_fallback.rs` contains regression checks using egui's actual font lookup,
text layout and rasterized atlas at 1.0 and 1.5 pixels per point. These checks do
not replace native GUI screenshots at normal and 1.5× scaling or native clipboard
verification during the X03 experience review.
