use std::ops::Deref;

use ahash::AHashSet;
use common::types::PointOffsetType;

use crate::types::{Condition, FieldCondition, PointIdType, VectorNameBuf};

pub mod bool_index;
pub(super) mod facet_index;
mod field_index_base;
pub mod full_text_index;
pub mod geo_hash;
pub mod geo_index;
mod histogram;
mod immutable_point_to_values;
pub mod index_selector;
pub mod map_index;
mod mmap_point_to_values;
pub mod null_index;
pub mod numeric_index;
mod stat_tools;
#[cfg(test)]
mod tests;
mod utils;

pub use field_index_base::*;

use crate::utils::maybe_arc::MaybeArc;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedHasId {
    /// Original IDs, as provided in filtering condition
    pub point_ids: MaybeArc<AHashSet<PointIdType>>,

    /// Resolved point offsets, which are specific to the segment.
    pub resolved_point_offsets: Vec<PointOffsetType>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrimaryCondition {
    Condition(Box<FieldCondition>),
    Ids(ResolvedHasId),
    HasVector(VectorNameBuf),
}

impl From<FieldCondition> for PrimaryCondition {
    fn from(condition: FieldCondition) -> Self {
        PrimaryCondition::Condition(Box::new(condition))
    }
}

#[derive(Debug, Clone)]
pub struct PayloadBlockCondition {
    pub condition: FieldCondition,
    pub cardinality: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardinalityEstimation {
    /// Conditions that could be used to make a primary point selection.
    pub primary_clauses: Vec<PrimaryCondition>,
    /// Minimal possible matched points in best case for a query
    pub min: usize,
    /// Expected number of matched points for a query, assuming even random distribution if stored data
    pub exp: usize,
    /// The largest possible number of matched points in a worst case for a query
    pub max: usize,
}

impl CardinalityEstimation {
    pub const fn exact(count: usize) -> Self {
        CardinalityEstimation {
            primary_clauses: vec![],
            min: count,
            exp: count,
            max: count,
        }
    }

    /// Generate estimation for unknown filter
    pub const fn unknown(total: usize) -> Self {
        CardinalityEstimation {
            primary_clauses: vec![],
            min: 0,
            exp: total / 2,
            max: total,
        }
    }

    /// Push a primary clause to the estimation
    pub fn with_primary_clause(mut self, clause: PrimaryCondition) -> Self {
        self.primary_clauses.push(clause);
        self
    }

    #[cfg(test)]
    pub const fn equals_min_exp_max(&self, other: &Self) -> bool {
        self.min == other.min && self.exp == other.exp && self.max == other.max
    }

    /// Checks that the given condition is a primary condition of the estimation.
    pub fn is_primary(&self, condition: &Condition) -> bool {
        self.primary_clauses
            .iter()
            .any(|primary_condition| match primary_condition {
                PrimaryCondition::Condition(primary_field_condition) => match condition {
                    Condition::Field(field_condition) => {
                        primary_field_condition.as_ref() == field_condition
                    }
                    _ => false,
                },
                PrimaryCondition::Ids(ids) => match condition {
                    Condition::HasId(has_id) => ids.point_ids.deref() == has_id.has_id.deref(),
                    _ => false,
                },
                PrimaryCondition::HasVector(has_vector) => match condition {
                    Condition::HasVector(vector_condition) => {
                        has_vector == &vector_condition.has_vector
                    }
                    _ => false,
                },
            })
    }
}
