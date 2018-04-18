use std::boxed::Box;
use std::fmt;

use core::index::LeafReader;
use core::search::conjunction::ConjunctionScorer;
use core::search::disjunction::DisjunctionScorer;
use core::search::match_all::ConstantScoreQuery;
use core::search::req_opt::ReqOptScorer;
use core::search::searcher::IndexSearcher;
use core::search::term_query::TermQuery;
use core::search::Query;
use core::search::Scorer;
use core::search::Weight;
use error::*;

pub struct BooleanQuery {
    must_queries: Vec<Box<Query>>,
    should_queries: Vec<Box<Query>>,
    filter_queries: Vec<Box<Query>>,
    minimum_should_match: i32,
}

impl BooleanQuery {
    pub fn build(
        musts: Vec<Box<Query>>,
        shoulds: Vec<Box<Query>>,
        filters: Vec<Box<Query>>,
    ) -> Result<Box<Query>> {
        let minimum_should_match = if musts.is_empty() { 1 } else { 0 };
        let mut musts = musts;
        let mut shoulds = shoulds;
        let mut filters = filters;
        if musts.len() + shoulds.len() + filters.len() == 0 {
            bail!("boolean query should at least contain one inner query!");
        }
        if musts.len() + shoulds.len() + filters.len() == 1 {
            let query = if musts.len() == 1 {
                musts.remove(0)
            } else if shoulds.len() == 1 {
                shoulds.remove(0)
            } else {
                Box::new(ConstantScoreQuery::with_weight(filters.remove(0), 0f32))
            };
            return Ok(query);
        }
        Ok(Box::new(BooleanQuery {
            must_queries: musts,
            should_queries: shoulds,
            filter_queries: filters,
            minimum_should_match,
        }))
    }

    fn queries_to_str(&self, queries: &[Box<Query>]) -> String {
        let query_strs: Vec<String> = queries.iter().map(|q| format!("{}", q)).collect();
        query_strs.join(", ")
    }
}

impl Query for BooleanQuery {
    fn create_weight(&self, searcher: &IndexSearcher, needs_scores: bool) -> Result<Box<Weight>> {
        let mut must_weights =
            Vec::with_capacity(self.must_queries.len() + self.filter_queries.len());
        for q in &self.must_queries {
            must_weights.push(q.create_weight(searcher, needs_scores)?);
        }
        for q in &self.filter_queries {
            must_weights.push(q.create_weight(searcher, false)?);
        }
        let mut should_weights = Vec::new();
        for q in &self.should_queries {
            should_weights.push(q.create_weight(searcher, needs_scores)?);
        }

        Ok(Box::new(BooleanWeight::new(
            must_weights,
            should_weights,
            needs_scores,
        )))
    }

    fn extract_terms(&self) -> Vec<TermQuery> {
        let mut term_query_list: Vec<TermQuery> = vec![];

        for query in &self.must_queries {
            for term_query in query.extract_terms() {
                term_query_list.push(term_query);
            }
        }

        for query in &self.should_queries {
            for term_query in query.extract_terms() {
                term_query_list.push(term_query);
            }
        }

        term_query_list
    }
}

impl fmt::Display for BooleanQuery {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let must_str = self.queries_to_str(&self.must_queries);
        let should_str = self.queries_to_str(&self.should_queries);
        write!(
            f,
            "BooleanQuery(must: [{}], should: [{}], match: {})",
            must_str, should_str, self.minimum_should_match
        )
    }
}

pub struct BooleanWeight {
    must_weights: Vec<Box<Weight>>,
    should_weights: Vec<Box<Weight>>,
    #[allow(dead_code)]
    minimum_should_match: i32,
    #[allow(dead_code)]
    needs_scores: bool,
}

impl BooleanWeight {
    pub fn new(
        musts: Vec<Box<Weight>>,
        shoulds: Vec<Box<Weight>>,
        needs_scores: bool,
    ) -> BooleanWeight {
        let minimum_should_match = if musts.is_empty() { 1 } else { 0 };
        BooleanWeight {
            must_weights: musts,
            should_weights: shoulds,
            minimum_should_match,
            needs_scores,
        }
    }

    fn build_scorers(
        &self,
        weights: &[Box<Weight>],
        leaf_reader: &LeafReader,
    ) -> Result<Vec<Box<Scorer>>> {
        let mut result = Vec::with_capacity(weights.len());
        for weight in weights {
            let scorer = weight.create_scorer(leaf_reader)?;
            result.push(scorer)
        }
        Ok(result)
    }
}

impl Weight for BooleanWeight {
    fn create_scorer(&self, leaf_reader: &LeafReader) -> Result<Box<Scorer>> {
        let must_scorer: Option<Box<Scorer>> = if !self.must_weights.is_empty() {
            if self.must_weights.len() > 1 {
                Some(Box::new(ConjunctionScorer::new(self.build_scorers(
                    &self.must_weights,
                    leaf_reader,
                )?)))
            } else {
                Some(self.must_weights[0].create_scorer(leaf_reader)?)
            }
        } else {
            None
        };
        let should_scorer: Option<Box<Scorer>> = if !self.should_weights.is_empty() {
            if self.should_weights.len() > 1 {
                Some(Box::new(DisjunctionScorer::new(self.build_scorers(
                    &self.should_weights,
                    leaf_reader,
                )?)))
            } else {
                Some(self.should_weights[0].create_scorer(leaf_reader)?)
            }
        } else {
            None
        };
        debug_assert!(must_scorer.is_some() || should_scorer.is_some());
        if must_scorer.is_none() {
            Ok(should_scorer.unwrap())
        } else if should_scorer.is_none() {
            Ok(must_scorer.unwrap())
        } else {
            Ok(Box::new(ReqOptScorer::new(
                must_scorer.unwrap(),
                should_scorer.unwrap(),
            )))
        }
    }
}
