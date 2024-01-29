// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A single OxQL query.

// Copyright 2024 Oxide Computer Company

use super::ast::ident::Ident;
use super::ast::logical_op::LogicalOp;
use super::ast::table_ops::filter::FilterExpr;
use super::ast::table_ops::group_by::GroupBy;
use super::ast::table_ops::BasicTableOp;
use super::ast::table_ops::TableOp;
use super::ast::SplitQuery;
use crate::oxql::ast::grammar;
use crate::oxql::ast::table_ops::filter::Filter;
use crate::oxql::ast::Query as QueryNode;
use crate::oxql::fmt_parse_error;
use crate::oxql::Error;
use crate::TimeseriesName;

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub(super) parsed: QueryNode,
}

impl Query {
    /// Construct a query written in OxQL.
    pub fn new(query: impl AsRef<str>) -> Result<Self, Error> {
        let raw = query.as_ref().trim();
        const MAX_LEN: usize = 4096;
        anyhow::ensure!(
            raw.len() <= MAX_LEN,
            "Queries must be <= {} characters",
            MAX_LEN,
        );
        match grammar::query_parser::query(raw) {
            Ok(parsed) => Ok(Self { parsed }),
            Err(e) => Err(fmt_parse_error(raw, e)),
        }
    }

    /// Return the next referenced timeseries name.
    ///
    /// Queries always start with either a single `get` operation, which refers
    /// to one timeseries; or a subquery, each component of which is a query. So
    /// it is always true that there is exactly one next timeseries name, since
    /// that comes from the current query, or the next subquery.
    pub fn timeseries_name(&self) -> &TimeseriesName {
        self.parsed.timeseries_name()
    }

    /// Return the transformation table ops, i.e., everything after the initial
    /// get operation or subquery.
    pub fn transformations(&self) -> &[TableOp] {
        self.parsed.transformations()
    }

    /// Return the set of all predicates in the query, coalesced.
    ///
    /// Query optimization is a large topic. There are few rules, and many
    /// heuristics. However, one of those is extremely useful for our case:
    /// predicate pushdown. This is where one moves predicates as close as
    /// possible to the data, filtering out unused data as early as possible in
    /// query processing.
    ///
    /// In our case, _currently_, we can implement this pretty easily. Filtering
    /// operations can usually be coalesced into a single item. That means:
    ///
    /// - successive filtering operations are merged: `filter a | filter b ->
    /// `filter (a) && (b)`.
    /// - filtering operations are "pushed down", to just after the initial
    /// `get` operation in the query.
    ///
    /// # Group by
    ///
    /// While filters can be combined and pushed down through many operations,
    /// special care is taken for `group_by`. Specifically, the filter must only
    /// name columns explicitly named in the `group_by`. If we pushed through
    /// filters which named one of the columns _within_ the group (one not
    /// named), then that would change the set of data in a group, and thus the
    /// result.
    ///
    /// # Datum filters
    ///
    /// We currently only push down filters on the timestamps, and that is only
    /// because we do _not_ support aggregations across time, only values. If
    /// and when we do support that, then filters which reference time also
    /// cannot be pushed down.
    ///
    /// # No predicates
    ///
    /// Note that this may return `None`, in the case where there are zero
    /// predicates of any kind.
    //
    // Pushing filters through a group by. Consider the following data:
    //
    // a    b   timestamp   datum
    // 0    0   0           0
    // 0    0   1           1
    // 0    1   0           2
    // 0    1   1           3
    // 1    0   0           4
    // 1    0   1           5
    // 1    1   0           6
    // 1    1   1           7
    //
    // So there are two groups for a and b columns each with two samples.
    //
    // Consider `get a:b | group_by [a] | filter a == 0`.
    //
    // After the group by, the result is:
    //
    // a        timestamp   datum
    // 0        0           avg([0, 2]) -> 1
    // 0        1           avg([1, 3]) -> 2
    // 1        0           avg([4, 6]) -> 5
    // 1        1           avg([5, 7]) -> 6
    //
    // Then after the filter, it becomes:
    //
    // a        timestamp   datum
    // 0        0           avg([0, 2]) -> 1
    // 0        1           avg([1, 3]) -> 2
    //
    // Now, let's do the filter first, as if we pushed that down.
    // i.e., `get a:b | filter a == 0 | group_by [a]`. After the filter, we get:
    //
    // a    b   timestamp   datum
    // 0    0   0           0
    // 0    0   1           1
    // 0    1   0           2
    // 0    1   1           3
    //
    // Then we apply the group by:
    //
    // a        timestamp   datum
    // 0        0           avg([0, 2]) -> 1
    // 0        1           avg([1, 3]) -> 2
    //
    // So we get the same result. Let's suppose we had a filter on the column
    // `b` instead. Doing the group_by first, we get the exact same result as
    // the first one above. Or we really get an error, because the resulting
    // table does not have a `b` column.
    //
    // If instead we did the filter first, we'd get a different result. Starting
    // from:
    //
    // a    b   timestamp   datum
    // 0    0   0           0
    // 0    0   1           1
    // 0    1   0           2
    // 0    1   1           3
    // 1    0   0           4
    // 1    0   1           5
    // 1    1   0           6
    // 1    1   1           7
    //
    // Apply `filter b == 0`:
    //
    //
    // a    b   timestamp   datum
    // 0    0   0           0
    // 0    0   1           1
    // 1    0   0           4
    // 1    0   1           5
    //
    // Then apply group_by [a]
    //
    // a        timestamp   datum
    // 0        0           avg([0, 1]) -> 0.5
    // 0        1           avg([4, 5]) -> 4.5
    //
    // So we get something very different.
    //
    // What about filtering by timestamp? Starting from the raw data again:
    //
    // a    b   timestamp   datum
    // 0    0   0           0
    // 0    0   1           1
    // 0    1   0           2
    // 0    1   1           3
    // 1    0   0           4
    // 1    0   1           5
    // 1    1   0           6
    // 1    1   1           7
    //
    // Let's add a `filter timestamp >= 1`. After the `group_by [a]`, we get:
    //
    // a        timestamp   datum
    // 0        0           avg([0, 2]) -> 1
    // 0        1           avg([1, 3]) -> 2
    // 1        0           avg([4, 6]) -> 5
    // 1        1           avg([5, 7]) -> 6
    //
    // Then after `filter timestamp >= 1`:
    //
    // a        timestamp   datum
    // 0        1           avg([1, 3]) -> 2
    // 1        1           avg([5, 7]) -> 6
    //
    // Now, filtering the timestamps first, after that we get:
    //
    // a    b   timestamp   datum
    // 0    0   1           1
    // 0    1   1           3
    // 1    0   1           5
    // 1    1   1           7
    //
    // Then grouping:
    //
    // a        timestamp   datum
    // 0        1           avg([1, 3]) -> 2
    // 1        1           avg([5, 7]) -> 6
    //
    // So that also works fine.
    pub fn coalesced_predicates(&self) -> Option<Filter> {
        self.transformations().iter().rev().fold(
            None,
            |maybe_filter, next_tr| {
                // Transformations only return basic ops, since all the
                // subqueries must be at the prefix of the query.
                let TableOp::Basic(op) = next_tr else {
                    unreachable!();
                };

                match op {
                    BasicTableOp::GroupBy(GroupBy { identifiers, .. }) => {
                        // Only push through columns referred to in the group by
                        // itself, which replaces the current filter.
                        maybe_filter.as_ref().and_then(|current| {
                            restrict_filter_idents(current, identifiers)
                        })
                    }
                    BasicTableOp::Filter(filter) => {
                        // Merge with any existing filter.
                        if let Some(left) = maybe_filter {
                            Some(Filter::Expr(FilterExpr {
                                left: Box::new(left),
                                op: LogicalOp::And,
                                right: Box::new(filter.clone()),
                            }))
                        } else {
                            Some(filter.clone())
                        }
                    }
                    _ => maybe_filter,
                }
            },
        )
    }

    pub(crate) fn split(&self) -> SplitQuery {
        self.parsed.split()
    }
}

// Return a new filter containing only parts that refer to either:
//
// - a `timestamp` column
// - a column listed in `identifiers`
fn restrict_filter_idents(
    current_filter: &Filter,
    identifiers: &[Ident],
) -> Option<Filter> {
    match current_filter {
        Filter::Atom(atom) => {
            let ident = atom.ident.as_str();
            if ident == "timestamp"
                || identifiers.iter().map(Ident::as_str).any(|id| id == ident)
            {
                Some(current_filter.clone())
            } else {
                None
            }
        }
        Filter::Expr(FilterExpr { left, op, right }) => {
            let maybe_left = restrict_filter_idents(left, identifiers);
            let maybe_right = restrict_filter_idents(right, identifiers);
            match (maybe_left, maybe_right) {
                (Some(left), Some(right)) => Some(Filter::Expr(FilterExpr {
                    left: Box::new(left),
                    op: *op,
                    right: Box::new(right),
                })),
                (Some(single), None) | (None, Some(single)) => Some(single),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Filter;
    use super::Ident;
    use super::Query;
    use crate::oxql::ast::cmp::Comparison;
    use crate::oxql::ast::literal::Literal;
    use crate::oxql::ast::logical_op::LogicalOp;
    use crate::oxql::ast::table_ops::filter::FilterAtom;
    use crate::oxql::ast::table_ops::filter::FilterExpr;
    use crate::oxql::ast::table_ops::join::Join;
    use crate::oxql::ast::table_ops::BasicTableOp;
    use crate::oxql::ast::table_ops::TableOp;
    use crate::oxql::ast::SplitQuery;
    use crate::oxql::query::restrict_filter_idents;

    #[test]
    fn test_restrict_filter_idents_single_atom() {
        let ident = Ident("foo".into());
        let filter = Filter::Atom(FilterAtom {
            negated: false,
            ident: ident.clone(),
            cmp: Comparison::Eq,
            expr: Literal::Boolean(false),
        });
        assert_eq!(
            restrict_filter_idents(&filter, &[ident.clone()]).unwrap(),
            filter
        );
        assert_eq!(restrict_filter_idents(&filter, &[]), None);
    }

    #[test]
    fn test_restrict_filter_idents_single_atom_with_timestamp() {
        let filter = Filter::Atom(FilterAtom {
            negated: false,
            ident: Ident("timestamp".into()),
            cmp: Comparison::Eq,
            expr: Literal::Boolean(false),
        });
        assert_eq!(restrict_filter_idents(&filter, &[]).unwrap(), filter);
    }

    #[test]
    fn test_restrict_filter_idents_expr() {
        let idents = [Ident("foo".into()), Ident("bar".into())];
        let left = Filter::Atom(FilterAtom {
            negated: false,
            ident: idents[0].clone(),
            cmp: Comparison::Eq,
            expr: Literal::Boolean(false),
        });
        let right = Filter::Atom(FilterAtom {
            negated: false,
            ident: idents[1].clone(),
            cmp: Comparison::Eq,
            expr: Literal::Boolean(false),
        });
        let filter = Filter::Expr(FilterExpr {
            left: Box::new(left.clone()),
            op: LogicalOp::And,
            right: Box::new(right.clone()),
        });
        assert_eq!(restrict_filter_idents(&filter, &idents).unwrap(), filter);

        // This should remove the right filter.
        assert_eq!(
            restrict_filter_idents(&filter, &idents[..1]).unwrap(),
            left
        );

        // And both
        assert_eq!(restrict_filter_idents(&filter, &[]), None);
    }

    #[test]
    fn test_split_query() {
        let q = Query::new("get a:b").unwrap();
        let split = q.split();
        assert_eq!(split, SplitQuery::Flat(q));

        let q = Query::new("get a:b | filter x == 0").unwrap();
        let split = q.split();
        assert_eq!(split, SplitQuery::Flat(q));

        let q = Query::new("{ get a:b } | join").unwrap();
        let split = q.split();
        assert_eq!(
            split,
            SplitQuery::Subquery {
                subqueries: vec![Query::new("get a:b").unwrap()],
                transformations: vec![TableOp::Basic(BasicTableOp::Join(Join))],
            }
        );

        let q = Query::new("{ get a:b | filter x == 0 } | join").unwrap();
        let split = q.split();
        assert_eq!(
            split,
            SplitQuery::Subquery {
                subqueries: vec![Query::new("get a:b | filter x == 0").unwrap()],
                transformations: vec![TableOp::Basic(BasicTableOp::Join(Join))],
            }
        );

        let q = Query::new("{ get a:b ; get a:b } | join").unwrap();
        let split = q.split();
        assert_eq!(
            split,
            SplitQuery::Subquery {
                subqueries: vec![Query::new("get a:b").unwrap(); 2],
                transformations: vec![TableOp::Basic(BasicTableOp::Join(Join))],
            }
        );

        let q = Query::new("{ { get a:b ; get a:b } | join } | join").unwrap();
        let split = q.split();
        let subqueries =
            vec![Query::new("{ get a:b; get a:b } | join").unwrap()];
        let expected = SplitQuery::Subquery {
            subqueries: subqueries.clone(),
            transformations: vec![TableOp::Basic(BasicTableOp::Join(Join))],
        };
        assert_eq!(split, expected);
        let split = subqueries[0].split();
        assert_eq!(
            split,
            SplitQuery::Subquery {
                subqueries: vec![Query::new("get a:b").unwrap(); 2],
                transformations: vec![TableOp::Basic(BasicTableOp::Join(Join))],
            }
        );
    }

    #[test]
    fn test_coalesce_predicates() {
        // Passed through group-by unchanged.
        let q = Query::new("get a:b | group_by [a] | filter a == 0").unwrap();
        let preds = Filter::Atom(FilterAtom {
            negated: false,
            ident: Ident("a".to_string()),
            cmp: Comparison::Eq,
            expr: Literal::Integer(0),
        });
        assert_eq!(q.coalesced_predicates(), Some(preds));

        // Merge the first two, then pass through group by.
        let q = Query::new(
            "get a:b | group_by [a] | filter a == 0 | filter a == 0",
        )
        .unwrap();
        let atom = Filter::Atom(FilterAtom {
            negated: false,
            ident: Ident("a".to_string()),
            cmp: Comparison::Eq,
            expr: Literal::Integer(0),
        });
        let preds = Filter::Expr(FilterExpr {
            left: Box::new(atom.clone()),
            op: LogicalOp::And,
            right: Box::new(atom.clone()),
        });
        assert_eq!(q.coalesced_predicates(), Some(preds));

        // These are also merged, even though they're on different sides of the
        // group by.
        let q = Query::new(
            "get a:b | filter a == 0 | group_by [a] | filter a == 0",
        )
        .unwrap();
        let atom = Filter::Atom(FilterAtom {
            negated: false,
            ident: Ident("a".to_string()),
            cmp: Comparison::Eq,
            expr: Literal::Integer(0),
        });
        let preds = Filter::Expr(FilterExpr {
            left: Box::new(atom.clone()),
            op: LogicalOp::And,
            right: Box::new(atom.clone()),
        });
        assert_eq!(q.coalesced_predicates(), Some(preds));

        // Second filter is _not_ passed through, because it refers to columns
        // not in the group by. We have only the first filter.
        let q = Query::new(
            "get a:b | filter a == 0 | group_by [a] | filter b == 0",
        )
        .unwrap();
        let preds = Filter::Atom(FilterAtom {
            negated: false,
            ident: Ident("a".to_string()),
            cmp: Comparison::Eq,
            expr: Literal::Integer(0),
        });
        assert_eq!(q.coalesced_predicates(), Some(preds));
    }
}
