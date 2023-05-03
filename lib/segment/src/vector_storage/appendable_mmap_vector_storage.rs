use std::fs::create_dir_all;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use atomic_refcell::AtomicRefCell;
use bitvec::prelude::BitSlice;

use crate::common::Flusher;
use crate::data_types::vectors::VectorElementType;
use crate::entry::entry_point::{check_process_stopped, OperationResult};
use crate::types::{Distance, PointOffsetType, QuantizationConfig};
use crate::vector_storage::chunked_mmap_vectors::ChunkedMmapVectors;
use crate::vector_storage::dynamic_mmap_flags::DynamicMmapFlags;
use crate::vector_storage::quantized::quantized_vectors_base::QuantizedVectorsStorage;
use crate::vector_storage::{VectorStorage, VectorStorageEnum};

pub const VECTORS_DIR_PATH: &str = "vectors";
pub const DELETED_DIR_PATH: &str = "deleted";

pub struct AppendableMmapVectorStorage {
    vectors: ChunkedMmapVectors<VectorElementType>,
    deleted: DynamicMmapFlags,
    distance: Distance,
    deleted_count: usize,
    quantized_vectors: Option<QuantizedVectorsStorage>,
}

pub fn open_appendable_memmap_vector_storage(
    path: &Path,
    dim: usize,
    distance: Distance,
) -> OperationResult<Arc<AtomicRefCell<VectorStorageEnum>>> {
    create_dir_all(path)?;

    let vectors_path = path.join(VECTORS_DIR_PATH);
    let deleted_path = path.join(DELETED_DIR_PATH);

    let vectors: ChunkedMmapVectors<VectorElementType> =
        ChunkedMmapVectors::open(&vectors_path, dim)?;

    let num_vectors = vectors.len();

    let deleted: DynamicMmapFlags = DynamicMmapFlags::open(&deleted_path)?;

    let mut deleted_count = 0;

    for i in 0..num_vectors {
        if deleted.get(i) {
            deleted_count += 1;
        }
    }

    let storage = AppendableMmapVectorStorage {
        vectors,
        deleted,
        distance,
        deleted_count,
        quantized_vectors: None,
    };

    Ok(Arc::new(AtomicRefCell::new(
        VectorStorageEnum::AppendableMemmap(storage),
    )))
}

impl AppendableMmapVectorStorage {
    /// Set deleted flag for given key. Returns previous deleted state.
    #[inline]
    fn set_deleted(&mut self, key: PointOffsetType, deleted: bool) -> OperationResult<bool> {
        if self.vectors.len() <= key as usize {
            return Ok(false);
        }

        if self.deleted.len() <= key as usize {
            self.deleted.set_len(key as usize + 1)?;
        }
        let previous = self.deleted.set(key, deleted);
        if !previous && deleted {
            self.deleted_count += 1;
        } else if previous && !deleted {
            self.deleted_count -= 1;
        }
        Ok(previous)
    }
}

impl VectorStorage for AppendableMmapVectorStorage {
    fn vector_dim(&self) -> usize {
        self.vectors.dim()
    }

    fn distance(&self) -> Distance {
        self.distance
    }

    fn total_vector_count(&self) -> usize {
        self.vectors.len()
    }

    fn get_vector(&self, key: PointOffsetType) -> &[VectorElementType] {
        self.vectors.get(key)
    }

    fn insert_vector(
        &mut self,
        key: PointOffsetType,
        vector: &[VectorElementType],
    ) -> OperationResult<()> {
        self.vectors.insert(key, vector)
    }

    fn update_from(
        &mut self,
        other: &VectorStorageEnum,
        other_ids: &mut dyn Iterator<Item = PointOffsetType>,
        stopped: &AtomicBool,
    ) -> OperationResult<Range<PointOffsetType>> {
        let start_index = self.vectors.len() as PointOffsetType;
        for point_id in other_ids {
            check_process_stopped(stopped)?;
            // Do not perform preprocessing - vectors should be already processed
            let other_deleted = other.is_deleted_vector(point_id);
            let other_vector = other.get_vector(point_id);
            let new_id = self.vectors.push(other_vector)?;
            self.set_deleted(new_id, other_deleted)?;
        }
        let end_index = self.vectors.len() as PointOffsetType;
        Ok(start_index..end_index)
    }

    fn flusher(&self) -> Flusher {
        todo!();
        // Box::new({
        //     let vectors = self.vectors.clone();
        //     let deleted_flusher = self.deleted.flusher();
        //     move || {
        //         vectors.read().flush()?;
        //         deleted_flusher()?;
        //         Ok(())
        //     }
        // })
    }

    fn quantize(
        &mut self,
        path: &Path,
        quantization_config: &QuantizationConfig,
    ) -> OperationResult<()> {
        let vector_data_iterator = (0..self.vectors.len() as u32).map(|i| self.vectors.get(i));
        self.quantized_vectors = Some(QuantizedVectorsStorage::create(
            vector_data_iterator,
            quantization_config,
            self.distance,
            self.vectors.dim(),
            self.vectors.len(),
            path,
            true,
        )?);
        Ok(())
    }

    fn load_quantization(&mut self, path: &Path) -> OperationResult<()> {
        if QuantizedVectorsStorage::check_exists(path) {
            self.quantized_vectors =
                Some(QuantizedVectorsStorage::load(path, true, self.distance)?);
        }
        Ok(())
    }

    fn quantized_storage(&self) -> Option<&QuantizedVectorsStorage> {
        self.quantized_vectors.as_ref()
    }

    fn files(&self) -> Vec<PathBuf> {
        let mut files = self.vectors.files();
        files.extend(self.deleted.files());
        files
    }

    fn delete_vector(&mut self, key: PointOffsetType) -> OperationResult<bool> {
        self.set_deleted(key, true)
    }

    fn is_deleted_vector(&self, key: PointOffsetType) -> bool {
        self.deleted.get(key)
    }

    fn deleted_vector_count(&self) -> usize {
        self.deleted_count
    }

    fn deleted_vector_bitslice(&self) -> &BitSlice {
        self.deleted.get_bitslice()
    }
}
