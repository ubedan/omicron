// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::DataStore;
use crate::authz;
use crate::context::OpContext;
use crate::db;
use crate::db::error::public_error_from_diesel;
use crate::db::error::public_error_from_diesel_lookup;
use crate::db::error::ErrorHandler;
use crate::db::queries::ALLOW_FULL_TABLE_SCAN_SQL;
use crate::db::TransactionError;
use anyhow::Context;
use async_bb8_diesel::AsyncConnection;
use async_bb8_diesel::AsyncRunQueryDsl;
use async_bb8_diesel::AsyncSimpleConnection;
use diesel::expression::SelectableHelper;
use diesel::sql_types::Nullable;
use diesel::BoolExpressionMethods;
use diesel::ExpressionMethods;
use diesel::IntoSql;
use diesel::JoinOnDsl;
use diesel::NullableExpressionMethods;
use diesel::OptionalExtension;
use diesel::QueryDsl;
use diesel::Table;
use futures::future::BoxFuture;
use futures::FutureExt;
use nexus_db_model::CabooseWhichEnum;
use nexus_db_model::HwBaseboardId;
use nexus_db_model::HwPowerState;
use nexus_db_model::HwPowerStateEnum;
use nexus_db_model::HwRotSlot;
use nexus_db_model::HwRotSlotEnum;
use nexus_db_model::InvCaboose;
use nexus_db_model::InvCollection;
use nexus_db_model::InvCollectionError;
use nexus_db_model::InvOmicronZone;
use nexus_db_model::InvOmicronZoneNic;
use nexus_db_model::InvRootOfTrust;
use nexus_db_model::InvRotPage;
use nexus_db_model::InvServiceProcessor;
use nexus_db_model::InvSledAgent;
use nexus_db_model::InvSledOmicronZones;
use nexus_db_model::RotPageWhichEnum;
use nexus_db_model::SledRole;
use nexus_db_model::SledRoleEnum;
use nexus_db_model::SpType;
use nexus_db_model::SpTypeEnum;
use nexus_db_model::SqlU16;
use nexus_db_model::SqlU32;
use nexus_db_model::SwCaboose;
use nexus_db_model::SwRotPage;
use nexus_types::inventory::BaseboardId;
use nexus_types::inventory::Collection;
use omicron_common::api::external::Error;
use omicron_common::api::external::InternalContext;
use omicron_common::api::external::LookupType;
use omicron_common::api::external::ResourceType;
use omicron_common::bail_unless;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;
use uuid::Uuid;

impl DataStore {
    /// Store a complete inventory collection into the database
    pub async fn inventory_insert_collection(
        &self,
        opctx: &OpContext,
        collection: &Collection,
    ) -> Result<(), Error> {
        opctx.authorize(authz::Action::Modify, &authz::INVENTORY).await?;

        // In the database, the collection is represented essentially as a tree
        // rooted at an `inv_collection` row.  Other nodes in the tree point
        // back at the `inv_collection` via `inv_collection_id`.
        //
        // It's helpful to assemble some values before entering the transaction
        // so that we can produce the `Error` type that we want here.
        let row_collection = InvCollection::from(collection);
        let collection_id = row_collection.id;
        let baseboards = collection
            .baseboards
            .iter()
            .map(|b| HwBaseboardId::from((**b).clone()))
            .collect::<Vec<_>>();
        let cabooses = collection
            .cabooses
            .iter()
            .map(|s| SwCaboose::from((**s).clone()))
            .collect::<Vec<_>>();
        let rot_pages = collection
            .rot_pages
            .iter()
            .map(|p| SwRotPage::from((**p).clone()))
            .collect::<Vec<_>>();
        let error_values = collection
            .errors
            .iter()
            .enumerate()
            .map(|(i, message)| {
                let index = u16::try_from(i).map_err(|e| {
                    Error::internal_error(&format!(
                        "failed to convert error index to u16 (too \
                        many errors in inventory collection?): {}",
                        e
                    ))
                })?;
                Ok(InvCollectionError::new(
                    collection_id,
                    index,
                    message.clone(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        // Partition the sled agents into those with an associated baseboard id
        // and those without one.  We handle these pretty differently.
        let (sled_agents_baseboards, sled_agents_no_baseboards): (
            Vec<_>,
            Vec<_>,
        ) = collection
            .sled_agents
            .values()
            .partition(|sled_agent| sled_agent.baseboard_id.is_some());
        let sled_agents_no_baseboards = sled_agents_no_baseboards
            .into_iter()
            .map(|sled_agent| {
                assert!(sled_agent.baseboard_id.is_none());
                InvSledAgent::new_without_baseboard(collection_id, sled_agent)
                    .map_err(|e| Error::internal_error(&e.to_string()))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let sled_omicron_zones = collection
            .omicron_zones
            .values()
            .map(|found| InvSledOmicronZones::new(collection_id, found))
            .collect::<Vec<_>>();
        let omicron_zones = collection
            .omicron_zones
            .values()
            .map(|found| {
                found.zones.zones.iter().map(|found_zone| {
                    InvOmicronZone::new(
                        collection_id,
                        found.sled_id,
                        found_zone,
                    )
                    .map_err(|e| Error::internal_error(&e.to_string()))
                })
            })
            .flatten()
            .collect::<Result<Vec<_>, Error>>()?;
        let omicron_zone_nics = collection
            .omicron_zones
            .values()
            .map(|found| {
                found.zones.zones.iter().filter_map(|found_zone| {
                    InvOmicronZoneNic::new(collection_id, found_zone)
                        .with_context(|| format!("zone {:?}", found_zone.id))
                        .map_err(|e| Error::internal_error(&format!("{:#}", e)))
                        .transpose()
                })
            })
            .flatten()
            .collect::<Result<Vec<InvOmicronZoneNic>, _>>()?;

        // This implementation inserts all records associated with the
        // collection in one transaction.  This is primarily for simplicity.  It
        // means we don't have to worry about other readers seeing a
        // half-inserted collection, nor leaving detritus around if we start
        // inserting records and then crash.  However, it does mean this is
        // likely to be a big transaction and if that becomes a problem we could
        // break this up as long as we address those problems.
        //
        // The SQL here is written so that it doesn't have to be an
        // *interactive* transaction.  That is, it should in principle be
        // possible to generate all this SQL up front and send it as one big
        // batch rather than making a bunch of round-trips to the database.
        // We'd do that if we had an interface for doing that with bound
        // parameters, etc.  See oxidecomputer/omicron#973.
        let pool = self.pool_connection_authorized(opctx).await?;
        pool.transaction_async(|conn| async move {
            // Insert records (and generate ids) for any baseboards that do not
            // already exist in the database.  These rows are not scoped to a
            // particular collection.  They contain only immutable data --
            // they're just a mapping between hardware-provided baseboard
            // identifiers (part number and model number) and an
            // Omicron-specific primary key (a UUID).
            {
                use db::schema::hw_baseboard_id::dsl;
                let _ = diesel::insert_into(dsl::hw_baseboard_id)
                    .values(baseboards)
                    .on_conflict_do_nothing()
                    .execute_async(&conn)
                    .await?;
            }

            // Insert records (and generate ids) for each distinct caboose that
            // we've found.  Like baseboards, these might already be present and
            // rows in this table are not scoped to a particular collection
            // because they only map (immutable) identifiers to UUIDs.
            {
                use db::schema::sw_caboose::dsl;
                let _ = diesel::insert_into(dsl::sw_caboose)
                    .values(cabooses)
                    .on_conflict_do_nothing()
                    .execute_async(&conn)
                    .await?;
            }

            // Insert records (and generate ids) for each distinct RoT page that
            // we've found.  Like baseboards, these might already be present and
            // rows in this table are not scoped to a particular collection
            // because they only map (immutable) identifiers to UUIDs.
            {
                use db::schema::sw_root_of_trust_page::dsl;
                let _ = diesel::insert_into(dsl::sw_root_of_trust_page)
                    .values(rot_pages)
                    .on_conflict_do_nothing()
                    .execute_async(&conn)
                    .await?;
            }

            // Insert a record describing the collection itself.
            {
                use db::schema::inv_collection::dsl;
                let _ = diesel::insert_into(dsl::inv_collection)
                    .values(row_collection)
                    .execute_async(&conn)
                    .await?;
            }

            // Insert rows for the service processors we found.  These have a
            // foreign key into the hw_baseboard_id table.  We don't have those
            // id values, though.  We may have just inserted them, or maybe not
            // (if they previously existed).  To avoid dozens of unnecessary
            // round-trips, we use INSERT INTO ... SELECT, which looks like
            // this:
            //
            //   INSERT INTO inv_service_processor
            //       SELECT
            //           id
            //           [other service_processor column values as literals]
            //         FROM hw_baseboard_id
            //         WHERE part_number = ... AND serial_number = ...;
            //
            // This way, we don't need to know the id.  The database looks it up
            // for us as it does the INSERT.
            {
                use db::schema::hw_baseboard_id::dsl as baseboard_dsl;
                use db::schema::inv_service_processor::dsl as sp_dsl;

                for (baseboard_id, sp) in &collection.sps {
                    let selection = db::schema::hw_baseboard_id::table
                        .select((
                            collection_id.into_sql::<diesel::sql_types::Uuid>(),
                            baseboard_dsl::id,
                            sp.time_collected
                                .into_sql::<diesel::sql_types::Timestamptz>(),
                            sp.source
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            SpType::from(sp.sp_type).into_sql::<SpTypeEnum>(),
                            i32::from(sp.sp_slot)
                                .into_sql::<diesel::sql_types::Int4>(),
                            i64::from(sp.baseboard_revision)
                                .into_sql::<diesel::sql_types::Int8>(),
                            sp.hubris_archive
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            HwPowerState::from(sp.power_state)
                                .into_sql::<HwPowerStateEnum>(),
                        ))
                        .filter(
                            baseboard_dsl::part_number
                                .eq(baseboard_id.part_number.clone()),
                        )
                        .filter(
                            baseboard_dsl::serial_number
                                .eq(baseboard_id.serial_number.clone()),
                        );

                    let _ = diesel::insert_into(
                        db::schema::inv_service_processor::table,
                    )
                    .values(selection)
                    .into_columns((
                        sp_dsl::inv_collection_id,
                        sp_dsl::hw_baseboard_id,
                        sp_dsl::time_collected,
                        sp_dsl::source,
                        sp_dsl::sp_type,
                        sp_dsl::sp_slot,
                        sp_dsl::baseboard_revision,
                        sp_dsl::hubris_archive_id,
                        sp_dsl::power_state,
                    ))
                    .execute_async(&conn)
                    .await?;

                    // This statement is just here to force a compilation error
                    // if the set of columns in `inv_service_processor` changes.
                    // The code above attempts to insert a row into
                    // `inv_service_processor` using an explicit list of columns
                    // and values.  Without the following statement, If a new
                    // required column were added, this would only fail at
                    // runtime.
                    //
                    // If you're here because of a compile error, you might be
                    // changing the `inv_service_processor` table.  Update the
                    // statement below and be sure to update the code above,
                    // too!
                    //
                    // See also similar comments in blocks below, near other
                    // uses of `all_columns().
                    let (
                        _inv_collection_id,
                        _hw_baseboard_id,
                        _time_collected,
                        _source,
                        _sp_type,
                        _sp_slot,
                        _baseboard_revision,
                        _hubris_archive_id,
                        _power_state,
                    ) = sp_dsl::inv_service_processor::all_columns();
                }
            }

            // Insert rows for the roots of trust that we found.  Like service
            // processors, we do this using INSERT INTO ... SELECT.
            {
                use db::schema::hw_baseboard_id::dsl as baseboard_dsl;
                use db::schema::inv_root_of_trust::dsl as rot_dsl;

                for (baseboard_id, rot) in &collection.rots {
                    let selection = db::schema::hw_baseboard_id::table
                        .select((
                            collection_id.into_sql::<diesel::sql_types::Uuid>(),
                            baseboard_dsl::id,
                            rot.time_collected
                                .into_sql::<diesel::sql_types::Timestamptz>(),
                            rot.source
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            HwRotSlot::from(rot.active_slot)
                                .into_sql::<HwRotSlotEnum>(),
                            HwRotSlot::from(rot.persistent_boot_preference)
                                .into_sql::<HwRotSlotEnum>(),
                            rot.pending_persistent_boot_preference
                                .map(HwRotSlot::from)
                                .into_sql::<Nullable<HwRotSlotEnum>>(),
                            rot.transient_boot_preference
                                .map(HwRotSlot::from)
                                .into_sql::<Nullable<HwRotSlotEnum>>(),
                            rot.slot_a_sha3_256_digest
                                .clone()
                                .into_sql::<Nullable<diesel::sql_types::Text>>(
                                ),
                            rot.slot_b_sha3_256_digest
                                .clone()
                                .into_sql::<Nullable<diesel::sql_types::Text>>(
                                ),
                        ))
                        .filter(
                            baseboard_dsl::part_number
                                .eq(baseboard_id.part_number.clone()),
                        )
                        .filter(
                            baseboard_dsl::serial_number
                                .eq(baseboard_id.serial_number.clone()),
                        );

                    let _ = diesel::insert_into(
                        db::schema::inv_root_of_trust::table,
                    )
                    .values(selection)
                    .into_columns((
                        rot_dsl::inv_collection_id,
                        rot_dsl::hw_baseboard_id,
                        rot_dsl::time_collected,
                        rot_dsl::source,
                        rot_dsl::slot_active,
                        rot_dsl::slot_boot_pref_persistent,
                        rot_dsl::slot_boot_pref_persistent_pending,
                        rot_dsl::slot_boot_pref_transient,
                        rot_dsl::slot_a_sha3_256,
                        rot_dsl::slot_b_sha3_256,
                    ))
                    .execute_async(&conn)
                    .await?;

                    // See the comment in the previous block (where we use
                    // `inv_service_processor::all_columns()`).  The same
                    // applies here.
                    let (
                        _inv_collection_id,
                        _hw_baseboard_id,
                        _time_collected,
                        _source,
                        _slot_active,
                        _slot_boot_pref_persistent,
                        _slot_boot_pref_persistent_pending,
                        _slot_boot_pref_transient,
                        _slot_a_sha3_256,
                        _slot_b_sha3_256,
                    ) = rot_dsl::inv_root_of_trust::all_columns();
                }
            }

            // Insert rows for the cabooses that we found.  Like service
            // processors and roots of trust, we do this using INSERT INTO ...
            // SELECT.  This one's a little more complicated because there are
            // two foreign keys.  Concretely, we have these three tables:
            //
            // - `hw_baseboard` with an "id" primary key and lookup columns
            //   "part_number" and "serial_number"
            // - `sw_caboose` with an "id" primary key and lookup columns
            //   "board", "git_commit", "name", and "version"
            // - `inv_caboose` with foreign keys "hw_baseboard_id",
            //   "sw_caboose_id", and various other columns
            //
            // We want to INSERT INTO `inv_caboose` a row with:
            //
            // - hw_baseboard_id (foreign key) the result of looking up an
            //   hw_baseboard row by a specific part number and serial number
            //
            // - sw_caboose_id (foreign key) the result of looking up a
            //   specific sw_caboose row by board, git_commit, name, and version
            //
            // - the other columns being literals
            //
            // To achieve this, we're going to generate something like:
            //
            //     INSERT INTO
            //         inv_caboose (
            //             hw_baseboard_id,
            //             sw_caboose_id,
            //             inv_collection_id,
            //             time_collected,
            //             source,
            //             which,
            //         )
            //         SELECT (
            //             hw_baseboard_id.id,
            //             sw_caboose.id,
            //             ...              /* literal collection id */
            //             ...              /* literal time collected */
            //             ...              /* literal source */
            //             ...              /* literal 'which' */
            //         )
            //         FROM
            //             hw_baseboard
            //         INNER JOIN
            //             sw_caboose
            //         ON  hw_baseboard.part_number = ...
            //         AND hw_baseboard.serial_number = ...
            //         AND sw_caboose.board = ...
            //         AND sw_caboose.git_commit = ...
            //         AND sw_caboose.name = ...
            //         AND sw_caboose.version = ...;
            //
            // Again, the whole point is to avoid back-and-forth between the
            // client and the database.  Those back-and-forth interactions can
            // significantly increase latency and the probability of transaction
            // conflicts.  See RFD 192 for details.  (Unfortunately, we still
            // _are_ going back and forth here to issue each of these queries.
            // But that's an artifact of the interface we currently have for
            // sending queries.  It should be possible to send all of these in
            // one batch.
            for (which, tree) in &collection.cabooses_found {
                let db_which = nexus_db_model::CabooseWhich::from(*which);
                for (baseboard_id, found_caboose) in tree {
                    use db::schema::hw_baseboard_id::dsl as dsl_baseboard_id;
                    use db::schema::inv_caboose::dsl as dsl_inv_caboose;
                    use db::schema::sw_caboose::dsl as dsl_sw_caboose;

                    let selection = db::schema::hw_baseboard_id::table
                        .inner_join(
                            db::schema::sw_caboose::table.on(
                                dsl_baseboard_id::part_number
                                    .eq(baseboard_id.part_number.clone())
                                    .and(
                                        dsl_baseboard_id::serial_number.eq(
                                            baseboard_id.serial_number.clone(),
                                        ),
                                    )
                                    .and(dsl_sw_caboose::board.eq(
                                        found_caboose.caboose.board.clone(),
                                    ))
                                    .and(
                                        dsl_sw_caboose::git_commit.eq(
                                            found_caboose
                                                .caboose
                                                .git_commit
                                                .clone(),
                                        ),
                                    )
                                    .and(
                                        dsl_sw_caboose::name.eq(found_caboose
                                            .caboose
                                            .name
                                            .clone()),
                                    )
                                    .and(dsl_sw_caboose::version.eq(
                                        found_caboose.caboose.version.clone(),
                                    )),
                            ),
                        )
                        .select((
                            dsl_baseboard_id::id,
                            dsl_sw_caboose::id,
                            collection_id.into_sql::<diesel::sql_types::Uuid>(),
                            found_caboose
                                .time_collected
                                .into_sql::<diesel::sql_types::Timestamptz>(),
                            found_caboose
                                .source
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            db_which.into_sql::<CabooseWhichEnum>(),
                        ));

                    let _ = diesel::insert_into(db::schema::inv_caboose::table)
                        .values(selection)
                        .into_columns((
                            dsl_inv_caboose::hw_baseboard_id,
                            dsl_inv_caboose::sw_caboose_id,
                            dsl_inv_caboose::inv_collection_id,
                            dsl_inv_caboose::time_collected,
                            dsl_inv_caboose::source,
                            dsl_inv_caboose::which,
                        ))
                        .execute_async(&conn)
                        .await?;

                    // See the comments above.  The same applies here.  If you
                    // update the statement below because the schema for
                    // `inv_caboose` has changed, be sure to update the code
                    // above, too!
                    let (
                        _hw_baseboard_id,
                        _sw_caboose_id,
                        _inv_collection_id,
                        _time_collected,
                        _source,
                        _which,
                    ) = dsl_inv_caboose::inv_caboose::all_columns();
                }
            }

            // Insert rows for the root of trust pages that we found. This is
            // almost identical to inserting cabooses above, and just like for
            // cabooses, we do this using INSERT INTO ... SELECT. We have these
            // three tables:
            //
            // - `hw_baseboard` with an "id" primary key and lookup columns
            //   "part_number" and "serial_number"
            // - `sw_root_of_trust_page` with an "id" primary key and lookup
            //   column "data_base64"
            // - `inv_root_of_trust_page` with foreign keys "hw_baseboard_id",
            //   "sw_root_of_trust_page_id", and various other columns
            //
            // and generate an INSERT INTO query that is structurally the same
            // as the caboose query described above.
            for (which, tree) in &collection.rot_pages_found {
                use db::schema::hw_baseboard_id::dsl as dsl_baseboard_id;
                use db::schema::inv_root_of_trust_page::dsl as dsl_inv_rot_page;
                use db::schema::sw_root_of_trust_page::dsl as dsl_sw_rot_page;
                let db_which = nexus_db_model::RotPageWhich::from(*which);
                for (baseboard_id, found_rot_page) in tree {
                    let selection = db::schema::hw_baseboard_id::table
                        .inner_join(
                            db::schema::sw_root_of_trust_page::table.on(
                                dsl_baseboard_id::part_number
                                    .eq(baseboard_id.part_number.clone())
                                    .and(
                                        dsl_baseboard_id::serial_number.eq(
                                            baseboard_id.serial_number.clone(),
                                        ),
                                    )
                                    .and(dsl_sw_rot_page::data_base64.eq(
                                        found_rot_page.page.data_base64.clone(),
                                    )),
                            ),
                        )
                        .select((
                            dsl_baseboard_id::id,
                            dsl_sw_rot_page::id,
                            collection_id.into_sql::<diesel::sql_types::Uuid>(),
                            found_rot_page
                                .time_collected
                                .into_sql::<diesel::sql_types::Timestamptz>(),
                            found_rot_page
                                .source
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            db_which.into_sql::<RotPageWhichEnum>(),
                        ));

                    let _ = diesel::insert_into(
                        db::schema::inv_root_of_trust_page::table,
                    )
                    .values(selection)
                    .into_columns((
                        dsl_inv_rot_page::hw_baseboard_id,
                        dsl_inv_rot_page::sw_root_of_trust_page_id,
                        dsl_inv_rot_page::inv_collection_id,
                        dsl_inv_rot_page::time_collected,
                        dsl_inv_rot_page::source,
                        dsl_inv_rot_page::which,
                    ))
                    .execute_async(&conn)
                    .await?;

                    // See the comments above.  The same applies here.  If you
                    // update the statement below because the schema for
                    // `inv_root_of_trust_page` has changed, be sure to update
                    // the code above, too!
                    let (
                        _hw_baseboard_id,
                        _sw_root_of_trust_page_id,
                        _inv_collection_id,
                        _time_collected,
                        _source,
                        _which,
                    ) = dsl_inv_rot_page::inv_root_of_trust_page::all_columns();
                }
            }

            // Insert rows for the sled agents that we found.  In practice, we'd
            // expect these to all have baseboards (if using Oxide hardware) or
            // none have baseboards (if not).
            {
                use db::schema::hw_baseboard_id::dsl as baseboard_dsl;
                use db::schema::inv_sled_agent::dsl as sa_dsl;

                // For sleds with a real baseboard id, we have to use the
                // `INSERT INTO ... SELECT` pattern that we used for other types
                // of rows above to pull in the baseboard id's uuid.
                for sled_agent in &sled_agents_baseboards {
                    let baseboard_id = sled_agent.baseboard_id.as_ref().expect(
                        "already selected only sled agents with baseboards",
                    );
                    let selection = db::schema::hw_baseboard_id::table
                        .select((
                            collection_id.into_sql::<diesel::sql_types::Uuid>(),
                            sled_agent
                                .time_collected
                                .into_sql::<diesel::sql_types::Timestamptz>(),
                            sled_agent
                                .source
                                .clone()
                                .into_sql::<diesel::sql_types::Text>(),
                            sled_agent
                                .sled_id
                                .into_sql::<diesel::sql_types::Uuid>(),
                            baseboard_dsl::id.nullable(),
                            nexus_db_model::ipv6::Ipv6Addr::from(
                                sled_agent.sled_agent_address.ip(),
                            )
                            .into_sql::<diesel::sql_types::Inet>(),
                            SqlU16(sled_agent.sled_agent_address.port())
                                .into_sql::<diesel::sql_types::Int4>(),
                            SledRole::from(sled_agent.sled_role)
                                .into_sql::<SledRoleEnum>(),
                            SqlU32(sled_agent.usable_hardware_threads)
                                .into_sql::<diesel::sql_types::Int8>(),
                            nexus_db_model::ByteCount::from(
                                sled_agent.usable_physical_ram,
                            )
                            .into_sql::<diesel::sql_types::Int8>(),
                            nexus_db_model::ByteCount::from(
                                sled_agent.reservoir_size,
                            )
                            .into_sql::<diesel::sql_types::Int8>(),
                        ))
                        .filter(
                            baseboard_dsl::part_number
                                .eq(baseboard_id.part_number.clone()),
                        )
                        .filter(
                            baseboard_dsl::serial_number
                                .eq(baseboard_id.serial_number.clone()),
                        );

                    let _ =
                        diesel::insert_into(db::schema::inv_sled_agent::table)
                            .values(selection)
                            .into_columns((
                                sa_dsl::inv_collection_id,
                                sa_dsl::time_collected,
                                sa_dsl::source,
                                sa_dsl::sled_id,
                                sa_dsl::hw_baseboard_id,
                                sa_dsl::sled_agent_ip,
                                sa_dsl::sled_agent_port,
                                sa_dsl::sled_role,
                                sa_dsl::usable_hardware_threads,
                                sa_dsl::usable_physical_ram,
                                sa_dsl::reservoir_size,
                            ))
                            .execute_async(&conn)
                            .await?;

                    // See the comment in the earlier block (where we use
                    // `inv_service_processor::all_columns()`).  The same
                    // applies here.
                    let (
                        _inv_collection_id,
                        _time_collected,
                        _source,
                        _sled_id,
                        _hw_baseboard_id,
                        _sled_agent_ip,
                        _sled_agent_port,
                        _sled_role,
                        _usable_hardware_threads,
                        _usable_physical_ram,
                        _reservoir_size,
                    ) = sa_dsl::inv_sled_agent::all_columns();
                }

                // For sleds with no baseboard information, we can't use
                // the same INSERT INTO ... SELECT pattern because we
                // won't find anything in the hw_baseboard_id table.  It
                // sucks that these are bifurcated code paths, but on
                // the plus side, this is a much simpler INSERT, and we
                // can insert all of them in one statement.
                let _ = diesel::insert_into(db::schema::inv_sled_agent::table)
                    .values(sled_agents_no_baseboards)
                    .execute_async(&conn)
                    .await?;
            }

            // Insert all the Omicron zones that we found.
            {
                use db::schema::inv_sled_omicron_zones::dsl as sled_zones;
                let _ = diesel::insert_into(sled_zones::inv_sled_omicron_zones)
                    .values(sled_omicron_zones)
                    .execute_async(&conn)
                    .await?;
            }

            {
                use db::schema::inv_omicron_zone::dsl as omicron_zone;
                let _ = diesel::insert_into(omicron_zone::inv_omicron_zone)
                    .values(omicron_zones)
                    .execute_async(&conn)
                    .await?;
            }

            {
                use db::schema::inv_omicron_zone_nic::dsl as omicron_zone_nic;
                let _ =
                    diesel::insert_into(omicron_zone_nic::inv_omicron_zone_nic)
                        .values(omicron_zone_nics)
                        .execute_async(&conn)
                        .await?;
            }

            // Finally, insert the list of errors.
            {
                use db::schema::inv_collection_error::dsl as errors_dsl;
                let _ = diesel::insert_into(errors_dsl::inv_collection_error)
                    .values(error_values)
                    .execute_async(&conn)
                    .await?;
            }

            Ok(())
        })
        .await
        .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        info!(
            &opctx.log,
            "inserted inventory collection";
            "collection_id" => collection.id.to_string(),
        );

        Ok(())
    }

    /// Prune inventory collections stored in the database, keeping at least
    /// `nkeep`.
    ///
    /// This function removes as many collections as possible while preserving
    /// the last `nkeep`.  This will also preserve at least one "complete"
    /// collection (i.e., one having zero errors).
    // It might seem surprising that such a high-level application policy is
    // embedded in the DataStore.  The reason is that we want to push a bunch of
    // the logic into the SQL to avoid interactive queries.
    pub async fn inventory_prune_collections(
        &self,
        opctx: &OpContext,
        nkeep: u32,
    ) -> Result<(), Error> {
        // Assumptions:
        //
        // - Most of the time, there will be about `nkeep + 1` collections in
        //   the database.  That's because the normal expected case is: we had
        //   `nkeep`, we created another one, and now we're pruning the oldest
        //   one.
        //
        // - There could be fewer collections in the database, early in the
        //   system's lifetime (before we've accumulated `nkeep` of them).
        //
        // - There could be many more collections in the database, if something
        //   has gone wrong and we've fallen behind in our cleanup.
        //
        // - Due to transient errors during the collection process, it's
        //   possible that a collection is known to be potentially incomplete.
        //   We can tell this because it has rows in `inv_collection_errors`.
        //   (It's possible that a collection can be incomplete with zero
        //   errors, but we can't know that here and so we can't do anything
        //   about it.)
        //
        // Goals:
        //
        // - When this function returns without error, there were at most
        //   `nkeep` collections in the database.
        //
        // - If we have to remove any collections, we want to start from the
        //   oldest ones.  (We want to maintain a window of the last `nkeep`,
        //   not the first `nkeep - 1` from the beginning of time plus the most
        //   recent one.)
        //
        // - We want to avoid removing the last collection that had zero errors.
        //   (If we weren't careful, we might do this if there were `nkeep`
        //   collections with errors that were newer than the last complete
        //   collection.)
        //
        // Here's the plan:
        //
        // - Select from the database the `nkeep + 1` oldest collections and the
        //   number of errors associated with each one.
        //
        // - If we got fewer than `nkeep + 1` back, we're done.  We shouldn't
        //   prune anything.
        //
        // - Otherwise, if the oldest collection is the only complete one,
        //   remove the next-oldest collection and go back to the top (repeat).
        //
        // - Otherwise, remove the oldest collection and go back to the top
        //   (repeat).
        //
        // This seems surprisingly complicated.  It's designed to meet the above
        // goals.
        //
        // Is this going to work if multiple Nexuses are doing it concurrently?
        // This cannot remove the last complete collection because a given Nexus
        // will only remove a complete collection if it has seen a newer
        // complete one.  This cannot result in keeping fewer than "nkeep"
        // collections because any Nexus will only remove a collection if there
        // are "nkeep" newer ones.  In both of these cases, another Nexus might
        // remove one of the ones that the first Nexus was counting on keeping,
        // but only if there was a newer one to replace it.

        opctx.authorize(authz::Action::Modify, &authz::INVENTORY).await?;

        loop {
            match self.inventory_find_pruneable(opctx, nkeep).await? {
                None => break,
                Some(collection_id) => {
                    self.inventory_delete_collection(opctx, collection_id)
                        .await?
                }
            }
        }

        Ok(())
    }

    /// Return the oldest inventory collection that's eligible for pruning,
    /// if any
    ///
    /// The caller of this (non-pub) function is responsible for authz.
    async fn inventory_find_pruneable(
        &self,
        opctx: &OpContext,
        nkeep: u32,
    ) -> Result<Option<Uuid>, Error> {
        let conn = self.pool_connection_authorized(opctx).await?;
        // Diesel requires us to use aliases in order to refer to the
        // `inv_collection` table twice in the same query.
        let (inv_collection1, inv_collection2) = diesel::alias!(
            db::schema::inv_collection as inv_collection1,
            db::schema::inv_collection as inv_collection2
        );

        // This subquery essentially generates:
        //
        //    SELECT id FROM inv_collection ORDER BY time_started" ASC LIMIT $1
        //
        // where $1 becomes `nkeep + 1`.  This just lists the `nkeep + 1` oldest
        // collections.
        let subquery = inv_collection1
            .select(inv_collection1.field(db::schema::inv_collection::id))
            .order_by(
                inv_collection1
                    .field(db::schema::inv_collection::time_started)
                    .asc(),
            )
            .limit(i64::from(nkeep) + 1);

        // This essentially generates:
        //
        //     SELECT
        //         inv_collection.id,
        //         count(inv_collection_error.inv_collection_id)
        //     FROM (
        //             inv_collection
        //         LEFT OUTER JOIN
        //             inv_collection_error
        //         ON (
        //             inv_collection_error.inv_collection_id = inv_collection.id
        //         )
        //     ) WHERE (
        //         inv_collection.id = ANY( <<subquery above>> )
        //     )
        //     GROUP BY inv_collection.id
        //     ORDER BY inv_collection.time_started ASC
        //
        // This looks a lot scarier than it is.  The goal is to produce a
        // two-column table that looks like this:
        //
        //     collection_id1     count of errors from collection_id1
        //     collection_id2     count of errors from collection_id2
        //     collection_id3     count of errors from collection_id3
        //     ...
        //
        let candidates: Vec<(Uuid, i64)> = inv_collection2
            .left_outer_join(db::schema::inv_collection_error::table)
            .filter(
                inv_collection2
                    .field(db::schema::inv_collection::id)
                    .eq_any(subquery),
            )
            .group_by(inv_collection2.field(db::schema::inv_collection::id))
            .select((
                inv_collection2.field(db::schema::inv_collection::id),
                diesel::dsl::count(
                    db::schema::inv_collection_error::inv_collection_id
                        .nullable(),
                ),
            ))
            .order_by(
                inv_collection2
                    .field(db::schema::inv_collection::time_started)
                    .asc(),
            )
            .load_async(&*conn)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
            .internal_context("listing oldest collections")?;

        if u32::try_from(candidates.len()).unwrap() <= nkeep {
            debug!(
                &opctx.log,
                "inventory_prune_one: nothing eligible for removal (too few)";
                "candidates" => ?candidates,
            );
            return Ok(None);
        }

        // We've now got up to "nkeep + 1" oldest collections, starting with the
        // very oldest.  We can get rid of the oldest one unless it's the only
        // complete one.  Another way to think about it: find the _last_
        // complete one.  Remove it from the list of candidates.  Now mark the
        // first item in the remaining list for deletion.
        let last_completed_idx = candidates
            .iter()
            .enumerate()
            .rev()
            .find(|(_i, (_collection_id, nerrors))| *nerrors == 0);
        let candidate = match last_completed_idx {
            Some((0, _)) => candidates.iter().skip(1).next(),
            _ => candidates.iter().next(),
        }
        .map(|(collection_id, _nerrors)| *collection_id);
        if let Some(c) = candidate {
            debug!(
                &opctx.log,
                "inventory_prune_one: eligible for removal";
                "collection_id" => c.to_string(),
                "candidates" => ?candidates,
            );
        } else {
            debug!(
                &opctx.log,
                "inventory_prune_one: nothing eligible for removal";
                "candidates" => ?candidates,
            );
        }
        Ok(candidate)
    }

    /// Removes an inventory collection from the database
    ///
    /// The caller of this (non-pub) function is responsible for authz.
    async fn inventory_delete_collection(
        &self,
        opctx: &OpContext,
        collection_id: Uuid,
    ) -> Result<(), Error> {
        // As with inserting a whole collection, we remove it in one big
        // transaction for simplicity.  Similar considerations apply.  We could
        // break it up if these transactions become too big.  But we'd need a
        // way to stop other clients from discovering a collection after we
        // start removing it and we'd also need to make sure we didn't leak a
        // collection if we crash while deleting it.
        let conn = self.pool_connection_authorized(opctx).await?;
        let (
            ncollections,
            nsps,
            nrots,
            ncabooses,
            nrot_pages,
            nsled_agents,
            nsled_agent_zones,
            nzones,
            nnics,
            nerrors,
        ) = conn
            .transaction_async(|conn| async move {
                // Remove the record describing the collection itself.
                let ncollections = {
                    use db::schema::inv_collection::dsl;
                    diesel::delete(
                        dsl::inv_collection.filter(dsl::id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for service processors.
                let nsps = {
                    use db::schema::inv_service_processor::dsl;
                    diesel::delete(
                        dsl::inv_service_processor
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for roots of trust.
                let nrots = {
                    use db::schema::inv_root_of_trust::dsl;
                    diesel::delete(
                        dsl::inv_root_of_trust
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for cabooses found.
                let ncabooses = {
                    use db::schema::inv_caboose::dsl;
                    diesel::delete(
                        dsl::inv_caboose
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for root of trust pages found.
                let nrot_pages = {
                    use db::schema::inv_root_of_trust_page::dsl;
                    diesel::delete(
                        dsl::inv_root_of_trust_page
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for sled agents found.
                let nsled_agents = {
                    use db::schema::inv_sled_agent::dsl;
                    diesel::delete(
                        dsl::inv_sled_agent
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows associated with Omicron zones
                let nsled_agent_zones = {
                    use db::schema::inv_sled_omicron_zones::dsl;
                    diesel::delete(
                        dsl::inv_sled_omicron_zones
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                let nzones = {
                    use db::schema::inv_omicron_zone::dsl;
                    diesel::delete(
                        dsl::inv_omicron_zone
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                let nnics = {
                    use db::schema::inv_omicron_zone_nic::dsl;
                    diesel::delete(
                        dsl::inv_omicron_zone_nic
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                // Remove rows for errors encountered.
                let nerrors = {
                    use db::schema::inv_collection_error::dsl;
                    diesel::delete(
                        dsl::inv_collection_error
                            .filter(dsl::inv_collection_id.eq(collection_id)),
                    )
                    .execute_async(&conn)
                    .await?
                };

                Ok((
                    ncollections,
                    nsps,
                    nrots,
                    ncabooses,
                    nrot_pages,
                    nsled_agents,
                    nsled_agent_zones,
                    nzones,
                    nnics,
                    nerrors,
                ))
            })
            .await
            .map_err(|error| match error {
                TransactionError::CustomError(e) => e,
                TransactionError::Database(e) => {
                    public_error_from_diesel(e, ErrorHandler::Server)
                }
            })?;

        info!(&opctx.log, "removed inventory collection";
            "collection_id" => collection_id.to_string(),
            "ncollections" => ncollections,
            "nsps" => nsps,
            "nrots" => nrots,
            "ncabooses" => ncabooses,
            "nrot_pages" => nrot_pages,
            "nsled_agents" => nsled_agents,
            "nsled_agent_zones" => nsled_agent_zones,
            "nzones" => nzones,
            "nnics" => nnics,
            "nerrors" => nerrors,
        );

        Ok(())
    }

    // Find the primary key for `hw_baseboard_id` given a `BaseboardId`
    pub async fn find_hw_baseboard_id(
        &self,
        opctx: &OpContext,
        baseboard_id: BaseboardId,
    ) -> Result<Uuid, Error> {
        opctx.authorize(authz::Action::Read, &authz::INVENTORY).await?;
        let conn = self.pool_connection_authorized(opctx).await?;
        use db::schema::hw_baseboard_id::dsl;
        dsl::hw_baseboard_id
            .filter(dsl::serial_number.eq(baseboard_id.serial_number.clone()))
            .filter(dsl::part_number.eq(baseboard_id.part_number.clone()))
            .select(dsl::id)
            .first_async::<Uuid>(&*conn)
            .await
            .map_err(|e| {
                public_error_from_diesel_lookup(
                    e,
                    ResourceType::Sled,
                    &LookupType::ByCompositeId(format!("{baseboard_id:?}")),
                )
            })
    }

    /// Attempt to read the latest collection while limiting queries to `limit`
    /// records
    ///
    /// If there aren't any collections, return `Ok(None)`.
    pub async fn inventory_get_latest_collection(
        &self,
        opctx: &OpContext,
        limit: NonZeroU32,
    ) -> Result<Option<Collection>, Error> {
        opctx.authorize(authz::Action::Read, &authz::INVENTORY).await?;
        let conn = self.pool_connection_authorized(opctx).await?;
        use db::schema::inv_collection::dsl;
        let collection_id = dsl::inv_collection
            .select(dsl::id)
            .order_by(dsl::time_started.desc())
            .first_async::<Uuid>(&*conn)
            .await
            .optional()
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        let Some(collection_id) = collection_id else {
            return Ok(None);
        };

        Ok(Some(
            self.inventory_collection_read_all_or_nothing(
                opctx,
                collection_id,
                limit,
            )
            .await?,
        ))
    }

    /// Attempt to read the given collection while limiting queries to `limit`
    /// records and returning nothing if `limit` is not large enough.
    async fn inventory_collection_read_all_or_nothing(
        &self,
        opctx: &OpContext,
        id: Uuid,
        limit: NonZeroU32,
    ) -> Result<Collection, Error> {
        let (collection, limit_reached) = self
            .inventory_collection_read_best_effort(opctx, id, limit)
            .await?;
        bail_unless!(
            !limit_reached,
            "hit limit of {} records while loading collection",
            limit
        );
        Ok(collection)
    }

    /// Make a best effort to read the given collection while limiting queries
    /// to `limit` results. Returns as much as it was able to get. The
    /// returned bool indicates whether the returned collection might be
    /// incomplete because the limit was reached.
    pub async fn inventory_collection_read_best_effort(
        &self,
        opctx: &OpContext,
        id: Uuid,
        limit: NonZeroU32,
    ) -> Result<(Collection, bool), Error> {
        let conn = self.pool_connection_authorized(opctx).await?;
        let sql_limit = i64::from(u32::from(limit));
        let usize_limit = usize::try_from(u32::from(limit)).unwrap();
        let mut limit_reached = false;
        let (time_started, time_done, collector) = {
            use db::schema::inv_collection::dsl;

            let collections = dsl::inv_collection
                .filter(dsl::id.eq(id))
                .limit(2)
                .select(InvCollection::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| {
                    public_error_from_diesel(e, ErrorHandler::Server)
                })?;
            bail_unless!(collections.len() == 1);
            let collection = collections.into_iter().next().unwrap();
            (
                collection.time_started,
                collection.time_done,
                collection.collector,
            )
        };

        let errors: Vec<String> = {
            use db::schema::inv_collection_error::dsl;
            dsl::inv_collection_error
                .filter(dsl::inv_collection_id.eq(id))
                .order_by(dsl::idx)
                .limit(sql_limit)
                .select(InvCollectionError::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|e| e.message)
                .collect()
        };
        limit_reached = limit_reached || errors.len() == usize_limit;

        let sps: BTreeMap<_, _> = {
            use db::schema::inv_service_processor::dsl;
            dsl::inv_service_processor
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvServiceProcessor::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|sp_row| {
                    let baseboard_id = sp_row.hw_baseboard_id;
                    (
                        baseboard_id,
                        nexus_types::inventory::ServiceProcessor::from(sp_row),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || sps.len() == usize_limit;

        let rots: BTreeMap<_, _> = {
            use db::schema::inv_root_of_trust::dsl;
            dsl::inv_root_of_trust
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvRootOfTrust::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|rot_row| {
                    let baseboard_id = rot_row.hw_baseboard_id;
                    (
                        baseboard_id,
                        nexus_types::inventory::RotState::from(rot_row),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || rots.len() == usize_limit;

        let sled_agent_rows: Vec<_> = {
            use db::schema::inv_sled_agent::dsl;
            dsl::inv_sled_agent
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvSledAgent::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| {
                    public_error_from_diesel(e, ErrorHandler::Server)
                })?
        };

        // Collect the unique baseboard ids referenced by SPs, RoTs, and Sled
        // Agents.
        let baseboard_id_ids: BTreeSet<_> = sps
            .keys()
            .chain(rots.keys())
            .cloned()
            .chain(sled_agent_rows.iter().filter_map(|s| s.hw_baseboard_id))
            .collect();
        // Fetch the corresponding baseboard records.
        let baseboards_by_id: BTreeMap<_, _> = {
            use db::schema::hw_baseboard_id::dsl;
            dsl::hw_baseboard_id
                .filter(dsl::id.eq_any(baseboard_id_ids))
                .limit(sql_limit)
                .select(HwBaseboardId::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|bb| {
                    (
                        bb.id,
                        Arc::new(nexus_types::inventory::BaseboardId::from(bb)),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || baseboards_by_id.len() == usize_limit;

        // Having those, we can replace the keys in the maps above with
        // references to the actual baseboard rather than the uuid.
        let sps = sps
            .into_iter()
            .map(|(id, sp)| {
                baseboards_by_id.get(&id).map(|bb| (bb.clone(), sp)).ok_or_else(
                    || {
                        Error::internal_error(
                            "missing baseboard that we should have fetched",
                        )
                    },
                )
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let rots = rots
            .into_iter()
            .map(|(id, rot)| {
                baseboards_by_id
                    .get(&id)
                    .map(|bb| (bb.clone(), rot))
                    .ok_or_else(|| {
                        Error::internal_error(
                            "missing baseboard that we should have fetched",
                        )
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let sled_agents: BTreeMap<_, _> =
            sled_agent_rows
                .into_iter()
                .map(|s: InvSledAgent| {
                    let sled_id = s.sled_id;
                    let baseboard_id = s
                        .hw_baseboard_id
                        .map(|id| {
                            baseboards_by_id.get(&id).cloned().ok_or_else(
                                || {
                                    Error::internal_error(
                                "missing baseboard that we should have fetched",
                            )
                                },
                            )
                        })
                        .transpose()?;
                    let sled_agent = nexus_types::inventory::SledAgent {
                        time_collected: s.time_collected,
                        source: s.source,
                        sled_id,
                        baseboard_id,
                        sled_agent_address: std::net::SocketAddrV6::new(
                            std::net::Ipv6Addr::from(s.sled_agent_ip),
                            u16::from(s.sled_agent_port),
                            0,
                            0,
                        ),
                        sled_role: nexus_types::inventory::SledRole::from(
                            s.sled_role,
                        ),
                        usable_hardware_threads: u32::from(
                            s.usable_hardware_threads,
                        ),
                        usable_physical_ram: s.usable_physical_ram.into(),
                        reservoir_size: s.reservoir_size.into(),
                    };
                    Ok((sled_id, sled_agent))
                })
                .collect::<Result<
                    BTreeMap<Uuid, nexus_types::inventory::SledAgent>,
                    Error,
                >>()?;

        // Fetch records of cabooses found.
        let inv_caboose_rows = {
            use db::schema::inv_caboose::dsl;
            dsl::inv_caboose
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvCaboose::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| {
                    public_error_from_diesel(e, ErrorHandler::Server)
                })?
        };
        limit_reached = limit_reached || inv_caboose_rows.len() == usize_limit;

        // Collect the unique sw_caboose_ids for those cabooses.
        let sw_caboose_ids: BTreeSet<_> = inv_caboose_rows
            .iter()
            .map(|inv_caboose| inv_caboose.sw_caboose_id)
            .collect();
        // Fetch the corresponing records.
        let cabooses_by_id: BTreeMap<_, _> = {
            use db::schema::sw_caboose::dsl;
            dsl::sw_caboose
                .filter(dsl::id.eq_any(sw_caboose_ids))
                .limit(sql_limit)
                .select(SwCaboose::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|sw_caboose_row| {
                    (
                        sw_caboose_row.id,
                        Arc::new(nexus_types::inventory::Caboose::from(
                            sw_caboose_row,
                        )),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || cabooses_by_id.len() == usize_limit;

        // Assemble the lists of cabooses found.
        let mut cabooses_found = BTreeMap::new();
        for c in inv_caboose_rows {
            let by_baseboard = cabooses_found
                .entry(nexus_types::inventory::CabooseWhich::from(c.which))
                .or_insert_with(BTreeMap::new);
            let Some(bb) = baseboards_by_id.get(&c.hw_baseboard_id) else {
                let msg = format!(
                    "unknown baseboard found in inv_caboose: {}",
                    c.hw_baseboard_id
                );
                return Err(Error::internal_error(&msg));
            };
            let Some(sw_caboose) = cabooses_by_id.get(&c.sw_caboose_id) else {
                let msg = format!(
                    "unknown caboose found in inv_caboose: {}",
                    c.sw_caboose_id
                );
                return Err(Error::internal_error(&msg));
            };

            let previous = by_baseboard.insert(
                bb.clone(),
                nexus_types::inventory::CabooseFound {
                    time_collected: c.time_collected,
                    source: c.source,
                    caboose: sw_caboose.clone(),
                },
            );
            bail_unless!(
                previous.is_none(),
                "duplicate caboose found: {:?} baseboard {:?}",
                c.which,
                c.hw_baseboard_id
            );
        }

        // Fetch records of RoT pages found.
        let inv_rot_page_rows = {
            use db::schema::inv_root_of_trust_page::dsl;
            dsl::inv_root_of_trust_page
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvRotPage::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| {
                    public_error_from_diesel(e, ErrorHandler::Server)
                })?
        };
        limit_reached = limit_reached || inv_rot_page_rows.len() == usize_limit;

        // Collect the unique sw_rot_page_ids for those pages.
        let sw_rot_page_ids: BTreeSet<_> = inv_rot_page_rows
            .iter()
            .map(|inv_rot_page| inv_rot_page.sw_root_of_trust_page_id)
            .collect();
        // Fetch the corresponding records.
        let rot_pages_by_id: BTreeMap<_, _> = {
            use db::schema::sw_root_of_trust_page::dsl;
            dsl::sw_root_of_trust_page
                .filter(dsl::id.eq_any(sw_rot_page_ids))
                .limit(sql_limit)
                .select(SwRotPage::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|sw_rot_page_row| {
                    (
                        sw_rot_page_row.id,
                        Arc::new(nexus_types::inventory::RotPage::from(
                            sw_rot_page_row,
                        )),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || rot_pages_by_id.len() == usize_limit;

        // Assemble the lists of rot pages found.
        let mut rot_pages_found = BTreeMap::new();
        for p in inv_rot_page_rows {
            let by_baseboard = rot_pages_found
                .entry(nexus_types::inventory::RotPageWhich::from(p.which))
                .or_insert_with(BTreeMap::new);
            let Some(bb) = baseboards_by_id.get(&p.hw_baseboard_id) else {
                let msg = format!(
                    "unknown baseboard found in inv_root_of_trust_page: {}",
                    p.hw_baseboard_id
                );
                return Err(Error::internal_error(&msg));
            };
            let Some(sw_rot_page) =
                rot_pages_by_id.get(&p.sw_root_of_trust_page_id)
            else {
                let msg = format!(
                    "unknown rot page found in inv_root_of_trust_page: {}",
                    p.sw_root_of_trust_page_id
                );
                return Err(Error::internal_error(&msg));
            };

            let previous = by_baseboard.insert(
                bb.clone(),
                nexus_types::inventory::RotPageFound {
                    time_collected: p.time_collected,
                    source: p.source,
                    page: sw_rot_page.clone(),
                },
            );
            bail_unless!(
                previous.is_none(),
                "duplicate rot page found: {:?} baseboard {:?}",
                p.which,
                p.hw_baseboard_id
            );
        }

        // Now read the Omicron zones.
        //
        // In the first pass, we'll load the "inv_sled_omicron_zones" records.
        // There's one of these per sled.  It does not contain the actual list
        // of zones -- basically just collection metadata and the generation
        // number.  We'll assemble these directly into the data structure we're
        // trying to build, which maps sled ids to objects describing the zones
        // found on each sled.
        let mut omicron_zones: BTreeMap<_, _> = {
            use db::schema::inv_sled_omicron_zones::dsl;
            dsl::inv_sled_omicron_zones
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvSledOmicronZones::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|sled_zones_config| {
                    (
                        sled_zones_config.sled_id,
                        sled_zones_config.into_uninit_zones_found(),
                    )
                })
                .collect()
        };
        limit_reached = limit_reached || omicron_zones.len() == usize_limit;

        // Assemble a mutable map of all the NICs found, by NIC id.  As we
        // match these up with the corresponding zone below, we'll remove items
        // from this set.  That way we can tell if the same NIC was used twice
        // or not used at all.
        let mut omicron_zone_nics: BTreeMap<_, _> = {
            use db::schema::inv_omicron_zone_nic::dsl;
            dsl::inv_omicron_zone_nic
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvOmicronZoneNic::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?
                .into_iter()
                .map(|found_zone_nic| (found_zone_nic.id, found_zone_nic))
                .collect()
        };
        limit_reached = limit_reached || omicron_zone_nics.len() == usize_limit;

        // Now load the actual list of zones from all sleds.
        let omicron_zones_list = {
            use db::schema::inv_omicron_zone::dsl;
            dsl::inv_omicron_zone
                .filter(dsl::inv_collection_id.eq(id))
                .limit(sql_limit)
                .select(InvOmicronZone::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| {
                    public_error_from_diesel(e, ErrorHandler::Server)
                })?
        };
        limit_reached =
            limit_reached || omicron_zones_list.len() == usize_limit;
        for z in omicron_zones_list {
            let nic_row = z
                .nic_id
                .map(|id| {
                    // This error means that we found a row in inv_omicron_zone
                    // that references a NIC by id but there's no corresponding
                    // row in inv_omicron_zone_nic with that id.  This should be
                    // impossible and reflects either a bug or database
                    // corruption.
                    omicron_zone_nics.remove(&id).ok_or_else(|| {
                        Error::internal_error(&format!(
                            "zone {:?}: expected to find NIC {:?}, but didn't",
                            z.id, z.nic_id
                        ))
                    })
                })
                .transpose()?;
            let map = omicron_zones.get_mut(&z.sled_id).ok_or_else(|| {
                // This error means that we found a row in inv_omicron_zone with
                // no associated record in inv_sled_omicron_zones.  This should
                // be impossible and reflects either a bug or database
                // corruption.
                Error::internal_error(&format!(
                    "zone {:?}: unknown sled: {:?}",
                    z.id, z.sled_id
                ))
            })?;
            let zone_id = z.id;
            let zone = z
                .into_omicron_zone_config(nic_row)
                .with_context(|| {
                    format!("zone {:?}: parse from database", zone_id)
                })
                .map_err(|e| {
                    Error::internal_error(&format!("{:#}", e.to_string()))
                })?;
            map.zones.zones.push(zone);
        }

        bail_unless!(
            omicron_zone_nics.is_empty(),
            "found extra Omicron zone NICs: {:?}",
            omicron_zone_nics.keys()
        );

        Ok((
            Collection {
                id,
                errors,
                time_started,
                time_done,
                collector,
                baseboards: baseboards_by_id.values().cloned().collect(),
                cabooses: cabooses_by_id.values().cloned().collect(),
                rot_pages: rot_pages_by_id.values().cloned().collect(),
                sps,
                rots,
                cabooses_found,
                rot_pages_found,
                sled_agents,
                omicron_zones,
            },
            limit_reached,
        ))
    }
}

/// Extra interfaces that are not intended (and potentially unsafe) for use in
/// Nexus, but useful for testing and `omdb`
pub trait DataStoreInventoryTest: Send + Sync {
    /// List all collections
    ///
    /// This does not paginate.
    fn inventory_collections(&self) -> BoxFuture<anyhow::Result<Vec<Uuid>>>;
}

impl DataStoreInventoryTest for DataStore {
    fn inventory_collections(&self) -> BoxFuture<anyhow::Result<Vec<Uuid>>> {
        async {
            let conn = self
                .pool_connection_for_tests()
                .await
                .context("getting connection")?;
            conn.transaction_async(|conn| async move {
                conn.batch_execute_async(ALLOW_FULL_TABLE_SCAN_SQL)
                    .await
                    .context("failed to allow table scan")?;

                use db::schema::inv_collection::dsl;
                dsl::inv_collection
                    .select(dsl::id)
                    .order_by(dsl::time_started)
                    .load_async(&conn)
                    .await
                    .context("failed to list collections")
            })
            .await
        }
        .boxed()
    }
}

#[cfg(test)]
mod test {
    use crate::context::OpContext;
    use crate::db::datastore::datastore_test;
    use crate::db::datastore::inventory::DataStoreInventoryTest;
    use crate::db::datastore::DataStore;
    use crate::db::datastore::DataStoreConnection;
    use crate::db::schema;
    use anyhow::Context;
    use async_bb8_diesel::AsyncConnection;
    use async_bb8_diesel::AsyncRunQueryDsl;
    use async_bb8_diesel::AsyncSimpleConnection;
    use diesel::QueryDsl;
    use gateway_client::types::SpType;
    use nexus_inventory::examples::representative;
    use nexus_inventory::examples::Representative;
    use nexus_test_utils::db::test_setup_database;
    use nexus_test_utils::db::ALLOW_FULL_TABLE_SCAN_SQL;
    use nexus_types::inventory::BaseboardId;
    use nexus_types::inventory::CabooseWhich;
    use nexus_types::inventory::Collection;
    use nexus_types::inventory::RotPageWhich;
    use omicron_common::api::external::Error;
    use omicron_test_utils::dev;
    use std::num::NonZeroU32;
    use uuid::Uuid;

    async fn read_collection(
        opctx: &OpContext,
        datastore: &DataStore,
        id: Uuid,
    ) -> anyhow::Result<Collection> {
        let limit = NonZeroU32::new(1000).unwrap();
        Ok(datastore
            .inventory_collection_read_all_or_nothing(opctx, id, limit)
            .await?)
    }

    struct CollectionCounts {
        baseboards: usize,
        cabooses: usize,
        rot_pages: usize,
    }

    impl CollectionCounts {
        async fn new(conn: &DataStoreConnection<'_>) -> anyhow::Result<Self> {
            conn.transaction_async(|conn| async move {
                conn.batch_execute_async(ALLOW_FULL_TABLE_SCAN_SQL)
                    .await
                    .unwrap();
                let bb_count = schema::hw_baseboard_id::dsl::hw_baseboard_id
                    .select(diesel::dsl::count_star())
                    .first_async::<i64>(&conn)
                    .await
                    .context("failed to count baseboards")?;
                let caboose_count = schema::sw_caboose::dsl::sw_caboose
                    .select(diesel::dsl::count_star())
                    .first_async::<i64>(&conn)
                    .await
                    .context("failed to count cabooses")?;
                let rot_page_count =
                    schema::sw_root_of_trust_page::dsl::sw_root_of_trust_page
                        .select(diesel::dsl::count_star())
                        .first_async::<i64>(&conn)
                        .await
                        .context("failed to count rot pages")?;
                let baseboards = usize::try_from(bb_count)
                    .context("failed to convert baseboard count to usize")?;
                let cabooses = usize::try_from(caboose_count)
                    .context("failed to convert caboose count to usize")?;
                let rot_pages = usize::try_from(rot_page_count)
                    .context("failed to convert rot page count to usize")?;
                Ok(Self { baseboards, cabooses, rot_pages })
            })
            .await
        }
    }

    #[tokio::test]
    async fn test_find_hw_baseboard_id_missing_returns_not_found() {
        let logctx = dev::test_setup_log("inventory_insert");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;
        let baseboard_id = BaseboardId {
            serial_number: "some-serial".into(),
            part_number: "some-part".into(),
        };
        let err = datastore
            .find_hw_baseboard_id(&opctx, baseboard_id)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ObjectNotFound { .. }));
        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    /// Tests inserting several collections, reading them back, and making sure
    /// they look the same.
    #[tokio::test]
    async fn test_inventory_insert() {
        // Setup
        let logctx = dev::test_setup_log("inventory_insert");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create an empty collection and write it to the database.
        let builder = nexus_inventory::CollectionBuilder::new("test");
        let collection1 = builder.build();
        datastore
            .inventory_insert_collection(&opctx, &collection1)
            .await
            .expect("failed to insert collection");

        // Read it back.
        let conn = datastore.pool_connection_for_tests().await.unwrap();
        let collection_read =
            read_collection(&opctx, &datastore, collection1.id)
                .await
                .expect("failed to read collection back");
        assert_eq!(collection1, collection_read);

        // There ought to be no baseboards, cabooses, or RoT pages in the
        // database from that collection.
        assert_eq!(collection1.baseboards.len(), 0);
        assert_eq!(collection1.cabooses.len(), 0);
        assert_eq!(collection1.rot_pages.len(), 0);
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_eq!(collection1.baseboards.len(), coll_counts.baseboards);
        assert_eq!(collection1.cabooses.len(), coll_counts.cabooses);
        assert_eq!(collection1.rot_pages.len(), coll_counts.rot_pages);

        // Now insert a more complex collection, write it to the database, and
        // read it back.
        let Representative { builder, .. } = representative();
        let collection2 = builder.build();
        datastore
            .inventory_insert_collection(&opctx, &collection2)
            .await
            .expect("failed to insert collection");
        let collection_read =
            read_collection(&opctx, &datastore, collection2.id)
                .await
                .expect("failed to read collection back");
        assert_eq!(collection2, collection_read);
        // Verify that we have exactly the set of cabooses, baseboards, and RoT
        // pages in the databases that came from this first non-empty
        // collection.
        assert_ne!(collection2.baseboards.len(), collection1.baseboards.len());
        assert_ne!(collection2.cabooses.len(), collection1.cabooses.len());
        assert_ne!(collection2.rot_pages.len(), collection1.rot_pages.len());
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_eq!(collection2.baseboards.len(), coll_counts.baseboards);
        assert_eq!(collection2.cabooses.len(), coll_counts.cabooses);
        assert_eq!(collection2.rot_pages.len(), coll_counts.rot_pages);

        // Check that we get an error on the limit being reached for
        // `read_all_or_nothing`
        let limit = NonZeroU32::new(1).unwrap();
        assert!(datastore
            .inventory_collection_read_all_or_nothing(
                &opctx,
                collection2.id,
                limit
            )
            .await
            .is_err());

        // Now insert an equivalent collection again.  Verify the distinct
        // baseboards, cabooses, and RoT pages again.  This is important: the
        // insertion process should re-use the baseboards, cabooses, and RoT
        // pages from the previous collection.
        let Representative { builder, .. } = representative();
        let collection3 = builder.build();
        datastore
            .inventory_insert_collection(&opctx, &collection3)
            .await
            .expect("failed to insert collection");
        let collection_read =
            read_collection(&opctx, &datastore, collection3.id)
                .await
                .expect("failed to read collection back");
        assert_eq!(collection3, collection_read);
        // Verify that we have the same number of cabooses, baseboards, and RoT
        // pages, since those didn't change.
        assert_eq!(collection3.baseboards.len(), collection2.baseboards.len());
        assert_eq!(collection3.cabooses.len(), collection2.cabooses.len());
        assert_eq!(collection3.rot_pages.len(), collection2.rot_pages.len());
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_eq!(collection3.baseboards.len(), coll_counts.baseboards);
        assert_eq!(collection3.cabooses.len(), coll_counts.cabooses);
        assert_eq!(collection3.rot_pages.len(), coll_counts.rot_pages);

        // Now insert a collection that's almost equivalent, but has an extra
        // couple of baseboards, one caboose, and one RoT page.  Verify that we
        // re-use the existing ones, but still insert the new ones.
        let Representative { mut builder, .. } = representative();
        builder.found_sp_state(
            "test suite",
            SpType::Switch,
            1,
            nexus_inventory::examples::sp_state("2"),
        );
        let bb = builder
            .found_sp_state(
                "test suite",
                SpType::Power,
                1,
                nexus_inventory::examples::sp_state("3"),
            )
            .unwrap();
        builder
            .found_caboose(
                &bb,
                CabooseWhich::SpSlot0,
                "dummy",
                nexus_inventory::examples::caboose("dummy"),
            )
            .unwrap();
        builder
            .found_rot_page(
                &bb,
                RotPageWhich::Cmpa,
                "dummy",
                nexus_inventory::examples::rot_page("dummy"),
            )
            .unwrap();
        let collection4 = builder.build();
        datastore
            .inventory_insert_collection(&opctx, &collection4)
            .await
            .expect("failed to insert collection");
        let collection_read =
            read_collection(&opctx, &datastore, collection4.id)
                .await
                .expect("failed to read collection back");
        assert_eq!(collection4, collection_read);
        // Verify the number of baseboards and collections again.
        assert_eq!(
            collection4.baseboards.len(),
            collection3.baseboards.len() + 2
        );
        assert_eq!(collection4.cabooses.len(), collection3.cabooses.len() + 1);
        assert_eq!(
            collection4.rot_pages.len(),
            collection3.rot_pages.len() + 1
        );
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_eq!(collection4.baseboards.len(), coll_counts.baseboards);
        assert_eq!(collection4.cabooses.len(), coll_counts.cabooses);
        assert_eq!(collection4.rot_pages.len(), coll_counts.rot_pages);

        // This time, go back to our earlier collection.  This logically removes
        // some baseboards.  They should still be present in the database, but
        // not in the collection.
        let Representative { builder, .. } = representative();
        let collection5 = builder.build();
        datastore
            .inventory_insert_collection(&opctx, &collection5)
            .await
            .expect("failed to insert collection");
        let collection_read =
            read_collection(&opctx, &datastore, collection5.id)
                .await
                .expect("failed to read collection back");
        assert_eq!(collection5, collection_read);
        assert_eq!(collection5.baseboards.len(), collection3.baseboards.len());
        assert_eq!(collection5.cabooses.len(), collection3.cabooses.len());
        assert_eq!(collection5.rot_pages.len(), collection3.rot_pages.len());
        assert_ne!(collection5.baseboards.len(), collection4.baseboards.len());
        assert_ne!(collection5.cabooses.len(), collection4.cabooses.len());
        assert_ne!(collection5.rot_pages.len(), collection4.rot_pages.len());
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_eq!(collection4.baseboards.len(), coll_counts.baseboards);
        assert_eq!(collection4.cabooses.len(), coll_counts.cabooses);
        assert_eq!(collection4.rot_pages.len(), coll_counts.rot_pages);

        // Try to insert the same collection again and make sure it fails.
        let error = datastore
            .inventory_insert_collection(&opctx, &collection5)
            .await
            .expect_err("unexpectedly succeeded in inserting collection");
        assert!(format!("{:#}", error)
            .contains("duplicate key value violates unique constraint"));

        // Now that we've inserted a bunch of collections, we can test pruning.
        //
        // The datastore should start by pruning the oldest collection, unless
        // it's the only collection with no errors.  The oldest one is
        // `collection1`, which _is_ the only one with no errors.  So we should
        // get back `collection2`.
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[
                collection1.id,
                collection2.id,
                collection3.id,
                collection4.id,
                collection5.id,
            ]
        );
        println!(
            "all collections: {:?}\n",
            &[
                collection1.id,
                collection2.id,
                collection3.id,
                collection4.id,
                collection5.id,
            ]
        );
        datastore
            .inventory_prune_collections(&opctx, 4)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection1.id, collection3.id, collection4.id, collection5.id,]
        );
        // Again, we should skip over collection1 and delete the next oldest:
        // collection3.
        datastore
            .inventory_prune_collections(&opctx, 3)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection1.id, collection4.id, collection5.id,]
        );
        // At this point, if we're keeping 3, we don't need to prune anything.
        datastore
            .inventory_prune_collections(&opctx, 3)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection1.id, collection4.id, collection5.id,]
        );

        // If we then insert an empty collection (which has no errors),
        // collection1 becomes pruneable.
        let builder = nexus_inventory::CollectionBuilder::new("test");
        let collection6 = builder.build();
        println!(
            "collection 6: {} ({:?})",
            collection6.id, collection6.time_started
        );
        datastore
            .inventory_insert_collection(&opctx, &collection6)
            .await
            .expect("failed to insert collection");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection1.id, collection4.id, collection5.id, collection6.id,]
        );
        datastore
            .inventory_prune_collections(&opctx, 3)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection4.id, collection5.id, collection6.id,]
        );
        // Again, at this point, we should not prune anything.
        datastore
            .inventory_prune_collections(&opctx, 3)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection4.id, collection5.id, collection6.id,]
        );

        // If we insert another collection with errors, then prune, we should
        // end up pruning collection 4.
        let Representative { builder, .. } = representative();
        let collection7 = builder.build();
        println!(
            "collection 7: {} ({:?})",
            collection7.id, collection7.time_started
        );
        datastore
            .inventory_insert_collection(&opctx, &collection7)
            .await
            .expect("failed to insert collection");
        datastore
            .inventory_prune_collections(&opctx, 3)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection5.id, collection6.id, collection7.id,]
        );

        // If we try to fetch a pruned collection, we should get nothing.
        let _ = read_collection(&opctx, &datastore, collection4.id)
            .await
            .expect_err("unexpectedly read pruned collection");

        // But we should still be able to fetch the collections that do exist.
        let collection_read =
            read_collection(&opctx, &datastore, collection5.id).await.unwrap();
        assert_eq!(collection5, collection_read);
        let collection_read =
            read_collection(&opctx, &datastore, collection6.id).await.unwrap();
        assert_eq!(collection6, collection_read);
        let collection_read =
            read_collection(&opctx, &datastore, collection7.id).await.unwrap();
        assert_eq!(collection7, collection_read);

        // We should prune more than one collection, if needed.  We'll wind up
        // with just collection6 because that's the latest one with no errors.
        datastore
            .inventory_prune_collections(&opctx, 1)
            .await
            .expect("failed to prune collections");
        assert_eq!(
            datastore.inventory_collections().await.unwrap(),
            &[collection6.id,]
        );

        // Remove the remaining collection and make sure the inventory tables
        // are empty (i.e., we got everything).
        datastore
            .inventory_delete_collection(&opctx, collection6.id)
            .await
            .expect("failed to delete collection");
        assert_eq!(datastore.inventory_collections().await.unwrap(), &[]);

        conn.transaction_async(|conn| async move {
            conn.batch_execute_async(ALLOW_FULL_TABLE_SCAN_SQL).await.unwrap();
            let count = schema::inv_collection::dsl::inv_collection
                .select(diesel::dsl::count_star())
                .first_async::<i64>(&conn)
                .await
                .unwrap();
            assert_eq!(0, count);
            let count = schema::inv_collection_error::dsl::inv_collection_error
                .select(diesel::dsl::count_star())
                .first_async::<i64>(&conn)
                .await
                .unwrap();
            assert_eq!(0, count);
            let count =
                schema::inv_service_processor::dsl::inv_service_processor
                    .select(diesel::dsl::count_star())
                    .first_async::<i64>(&conn)
                    .await
                    .unwrap();
            assert_eq!(0, count);
            let count = schema::inv_root_of_trust::dsl::inv_root_of_trust
                .select(diesel::dsl::count_star())
                .first_async::<i64>(&conn)
                .await
                .unwrap();
            assert_eq!(0, count);
            let count = schema::inv_caboose::dsl::inv_caboose
                .select(diesel::dsl::count_star())
                .first_async::<i64>(&conn)
                .await
                .unwrap();
            assert_eq!(0, count);
            Ok::<(), anyhow::Error>(())
        })
        .await
        .expect("failed to check that tables were empty");

        // We currently keep the baseboard ids and sw_cabooses around.
        let coll_counts = CollectionCounts::new(&conn).await.unwrap();
        assert_ne!(coll_counts.baseboards, 0);
        assert_ne!(coll_counts.cabooses, 0);
        assert_ne!(coll_counts.rot_pages, 0);

        // Clean up.
        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }
}
