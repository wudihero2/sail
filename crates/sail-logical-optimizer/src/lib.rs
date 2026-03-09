use std::sync::Arc;

use datafusion::optimizer::{Analyzer, AnalyzerRule, Optimizer, OptimizerRule};

mod lateral_join;
pub use lateral_join::DecorrelateLateralProjection;

pub fn default_analyzer_rules() -> Vec<Arc<dyn AnalyzerRule + Send + Sync>> {
    let Analyzer {
        function_rewrites: _,
        rules: built_in_rules,
    } = Analyzer::default();

    let mut rules: Vec<Arc<dyn AnalyzerRule + Send + Sync>> = vec![];
    rules.extend(built_in_rules);
    rules
}

pub fn default_optimizer_rules() -> Vec<Arc<dyn OptimizerRule + Send + Sync>> {
    let Optimizer { rules } = Optimizer::default();
    let mut custom: Vec<Arc<dyn OptimizerRule + Send + Sync>> =
        vec![Arc::new(DecorrelateLateralProjection::new())];
    custom.extend(rules);
    custom
}
