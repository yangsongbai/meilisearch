use roaring::RoaringBitmap;

use super::logger::SearchLogger;
use super::ranking_rules::{BoxRankingRule, RankingRuleQueryTrait};
use super::SearchContext;
use crate::search::new::distinct::{apply_distinct_rule, distinct_single_docid, DistinctOutput};
use crate::Result;

pub struct BucketSortOutput {
    pub docids: Vec<u32>,
    pub scores: Vec<f64>,
    pub all_candidates: RoaringBitmap,
}

pub fn bucket_sort<'ctx, Q: RankingRuleQueryTrait>(
    ctx: &mut SearchContext<'ctx>,
    mut ranking_rules: Vec<BoxRankingRule<'ctx, Q>>,
    query: &Q,
    universe: &RoaringBitmap,
    from: usize,
    length: usize,
    logger: &mut dyn SearchLogger<Q>,
) -> Result<BucketSortOutput> {
    logger.initial_query(query);
    logger.ranking_rules(&ranking_rules);
    logger.initial_universe(universe);

    let distinct_fid = if let Some(field) = ctx.index.distinct_field(ctx.txn)? {
        ctx.index.fields_ids_map(ctx.txn)?.id(field)
    } else {
        None
    };

    if universe.len() < from as u64 {
        return Ok(BucketSortOutput {
            docids: vec![],
            scores: vec![],
            all_candidates: universe.clone(),
        });
    }
    if ranking_rules.is_empty() {
        if let Some(distinct_fid) = distinct_fid {
            let mut excluded = RoaringBitmap::new();
            let mut results = vec![];
            for docid in universe.iter() {
                if results.len() >= from + length {
                    break;
                }
                if excluded.contains(docid) {
                    continue;
                }
                distinct_single_docid(ctx.index, ctx.txn, distinct_fid, docid, &mut excluded)?;
                results.push(docid);
            }
            let mut all_candidates = universe - excluded;
            all_candidates.extend(results.iter().copied());
            return Ok(BucketSortOutput {
                scores: vec![1.0; results.len()],
                docids: results,
                all_candidates,
            });
        } else {
            let docids: Vec<u32> = universe.iter().skip(from).take(length).collect();
            return Ok(BucketSortOutput {
                scores: vec![1.0; docids.len()],
                docids,
                all_candidates: universe.clone(),
            });
        };
    }

    let ranking_rules_len = ranking_rules.len();

    logger.start_iteration_ranking_rule(0, ranking_rules[0].as_ref(), query, universe);
    let mut ranking_rule_total_bucket_counts: Vec<u64> = vec![0; ranking_rules_len];
    ranking_rule_total_bucket_counts[0] =
        ranking_rules[0].start_iteration(ctx, logger, universe, query)?;
    // A vector of the number of leftover buckets and total buckets of each preceding ranking rule.
    // To compute the global bucket of a local bucket returned by a ranking rule,
    // we add it to the leftover buckets minus one multiplicated by the total number of buckets returned by that ranking rule.
    let mut ranking_rule_bucket_counts: Vec<(u64, u64)> = vec![(1, 1); ranking_rules_len];

    let mut ranking_rule_universes: Vec<RoaringBitmap> =
        vec![RoaringBitmap::default(); ranking_rules_len];
    ranking_rule_universes[0] = universe.clone();
    let mut cur_ranking_rule_index = 0;

    /// Finish iterating over the current ranking rule, yielding
    /// control to the parent (or finishing the search if not possible).
    /// Update the universes accordingly and inform the logger.
    macro_rules! back {
        () => {
            assert!(
                ranking_rule_universes[cur_ranking_rule_index].is_empty(),
                "The ranking rule {} did not sort its bucket exhaustively",
                ranking_rules[cur_ranking_rule_index].id()
            );
            logger.end_iteration_ranking_rule(
                cur_ranking_rule_index,
                ranking_rules[cur_ranking_rule_index].as_ref(),
                &ranking_rule_universes[cur_ranking_rule_index],
            );
            ranking_rule_universes[cur_ranking_rule_index].clear();
            ranking_rules[cur_ranking_rule_index].end_iteration(ctx, logger);
            if cur_ranking_rule_index == 0 {
                break;
            } else {
                cur_ranking_rule_index -= 1;
            }
        };
    }

    let mut all_candidates = universe.clone();
    let mut valid_docids = vec![];
    let mut valid_scores = vec![];
    let mut cur_offset = 0usize;

    macro_rules! maybe_add_to_results {
        ($candidates:expr, $score:expr) => {
            maybe_add_to_results(
                ctx,
                from,
                length,
                logger,
                &mut valid_docids,
                &mut valid_scores,
                &mut all_candidates,
                &mut ranking_rule_universes,
                &mut ranking_rules,
                cur_ranking_rule_index,
                &mut cur_offset,
                distinct_fid,
                $candidates,
                $score,
            )?;
        };
    }

    while valid_docids.len() < length {
        let (leftover_buckets, total_buckets) = ranking_rule_bucket_counts[cur_ranking_rule_index];
        // The universe for this bucket is zero or one element, so we don't need to sort
        // anything, just extend the results and go back to the parent ranking rule.
        if ranking_rule_universes[cur_ranking_rule_index].len() <= 1 {
            let bucket = std::mem::take(&mut ranking_rule_universes[cur_ranking_rule_index]);
            dbg!(
                "adding leftover for rule",
                ranking_rules[cur_ranking_rule_index].id(),
                leftover_buckets,
                total_buckets
            );
            maybe_add_to_results!(bucket, leftover_buckets as f64 / total_buckets as f64);
            back!();
            continue;
        }
        // remove one bucket from the leftovers as it is the one we'll compute.
        let mut remaining_buckets = leftover_buckets.checked_sub(1).unwrap();
        // multiply both the numerator and the total by the total count for the rule
        remaining_buckets *= ranking_rule_total_bucket_counts[cur_ranking_rule_index];
        let total_buckets =
            total_buckets * ranking_rule_total_bucket_counts[cur_ranking_rule_index];

        let Some(next_bucket) = ranking_rules[cur_ranking_rule_index].next_bucket(ctx, logger, &ranking_rule_universes[cur_ranking_rule_index])? else {
            back!();
            continue;
        };

        // add the remaining buckets from the ranking rule, increasing of at most the bucket of higher level that has been removed.
        remaining_buckets += next_bucket.remaining_buckets;

        println!(
            "computing bucket for rule '{}': local {}/{}, global {}/{}",
            ranking_rules[cur_ranking_rule_index].id(),
            next_bucket.remaining_buckets,
            ranking_rule_total_bucket_counts[cur_ranking_rule_index],
            remaining_buckets,
            total_buckets
        );

        logger.next_bucket_ranking_rule(
            cur_ranking_rule_index,
            ranking_rules[cur_ranking_rule_index].as_ref(),
            &ranking_rule_universes[cur_ranking_rule_index],
            &next_bucket.candidates,
        );

        debug_assert!(
            ranking_rule_universes[cur_ranking_rule_index].is_superset(&next_bucket.candidates)
        );
        ranking_rule_universes[cur_ranking_rule_index] -= &next_bucket.candidates;

        if cur_ranking_rule_index == ranking_rules_len - 1
            || next_bucket.candidates.len() <= 1
            || cur_offset + (next_bucket.candidates.len() as usize) < from
        {
            maybe_add_to_results!(
                next_bucket.candidates,
                remaining_buckets as f64 / total_buckets as f64
            );
            continue;
        }

        cur_ranking_rule_index += 1;
        ranking_rule_universes[cur_ranking_rule_index] = next_bucket.candidates.clone();
        logger.start_iteration_ranking_rule(
            cur_ranking_rule_index,
            ranking_rules[cur_ranking_rule_index].as_ref(),
            &next_bucket.query,
            &ranking_rule_universes[cur_ranking_rule_index],
        );
        ranking_rule_total_bucket_counts[cur_ranking_rule_index] = ranking_rules
            [cur_ranking_rule_index]
            .start_iteration(ctx, logger, &next_bucket.candidates, &next_bucket.query)?;
        ranking_rule_bucket_counts[cur_ranking_rule_index] = (remaining_buckets, total_buckets);
    }

    Ok(BucketSortOutput { docids: valid_docids, scores: valid_scores, all_candidates })
}

/// Add the candidates to the results. Take `distinct`, `from`, `length`, and `cur_offset`
/// into account and inform the logger.
#[allow(clippy::too_many_arguments)]
fn maybe_add_to_results<'ctx, Q: RankingRuleQueryTrait>(
    ctx: &mut SearchContext<'ctx>,
    from: usize,
    length: usize,
    logger: &mut dyn SearchLogger<Q>,

    valid_docids: &mut Vec<u32>,
    valid_scores: &mut Vec<f64>,
    all_candidates: &mut RoaringBitmap,

    ranking_rule_universes: &mut [RoaringBitmap],
    ranking_rules: &mut [BoxRankingRule<'ctx, Q>],
    cur_ranking_rule_index: usize,

    cur_offset: &mut usize,
    distinct_fid: Option<u16>,
    candidates: RoaringBitmap,
    score: f64,
) -> Result<()> {
    // First apply the distinct rule on the candidates, reducing the universes if necessary
    let candidates = if let Some(distinct_fid) = distinct_fid {
        let DistinctOutput { remaining, excluded } =
            apply_distinct_rule(ctx, distinct_fid, &candidates)?;
        for universe in ranking_rule_universes.iter_mut() {
            *universe -= &excluded;
            *all_candidates -= &excluded;
        }
        remaining
    } else {
        candidates.clone()
    };
    *all_candidates |= &candidates;

    // if the candidates are empty, there is nothing to do;
    if candidates.is_empty() {
        return Ok(());
    }

    // if we still haven't reached the first document to return
    if *cur_offset < from {
        // and if no document from this bucket can be returned
        if *cur_offset + (candidates.len() as usize) < from {
            // then just skip the bucket
            logger.skip_bucket_ranking_rule(
                cur_ranking_rule_index,
                ranking_rules[cur_ranking_rule_index].as_ref(),
                &candidates,
            );
        } else {
            // otherwise, skip some of the documents and add some of the rest, in order of ids
            let candidates_vec = candidates.iter().collect::<Vec<_>>();
            let (skipped_candidates, candidates) = candidates_vec.split_at(from - *cur_offset);

            logger.skip_bucket_ranking_rule(
                cur_ranking_rule_index,
                ranking_rules[cur_ranking_rule_index].as_ref(),
                &skipped_candidates.iter().collect(),
            );
            let candidates =
                candidates.iter().take(length - valid_docids.len()).copied().collect::<Vec<_>>();
            logger.add_to_results(&candidates);
            valid_docids.extend_from_slice(&candidates);
            valid_scores.extend(std::iter::repeat(score).take(candidates.len()))
        }
    } else {
        // if we have passed the offset already, add some of the documents (up to the limit)
        let candidates = candidates.iter().take(length - valid_docids.len()).collect::<Vec<u32>>();
        logger.add_to_results(&candidates);
        valid_docids.extend_from_slice(&candidates);
        valid_scores.extend(std::iter::repeat(score).take(candidates.len()))
    }

    *cur_offset += candidates.len() as usize;
    Ok(())
}
