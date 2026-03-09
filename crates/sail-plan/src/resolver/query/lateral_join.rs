use std::sync::Arc;

use datafusion_common::Spans;
use datafusion_expr::{build_join_schema, Expr, LogicalPlan, LogicalPlanBuilder, Subquery};
use sail_common::spec;

use crate::error::PlanResult;
use crate::resolver::state::PlanResolverState;
use crate::resolver::PlanResolver;

impl PlanResolver<'_> {
    pub(super) async fn resolve_query_lateral_join(
        &self,
        left: spec::QueryPlan,
        right: spec::QueryPlan,
        join_condition: Option<spec::Expr>,
        join_type: spec::JoinType,
        state: &mut PlanResolverState,
    ) -> PlanResult<LogicalPlan> {
        let left = self.resolve_query_plan(left, state).await?;

        let right = {
            let mut scope = state.enter_query_scope(left.schema().clone());
            self.resolve_query_plan(right, scope.state()).await?
        };

        let outer_ref_columns = right.all_out_ref_exprs();

        let right = if !outer_ref_columns.is_empty() && needs_subquery_wrapper(&right) {
            LogicalPlan::Subquery(Subquery {
                subquery: Arc::new(right),
                outer_ref_columns,
                spans: Spans::new(),
            })
        } else {
            right
        };

        let df_join_type = match join_type {
            spec::JoinType::Inner | spec::JoinType::Cross => datafusion_common::JoinType::Inner,
            spec::JoinType::LeftOuter => datafusion_common::JoinType::Left,
            spec::JoinType::RightOuter => datafusion_common::JoinType::Right,
            spec::JoinType::FullOuter => datafusion_common::JoinType::Full,
            spec::JoinType::LeftSemi => datafusion_common::JoinType::LeftSemi,
            spec::JoinType::LeftAnti => datafusion_common::JoinType::LeftAnti,
            spec::JoinType::RightSemi => datafusion_common::JoinType::RightSemi,
            spec::JoinType::RightAnti => datafusion_common::JoinType::RightAnti,
        };

        let join_schema = Arc::new(build_join_schema(
            left.schema(),
            right.schema(),
            &datafusion_common::JoinType::Inner,
        )?);

        let condition = if let Some(cond) = join_condition {
            Some(
                self.resolve_expression(cond, &join_schema, state)
                    .await?
                    .unalias_nested()
                    .data,
            )
        } else {
            None
        };

        let wrapped_as_subquery = matches!(&right, LogicalPlan::Subquery(_));

        let plan =
            if condition.is_none() && matches!(df_join_type, datafusion_common::JoinType::Inner) {
                LogicalPlanBuilder::from(left).cross_join(right)?.build()?
            } else {
                LogicalPlanBuilder::from(left)
                    .join_on(right, df_join_type, condition)?
                    .build()?
            };

        let plan = if wrapped_as_subquery {
            let original_columns: Vec<Expr> = plan
                .schema()
                .columns()
                .into_iter()
                .map(Expr::Column)
                .collect();
            LogicalPlanBuilder::from(plan)
                .project(original_columns)?
                .build()?
        } else {
            plan
        };

        Ok(plan)
    }
}

/// Check if OuterRef only appears in the top-level Projection's expr
/// (the pattern handled by DecorrelateLateralProjection).
/// If OuterRef also appears deeper (e.g. in Filter), return true
/// so it gets wrapped as Subquery for DecorrelateLateralJoin.
fn needs_subquery_wrapper(plan: &LogicalPlan) -> bool {
    let plan = match plan {
        LogicalPlan::SubqueryAlias(sa) => sa.input.as_ref(),
        other => other,
    };
    let LogicalPlan::Projection(proj) = plan else {
        return true;
    };
    // If the Projection's input contains any OuterRef, it needs Subquery wrapping.
    proj.input.all_out_ref_exprs().iter().count() > 0
}
