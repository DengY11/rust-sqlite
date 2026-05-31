use std::cmp::{Ordering, Reverse};
use std::collections::HashMap;

use crate::common::error::Result;
use crate::common::types::{IndexMeta, Value};
use crate::sql::ast::{CompareOp, Expr};
use crate::sql::plan::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, Plan};
use crate::sql::planner::PlanningContext;

trait OptimizerPass {
    fn name(&self) -> &'static str;
    fn optimize(&self, plan: Plan, context: &PlanningContext) -> Result<Plan>;
}

#[derive(Debug, Clone, Copy)]
struct IndexSelectionPass;

impl OptimizerPass for IndexSelectionPass {
    fn name(&self) -> &'static str {
        "index_selection"
    }

    fn optimize(&self, plan: Plan, context: &PlanningContext) -> Result<Plan> {
        match plan {
            Plan::SeqScan {
                table,
                table_alias,
                columns,
                filter,
                order_by,
                limit,
                distinct,
            } if self.is_plain_indexable_filter(filter.as_ref()) => {
                if let Some(expr) = filter.as_ref() {
                    if let Some(scans) = self.find_index_union_scans(context, &table, expr) {
                        return Ok(Plan::IndexUnion {
                            table,
                            table_alias,
                            columns,
                            scans,
                            filter,
                            order_by,
                            limit,
                            distinct,
                        });
                    }

                    if let Some(scan) = self.find_matching_index_scan(context, &table, expr) {
                        return Ok(Plan::IndexScan {
                            table,
                            table_alias,
                            columns,
                            index: scan.index,
                            mode: scan.mode,
                            key_prefix: scan.key_prefix,
                            range: scan.range,
                            filter,
                            order_by,
                            limit,
                            distinct,
                        });
                    }
                }

                Ok(Plan::SeqScan {
                    table,
                    table_alias,
                    columns,
                    filter,
                    order_by,
                    limit,
                    distinct,
                })
            }
            Plan::Aggregate {
                source,
                columns,
                group_by,
                having,
                order_by,
                limit,
            } => Ok(Plan::Aggregate {
                source: Box::new(self.optimize(*source, context)?),
                columns,
                group_by,
                having,
                order_by,
                limit,
            }),
            Plan::ExplainQueryPlan { plan } => Ok(Plan::ExplainQueryPlan {
                plan: Box::new(self.optimize(*plan, context)?),
            }),
            plan => Ok(plan),
        }
    }
}

impl IndexSelectionPass {
    fn is_plain_indexable_filter(&self, filter: Option<&Expr>) -> bool {
        filter.is_some_and(|expr| self.expr_is_plain_indexable(expr))
    }

    fn expr_is_plain_indexable(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Compare { .. } => true,
            Expr::IsNull { negated, .. } => !negated,
            Expr::Not(inner) => self.expr_is_plain_indexable(inner),
            Expr::And(left, right) | Expr::Or(left, right) => {
                self.expr_is_plain_indexable(left) && self.expr_is_plain_indexable(right)
            }
            Expr::Between { negated, .. } => !negated,
            Expr::Like {
                pattern, negated, ..
            } => !negated && Self::prefix_like_bounds(pattern).is_some(),
            Expr::CompareColumns { .. }
            | Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. } => false,
        }
    }

    fn find_matching_index_scan(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<IndexScanSpec> {
        let (index, key_prefix, range, mode) = self.find_matching_index(context, table, filter)?;
        Some(IndexScanSpec {
            index: index.name.clone(),
            mode,
            key_prefix,
            range,
        })
    }

    fn find_index_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let mut branches = Vec::new();
        self.collect_or_branches(filter, &mut branches);
        if branches.len() < 2 {
            return None;
        }

        branches
            .into_iter()
            .map(|branch| self.find_matching_index_scan(context, table, branch))
            .collect()
    }

    fn collect_or_branches<'a>(&self, expr: &'a Expr, branches: &mut Vec<&'a Expr>) {
        match expr {
            Expr::Or(left, right) => {
                self.collect_or_branches(left, branches);
                self.collect_or_branches(right, branches);
            }
            _ => branches.push(expr),
        }
    }

    fn find_matching_index<'a>(
        &self,
        context: &'a PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<(&'a IndexMeta, Vec<Value>, Option<IndexRange>, IndexScanMode)> {
        let predicate_summary = self.extract_conjunctive_terms(filter)?;
        context
            .indexes_for(table)
            .iter()
            .filter_map(|index| {
                let key_prefix = index
                    .columns
                    .iter()
                    .map_while(|column| predicate_summary.equality_terms.get(column).cloned())
                    .collect::<Vec<_>>();
                let range = index.columns.get(key_prefix.len()).and_then(|column| {
                    let bounds = predicate_summary.range_terms.get(column)?;
                    (bounds.lower.is_some() || bounds.upper.is_some()).then(|| IndexRange {
                        column: column.clone(),
                        lower: bounds.lower.as_ref().map(|(op, value)| IndexBound {
                            op: *op,
                            value: value.clone(),
                        }),
                        upper: bounds.upper.as_ref().map(|(op, value)| IndexBound {
                            op: *op,
                            value: value.clone(),
                        }),
                    })
                });
                let mode = match &range {
                    Some(_) => IndexScanMode::Range,
                    None if key_prefix.len() == index.columns.len() => IndexScanMode::Lookup,
                    None => IndexScanMode::Prefix,
                };
                (!key_prefix.is_empty() || range.is_some())
                    .then_some((index, key_prefix, range, mode))
            })
            .max_by_key(|(index, key_prefix, range, _)| {
                (
                    key_prefix.len(),
                    range.is_some(),
                    index.unique,
                    Reverse(index.columns.len()),
                )
            })
    }

    fn extract_conjunctive_terms(&self, expr: &Expr) -> Option<PredicateSummary> {
        let mut summary = PredicateSummary::default();
        self.collect_conjunctive_terms(expr, &mut summary)
            .then_some(summary)
    }

    fn collect_conjunctive_terms(&self, expr: &Expr, summary: &mut PredicateSummary) -> bool {
        match expr {
            Expr::Compare {
                column,
                op: CompareOp::Eq,
                value,
            } => {
                summary
                    .equality_terms
                    .entry(column.clone())
                    .or_insert_with(|| value.clone());
                true
            }
            Expr::Compare { column, op, value } => {
                let entry = summary.range_terms.entry(column.clone()).or_default();
                match op {
                    CompareOp::Gt | CompareOp::Gte => self.tighten_lower_bound(entry, *op, value),
                    CompareOp::Lt | CompareOp::Lte => self.tighten_upper_bound(entry, *op, value),
                    CompareOp::Ne => {}
                    CompareOp::Eq => unreachable!("equality branch handled above"),
                }
                true
            }
            Expr::Between {
                column,
                low,
                high,
                negated: false,
            } => {
                let entry = summary.range_terms.entry(column.clone()).or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, low);
                self.tighten_upper_bound(entry, CompareOp::Lte, high);
                true
            }
            Expr::Like {
                column,
                pattern,
                negated: false,
            } => {
                let Some((lower, upper)) = Self::prefix_like_bounds(pattern) else {
                    return false;
                };
                let entry = summary.range_terms.entry(column.clone()).or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, &Value::Text(lower));
                self.tighten_upper_bound(entry, CompareOp::Lt, &Value::Text(upper));
                true
            }
            Expr::CompareColumns { .. }
            | Expr::InSubquery { .. }
            | Expr::CompareSubquery { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => false,
            Expr::IsNull {
                column,
                negated: false,
            } => {
                summary
                    .equality_terms
                    .entry(column.clone())
                    .or_insert(Value::Null);
                true
            }
            Expr::IsNull { .. } => false,
            Expr::Not(_) => false,
            Expr::Or(_, _) => false,
            Expr::And(left, right) => {
                self.collect_conjunctive_terms(left, summary)
                    && self.collect_conjunctive_terms(right, summary)
            }
        }
    }

    fn tighten_lower_bound(&self, entry: &mut RangeBounds, op: CompareOp, value: &Value) {
        match entry.lower.as_ref() {
            None => entry.lower = Some((op, value.clone())),
            Some((current_op, current)) => match self.compare_values(current, value) {
                Some(Ordering::Less) => entry.lower = Some((op, value.clone())),
                Some(Ordering::Equal)
                    if Self::lower_bound_strictness(op)
                        > Self::lower_bound_strictness(*current_op) =>
                {
                    entry.lower = Some((op, value.clone()));
                }
                _ => {}
            },
        }
    }

    fn tighten_upper_bound(&self, entry: &mut RangeBounds, op: CompareOp, value: &Value) {
        match entry.upper.as_ref() {
            None => entry.upper = Some((op, value.clone())),
            Some((current_op, current)) => match self.compare_values(current, value) {
                Some(Ordering::Greater) => entry.upper = Some((op, value.clone())),
                Some(Ordering::Equal)
                    if Self::upper_bound_strictness(op)
                        > Self::upper_bound_strictness(*current_op) =>
                {
                    entry.upper = Some((op, value.clone()));
                }
                _ => {}
            },
        }
    }

    fn lower_bound_strictness(op: CompareOp) -> u8 {
        match op {
            CompareOp::Gt => 2,
            CompareOp::Gte => 1,
            _ => 0,
        }
    }

    fn upper_bound_strictness(op: CompareOp) -> u8 {
        match op {
            CompareOp::Lt => 2,
            CompareOp::Lte => 1,
            _ => 0,
        }
    }

    fn compare_values(&self, left: &Value, right: &Value) -> Option<Ordering> {
        match (left, right) {
            (Value::Null, Value::Null) => Some(Ordering::Equal),
            (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
            (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }

    fn prefix_like_bounds(pattern: &str) -> Option<(String, String)> {
        let prefix = pattern.strip_suffix('%')?;
        if prefix.is_empty() || prefix.contains('%') || prefix.contains('_') || !prefix.is_ascii() {
            return None;
        }

        let mut upper = prefix.as_bytes().to_vec();
        let last = upper.last_mut()?;
        if *last == u8::MAX {
            return None;
        }
        *last += 1;

        String::from_utf8(upper)
            .ok()
            .map(|upper| (prefix.to_string(), upper))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Optimizer;

impl Optimizer {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub fn optimize(&self, plan: Plan) -> Result<Plan> {
        self.optimize_with_context(plan, &PlanningContext::default())
    }

    pub fn optimize_with_context(&self, mut plan: Plan, context: &PlanningContext) -> Result<Plan> {
        for pass in self.passes() {
            plan = pass.optimize(plan, context)?;
        }
        Ok(plan)
    }

    #[must_use]
    pub fn pass_names(&self) -> Vec<&'static str> {
        self.passes().iter().map(|pass| pass.name()).collect()
    }

    fn passes(&self) -> Vec<Box<dyn OptimizerPass>> {
        vec![Box::new(IndexSelectionPass)]
    }
}

#[derive(Debug, Default)]
struct PredicateSummary {
    equality_terms: HashMap<String, Value>,
    range_terms: HashMap<String, RangeBounds>,
}

#[derive(Debug, Default)]
struct RangeBounds {
    lower: Option<(CompareOp, Value)>,
    upper: Option<(CompareOp, Value)>,
}
