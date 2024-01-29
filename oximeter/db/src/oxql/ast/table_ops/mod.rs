// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! AST nodes for table operations.

// Copyright 2024 Oxide Computer Company

pub mod filter;
pub mod get;
pub mod group_by;
pub mod join;

use self::filter::Filter;
use self::group_by::GroupBy;
use self::join::Join;
use crate::oxql::ast::Query;
use crate::oxql::Error;
use crate::oxql::Table;
use oximeter::TimeseriesName;

/// A basic table operation, the atoms of an OxQL query.
#[derive(Clone, Debug, PartialEq)]
pub enum BasicTableOp {
    Get(TimeseriesName),
    Filter(Filter),
    GroupBy(GroupBy),
    Join(Join),
}

impl BasicTableOp {
    pub(crate) fn apply(&self, tables: &[Table]) -> Result<Vec<Table>, Error> {
        match self {
            BasicTableOp::Get(_) => panic!("Should not apply get table ops"),
            BasicTableOp::Filter(f) => f.apply(tables),
            BasicTableOp::GroupBy(g) => g.apply(tables),
            BasicTableOp::Join(j) => j.apply(tables),
        }
    }
}

/// A grouped table operation is a subquery in OxQL.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupedTableOp {
    pub ops: Vec<Query>,
}

/// Any kind of OxQL table operation.
#[derive(Clone, Debug, PartialEq)]
pub enum TableOp {
    Basic(BasicTableOp),
    Grouped(GroupedTableOp),
}

impl TableOp {
    // Is this a merging table operation, one which takes 2 or more tables, and
    // produces 1.
    pub(crate) fn is_merge(&self) -> bool {
        matches!(self, TableOp::Basic(BasicTableOp::Join(_)))
    }

    pub(crate) fn apply(&self, result: &[Table]) -> Result<Vec<Table>, Error> {
        let TableOp::Basic(basic) = self else {
            panic!("Should not apply grouped table ops");
        };
        basic.apply(result)
    }
}
