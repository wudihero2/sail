use std::sync::Arc;

use datafusion::optimizer::{ApplyOrder, OptimizerConfig, OptimizerRule};
use datafusion_common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion_common::Result;
use datafusion_expr::{Expr, LogicalPlan, LogicalPlanBuilder};

/// Rewrites lateral join plans that contain `OuterReferenceColumn` in the
/// right side's projection.
///
/// The lateral join resolver produces:
/// ```text
/// Join(Inner, on=[], filter=None)
///   left: <any plan>
///   right: Projection([OuterRef(left.col) AS alias, ...])
///     input: <base plan>
/// ```
///
/// This rule rewrites it to:
/// ```text
/// Projection([left_cols..., Column(left.col) AS alias, ...])
///   CrossJoin(left, base_plan)
/// ```
///
/// This must run before DataFusion's built-in `DecorrelateLateralJoin` to
/// prevent `OuterReferenceColumn` from leaking into containing expressions
/// like `ScalarSubquery`.
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

    /// Return `Some(BottomUp)` so the optimizer framework handles recursion
    /// including descending into subquery expressions (ScalarSubquery, etc.).
    fn apply_order(&self) -> Option<ApplyOrder> {
        Some(ApplyOrder::BottomUp)
    }

    fn rewrite(
        &self,
        plan: LogicalPlan,
        _config: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>> {
        // Only match Join nodes
        let LogicalPlan::Join(join) = &plan else {
            return Ok(Transformed::no(plan));
        };

        // Only match cross-join-like joins (no ON conditions, no filter)
        if !join.on.is_empty() || join.filter.is_some() {
            return Ok(Transformed::no(plan));
        }

        // Right side must be a Projection
        let LogicalPlan::Projection(proj) = &*join.right else {
            return Ok(Transformed::no(plan));
        };

        // Check if any projection expression contains OuterReferenceColumn
        if !proj.expr.iter().any(|e| contains_outer_ref(e)) {
            return Ok(Transformed::no(plan));
        }

        // Save left field count before destructuring
        let left_field_count = join.left.schema().fields().len();

        // Destructure the Join and Projection to take ownership
        let LogicalPlan::Join(join) = plan else {
            unreachable!();
        };
        let left = Arc::unwrap_or_clone(join.left);
        let LogicalPlan::Projection(proj) = Arc::unwrap_or_clone(join.right) else {
            unreachable!();
        };
        let right_exprs = proj.expr;
        let right_base = Arc::unwrap_or_clone(proj.input);

        // Build CrossJoin(left, right_base)
        let cross_join = LogicalPlanBuilder::from(left)
            .cross_join(right_base)?
            .build()?;

        // Build output: left columns + right expressions with OuterRef replaced
        let mut output_exprs: Vec<Expr> = Vec::new();

        // Keep all columns from the left side
        for col in cross_join
            .schema()
            .columns()
            .into_iter()
            .take(left_field_count)
        {
            output_exprs.push(Expr::Column(col));
        }

        // Replace OuterReferenceColumn with Column in right expressions
        for expr in right_exprs {
            let replaced = expr
                .transform(|e| match e {
                    Expr::OuterReferenceColumn(_field, col) => {
                        Ok(Transformed::yes(Expr::Column(col)))
                    }
                    _ => Ok(Transformed::no(e)),
                })
                .data()?;
            output_exprs.push(replaced);
        }

        let result = LogicalPlanBuilder::from(cross_join)
            .project(output_exprs)?
            .build()?;

        Ok(Transformed::yes(result))
    }
}

/// Returns true if the expression tree contains any `OuterReferenceColumn`.
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
