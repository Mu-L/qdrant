use std::path::{Path, PathBuf};

use common::counter::hardware_counter::HardwareCounterCell;
use common::counter::iterator_hw_measurement::HwMeasurementIteratorExt;
use common::types::PointOffsetType;
use serde_json::Value;

use crate::common::Flusher;
use crate::common::operation_error::{OperationError, OperationResult};
use crate::index::field_index::{
    CardinalityEstimation, FieldIndexBuilderTrait, PayloadBlockCondition, PayloadFieldIndex,
    PrimaryCondition,
};
use crate::index::payload_config::{IndexMutability, StorageType};
use crate::telemetry::PayloadIndexTelemetry;
use crate::types::{FieldCondition, PayloadKeyType};
use crate::vector_storage::dense::dynamic_mmap_flags::DynamicMmapFlags;

const HAS_VALUES_DIRNAME: &str = "has_values";
const IS_NULL_DIRNAME: &str = "is_null";

/// Special type of payload index that is supposed to speed-up IsNull and IsEmpty conditions.
/// This index is supposed to be a satellite index for the main index.
/// Majority of the time this index will be empty, but it is supposed to prevent expensive disk reads
/// in case of IsNull and IsEmpty conditions.
pub struct MmapNullIndex {
    base_dir: PathBuf,
    storage: Option<Storage>,
    total_point_count: usize,
}

struct Storage {
    /// If true, payload field has some values.
    has_values_slice: DynamicMmapFlags,
    /// If true, then payload field contains null value.
    is_null_slice: DynamicMmapFlags,
}

/// Don't populate null index as it is not essential
/// and will be populated on the fly fast enough
const POPULATE_NULL_INDEX: bool = false;

impl MmapNullIndex {
    pub fn builder(path: &Path) -> OperationResult<MmapNullIndexBuilder> {
        Ok(MmapNullIndexBuilder(Self::open(path, 0, true)?))
    }

    /// Open or create a null index at the given path.
    ///
    /// # Arguments
    /// - `path` - The directory where the index files should live, must be exclusive to this index.
    /// - `total_point_count` - Total number of points in the segment.
    /// - `create_if_missing` - If true, creates the index if it doesn't exist.
    pub fn open(
        path: &Path,
        total_point_count: usize,
        create_if_missing: bool,
    ) -> OperationResult<Self> {
        let has_values_dir = path.join(HAS_VALUES_DIRNAME);

        // If has values directory doesn't exist, assume the index doesn't exist on disk
        if !has_values_dir.is_dir() && !create_if_missing {
            return Ok(Self {
                base_dir: path.to_path_buf(),
                storage: None,
                total_point_count,
            });
        }

        Self::open_or_create(path, total_point_count)
    }

    fn open_or_create(path: &Path, total_point_count: usize) -> OperationResult<Self> {
        std::fs::create_dir_all(path).map_err(|err| {
            OperationError::service_error(format!(
                "Failed to create null-index directory: {err}, path: {path:?}"
            ))
        })?;

        let has_values_path = path.join(HAS_VALUES_DIRNAME);
        let has_values_slice = DynamicMmapFlags::open(&has_values_path, POPULATE_NULL_INDEX)?;

        let is_null_path = path.join(IS_NULL_DIRNAME);
        let is_null_slice = DynamicMmapFlags::open(&is_null_path, POPULATE_NULL_INDEX)?;

        Ok(Self {
            base_dir: path.to_path_buf(),
            storage: Some(Storage {
                has_values_slice,
                is_null_slice,
            }),
            total_point_count,
        })
    }

    /// Open a null index at the given path, only if it exists.
    ///
    /// # Arguments
    /// - `path` - The directory where the index files should live, must be exclusive to this index.
    /// - `total_point_count` - Total number of points in the segment.
    /// - `create_if_missing` - If true, creates the index if it doesn't exist.
    pub fn open_if_exists(
        path: &Path,
        total_point_count: usize,
        create_if_missing: bool,
    ) -> OperationResult<Option<Self>> {
        if !path.is_dir() {
            if create_if_missing {
                return Ok(Some(Self::open_or_create(path, total_point_count)?));
            }
            return Ok(None);
        }

        let has_values_path = path.join(HAS_VALUES_DIRNAME);
        let is_null_path = path.join(IS_NULL_DIRNAME);

        if has_values_path.exists() && is_null_path.exists() {
            let has_values_slice = DynamicMmapFlags::open(&has_values_path, POPULATE_NULL_INDEX)?;
            let is_null_slice = DynamicMmapFlags::open(&is_null_path, POPULATE_NULL_INDEX)?;
            Ok(Some(Self {
                base_dir: path.to_path_buf(),
                storage: Some(Storage {
                    has_values_slice,
                    is_null_slice,
                }),
                total_point_count,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn add_point(
        &mut self,
        id: PointOffsetType,
        payload: &[&Value],
        hw_counter: &HardwareCounterCell,
    ) -> OperationResult<()> {
        let Some(storage) = &mut self.storage else {
            return Err(OperationError::service_error(
                "MmapNullIndex storage is not initialized".to_string(),
            ));
        };

        let mut is_null = false;
        let mut has_values = false;
        for value in payload {
            match value {
                Value::Null => {
                    is_null = true;
                }
                Value::Bool(_) => {
                    has_values = true;
                }
                Value::Number(_) => {
                    has_values = true;
                }
                Value::String(_) => {
                    has_values = true;
                }
                Value::Array(array) => {
                    if array.iter().any(|v| v.is_null()) {
                        is_null = true;
                    }
                    if !array.is_empty() {
                        has_values = true;
                    }
                }
                Value::Object(_) => {
                    has_values = true;
                }
            }
            if is_null && has_values {
                break;
            }
        }

        let hw_counter_ref = hw_counter.ref_payload_index_io_write_counter();

        storage
            .has_values_slice
            .set_with_resize(id, has_values, hw_counter_ref)?;
        storage
            .is_null_slice
            .set_with_resize(id, is_null, hw_counter_ref)?;

        // Bump total points
        self.total_point_count = std::cmp::max(self.total_point_count, id as usize + 1);

        Ok(())
    }

    pub fn remove_point(&mut self, id: PointOffsetType) -> OperationResult<()> {
        let Some(storage) = &mut self.storage else {
            return Ok(());
        };

        let disposed_hw = HardwareCounterCell::disposable(); // Deleting is unmeasured OP.
        let disposed_hw = disposed_hw.ref_payload_index_io_write_counter();

        storage
            .has_values_slice
            .set_with_resize(id, false, disposed_hw)?;
        storage
            .is_null_slice
            .set_with_resize(id, false, disposed_hw)?;

        // Bump total points
        // We MUST bump the total point count when removing a point too
        // On upsert without this respective field, remove point is called rather than add point
        // Bumping the total point count ensures we correctly iterate over all empty points
        // Bug: <https://github.com/qdrant/qdrant/pull/6882>
        self.total_point_count = std::cmp::max(self.total_point_count, id as usize + 1);

        Ok(())
    }

    pub fn values_count(&self, id: PointOffsetType) -> usize {
        self.storage
            .as_ref()
            .map_or(0, |storage| usize::from(storage.has_values_slice.get(id)))
    }

    pub fn values_is_empty(&self, id: PointOffsetType) -> bool {
        self.storage
            .as_ref()
            .is_none_or(|storage| !storage.has_values_slice.get(id))
    }

    pub fn values_is_null(&self, id: PointOffsetType) -> bool {
        self.storage
            .as_ref()
            .is_some_and(|storage| storage.is_null_slice.get(id))
    }

    pub fn get_telemetry_data(&self) -> PayloadIndexTelemetry {
        let points_count = self
            .storage
            .as_ref()
            .map_or(0, |storage| storage.has_values_slice.len());
        PayloadIndexTelemetry {
            field_name: None,
            points_count,
            points_values_count: points_count,
            histogram_bucket_size: None,
            index_type: "mmap_null_index",
        }
    }

    pub fn is_on_disk(&self) -> bool {
        !POPULATE_NULL_INDEX
    }

    /// Populate all pages in the mmap.
    /// Block until all pages are populated.
    pub fn populate(&self) -> OperationResult<()> {
        if let Some(storage) = &self.storage {
            storage.is_null_slice.populate()?;
            storage.has_values_slice.populate()?;
        }
        Ok(())
    }

    /// Drop disk cache.
    pub fn clear_cache(&self) -> OperationResult<()> {
        if let Some(storage) = &self.storage {
            storage.is_null_slice.clear_cache()?;
            storage.has_values_slice.clear_cache()?;
        }

        Ok(())
    }

    pub fn get_mutability_type(&self) -> IndexMutability {
        // Mmap null index can be both mutable and immutable, so we pick mutable
        IndexMutability::Mutable
    }

    pub fn get_storage_type(&self) -> StorageType {
        StorageType::Mmap {
            is_on_disk: self.is_on_disk(),
        }
    }
}

impl PayloadFieldIndex for MmapNullIndex {
    fn count_indexed_points(&self) -> usize {
        self.storage
            .as_ref()
            .map_or(0, |storage| storage.has_values_slice.len())
    }

    fn load(&mut self) -> OperationResult<bool> {
        let is_loaded = self.storage.is_some();
        Ok(is_loaded)
    }

    fn cleanup(self) -> OperationResult<()> {
        std::fs::remove_dir_all(&self.base_dir)?;
        Ok(())
    }

    fn flusher(&self) -> Flusher {
        let Some(storage) = &self.storage else {
            return Box::new(|| Ok(()));
        };

        let Self {
            base_dir: _,
            storage: _,
            total_point_count: _,
        } = self;
        let Storage {
            has_values_slice,
            is_null_slice,
        } = storage;

        let is_empty_flusher = has_values_slice.flusher();
        let is_null_flusher = is_null_slice.flusher();

        Box::new(move || {
            is_empty_flusher()?;
            is_null_flusher()?;
            Ok(())
        })
    }

    fn files(&self) -> Vec<PathBuf> {
        let Some(storage) = &self.storage else {
            return vec![];
        };

        let Self {
            base_dir: _,
            storage: _,
            total_point_count: _,
        } = self;
        let Storage {
            has_values_slice,
            is_null_slice,
        } = storage;

        let mut files = has_values_slice.files();
        files.extend(is_null_slice.files());
        files
    }

    fn immutable_files(&self) -> Vec<PathBuf> {
        Vec::new() // everything is mutable
    }

    fn filter<'a>(
        &'a self,
        condition: &'a FieldCondition,
        hw_counter: &'a HardwareCounterCell,
    ) -> Option<Box<dyn Iterator<Item = PointOffsetType> + 'a>> {
        let Some(storage) = &self.storage else {
            return None;
        };

        let FieldCondition {
            key: _,
            r#match: _,
            range: _,
            geo_bounding_box: _,
            geo_radius: _,
            geo_polygon: _,
            values_count: _,
            is_empty,
            is_null,
        } = condition;

        if let Some(is_empty) = is_empty {
            hw_counter
                .payload_index_io_read_counter()
                .incr_delta(storage.has_values_slice.len() / u8::BITS as usize);

            if *is_empty {
                // Iterate over all tracked values, but filter out those which have a value
                let iter = (0..self.total_point_count as PointOffsetType)
                    .filter(move |&id| !storage.has_values_slice.get(id))
                    .measure_hw_with_cell(hw_counter, 1, |i| i.payload_index_io_read_counter());
                Some(Box::new(iter))
            } else {
                // Non-empty values are registered in the index explicitly
                let iter = storage.has_values_slice.iter_trues().measure_hw_with_cell(
                    hw_counter,
                    1,
                    |i| i.payload_index_io_read_counter(),
                );
                Some(Box::new(iter))
            }
        } else if let Some(is_null) = is_null {
            hw_counter
                .payload_index_io_read_counter()
                .incr_delta(storage.is_null_slice.len() / u8::BITS as usize);
            if *is_null {
                // We DO have list of all null values, so we can iterate over them
                // Null values are explicitly marked in the index
                let iter =
                    storage
                        .is_null_slice
                        .iter_trues()
                        .measure_hw_with_cell(hw_counter, 1, |i| i.payload_index_io_read_counter());
                Some(Box::new(iter))
            } else {
                // Iterate over all tracked values, but filter out those which are null
                let iter = (0..self.total_point_count as PointOffsetType)
                    .filter(move |&id| !storage.is_null_slice.get(id))
                    .measure_hw_with_cell(hw_counter, 1, |i| i.payload_index_io_read_counter());
                Some(Box::new(iter))
            }
        } else {
            None
        }
    }

    fn estimate_cardinality(
        &self,
        condition: &FieldCondition,
        hw_counter: &HardwareCounterCell,
    ) -> Option<CardinalityEstimation> {
        let Some(storage) = &self.storage else {
            return None;
        };

        let FieldCondition {
            key,
            r#match: _,
            range: _,
            geo_bounding_box: _,
            geo_radius: _,
            geo_polygon: _,
            values_count: _,
            is_empty,
            is_null,
        } = condition;

        if let Some(is_empty) = is_empty {
            hw_counter
                .payload_index_io_read_counter()
                .incr_delta(storage.has_values_slice.len() / u8::BITS as usize);
            if *is_empty {
                // We can estimate using the total_point_count, but not exactly since we don't know which are deleted
                let estimated = self
                    .total_point_count
                    .saturating_sub(storage.has_values_slice.count_flags());

                Some(CardinalityEstimation {
                    min: 0,
                    exp: 2 * estimated / 3, // assuming 1/3 of the points are deleted
                    max: estimated,
                    primary_clauses: vec![PrimaryCondition::from(FieldCondition::new_is_empty(
                        key.clone(),
                        true,
                    ))],
                })
            } else {
                // All non-empty values are explicitly marked in the index
                Some(
                    CardinalityEstimation::exact(storage.has_values_slice.count_flags())
                        .with_primary_clause(PrimaryCondition::from(FieldCondition::new_is_empty(
                            key.clone(),
                            false,
                        ))),
                )
            }
        } else if let Some(is_null) = is_null {
            hw_counter
                .payload_index_io_read_counter()
                .incr_delta(storage.is_null_slice.len() / u8::BITS as usize);

            if *is_null {
                // Null values are explicitly marked in the index
                Some(
                    CardinalityEstimation::exact(storage.is_null_slice.count_flags())
                        .with_primary_clause(PrimaryCondition::from(FieldCondition::new_is_null(
                            key.clone(),
                            true,
                        ))),
                )
            } else {
                // We can estimate the non-null values from the total number of values
                let estimated = self
                    .total_point_count
                    .saturating_sub(storage.is_null_slice.count_flags());

                Some(CardinalityEstimation {
                    min: 0,                 // assuming all points are deleted
                    exp: 2 * estimated / 3, // assuming 1/3 of the points are deleted
                    max: estimated,
                    primary_clauses: vec![PrimaryCondition::from(FieldCondition::new_is_null(
                        key.clone(),
                        false,
                    ))],
                })
            }
        } else {
            None
        }
    }

    fn payload_blocks(
        &self,
        _threshold: usize,
        _key: PayloadKeyType,
    ) -> Box<dyn Iterator<Item = PayloadBlockCondition> + '_> {
        // No payload blocks
        Box::new(std::iter::empty())
    }
}

pub struct MmapNullIndexBuilder(MmapNullIndex);

impl FieldIndexBuilderTrait for MmapNullIndexBuilder {
    type FieldIndexType = MmapNullIndex;

    fn init(&mut self) -> OperationResult<()> {
        // After Self is created, it is already initialized
        Ok(())
    }

    fn add_point(
        &mut self,
        id: PointOffsetType,
        payload: &[&serde_json::Value],
        hw_counter: &HardwareCounterCell,
    ) -> OperationResult<()> {
        self.0.add_point(id, payload, hw_counter)
    }

    fn finalize(self) -> OperationResult<Self::FieldIndexType> {
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use common::counter::hardware_accumulator::HwMeasurementAcc;
    use tempfile::TempDir;

    use super::*;
    use crate::json_path::JsonPath;

    #[test]
    fn test_build_and_use_null_index() {
        let dir = TempDir::with_prefix("test_null_index").unwrap();

        let null_value = Value::Null;
        let null_value_in_array =
            Value::Array(vec![Value::String("test".to_string()), Value::Null]);

        let mut builder = MmapNullIndex::builder(dir.path()).unwrap();

        let n = 100;

        let hw_counter = HardwareCounterCell::new();

        for i in 0..n {
            match i % 4 {
                0 => builder.add_point(i, &[&null_value], &hw_counter).unwrap(),
                1 => builder
                    .add_point(i, &[&null_value_in_array], &hw_counter)
                    .unwrap(),
                2 => builder.add_point(i, &[], &hw_counter).unwrap(),
                3 => builder
                    .add_point(i, &[&Value::Bool(true)], &hw_counter)
                    .unwrap(),
                _ => unreachable!(),
            }
        }

        let null_index = builder.finalize().unwrap();
        let key = JsonPath::new("test");

        let filter_is_null = FieldCondition::new_is_null(key.clone(), true);

        let filter_is_not_empty = FieldCondition {
            key: key.clone(),
            r#match: None,
            range: None,
            geo_bounding_box: None,
            geo_radius: None,
            geo_polygon: None,
            values_count: None,
            is_empty: Some(false),
            is_null: None,
        };

        let hw_acc = HwMeasurementAcc::new();
        let hw_counter = hw_acc.get_counter_cell();

        let is_null_values: Vec<_> = null_index
            .filter(&filter_is_null, &hw_counter)
            .unwrap()
            .collect();
        let not_empty_values: Vec<_> = null_index
            .filter(&filter_is_not_empty, &hw_counter)
            .unwrap()
            .collect();

        let is_empty_values: Vec<_> = (0..n)
            .filter(|&id| null_index.values_is_empty(id))
            .collect();
        let not_null_values: Vec<_> = (0..n)
            .filter(|&id| !null_index.values_is_null(id))
            .collect();

        for i in 0..n {
            match i % 4 {
                0 => {
                    // &[&null_value]
                    assert!(is_null_values.contains(&i));
                    assert!(!not_empty_values.contains(&i));

                    assert!(!not_null_values.contains(&i));
                    assert!(is_empty_values.contains(&i));
                }
                1 => {
                    // &[&null_value_in_array]
                    assert!(is_null_values.contains(&i));
                    assert!(not_empty_values.contains(&i));

                    assert!(!not_null_values.contains(&i));
                    assert!(!is_empty_values.contains(&i));
                }
                2 => {
                    // &[]
                    assert!(!is_null_values.contains(&i));
                    assert!(!not_empty_values.contains(&i));

                    assert!(not_null_values.contains(&i));
                    assert!(is_empty_values.contains(&i));
                }
                3 => {
                    // &[&Value::Bool(true)]
                    assert!(!is_null_values.contains(&i));
                    assert!(not_empty_values.contains(&i));

                    assert!(not_null_values.contains(&i));
                    assert!(!is_empty_values.contains(&i));
                }
                _ => unreachable!(),
            }
        }

        let hw_cell = HardwareCounterCell::new();
        let is_null_cardinality = null_index
            .estimate_cardinality(&filter_is_null, &hw_cell)
            .unwrap();
        let non_empty_cardinality = null_index
            .estimate_cardinality(&filter_is_not_empty, &hw_cell)
            .unwrap();

        assert_eq!(is_null_cardinality.exp, 50);
        assert_eq!(non_empty_cardinality.exp, 50);
    }
}
