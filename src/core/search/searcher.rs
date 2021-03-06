// Copyright 2019 Zhizhesihai (Beijing) Technology Limited.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use crossbeam::channel::{unbounded, Receiver, Sender};

use core::codec::{Codec, CodecTermState};
use core::index::LeafReaderContext;
use core::index::{get_terms, IndexReader, SearchLeafReader};
use core::index::{Term, TermContext, Terms};
use core::search::bm25_similarity::BM25Similarity;
use core::search::bulk_scorer::BulkScorer;
use core::search::cache_policy::{QueryCachingPolicy, UsageTrackingQueryCachingPolicy};
use core::search::collector::{self, Collector, ParallelLeafCollector, SearchCollector};
use core::search::explanation::Explanation;
use core::search::match_all::{ConstantScoreQuery, MatchAllDocsQuery};
use core::search::query_cache::{LRUQueryCache, QueryCache};
use core::search::statistics::{CollectionStatistics, TermStatistics};
use core::search::term_query::TermQuery;
use core::search::{Query, Scorer, Weight, NO_MORE_DOCS};
use core::search::{SimScorer, SimWeight, Similarity, SimilarityProducer};
use core::util::bits::Bits;
use core::util::thread_pool::{DefaultContext, ThreadPool, ThreadPoolBuilder};
use core::util::DocId;
use core::util::KeyedContext;

use error::{Error, ErrorKind, Result};

/// Implements search over a single IndexReader.
///
/// For performance reasons, if your index is unchanging, you
/// should share a single IndexSearcher instance across
/// multiple searches instead of creating a new one
/// per-search.  If your index has changed and you wish to
/// see the changes reflected in searching, you should
/// use `DirectoryReader::open`
/// to obtain a new reader and
/// then create a new IndexSearcher from that.  Also, for
/// low-latency turnaround it's best to use a near-real-time
/// reader.
/// Once you have a new `IndexReader`, it's relatively
/// cheap to create a new IndexSearcher from it.
///
/// *NOTE:* `IndexSearcher` instances are completely
/// thread safe, meaning multiple threads can call any of its
/// methods, concurrently.  If your application requires
/// external synchronization, you should *not*
/// synchronize on the `IndexSearcher` instance.

pub struct DefaultSimilarityProducer;

impl<C: Codec> SimilarityProducer<C> for DefaultSimilarityProducer {
    fn create(&self, _field: &str) -> Box<dyn Similarity<C>> {
        Box::new(BM25Similarity::default())
    }
}

pub struct NonScoringSimilarity;

impl<C: Codec> Similarity<C> for NonScoringSimilarity {
    fn compute_weight(
        &self,
        _collection_stats: &CollectionStatistics,
        _term_stats: &[TermStatistics],
        _context: Option<&KeyedContext>,
        _boost: f32,
    ) -> Box<dyn SimWeight<C>> {
        Box::new(NonScoringSimWeight {})
    }
}

impl fmt::Display for NonScoringSimilarity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "non-scoring")
    }
}

pub struct NonScoringSimWeight;

impl<C: Codec> SimWeight<C> for NonScoringSimWeight {
    fn get_value_for_normalization(&self) -> f32 {
        1.0f32
    }

    fn normalize(&mut self, _query_norm: f32, _boost: f32) {}

    fn sim_scorer(&self, _reader: &SearchLeafReader<C>) -> Result<Box<dyn SimScorer>> {
        Ok(Box::new(NonScoringSimScorer {}))
    }
}

pub struct NonScoringSimScorer;

impl SimScorer for NonScoringSimScorer {
    fn score(&mut self, _doc: i32, _freq: f32) -> Result<f32> {
        Ok(0f32)
    }

    fn compute_slop_factor(&self, _distance: i32) -> f32 {
        1.0f32
    }
}

pub trait IndexSearcher<C: Codec>: SearchPlanBuilder<C> {
    type Reader: IndexReader<Codec = C> + ?Sized;
    fn reader(&self) -> &Self::Reader;

    fn search<S>(&self, query: &dyn Query<C>, collector: &mut S) -> Result<()>
    where
        S: SearchCollector + ?Sized;

    fn search_parallel<S>(&self, query: &dyn Query<C>, collector: &mut S) -> Result<()>
    where
        S: SearchCollector + ?Sized;

    fn count(&self, query: &dyn Query<C>) -> Result<i32>;

    fn explain(&self, query: &dyn Query<C>, doc: DocId) -> Result<Explanation>;
}

pub trait SearchPlanBuilder<C: Codec> {
    fn num_docs(&self) -> i32;

    fn max_doc(&self) -> i32;

    /// Creates a {@link Weight} for the given query, potentially adding caching
    /// if possible and configured.
    fn create_weight(&self, query: &dyn Query<C>, needs_scores: bool)
        -> Result<Box<dyn Weight<C>>>;

    /// Creates a normalized weight for a top-level `Query`.
    /// The query is rewritten by this method and `Query#createWeight` called,
    /// afterwards the `Weight` is normalized. The returned `Weight`
    /// can then directly be used to get a `Scorer`.
    fn create_normalized_weight(
        &self,
        query: &dyn Query<C>,
        needs_scores: bool,
    ) -> Result<Box<dyn Weight<C>>>;

    fn similarity(&self, field: &str, needs_scores: bool) -> Box<dyn Similarity<C>>;

    fn term_state(&self, term: &Term) -> Result<Arc<TermContext<CodecTermState<C>>>>;

    fn term_statistics(
        &self,
        term: Term,
        context: &TermContext<CodecTermState<C>>,
    ) -> TermStatistics;

    fn collections_statistics(&self, field: &str) -> Result<CollectionStatistics>;
}

pub struct DefaultIndexSearcher<
    C: Codec,
    R: IndexReader<Codec = C> + ?Sized,
    IR: Deref<Target = R>,
    SP: SimilarityProducer<C>,
> {
    reader: IR,
    sim_producer: SP,
    query_cache: Arc<dyn QueryCache<C>>,
    cache_policy: Arc<dyn QueryCachingPolicy<C>>,
    collection_statistics: RwLock<HashMap<String, CollectionStatistics>>,
    term_contexts: RwLock<HashMap<String, Arc<TermContext<CodecTermState<C>>>>>,
    thread_pool: Option<Arc<ThreadPool<DefaultContext>>>,
}

impl<C: Codec, R: IndexReader<Codec = C> + ?Sized, IR: Deref<Target = R>>
    DefaultIndexSearcher<C, R, IR, DefaultSimilarityProducer>
{
    pub fn new(reader: IR) -> DefaultIndexSearcher<C, R, IR, DefaultSimilarityProducer> {
        Self::with_similarity(reader, DefaultSimilarityProducer {})
    }
}

impl<C, R, IR, SP> DefaultIndexSearcher<C, R, IR, SP>
where
    C: Codec,
    R: IndexReader<Codec = C> + ?Sized,
    IR: Deref<Target = R>,
    SP: SimilarityProducer<C>,
{
    pub fn with_similarity(reader: IR, sim_producer: SP) -> DefaultIndexSearcher<C, R, IR, SP> {
        DefaultIndexSearcher {
            reader,
            sim_producer,
            query_cache: Arc::new(LRUQueryCache::new(1000)),
            cache_policy: Arc::new(UsageTrackingQueryCachingPolicy::default()),
            collection_statistics: RwLock::new(HashMap::new()),
            term_contexts: RwLock::new(HashMap::new()),
            thread_pool: None,
        }
    }

    pub fn with_thread_pool(&mut self, num_threads: usize) {
        // at least 2 thread to support parallel
        if num_threads > 1 {
            let thread_pool = ThreadPoolBuilder::with_default_factory("search".into())
                .thread_count(num_threads)
                .build();
            self.thread_pool = Some(Arc::new(thread_pool));
        }
    }

    pub fn set_thread_pool(&mut self, pool: Arc<ThreadPool<DefaultContext>>) {
        self.thread_pool = Some(pool);
    }

    pub fn set_query_cache(&mut self, cache: Arc<dyn QueryCache<C>>) {
        self.query_cache = cache;
    }

    pub fn set_query_cache_policy(&mut self, cache_policy: Arc<dyn QueryCachingPolicy<C>>) {
        self.cache_policy = cache_policy;
    }

    fn do_search<S: Scorer + ?Sized, T: Collector + ?Sized, B: Bits + ?Sized>(
        scorer: &mut S,
        collector: &mut T,
        live_docs: &B,
    ) -> Result<()> {
        let mut bulk_scorer = BulkScorer::new(scorer);
        match bulk_scorer.score(collector, Some(live_docs), 0, NO_MORE_DOCS) {
            Err(Error(ErrorKind::Collector(collector::ErrorKind::CollectionTerminated), _)) => {
                // Collection was terminated prematurely
                Ok(())
            }
            Err(Error(ErrorKind::Collector(collector::ErrorKind::LeafCollectionTerminated), _))
            | Ok(_) => {
                // Leaf collection was terminated prematurely,
                // continue with the following leaf
                Ok(())
            }
            Err(e) => {
                // something goes wrong, stop search and return error!
                return Err(e);
            }
        }
    }
}

impl<C, R, IR, SP> IndexSearcher<C> for DefaultIndexSearcher<C, R, IR, SP>
where
    C: Codec,
    R: IndexReader<Codec = C> + ?Sized,
    IR: Deref<Target = R>,
    SP: SimilarityProducer<C>,
{
    type Reader = R;
    #[inline]
    fn reader(&self) -> &R {
        &*self.reader
    }

    /// Lower-level search API.
    fn search<S>(&self, query: &dyn Query<C>, collector: &mut S) -> Result<()>
    where
        S: SearchCollector + ?Sized,
    {
        let weight = self.create_weight(query, collector.needs_scores())?;

        for reader in self.reader.leaves() {
            if let Some(mut scorer) = weight.create_scorer(&reader)? {
                // some in running segment maybe wrong, just skip it!
                // TODO maybe we should matching more specific error type
                if let Err(e) = collector.set_next_reader(&reader) {
                    error!(
                        "set next reader for leaf {} failed!, {:?}",
                        reader.reader.name(),
                        e
                    );
                    continue;
                }
                let live_docs = reader.reader.live_docs();

                Self::do_search(&mut *scorer, collector, live_docs.as_ref())?;
            }
        }

        Ok(())
    }

    fn search_parallel<S>(&self, query: &dyn Query<C>, collector: &mut S) -> Result<()>
    where
        S: SearchCollector + ?Sized,
    {
        if collector.support_parallel() && self.reader.leaves().len() > 1 {
            if let Some(ref thread_pool) = self.thread_pool {
                let weight = self.create_weight(query, collector.needs_scores())?;

                for (_ord, reader) in self.reader.leaves().iter().enumerate() {
                    if let Some(scorer) = weight.create_scorer(reader)? {
                        match collector.leaf_collector(reader) {
                            Ok(leaf_collector) => {
                                let live_docs = reader.reader.live_docs();
                                thread_pool.execute(move |_ctx| {
                                    let mut collector = leaf_collector;
                                    let mut scorer = scorer;
                                    if let Err(e) = Self::do_search(
                                        scorer.as_mut(),
                                        &mut collector,
                                        live_docs.as_ref(),
                                    ) {
                                        error!(
                                            "do search parallel failed by '{:?}', may return \
                                             partial result",
                                            e
                                        );
                                    }
                                    if let Err(e) = collector.finish_leaf() {
                                        error!(
                                            "finish search parallel failed by '{:?}', may return \
                                             partial result",
                                            e
                                        );
                                    }
                                })
                            }
                            Err(e) => {
                                error!(
                                    "create leaf collector for leaf {} failed with '{:?}'",
                                    reader.reader.name(),
                                    e
                                );
                            }
                        }
                    }
                }
                return collector.finish_parallel();
            }
        }
        self.search(query, collector)
    }

    fn count(&self, query: &dyn Query<C>) -> Result<i32> {
        let mut query = query;
        loop {
            if let Some(constant_query) = query.as_any().downcast_ref::<ConstantScoreQuery<C>>() {
                query = constant_query.get_raw_query();
            } else {
                break;
            }
        }

        if let Some(_) = query.as_any().downcast_ref::<MatchAllDocsQuery>() {
            return Ok(self.reader().num_docs());
        } else if let Some(term_query) = query.as_any().downcast_ref::<TermQuery>() {
            if !self.reader().has_deletions() {
                let term = &term_query.term;
                let mut count = 0;
                for leaf in self.reader().leaves() {
                    count += leaf.reader.doc_freq(term)?;
                }
                return Ok(count);
            }
        }

        let mut collector = TotalHitCountCollector::new();
        self.search_parallel(query, &mut collector)?;
        Ok(collector.total_hits())
    }

    fn explain(&self, query: &dyn Query<C>, doc: DocId) -> Result<Explanation> {
        let reader = self.reader.leaf_reader_for_doc(doc);
        let live_docs = reader.reader.live_docs();
        if !live_docs.get((doc - reader.doc_base()) as usize)? {
            Ok(Explanation::new(
                false,
                0.0f32,
                format!("Document {} if deleted", doc),
                vec![],
            ))
        } else {
            self.create_normalized_weight(query, true)?
                .explain(&reader, doc - reader.doc_base())
        }
    }
}

impl<C, R, IR, SP> SearchPlanBuilder<C> for DefaultIndexSearcher<C, R, IR, SP>
where
    C: Codec,
    R: IndexReader<Codec = C> + ?Sized,
    IR: Deref<Target = R>,
    SP: SimilarityProducer<C>,
{
    fn num_docs(&self) -> i32 {
        self.reader.num_docs()
    }

    fn max_doc(&self) -> i32 {
        self.reader.max_doc()
    }

    /// Creates a {@link Weight} for the given query, potentially adding caching
    /// if possible and configured.
    fn create_weight(
        &self,
        query: &dyn Query<C>,
        needs_scores: bool,
    ) -> Result<Box<dyn Weight<C>>> {
        let mut weight = query.create_weight(self, needs_scores)?;
        if !needs_scores {
            weight = self
                .query_cache
                .do_cache(weight, Arc::clone(&self.cache_policy));
        }
        Ok(weight)
    }

    /// Creates a normalized weight for a top-level `Query`.
    /// The query is rewritten by this method and `Query#createWeight` called,
    /// afterwards the `Weight` is normalized. The returned `Weight`
    /// can then directly be used to get a `Scorer`.
    fn create_normalized_weight(
        &self,
        query: &dyn Query<C>,
        needs_scores: bool,
    ) -> Result<Box<dyn Weight<C>>> {
        let weight = self.create_weight(query, needs_scores)?;
        //        let v = weight.value_for_normalization();
        //        let mut norm: f32 = self.similarity("", needs_scores).query_norm(v, None);
        //        if norm.is_finite() || norm.is_nan() {
        //            norm = 1.0f32;
        //        }
        //        weight.normalize(norm, 1.0f32);
        Ok(weight)
    }

    fn similarity(&self, field: &str, needs_scores: bool) -> Box<dyn Similarity<C>> {
        if needs_scores {
            self.sim_producer.create(field)
        } else {
            Box::new(NonScoringSimilarity {})
        }
    }

    fn term_state(&self, term: &Term) -> Result<Arc<TermContext<CodecTermState<C>>>> {
        let term_context: Arc<TermContext<CodecTermState<C>>>;
        let mut builded = false;
        let term_key = format!("{}_{}", term.field, term.text()?);
        if self.term_contexts.read().unwrap().contains_key(&term_key) {
            builded = true;
        }

        if builded {
            term_context = Arc::clone(self.term_contexts.read().unwrap().get(&term_key).unwrap());
        } else {
            let mut context = TermContext::new(&*self.reader);
            context.build(&*self.reader, &term)?;
            term_context = Arc::new(context);
            self.term_contexts
                .write()
                .unwrap()
                .insert(term_key.clone(), Arc::clone(&term_context));
        };

        Ok(term_context)
    }

    fn term_statistics(
        &self,
        term: Term,
        context: &TermContext<CodecTermState<C>>,
    ) -> TermStatistics {
        TermStatistics::new(
            term.bytes,
            i64::from(context.doc_freq),
            context.total_term_freq,
        )
    }

    fn collections_statistics(&self, field: &str) -> Result<CollectionStatistics> {
        {
            let statistics = self.collection_statistics.read().unwrap();
            if let Some(stat) = statistics.get(field) {
                return Ok(stat.clone());
            }
        }
        // slow path
        let mut doc_count = 0i32;
        let mut sum_total_term_freq = 0i64;
        let mut sum_doc_freq = 0i64;
        if let Some(terms) = get_terms(&*self.reader, field)? {
            doc_count = terms.doc_count()?;
            sum_total_term_freq = terms.sum_total_term_freq()?;
            sum_doc_freq = terms.sum_doc_freq()?;
        }
        let stat = CollectionStatistics::new(
            field.into(),
            i64::from(self.reader.max_doc()),
            i64::from(doc_count),
            sum_total_term_freq,
            sum_doc_freq,
        );

        let mut statistics = self.collection_statistics.write().unwrap();
        statistics.insert(field.into(), stat);
        Ok(statistics[field].clone())
    }
}

struct TotalHitCountCollector {
    total_hits: i32,
    channel: Option<(Sender<i32>, Receiver<i32>)>,
}

impl TotalHitCountCollector {
    pub fn new() -> Self {
        TotalHitCountCollector {
            total_hits: 0,
            channel: None,
        }
    }

    pub fn total_hits(&self) -> i32 {
        self.total_hits
    }
}

impl SearchCollector for TotalHitCountCollector {
    type LC = TotalHitsCountLeafCollector;
    fn set_next_reader<C: Codec>(&mut self, _reader: &LeafReaderContext<'_, C>) -> Result<()> {
        Ok(())
    }

    fn support_parallel(&self) -> bool {
        true
    }

    fn leaf_collector<C: Codec>(
        &mut self,
        _reader: &LeafReaderContext<'_, C>,
    ) -> Result<TotalHitsCountLeafCollector> {
        if self.channel.is_none() {
            self.channel = Some(unbounded());
        }
        Ok(TotalHitsCountLeafCollector {
            count: 0,
            sender: self.channel.as_ref().unwrap().0.clone(),
        })
    }

    fn finish_parallel(&mut self) -> Result<()> {
        let channel = self.channel.take();
        // iff all the `weight.create_scorer(leaf_reader)` return None, the channel won't
        // inited and thus stay None
        if let Some((sender, receiver)) = channel {
            drop(sender);
            while let Ok(v) = receiver.recv() {
                self.total_hits += v;
            }
        }

        Ok(())
    }
}

impl Collector for TotalHitCountCollector {
    fn needs_scores(&self) -> bool {
        false
    }

    fn collect<S: Scorer + ?Sized>(&mut self, _doc: i32, _scorer: &mut S) -> Result<()> {
        self.total_hits += 1;
        Ok(())
    }
}

struct TotalHitsCountLeafCollector {
    count: i32,
    sender: Sender<i32>,
}

impl Collector for TotalHitsCountLeafCollector {
    fn needs_scores(&self) -> bool {
        false
    }

    fn collect<S: Scorer + ?Sized>(&mut self, _doc: i32, _scorer: &mut S) -> Result<()> {
        self.count += 1;
        Ok(())
    }
}

impl ParallelLeafCollector for TotalHitsCountLeafCollector {
    fn finish_leaf(&mut self) -> Result<()> {
        self.sender.send(self.count).map_err(|e| {
            ErrorKind::IllegalState(format!(
                "channel unexpected closed before search complete with err: {:?}",
                e
            ))
            .into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::codec::tests::TestCodec;
    use core::index::tests::*;
    use core::search::collector::top_docs::*;
    use core::search::collector::*;
    use core::search::term_query::TermQuery;
    use core::search::tests::*;
    use core::search::*;
    use core::util::DocId;
    use std::sync::atomic::Ordering;

    pub const MOCK_QUERY: &str = "mock";

    struct MockQuery {
        docs: Vec<DocId>,
    }

    impl MockQuery {
        pub fn new(docs: Vec<DocId>) -> MockQuery {
            MockQuery { docs }
        }
    }

    impl<C: Codec> Query<C> for MockQuery {
        fn create_weight(
            &self,
            _searcher: &dyn SearchPlanBuilder<C>,
            _needs_scores: bool,
        ) -> Result<Box<dyn Weight<C>>> {
            Ok(Box::new(create_mock_weight(self.docs.clone())))
        }

        fn extract_terms(&self) -> Vec<TermQuery> {
            unimplemented!()
        }

        fn query_type(&self) -> &'static str {
            MOCK_QUERY
        }

        fn as_any(&self) -> &::std::any::Any {
            unreachable!()
        }
    }

    impl fmt::Display for MockQuery {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "MockQuery")
        }
    }

    #[test]
    fn test_early_terminating_search() {
        let leaf_reader1 = MockLeafReader::new(0);
        let leaf_reader2 = MockLeafReader::new(10);
        let leaf_reader3 = MockLeafReader::new(20);
        let index_reader: Arc<dyn IndexReader<Codec = TestCodec>> =
            Arc::new(MockIndexReader::new(vec![
                leaf_reader1,
                leaf_reader2,
                leaf_reader3,
            ]));

        let mut top_collector = TopDocsCollector::new(3);
        {
            let mut early_terminating_collector = EarlyTerminatingSortingCollector::new(3);
            {
                let mut chained_collector =
                    ChainedCollector::new(&mut early_terminating_collector, &mut top_collector);
                let query = MockQuery::new(vec![1, 5, 3, 4, 2]);
                {
                    let searcher = DefaultIndexSearcher::new(index_reader);
                    searcher.search(&query, &mut chained_collector).unwrap();
                }
            }

            assert_eq!(
                early_terminating_collector
                    .early_terminated
                    .load(Ordering::Acquire),
                true
            );
        }

        let top_docs = top_collector.top_docs();
        assert_eq!(top_docs.total_hits(), 9);

        let score_docs = top_docs.score_docs();
        assert_eq!(score_docs.len(), 3);
        assert!((score_docs[0].score() - 5f32) < ::std::f32::EPSILON);
        assert!((score_docs[1].score() - 5f32) < ::std::f32::EPSILON);
        assert!((score_docs[2].score() - 5f32) < ::std::f32::EPSILON);
    }
}
