// Copyright 2020 Evgeniy Reizner
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This module is closer to SkDraw than SkCanvas
// and since it was written from scratch, there is no Google copyright.

use crate::{Pixmap, Transform, Path, Paint, StrokeProps, Point, PathStroker, NormalizedF32, Color};
use crate::{PathBuilder, Pattern, FilterQuality, BlendMode, FillType, Rect, SpreadMode};

use crate::painter::Painter;
use crate::safe_geom_ext::TransformExt;
use crate::scalar::Scalar;


/// Controls how a pixmap should be blended.
///
/// Like `Paint`, but for `Pixmap`.
#[derive(Copy, Clone, Debug)]
pub struct PixmapPaint {
    /// Pixmap opacity.
    ///
    /// Default: 1.0
    pub opacity: NormalizedF32,

    /// Pixmap blending mode.
    ///
    /// Default: SourceOver
    pub blend_mode: BlendMode,

    /// Specifies how much filtering to be done when transforming images.
    ///
    /// Default: Nearest
    pub quality: FilterQuality,
}

impl Default for PixmapPaint {
    #[inline]
    fn default() -> Self {
        PixmapPaint {
            opacity: NormalizedF32::ONE,
            blend_mode: BlendMode::default(),
            quality: FilterQuality::Nearest,
        }
    }
}


/// Provides a high-level rendering API.
///
/// Unlike the most of other types, `Canvas` provides an unchecked API.
/// Which means that a drawing command will simply be ignored in case of an error
/// and a caller has no way of checking it.
#[allow(missing_debug_implementations)]
#[derive(Clone)]
pub struct Canvas {
    /// A pixmap owned by the canvas.
    pub pixmap: Pixmap,
    /// Canvas's transform.
    transform: Transform,

    /// A path stroker used to cache temporary stroking data.
    stroker: PathStroker,
    stroked_path: Option<Path>,
}

impl From<Pixmap> for Canvas {
    #[inline]
    fn from(pixmap: Pixmap) -> Self {
        Canvas {
            pixmap,
            transform: Transform::identity(),
            stroker: PathStroker::new(),
            stroked_path: None,
        }
    }
}

impl Canvas {
    /// Creates a new canvas.
    ///
    /// Allocates a new pixmap. Use `Canvas::from(pixmap)` to reuse an existing one.
    ///
    /// Zero size in an error.
    ///
    /// Pixmap's width is limited by i32::MAX/4.
    #[inline]
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Some(Canvas {
            pixmap: Pixmap::new(width, height)?,
            transform: Transform::identity(),
            stroker: PathStroker::new(),
            stroked_path: None,
        })
    }

    /// Translates the canvas.
    #[inline]
    pub fn translate(&mut self, tx: f32, ty: f32) {
        if let Some(ts) = self.transform.post_translate(tx, ty) {
            self.transform = ts;
        }
    }

    /// Scales the canvas.
    #[inline]
    pub fn scale(&mut self, sx: f32, sy: f32) {
        if let Some(ts) = self.transform.post_scale(sx, sy) {
            self.transform = ts;
        }
    }

    /// Applies an affine transformation to the canvas.
    #[inline]
    pub fn transform(&mut self, sx: f32, ky: f32, kx: f32, sy: f32, tx: f32, ty: f32) {
        if let Some(ref ts) = Transform::from_row(sx, ky, kx, sy, tx, ty) {
            if let Some(ts) = self.transform.post_concat(ts) {
                self.transform = ts;
            }
        }
    }

    /// Gets the current canvas transform.
    #[inline]
    pub fn get_transform(&mut self) -> Transform {
        self.transform
    }

    /// Sets the canvas transform.
    #[inline]
    pub fn set_transform(&mut self, ts: Transform) {
        self.transform = ts;
    }

    /// Fills the whole canvas with a color.
    pub fn fill_canvas(&mut self, color: Color) {
        self.pixmap.fill(color);
    }

    /// Fills a path.
    pub fn fill_path(&mut self, path: &Path, paint: &Paint, fill_type: FillType) {
        self.fill_path_impl(path, paint, fill_type);
    }

    #[inline(always)]
    fn fill_path_impl(&mut self, path: &Path, paint: &Paint, fill_type: FillType) -> Option<()> {
        if !self.transform.is_identity() {
            let path = path.clone().transform(&self.transform)?;

            let mut paint = paint.clone();
            paint.shader.transform(&self.transform);

            self.pixmap.fill_path(&path, &paint, fill_type)
        } else {
            self.pixmap.fill_path(path, paint, fill_type)
        }
    }

    /// Strokes a path.
    ///
    /// Stroking is implemented using two separate algorithms:
    ///
    /// 1. If a stroke width is wider than 1px (after applying the transformation),
    ///    a path will be converted into a stroked path and then filled using `Painter::fill_path`.
    ///    Which means that we have to allocate a separate `Path`, that can be 2-3x larger
    ///    then the original path.
    ///    `Canvas` will reuse this allocation during subsequent strokes.
    /// 2. If a stroke width is thinner than 1px (after applying the transformation),
    ///    we will use hairline stroking, which doesn't involve a separate path allocation.
    pub fn stroke_path(&mut self, path: &Path, paint: &Paint, stroke: StrokeProps) {
        self.stroke_path_impl(path, paint, stroke);
    }

    #[inline(always)]
    fn stroke_path_impl(&mut self, path: &Path, paint: &Paint, mut stroke: StrokeProps) -> Option<()> {
        if stroke.width < 0.0 {
            return None;
        }

        let mut paint = paint.clone();

        let transformed_path;
        let path = if !self.transform.is_identity() {
            stroke.width *= compute_res_scale_for_stroking(&self.transform);

            paint.shader.transform(&self.transform);

            transformed_path = path.clone().transform(&self.transform)?;
            &transformed_path
        } else {
            path
        };

        if let Some(coverage) = treat_as_hairline(&paint, stroke, &self.transform) {
            if coverage == 1.0 {
                // No changes to the `paint`.
            } else if paint.blend_mode.should_pre_scale_coverage() {
                // This is the old technique, which we preserve for now so
                // we don't change previous results (testing)
                // the new way seems fine, its just (a tiny bit) different.
                let scale = (coverage * 256.0) as i32;
                let new_alpha = 255 * scale >> 8;
                paint.shader.apply_opacity(NormalizedF32::new_bounded(new_alpha as f32 / 255.0));
            }

            self.pixmap.stroke_hairline(&path, &paint, stroke.line_cap)
        } else {
            if let Some(stroked_path) = self.stroked_path.take() {
                self.stroked_path = Some(self.stroker.stroke_to(&path, stroke, stroked_path)?);
            } else {
                self.stroked_path = Some(self.stroker.stroke(&path, stroke)?);
            }

            self.pixmap.fill_path(self.stroked_path.as_ref()?, &paint, FillType::Winding)
        }
    }

    /// Draws a `Pixmap` on top of the current `Pixmap`.
    ///
    /// We basically filling a rectangle with a `pixmap` pattern.
    pub fn draw_pixmap(&mut self, x: i32, y: i32, pixmap: &Pixmap, paint: &PixmapPaint) {
        self.draw_pixmap_impl(x, y, pixmap, paint);
    }

    #[inline(always)]
    fn draw_pixmap_impl(&mut self, x: i32, y: i32, pixmap: &Pixmap, paint: &PixmapPaint) -> Option<()> {
        let rect = pixmap.size().to_int_rect(x, y).to_rect();

        // TODO: SkSpriteBlitter
        // TODO: partially clipped
        // TODO: clipped out

        // Translate pattern as well as bounds.
        let transform = Transform::from_translate(x as f32, y as f32)?;

        let paint = Paint {
            shader: Pattern::new(
                &pixmap,
                SpreadMode::Pad, // Pad, otherwise we will get weird borders overlap.
                paint.quality,
                paint.opacity,
                transform,
            ),
            blend_mode: paint.blend_mode,
            anti_alias: false, // Skia doesn't use it too.
            force_hq_pipeline: false, // Pattern will use hq anyway.
        };

        self.fill_rect_impl(&rect, &paint)
    }

    /// Fills a rectangle.
    ///
    /// If there is no transform - uses `Painter::fill_rect`.
    /// Otherwise, it is just a `Canvas::fill_path` with a rectangular path.
    pub fn fill_rect(&mut self, rect: &Rect, paint: &Paint) {
        self.fill_rect_impl(rect, paint);
    }

    #[inline(always)]
    fn fill_rect_impl(&mut self, rect: &Rect, paint: &Paint) -> Option<()> {
        // TODO: allow translate too
        if self.transform.is_identity() {
            self.pixmap.fill_rect(rect, paint)
        } else {
            let bounds = rect.to_bounds()?;
            let path = PathBuilder::from_bounds(bounds);
            self.fill_path_impl(&path, paint, FillType::Winding)
        }
    }
}

fn compute_res_scale_for_stroking(ts: &Transform) -> f32 {
    let (sx, ky, kx, sy, _,  _) = ts.get_row();
    let sx = Point::from_xy(sx, kx).length();
    let sy = Point::from_xy(ky, sy).length();
    if sx.is_finite() && sy.is_finite() {
        let scale = sx.max(sy);
        if scale > 0.0 {
            return scale;
        }
    }

    1.0
}

fn treat_as_hairline(paint: &Paint, stroke: StrokeProps, ts: &Transform) -> Option<f32> {
    debug_assert!(stroke.width >= 0.0);

    if stroke.width == 0.0 {
        return Some(1.0);
    }

    if !paint.anti_alias {
        return None;
    }

    // We don't care about translate.
    let ts = {
        let (sx, ky, kx, sy, _, _) = ts.get_row();
        Transform::from_row(sx, ky, kx, sy, 0.0, 0.0)?
    };

    // We need to try to fake a thick-stroke with a modulated hairline.
    let mut points = [Point::from_xy(stroke.width, 0.0), Point::from_xy(0.0, stroke.width)];
    ts.map_points(&mut points);

    let len0 = fast_len(points[0]);
    let len1 = fast_len(points[1]);

    if len0 <= 1.0 && len1 <= 1.0 {
        return Some(len0.ave(len1));
    }

    None
}

#[inline]
fn fast_len(p: Point) -> f32 {
    let mut x = p.x.abs();
    let mut y = p.y.abs();
    if x < y {
        std::mem::swap(&mut x, &mut y);
    }

    x + y.half()
}
