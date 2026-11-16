//! Label text rasteriser for the hint overlay.
//!
//! Turns label strings into 8-bit glyph-coverage masks ([`Coverage`])
//! that the overlay composites in the label colour over its translucent
//! badges. The overlay never draws text itself, so the rasteriser can
//! change without touching the compositor.
//!
//! The primary path is DirectWrite through a software Direct2D DC render
//! target with grayscale antialiasing: grayscale coverage lands in the
//! alpha channel, which is what lets glyphs stay fully opaque on a
//! see-through badge (ClearType needs an opaque background). If Direct2D
//! or DirectWrite can't be created, GDI grayscale text renders the same
//! masks so the overlay stays usable.
//!
//! Masks are made once per pick: every label goes into one scratch
//! "atlas" surface in a single draw pass, and per-keystroke frames just
//! blit the cached masks. Fonts and measured advances are cached by
//! pixel height, so a [`LabelText`] kept alive across picks serves any
//! mix of badge sizes.

use std::ffi::c_void;

use anyhow::{anyhow, Context, Result};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{BOOL, COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_BOLD, FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};

/// Overlay font. Consolas is a clean monospace font shipped with every
/// supported Windows version, and it draws lower-case `l` and capital
/// `I` distinctly (the Segoe UI default used through v0.3.0 collapsed
/// the two on small badges; issue #4). DirectWrite and GDI both
/// substitute the closest face if it's missing.
const FAMILY: &str = "Consolas";

/// Tallest atlas surface drawn in one pass. Larger picks take several.
const MAX_ATLAS_H: i32 = 4096;
/// Narrowest atlas surface; wider when a single item needs it.
const MIN_ATLAS_W: i32 = 1024;
/// Empty pixels between atlas items so antialiasing can't bleed across.
const ATLAS_GAP: i32 = 2;

/// What a piece of text is, which decides its font and alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// Matchable hint label: bold, centred.
    Label,
    /// Window-title "extra" pill: regular weight, left-aligned,
    /// trimmed with an ellipsis when it doesn't fit.
    Title,
}

/// A font size and role. `px` is the GDI cell height (ascent + descent)
/// in physical pixels, i.e. `HintStyle::font_height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Font {
    pub px: i32,
    pub role: Role,
}

/// One string to rasterise into a `w` × `h` box.
pub(crate) struct TextItem<'a> {
    pub text: &'a str,
    pub font: Font,
    pub w: i32,
    pub h: i32,
}

/// Glyph coverage for one [`TextItem`]: `w * h` bytes, row-major,
/// 0 = no ink, 255 = fully covered.
#[derive(Debug, Clone, Default)]
pub(crate) struct Coverage {
    pub w: i32,
    pub h: i32,
    pub a: Vec<u8>,
}

/// Rasteriser plus its font cache. Create once and keep it: the
/// overlay holds one for the life of the process.
pub(crate) struct LabelText {
    dwrite: Option<DWrite>,
    gdi_fonts: Vec<(Font, HFONT)>,
    /// Measured advance width (px per character) per font.
    advances: Vec<(Font, f32)>,
}

struct DWrite {
    // Kept alive for the render target's sake.
    _d2d: ID2D1Factory,
    factory: IDWriteFactory,
    fonts: IDWriteFontCollection,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    formats: Vec<(Font, IDWriteTextFormat)>,
}

impl LabelText {
    /// DirectWrite renderer, falling back to GDI if it can't be created.
    pub fn new() -> Self {
        let dwrite = match unsafe { DWrite::new() } {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(error = %e, "DirectWrite unavailable; drawing labels with GDI");
                None
            }
        };
        Self {
            dwrite,
            gdi_fonts: Vec::new(),
            advances: Vec::new(),
        }
    }

    /// GDI-only renderer, for testing the fallback path.
    #[cfg(test)]
    pub fn gdi_only() -> Self {
        Self {
            dwrite: None,
            gdi_fonts: Vec::new(),
            advances: Vec::new(),
        }
    }

    /// Width of one character of `font` in pixels. Consolas is
    /// monospaced, so a label's width is `chars × advance`.
    pub fn advance(&mut self, font: Font) -> f32 {
        if let Some(&(_, a)) = self.advances.iter().find(|(f, _)| *f == font) {
            return a;
        }
        const PROBE: &str = "0000000000";
        let measured = self
            .dwrite
            .as_mut()
            .and_then(|d| unsafe { d.measure(font, PROBE) }.ok())
            .or_else(|| unsafe { self.gdi_measure(font, PROBE) })
            .map(|w| w / PROBE.len() as f32)
            .filter(|a| *a > 0.0)
            // Last resort: the ratio layout used before text was measured.
            .unwrap_or(font.px as f32 * 0.62);
        self.advances.push((font, measured));
        measured
    }

    /// Rasterise every item. Returns one [`Coverage`] per item, in order.
    pub fn rasterize(&mut self, items: &[TextItem]) -> Vec<Coverage> {
        let mut out: Vec<Coverage> = items
            .iter()
            .map(|it| Coverage {
                w: it.w.max(0),
                h: it.h.max(0),
                a: Vec::new(),
            })
            .collect();
        let todo: Vec<usize> = (0..items.len())
            .filter(|&i| !items[i].text.is_empty() && items[i].w > 0 && items[i].h > 0)
            .collect();
        let atlas_w = todo
            .iter()
            .map(|&i| items[i].w + ATLAS_GAP)
            .max()
            .unwrap_or(0)
            .max(MIN_ATLAS_W);

        // Shelf-pack into passes no taller than MAX_ATLAS_H.
        let mut pass: Vec<(usize, i32, i32)> = Vec::new();
        let (mut x, mut y, mut shelf_h) = (0, 0, 0);
        for &i in &todo {
            let it = &items[i];
            if x + it.w > atlas_w {
                x = 0;
                y += shelf_h + ATLAS_GAP;
                shelf_h = 0;
            }
            if y + it.h > MAX_ATLAS_H && !pass.is_empty() {
                self.draw_pass(items, &pass, atlas_w, y, &mut out);
                pass.clear();
                (x, y, shelf_h) = (0, 0, 0);
            }
            pass.push((i, x, y));
            x += it.w + ATLAS_GAP;
            shelf_h = shelf_h.max(it.h);
        }
        if !pass.is_empty() {
            self.draw_pass(items, &pass, atlas_w, y + shelf_h, &mut out);
        }
        for c in &mut out {
            c.a.resize((c.w * c.h) as usize, 0);
        }
        out
    }

    /// Draw one atlas pass (`(item, x, y)` placements) and copy each
    /// item's coverage out.
    fn draw_pass(
        &mut self,
        items: &[TextItem],
        placed: &[(usize, i32, i32)],
        w: i32,
        h: i32,
        out: &mut [Coverage],
    ) {
        let Ok(mut atlas) = (unsafe { Atlas::new(w, h) }) else {
            tracing::warn!(w, h, "label atlas allocation failed");
            return;
        };
        let drawn = match self.dwrite.as_mut() {
            Some(d) => match unsafe { d.draw(&atlas, items, placed) } {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "DirectWrite label pass failed; using GDI");
                    atlas.clear();
                    false
                }
            },
            None => false,
        };
        // DirectWrite writes coverage to alpha (white text on clear);
        // GDI writes it to the colour channels and leaves alpha at 0.
        let from_alpha = drawn || {
            unsafe { self.gdi_draw(&atlas, items, placed) };
            false
        };
        let px = atlas.pixels();
        for &(i, x0, y0) in placed {
            let it = &items[i];
            let mut a = Vec::with_capacity((it.w * it.h) as usize);
            for y in y0..y0 + it.h {
                let row = &px[((y * w + x0) * 4) as usize..((y * w + x0 + it.w) * 4) as usize];
                a.extend(row.chunks_exact(4).map(|p| {
                    if from_alpha {
                        p[3]
                    } else {
                        p[0].max(p[1]).max(p[2])
                    }
                }));
            }
            out[i].a = a;
        }
    }

    // ---- GDI fallback -------------------------------------------------

    unsafe fn gdi_font(&mut self, font: Font) -> HFONT {
        if let Some(&(_, f)) = self.gdi_fonts.iter().find(|(f, _)| *f == font) {
            return f;
        }
        let weight = match font.role {
            Role::Label => FW_BOLD.0,
            Role::Title => FW_NORMAL.0,
        };
        let f = CreateFontW(
            font.px,
            0,
            0,
            0,
            weight as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0.into(),
            OUT_DEFAULT_PRECIS.0.into(),
            CLIP_DEFAULT_PRECIS.0.into(),
            ANTIALIASED_QUALITY.0.into(),
            u32::from(DEFAULT_PITCH.0 | (FF_DONTCARE.0 << 4)),
            &HSTRING::from(FAMILY),
        );
        self.gdi_fonts.push((font, f));
        f
    }

    unsafe fn gdi_measure(&mut self, font: Font, text: &str) -> Option<f32> {
        let f = self.gdi_font(font);
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let old = SelectObject(dc, HGDIOBJ(f.0));
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        let ok = GetTextExtentPoint32W(dc, &wide, &mut sz).as_bool();
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        ok.then_some(sz.cx as f32)
    }

    unsafe fn gdi_draw(&mut self, atlas: &Atlas, items: &[TextItem], placed: &[(usize, i32, i32)]) {
        let _ = SetBkMode(atlas.dc, TRANSPARENT);
        let _ = SetTextColor(atlas.dc, COLORREF(0x00FF_FFFF));
        for &(i, x, y) in placed {
            let it = &items[i];
            let f = self.gdi_font(it.font);
            let old = SelectObject(atlas.dc, HGDIOBJ(f.0));
            let mut wide: Vec<u16> = it.text.encode_utf16().collect();
            let mut r = RECT {
                left: x,
                top: y,
                right: x + it.w,
                bottom: y + it.h,
            };
            let align: DRAW_TEXT_FORMAT = match it.font.role {
                Role::Label => DT_CENTER,
                Role::Title => DT_LEFT | DT_END_ELLIPSIS,
            };
            DrawTextW(
                atlas.dc,
                &mut wide,
                &mut r,
                align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(atlas.dc, old);
        }
    }
}

impl Drop for LabelText {
    fn drop(&mut self) {
        for (_, f) in self.gdi_fonts.drain(..) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }
    }
}

impl DWrite {
    unsafe fn new() -> Result<Self> {
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let mut fonts = None;
        factory.GetSystemFontCollection(&mut fonts, BOOL(0))?;
        let fonts = fonts.ok_or_else(|| anyhow!("no system font collection"))?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            // Software: no D3D device start-up, no GPU readback per pass.
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // 96 so one DIP is one pixel; sizes are already DPI-scaled.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        // Grayscale: coverage goes to alpha, so glyphs can sit opaque on
        // a translucent badge. Default rendering params otherwise.
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let white = D2D1_COLOR_F {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        };
        let brush = rt.CreateSolidColorBrush(&white, None)?;
        Ok(Self {
            _d2d: d2d,
            factory,
            fonts,
            rt,
            brush,
            formats: Vec::new(),
        })
    }

    unsafe fn format(&mut self, font: Font) -> Result<IDWriteTextFormat> {
        if let Some((_, f)) = self.formats.iter().find(|(f, _)| *f == font) {
            return Ok(f.clone());
        }
        let weight = match font.role {
            Role::Label => DWRITE_FONT_WEIGHT_BOLD,
            Role::Title => DWRITE_FONT_WEIGHT_NORMAL,
        };
        let em = self.em_size(font.px, weight)?;
        let name = HSTRING::from(FAMILY);
        let format = self.factory.CreateTextFormat(
            &name,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            em,
            w!("en-us"),
        )?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        match font.role {
            Role::Label => format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?,
            Role::Title => {
                format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
                let sign = self.factory.CreateEllipsisTrimmingSign(&format)?;
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                format.SetTrimming(&trimming, &sign)?;
            }
        }
        self.formats.push((font, format.clone()));
        Ok(format)
    }

    /// DirectWrite em size equivalent to a GDI cell height of `px`
    /// (`CreateFontW`'s positive height is ascent + descent).
    unsafe fn em_size(&self, px: i32, weight: DWRITE_FONT_WEIGHT) -> Result<f32> {
        let name = HSTRING::from(FAMILY);
        let mut index = 0u32;
        let mut exists = BOOL(0);
        self.fonts.FindFamilyName(&name, &mut index, &mut exists)?;
        if !exists.as_bool() {
            return Ok(px as f32);
        }
        let font = self.fonts.GetFontFamily(index)?.GetFirstMatchingFont(
            weight,
            DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
        )?;
        let mut m = DWRITE_FONT_METRICS::default();
        font.GetMetrics(&mut m);
        let cell = m.ascent as f32 + m.descent as f32;
        Ok(if cell > 0.0 {
            px as f32 * m.designUnitsPerEm as f32 / cell
        } else {
            px as f32
        })
    }

    unsafe fn measure(&mut self, font: Font, text: &str) -> Result<f32> {
        let format = self.format(font)?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let layout = self
            .factory
            .CreateTextLayout(&wide, &format, f32::MAX, f32::MAX)?;
        let mut m = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut m)?;
        Ok(m.widthIncludingTrailingWhitespace)
    }

    unsafe fn draw(
        &mut self,
        atlas: &Atlas,
        items: &[TextItem],
        placed: &[(usize, i32, i32)],
    ) -> Result<()> {
        // Resolve formats first: `format` needs `&mut self`.
        let mut formats = Vec::with_capacity(placed.len());
        for &(i, _, _) in placed {
            formats.push(self.format(items[i].font)?);
        }
        let full = RECT {
            left: 0,
            top: 0,
            right: atlas.w,
            bottom: atlas.h,
        };
        self.rt.BindDC(atlas.dc, &full)?;
        self.rt.BeginDraw();
        self.rt.Clear(Some(&D2D1_COLOR_F::default()));
        for (&(i, x, y), format) in placed.iter().zip(&formats) {
            let it = &items[i];
            let wide: Vec<u16> = it.text.encode_utf16().collect();
            let r = D2D_RECT_F {
                left: x as f32,
                top: y as f32,
                right: (x + it.w) as f32,
                bottom: (y + it.h) as f32,
            };
            self.rt.DrawText(
                &wide,
                format,
                &r,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
        self.rt.EndDraw(None, None).context("EndDraw")?;
        Ok(())
    }
}

/// Scratch top-down 32-bit DIB bound to a memory DC.
struct Atlas {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Atlas {
    unsafe fn new(w: i32, h: i32) -> Result<Self> {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                let _ = DeleteDC(dc);
                return Err(anyhow!("CreateDIBSection failed"));
            }
        };
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        // Fresh DIB sections are zero-filled: fully transparent.
        Ok(Self {
            dc,
            bmp,
            old,
            bits: bits as *mut u8,
            w,
            h,
        })
    }

    fn pixels(&self) -> &[u8] {
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.w * self.h * 4) as usize)
        }
    }

    fn clear(&mut self) {
        unsafe {
            let _ = GdiFlush();
            std::ptr::write_bytes(self.bits, 0, (self.w * self.h * 4) as usize);
        }
    }
}

impl Drop for Atlas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bmp.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL: Font = Font {
        px: 20,
        role: Role::Label,
    };

    fn ink(c: &Coverage) -> (u32, i32, i32) {
        let mut sum = 0u32;
        let (mut min_x, mut max_x) = (c.w, -1);
        for y in 0..c.h {
            for x in 0..c.w {
                let v = c.a[(y * c.w + x) as usize];
                sum += v as u32;
                if v > 0 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                }
            }
        }
        (sum, min_x, max_x)
    }

    fn check_renders(mut t: LabelText) {
        let items = [
            TextItem {
                text: "W",
                font: LABEL,
                w: 30,
                h: 24,
            },
            TextItem {
                text: "",
                font: LABEL,
                w: 10,
                h: 10,
            },
        ];
        let out = t.rasterize(&items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].a.len(), 30 * 24);
        let (sum, min_x, max_x) = ink(&out[0]);
        assert!(sum > 255 * 20, "W has ink, got {sum}");
        // Centred: ink starts and ends away from both edges.
        assert!(min_x > 2 && max_x < 27, "{min_x}..{max_x}");
        assert!(out[1].a.iter().all(|&v| v == 0), "empty text has no ink");
        let a = t.advance(LABEL);
        assert!((9.0..13.0).contains(&a), "Consolas advance at 20px: {a}");
    }

    #[test]
    fn directwrite_renders_centred_glyphs() {
        let t = LabelText::new();
        assert!(t.dwrite.is_some(), "DirectWrite should be available");
        check_renders(t);
    }

    #[test]
    fn gdi_fallback_renders_centred_glyphs() {
        check_renders(LabelText::gdi_only());
    }

    #[test]
    fn many_items_split_into_passes_and_keep_order() {
        let mut t = LabelText::new();
        let texts: Vec<String> = (0..10000).map(|i| format!("{}", i % 10)).collect();
        let items: Vec<TextItem> = texts
            .iter()
            .map(|s| TextItem {
                text: s,
                font: LABEL,
                w: 20,
                h: 24,
            })
            .collect();
        let out = t.rasterize(&items);
        assert_eq!(out.len(), items.len());
        // Same glyph → same mask, wherever it landed in the atlas.
        assert_eq!(out[1].a, out[9991].a);
        assert_ne!(out[1].a, out[2].a);
        assert!(out.iter().all(|c| ink(c).0 > 0));
    }

    #[test]
    fn titles_are_trimmed_to_their_box() {
        let mut t = LabelText::new();
        let font = Font {
            px: 16,
            role: Role::Title,
        };
        let items = [TextItem {
            text: "A very long window title that cannot possibly fit",
            font,
            w: 60,
            h: 20,
        }];
        let out = t.rasterize(&items);
        let (sum, _, max_x) = ink(&out[0]);
        assert!(sum > 0);
        assert!(max_x < 60);
    }
}
