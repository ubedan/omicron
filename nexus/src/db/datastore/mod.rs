// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Primary control plane interface for database read and write operations

// TODO-scalability review all queries for use of indexes (may need
// "time_deleted IS NOT NULL" conditions) Figure out how to automate this.
//
// TODO-design Better support for joins?
// The interfaces here often require that to do anything with an object, a
// caller must first look up the id and then do operations with the id.  For
// example, the caller of project_list_disks() always looks up the project to
// get the project_id, then lists disks having that project_id.  It's possible
// to implement this instead with a JOIN in the database so that we do it with
// one database round-trip.  We could use CTEs similar to what we do with
// conditional updates to distinguish the case where the project didn't exist
// vs. there were no disks in it.  This seems likely to be a fair bit more
// complicated to do safely and generally compared to what we have now.

use super::pool::DbConnection;
use super::Pool;
use crate::authz;
use crate::context::OpContext;
use crate::db::{
    self,
    error::{public_error_from_diesel_pool, ErrorHandler},
};
use async_bb8_diesel::{AsyncRunQueryDsl, ConnectionManager};
use diesel::pg::Pg;
use diesel::prelude::*;
use diesel::query_builder::{QueryFragment, QueryId};
use diesel::query_dsl::methods::LoadQuery;
use diesel::{ExpressionMethods, QueryDsl};
use omicron_common::api::external::Error;
use omicron_common::api::external::IdentityMetadataCreateParams;
use omicron_common::api::external::LookupType;
use omicron_common::api::external::ResourceType;
use std::net::Ipv6Addr;
use std::sync::Arc;
use uuid::Uuid;

mod console_session;
mod dataset;
mod device_auth;
mod disk;
mod external_ip;
mod global_image;
mod identity_provider;
mod instance;
mod ip_pool;
mod network_interface;
mod organization;
mod oximeter;
mod project;
mod rack;
mod region;
mod role;
mod saga;
mod service;
mod silo;
mod silo_group;
mod silo_user;
mod sled;
mod snapshot;
mod ssh_key;
mod update;
mod volume;
mod vpc;
mod zpool;

// Number of unique datasets required to back a region.
// TODO: This should likely turn into a configuration option.
const REGION_REDUNDANCY_THRESHOLD: usize = 3;

// Represents a query that is ready to be executed.
//
// This helper trait lets the statement either be executed or explained.
//
// U: The output type of executing the statement.
pub trait RunnableQueryNoReturn:
    RunQueryDsl<DbConnection> + QueryFragment<Pg> + QueryId
{
}

impl<T> RunnableQueryNoReturn for T where
    T: RunQueryDsl<DbConnection> + QueryFragment<Pg> + QueryId
{
}

pub trait RunnableQuery<U>:
    RunnableQueryNoReturn + LoadQuery<'static, DbConnection, U>
{
}

impl<U, T> RunnableQuery<U> for T where
    T: RunnableQueryNoReturn + LoadQuery<'static, DbConnection, U>
{
}

// Redundancy for the number of datasets to be provisioned.
#[derive(Clone, Copy, Debug)]
pub enum DatasetRedundancy {
    // The dataset should exist on all zpools.
    OnAll,
    // The dataset should exist on at least this many zpools.
    PerRack(u32),
}

pub struct DataStore {
    pool: Arc<Pool>,
}

// The majority of `DataStore`'s methods live in our submodules as a concession
// to compilation times; changing a query only requires incremental
// recompilation of that query's module instead of all queries on `DataStore`.
impl DataStore {
    pub fn new(pool: Arc<Pool>) -> Self {
        DataStore { pool }
    }

    // TODO-security This should be deprecated in favor of pool_authorized(),
    // which gives us the chance to do a minimal security check before hitting
    // the database.  Eventually, this function should only be used for doing
    // authentication in the first place (since we can't do an authz check in
    // that case).
    fn pool(&self) -> &bb8::Pool<ConnectionManager<DbConnection>> {
        self.pool.pool()
    }

    pub(super) async fn pool_authorized(
        &self,
        opctx: &OpContext,
    ) -> Result<&bb8::Pool<ConnectionManager<DbConnection>>, Error> {
        opctx.authorize(authz::Action::Query, &authz::DATABASE).await?;
        Ok(self.pool.pool())
    }

    fn next_ipv6_address_query(
        sled_id: Uuid,
    ) -> impl RunnableQuery<ipnetwork::IpNetwork> {
        use db::schema::sled::dsl;
        diesel::update(
            dsl::sled.find(sled_id).filter(dsl::time_deleted.is_null()),
        )
        .set(dsl::last_used_address.eq(dsl::last_used_address + 1))
        .returning(dsl::last_used_address)
    }

    pub async fn next_ipv6_address_on_connection<ConnErr>(
        conn: &(impl async_bb8_diesel::AsyncConnection<DbConnection, ConnErr>
              + Sync),
        sled_id: Uuid,
    ) -> Result<Ipv6Addr, Error>
    where
        ConnErr: From<diesel::result::Error> + Send + 'static,
        async_bb8_diesel::PoolError: From<ConnErr>,
    {
        let net = Self::next_ipv6_address_query(sled_id)
            .get_result_async(conn)
            .await
            .map_err(|e| {
                public_error_from_diesel_pool(
                    async_bb8_diesel::PoolError::from(e),
                    ErrorHandler::NotFoundByLookup(
                        ResourceType::Sled,
                        LookupType::ById(sled_id),
                    ),
                )
            })?;

        // TODO-correctness: We need to ensure that this address is actually
        // within the sled's underlay prefix, once that's included in the
        // database record.
        match net {
            ipnetwork::IpNetwork::V6(net) => Ok(net.ip()),
            _ => Err(Error::InternalError {
                internal_message: String::from("Sled IP address must be IPv6"),
            }),
        }
    }

    /// Return the next available IPv6 address for an Oxide service running on
    /// the provided sled.
    pub async fn next_ipv6_address(
        &self,
        opctx: &OpContext,
        sled_id: Uuid,
    ) -> Result<Ipv6Addr, Error> {
        let conn = self.pool_authorized(opctx).await?;
        Self::next_ipv6_address_on_connection(conn, sled_id).await
    }

    // Test interfaces

    #[cfg(test)]
    async fn test_try_table_scan(&self, opctx: &OpContext) -> Error {
        use db::schema::project::dsl;
        let conn = self.pool_authorized(opctx).await;
        if let Err(error) = conn {
            return error;
        }
        let result = dsl::project
            .select(diesel::dsl::count_star())
            .first_async::<i64>(conn.unwrap())
            .await;
        match result {
            Ok(_) => Error::internal_error("table scan unexpectedly succeeded"),
            Err(error) => {
                public_error_from_diesel_pool(error, ErrorHandler::Server)
            }
        }
    }
}

/// Constructs a DataStore for use in test suites that has preloaded the
/// built-in users, roles, and role assignments that are needed for basic
/// operation
#[cfg(test)]
pub async fn datastore_test(
    logctx: &dropshot::test_util::LogContext,
    db: &omicron_test_utils::dev::db::CockroachInstance,
) -> (OpContext, Arc<DataStore>) {
    use crate::authn;

    let cfg = db::Config { url: db.pg_config().clone() };
    let pool = Arc::new(db::Pool::new(&cfg));
    let datastore = Arc::new(DataStore::new(pool));

    // Create an OpContext with the credentials of "db-init" just for the
    // purpose of loading the built-in users, roles, and assignments.
    let opctx = OpContext::for_background(
        logctx.log.new(o!()),
        Arc::new(authz::Authz::new(&logctx.log)),
        authn::Context::internal_db_init(),
        Arc::clone(&datastore),
    );
    datastore.load_builtin_users(&opctx).await.unwrap();
    datastore.load_builtin_roles(&opctx).await.unwrap();
    datastore.load_builtin_role_asgns(&opctx).await.unwrap();
    datastore.load_builtin_silos(&opctx).await.unwrap();
    datastore.load_silo_users(&opctx).await.unwrap();
    datastore.load_silo_user_role_assignments(&opctx).await.unwrap();

    // Create an OpContext with the credentials of "test-privileged" for general
    // testing.
    let opctx =
        OpContext::for_tests(logctx.log.new(o!()), Arc::clone(&datastore));

    (opctx, datastore)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::authn;
    use crate::authz;
    use crate::db::explain::ExplainableAsync;
    use crate::db::fixed_data::silo::SILO_ID;
    use crate::db::identity::Asset;
    use crate::db::identity::Resource;
    use crate::db::lookup::LookupPath;
    use crate::db::model::Dataset;
    use crate::db::model::ExternalIp;
    use crate::db::model::Rack;
    use crate::db::model::Region;
    use crate::db::model::Service;
    use crate::db::model::SiloUser;
    use crate::db::model::Sled;
    use crate::db::model::SshKey;
    use crate::db::model::VpcSubnet;
    use crate::db::model::Zpool;
    use crate::db::model::{ConsoleSession, DatasetKind, Project, ServiceKind};
    use crate::db::queries::vpc_subnet::FilterConflictingVpcSubnetRangesQuery;
    use crate::external_api::params;
    use chrono::{Duration, Utc};
    use nexus_test_utils::db::test_setup_database;
    use omicron_common::address::{Ipv6Subnet, DNS_REDUNDANCY, RACK_PREFIX};
    use omicron_common::api::external::{
        ByteCount, Error, IdentityMetadataCreateParams, LookupType, Name,
    };
    use omicron_test_utils::dev;
    use ref_cast::RefCast;
    use std::collections::{HashMap, HashSet};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV6};
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_project_creation() {
        let logctx = dev::test_setup_log("test_project_creation");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;
        let organization = params::OrganizationCreate {
            identity: IdentityMetadataCreateParams {
                name: "org".parse().unwrap(),
                description: "desc".to_string(),
            },
        };

        let organization =
            datastore.organization_create(&opctx, &organization).await.unwrap();

        let project = Project::new(
            organization.id(),
            params::ProjectCreate {
                identity: IdentityMetadataCreateParams {
                    name: "project".parse().unwrap(),
                    description: "desc".to_string(),
                },
            },
        );
        let (.., authz_org) = LookupPath::new(&opctx, &datastore)
            .organization_id(organization.id())
            .lookup_for(authz::Action::CreateChild)
            .await
            .unwrap();
        datastore.project_create(&opctx, &authz_org, project).await.unwrap();

        let (.., organization_after_project_create) =
            LookupPath::new(&opctx, &datastore)
                .organization_name(db::model::Name::ref_cast(
                    organization.name(),
                ))
                .fetch()
                .await
                .unwrap();
        assert!(organization_after_project_create.rcgen > organization.rcgen);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_session_methods() {
        let logctx = dev::test_setup_log("test_session_methods");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;
        let authn_opctx = OpContext::for_background(
            logctx.log.new(o!("component" => "TestExternalAuthn")),
            Arc::new(authz::Authz::new(&logctx.log)),
            authn::Context::external_authn(),
            Arc::clone(&datastore),
        );

        let token = "a_token".to_string();
        let silo_user_id = Uuid::new_v4();

        let session = ConsoleSession {
            token: token.clone(),
            time_created: Utc::now() - Duration::minutes(5),
            time_last_used: Utc::now() - Duration::minutes(5),
            silo_user_id,
        };

        let _ = datastore
            .session_create(&authn_opctx, session.clone())
            .await
            .unwrap();

        // Associate silo with user
        datastore
            .silo_user_create(SiloUser::new(
                *SILO_ID,
                silo_user_id,
                "external_id".into(),
            ))
            .await
            .unwrap();

        let (.., db_silo_user) = LookupPath::new(&opctx, &datastore)
            .silo_user_id(session.silo_user_id)
            .fetch()
            .await
            .unwrap();
        assert_eq!(*SILO_ID, db_silo_user.silo_id);

        // fetch the one we just created
        let (.., fetched) = LookupPath::new(&opctx, &datastore)
            .console_session_token(&token)
            .fetch()
            .await
            .unwrap();
        assert_eq!(session.silo_user_id, fetched.silo_user_id);

        // trying to insert the same one again fails
        let duplicate =
            datastore.session_create(&authn_opctx, session.clone()).await;
        assert!(matches!(
            duplicate,
            Err(Error::InternalError { internal_message: _ })
        ));

        // update last used (i.e., renew token)
        let authz_session = authz::ConsoleSession::new(
            authz::FLEET,
            token.clone(),
            LookupType::ByCompositeId(token.clone()),
        );
        let renewed = datastore
            .session_update_last_used(&opctx, &authz_session)
            .await
            .unwrap();
        assert!(
            renewed.console_session.time_last_used > session.time_last_used
        );

        // time_last_used change persists in DB
        let (.., fetched) = LookupPath::new(&opctx, &datastore)
            .console_session_token(&token)
            .fetch()
            .await
            .unwrap();
        assert!(fetched.time_last_used > session.time_last_used);

        // deleting it using `opctx` (which represents the test-privileged user)
        // should succeed but not do anything -- you can't delete someone else's
        // session
        let delete =
            datastore.session_hard_delete(&opctx, &authz_session).await;
        assert_eq!(delete, Ok(()));
        let fetched = LookupPath::new(&opctx, &datastore)
            .console_session_token(&token)
            .fetch()
            .await;
        assert!(fetched.is_ok());

        // delete it and fetch should come back with nothing
        let silo_user_opctx = OpContext::for_background(
            logctx.log.new(o!()),
            Arc::new(authz::Authz::new(&logctx.log)),
            authn::Context::for_test_user(silo_user_id, *SILO_ID),
            Arc::clone(&datastore),
        );
        let delete = datastore
            .session_hard_delete(&silo_user_opctx, &authz_session)
            .await;
        assert_eq!(delete, Ok(()));
        let fetched = LookupPath::new(&opctx, &datastore)
            .console_session_token(&token)
            .fetch()
            .await;
        assert!(matches!(
            fetched,
            Err(Error::ObjectNotFound { type_name: _, lookup_type: _ })
        ));

        // deleting an already nonexistent is considered a success
        let delete_again =
            datastore.session_hard_delete(&opctx, &authz_session).await;
        assert_eq!(delete_again, Ok(()));

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    // Creates a test sled, returns its UUID.
    async fn create_test_sled(datastore: &DataStore) -> Uuid {
        let bogus_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let rack_id = Uuid::new_v4();
        let sled_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, bogus_addr.clone(), is_scrimlet, rack_id);
        datastore.sled_upsert(sled).await.unwrap();
        sled_id
    }

    fn test_zpool_size() -> ByteCount {
        ByteCount::from_gibibytes_u32(100)
    }

    // Creates a test zpool, returns its UUID.
    async fn create_test_zpool(datastore: &DataStore, sled_id: Uuid) -> Uuid {
        let zpool_id = Uuid::new_v4();
        let zpool = Zpool::new(
            zpool_id,
            sled_id,
            &crate::internal_api::params::ZpoolPutRequest {
                size: test_zpool_size(),
            },
        );
        datastore.zpool_upsert(zpool).await.unwrap();
        zpool_id
    }

    fn create_test_disk_create_params(
        name: &str,
        size: ByteCount,
    ) -> params::DiskCreate {
        params::DiskCreate {
            identity: IdentityMetadataCreateParams {
                name: Name::try_from(name.to_string()).unwrap(),
                description: name.to_string(),
            },
            disk_source: params::DiskSource::Blank {
                block_size: params::BlockSize::try_from(4096).unwrap(),
            },
            size,
        }
    }

    #[tokio::test]
    async fn test_region_allocation() {
        let logctx = dev::test_setup_log("test_region_allocation");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = db::Pool::new(&cfg);
        let datastore = Arc::new(DataStore::new(Arc::new(pool)));
        let opctx =
            OpContext::for_tests(logctx.log.new(o!()), datastore.clone());

        // Create a sled...
        let sled_id = create_test_sled(&datastore).await;

        // ... and a zpool within that sled...
        let zpool_id = create_test_zpool(&datastore, sled_id).await;

        // ... and datasets within that zpool.
        let dataset_count = REGION_REDUNDANCY_THRESHOLD * 2;
        let bogus_addr = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8080, 0, 0);
        let dataset_ids: Vec<Uuid> =
            (0..dataset_count).map(|_| Uuid::new_v4()).collect();
        for id in &dataset_ids {
            let dataset =
                Dataset::new(*id, zpool_id, bogus_addr, DatasetKind::Crucible);
            datastore.dataset_upsert(dataset).await.unwrap();
        }

        // Allocate regions from the datasets for this disk.
        let params = create_test_disk_create_params(
            "disk1",
            ByteCount::from_mebibytes_u32(500),
        );
        let volume1_id = Uuid::new_v4();
        // Currently, we only allocate one Region Set per volume.
        let expected_region_count = REGION_REDUNDANCY_THRESHOLD;
        let dataset_and_regions = datastore
            .region_allocate(&opctx, volume1_id, &params)
            .await
            .unwrap();

        // Verify the allocation.
        assert_eq!(expected_region_count, dataset_and_regions.len());
        let mut disk1_datasets = HashSet::new();
        for (dataset, region) in dataset_and_regions {
            assert!(disk1_datasets.insert(dataset.id()));
            assert_eq!(volume1_id, region.volume_id());
            assert_eq!(ByteCount::from(4096), region.block_size());
            assert_eq!(params.extent_size() / 4096, region.blocks_per_extent());
            assert_eq!(params.extent_count(), region.extent_count());
        }

        // Allocate regions for a second disk. Observe that we allocate from
        // the three previously unused datasets.
        let params = create_test_disk_create_params(
            "disk2",
            ByteCount::from_mebibytes_u32(500),
        );
        let volume2_id = Uuid::new_v4();
        let dataset_and_regions = datastore
            .region_allocate(&opctx, volume2_id, &params)
            .await
            .unwrap();
        assert_eq!(expected_region_count, dataset_and_regions.len());
        let mut disk2_datasets = HashSet::new();
        for (dataset, region) in dataset_and_regions {
            assert!(disk2_datasets.insert(dataset.id()));
            assert_eq!(volume2_id, region.volume_id());
            assert_eq!(ByteCount::from(4096), region.block_size());
            assert_eq!(params.extent_size() / 4096, region.blocks_per_extent());
            assert_eq!(params.extent_count(), region.extent_count());
        }

        // Double-check that the datasets used for the first disk weren't
        // used when allocating the second disk.
        assert_eq!(0, disk1_datasets.intersection(&disk2_datasets).count());

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_region_allocation_is_idempotent() {
        let logctx =
            dev::test_setup_log("test_region_allocation_is_idempotent");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = db::Pool::new(&cfg);
        let datastore = Arc::new(DataStore::new(Arc::new(pool)));
        let opctx =
            OpContext::for_tests(logctx.log.new(o!()), datastore.clone());

        // Create a sled...
        let sled_id = create_test_sled(&datastore).await;

        // ... and a zpool within that sled...
        let zpool_id = create_test_zpool(&datastore, sled_id).await;

        // ... and datasets within that zpool.
        let dataset_count = REGION_REDUNDANCY_THRESHOLD;
        let bogus_addr = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8080, 0, 0);
        let dataset_ids: Vec<Uuid> =
            (0..dataset_count).map(|_| Uuid::new_v4()).collect();
        for id in &dataset_ids {
            let dataset =
                Dataset::new(*id, zpool_id, bogus_addr, DatasetKind::Crucible);
            datastore.dataset_upsert(dataset).await.unwrap();
        }

        // Allocate regions from the datasets for this volume.
        let params = create_test_disk_create_params(
            "disk",
            ByteCount::from_mebibytes_u32(500),
        );
        let volume_id = Uuid::new_v4();
        let mut dataset_and_regions1 = datastore
            .region_allocate(&opctx, volume_id, &params)
            .await
            .unwrap();
        let mut dataset_and_regions2 = datastore
            .region_allocate(&opctx, volume_id, &params)
            .await
            .unwrap();

        // Give them a consistent order so we can easily compare them.
        let sort_vec = |v: &mut Vec<(Dataset, Region)>| {
            v.sort_by(|(d1, r1), (d2, r2)| {
                let order = d1.id().cmp(&d2.id());
                match order {
                    std::cmp::Ordering::Equal => r1.id().cmp(&r2.id()),
                    _ => order,
                }
            });
        };
        sort_vec(&mut dataset_and_regions1);
        sort_vec(&mut dataset_and_regions2);

        // Validate that the two calls to allocate return the same data.
        assert_eq!(dataset_and_regions1.len(), dataset_and_regions2.len());
        for i in 0..dataset_and_regions1.len() {
            assert_eq!(dataset_and_regions1[i], dataset_and_regions2[i],);
        }

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_region_allocation_not_enough_datasets() {
        let logctx =
            dev::test_setup_log("test_region_allocation_not_enough_datasets");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = db::Pool::new(&cfg);
        let datastore = Arc::new(DataStore::new(Arc::new(pool)));
        let opctx =
            OpContext::for_tests(logctx.log.new(o!()), datastore.clone());

        // Create a sled...
        let sled_id = create_test_sled(&datastore).await;

        // ... and a zpool within that sled...
        let zpool_id = create_test_zpool(&datastore, sled_id).await;

        // ... and datasets within that zpool.
        let dataset_count = REGION_REDUNDANCY_THRESHOLD - 1;
        let bogus_addr = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8080, 0, 0);
        let dataset_ids: Vec<Uuid> =
            (0..dataset_count).map(|_| Uuid::new_v4()).collect();
        for id in &dataset_ids {
            let dataset =
                Dataset::new(*id, zpool_id, bogus_addr, DatasetKind::Crucible);
            datastore.dataset_upsert(dataset).await.unwrap();
        }

        // Allocate regions from the datasets for this volume.
        let params = create_test_disk_create_params(
            "disk1",
            ByteCount::from_mebibytes_u32(500),
        );
        let volume1_id = Uuid::new_v4();
        let err = datastore
            .region_allocate(&opctx, volume1_id, &params)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Not enough datasets to allocate disks"));

        assert!(matches!(err, Error::ServiceUnavailable { .. }));

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_region_allocation_out_of_space_fails() {
        let logctx =
            dev::test_setup_log("test_region_allocation_out_of_space_fails");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = db::Pool::new(&cfg);
        let datastore = Arc::new(DataStore::new(Arc::new(pool)));
        let opctx =
            OpContext::for_tests(logctx.log.new(o!()), datastore.clone());

        // Create a sled...
        let sled_id = create_test_sled(&datastore).await;

        // ... and a zpool within that sled...
        let zpool_id = create_test_zpool(&datastore, sled_id).await;

        // ... and datasets within that zpool.
        let dataset_count = REGION_REDUNDANCY_THRESHOLD;
        let bogus_addr = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8080, 0, 0);
        let dataset_ids: Vec<Uuid> =
            (0..dataset_count).map(|_| Uuid::new_v4()).collect();
        for id in &dataset_ids {
            let dataset =
                Dataset::new(*id, zpool_id, bogus_addr, DatasetKind::Crucible);
            datastore.dataset_upsert(dataset).await.unwrap();
        }

        // Allocate regions from the datasets for this disk.
        //
        // Note that we ask for a disk which is as large as the zpool,
        // so we shouldn't have space for redundancy.
        let disk_size = test_zpool_size();
        let params = create_test_disk_create_params("disk1", disk_size);
        let volume1_id = Uuid::new_v4();

        assert!(datastore
            .region_allocate(&opctx, volume1_id, &params)
            .await
            .is_err());

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    // Validate that queries which should be executable without a full table
    // scan are, in fact, runnable without a FULL SCAN.
    #[tokio::test]
    async fn test_queries_do_not_require_full_table_scan() {
        use omicron_common::api::external;
        let logctx =
            dev::test_setup_log("test_queries_do_not_require_full_table_scan");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = db::Pool::new(&cfg);
        let datastore = DataStore::new(Arc::new(pool));

        let explanation = DataStore::get_allocated_regions_query(Uuid::nil())
            .explain_async(datastore.pool())
            .await
            .unwrap();
        assert!(
            !explanation.contains("FULL SCAN"),
            "Found an unexpected FULL SCAN: {}",
            explanation
        );

        let explanation = DataStore::get_allocatable_datasets_query()
            .explain_async(datastore.pool())
            .await
            .unwrap();
        assert!(
            !explanation.contains("FULL SCAN"),
            "Found an unexpected FULL SCAN: {}",
            explanation
        );

        let subnet = db::model::VpcSubnet::new(
            Uuid::nil(),
            Uuid::nil(),
            external::IdentityMetadataCreateParams {
                name: external::Name::try_from(String::from("name")).unwrap(),
                description: String::from("description"),
            },
            external::Ipv4Net("172.30.0.0/22".parse().unwrap()),
            external::Ipv6Net("fd00::/64".parse().unwrap()),
        );
        let values = FilterConflictingVpcSubnetRangesQuery::new(subnet);
        let query =
            diesel::insert_into(db::schema::vpc_subnet::dsl::vpc_subnet)
                .values(values)
                .returning(VpcSubnet::as_returning());
        println!("{}", diesel::debug_query(&query));
        let explanation = query.explain_async(datastore.pool()).await.unwrap();
        assert!(
            !explanation.contains("FULL SCAN"),
            "Found an unexpected FULL SCAN: {}",
            explanation,
        );

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    // Test sled-specific IPv6 address allocation
    #[tokio::test]
    async fn test_sled_ipv6_address_allocation() {
        use omicron_common::address::RSS_RESERVED_ADDRESSES as STATIC_IPV6_ADDRESS_OFFSET;
        use std::net::Ipv6Addr;

        let logctx = dev::test_setup_log("test_sled_ipv6_address_allocation");
        let mut db = test_setup_database(&logctx.log).await;
        let cfg = db::Config { url: db.pg_config().clone() };
        let pool = Arc::new(db::Pool::new(&cfg));
        let datastore = Arc::new(DataStore::new(Arc::clone(&pool)));
        let opctx =
            OpContext::for_tests(logctx.log.new(o!()), datastore.clone());

        let rack_id = Uuid::new_v4();
        let addr1 = "[fd00:1de::1]:12345".parse().unwrap();
        let sled1_id = "0de4b299-e0b4-46f0-d528-85de81a7095f".parse().unwrap();
        let is_scrimlet = false;
        let sled1 = db::model::Sled::new(sled1_id, addr1, is_scrimlet, rack_id);
        datastore.sled_upsert(sled1).await.unwrap();

        let addr2 = "[fd00:1df::1]:12345".parse().unwrap();
        let sled2_id = "66285c18-0c79-43e0-e54f-95271f271314".parse().unwrap();
        let sled2 = db::model::Sled::new(sled2_id, addr2, is_scrimlet, rack_id);
        datastore.sled_upsert(sled2).await.unwrap();

        let ip = datastore.next_ipv6_address(&opctx, sled1_id).await.unwrap();
        let expected_ip = Ipv6Addr::new(
            0xfd00,
            0x1de,
            0,
            0,
            0,
            0,
            0,
            2 + STATIC_IPV6_ADDRESS_OFFSET,
        );
        assert_eq!(ip, expected_ip);
        let ip = datastore.next_ipv6_address(&opctx, sled1_id).await.unwrap();
        let expected_ip = Ipv6Addr::new(
            0xfd00,
            0x1de,
            0,
            0,
            0,
            0,
            0,
            3 + STATIC_IPV6_ADDRESS_OFFSET,
        );
        assert_eq!(ip, expected_ip);

        let ip = datastore.next_ipv6_address(&opctx, sled2_id).await.unwrap();
        let expected_ip = Ipv6Addr::new(
            0xfd00,
            0x1df,
            0,
            0,
            0,
            0,
            0,
            2 + STATIC_IPV6_ADDRESS_OFFSET,
        );
        assert_eq!(ip, expected_ip);

        let _ = db.cleanup().await;
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ssh_keys() {
        let logctx = dev::test_setup_log("test_ssh_keys");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a new Silo user so that we can lookup their keys.
        let silo_user_id = Uuid::new_v4();
        datastore
            .silo_user_create(SiloUser::new(
                *SILO_ID,
                silo_user_id,
                "external@id".into(),
            ))
            .await
            .unwrap();

        let (.., authz_user) = LookupPath::new(&opctx, &datastore)
            .silo_user_id(silo_user_id)
            .lookup_for(authz::Action::CreateChild)
            .await
            .unwrap();
        assert_eq!(authz_user.id(), silo_user_id);

        // Create a new SSH public key for the new user.
        let key_name = Name::try_from(String::from("sshkey")).unwrap();
        let public_key = "ssh-test AAAAAAAAKEY".to_string();
        let ssh_key = SshKey::new(
            silo_user_id,
            params::SshKeyCreate {
                identity: IdentityMetadataCreateParams {
                    name: key_name.clone(),
                    description: "my SSH public key".to_string(),
                },
                public_key,
            },
        );
        let created = datastore
            .ssh_key_create(&opctx, &authz_user, ssh_key.clone())
            .await
            .unwrap();
        assert_eq!(created.silo_user_id, ssh_key.silo_user_id);
        assert_eq!(created.public_key, ssh_key.public_key);

        // Lookup the key we just created.
        let (authz_silo, authz_silo_user, authz_ssh_key, found) =
            LookupPath::new(&opctx, &datastore)
                .silo_user_id(silo_user_id)
                .ssh_key_name(&key_name.into())
                .fetch()
                .await
                .unwrap();
        assert_eq!(authz_silo.id(), *SILO_ID);
        assert_eq!(authz_silo_user.id(), silo_user_id);
        assert_eq!(found.silo_user_id, ssh_key.silo_user_id);
        assert_eq!(found.public_key, ssh_key.public_key);

        // Trying to insert the same one again fails.
        let duplicate = datastore
            .ssh_key_create(&opctx, &authz_user, ssh_key.clone())
            .await;
        assert!(matches!(
            duplicate,
            Err(Error::ObjectAlreadyExists { type_name, object_name })
                if type_name == ResourceType::SshKey
                    && object_name == "sshkey"
        ));

        // Delete the key we just created.
        datastore.ssh_key_delete(&opctx, &authz_ssh_key).await.unwrap();

        // Clean up.
        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_service_upsert() {
        let logctx = dev::test_setup_log("test_service_upsert");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the service should exist.
        let sled_id = create_test_sled(&datastore).await;

        // Create a new service to exist on this sled.
        let service_id = Uuid::new_v4();
        let addr = Ipv6Addr::LOCALHOST;
        let kind = ServiceKind::Nexus;

        let service = Service::new(service_id, sled_id, addr, kind);
        let result =
            datastore.service_upsert(&opctx, service.clone()).await.unwrap();
        assert_eq!(service.id(), result.id());
        assert_eq!(service.ip, result.ip);
        assert_eq!(service.kind, result.kind);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_service() {
        let logctx = dev::test_setup_log("test_ensure_rack_service");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the service should exist.
        let sled_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let sled_id = Uuid::new_v4();
        let rack_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
        datastore
            .sled_upsert(sled)
            .await
            .expect("Should be able to upsert sled");

        // Ensure a service exists on the rack.
        let services = datastore
            .ensure_rack_service(&opctx, rack_id, ServiceKind::Nexus, 1)
            .await
            .expect("Should have allocated service");

        // Only a single service was allocated, with the type / address we
        // expect.
        assert_eq!(1, services.len());
        assert_eq!(ServiceKind::Nexus, services[0].kind);
        assert_eq!(sled_id, services[0].sled_id);

        // Listing services only shows this one.
        let observed_services = datastore
            .service_list(&opctx, sled_id)
            .await
            .expect("Should be able to list services");
        assert_eq!(1, observed_services.len());
        assert_eq!(services[0].id(), observed_services[0].id());

        // Test that ensuring services is idempotent.
        let services_again = datastore
            .ensure_rack_service(&opctx, rack_id, ServiceKind::Nexus, 1)
            .await
            .expect("Should have allocated service");
        assert_eq!(services_again, services);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_service_multi_sled() {
        let logctx = dev::test_setup_log("test_ensure_rack_service_multi_sled");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        let rack_id = Uuid::new_v4();

        // Create sleds with distinct underlay subnets.
        const SLED_COUNT: usize = 3;
        let mut sleds = HashMap::new();
        for i in 0..SLED_COUNT {
            let sled_addr = SocketAddrV6::new(
                Ipv6Addr::new(0xfd00, 0, 0, i.try_into().unwrap(), 0, 0, 0, 1),
                8080,
                0,
                0,
            );
            let sled_id = Uuid::new_v4();
            let is_scrimlet = false;
            let sled =
                Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
            datastore
                .sled_upsert(sled.clone())
                .await
                .expect("Should be able to upsert sled");
            sleds.insert(sled.id(), sled);
        }

        // Ensure a service exists on the rack, with some redundancy.
        const NEXUS_COUNT: u32 = 3;
        let mut services = datastore
            .ensure_rack_service(
                &opctx,
                rack_id,
                ServiceKind::Nexus,
                NEXUS_COUNT,
            )
            .await
            .expect("Should have allocated service");
        services.sort_by(|a, b| a.id().cmp(&b.id()));

        assert_eq!(NEXUS_COUNT, services.len() as u32);
        for svc in &services {
            assert_eq!(ServiceKind::Nexus, svc.kind);

            // Each service should have been provisioned to a distinct sled.
            let observed_services = datastore
                .service_list(&opctx, svc.sled_id)
                .await
                .expect("Should be able to list services");
            assert_eq!(1, observed_services.len());
            assert_eq!(svc.id(), observed_services[0].id());
        }

        // Test that ensuring services is idempotent.
        let mut services_again = datastore
            .ensure_rack_service(
                &opctx,
                rack_id,
                ServiceKind::Nexus,
                NEXUS_COUNT,
            )
            .await
            .expect("Should have allocated service");
        services_again.sort_by(|a, b| a.id().cmp(&b.id()));
        assert_eq!(services_again, services);

        // Ask for a different service type on the rack.
        let oximeter_services = datastore
            .ensure_rack_service(&opctx, rack_id, ServiceKind::Oximeter, 1)
            .await
            .expect("Should have allocated service");

        // This should only return a single service
        assert_eq!(1, oximeter_services.len());

        // The target sled should contain both the nexus and oximeter services
        let observed_services = datastore
            .service_list(&opctx, oximeter_services[0].sled_id)
            .await
            .expect("Should be able to list services");
        assert_eq!(2, observed_services.len());

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_service_not_enough_sleds() {
        let logctx =
            dev::test_setup_log("test_ensure_rack_service_not_enough_sleds");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the service should exist.
        let sled_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let sled_id = Uuid::new_v4();
        let rack_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
        datastore
            .sled_upsert(sled)
            .await
            .expect("Should be able to upsert sled");

        // Try to request a redundancy which is larger than the number of sleds.
        let err = datastore
            .ensure_rack_service(&opctx, rack_id, ServiceKind::Nexus, 2)
            .await
            .expect_err("Should have failed to allocate service");

        assert!(
            matches!(err, Error::ServiceUnavailable { .. }),
            "Error should have been ServiceUnavailable: {:?}",
            err
        );
        assert!(
            err.to_string().contains("Not enough sleds"),
            "Error should have identified 'Not enough sleds' as the cause: {:?}", err
        );

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_dns_service() {
        let logctx = dev::test_setup_log("test_ensure_dns_service");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the service should exist.
        let sled_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let sled_id = Uuid::new_v4();
        let rack_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
        datastore
            .sled_upsert(sled)
            .await
            .expect("Should be able to upsert sled");

        let rack_subnet = Ipv6Subnet::<RACK_PREFIX>::new(*sled_addr.ip());

        // Ensure a service exists on the rack.
        let services = datastore
            .ensure_dns_service(&opctx, rack_subnet, 1)
            .await
            .expect("Should have allocated service");

        // Only a single service was allocated, with the type / address we
        // expect.
        assert_eq!(1, services.len());
        assert_eq!(ServiceKind::InternalDNS, services[0].kind);
        assert_eq!(sled_id, services[0].sled_id);

        // Listing services only shows this one.
        let observed_services = datastore
            .service_list(&opctx, sled_id)
            .await
            .expect("Should be able to list services");
        assert_eq!(1, observed_services.len());
        assert_eq!(services[0].id(), observed_services[0].id());

        // Test that ensuring services is idempotent.
        let services_again = datastore
            .ensure_dns_service(&opctx, rack_subnet, 1)
            .await
            .expect("Should have allocated service");
        assert_eq!(services_again, services);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_dns_service_multi_sled() {
        let logctx = dev::test_setup_log("test_ensure_dns_service_multi_sled");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        let rack_id = Uuid::new_v4();

        // Create sleds with distinct underlay subnets.
        const SLED_COUNT: u32 = DNS_REDUNDANCY;
        let mut sleds = HashMap::new();
        for i in 0..SLED_COUNT {
            let sled_addr = SocketAddrV6::new(
                Ipv6Addr::new(0xfd00, 0, 0, i.try_into().unwrap(), 0, 0, 0, 1),
                8080,
                0,
                0,
            );
            let sled_id = Uuid::new_v4();
            let is_scrimlet = false;
            let sled =
                Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
            datastore
                .sled_upsert(sled.clone())
                .await
                .expect("Should be able to upsert sled");
            sleds.insert(sled.id(), sled);
        }
        let rack_subnet = Ipv6Subnet::<RACK_PREFIX>::new(Ipv6Addr::from(
            sleds.values().next().unwrap().ip,
        ));

        for sled in sleds.values() {
            assert_eq!(
                rack_subnet,
                Ipv6Subnet::<RACK_PREFIX>::new(Ipv6Addr::from(sled.ip)),
                "Test pre-condition violated: All sleds must belong to the same rack"
            );
        }

        // Ensure a service exists on the rack.
        const DNS_COUNT: u32 = DNS_REDUNDANCY;
        let mut services = datastore
            .ensure_dns_service(&opctx, rack_subnet, DNS_COUNT)
            .await
            .expect("Should have allocated service");
        services.sort_by(|a, b| a.id().cmp(&b.id()));

        assert_eq!(DNS_COUNT, services.len() as u32);
        for svc in &services {
            assert_eq!(ServiceKind::InternalDNS, svc.kind);

            // Each service should have been provisioned to a distinct sled.
            let observed_services = datastore
                .service_list(&opctx, svc.sled_id)
                .await
                .expect("Should be able to list services");
            assert_eq!(1, observed_services.len());
            assert_eq!(svc.id(), observed_services[0].id());
        }

        // Test for idempotency
        let mut services_again = datastore
            .ensure_dns_service(&opctx, rack_subnet, DNS_COUNT)
            .await
            .expect("Should have allocated service");
        services_again.sort_by(|a, b| a.id().cmp(&b.id()));
        assert_eq!(services_again, services);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_dataset() {
        let logctx = dev::test_setup_log("test_ensure_rack_dataset");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the dataset should exist.
        let sled_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let sled_id = Uuid::new_v4();
        let rack_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
        datastore
            .sled_upsert(sled)
            .await
            .expect("Should be able to upsert sled");
        let zpool_id = create_test_zpool(&datastore, sled_id).await;

        // Ensure a dataset exists on the rack.
        let output = datastore
            .ensure_rack_dataset(
                &opctx,
                rack_id,
                DatasetKind::Crucible,
                DatasetRedundancy::PerRack(1),
            )
            .await
            .expect("Should have allocated dataset");

        // Observe that only a single dataset was allocated
        assert_eq!(1, output.len());
        let (_, _, output_dataset) = &output[0];
        assert_eq!(DatasetKind::Crucible, output_dataset.kind);
        assert_eq!(zpool_id, output_dataset.pool_id);

        // Listing datasets only shows this one.
        let observed_datasets = datastore
            .dataset_list(&opctx, zpool_id)
            .await
            .expect("Should be able to list datasets");
        assert_eq!(1, observed_datasets.len());
        assert_eq!(output_dataset.id(), observed_datasets[0].id());

        // Test that ensuring datasets is idempotent.
        let output_again = datastore
            .ensure_rack_dataset(
                &opctx,
                rack_id,
                DatasetKind::Crucible,
                DatasetRedundancy::PerRack(1),
            )
            .await
            .expect("Should have allocated dataset");
        let (_, _, output_dataset_again) = &output_again[0];
        assert_eq!(output_dataset_again, output_dataset);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_dataset_not_enough_zpools() {
        let logctx =
            dev::test_setup_log("test_ensure_rack_dataset_not_enough_zpools");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a sled on which the dataset should exist.
        let sled_addr = SocketAddrV6::new(
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
            8080,
            0,
            0,
        );
        let sled_id = Uuid::new_v4();
        let rack_id = Uuid::new_v4();
        let is_scrimlet = false;
        let sled = Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
        datastore
            .sled_upsert(sled)
            .await
            .expect("Should be able to upsert sled");

        // Attempt to allocate a dataset on a rack without zpools.
        let err = datastore
            .ensure_rack_dataset(
                &opctx,
                rack_id,
                DatasetKind::Crucible,
                DatasetRedundancy::PerRack(1),
            )
            .await
            .expect_err("Should not have allocated dataset");

        assert!(
            matches!(err, Error::ServiceUnavailable { .. }),
            "Error should have been ServiceUnavailable: {:?}",
            err
        );
        assert!(
            err.to_string().contains("Not enough zpools"),
            "Error should have identified 'Not enough zpools' as the cause: {:?}", err
        );

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_ensure_rack_dataset_multi_sled() {
        let logctx = dev::test_setup_log("test_ensure_rack_dataset_multi_sled");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        let rack_id = Uuid::new_v4();

        // Create sleds with distinct underlay subnets.
        const SLED_COUNT: usize = 3;
        let mut sleds = HashMap::new();
        for i in 0..SLED_COUNT {
            let sled_addr = SocketAddrV6::new(
                Ipv6Addr::new(0xfd00, 0, 0, i.try_into().unwrap(), 0, 0, 0, 1),
                8080,
                0,
                0,
            );
            let sled_id = Uuid::new_v4();
            let is_scrimlet = false;
            let sled =
                Sled::new(sled_id, sled_addr.clone(), is_scrimlet, rack_id);
            datastore
                .sled_upsert(sled.clone())
                .await
                .expect("Should be able to upsert sled");
            sleds.insert(sled.id(), sled);
            create_test_zpool(&datastore, sled_id).await;
        }

        // Ensure datasets exist on the rack.
        let output = datastore
            .ensure_rack_dataset(
                &opctx,
                rack_id,
                DatasetKind::Crucible,
                DatasetRedundancy::OnAll,
            )
            .await
            .expect("Should have allocated dataset");
        assert_eq!(SLED_COUNT, output.len());
        for (sled, zpool, dataset) in &output {
            assert_eq!(DatasetKind::Crucible, dataset.kind);
            assert_eq!(zpool.id(), dataset.pool_id);
            assert_eq!(sled.id(), zpool.sled_id);

            let observed_datasets = datastore
                .dataset_list(&opctx, zpool.id())
                .await
                .expect("Should be able to list datasets");
            assert_eq!(1, observed_datasets.len());
            assert_eq!(dataset.id(), observed_datasets[0].id())
        }

        // Test that ensuring datasets is idempotent.
        let output_again = datastore
            .ensure_rack_dataset(
                &opctx,
                rack_id,
                DatasetKind::Crucible,
                DatasetRedundancy::OnAll,
            )
            .await
            .expect("Should have allocated dataset");

        let mut output: Vec<_> =
            output.into_iter().map(|(_, _, dataset)| dataset).collect();
        output.sort_by(|a, b| a.id().cmp(&b.id()));
        let mut output_again: Vec<_> =
            output_again.into_iter().map(|(_, _, dataset)| dataset).collect();
        output_again.sort_by(|a, b| a.id().cmp(&b.id()));
        assert_eq!(output, output_again);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_rack_initialize_is_idempotent() {
        let logctx = dev::test_setup_log("test_rack_initialize_is_idempotent");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a Rack, insert it into the DB.
        let rack = Rack::new(Uuid::new_v4());
        let result = datastore.rack_insert(&opctx, &rack).await.unwrap();
        assert_eq!(result.id(), rack.id());
        assert_eq!(result.initialized, false);

        // Re-insert the Rack (check for idempotency).
        let result = datastore.rack_insert(&opctx, &rack).await.unwrap();
        assert_eq!(result.id(), rack.id());
        assert_eq!(result.initialized, false);

        // Initialize the Rack.
        let result = datastore
            .rack_set_initialized(&opctx, rack.id(), vec![], vec![])
            .await
            .unwrap();
        assert!(result.initialized);

        // Re-initialize the rack (check for idempotency)
        let result = datastore
            .rack_set_initialized(&opctx, rack.id(), vec![], vec![])
            .await
            .unwrap();
        assert!(result.initialized);

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_table_scan() {
        let logctx = dev::test_setup_log("test_table_scan");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        let error = datastore.test_try_table_scan(&opctx).await;
        println!("error from attempted table scan: {:#}", error);
        match error {
            Error::InternalError { internal_message } => {
                assert!(internal_message.contains(
                    "contains a full table/index scan which is \
                    explicitly disallowed"
                ));
            }
            error => panic!(
                "expected internal error with specific message, found {:?}",
                error
            ),
        }

        // Clean up.
        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_deallocate_external_ip_by_instance_id_is_idempotent() {
        use crate::db::model::IpKind;
        use crate::db::schema::external_ip::dsl;

        let logctx = dev::test_setup_log(
            "test_deallocate_external_ip_by_instance_id_is_idempotent",
        );
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a few records.
        let now = Utc::now();
        let instance_id = Uuid::new_v4();
        let ips = (0..4)
            .map(|i| ExternalIp {
                id: Uuid::new_v4(),
                name: None,
                description: None,
                time_created: now,
                time_modified: now,
                time_deleted: None,
                ip_pool_id: Uuid::new_v4(),
                ip_pool_range_id: Uuid::new_v4(),
                project_id: Some(Uuid::new_v4()),
                instance_id: Some(instance_id),
                kind: IpKind::Ephemeral,
                ip: ipnetwork::IpNetwork::from(IpAddr::from(Ipv4Addr::new(
                    10, 0, 0, i,
                ))),
                first_port: crate::db::model::SqlU16(0),
                last_port: crate::db::model::SqlU16(10),
            })
            .collect::<Vec<_>>();
        diesel::insert_into(dsl::external_ip)
            .values(ips.clone())
            .execute_async(datastore.pool())
            .await
            .unwrap();

        // Delete everything, make sure we delete all records we made above
        let count = datastore
            .deallocate_external_ip_by_instance_id(&opctx, instance_id)
            .await
            .expect("Failed to delete instance external IPs");
        assert_eq!(
            count,
            ips.len(),
            "Expected to delete all IPs for the instance"
        );

        // Do it again, we should get zero records
        let count = datastore
            .deallocate_external_ip_by_instance_id(&opctx, instance_id)
            .await
            .expect("Failed to delete instance external IPs");
        assert_eq!(count, 0, "Expected to delete zero IPs for the instance");

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_deallocate_external_ip_is_idempotent() {
        use crate::db::model::IpKind;
        use crate::db::schema::external_ip::dsl;

        let logctx =
            dev::test_setup_log("test_deallocate_external_ip_is_idempotent");
        let mut db = test_setup_database(&logctx.log).await;
        let (opctx, datastore) = datastore_test(&logctx, &db).await;

        // Create a record.
        let now = Utc::now();
        let ip = ExternalIp {
            id: Uuid::new_v4(),
            name: None,
            description: None,
            time_created: now,
            time_modified: now,
            time_deleted: None,
            ip_pool_id: Uuid::new_v4(),
            ip_pool_range_id: Uuid::new_v4(),
            project_id: Some(Uuid::new_v4()),
            instance_id: Some(Uuid::new_v4()),
            kind: IpKind::SNat,
            ip: ipnetwork::IpNetwork::from(IpAddr::from(Ipv4Addr::new(
                10, 0, 0, 1,
            ))),
            first_port: crate::db::model::SqlU16(0),
            last_port: crate::db::model::SqlU16(10),
        };
        diesel::insert_into(dsl::external_ip)
            .values(ip.clone())
            .execute_async(datastore.pool())
            .await
            .unwrap();

        // Delete it twice, make sure we get the right sentinel return values.
        let deleted =
            datastore.deallocate_external_ip(&opctx, ip.id).await.unwrap();
        assert!(
            deleted,
            "Got unexpected sentinel value back when \
            deleting external IP the first time"
        );
        let deleted =
            datastore.deallocate_external_ip(&opctx, ip.id).await.unwrap();
        assert!(
            !deleted,
            "Got unexpected sentinel value back when \
            deleting external IP the second time"
        );

        // Deleting a non-existing record fails
        assert!(datastore
            .deallocate_external_ip(&opctx, Uuid::nil())
            .await
            .is_err());

        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }

    #[tokio::test]
    async fn test_external_ip_check_constraints() {
        use crate::db::model::IpKind;
        use crate::db::schema::external_ip::dsl;
        use async_bb8_diesel::ConnectionError::Query;
        use async_bb8_diesel::PoolError::Connection;
        use diesel::result::DatabaseErrorKind::CheckViolation;
        use diesel::result::Error::DatabaseError;

        let logctx = dev::test_setup_log("test_external_ip_check_constraints");
        let mut db = test_setup_database(&logctx.log).await;
        let (_opctx, datastore) = datastore_test(&logctx, &db).await;
        let now = Utc::now();

        // Create a mostly-populated record, for a floating IP
        let subnet = ipnetwork::IpNetwork::new(
            IpAddr::from(Ipv4Addr::new(10, 0, 0, 0)),
            8,
        )
        .unwrap();
        let mut addresses = subnet.iter();
        let ip = ExternalIp {
            id: Uuid::new_v4(),
            name: None,
            description: None,
            time_created: now,
            time_modified: now,
            time_deleted: None,
            ip_pool_id: Uuid::new_v4(),
            ip_pool_range_id: Uuid::new_v4(),
            project_id: Some(Uuid::new_v4()),
            instance_id: Some(Uuid::new_v4()),
            kind: IpKind::Floating,
            ip: addresses.next().unwrap().into(),
            first_port: crate::db::model::SqlU16(0),
            last_port: crate::db::model::SqlU16(10),
        };

        // Combinations of NULL and non-NULL for:
        // - name
        // - description
        // - instance UUID
        let names = [
            None,
            Some(db::model::Name(Name::try_from("foo".to_string()).unwrap())),
        ];
        let descriptions = [None, Some("foo".to_string())];
        let instance_ids = [None, Some(Uuid::new_v4())];

        // For Floating IPs, both name and description must be non-NULL
        for name in names.iter() {
            for description in descriptions.iter() {
                for instance_id in instance_ids.iter() {
                    let new_ip = ExternalIp {
                        id: Uuid::new_v4(),
                        name: name.clone(),
                        description: description.clone(),
                        ip: addresses.next().unwrap().into(),
                        instance_id: *instance_id,
                        ..ip
                    };
                    let res = diesel::insert_into(dsl::external_ip)
                        .values(new_ip)
                        .execute_async(datastore.pool())
                        .await;
                    if name.is_some() && description.is_some() {
                        // Name/description must be non-NULL, instance ID can be
                        // either
                        res.expect(
                            "Failed to insert Floating IP with valid \
                            name, description, and instance ID",
                        );
                    } else {
                        // At least one is not valid, we expect a check violation
                        let err = res.expect_err(
                            "Expected a CHECK violation when inserting a \
                            Floating IP record with NULL name and/or description",
                        );
                        assert!(
                            matches!(
                                err,
                                Connection(Query(DatabaseError(
                                    CheckViolation,
                                    _
                                )))
                            ),
                            "Expected a CHECK violation when inserting a \
                        Floating IP record with NULL name and/or description",
                        );
                    }
                }
            }
        }

        // For other IP types, both name and description must be NULL
        for kind in [IpKind::SNat, IpKind::Ephemeral].into_iter() {
            for name in names.iter() {
                for description in descriptions.iter() {
                    for instance_id in instance_ids.iter() {
                        let new_ip = ExternalIp {
                            id: Uuid::new_v4(),
                            name: name.clone(),
                            description: description.clone(),
                            kind,
                            ip: addresses.next().unwrap().into(),
                            instance_id: *instance_id,
                            ..ip
                        };
                        let res = diesel::insert_into(dsl::external_ip)
                            .values(new_ip.clone())
                            .execute_async(datastore.pool())
                            .await;
                        if name.is_none()
                            && description.is_none()
                            && instance_id.is_some()
                        {
                            // Name/description must be NULL, instance ID cannot
                            // be NULL.
                            assert!(
                                res.is_ok(),
                                "Failed to insert {:?} IP with valid \
                                name, description, and instance ID",
                                kind,
                            );
                        } else {
                            // One is not valid, we expect a check violation
                            assert!(
                                res.is_err(),
                                "Expected a CHECK violation when inserting a \
                                {:?} IP record with non-NULL name, description, \
                                and/or instance ID",
                                kind,
                            );
                            let err = res.unwrap_err();
                            assert!(
                                matches!(
                                    err,
                                    Connection(Query(DatabaseError(
                                        CheckViolation,
                                        _
                                    )))
                                ),
                                "Expected a CHECK violation when inserting a \
                            {:?} IP record with non-NULL name, description, \
                            and/or instance ID",
                                kind
                            );
                        }
                    }
                }
            }
        }
        db.cleanup().await.unwrap();
        logctx.cleanup_successful();
    }
}
