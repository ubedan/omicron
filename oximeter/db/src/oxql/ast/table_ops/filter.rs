// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An AST node describing filtering table operations.

// Copyright 2024 Oxide Computer Company

use crate::oxql::ast::cmp::Comparison;
use crate::oxql::ast::ident::Ident;
use crate::oxql::ast::literal::Literal;
use crate::oxql::ast::logical_op::LogicalOp;
use crate::oxql::point::Points;
use crate::oxql::point::ValueArray;
use crate::oxql::Error;
use crate::oxql::Table;
use crate::oxql::Timeseries;
use chrono::DateTime;
use chrono::Utc;
use oximeter::FieldType;
use oximeter::FieldValue;
use regex::Regex;
use std::fmt;

/// An AST node for the `filter` table operation.
///
/// This can be a simple operation like `foo == "bar"` or a more complex
/// expression, such as: `filter hostname == "foo" || (hostname == "bar"
/// && id == "baz")`.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    /// An individual filter atom, like `foo == "bar"`.
    Atom(FilterAtom),
    /// A filtering expression combining multiple filters.
    Expr(FilterExpr),
}

impl Filter {
    // Apply the filter to the provided field.
    fn filter_field(
        &self,
        name: &str,
        value: &FieldValue,
    ) -> Result<bool, Error> {
        match self {
            Filter::Atom(atom) => atom.filter_field(name, value),
            Filter::Expr(expr) => expr.filter_field(name, value),
        }
    }

    // Apply the filter to the provided points.
    fn filter_points2(&self, points2: &Points) -> Result<Points, Error> {
        let to_keep = self.filter_points2_inner(points2)?;
        points2.filter(to_keep)
    }

    // Inner implementation of filtering points.
    //
    // Returns an array of bools, where true indicates the point should be kept.
    fn filter_points2_inner(
        &self,
        points2: &Points,
    ) -> Result<Vec<bool>, Error> {
        match self {
            Filter::Atom(atom) => atom.filter_points2(points2),
            Filter::Expr(expr) => expr.filter_points2(points2),
        }
    }

    // Apply the filtering table operation.
    pub(crate) fn apply(&self, tables: &[Table]) -> Result<Vec<Table>, Error> {
        anyhow::ensure!(
            tables.len() == 1,
            "Filtering operations require exactly one table",
        );
        let table = tables.first().unwrap();

        let mut timeseries = Vec::with_capacity(table.len());
        'timeseries: for input in table.iter() {
            // If the filter restricts any of the fields, remove this
            // timeseries altogether.
            for (name, value) in input.fields.iter() {
                if !self.filter_field(name, value)? {
                    continue 'timeseries;
                }
            }

            // Apply the filter to the data points as well.
            let points2 = self.filter_points2(&input.points)?;
            timeseries.push(Timeseries {
                fields: input.fields.clone(),
                points: points2,
            })
        }
        Ok(vec![Table::from_timeseries(table.name(), timeseries.into_iter())?])
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Filter::Atom(inner) => write!(f, "{}", inner),
            Filter::Expr(inner) => write!(f, "{}", inner),
        }
    }
}

/// A more complicated expression as part of a filtering operation.
///
/// E.g., the `hostname == "bar" && id == "baz"` in the below.
// NOTE: This should really be extended to a generic binary op expression.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterExpr {
    pub left: Box<Filter>,
    pub op: LogicalOp,
    pub right: Box<Filter>,
}

impl FilterExpr {
    // Apply the filter to the provided field.
    fn filter_field(
        &self,
        name: &str,
        value: &FieldValue,
    ) -> Result<bool, Error> {
        let left = self.left.filter_field(name, value)?;
        let right = self.right.filter_field(name, value)?;
        match self.op {
            LogicalOp::And => Ok(left && right),
            LogicalOp::Or => Ok(left || right),
        }
    }

    // Apply the filter to the provided points.
    fn filter_points2(&self, points2: &Points) -> Result<Vec<bool>, Error> {
        let mut left = self.left.filter_points2_inner(points2)?;
        let right = self.right.filter_points2_inner(points2)?;
        match self.op {
            LogicalOp::And => {
                for i in 0..left.len() {
                    left[i] &= right[i];
                }
                Ok(left)
            }
            LogicalOp::Or => {
                for i in 0..left.len() {
                    left[i] |= right[i];
                }
                Ok(left)
            }
        }
    }
}

impl fmt::Display for FilterExpr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "({} {} {})", self.left, self.op, self.right)
    }
}

/// An atom of a filtering expression.
///
/// E.g, the `hostname == "foo"` in the below.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterAtom {
    pub negated: bool,
    pub ident: Ident,
    pub cmp: Comparison,
    pub expr: Literal,
}

impl FilterAtom {
    // Apply this filter to the provided field.
    //
    // If the field name does not match the identifier in `self`, return `true`,
    // since this filter does not apply to the provided field.
    //
    // If the name matches and the type of `self` is compatible, return `Ok(x)`
    // where `x` is the logical application of the filter to the field.
    //
    // If the field matches the name, but the type is not compatible, return an
    // error.
    fn filter_field(
        &self,
        name: &str,
        value: &FieldValue,
    ) -> Result<bool, Error> {
        // If the name matches, this filter does _not_ apply, and so we do not
        // filter the field.
        if self.ident.as_str() != name {
            return Ok(true);
        }
        self.expr
            .compare_field(value, self.cmp)
            .map(|res| if self.negated { !res } else { res })
            .ok_or_else(|| {
                anyhow::anyhow!(
            "Filter matches the field named '{}', but the expression type \
            is not compatible, or cannot be applied",
            name,
        )
            })
    }

    pub(crate) fn expr_type_is_compatible_with_field(
        &self,
        field_type: FieldType,
    ) -> bool {
        self.expr.is_compatible_with_field(field_type)
    }

    pub(crate) fn as_db_safe_string(&self) -> String {
        let not = if self.negated { " NOT " } else { "" };
        let expr = self.expr.as_db_safe_string();
        let fn_name = self.cmp.as_db_function_name();
        format!("{}{}({}, {})", not, fn_name, self.ident, expr)
    }

    // Returns an array of bools, where true indicates the point should be kept.
    fn filter_points2(&self, points2: &Points) -> Result<Vec<bool>, Error> {
        let ident = self.ident.as_str();
        if ident == "timestamp" {
            self.filter_points_by_timestamp(&points2.timestamps)
        } else if ident == "datum" {
            anyhow::ensure!(
                points2.dimensionality() == 1,
                "Filtering multidimensional values by datum is not yet supported"
            );
            self.filter_points_by_datum(points2.values(0).unwrap())
        } else {
            Ok(vec![true; points2.len()])
        }
    }

    fn filter_points_by_timestamp(
        &self,
        timestamps: &[DateTime<Utc>],
    ) -> Result<Vec<bool>, Error> {
        let Literal::Timestamp(timestamp) = &self.expr else {
            anyhow::bail!(
                "Cannot compare non-timestamp filter against a timestamp"
            );
        };
        match self.cmp {
            Comparison::Eq => {
                Ok(timestamps.iter().map(|t| t == timestamp).collect())
            }
            Comparison::Ne => {
                Ok(timestamps.iter().map(|t| t != timestamp).collect())
            }
            Comparison::Gt => {
                Ok(timestamps.iter().map(|t| t > timestamp).collect())
            }
            Comparison::Ge => {
                Ok(timestamps.iter().map(|t| t >= timestamp).collect())
            }
            Comparison::Lt => {
                Ok(timestamps.iter().map(|t| t < timestamp).collect())
            }
            Comparison::Le => {
                Ok(timestamps.iter().map(|t| t <= timestamp).collect())
            }
            Comparison::Like => unreachable!(),
        }
    }

    fn filter_points_by_datum(
        &self,
        values: &ValueArray,
    ) -> Result<Vec<bool>, Error> {
        match (&self.expr, values) {
            (Literal::Integer(int), ValueArray::Integer(ints)) => {
                match self.cmp {
                    Comparison::Eq => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) == *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ne => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) != *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Gt => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) > *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ge => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) >= *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Lt => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) < *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Le => Ok(ints
                        .iter()
                        .map(|maybe_int| {
                            maybe_int
                                .map(|i| i128::from(i) <= *int)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Like => unreachable!(),
                }
            }
            (Literal::Double(double), ValueArray::Double(doubles)) => {
                match self.cmp {
                    Comparison::Eq => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d == *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ne => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d != *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Gt => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d > *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ge => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d >= *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Lt => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d < *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Le => Ok(doubles
                        .iter()
                        .map(|maybe_double| {
                            maybe_double.map(|d| d <= *double).unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Like => unreachable!(),
                }
            }
            (Literal::String(string), ValueArray::String(strings)) => {
                let string = string.as_str();
                match self.cmp {
                    Comparison::Eq => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s == string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ne => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s != string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Gt => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s > string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ge => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s >= string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Lt => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s < string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Le => Ok(strings
                        .iter()
                        .map(|maybe_string| {
                            maybe_string
                                .as_deref()
                                .map(|s| s <= string)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Like => {
                        let re = Regex::new(string)?;
                        Ok(strings
                            .iter()
                            .map(|maybe_string| {
                                maybe_string
                                    .as_deref()
                                    .map(|s| re.is_match(s))
                                    .unwrap_or(false)
                            })
                            .collect())
                    }
                }
            }
            (Literal::Boolean(boolean), ValueArray::Boolean(booleans)) => {
                match self.cmp {
                    Comparison::Eq => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| b == *boolean)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ne => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| b != *boolean)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Gt => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| b & !(*boolean))
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Ge => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| b >= *boolean)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Lt => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| !b & *boolean)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Le => Ok(booleans
                        .iter()
                        .map(|maybe_boolean| {
                            maybe_boolean
                                .map(|b| b <= *boolean)
                                .unwrap_or(false)
                        })
                        .collect()),
                    Comparison::Like => unreachable!(),
                }
            }
            (_, _) => {
                let lit_type = match &self.expr {
                    Literal::Uuid(_) => "UUID",
                    Literal::Duration(_) => "duration",
                    Literal::Timestamp(_) => "timestamp",
                    Literal::IpAddr(_) => "IP address",
                    Literal::Integer(_) => "integer",
                    Literal::Double(_) => "double",
                    Literal::String(_) => "string",
                    Literal::Boolean(_) => "boolean",
                };
                anyhow::bail!(
                    "Cannot compare {} literal against values of type {}",
                    lit_type,
                    values.data_type(),
                )
            }
        }
    }
}

impl fmt::Display for FilterAtom {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let bang = if self.negated { "!" } else { "" };
        write!(f, "{}({} {} {})", bang, self.ident, self.cmp, self.expr,)
    }
}

#[cfg(test)]
mod tests {
    use crate::oxql::ast::grammar::query_parser;
    use crate::oxql::point::MetricType;
    use crate::oxql::point::Points;
    use crate::oxql::point::ValueArray;
    use crate::oxql::point::Values;
    use chrono::Utc;
    use std::time::Duration;

    #[test]
    fn test_atom_filter_double_points() {
        let start_times = None;
        let timestamps =
            vec![Utc::now(), Utc::now() + Duration::from_secs(1000)];
        let values = vec![Values {
            values: ValueArray::Double(vec![Some(0.0), Some(2.0)]),
            metric_type: MetricType::Gauge,
        }];
        let points = Points { start_times, timestamps, values };

        // This filter should remove the first point based on its timestamp.
        let t = Utc::now() + Duration::from_secs(10);
        let q =
            format!("filter timestamp > @{}", t.format("%Y-%m-%dT%H:%M:%S"));
        let filter = query_parser::filter(q.as_str()).unwrap();
        let out = filter.filter_points2(&points).unwrap();
        assert!(out.len() == 1);
        assert_eq!(
            out.values(0).unwrap().as_double().unwrap()[0],
            points.values(0).unwrap().as_double().unwrap()[1],
        );

        // And this one the second point based on the datum
        let filter = query_parser::filter("filter datum < 1.0").unwrap();
        let out = filter.filter_points2(&points).unwrap();
        assert!(out.len() == 1);
        assert_eq!(
            out.values(0).unwrap().as_double().unwrap()[0],
            points.values(0).unwrap().as_double().unwrap()[0],
        );
    }

    #[test]
    fn test_atom_filter_points_wrong_type() {
        let start_times = None;
        let timestamps =
            vec![Utc::now(), Utc::now() + Duration::from_secs(1000)];
        let values = vec![Values {
            values: ValueArray::Double(vec![Some(0.0), Some(2.0)]),
            metric_type: MetricType::Gauge,
        }];
        let points = Points { start_times, timestamps, values };

        let filter =
            query_parser::filter("filter datum < \"something\"").unwrap();
        assert!(filter.filter_points2(&points).is_err());
    }
}
