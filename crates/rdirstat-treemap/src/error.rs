//! Layout failures.
//!
//! Everything here is a *caller* error — a viewport that is not a viewport, a
//! node that is not in the tree — or an Arrow encoding failure. A tree that is
//! merely huge, empty, or entirely sub-pixel is not an error: it produces zero
//! or few tiles.

use rdirstat_core::{NodeId, QueryError};

/// Why a layout could not be produced.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// The requested root is not in this tree (or, for a virtual `<Files>`
    /// group, its owning directory is not).
    #[error("unknown node {node}")]
    UnknownNode {
        /// The offending id.
        node: NodeId,
    },

    /// A viewport dimension was zero, negative, NaN, or infinite. The canvas
    /// has not been measured yet; the frontend must not ask for a layout of a
    /// zero-sized surface.
    #[error("viewport {field} is {value}: expected a finite value in (0, {max}]")]
    InvalidViewport {
        /// Which field: `"width"`, `"height"`, or `"device_pixel_ratio"`.
        field: &'static str,
        /// What was supplied.
        value: f32,
        /// The inclusive ceiling for that field.
        max: f32,
    },

    /// The sub-pixel cutoff was zero, negative, NaN, infinite, or absurd.
    ///
    /// The cutoff is load-bearing: it is the only thing that bounds a 69M-entry
    /// tree to a few thousand drawn tiles, so a zero cutoff is rejected rather
    /// than clamped.
    #[error("min_px is {value}: expected a finite value in (0, {max}]")]
    InvalidMinPx {
        /// What was supplied.
        value: f32,
        /// The inclusive ceiling.
        max: f32,
    },

    /// Arrow rejected the record batch or the IPC stream. Only reachable from
    /// a column-length mismatch, which is a bug in this crate.
    #[error("arrow encoding failed: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
}

impl From<LayoutError> for QueryError {
    /// Maps onto the frozen command error type.
    ///
    /// [`QueryError`] has no "bad argument" variant, so the two validation
    /// failures land in `Internal` carrying their own `Display` text. That is
    /// honest: a zero-sized viewport is a frontend bug, and the message names
    /// the offending field.
    fn from(error: LayoutError) -> Self {
        match error {
            LayoutError::UnknownNode { node } => Self::UnknownNode { node },
            other
            @ (LayoutError::InvalidViewport { .. } | LayoutError::InvalidMinPx { .. } | LayoutError::Arrow(_)) => {
                Self::Internal(other.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LayoutError;
    use rdirstat_core::{NodeId, QueryError};

    #[test]
    fn unknown_node_maps_to_the_matching_query_error() {
        let error = LayoutError::UnknownNode { node: NodeId::ROOT };
        match QueryError::from(error) {
            QueryError::UnknownNode { node } => assert_eq!(node, NodeId::ROOT),
            other => panic!("expected UnknownNode, got {other:?}"),
        }
    }

    #[test]
    fn validation_failures_map_to_internal_with_the_field_named() {
        let error = LayoutError::InvalidViewport {
            field: "width",
            value: 0.0,
            max: 65_536.0,
        };
        match QueryError::from(error) {
            QueryError::Internal(detail) => assert!(detail.contains("width"), "{detail}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
