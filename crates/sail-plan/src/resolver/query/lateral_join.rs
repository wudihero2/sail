use std::sync::Arc;

use datafusion_expr::{LogicalPlan, LogicalPlanBuilder};
use sail_common::spec;

use crate::error::{PlanError, PlanResult};
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
        // 1. Resolve left plan
        let left = self.resolve_query_plan(left, state).await?;

        // 2. Resolve right plan with outer scope so that references to left
        //    columns produce OuterReferenceColumn expressions.
        let right = {
            let mut scope = state.enter_query_scope(left.schema().clone());
            self.resolve_query_plan(right, scope.state()).await?
        };

        // 3. Build cross join. The OuterReferenceColumn in the right plan
        //    will be cleaned up by the DecorrelateLateralProjection optimizer rule.
        let mut plan = LogicalPlanBuilder::from(left)
            .cross_join(right)?
            .build()?;

        use std::fs;
        fs::write("/Users/stanhsu/projects/sail/t.txt", format!("{plan:#?}")).unwrap();

        // 4. Apply join condition as filter if present
        if let Some(condition) = join_condition {
            let condition = self
                .resolve_expression(condition, plan.schema(), state)
                .await?
                .unalias_nested()
                .data;
            plan = LogicalPlanBuilder::from(plan)
                .filter(condition)?
                .build()?;
        }
        Ok(plan)
    }
}
