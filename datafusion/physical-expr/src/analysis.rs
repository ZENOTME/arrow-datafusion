// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Interval and selectivity in [`AnalysisContext`]

use std::fmt::Debug;
use std::sync::Arc;

use crate::expressions::Column;
use crate::intervals::cp_solver::PropagationResult;
use crate::intervals::{cardinality_ratio, ExprIntervalGraph, Interval, IntervalBound};
use crate::utils::collect_columns;
use crate::PhysicalExpr;

use arrow::datatypes::Schema;
use datafusion_common::stats::Precision;
use datafusion_common::{
    internal_err, ColumnStatistics, DataFusionError, Result, ScalarValue,
};

/// The shared context used during the analysis of an expression. Includes
/// the boundaries for all known columns.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalysisContext {
    // A list of known column boundaries, ordered by the index
    // of the column in the current schema.
    pub boundaries: Vec<ExprBoundaries>,
    /// The estimated percentage of rows that this expression would select, if
    /// it were to be used as a boolean predicate on a filter. The value will be
    /// between 0.0 (selects nothing) and 1.0 (selects everything).
    pub selectivity: Option<f64>,
}

impl AnalysisContext {
    pub fn new(boundaries: Vec<ExprBoundaries>) -> Self {
        Self {
            boundaries,
            selectivity: None,
        }
    }

    pub fn with_selectivity(mut self, selectivity: f64) -> Self {
        self.selectivity = Some(selectivity);
        self
    }

    /// Create a new analysis context from column statistics.
    pub fn try_from_statistics(
        input_schema: &Schema,
        statistics: &[ColumnStatistics],
    ) -> Result<Self> {
        statistics
            .iter()
            .enumerate()
            .map(|(idx, stats)| ExprBoundaries::try_from_column(input_schema, stats, idx))
            .collect::<Result<Vec<_>>>()
            .map(Self::new)
    }
}

/// Represents the boundaries of the resulting value from a physical expression,
/// if it were to be an expression, if it were to be evaluated.
#[derive(Clone, Debug, PartialEq)]
pub struct ExprBoundaries {
    pub column: Column,
    /// Minimum and maximum values this expression can have.
    pub interval: Interval,
    /// Maximum number of distinct values this expression can produce, if known.
    pub distinct_count: Precision<usize>,
}

impl ExprBoundaries {
    /// Create a new `ExprBoundaries` object from column level statistics.
    pub fn try_from_column(
        schema: &Schema,
        col_stats: &ColumnStatistics,
        col_index: usize,
    ) -> Result<Self> {
        let field = &schema.fields()[col_index];
        let empty_field = ScalarValue::try_from(field.data_type())?;
        let interval = Interval::new(
            IntervalBound::new_closed(
                col_stats
                    .min_value
                    .get_value()
                    .cloned()
                    .unwrap_or(empty_field.clone()),
            ),
            IntervalBound::new_closed(
                col_stats
                    .max_value
                    .get_value()
                    .cloned()
                    .unwrap_or(empty_field),
            ),
        );
        let column = Column::new(field.name(), col_index);
        Ok(ExprBoundaries {
            column,
            interval,
            distinct_count: col_stats.distinct_count.clone(),
        })
    }
}

/// Attempts to refine column boundaries and compute a selectivity value.
///
/// The function accepts boundaries of the input columns in the `context` parameter.
/// It then tries to tighten these boundaries based on the provided `expr`.
/// The resulting selectivity value is calculated by comparing the initial and final boundaries.
/// The computation assumes that the data within the column is uniformly distributed and not sorted.
///
/// # Arguments
///
/// * `context` - The context holding input column boundaries.
/// * `expr` - The expression used to shrink the column boundaries.
///
/// # Returns
///
/// * `AnalysisContext` constructed by pruned boundaries and a selectivity value.
pub fn analyze(
    expr: &Arc<dyn PhysicalExpr>,
    context: AnalysisContext,
) -> Result<AnalysisContext> {
    let target_boundaries = context.boundaries;

    let mut graph = ExprIntervalGraph::try_new(expr.clone())?;

    let columns: Vec<Arc<dyn PhysicalExpr>> = collect_columns(expr)
        .into_iter()
        .map(|c| Arc::new(c) as Arc<dyn PhysicalExpr>)
        .collect();

    let target_expr_and_indices: Vec<(Arc<dyn PhysicalExpr>, usize)> =
        graph.gather_node_indices(columns.as_slice());

    let mut target_indices_and_boundaries: Vec<(usize, Interval)> =
        target_expr_and_indices
            .iter()
            .filter_map(|(expr, i)| {
                target_boundaries.iter().find_map(|bound| {
                    expr.as_any()
                        .downcast_ref::<Column>()
                        .filter(|expr_column| bound.column.eq(*expr_column))
                        .map(|_| (*i, bound.interval.clone()))
                })
            })
            .collect();
    Ok(
        match graph.update_ranges(&mut target_indices_and_boundaries)? {
            PropagationResult::Success => shrink_boundaries(
                expr,
                graph,
                target_boundaries,
                target_expr_and_indices,
            )?,
            PropagationResult::Infeasible => {
                AnalysisContext::new(target_boundaries).with_selectivity(0.0)
            }
            PropagationResult::CannotPropagate => {
                AnalysisContext::new(target_boundaries).with_selectivity(1.0)
            }
        },
    )
}

/// If the `PropagationResult` indicates success, this function calculates the
/// selectivity value by comparing the initial and final column boundaries.
/// Following this, it constructs and returns a new `AnalysisContext` with the
/// updated parameters.
fn shrink_boundaries(
    expr: &Arc<dyn PhysicalExpr>,
    mut graph: ExprIntervalGraph,
    mut target_boundaries: Vec<ExprBoundaries>,
    target_expr_and_indices: Vec<(Arc<dyn PhysicalExpr>, usize)>,
) -> Result<AnalysisContext> {
    let initial_boundaries = target_boundaries.clone();
    target_expr_and_indices.iter().for_each(|(expr, i)| {
        if let Some(column) = expr.as_any().downcast_ref::<Column>() {
            if let Some(bound) = target_boundaries
                .iter_mut()
                .find(|bound| bound.column.eq(column))
            {
                bound.interval = graph.get_interval(*i);
            };
        }
    });
    let graph_nodes = graph.gather_node_indices(&[expr.clone()]);
    let Some((_, root_index)) = graph_nodes.get(0) else {
        return internal_err!(
            "The ExprIntervalGraph under investigation does not have any nodes."
        );
    };
    let final_result = graph.get_interval(*root_index);

    let selectivity = calculate_selectivity(
        &final_result.lower.value,
        &final_result.upper.value,
        &target_boundaries,
        &initial_boundaries,
    )?;

    Ok(AnalysisContext::new(target_boundaries).with_selectivity(selectivity))
}

/// This function calculates the filter predicate's selectivity by comparing
/// the initial and pruned column boundaries. Selectivity is defined as the
/// ratio of rows in a table that satisfy the filter's predicate.
///
/// An exact propagation result at the root, i.e. `[true, true]` or `[false, false]`,
/// leads to early exit (returning a selectivity value of either 1.0 or 0.0). In such
/// a case, `[true, true]` indicates that all data values satisfy the predicate (hence,
/// selectivity is 1.0), and `[false, false]` suggests that no data value meets the
/// predicate (therefore, selectivity is 0.0).
fn calculate_selectivity(
    lower_value: &ScalarValue,
    upper_value: &ScalarValue,
    target_boundaries: &[ExprBoundaries],
    initial_boundaries: &[ExprBoundaries],
) -> Result<f64> {
    match (lower_value, upper_value) {
        (ScalarValue::Boolean(Some(true)), ScalarValue::Boolean(Some(true))) => Ok(1.0),
        (ScalarValue::Boolean(Some(false)), ScalarValue::Boolean(Some(false))) => Ok(0.0),
        _ => {
            // Since the intervals are assumed uniform and the values
            // are not correlated, we need to multiply the selectivities
            // of multiple columns to get the overall selectivity.
            target_boundaries.iter().enumerate().try_fold(
                1.0,
                |acc, (i, ExprBoundaries { interval, .. })| {
                    let temp =
                        cardinality_ratio(&initial_boundaries[i].interval, interval)?;
                    Ok(acc * temp)
                },
            )
        }
    }
}
