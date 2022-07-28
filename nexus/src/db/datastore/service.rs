// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods on [`Service`]s.

use super::DataStore;
use crate::authz;
use crate::context::OpContext;
use crate::db;
use crate::db::collection_insert::AsyncInsertError;
use crate::db::collection_insert::DatastoreCollection;
use crate::db::collection_insert::SyncInsertError;
use crate::db::error::public_error_from_diesel_create;
use crate::db::error::public_error_from_diesel_pool;
use crate::db::error::ErrorHandler;
use crate::db::error::TransactionError;
use crate::db::identity::Asset;
use crate::db::model::Service;
use crate::db::model::ServiceKind;
use crate::db::model::Sled;
use crate::db::pool::DbConnection;
use async_bb8_diesel::{AsyncConnection, AsyncRunQueryDsl};
use chrono::Utc;
use diesel::prelude::*;
use diesel::upsert::excluded;
use omicron_common::address::Ipv6Subnet;
use omicron_common::address::ReservedRackSubnet;
use omicron_common::address::DNS_REDUNDANCY;
use omicron_common::address::RACK_PREFIX;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::LookupType;
use omicron_common::api::external::ResourceType;
use std::net::Ipv6Addr;
use uuid::Uuid;

impl DataStore {
    /// Stores a new service in the database.
    pub async fn service_upsert(
        &self,
        opctx: &OpContext,
        service: Service,
    ) -> CreateResult<Service> {
        use db::schema::service::dsl;

        let sled_id = service.sled_id;
        Sled::insert_resource(
            sled_id,
            diesel::insert_into(dsl::service)
                .values(service.clone())
                .on_conflict(dsl::id)
                .do_update()
                .set((
                    dsl::time_modified.eq(Utc::now()),
                    dsl::sled_id.eq(excluded(dsl::sled_id)),
                    dsl::ip.eq(excluded(dsl::ip)),
                    dsl::kind.eq(excluded(dsl::kind)),
                )),
        )
        .insert_and_get_result_async(self.pool_authorized(opctx).await?)
        .await
        .map_err(|e| match e {
            AsyncInsertError::CollectionNotFound => Error::ObjectNotFound {
                type_name: ResourceType::Sled,
                lookup_type: LookupType::ById(sled_id),
            },
            AsyncInsertError::DatabaseError(e) => {
                public_error_from_diesel_pool(
                    e,
                    ErrorHandler::Conflict(
                        ResourceType::Service,
                        &service.id().to_string(),
                    ),
                )
            }
        })
    }

    fn service_upsert_sync(
        conn: &mut DbConnection,
        service: Service,
    ) -> CreateResult<Service> {
        use db::schema::service::dsl;

        let sled_id = service.sled_id;
        Sled::insert_resource(
            sled_id,
            diesel::insert_into(dsl::service)
                .values(service.clone())
                .on_conflict(dsl::id)
                .do_update()
                .set((
                    dsl::time_modified.eq(Utc::now()),
                    dsl::sled_id.eq(excluded(dsl::sled_id)),
                    dsl::ip.eq(excluded(dsl::ip)),
                    dsl::kind.eq(excluded(dsl::kind)),
                )),
        )
        .insert_and_get_result(conn)
        .map_err(|e| match e {
            SyncInsertError::CollectionNotFound => Error::ObjectNotFound {
                type_name: ResourceType::Sled,
                lookup_type: LookupType::ById(sled_id),
            },
            SyncInsertError::DatabaseError(e) => {
                public_error_from_diesel_create(
                    e,
                    ResourceType::Service,
                    &service.id().to_string(),
                )
            }
        })
    }

    fn sled_list_with_limit_sync(
        conn: &mut DbConnection,
        limit: u32,
    ) -> Result<Vec<Sled>, diesel::result::Error> {
        use db::schema::sled::dsl;
        dsl::sled
            .filter(dsl::time_deleted.is_null())
            .limit(limit as i64)
            .select(Sled::as_select())
            .load(conn)
    }

    pub async fn service_list(
        &self,
        opctx: &OpContext,
        sled_id: Uuid,
    ) -> Result<Vec<Service>, Error> {
        opctx.authorize(authz::Action::Read, &authz::FLEET).await?;
        use db::schema::service::dsl;
        dsl::service
            .filter(dsl::sled_id.eq(sled_id))
            .select(Service::as_select())
            .load_async(self.pool_authorized(opctx).await?)
            .await
            .map_err(|e| public_error_from_diesel_pool(e, ErrorHandler::Server))
    }

    fn sled_and_service_list_sync(
        conn: &mut DbConnection,
        rack_id: Uuid,
        kind: ServiceKind,
    ) -> Result<Vec<(Sled, Option<Service>)>, diesel::result::Error> {
        use db::schema::service::dsl as svc_dsl;
        use db::schema::sled::dsl as sled_dsl;

        db::schema::sled::table
            .filter(sled_dsl::time_deleted.is_null())
            .filter(sled_dsl::rack_id.eq(rack_id))
            .left_outer_join(db::schema::service::table.on(
                svc_dsl::sled_id.eq(sled_dsl::id).and(svc_dsl::kind.eq(kind)),
            ))
            .select(<(Sled, Option<Service>)>::as_select())
            .get_results(conn)
    }

    pub async fn ensure_rack_service(
        &self,
        opctx: &OpContext,
        rack_id: Uuid,
        kind: ServiceKind,
        redundancy: u32,
    ) -> Result<Vec<Service>, Error> {
        opctx.authorize(authz::Action::Read, &authz::FLEET).await?;

        #[derive(Debug)]
        enum ServiceError {
            NotEnoughSleds,
            Other(Error),
        }
        type TxnError = TransactionError<ServiceError>;

        self.pool()
            .transaction(move |conn| {
                let sleds_and_maybe_svcs =
                    Self::sled_and_service_list_sync(conn, rack_id, kind)?;

                // Split the set of returned sleds into "those with" and "those
                // without" the requested service.
                let (sleds_with_svc, sleds_without_svc): (Vec<_>, Vec<_>) =
                    sleds_and_maybe_svcs
                        .into_iter()
                        .partition(|(_, maybe_svc)| maybe_svc.is_some());
                // Identify sleds without services (targets for future
                // allocation).
                let mut sleds_without_svc =
                    sleds_without_svc.into_iter().map(|(sled, _)| sled);

                // Identify sleds with services (part of output).
                let mut svcs: Vec<_> = sleds_with_svc
                    .into_iter()
                    .map(|(_, maybe_svc)| {
                        maybe_svc.expect(
                            "Should have filtered by sleds with the service",
                        )
                    })
                    .collect();

                // Add services to sleds, in-order, until we've met a
                // number sufficient for our redundancy.
                //
                // The selection of "which sleds run this service" is completely
                // arbitrary.
                while svcs.len() < (redundancy as usize) {
                    let sled = sleds_without_svc.next().ok_or_else(|| {
                        TxnError::CustomError(ServiceError::NotEnoughSleds)
                    })?;
                    let svc_id = Uuid::new_v4();
                    let address = Self::next_ipv6_address_sync(conn, sled.id())
                        .map_err(|e| {
                            TxnError::CustomError(ServiceError::Other(e))
                        })?;

                    let service = db::model::Service::new(
                        svc_id,
                        sled.id(),
                        address,
                        kind,
                    );

                    let svc = Self::service_upsert_sync(conn, service)
                        .map_err(|e| {
                            TxnError::CustomError(ServiceError::Other(e))
                        })?;
                    svcs.push(svc);
                }

                return Ok(svcs);
            })
            .await
            .map_err(|e| match e {
                TxnError::CustomError(ServiceError::NotEnoughSleds) => {
                    Error::unavail("Not enough sleds for service allocation")
                }
                TxnError::CustomError(ServiceError::Other(e)) => e,
                TxnError::Pool(e) => {
                    public_error_from_diesel_pool(e, ErrorHandler::Server)
                }
            })
    }

    pub async fn ensure_dns_service(
        &self,
        opctx: &OpContext,
        rack_subnet: Ipv6Subnet<RACK_PREFIX>,
        redundancy: u32,
    ) -> Result<Vec<Service>, Error> {
        opctx.authorize(authz::Action::Read, &authz::FLEET).await?;

        #[derive(Debug)]
        enum ServiceError {
            NotEnoughSleds,
            NotEnoughIps,
            Other(Error),
        }
        type TxnError = TransactionError<ServiceError>;

        self.pool()
            .transaction(move |conn| {
                let mut svcs = Self::dns_service_list_sync(conn)?;

                // Get all subnets not allocated to existing services.
                let mut usable_dns_subnets = ReservedRackSubnet(rack_subnet)
                    .get_dns_subnets()
                    .into_iter()
                    .filter(|subnet| {
                        // If any existing services are using this address,
                        // skip it.
                        !svcs.iter().any(|svc| {
                            Ipv6Addr::from(svc.ip) == subnet.dns_address().ip()
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter();

                // Get all sleds which aren't already running DNS services.
                let mut target_sleds =
                    Self::sled_list_with_limit_sync(conn, redundancy)?
                        .into_iter()
                        .filter(|sled| {
                            // The target sleds are only considered if they aren't already
                            // running a DNS service.
                            svcs.iter().all(|svc| svc.sled_id != sled.id())
                        })
                        .collect::<Vec<_>>()
                        .into_iter();

                while svcs.len() < (redundancy as usize) {
                    let sled = target_sleds.next().ok_or_else(|| {
                        TxnError::CustomError(ServiceError::NotEnoughSleds)
                    })?;
                    let svc_id = Uuid::new_v4();
                    let dns_subnet =
                        usable_dns_subnets.next().ok_or_else(|| {
                            TxnError::CustomError(ServiceError::NotEnoughIps)
                        })?;
                    let address = dns_subnet.dns_address().ip();
                    let service = db::model::Service::new(
                        svc_id,
                        sled.id(),
                        address,
                        ServiceKind::InternalDNS,
                    );

                    let svc = Self::service_upsert_sync(conn, service)
                        .map_err(|e| {
                            TxnError::CustomError(ServiceError::Other(e))
                        })?;

                    svcs.push(svc);
                }
                return Ok(svcs);
            })
            .await
            .map_err(|e| match e {
                TxnError::CustomError(ServiceError::NotEnoughSleds) => {
                    Error::unavail("Not enough sleds for service allocation")
                }
                TxnError::CustomError(ServiceError::NotEnoughIps) => {
                    Error::unavail(
                        "Not enough IP addresses for service allocation",
                    )
                }
                TxnError::CustomError(ServiceError::Other(e)) => e,
                TxnError::Pool(e) => {
                    public_error_from_diesel_pool(e, ErrorHandler::Server)
                }
            })
    }

    fn dns_service_list_sync(
        conn: &mut DbConnection,
    ) -> Result<Vec<Service>, diesel::result::Error> {
        use db::schema::service::dsl as svc;

        svc::service
            .filter(svc::kind.eq(ServiceKind::InternalDNS))
            .limit(DNS_REDUNDANCY.into())
            .select(Service::as_select())
            .get_results(conn)
    }
}
