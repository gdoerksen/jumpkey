//! Render a side-by-side contact sheet of hint-label rendering options.
//!
//! Rows are rendering pipelines (the current GDI path plus candidate
//! fixes), each composited over light, dark and busy backgrounds with
//! the same premultiplied "over" maths DWM applies to the layered
//! overlay window. Columns are font faces. One PNG is written per DPI
//! scale to `target/label-compare/`.
//!
//! ```text
//! cargo run --example label_compare
//! ```
//!
//! View the PNGs at 100% zoom: any scaled preview resamples the pixels
//! and hides exactly the differences this sheet is meant to show.

#[cfg(not(windows))]
fn main() {
    eprintln!("label_compare is Windows-only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io::BufWriter;
    use std::path::PathBuf;

    use anyhow::{anyhow, Context, Result};
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::{BOOL, COLORREF, RECT, SIZE};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
        D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
        D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
        D2D1_RENDER_TARGET_USAGE_NONE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
        DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_METRICS, DWRITE_FONT_STRETCH_NORMAL,
        DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_PIXEL_GEOMETRY_FLAT,
        DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_WORD_WRAPPING_NO_WRAP,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
        GdiFlush, GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor,
        ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FONT_QUALITY, HBITMAP, HDC, HFONT,
        HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
    };
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // ---- colours ------------------------------------------------------

    #[derive(Clone, Copy)]
    struct Rgb(u8, u8, u8);

    impl Rgb {
        /// From a GDI `COLORREF` (0x00BBGGRR).
        const fn cref(c: u32) -> Self {
            Rgb(
                (c & 0xFF) as u8,
                ((c >> 8) & 0xFF) as u8,
                ((c >> 16) & 0xFF) as u8,
            )
        }
        fn colorref(self) -> COLORREF {
            COLORREF(self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16)
        }
        fn f(self) -> [f32; 3] {
            [self.0 as f32, self.1 as f32, self.2 as f32]
        }
    }

    // ---- what we compare ----------------------------------------------

    #[derive(Clone, Copy, PartialEq)]
    enum Text {
        Gdi { quality: FONT_QUALITY, shadow: bool },
        DWrite { symmetric: bool },
    }

    struct Pipeline {
        name: &'static str,
        text: Text,
        /// `false`: whole window faded by `SourceConstantAlpha` (current).
        /// `true`: only the badge fill is translucent; glyphs stay opaque.
        opaque_glyphs: bool,
    }

    const PIPELINES: &[Pipeline] = &[
        Pipeline {
            name: "GDI ClearType + 1px shadow, all 76% (pre-0.6)",
            text: Text::Gdi {
                quality: CLEARTYPE_QUALITY,
                shadow: true,
            },
            opaque_glyphs: false,
        },
        Pipeline {
            name: "GDI ClearType, no shadow, all 76% (CURRENT)",
            text: Text::Gdi {
                quality: CLEARTYPE_QUALITY,
                shadow: false,
            },
            opaque_glyphs: false,
        },
        Pipeline {
            name: "GDI ClearType, no shadow, opaque glyphs",
            text: Text::Gdi {
                quality: CLEARTYPE_QUALITY,
                shadow: false,
            },
            opaque_glyphs: true,
        },
        Pipeline {
            name: "GDI grayscale AA, no shadow, opaque glyphs",
            text: Text::Gdi {
                quality: ANTIALIASED_QUALITY,
                shadow: false,
            },
            opaque_glyphs: true,
        },
        Pipeline {
            name: "DirectWrite grayscale, opaque glyphs",
            text: Text::DWrite { symmetric: false },
            opaque_glyphs: true,
        },
        Pipeline {
            name: "DirectWrite natural-symmetric, opaque glyphs",
            text: Text::DWrite { symmetric: true },
            opaque_glyphs: true,
        },
    ];

    struct FontSpec {
        family: &'static str,
        weight: i32,
        caption: &'static str,
    }

    const FONTS: &[FontSpec] = &[
        FontSpec {
            family: "Consolas",
            weight: 700,
            caption: "Consolas Bold (current)",
        },
        FontSpec {
            family: "Cascadia Mono",
            weight: 700,
            caption: "Cascadia Mono Bold",
        },
        FontSpec {
            family: "Segoe UI Variable Text",
            weight: 600,
            caption: "Segoe UI Variable Semibold",
        },
        FontSpec {
            family: "Segoe UI",
            weight: 700,
            caption: "Segoe UI Bold",
        },
    ];

    /// Mirrors the relevant bits of `HintStyle::elements()` / `windows()`.
    struct Style {
        name: &'static str,
        base_font: i32,
        pad_x: i32,
        pad_y: i32,
        bg: Rgb,
        fg: Rgb,
        border: Rgb,
        opacity: u8,
        callout: bool,
        labels: &'static [&'static str],
    }

    const STYLES: &[Style] = &[
        Style {
            name: "Element hints (callout)",
            base_font: 13,
            pad_x: 3,
            pad_y: 1,
            bg: Rgb::cref(0x0000E5FF),
            fg: Rgb::cref(0x00000000),
            border: Rgb::cref(0x00202020),
            opacity: 195,
            callout: true,
            labels: &["a", "sd", "jk", "il", "rn", "wm", "gq"],
        },
        Style {
            name: "Window picker",
            base_font: 30,
            pad_x: 12,
            pad_y: 6,
            bg: Rgb::cref(0x00FFAA33),
            fg: Rgb::cref(0x00FFFFFF),
            border: Rgb::cref(0x00101010),
            opacity: 240,
            callout: false,
            labels: &["a", "sd", "il"],
        },
    ];

    const BACKGROUNDS: &[&str] = &["light", "dark", "busy"];

    fn background_px(kind: usize, x: i32, y: i32) -> Rgb {
        match kind {
            0 => Rgb(0xF3, 0xF3, 0xF3),
            1 => Rgb(0x20, 0x20, 0x20),
            _ => {
                const P: [Rgb; 4] = [
                    Rgb(0xFF, 0xFF, 0xFF),
                    Rgb(0x1E, 0x90, 0xFF),
                    Rgb(0x22, 0x22, 0x22),
                    Rgb(0xFF, 0x8C, 0x00),
                ];
                P[(((x / 5) + (y / 5)) % 4) as usize]
            }
        }
    }

    const SCALES: &[(f32, &str)] = &[(1.0, "100"), (1.25, "125"), (1.5, "150"), (2.0, "200")];

    // ---- premultiplied BGRA surface backed by a DIB section --------------

    struct Surface {
        dc: HDC,
        bmp: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u8,
        w: i32,
        h: i32,
    }

    impl Surface {
        unsafe fn new(w: i32, h: i32) -> Result<Self> {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(anyhow!("CreateCompatibleDC failed"));
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .context("CreateDIBSection failed")?;
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let _ = SetBkMode(dc, TRANSPARENT);
            let mut s = Surface {
                dc,
                bmp,
                old,
                bits: bits as *mut u8,
                w,
                h,
            };
            s.clear();
            Ok(s)
        }

        fn px(&mut self) -> &mut [u8] {
            unsafe {
                let _ = GdiFlush();
                std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize)
            }
        }

        fn clear(&mut self) {
            self.px().fill(0);
        }

        /// Visit every pixel in `r ∩ surface` as `(x, y, &mut [b, g, r, a])`.
        fn each(&mut self, r: &RECT, mut f: impl FnMut(i32, i32, &mut [u8])) {
            let (w, h) = (self.w, self.h);
            let px = self.px();
            for y in r.top.max(0)..r.bottom.min(h) {
                for x in r.left.max(0)..r.right.min(w) {
                    let i = ((y * w + x) * 4) as usize;
                    f(x, y, &mut px[i..i + 4]);
                }
            }
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.bmp.0));
                let _ = DeleteDC(self.dc);
            }
        }
    }

    // ---- badge body ---------------------------------------------------

    /// Coverage of pixel `(x, y)` by a rounded rect, 4×4 supersampled.
    fn rrect_cov(x: i32, y: i32, l: f32, t: f32, r: f32, b: f32, rad: f32) -> f32 {
        let mut hit = 0;
        for sy in 0..4 {
            for sx in 0..4 {
                let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                if px < l || px > r || py < t || py > b {
                    continue;
                }
                let cx = px.clamp(l + rad, r - rad);
                let cy = py.clamp(t + rad, b - rad);
                let (dx, dy) = (px - cx, py - cy);
                if dx * dx + dy * dy <= rad * rad {
                    hit += 1;
                }
            }
        }
        hit as f32 / 16.0
    }

    /// Fill + 1px border, premultiplied, written straight into the DIB
    /// (the overlay does the same via a cached sprite / frame helpers).
    fn paint_badge(s: &mut Surface, body: &RECT, rad: i32, bg: Rgb, border: Rgb) {
        let (l, t, r, b) = (
            body.left as f32,
            body.top as f32,
            body.right as f32,
            body.bottom as f32,
        );
        let rad = rad as f32;
        let (bgf, bdf) = (bg.f(), border.f());
        s.each(body, |x, y, p| {
            let outer = rrect_cov(x, y, l, t, r, b, rad);
            let inner = rrect_cov(
                x,
                y,
                l + 1.0,
                t + 1.0,
                r - 1.0,
                b - 1.0,
                (rad - 1.0).max(0.0),
            );
            let edge = outer - inner;
            for (c, out) in [2, 1, 0].into_iter().zip(0..3) {
                p[c] = (bgf[out] * inner + bdf[out] * edge).round() as u8;
            }
            p[3] = (outer * 255.0).round() as u8;
        });
    }

    /// Multiply every premultiplied pixel in `r` by `a`.
    fn fade(s: &mut Surface, r: &RECT, a: f32) {
        s.each(r, |_, _, p| {
            for v in p.iter_mut() {
                *v = (*v as f32 * a).round() as u8;
            }
        });
    }

    /// After GDI drew `fg` text onto an opaque `bg` fill, recover the
    /// per-channel glyph coverage and rebuild the pixel as an opaque
    /// glyph over a fill faded to `a`.
    fn bake_opaque_glyphs(s: &mut Surface, body: &RECT, text: &RECT, bg: Rgb, fg: Rgb, a: f32) {
        let (b, f) = (bg.f(), fg.f());
        s.each(body, |x, y, p| {
            let inside = x >= text.left && x < text.right && y >= text.top && y < text.bottom;
            if !inside {
                for v in p.iter_mut() {
                    *v = (*v as f32 * a).round() as u8;
                }
                return;
            }
            let c = [p[2] as f32, p[1] as f32, p[0] as f32];
            let mut cov = [f32::NAN; 3];
            for i in 0..3 {
                if (f[i] - b[i]).abs() >= 32.0 {
                    cov[i] = ((c[i] - b[i]) / (f[i] - b[i])).clamp(0.0, 1.0);
                }
            }
            let known: Vec<f32> = cov.iter().copied().filter(|v| !v.is_nan()).collect();
            let avg = known.iter().sum::<f32>() / known.len().max(1) as f32;
            for v in cov.iter_mut() {
                if v.is_nan() {
                    *v = avg;
                }
            }
            for i in 0..3 {
                p[2 - i] = (cov[i] * f[i] + (1.0 - cov[i]) * b[i] * a).round() as u8;
            }
            p[3] = ((avg + (1.0 - avg) * a) * 255.0).round() as u8;
        });
    }

    // ---- fonts ----------------------------------------------------------

    struct Ctx {
        dwrite: IDWriteFactory,
        fonts: IDWriteFontCollection,
        rt: ID2D1DCRenderTarget,
        rt_sym: ID2D1DCRenderTarget,
        brush: ID2D1SolidColorBrush,
        brush_sym: ID2D1SolidColorBrush,
        caption: HFONT,
        caption_bold: HFONT,
    }

    struct FontRes {
        exists: bool,
        gdi_ct: HFONT,
        gdi_aa: HFONT,
        dw: IDWriteTextFormat,
    }

    impl Drop for FontRes {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.gdi_ct.0));
                let _ = DeleteObject(HGDIOBJ(self.gdi_aa.0));
            }
        }
    }

    unsafe fn gdi_font(family: &str, height: i32, weight: i32, quality: FONT_QUALITY) -> HFONT {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0.into(),
            OUT_DEFAULT_PRECIS.0.into(),
            CLIP_DEFAULT_PRECIS.0.into(),
            quality.0.into(),
            u32::from(DEFAULT_PITCH.0 | (FF_DONTCARE.0 << 4)),
            &HSTRING::from(family),
        )
    }

    impl Ctx {
        unsafe fn new() -> Result<Self> {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut fonts = None;
            dwrite.GetSystemFontCollection(&mut fonts, BOOL(0))?;
            let fonts = fonts.ok_or_else(|| anyhow!("no system font collection"))?;

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
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
            let white = D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            };
            let rt = d2d.CreateDCRenderTarget(&props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = rt.CreateSolidColorBrush(&white, None)?;

            let rt_sym = d2d.CreateDCRenderTarget(&props)?;
            rt_sym.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let params = dwrite.CreateCustomRenderingParams(
                1.8,
                0.5,
                0.0,
                DWRITE_PIXEL_GEOMETRY_FLAT,
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
            )?;
            rt_sym.SetTextRenderingParams(&params);
            let brush_sym = rt_sym.CreateSolidColorBrush(&white, None)?;

            Ok(Ctx {
                dwrite,
                fonts,
                rt,
                rt_sym,
                brush,
                brush_sym,
                caption: gdi_font("Segoe UI", 17, 400, CLEARTYPE_QUALITY),
                caption_bold: gdi_font("Segoe UI", 20, 700, CLEARTYPE_QUALITY),
            })
        }

        /// GDI fonts (cell height `height`, as `CreateFontW` gets in the
        /// overlay) plus a DirectWrite format with the equivalent em size.
        unsafe fn font(&self, spec: &FontSpec, height: i32) -> Result<FontRes> {
            let name = HSTRING::from(spec.family);
            let mut index = 0u32;
            let mut exists = BOOL(0);
            self.fonts.FindFamilyName(&name, &mut index, &mut exists)?;
            let weight = DWRITE_FONT_WEIGHT(spec.weight);

            // GDI's positive height is ascent+descent; DirectWrite wants em.
            let mut em = height as f32;
            if exists.as_bool() {
                let font = self.fonts.GetFontFamily(index)?.GetFirstMatchingFont(
                    weight,
                    DWRITE_FONT_STRETCH_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                )?;
                let mut m = DWRITE_FONT_METRICS::default();
                font.GetMetrics(&mut m);
                let cell = m.ascent as f32 + m.descent as f32;
                if cell > 0.0 {
                    em = height as f32 * m.designUnitsPerEm as f32 / cell;
                }
            }
            let dw = self.dwrite.CreateTextFormat(
                &name,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                em,
                w!("en-us"),
            )?;
            dw.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            dw.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            dw.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

            Ok(FontRes {
                exists: exists.as_bool(),
                gdi_ct: gdi_font(spec.family, height, spec.weight, CLEARTYPE_QUALITY),
                gdi_aa: gdi_font(spec.family, height, spec.weight, ANTIALIASED_QUALITY),
                dw,
            })
        }
    }

    unsafe fn gdi_text(
        s: &Surface,
        font: HFONT,
        text: &str,
        rect: &RECT,
        color: Rgb,
        center: bool,
    ) {
        // An empty Vec hands DrawTextW a dangling pointer, which faults.
        if text.is_empty() {
            return;
        }
        let old = SelectObject(s.dc, HGDIOBJ(font.0));
        let _ = SetTextColor(s.dc, color.colorref());
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut r = *rect;
        let align = if center {
            DT_CENTER | DT_VCENTER
        } else {
            DT_LEFT | DT_VCENTER
        };
        DrawTextW(s.dc, &mut wide, &mut r, align | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(s.dc, old);
    }

    unsafe fn text_width(dc: HDC, font: HFONT, text: &str) -> i32 {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &wide, &mut sz);
        SelectObject(dc, old);
        sz.cx
    }

    // ---- one sheet ------------------------------------------------------

    struct Badge {
        body: RECT,
        text: RECT,
        label: &'static str,
    }

    const MARGIN: i32 = 10;
    const LABEL_COL: i32 = 420;
    const HEADER_H: i32 = 34;

    fn scaled(v: i32, s: f32, ceil: bool) -> i32 {
        if s <= 1.0 {
            v
        } else if ceil {
            (v as f32 * s).ceil() as i32
        } else {
            (v as f32 * s).round() as i32
        }
    }

    /// Lay out a style's sample labels in a row, as `CalloutGeom` sizes them.
    unsafe fn layout(dc: HDC, font: &FontRes, st: &Style, scale: f32) -> (Vec<Badge>, i32, i32) {
        let fh = scaled(st.base_font, scale, false);
        let pad_x = scaled(st.pad_x, scale, true);
        let pad_y = scaled(st.pad_y, scale, true);
        let (radius, body_h, pad_x) = if st.callout {
            let radius = ((fh as f32 * 0.3).round() as i32).max(2);
            (radius, fh + 2 * pad_y.max(2), pad_x.max(radius))
        } else {
            (0, fh + 2 * pad_y, pad_x)
        };
        let gap = scaled(8, scale, true);
        let mut x = MARGIN;
        let mut out = Vec::new();
        for &label in st.labels {
            let bw = text_width(dc, font.gdi_ct, label) + 2 * pad_x;
            let body = RECT {
                left: x,
                top: MARGIN,
                right: x + bw,
                bottom: MARGIN + body_h,
            };
            let text = RECT {
                left: body.left + pad_x,
                top: body.top + 1,
                right: body.right - pad_x,
                bottom: body.bottom - 1,
            };
            out.push(Badge { body, text, label });
            x += bw + gap;
        }
        (out, x - gap + MARGIN, radius)
    }

    /// Draw one cell's badges into `cell` for `pipe`, returning the
    /// `SourceConstantAlpha` the overlay would hand DWM.
    #[allow(clippy::too_many_arguments)]
    unsafe fn render_cell(
        ctx: &Ctx,
        font: &FontRes,
        st: &Style,
        pipe: &Pipeline,
        badges: &[Badge],
        radius: i32,
        cell: &mut Surface,
        mask: &mut Surface,
    ) -> Result<u8> {
        let a = st.opacity as f32 / 255.0;
        cell.clear();
        for b in badges {
            paint_badge(cell, &b.body, radius, st.bg, st.border);
        }
        match pipe.text {
            Text::Gdi { quality, shadow } => {
                let hf = if quality == CLEARTYPE_QUALITY {
                    font.gdi_ct
                } else {
                    font.gdi_aa
                };
                for b in badges {
                    if shadow {
                        let r = RECT {
                            left: b.text.left + 1,
                            top: b.text.top + 1,
                            right: b.text.right + 1,
                            bottom: b.text.bottom + 1,
                        };
                        gdi_text(cell, hf, b.label, &r, Rgb(0x40, 0x40, 0x40), true);
                    }
                    gdi_text(cell, hf, b.label, &b.text, st.fg, true);
                    // GDI zeroes alpha on glyph pixels; the overlay lifts
                    // the text rect back to opaque the same way.
                    cell.each(&b.text, |_, _, p| p[3] = 255);
                    if pipe.opaque_glyphs {
                        bake_opaque_glyphs(cell, &b.body, &b.text, st.bg, st.fg, a);
                    }
                }
            }
            Text::DWrite { symmetric } => {
                if pipe.opaque_glyphs {
                    for b in badges {
                        fade(cell, &b.body, a);
                    }
                }
                let (rt, brush) = if symmetric {
                    (&ctx.rt_sym, &ctx.brush_sym)
                } else {
                    (&ctx.rt, &ctx.brush)
                };
                mask.clear();
                let full = RECT {
                    left: 0,
                    top: 0,
                    right: mask.w,
                    bottom: mask.h,
                };
                rt.BindDC(mask.dc, &full)?;
                rt.BeginDraw();
                rt.Clear(Some(&D2D1_COLOR_F::default()));
                for b in badges {
                    let wide: Vec<u16> = b.label.encode_utf16().collect();
                    let r = D2D_RECT_F {
                        left: b.text.left as f32,
                        top: b.text.top as f32,
                        right: b.text.right as f32,
                        bottom: b.text.bottom as f32,
                    };
                    rt.DrawText(
                        &wide,
                        &font.dw,
                        &r,
                        brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
                rt.EndDraw(None, None)?;
                // White-on-transparent mask: alpha is glyph coverage.
                let cov: Vec<u8> = mask.px().chunks_exact(4).map(|p| p[3]).collect();
                let fg = st.fg.f();
                let w = cell.w;
                let all = RECT {
                    left: 0,
                    top: 0,
                    right: cell.w,
                    bottom: cell.h,
                };
                cell.each(&all, |x, y, p| {
                    let c = cov[(y * w + x) as usize] as f32 / 255.0;
                    if c > 0.0 {
                        for i in 0..3 {
                            p[2 - i] = (c * fg[i] + (1.0 - c) * p[2 - i] as f32).round() as u8;
                        }
                        p[3] = (c * 255.0 + (1.0 - c) * p[3] as f32).round() as u8;
                    }
                });
            }
        }
        Ok(if pipe.opaque_glyphs { 255 } else { st.opacity })
    }

    /// DWM-style premultiplied "over" with a constant alpha, onto a
    /// background pattern.
    fn composite(sheet: &mut Surface, cell: &mut Surface, ox: i32, oy: i32, ca: u8, bg: usize) {
        let ca = ca as f32 / 255.0;
        let (cw, ch) = (cell.w, cell.h);
        let src: Vec<u8> = cell.px().to_vec();
        let r = RECT {
            left: ox,
            top: oy,
            right: ox + cw,
            bottom: oy + ch,
        };
        sheet.each(&r, |x, y, p| {
            let i = (((y - oy) * cw + (x - ox)) * 4) as usize;
            let back = background_px(bg, x - ox, y - oy);
            let back = [back.2 as f32, back.1 as f32, back.0 as f32];
            let sa = src[i + 3] as f32 / 255.0 * ca;
            for c in 0..3 {
                p[c] = (src[i + c] as f32 * ca + back[c] * (1.0 - sa))
                    .round()
                    .min(255.0) as u8;
            }
            p[3] = 255;
        });
    }

    fn fill(s: &mut Surface, r: &RECT, c: Rgb) {
        s.each(r, |_, _, p| {
            p[0] = c.2;
            p[1] = c.1;
            p[2] = c.0;
            p[3] = 255;
        });
    }

    unsafe fn caption(s: &Surface, font: HFONT, text: &str, x: i32, y: i32, w: i32, h: i32) {
        let r = RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        gdi_text(s, font, text, &r, Rgb(0x20, 0x20, 0x20), false);
    }

    unsafe fn render_sheet(ctx: &Ctx, scale: f32, path: &PathBuf) -> Result<()> {
        struct Section<'a> {
            st: &'a Style,
            fonts: Vec<FontRes>,
            layouts: Vec<(Vec<Badge>, i32, i32)>,
            cell_h: i32,
        }

        let probe = Surface::new(1, 1)?;
        let mut sections = Vec::new();
        for st in STYLES {
            let fh = scaled(st.base_font, scale, false);
            let fonts: Vec<FontRes> = FONTS
                .iter()
                .map(|f| ctx.font(f, fh))
                .collect::<Result<_>>()?;
            let layouts: Vec<_> = fonts
                .iter()
                .map(|f| layout(probe.dc, f, st, scale))
                .collect();
            let body_h = layouts[0].0[0].body.bottom - MARGIN;
            sections.push(Section {
                st,
                fonts,
                layouts,
                cell_h: body_h + 2 * MARGIN,
            });
        }

        // Column widths: widest cell per font across both sections.
        let col_w: Vec<i32> = (0..FONTS.len())
            .map(|i| {
                sections
                    .iter()
                    .map(|s| s.layouts[i].1)
                    .max()
                    .unwrap_or(0)
                    .max(260)
            })
            .collect();
        let width = LABEL_COL + col_w.iter().sum::<i32>();
        let rows = PIPELINES.len() * BACKGROUNDS.len();
        let height: i32 = sections
            .iter()
            .map(|s| 2 * HEADER_H + rows as i32 * s.cell_h + PIPELINES.len() as i32 * 6)
            .sum::<i32>()
            + HEADER_H;

        let mut sheet = Surface::new(width, height)?;
        fill(
            &mut sheet,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            Rgb(255, 255, 255),
        );
        let title = format!(
            "Hint label rendering at {}% scale. View at 100% zoom.",
            (scale * 100.0) as i32
        );
        caption(&sheet, ctx.caption_bold, &title, MARGIN, 0, width, HEADER_H);

        let mut y = HEADER_H;
        for sec in &sections {
            let fh = scaled(sec.st.base_font, scale, false);
            let head = format!("{}: {} px, opacity {}/255", sec.st.name, fh, sec.st.opacity);
            caption(
                &sheet,
                ctx.caption_bold,
                &head,
                MARGIN,
                y,
                LABEL_COL,
                HEADER_H,
            );
            let mut x = LABEL_COL;
            for (i, f) in FONTS.iter().enumerate() {
                let mut text = f.caption.to_string();
                if !sec.fonts[i].exists {
                    text.push_str(" (MISSING, substituted)");
                }
                caption(
                    &sheet,
                    ctx.caption_bold,
                    &text,
                    x + MARGIN,
                    y + HEADER_H,
                    col_w[i],
                    HEADER_H,
                );
                x += col_w[i];
            }
            y += 2 * HEADER_H;

            let max_w = *col_w.iter().max().unwrap_or(&1);
            let mut cell = Surface::new(max_w, sec.cell_h)?;
            let mut mask = Surface::new(max_w, sec.cell_h)?;
            for (pi, pipe) in PIPELINES.iter().enumerate() {
                fill(
                    &mut sheet,
                    &RECT {
                        left: 0,
                        top: y,
                        right: width,
                        bottom: y + 2,
                    },
                    Rgb(0xC0, 0xC0, 0xC0),
                );
                y += 6;
                for (bi, bg) in BACKGROUNDS.iter().enumerate() {
                    let label = if bi == 0 {
                        format!("{}. {}", pi + 1, pipe.name)
                    } else {
                        String::new()
                    };
                    caption(
                        &sheet,
                        ctx.caption,
                        &label,
                        MARGIN,
                        y,
                        LABEL_COL - 60,
                        sec.cell_h,
                    );
                    caption(&sheet, ctx.caption, bg, LABEL_COL - 50, y, 50, sec.cell_h);
                    let mut x = LABEL_COL;
                    for (fi, font) in sec.fonts.iter().enumerate() {
                        let (badges, _, radius) = &sec.layouts[fi];
                        let ca = render_cell(
                            ctx, font, sec.st, pipe, badges, *radius, &mut cell, &mut mask,
                        )?;
                        // Paint the whole column cell's background first.
                        let full = RECT {
                            left: x,
                            top: y,
                            right: x + col_w[fi],
                            bottom: y + sec.cell_h,
                        };
                        sheet.each(&full, |px, py, p| {
                            let c = background_px(bi, px - x, py - y);
                            p[0] = c.2;
                            p[1] = c.1;
                            p[2] = c.0;
                        });
                        composite(&mut sheet, &mut cell, x, y, ca, bi);
                        x += col_w[fi];
                    }
                    y += sec.cell_h;
                }
            }
            y += HEADER_H / 2;
        }

        write_png(&mut sheet, path)
    }

    fn write_png(s: &mut Surface, path: &PathBuf) -> Result<()> {
        let (w, h) = (s.w as u32, s.h as u32);
        let rgb: Vec<u8> = s
            .px()
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect();
        let file = BufWriter::new(File::create(path)?);
        let mut enc = png::Encoder::new(file, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgb)?;
        Ok(())
    }

    pub fn run() -> Result<()> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let ctx = Ctx::new()?;
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/label-compare");
            std::fs::create_dir_all(&dir)?;
            for &(scale, tag) in SCALES {
                let path = dir.join(format!("compare_{tag}.png"));
                render_sheet(&ctx, scale, &path)?;
                println!("wrote {}", path.display());
            }
            let _ = DeleteObject(HGDIOBJ(ctx.caption.0));
            let _ = DeleteObject(HGDIOBJ(ctx.caption_bold.0));
        }
        Ok(())
    }
}
