use roaring::RoaringBitmap;

use super::logger::SearchLogger;
use super::query_graph::QueryNode;
use super::ranking_rules::TotalBucketCount;
use super::resolve_query_graph::compute_query_graph_docids;
use super::small_bitmap::SmallBitmap;
use super::{QueryGraph, RankingRule, RankingRuleOutput, SearchContext};
use crate::{Result, TermsMatchingStrategy};

pub struct Words {
    exhausted: bool, // TODO: remove
    query_graph: Option<QueryGraph>,
    nodes_to_remove: Vec<SmallBitmap<QueryNode>>,
    terms_matching_strategy: TermsMatchingStrategy,
}
impl Words {
    pub fn new(terms_matching_strategy: TermsMatchingStrategy) -> Self {
        Self {
            exhausted: true,
            query_graph: None,
            nodes_to_remove: vec![],
            terms_matching_strategy,
        }
    }
}

impl<'ctx> RankingRule<'ctx, QueryGraph> for Words {
    fn id(&self) -> String {
        "words".to_owned()
    }
    fn start_iteration(
        &mut self,
        ctx: &mut SearchContext<'ctx>,
        _logger: &mut dyn SearchLogger<QueryGraph>,
        _universe: &RoaringBitmap,
        parent_query_graph: &QueryGraph,
    ) -> Result<TotalBucketCount> {
        self.exhausted = false;
        self.query_graph = Some(parent_query_graph.clone());
        self.nodes_to_remove = match self.terms_matching_strategy {
            TermsMatchingStrategy::Last => {
                let mut ns = parent_query_graph.removal_order_for_terms_matching_strategy_last(ctx);
                ns.reverse();
                ns
            }
            TermsMatchingStrategy::All => {
                vec![]
            }
        };
        Ok(self.nodes_to_remove.len() as u64 + 1)
    }

    fn next_bucket(
        &mut self,
        ctx: &mut SearchContext<'ctx>,
        logger: &mut dyn SearchLogger<QueryGraph>,
        universe: &RoaringBitmap,
    ) -> Result<Option<RankingRuleOutput<QueryGraph>>> {
        if self.exhausted {
            return Ok(None);
        }
        let Some(query_graph) = &mut self.query_graph else { panic!() };
        logger.log_internal_state(query_graph);

        let this_bucket = compute_query_graph_docids(ctx, query_graph, universe)?;

        let child_query_graph = query_graph.clone();

        if self.nodes_to_remove.is_empty() {
            self.exhausted = true;
        } else {
            let nodes_to_remove = self.nodes_to_remove.pop().unwrap();
            query_graph.remove_nodes_keep_edges(&nodes_to_remove.iter().collect::<Vec<_>>());
        }
        Ok(Some(RankingRuleOutput {
            query: child_query_graph,
            candidates: this_bucket,
            remaining_buckets: self.nodes_to_remove.len() as u64 + 2,
        }))
    }

    fn end_iteration(
        &mut self,
        _ctx: &mut SearchContext<'ctx>,
        _logger: &mut dyn SearchLogger<QueryGraph>,
    ) {
        self.exhausted = true;
        self.nodes_to_remove = vec![];
        self.query_graph = None;
    }
}
