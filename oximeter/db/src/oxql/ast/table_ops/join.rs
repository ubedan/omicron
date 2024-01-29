// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An AST node describing join table operations.

// Copyright 2024 Oxide Computer Company

use crate::oxql::Error;
use crate::oxql::Table;

/// An AST node for a natural inner join.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Join;
impl Join {
    // Apply the group_by table operation.
    pub(crate) fn apply(&self, tables: &[Table]) -> Result<Vec<Table>, Error> {
        anyhow::ensure!(
            tables.len() > 1,
            "Join operations require more than one table",
        );
        let mut tables = tables.iter().cloned().enumerate();
        let (_, mut out) = tables.next().unwrap();
        for (i, next_table) in tables {
            let name = next_table.name().to_string();
            for next_timeseries in next_table.into_iter() {
                let key = next_timeseries.key();
                let Some(timeseries) = out.iter_mut().find(|t| t.key() == key)
                else {
                    anyhow::bail!(
                        "Join failed, input table {} does not \
                        contain a timeseries with key {}",
                        i,
                        key,
                    );
                };

                // Join the timeseries, by stacking together the points.
                //
                // TODO-correctness: This really requires aligned timeseries, or
                // at least interpolation of some kind. We're just blinding
                // stacking indices, not timepoints.
                timeseries.points.values.extend(next_timeseries.points.values);
            }
            // We'll also update the name, to indicate the joined data.
            out.name.push(',');
            out.name.push_str(&name);
        }
        Ok(vec![out])
    }
}
