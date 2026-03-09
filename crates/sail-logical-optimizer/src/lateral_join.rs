use std::sync::Arc;

use datafusion::optimizer::{ApplyOrder, OptimizerConfig, OptimizerRule};
use datafusion_common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion_common::Result;
use datafusion_expr::{Expr, JoinType, LogicalPlan, LogicalPlanBuilder};

#[derive(Debug, Clone, Default)]
pub struct DecorrelateLateralProjection;

impl DecorrelateLateralProjection {
    pub fn new() -> Self {
        Self
    }
}

impl OptimizerRule for DecorrelateLateralProjection {
    fn name(&self) -> &str {
        "decorrelate_lateral_projection"
    }

    fn apply_order(&self) -> Option<ApplyOrder> {
        Some(ApplyOrder::BottomUp)
    }

    fn rewrite(
        &self,
        plan: LogicalPlan,
        _config: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        let LogicalPlan::Join(join) = &plan else {
            return Ok(Transformed::no(plan));
        };

        if join.join_type != JoinType::Inner {
            return Ok(Transformed::no(plan));
        }

        let subquery_alias = match &*join.right {
            LogicalPlan::SubqueryAlias(sa) => Some(sa.alias.clone()),
            _ => None,
        };

        let right_plan = match &*join.right {
            LogicalPlan::SubqueryAlias(sa) => sa.input.as_ref(),
            other => other,
        };

        let LogicalPlan::Projection(proj) = right_plan else {
            return Ok(Transformed::no(plan));
        };

        if !proj.expr.iter().any(contains_outer_ref) {
            return Ok(Transformed::no(plan));
        }

        let left_field_count = join.left.schema().fields().len();

        let LogicalPlan::Join(join) = plan else {
            unreachable!();
        };

        let join_on = join.on;
        let join_filter = join.filter;

        let left: LogicalPlan = Arc::unwrap_or_clone(join.left);
        let right_inner = match Arc::unwrap_or_clone(join.right) {
            LogicalPlan::SubqueryAlias(sa) => Arc::unwrap_or_clone(sa.input),
            other => other,
        };

        let LogicalPlan::Projection(proj) = right_inner else {
            unreachable!();
        };
        let right_exprs = proj.expr;
        let right_base = Arc::unwrap_or_clone(proj.input);

        let cross_join = LogicalPlanBuilder::from(left)
            .cross_join(right_base)?
            .build()?;

        let mut output_exprs: Vec<Expr> = Vec::new();

        for col in cross_join
            .schema()
            .columns()
            .into_iter()
            .take(left_field_count)
        {
            output_exprs.push(Expr::Column(col));
        }

        for expr in right_exprs {
            let replaced = expr
                .transform(|e| match e {
                    Expr::OuterReferenceColumn(_field, col) => {
                        Ok(Transformed::yes(Expr::Column(col)))
                    }
                    _ => Ok(Transformed::no(e)),
                })
                .data()?;
            // If the original right side had a SubqueryAlias (e.g. `LATERAL (...) AS t`),
            // set the alias relation so the output field gets the correct qualifier.
            // This avoids wrapping the entire plan in SubqueryAlias which would
            // change the qualifier of left-side columns too.
            let replaced = match (&subquery_alias, replaced) {
                (Some(alias), Expr::Alias(mut a)) => {
                    a.relation = Some(alias.clone());
                    Expr::Alias(a)
                }
                (_, other) => other,
            };
            output_exprs.push(replaced);
        }

        let projected = LogicalPlanBuilder::from(cross_join)
            .project(output_exprs)?
            .build()?;

        let result = if join_on.is_empty() && join_filter.is_none() {
            projected
        } else {
            let mut conditions: Vec<Expr> = Vec::new();

            for (l, r) in join_on {
                conditions.push(l.eq(r));
            }

            if let Some(f) = join_filter {
                conditions.push(f);
            }

            let Some(filter_expr) = conditions.into_iter().reduce(Expr::and) else {
                return datafusion_common::plan_err!(
                    "join condition should not be empty for lateral join"
                );
            };

            LogicalPlanBuilder::from(projected)
                .filter(filter_expr)?
                .build()?
        };

        Ok(Transformed::yes(result))
    }
}

fn contains_outer_ref(expr: &Expr) -> bool {
    let mut found = false;
    let _ = expr.clone().transform(|e| {
        if matches!(e, Expr::OuterReferenceColumn(..)) {
            found = true;
        }
        Ok(Transformed::no(e))
    });
    found
}
