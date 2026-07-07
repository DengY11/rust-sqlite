use std::cmp::{Ordering, Reverse};
use std::collections::HashMap;

use crate::common::error::Result;
use crate::common::types::{CheckExpr, CheckOp, IndexMeta, Value};
use crate::sql::ast::{AggregateArg, AggregateFunc, CompareOp, Expr, ScalarExpr, ScalarFunc};
use crate::sql::parser::{parse_check_constraint_expression, parse_scalar_sql_expression};
use crate::sql::plan::{IndexBound, IndexRange, IndexScanMode, IndexScanSpec, Plan};
use crate::sql::planner::PlanningContext;
use crate::storage::sqlite3::index_expr::evaluate_constant_expr;

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
                offset,
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
                            offset,
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
                            offset,
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
                    offset,
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
                offset,
            } => Ok(Plan::Aggregate {
                source: Box::new(self.optimize(*source, context)?),
                columns,
                group_by,
                having,
                order_by,
                limit,
                offset,
            }),
            Plan::Union {
                left,
                right,
                operator,
                all,
                order_by,
                limit,
                offset,
            } => Ok(Plan::Union {
                left: Box::new(self.optimize(*left, context)?),
                right: Box::new(self.optimize(*right, context)?),
                operator,
                all,
                order_by,
                limit,
                offset,
            }),
            Plan::ExplainQueryPlan { plan } => Ok(Plan::ExplainQueryPlan {
                plan: Box::new(self.optimize(*plan, context)?),
            }),
            Plan::NoOp => Ok(Plan::NoOp),
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
            Expr::CompareScalar { left, op, right } => {
                matches!(
                    op,
                    CompareOp::Eq | CompareOp::Gt | CompareOp::Gte | CompareOp::Lt | CompareOp::Lte
                ) && ((Self::scalar_expr_constant_value(left).is_some()
                    && !matches!(right, ScalarExpr::Literal(_)))
                    || (Self::scalar_expr_constant_value(right).is_some()
                        && !matches!(left, ScalarExpr::Literal(_))))
            }
            Expr::IsNull { negated, .. } => !negated,
            Expr::IsNullScalar { negated, .. } => !negated,
            Expr::LikeScalar {
                pattern,
                escape: None,
                negated,
                ..
            } => !negated && Self::prefix_like_bounds(pattern).is_some(),
            Expr::LikeScalar { .. } => false,
            Expr::GlobScalar {
                pattern, negated, ..
            } => !negated && Self::prefix_glob_bounds(pattern).is_some(),
            Expr::BetweenScalar { negated, .. } => !negated,
            Expr::InListScalar {
                expr,
                values,
                negated,
            } => {
                !negated
                    && !matches!(expr, ScalarExpr::Literal(_))
                    && values
                        .iter()
                        .all(|value| Self::scalar_expr_constant_value(value).is_some())
            }
            Expr::Is { .. } => false,
            Expr::IsBool { .. } => false,
            Expr::Not(inner) => self.expr_is_plain_indexable(inner),
            Expr::And(left, right) => {
                self.expr_is_plain_indexable(left) || self.expr_is_plain_indexable(right)
            }
            Expr::Or(left, right) => {
                self.expr_is_plain_indexable(left) && self.expr_is_plain_indexable(right)
            }
            Expr::Between { negated, .. } => !negated,
            Expr::Glob {
                pattern, negated, ..
            } => !negated && Self::prefix_glob_bounds(pattern).is_some(),
            Expr::InList { negated, .. } => !negated,
            Expr::Like { .. } => false,
            Expr::CompareColumns { .. }
            | Expr::InSubquery { .. }
            | Expr::InSubqueryScalar { .. }
            | Expr::CompareSubquery { .. }
            | Expr::CompareSubqueryScalar { .. }
            | Expr::ExistsSubquery { .. } => false,
        }
    }

    fn find_matching_index_scan(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<IndexScanSpec> {
        if let Some(scan) = self.find_single_value_in_list_scan(context, table, filter) {
            return Some(scan);
        }
        if let Some(scan) = self.find_single_value_in_list_scalar_scan(context, table, filter) {
            return Some(scan);
        }

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
        if let Some(scans) = self.find_in_list_union_scans(context, table, filter) {
            return Some(scans);
        }
        if let Some(scans) = self.find_in_list_scalar_union_scans(context, table, filter) {
            return Some(scans);
        }
        if let Some(scans) = self.find_conjunctive_in_list_union_scans(context, table, filter) {
            return Some(scans);
        }
        if let Some(scans) = self.find_conjunctive_or_union_scans(context, table, filter) {
            return Some(scans);
        }

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

    fn find_conjunctive_or_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let branches = self.expand_conjunctive_or_branches(filter)?;
        let scans = branches
            .iter()
            .map(|branch| self.find_matching_index_scan(context, table, branch))
            .collect::<Option<Vec<_>>>()?;
        (scans.len() >= 2).then_some(scans)
    }

    fn find_conjunctive_in_list_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let branches = self.expand_conjunctive_in_list_branches(filter)?;
        let scans = branches
            .iter()
            .map(|branch| self.find_matching_index_scan(context, table, branch))
            .collect::<Option<Vec<_>>>()?;
        (scans.len() >= 2).then_some(scans)
    }

    fn find_in_list_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let Expr::InList {
            column,
            values,
            negated: false,
        } = filter
        else {
            return None;
        };

        let scans = values
            .iter()
            .map(|value| {
                self.find_matching_index_scan(
                    context,
                    table,
                    &Expr::Compare {
                        column: column.clone(),
                        op: CompareOp::Eq,
                        value: value.clone(),
                    },
                )
            })
            .collect::<Option<Vec<_>>>()?;

        (scans.len() >= 2).then_some(scans)
    }

    fn find_in_list_scalar_union_scans(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<Vec<IndexScanSpec>> {
        let Expr::InListScalar {
            expr,
            values,
            negated: false,
        } = filter
        else {
            return None;
        };

        let scans = values
            .iter()
            .map(|value| {
                let literal = Self::scalar_expr_constant_value(value)?;
                self.find_matching_index_scan(
                    context,
                    table,
                    &Expr::CompareScalar {
                        left: expr.clone(),
                        op: CompareOp::Eq,
                        right: Self::value_to_literal_expr(literal),
                    },
                )
            })
            .collect::<Option<Vec<_>>>()?;

        (scans.len() >= 2).then_some(scans)
    }

    fn expand_conjunctive_in_list_branches(&self, filter: &Expr) -> Option<Vec<Expr>> {
        const MAX_CONJUNCTIVE_IN_LIST_BRANCHES: usize = 16;

        let mut terms = Vec::new();
        Self::collect_and_terms(filter, &mut terms);
        if terms.len() < 2 {
            return None;
        }

        let mut expandable_terms = Vec::new();

        for (index, term) in terms.iter().enumerate() {
            let expanded = match term {
                Expr::InList {
                    column,
                    values,
                    negated: false,
                } if values.len() >= 2 => Some(
                    values
                        .iter()
                        .cloned()
                        .map(|value| Expr::Compare {
                            column: column.clone(),
                            op: CompareOp::Eq,
                            value,
                        })
                        .collect::<Vec<_>>(),
                ),
                Expr::InListScalar {
                    expr,
                    values,
                    negated: false,
                } if values.len() >= 2 => {
                    let expanded = values
                        .iter()
                        .map(|value| {
                            let literal = Self::scalar_expr_constant_value(value)?;
                            Some(Expr::CompareScalar {
                                left: expr.clone(),
                                op: CompareOp::Eq,
                                right: Self::value_to_literal_expr(literal),
                            })
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(expanded)
                }
                _ => None,
            };

            if let Some(expanded) = expanded {
                expandable_terms.push((index, expanded));
            }
        }

        if expandable_terms.is_empty() {
            return None;
        }

        let branch_count = expandable_terms
            .iter()
            .try_fold(1_usize, |acc, (_, expanded)| {
                acc.checked_mul(expanded.len())
                    .filter(|count| *count <= MAX_CONJUNCTIVE_IN_LIST_BRANCHES)
            })?;

        if branch_count < 2 {
            return None;
        }

        let mut branch_terms: Vec<Vec<Expr>> =
            vec![terms.iter().map(|term| (*term).clone()).collect()];
        for (index, expanded) in expandable_terms {
            let mut next = Vec::with_capacity(branch_terms.len() * expanded.len());
            for existing in &branch_terms {
                for replacement in &expanded {
                    let mut branch = existing.clone();
                    branch[index] = replacement.clone();
                    next.push(branch);
                }
            }
            branch_terms = next;
        }

        Some(
            branch_terms
                .into_iter()
                .map(Self::rebuild_and_expr)
                .collect(),
        )
    }

    fn expand_conjunctive_or_branches(&self, filter: &Expr) -> Option<Vec<Expr>> {
        let mut terms = Vec::new();
        Self::collect_and_terms(filter, &mut terms);
        if terms.len() < 2 {
            return None;
        }

        let mut selected_index = None;
        let mut selected_branches = Vec::new();
        for (index, term) in terms.iter().enumerate() {
            let mut or_branches = Vec::new();
            self.collect_or_branches(term, &mut or_branches);
            if or_branches.len() < 2 {
                continue;
            }
            if selected_index.is_some() {
                return None;
            }
            selected_index = Some(index);
            selected_branches = or_branches.into_iter().map(Clone::clone).collect();
        }

        let selected_index = selected_index?;
        Some(
            selected_branches
                .into_iter()
                .map(|branch| {
                    let mut branch_terms = Vec::with_capacity(terms.len());
                    for (index, term) in terms.iter().enumerate() {
                        if index == selected_index {
                            branch_terms.push(branch.clone());
                        } else {
                            branch_terms.push((*term).clone());
                        }
                    }
                    Self::rebuild_and_expr(branch_terms)
                })
                .collect(),
        )
    }

    fn find_single_value_in_list_scan(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<IndexScanSpec> {
        let Expr::InList {
            column,
            values,
            negated: false,
        } = filter
        else {
            return None;
        };

        if values.len() != 1 {
            return None;
        }

        let compare = Expr::Compare {
            column: column.clone(),
            op: CompareOp::Eq,
            value: values.first()?.clone(),
        };
        let (index, key_prefix, range, mode) =
            self.find_matching_index(context, table, &compare)?;
        Some(IndexScanSpec {
            index: index.name.clone(),
            mode,
            key_prefix,
            range,
        })
    }

    fn find_single_value_in_list_scalar_scan(
        &self,
        context: &PlanningContext,
        table: &str,
        filter: &Expr,
    ) -> Option<IndexScanSpec> {
        let Expr::InListScalar {
            expr,
            values,
            negated: false,
        } = filter
        else {
            return None;
        };

        if values.len() != 1 {
            return None;
        }

        let literal = Self::scalar_expr_constant_value(values.first()?)?;
        let compare = Expr::CompareScalar {
            left: expr.clone(),
            op: CompareOp::Eq,
            right: Self::value_to_literal_expr(literal),
        };
        let (index, key_prefix, range, mode) =
            self.find_matching_index(context, table, &compare)?;
        Some(IndexScanSpec {
            index: index.name.clone(),
            mode,
            key_prefix,
            range,
        })
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

    fn collect_and_terms<'a>(expr: &'a Expr, terms: &mut Vec<&'a Expr>) {
        match expr {
            Expr::And(left, right) => {
                Self::collect_and_terms(left, terms);
                Self::collect_and_terms(right, terms);
            }
            _ => terms.push(expr),
        }
    }

    fn rebuild_and_expr(mut terms: Vec<Expr>) -> Expr {
        let first = terms.remove(0);
        terms.into_iter().fold(first, |left, right| {
            Expr::And(Box::new(left), Box::new(right))
        })
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
            .filter(|index| {
                index.predicate.as_deref().is_none_or(|predicate| {
                    self.partial_index_predicate_matches(predicate, &predicate_summary)
                })
            })
            .filter_map(|index| {
                if self.index_uses_nocase_column(context, table, index) {
                    return None;
                }
                let key_prefix = index
                    .columns
                    .iter()
                    .map_while(|column| {
                        let canonical = Self::canonical_index_term(column)?;
                        predicate_summary.equality_terms.get(&canonical).cloned()
                    })
                    .collect::<Vec<_>>();
                let range = index.columns.get(key_prefix.len()).and_then(|column| {
                    let canonical = Self::canonical_index_term(column)?;
                    let bounds = predicate_summary.range_terms.get(&canonical)?;
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

    fn index_uses_nocase_column(
        &self,
        context: &PlanningContext,
        table: &str,
        index: &IndexMeta,
    ) -> bool {
        let Some(schema) = context.schema(table) else {
            return false;
        };
        index.columns.iter().any(|term| {
            schema
                .columns
                .iter()
                .find(|column| column.name == *term)
                .and_then(|column| column.collation.as_deref())
                .is_some_and(|collation| collation.eq_ignore_ascii_case("NOCASE"))
        })
    }

    fn partial_index_predicate_matches(&self, predicate: &str, summary: &PredicateSummary) -> bool {
        let Ok(expr) = parse_check_constraint_expression(predicate) else {
            return false;
        };

        self.partial_check_expr_matches(&expr, summary)
    }

    fn partial_check_expr_matches(&self, expr: &CheckExpr, summary: &PredicateSummary) -> bool {
        match expr {
            CheckExpr::Compare {
                column,
                op: CheckOp::Eq,
                value,
            } => summary
                .equality_terms
                .get(column)
                .is_some_and(|actual| actual == value),
            CheckExpr::Compare { .. } => false,
            CheckExpr::IsNull {
                column,
                negated: false,
            } => summary
                .equality_terms
                .get(column)
                .is_some_and(|actual| matches!(actual, Value::Null)),
            CheckExpr::IsNull {
                column,
                negated: true,
            } => summary.non_null_terms.contains(column),
            CheckExpr::Glob { .. }
            | CheckExpr::Like { .. }
            | CheckExpr::InList { .. }
            | CheckExpr::Between { .. }
            | CheckExpr::IsBool { .. }
            | CheckExpr::Truthy { .. }
            | CheckExpr::IsDistinct { .. } => false,
            CheckExpr::And(left, right) => {
                self.partial_check_expr_matches(left, summary)
                    && self.partial_check_expr_matches(right, summary)
            }
            CheckExpr::Or(_, _) | CheckExpr::Not(_) => false,
        }
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
            Expr::CompareScalar { left, op, right } => {
                if let Some(value) = Self::scalar_expr_constant_value(right) {
                    let key = Self::scalar_expr_key(left);
                    match op {
                        CompareOp::Eq => {
                            summary
                                .equality_terms
                                .entry(key)
                                .or_insert_with(|| value.clone());
                            return true;
                        }
                        CompareOp::Gt | CompareOp::Gte => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_lower_bound(entry, *op, &value);
                            return true;
                        }
                        CompareOp::Lt | CompareOp::Lte => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_upper_bound(entry, *op, &value);
                            return true;
                        }
                        CompareOp::Ne => return false,
                    }
                }
                if let Some(value) = Self::scalar_expr_constant_value(left) {
                    let key = Self::scalar_expr_key(right);
                    match op {
                        CompareOp::Eq => {
                            summary
                                .equality_terms
                                .entry(key)
                                .or_insert_with(|| value.clone());
                            return true;
                        }
                        CompareOp::Gt => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_upper_bound(entry, CompareOp::Lt, &value);
                            return true;
                        }
                        CompareOp::Gte => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_upper_bound(entry, CompareOp::Lte, &value);
                            return true;
                        }
                        CompareOp::Lt => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_lower_bound(entry, CompareOp::Gt, &value);
                            return true;
                        }
                        CompareOp::Lte => {
                            let entry = summary.range_terms.entry(key).or_default();
                            self.tighten_lower_bound(entry, CompareOp::Gte, &value);
                            return true;
                        }
                        CompareOp::Ne => return false,
                    }
                }
                false
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
            Expr::LikeScalar {
                expr,
                pattern,
                escape: None,
                negated: false,
            } => {
                let Some((lower, upper)) = Self::prefix_like_bounds(pattern) else {
                    return false;
                };
                let entry = summary
                    .range_terms
                    .entry(Self::scalar_expr_key(expr))
                    .or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, &Value::Text(lower));
                self.tighten_upper_bound(entry, CompareOp::Lt, &Value::Text(upper));
                true
            }
            Expr::Like { negated: false, .. } => false,
            Expr::LikeScalar { negated: false, .. } => false,
            Expr::Glob {
                column,
                pattern,
                negated: false,
            } => {
                let Some((lower, upper)) = Self::prefix_glob_bounds(pattern) else {
                    return false;
                };
                let entry = summary.range_terms.entry(column.clone()).or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, &Value::Text(lower));
                self.tighten_upper_bound(entry, CompareOp::Lt, &Value::Text(upper));
                true
            }
            Expr::GlobScalar {
                expr,
                pattern,
                negated: false,
            } => {
                let Some((lower, upper)) = Self::prefix_glob_bounds(pattern) else {
                    return false;
                };
                let entry = summary
                    .range_terms
                    .entry(Self::scalar_expr_key(expr))
                    .or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, &Value::Text(lower));
                self.tighten_upper_bound(entry, CompareOp::Lt, &Value::Text(upper));
                true
            }
            Expr::BetweenScalar {
                expr,
                low,
                high,
                negated: false,
            } => {
                let Some(low_value) = Self::scalar_expr_constant_value(low) else {
                    return false;
                };
                let Some(high_value) = Self::scalar_expr_constant_value(high) else {
                    return false;
                };
                let entry = summary
                    .range_terms
                    .entry(Self::scalar_expr_key(expr))
                    .or_default();
                self.tighten_lower_bound(entry, CompareOp::Gte, &low_value);
                self.tighten_upper_bound(entry, CompareOp::Lte, &high_value);
                true
            }
            Expr::BetweenScalar { .. } => false,
            Expr::LikeScalar { .. } => false,
            Expr::GlobScalar { .. } => false,
            Expr::Glob { .. } => false,
            Expr::IsNullScalar {
                expr,
                negated: false,
            } => {
                summary
                    .equality_terms
                    .entry(Self::scalar_expr_key(expr))
                    .or_insert(Value::Null);
                true
            }
            Expr::IsNullScalar { .. } => false,
            Expr::Is { .. } => true,
            Expr::CompareColumns { .. }
            | Expr::InList { .. }
            | Expr::InSubquery { .. }
            | Expr::InListScalar { .. }
            | Expr::InSubqueryScalar { .. }
            | Expr::CompareSubquery { .. }
            | Expr::CompareSubqueryScalar { .. }
            | Expr::ExistsSubquery { .. }
            | Expr::Like { .. }
            | Expr::Between { .. } => true,
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
            Expr::IsNull {
                column,
                negated: true,
            } => {
                summary.non_null_terms.insert(column.clone());
                true
            }
            Expr::IsBool { .. } => true,
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
            (Value::Blob(left), Value::Blob(right)) => Some(left.cmp(right)),
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }

    fn prefix_glob_bounds(pattern: &str) -> Option<(String, String)> {
        let prefix = pattern.strip_suffix('*')?;
        if prefix.is_empty()
            || prefix.contains('*')
            || prefix.contains('?')
            || prefix.contains('[')
            || !prefix.is_ascii()
        {
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

    fn scalar_expr_constant_value(expr: &ScalarExpr) -> Option<Value> {
        match expr {
            ScalarExpr::Literal(value) => Some(value.clone()),
            _ => evaluate_constant_expr(expr),
        }
    }

    fn value_to_literal_expr(value: Value) -> ScalarExpr {
        ScalarExpr::Literal(value)
    }

    fn canonical_index_term(term: &str) -> Option<String> {
        parse_scalar_sql_expression(term)
            .ok()
            .map(|expr| Self::scalar_expr_key(&expr))
            .or_else(|| (!term.is_empty()).then(|| term.to_string()))
    }

    fn scalar_expr_key(expr: &ScalarExpr) -> String {
        match expr {
            ScalarExpr::Literal(value) => value.to_string(),
            ScalarExpr::Column(name) => name.clone(),
            ScalarExpr::UnaryMinus(expr) => format!("-{}", Self::scalar_expr_key(expr)),
            ScalarExpr::BitNot(expr) => format!("~{}", Self::scalar_expr_key(expr)),
            ScalarExpr::Not(expr) => format!("NOT {}", Self::scalar_expr_key(expr)),
            ScalarExpr::Collate { expr, collation } => {
                format!("{} COLLATE {}", Self::scalar_expr_key(expr), collation)
            }
            ScalarExpr::Cast { expr, ty } => {
                format!("CAST({} AS {})", Self::scalar_expr_key(expr), ty.name())
            }
            ScalarExpr::Is {
                left,
                right,
                negated,
            } => format!(
                "{} IS {}{}",
                Self::scalar_expr_key(left),
                if *negated { "NOT " } else { "" },
                Self::scalar_expr_key(right)
            ),
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => format!(
                "{} IS {}{}",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" },
                if *value { "TRUE" } else { "FALSE" }
            ),
            ScalarExpr::InList {
                expr,
                values,
                negated,
            } => format!(
                "{} {}IN ({})",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" },
                values
                    .iter()
                    .map(Self::scalar_expr_key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::InSubquery {
                expr,
                query: _,
                negated,
            } => format!(
                "{} {}IN (SELECT ...)",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" }
            ),
            ScalarExpr::Subquery { .. } => "(SELECT ...)".to_string(),
            ScalarExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => format!(
                "{} {}LIKE '{}'{}",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" },
                pattern,
                escape
                    .as_ref()
                    .map(|escape| format!(" ESCAPE '{}'", escape.replace('\'', "''")))
                    .unwrap_or_default()
            ),
            ScalarExpr::Glob {
                expr,
                pattern,
                negated,
            } => format!(
                "{} {}GLOB '{}'",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" },
                pattern
            ),
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => format!(
                "{} {}BETWEEN {} AND {}",
                Self::scalar_expr_key(expr),
                if *negated { "NOT " } else { "" },
                Self::scalar_expr_key(low),
                Self::scalar_expr_key(high)
            ),
            ScalarExpr::Compare { left, op, right } => format!(
                "{} {} {}",
                Self::scalar_expr_key(left),
                match op {
                    CompareOp::Eq => "=",
                    CompareOp::Ne => "!=",
                    CompareOp::Gt => ">",
                    CompareOp::Gte => ">=",
                    CompareOp::Lt => "<",
                    CompareOp::Lte => "<=",
                },
                Self::scalar_expr_key(right)
            ),
            ScalarExpr::CompareSubquery { left, op, query: _ } => format!(
                "{} {} (SELECT ...)",
                Self::scalar_expr_key(left),
                match op {
                    CompareOp::Eq => "=",
                    CompareOp::Ne => "!=",
                    CompareOp::Gt => ">",
                    CompareOp::Gte => ">=",
                    CompareOp::Lt => "<",
                    CompareOp::Lte => "<=",
                }
            ),
            ScalarExpr::Case {
                base,
                when_then_clauses,
                else_expr,
            } => {
                let mut parts = vec!["CASE".to_string()];
                if let Some(base) = base {
                    parts.push(Self::scalar_expr_key(base));
                }
                for (when_expr, then_expr) in when_then_clauses {
                    parts.push(format!(
                        "WHEN {} THEN {}",
                        Self::scalar_expr_key(when_expr),
                        Self::scalar_expr_key(then_expr)
                    ));
                }
                if let Some(else_expr) = else_expr {
                    parts.push(format!("ELSE {}", Self::scalar_expr_key(else_expr)));
                }
                parts.push("END".to_string());
                parts.join(" ")
            }
            ScalarExpr::Binary { left, op, right } => format!(
                "{} {} {}",
                Self::scalar_expr_key(left),
                match op {
                    crate::sql::ast::ScalarBinaryOp::Add => "+",
                    crate::sql::ast::ScalarBinaryOp::Subtract => "-",
                    crate::sql::ast::ScalarBinaryOp::Multiply => "*",
                    crate::sql::ast::ScalarBinaryOp::Divide => "/",
                    crate::sql::ast::ScalarBinaryOp::Modulo => "%",
                    crate::sql::ast::ScalarBinaryOp::BitAnd => "&",
                    crate::sql::ast::ScalarBinaryOp::BitOr => "|",
                    crate::sql::ast::ScalarBinaryOp::ShiftLeft => "<<",
                    crate::sql::ast::ScalarBinaryOp::ShiftRight => ">>",
                    crate::sql::ast::ScalarBinaryOp::Concat => "||",
                    crate::sql::ast::ScalarBinaryOp::JsonExtract => "->",
                    crate::sql::ast::ScalarBinaryOp::JsonExtractText => "->>",
                },
                Self::scalar_expr_key(right)
            ),
            ScalarExpr::Function { func, args } => format!(
                "{}({})",
                match func {
                    ScalarFunc::Length => "LENGTH",
                    ScalarFunc::OctetLength => "OCTET_LENGTH",
                    ScalarFunc::MinScalar => "MIN",
                    ScalarFunc::MaxScalar => "MAX",
                    ScalarFunc::Date => "DATE",
                    ScalarFunc::Time => "TIME",
                    ScalarFunc::DateTime => "DATETIME",
                    ScalarFunc::TimeDiff => "TIMEDIFF",
                    ScalarFunc::Strftime => "STRFTIME",
                    ScalarFunc::JulianDay => "JULIANDAY",
                    ScalarFunc::UnixEpoch => "UNIXEPOCH",
                    ScalarFunc::Changes => "CHANGES",
                    ScalarFunc::TotalChanges => "TOTAL_CHANGES",
                    ScalarFunc::Printf => "PRINTF",
                    ScalarFunc::IIf => "IIF",
                    ScalarFunc::If => "IF",
                    ScalarFunc::Concat => "CONCAT",
                    ScalarFunc::ConcatWs => "CONCAT_WS",
                    ScalarFunc::SqliteSourceId => "SQLITE_SOURCE_ID",
                    ScalarFunc::Sign => "SIGN",
                    ScalarFunc::RandomBlob => "RANDOMBLOB",
                    ScalarFunc::Random => "RANDOM",
                    ScalarFunc::Unhex => "UNHEX",
                    ScalarFunc::Unistr => "UNISTR",
                    ScalarFunc::UnistrQuote => "UNISTR_QUOTE",
                    ScalarFunc::SqliteVersion => "SQLITE_VERSION",
                    ScalarFunc::SqliteCompileOptionUsed => "SQLITE_COMPILEOPTION_USED",
                    ScalarFunc::SqliteCompileOptionGet => "SQLITE_COMPILEOPTION_GET",
                    ScalarFunc::Likely => "LIKELY",
                    ScalarFunc::Unlikely => "UNLIKELY",
                    ScalarFunc::Likelihood => "LIKELIHOOD",
                    ScalarFunc::Mod => "MOD",
                    ScalarFunc::Ceil => "CEIL",
                    ScalarFunc::Ceiling => "CEILING",
                    ScalarFunc::Floor => "FLOOR",
                    ScalarFunc::Trunc => "TRUNC",
                    ScalarFunc::Pi => "PI",
                    ScalarFunc::Sqrt => "SQRT",
                    ScalarFunc::Power => "POWER",
                    ScalarFunc::Exp => "EXP",
                    ScalarFunc::Sin => "SIN",
                    ScalarFunc::Cos => "COS",
                    ScalarFunc::Tan => "TAN",
                    ScalarFunc::Sinh => "SINH",
                    ScalarFunc::Cosh => "COSH",
                    ScalarFunc::Tanh => "TANH",
                    ScalarFunc::Acos => "ACOS",
                    ScalarFunc::Asin => "ASIN",
                    ScalarFunc::Atan => "ATAN",
                    ScalarFunc::Atan2 => "ATAN2",
                    ScalarFunc::Acosh => "ACOSH",
                    ScalarFunc::Asinh => "ASINH",
                    ScalarFunc::Atanh => "ATANH",
                    ScalarFunc::Ln => "LN",
                    ScalarFunc::Log10 => "LOG10",
                    ScalarFunc::Log2 => "LOG2",
                    ScalarFunc::Log => "LOG",
                    ScalarFunc::Degrees => "DEGREES",
                    ScalarFunc::Radians => "RADIANS",
                    ScalarFunc::Char => "CHAR",
                    ScalarFunc::ZeroBlob => "ZEROBLOB",
                    ScalarFunc::TypeOf => "TYPEOF",
                    ScalarFunc::Subtype => "SUBTYPE",
                    ScalarFunc::Hex => "HEX",
                    ScalarFunc::Substr => "SUBSTR",
                    ScalarFunc::Instr => "INSTR",
                    ScalarFunc::Replace => "REPLACE",
                    ScalarFunc::LikeFunc => "LIKE",
                    ScalarFunc::GlobFunc => "GLOB",
                    ScalarFunc::Quote => "QUOTE",
                    ScalarFunc::Unicode => "UNICODE",
                    ScalarFunc::Trim => "TRIM",
                    ScalarFunc::LTrim => "LTRIM",
                    ScalarFunc::RTrim => "RTRIM",
                    ScalarFunc::Lower => "LOWER",
                    ScalarFunc::Upper => "UPPER",
                    ScalarFunc::Abs => "ABS",
                    ScalarFunc::Round => "ROUND",
                    ScalarFunc::LastInsertRowId => "LAST_INSERT_ROWID",
                    ScalarFunc::Coalesce => "COALESCE",
                    ScalarFunc::IfNull => "IFNULL",
                    ScalarFunc::NullIf => "NULLIF",
                    ScalarFunc::Json => "JSON",
                    ScalarFunc::JsonValid => "JSON_VALID",
                    ScalarFunc::JsonErrorPosition => "JSON_ERROR_POSITION",
                    ScalarFunc::JsonPretty => "JSON_PRETTY",
                    ScalarFunc::JsonQuote => "JSON_QUOTE",
                    ScalarFunc::JsonExtract => "JSON_EXTRACT",
                    ScalarFunc::JsonType => "JSON_TYPE",
                    ScalarFunc::JsonArray => "JSON_ARRAY",
                    ScalarFunc::JsonObject => "JSON_OBJECT",
                    ScalarFunc::JsonArrayLength => "JSON_ARRAY_LENGTH",
                    ScalarFunc::JsonRemove => "JSON_REMOVE",
                    ScalarFunc::JsonSet => "JSON_SET",
                    ScalarFunc::JsonInsert => "JSON_INSERT",
                    ScalarFunc::JsonReplace => "JSON_REPLACE",
                    ScalarFunc::JsonPatch => "JSON_PATCH",
                },
                args.iter()
                    .map(Self::scalar_expr_key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ScalarExpr::Aggregate { func, arg, .. } => Self::aggregate_expr_key(*func, arg),
            ScalarExpr::Tuple(values) => format!(
                "({})",
                values
                    .iter()
                    .map(Self::scalar_expr_key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn aggregate_expr_key(func: AggregateFunc, arg: &AggregateArg) -> String {
        format!(
            "{}({})",
            match func {
                AggregateFunc::Count => "COUNT",
                AggregateFunc::Sum => "SUM",
                AggregateFunc::Avg => "AVG",
                AggregateFunc::Total => "TOTAL",
                AggregateFunc::Median => "MEDIAN",
                AggregateFunc::Percentile => "PERCENTILE",
                AggregateFunc::PercentileCont => "PERCENTILE_CONT",
                AggregateFunc::PercentileDisc => "PERCENTILE_DISC",
                AggregateFunc::GroupConcat => "GROUP_CONCAT",
                AggregateFunc::JsonGroupArray => "JSON_GROUP_ARRAY",
                AggregateFunc::JsonGroupObject => "JSON_GROUP_OBJECT",
                AggregateFunc::Min => "MIN",
                AggregateFunc::Max => "MAX",
            },
            match arg {
                AggregateArg::Wildcard => "*".to_string(),
                AggregateArg::Expr { expr, distinct, .. } => {
                    if *distinct {
                        format!("DISTINCT {}", Self::scalar_expr_key(expr))
                    } else {
                        Self::scalar_expr_key(expr)
                    }
                }
                AggregateArg::GroupConcat {
                    expr,
                    separator,
                    distinct,
                    ..
                } => {
                    let expr = if *distinct {
                        format!("DISTINCT {}", Self::scalar_expr_key(expr))
                    } else {
                        Self::scalar_expr_key(expr)
                    };
                    if let Some(separator) = separator {
                        format!("{expr}, {}", Self::scalar_expr_key(separator))
                    } else {
                        expr
                    }
                }
                AggregateArg::JsonGroupObject { key, value, .. } => {
                    format!(
                        "{}, {}",
                        Self::scalar_expr_key(key),
                        Self::scalar_expr_key(value)
                    )
                }
                AggregateArg::Percentile { expr, fraction, .. } => {
                    format!(
                        "{}, {}",
                        Self::scalar_expr_key(expr),
                        Self::scalar_expr_key(fraction)
                    )
                }
            }
        )
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
    non_null_terms: std::collections::HashSet<String>,
    range_terms: HashMap<String, RangeBounds>,
}

#[derive(Debug, Default)]
struct RangeBounds {
    lower: Option<(CompareOp, Value)>,
    upper: Option<(CompareOp, Value)>,
}
