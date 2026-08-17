//! Validated layout inputs.
//!
//! The frozen command signature is `layout(generation, root, kind, viewport,
//! min_px)`. Those raw `f32`s become the newtypes here exactly once, at the
//! crate boundary, so nothing downstream has to re-ask whether a width is
//! finite.

use crate::error::LayoutError;
use rdirstat_core::{LayoutKind, Viewport};
use serde::{Deserialize, Serialize};

/// Which byte total drives tile area.
///
/// docs/05-UI.md: "default tables and charts to allocated while labelling that
/// APFS sharing can make physical recovery smaller than the displayed
/// allocation". Logical and allocated are never summed or reconciled — a layout
/// is computed from one of them, never a blend.
///
/// It is `Serialize`/`specta::Type` because docs/05-UI.md makes this an explicit
/// user choice — "a segmented control in the toolbar that retitles the size
/// columns, so a screenshot is never ambiguous about which number it is
/// showing" — not a silent default. The frozen `layout` signature does not carry
/// it yet; when the toolbar lands, this is the type that crosses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SizeMetric {
    /// `st_blocks * 512` rolled up the subtree. The default.
    #[default]
    Allocated,
    /// Logical bytes rolled up the subtree.
    Logical,
}

/// The largest viewport edge this crate will accept, in CSS pixels.
pub const MAX_VIEWPORT_PX: f32 = 65_536.0;
/// The largest device-pixel-ratio this crate will accept.
pub const MAX_DEVICE_PIXEL_RATIO: f32 = 8.0;
/// The largest sub-pixel cutoff this crate will accept, in device pixels.
pub const MAX_MIN_PX: f32 = 4_096.0;

/// Base icicle row height in CSS pixels, before the fill pass.
pub const ICICLE_ROW_PX: f32 = 18.0;
/// Ceiling on icicle row height after the fill pass, so a two-level tree does
/// not become two 400-pixel bands.
pub const ICICLE_MAX_ROW_PX: f32 = 48.0;
/// Base sunburst ring thickness in CSS pixels, before the fill pass.
pub const SUNBURST_RING_PX: f32 = 26.0;
/// Ceiling on sunburst ring thickness after the fill pass.
pub const SUNBURST_MAX_RING_PX: f32 = 96.0;

/// Default ceiling on emitted tiles.
///
/// The sub-pixel cutoff already bounds the drawn set; this is the backstop that
/// keeps a pathological viewport (16k wide, `min_px` of 0.01) from allocating
/// without limit. Hitting it sets [`LayoutStats::truncated`](crate::LayoutStats::truncated).
pub const DEFAULT_MAX_TILES: usize = 100_000;

/// The sub-pixel cutoff, in **device** pixels.
///
/// This is the load-bearing parameter of the whole crate. A tile whose smallest
/// drawn dimension is below the cutoff is neither emitted nor recursed into, and
/// that single rule is what turns an unbounded tree into a bounded draw call.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct MinPx(f32);

impl MinPx {
    /// The pinned default, [`rdirstat_core::MIN_TILE_PX`] (3.0).
    pub const DEFAULT: Self = Self(rdirstat_core::MIN_TILE_PX);

    /// Validates a caller-supplied cutoff.
    ///
    /// # Errors
    ///
    /// [`LayoutError::InvalidMinPx`] if `px` is not finite, not positive, or
    /// above [`MAX_MIN_PX`].
    pub fn new(px: f32) -> Result<Self, LayoutError> {
        if !px.is_finite() || px <= 0.0 || px > MAX_MIN_PX {
            return Err(LayoutError::InvalidMinPx {
                value: px,
                max: MAX_MIN_PX,
            });
        }
        Ok(Self(px))
    }

    /// The cutoff in device pixels.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Default for MinPx {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A viewport that has been checked: finite, positive, and within range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Canvas {
    width: f64,
    height: f64,
    device_pixel_ratio: f64,
}

impl Canvas {
    /// Validates a caller-supplied viewport.
    ///
    /// # Errors
    ///
    /// [`LayoutError::InvalidViewport`] naming the first field that is not a
    /// finite positive number within range.
    pub fn new(viewport: Viewport) -> Result<Self, LayoutError> {
        let width = check(viewport.width, "width", MAX_VIEWPORT_PX)?;
        let height = check(viewport.height, "height", MAX_VIEWPORT_PX)?;
        let ratio = check(
            viewport.device_pixel_ratio,
            "device_pixel_ratio",
            MAX_DEVICE_PIXEL_RATIO,
        )?;
        Ok(Self {
            width: f64::from(width),
            height: f64::from(height),
            device_pixel_ratio: f64::from(ratio),
        })
    }

    /// CSS-pixel width.
    #[must_use]
    pub const fn width(self) -> f64 {
        self.width
    }

    /// CSS-pixel height.
    #[must_use]
    pub const fn height(self) -> f64 {
        self.height
    }

    /// Device pixels per CSS pixel.
    #[must_use]
    pub const fn device_pixel_ratio(self) -> f64 {
        self.device_pixel_ratio
    }

    /// The cutoff expressed in CSS pixels, which is the unit every emitted
    /// coordinate uses.
    #[must_use]
    pub fn min_side(self, min_px: MinPx) -> f64 {
        f64::from(min_px.get()) / self.device_pixel_ratio
    }

    /// Radius of the largest circle centred in this viewport, in CSS pixels.
    #[must_use]
    pub fn radius(self) -> f64 {
        self.width.min(self.height) / 2.0
    }
}

fn check(value: f32, field: &'static str, max: f32) -> Result<f32, LayoutError> {
    if !value.is_finite() || value <= 0.0 || value > max {
        return Err(LayoutError::InvalidViewport { field, value, max });
    }
    Ok(value)
}

/// Everything a layout run needs beyond the tree and the root.
///
/// Construct with [`LayoutOptions::new`] and refine with the `with_*` methods;
/// the struct is `#[non_exhaustive]` so adding a knob is not a breaking change.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct LayoutOptions {
    /// Treemap, icicle, or sunburst.
    pub kind: LayoutKind,
    /// The validated canvas.
    pub canvas: Canvas,
    /// The sub-pixel cutoff.
    pub min_px: MinPx,
    /// Which byte total drives area.
    pub metric: SizeMetric,
    /// Backstop on emitted tiles.
    pub max_tiles: usize,
}

impl LayoutOptions {
    /// Validates the raw command arguments into a runnable configuration.
    ///
    /// # Errors
    ///
    /// [`LayoutError::InvalidViewport`] or [`LayoutError::InvalidMinPx`].
    pub fn new(kind: LayoutKind, viewport: Viewport, min_px: f32) -> Result<Self, LayoutError> {
        Ok(Self {
            kind,
            canvas: Canvas::new(viewport)?,
            min_px: MinPx::new(min_px)?,
            metric: SizeMetric::Allocated,
            max_tiles: DEFAULT_MAX_TILES,
        })
    }

    /// Overrides the byte total that drives area.
    #[must_use]
    pub const fn with_metric(mut self, metric: SizeMetric) -> Self {
        self.metric = metric;
        self
    }

    /// Overrides the tile backstop. Zero is treated as "no tiles".
    #[must_use]
    pub const fn with_max_tiles(mut self, max_tiles: usize) -> Self {
        self.max_tiles = max_tiles;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{Canvas, LayoutOptions, MAX_MIN_PX, MinPx, SizeMetric};
    use crate::error::LayoutError;
    use rdirstat_core::{LayoutKind, Viewport};

    fn viewport(width: f32, height: f32, ratio: f32) -> Viewport {
        Viewport {
            width,
            height,
            device_pixel_ratio: ratio,
        }
    }

    #[test]
    fn default_cutoff_is_the_pinned_constant() {
        assert!((MinPx::default().get() - rdirstat_core::MIN_TILE_PX).abs() < f32::EPSILON);
    }

    #[test]
    fn a_zero_cutoff_is_rejected_rather_than_clamped() {
        let error = MinPx::new(0.0).expect_err("zero must be rejected");
        assert!(matches!(error, LayoutError::InvalidMinPx { .. }));
        assert!(MinPx::new(f32::NAN).is_err());
        assert!(MinPx::new(MAX_MIN_PX + 1.0).is_err());
    }

    #[test]
    fn a_zero_sized_viewport_names_the_offending_field() {
        let error = Canvas::new(viewport(0.0, 100.0, 2.0)).expect_err("zero width must be rejected");
        match error {
            LayoutError::InvalidViewport { field, .. } => assert_eq!(field, "width"),
            other => panic!("expected InvalidViewport, got {other:?}"),
        }
        let error = Canvas::new(viewport(100.0, f32::INFINITY, 2.0)).expect_err("infinite height must be rejected");
        match error {
            LayoutError::InvalidViewport { field, .. } => assert_eq!(field, "height"),
            other => panic!("expected InvalidViewport, got {other:?}"),
        }
        let error = Canvas::new(viewport(100.0, 100.0, 0.0)).expect_err("zero ratio must be rejected");
        match error {
            LayoutError::InvalidViewport { field, .. } => assert_eq!(field, "device_pixel_ratio"),
            other => panic!("expected InvalidViewport, got {other:?}"),
        }
    }

    #[test]
    fn the_cutoff_converts_into_css_pixels_through_the_device_ratio() {
        let canvas = Canvas::new(viewport(800.0, 600.0, 2.0)).expect("valid viewport");
        let min_side = canvas.min_side(MinPx::new(3.0).expect("valid cutoff"));
        assert!((min_side - 1.5).abs() < 1e-12, "{min_side}");
        assert!((canvas.radius() - 300.0).abs() < 1e-12);
    }

    #[test]
    fn the_size_metric_crosses_the_wire_in_snake_case() {
        // The frontend's segmented control sends these two strings.
        let allocated = serde_json::to_string(&SizeMetric::Allocated).expect("serializable");
        let logical = serde_json::to_string(&SizeMetric::Logical).expect("serializable");
        assert_eq!(allocated, "\"allocated\"");
        assert_eq!(logical, "\"logical\"");
        let parsed: SizeMetric = serde_json::from_str("\"logical\"").expect("deserializable");
        assert_eq!(parsed, SizeMetric::Logical);
    }

    #[test]
    fn options_default_to_allocated_bytes() {
        let options = LayoutOptions::new(LayoutKind::Treemap, viewport(800.0, 600.0, 2.0), 3.0).expect("valid options");
        assert_eq!(options.metric, SizeMetric::Allocated);
        assert_eq!(options.with_metric(SizeMetric::Logical).metric, SizeMetric::Logical);
        assert_eq!(options.with_max_tiles(7).max_tiles, 7);
    }
}
