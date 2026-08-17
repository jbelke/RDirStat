//! Arrow encoding.
//!
//! The Arrow schema **is** the contract (docs/01-ARCHITECTURE.md#ipc-contract):
//! columns `node, depth, x, y, w, h, category`, none nullable, with the
//! generation and the schema version carried as schema metadata so the frontend
//! rejects a batch from the wrong tree instead of drawing it.

use crate::error::LayoutError;
use crate::tiles::TileBuffer;
use arrow::array::{ArrayRef, Float32Array, UInt8Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use rdirstat_core::{
    ARROW_META_GENERATION, ARROW_META_PROTOCOL_VERSION, ARROW_META_SCHEMA_NAME, ARROW_META_SCHEMA_VERSION,
    BinaryResponse, LAYOUT_COLUMNS, LAYOUT_SCHEMA_NAME, LAYOUT_SCHEMA_VERSION, PROTOCOL_VERSION, TreeGeneration,
};
use std::collections::HashMap;
use std::sync::Arc;

/// The pinned Arrow types, positionally aligned with
/// [`LAYOUT_COLUMNS`](rdirstat_core::LAYOUT_COLUMNS).
///
/// `x/y/w/h` are rectangle coordinates for treemap and icicle and
/// `(start_angle, inner_radius, sweep, thickness)` for sunburst; the layout kind
/// is the caller's, so it is not repeated in the payload.
pub const LAYOUT_COLUMN_TYPES: [DataType; 7] = [
    DataType::UInt32,
    DataType::UInt32,
    DataType::Float32,
    DataType::Float32,
    DataType::Float32,
    DataType::Float32,
    DataType::UInt8,
];

/// The `layout` schema, stamped with `generation`.
#[must_use]
pub fn layout_schema(generation: TreeGeneration) -> Schema {
    let fields: Vec<Field> = LAYOUT_COLUMNS
        .iter()
        .zip(LAYOUT_COLUMN_TYPES.iter())
        .map(|(name, data_type)| Field::new(*name, data_type.clone(), false))
        .collect();

    let mut metadata = HashMap::with_capacity(4);
    metadata.insert(ARROW_META_PROTOCOL_VERSION.to_owned(), PROTOCOL_VERSION.to_string());
    metadata.insert(ARROW_META_GENERATION.to_owned(), generation.get().to_string());
    metadata.insert(ARROW_META_SCHEMA_NAME.to_owned(), LAYOUT_SCHEMA_NAME.to_owned());
    metadata.insert(ARROW_META_SCHEMA_VERSION.to_owned(), LAYOUT_SCHEMA_VERSION.to_string());

    Schema::new_with_metadata(fields, metadata)
}

/// Wraps the tile columns in a `RecordBatch` under [`layout_schema`].
///
/// # Errors
///
/// [`LayoutError::Arrow`] if the columns disagree on length, which would be a
/// bug in [`TileBuffer`].
pub fn tiles_to_batch(tiles: &TileBuffer, generation: TreeGeneration) -> Result<RecordBatch, LayoutError> {
    let schema = Arc::new(layout_schema(generation));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from_iter_values(tiles.nodes().iter().copied())),
        Arc::new(UInt32Array::from_iter_values(tiles.depths().iter().copied())),
        Arc::new(Float32Array::from_iter_values(tiles.xs().iter().copied())),
        Arc::new(Float32Array::from_iter_values(tiles.ys().iter().copied())),
        Arc::new(Float32Array::from_iter_values(tiles.ws().iter().copied())),
        Arc::new(Float32Array::from_iter_values(tiles.hs().iter().copied())),
        Arc::new(UInt8Array::from_iter_values(tiles.categories().iter().copied())),
    ];
    Ok(RecordBatch::try_new(schema, columns)?)
}

/// Serializes the tiles as a single-batch Arrow IPC **stream**.
///
/// A stream, not a file: `apache-arrow`'s `tableFromIPC` reads either, and the
/// stream has no 8-byte magic footer to keep aligned.
///
/// # Errors
///
/// [`LayoutError::Arrow`] on a batch or writer failure.
pub fn tiles_to_ipc(tiles: &TileBuffer, generation: TreeGeneration) -> Result<Vec<u8>, LayoutError> {
    let batch = tiles_to_batch(tiles, generation)?;
    // 7 columns x 4 bytes is the dominant term; 1 KiB covers the schema block.
    let mut buffer: Vec<u8> = Vec::with_capacity(1_024 + tiles.len() * 25);
    {
        let mut writer = StreamWriter::try_new(&mut buffer, batch.schema_ref())?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    Ok(buffer)
}

/// Packages the tiles as the `layout` command's [`BinaryResponse`].
///
/// # Errors
///
/// [`LayoutError::Arrow`] on an encoding failure.
pub fn tiles_to_response(tiles: &TileBuffer, generation: TreeGeneration) -> Result<BinaryResponse, LayoutError> {
    Ok(BinaryResponse::new(
        generation,
        LAYOUT_SCHEMA_VERSION,
        tiles_to_ipc(tiles, generation)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{layout_schema, tiles_to_batch, tiles_to_ipc, tiles_to_response};
    use crate::tiles::{Tile, TileBuffer};
    use arrow::datatypes::DataType;
    use arrow::ipc::reader::StreamReader;
    use rdirstat_core::{
        ARROW_META_GENERATION, ARROW_META_PROTOCOL_VERSION, ARROW_META_SCHEMA_NAME, ARROW_META_SCHEMA_VERSION,
        CategoryId, LAYOUT_COLUMNS, LAYOUT_SCHEMA_VERSION, NodeId, PROTOCOL_VERSION, TreeGeneration,
    };

    fn buffer() -> TileBuffer {
        let mut tiles = TileBuffer::new();
        tiles.push(Tile {
            node: NodeId::ROOT,
            depth: 0,
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
            category: CategoryId::UNCATEGORIZED,
        });
        tiles.push(Tile {
            node: NodeId::from_index(4).expect("a valid index"),
            depth: 1,
            x: 10.0,
            y: 20.0,
            w: 30.0,
            h: 40.0,
            category: CategoryId::from_raw(7),
        });
        tiles
    }

    #[test]
    fn the_schema_pins_the_seven_columns_and_their_types() {
        let schema = layout_schema(TreeGeneration::FIRST);
        let names: Vec<&str> = schema.fields().iter().map(|field| field.name().as_str()).collect();
        assert_eq!(names, LAYOUT_COLUMNS.to_vec());
        assert!(schema.fields().iter().all(|field| !field.is_nullable()));
        assert_eq!(schema.field(0).data_type(), &DataType::UInt32);
        assert_eq!(schema.field(1).data_type(), &DataType::UInt32);
        assert_eq!(schema.field(2).data_type(), &DataType::Float32);
        assert_eq!(schema.field(6).data_type(), &DataType::UInt8);
    }

    #[test]
    fn the_schema_carries_generation_protocol_and_schema_version() {
        let generation = TreeGeneration::from_raw(42);
        let schema = layout_schema(generation);
        let metadata = schema.metadata();
        assert_eq!(metadata.get(ARROW_META_GENERATION).map(String::as_str), Some("42"));
        assert_eq!(
            metadata.get(ARROW_META_PROTOCOL_VERSION).map(String::as_str),
            Some(PROTOCOL_VERSION.to_string().as_str())
        );
        assert_eq!(metadata.get(ARROW_META_SCHEMA_NAME).map(String::as_str), Some("layout"));
        assert_eq!(
            metadata.get(ARROW_META_SCHEMA_VERSION).map(String::as_str),
            Some(LAYOUT_SCHEMA_VERSION.to_string().as_str())
        );
    }

    #[test]
    fn a_batch_has_one_row_per_tile() {
        let batch = tiles_to_batch(&buffer(), TreeGeneration::FIRST).expect("a valid batch");
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 7);
    }

    #[test]
    fn an_empty_buffer_round_trips_as_a_zero_row_batch() {
        let bytes = tiles_to_ipc(&TileBuffer::new(), TreeGeneration::FIRST).expect("a valid stream");
        let reader = StreamReader::try_new(bytes.as_slice(), None).expect("a readable stream");
        assert_eq!(reader.schema().fields().len(), 7);
        let rows: usize = reader.map(|batch| batch.expect("a decodable batch").num_rows()).sum();
        assert_eq!(rows, 0);
    }

    #[test]
    fn the_ipc_stream_round_trips_values_and_metadata() {
        let generation = TreeGeneration::from_raw(9);
        let bytes = tiles_to_ipc(&buffer(), generation).expect("a valid stream");
        let mut reader = StreamReader::try_new(bytes.as_slice(), None).expect("a readable stream");
        let schema = reader.schema();
        assert_eq!(
            schema.metadata().get(ARROW_META_GENERATION).map(String::as_str),
            Some("9")
        );
        let batch = reader.next().expect("one batch").expect("a decodable batch");
        assert_eq!(batch.num_rows(), 2);
        let nodes = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .expect("node is UInt32");
        assert_eq!(nodes.value(0), NodeId::ROOT.raw());
        assert_eq!(nodes.value(1), 4);
        let widths = batch
            .column(4)
            .as_any()
            .downcast_ref::<arrow::array::Float32Array>()
            .expect("w is Float32");
        assert!((widths.value(1) - 30.0).abs() < f32::EPSILON);
        assert!(reader.next().is_none(), "exactly one batch per response");
    }

    #[test]
    fn the_response_reports_the_pinned_versions() {
        let response = tiles_to_response(&buffer(), TreeGeneration::FIRST).expect("a valid response");
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        assert_eq!(response.schema_version, LAYOUT_SCHEMA_VERSION);
        assert_eq!(response.generation, TreeGeneration::FIRST);
        assert!(!response.bytes.is_empty());
    }
}
