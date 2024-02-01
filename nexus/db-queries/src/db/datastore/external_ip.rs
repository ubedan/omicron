// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods on [`ExternalIp`]s.

use super::DataStore;
use crate::authz;
use crate::authz::ApiResource;
use crate::context::OpContext;
use crate::db;
use crate::db::collection_attach::AttachError;
use crate::db::collection_attach::DatastoreAttachTarget;
use crate::db::collection_detach::DatastoreDetachTarget;
use crate::db::collection_detach::DetachError;
use crate::db::error::public_error_from_diesel;
use crate::db::error::retryable;
use crate::db::error::ErrorHandler;
use crate::db::error::TransactionError;
use crate::db::lookup::LookupPath;
use crate::db::model::ExternalIp;
use crate::db::model::FloatingIp;
use crate::db::model::IncompleteExternalIp;
use crate::db::model::IpKind;
use crate::db::model::Name;
use crate::db::pagination::paginated;
use crate::db::pool::DbConnection;
use crate::db::queries::external_ip::NextExternalIp;
use crate::db::queries::external_ip::MAX_EXTERNAL_IPS_PER_INSTANCE;
use crate::db::queries::external_ip::SAFE_TO_ATTACH_INSTANCE_STATES;
use crate::db::queries::external_ip::SAFE_TO_ATTACH_INSTANCE_STATES_CREATING;
use crate::db::queries::external_ip::SAFE_TRANSIENT_INSTANCE_STATES;
use crate::db::update_and_check::UpdateAndCheck;
use crate::db::update_and_check::UpdateStatus;
use async_bb8_diesel::AsyncRunQueryDsl;
use chrono::Utc;
use diesel::prelude::*;
use nexus_db_model::Instance;
use nexus_db_model::IpAttachState;
use nexus_types::external_api::params;
use nexus_types::identity::Resource;
use omicron_common::api::external::http_pagination::PaginatedBy;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::ResourceType;
use omicron_common::api::external::UpdateResult;
use ref_cast::RefCast;
use std::net::IpAddr;
use uuid::Uuid;

const MAX_EXTERNAL_IPS_PLUS_SNAT: u32 = MAX_EXTERNAL_IPS_PER_INSTANCE + 1;

impl DataStore {
    /// Create an external IP address for source NAT for an instance.
    pub async fn allocate_instance_snat_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        instance_id: Uuid,
        pool_id: Uuid,
    ) -> CreateResult<ExternalIp> {
        let data = IncompleteExternalIp::for_instance_source_nat(
            ip_id,
            instance_id,
            pool_id,
        );
        self.allocate_external_ip(opctx, data).await
    }

    /// Create an Ephemeral IP address for an instance.
    ///
    /// For consistency between instance create and External IP attach/detach
    /// operations, this IP will be created in the `Attaching` state to block
    /// concurrent access.
    /// Callers must call `external_ip_complete_op` on saga completion to move
    /// the IP to `Attached`.
    ///
    /// To better handle idempotent attachment, this method returns an
    /// additional bool:
    /// - true: EIP was detached or attaching. proceed with saga.
    /// - false: EIP was attached. No-op for remainder of saga.
    pub async fn allocate_instance_ephemeral_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        instance_id: Uuid,
        pool: Option<authz::IpPool>,
        creating_instance: bool,
    ) -> CreateResult<(ExternalIp, bool)> {
        // This is slightly hacky: we need to create an unbound ephemeral IP, and
        // then attempt to bind it to respect two separate constraints:
        // - At most one Ephemeral IP per instance
        // - At most MAX external IPs per instance
        // Naturally, we now *need* to destroy the ephemeral IP if the newly alloc'd
        // IP was not attached, including on idempotent success.
        let pool = match pool {
            Some(authz_pool) => {
                let (.., pool) = LookupPath::new(opctx, &self)
                    .ip_pool_id(authz_pool.id())
                    // any authenticated user can CreateChild on an IP pool. this is
                    // meant to represent allocating an IP
                    .fetch_for(authz::Action::CreateChild)
                    .await?;

                // If this pool is not linked to the current silo, 404
                // As name resolution happens one layer up, we need to use the *original*
                // authz Pool.
                if self.ip_pool_fetch_link(opctx, pool.id()).await.is_err() {
                    return Err(authz_pool.not_found());
                }

                pool
            }
            // If no name given, use the default logic
            None => {
                let (.., pool) = self.ip_pools_fetch_default(&opctx).await?;
                pool
            }
        };

        let pool_id = pool.identity.id;
        let data = IncompleteExternalIp::for_ephemeral(ip_id, pool_id);

        // We might not be able to acquire a new IP, but in the event of an
        // idempotent or double attach this failure is allowed.
        let temp_ip = self.allocate_external_ip(opctx, data).await;
        if let Err(e) = temp_ip {
            let eip = self
                .instance_lookup_ephemeral_ip(opctx, instance_id)
                .await?
                .ok_or(e)?;

            return Ok((eip, false));
        }
        let temp_ip = temp_ip?;

        match self
            .begin_attach_ip(
                opctx,
                temp_ip.id,
                instance_id,
                IpKind::Ephemeral,
                creating_instance,
            )
            .await
        {
            Err(e) => {
                self.deallocate_external_ip(opctx, temp_ip.id).await?;
                Err(e)
            }
            // Idempotent case: attach failed due to a caught UniqueViolation.
            Ok(None) => {
                self.deallocate_external_ip(opctx, temp_ip.id).await?;
                let eip = self
                    .instance_lookup_ephemeral_ip(opctx, instance_id)
                    .await?
                    .ok_or_else(|| Error::internal_error(
                        "failed to lookup current ephemeral IP for idempotent attach"
                    ))?;
                let do_saga = eip.state != IpAttachState::Attached;
                Ok((eip, do_saga))
            }
            Ok(Some(v)) => Ok(v),
        }
    }

    /// Fetch all external IP addresses of any kind for the provided service.
    pub async fn service_lookup_external_ips(
        &self,
        opctx: &OpContext,
        service_id: Uuid,
    ) -> LookupResult<Vec<ExternalIp>> {
        use db::schema::external_ip::dsl;
        dsl::external_ip
            .filter(dsl::is_service.eq(true))
            .filter(dsl::parent_id.eq(service_id))
            .filter(dsl::time_deleted.is_null())
            .select(ExternalIp::as_select())
            .get_results_async(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Allocates an IP address for internal service usage.
    pub async fn allocate_service_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        name: &Name,
        description: &str,
        service_id: Uuid,
    ) -> CreateResult<ExternalIp> {
        let (.., pool) = self.ip_pools_service_lookup(opctx).await?;

        let data = IncompleteExternalIp::for_service(
            ip_id,
            name,
            description,
            service_id,
            pool.id(),
        );
        self.allocate_external_ip(opctx, data).await
    }

    /// Allocates an SNAT IP address for internal service usage.
    pub async fn allocate_service_snat_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        service_id: Uuid,
    ) -> CreateResult<ExternalIp> {
        let (.., pool) = self.ip_pools_service_lookup(opctx).await?;

        let data = IncompleteExternalIp::for_service_snat(
            ip_id,
            service_id,
            pool.id(),
        );
        self.allocate_external_ip(opctx, data).await
    }

    /// Allocates a floating IP address for instance usage.
    pub async fn allocate_floating_ip(
        &self,
        opctx: &OpContext,
        project_id: Uuid,
        params: params::FloatingIpCreate,
        pool: Option<authz::IpPool>,
    ) -> CreateResult<ExternalIp> {
        let ip_id = Uuid::new_v4();

        // This implements the same pattern as in `allocate_instance_ephemeral_ip` to
        // check that a chosen pool is valid from within the current silo.
        let pool = match pool {
            Some(authz_pool) => {
                let (.., pool) = LookupPath::new(opctx, &self)
                    .ip_pool_id(authz_pool.id())
                    .fetch_for(authz::Action::CreateChild)
                    .await?;

                if self.ip_pool_fetch_link(opctx, pool.id()).await.is_err() {
                    return Err(authz_pool.not_found());
                }

                pool
            }
            // If no name given, use the default logic
            None => {
                let (.., pool) = self.ip_pools_fetch_default(&opctx).await?;
                pool
            }
        };

        let pool_id = pool.id();

        let data = if let Some(ip) = params.address {
            IncompleteExternalIp::for_floating_explicit(
                ip_id,
                &Name(params.identity.name),
                &params.identity.description,
                project_id,
                ip,
                pool_id,
            )
        } else {
            IncompleteExternalIp::for_floating(
                ip_id,
                &Name(params.identity.name),
                &params.identity.description,
                project_id,
                pool_id,
            )
        };

        self.allocate_external_ip(opctx, data).await
    }

    async fn allocate_external_ip(
        &self,
        opctx: &OpContext,
        data: IncompleteExternalIp,
    ) -> CreateResult<ExternalIp> {
        let conn = self.pool_connection_authorized(opctx).await?;
        let ip = Self::allocate_external_ip_on_connection(&conn, data).await?;
        Ok(ip)
    }

    /// Variant of [Self::allocate_external_ip] which may be called from a
    /// transaction context.
    pub(crate) async fn allocate_external_ip_on_connection(
        conn: &async_bb8_diesel::Connection<DbConnection>,
        data: IncompleteExternalIp,
    ) -> Result<ExternalIp, TransactionError<Error>> {
        use diesel::result::DatabaseErrorKind::UniqueViolation;
        // Name needs to be cloned out here (if present) to give users a
        // sensible error message on name collision.
        let name = data.name().clone();
        let explicit_ip = data.explicit_ip().is_some();
        NextExternalIp::new(data).get_result_async(conn).await.map_err(|e| {
            use diesel::result::Error::DatabaseError;
            use diesel::result::Error::NotFound;
            match e {
                NotFound => {
                    if explicit_ip {
                        TransactionError::CustomError(Error::invalid_request(
                            "Requested external IP address not available",
                        ))
                    } else {
                        TransactionError::CustomError(
                            Error::insufficient_capacity(
                                "No external IP addresses available",
                                "NextExternalIp::new returned NotFound",
                            ),
                        )
                    }
                }
                // Floating IP: name conflict
                DatabaseError(UniqueViolation, ..) if name.is_some() => {
                    TransactionError::CustomError(public_error_from_diesel(
                        e,
                        ErrorHandler::Conflict(
                            ResourceType::FloatingIp,
                            name.as_ref()
                                .map(|m| m.as_str())
                                .unwrap_or_default(),
                        ),
                    ))
                }
                _ => {
                    if retryable(&e) {
                        return TransactionError::Database(e);
                    }
                    TransactionError::CustomError(
                        crate::db::queries::external_ip::from_diesel(e),
                    )
                }
            }
        })
    }

    /// Allocates an explicit Floating IP address for an internal service.
    ///
    /// Unlike the other IP allocation requests, this does not search for an
    /// available IP address, it asks for one explicitly.
    pub async fn allocate_explicit_service_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        name: &Name,
        description: &str,
        service_id: Uuid,
        ip: IpAddr,
    ) -> CreateResult<ExternalIp> {
        let (.., pool) = self.ip_pools_service_lookup(opctx).await?;
        let data = IncompleteExternalIp::for_service_explicit(
            ip_id,
            name,
            description,
            service_id,
            pool.id(),
            ip,
        );
        self.allocate_external_ip(opctx, data).await
    }

    /// Allocates an explicit SNAT IP address for an internal service.
    ///
    /// Unlike the other IP allocation requests, this does not search for an
    /// available IP address, it asks for one explicitly.
    pub async fn allocate_explicit_service_snat_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        service_id: Uuid,
        ip: IpAddr,
        port_range: (u16, u16),
    ) -> CreateResult<ExternalIp> {
        let (.., pool) = self.ip_pools_service_lookup(opctx).await?;
        let data = IncompleteExternalIp::for_service_explicit_snat(
            ip_id,
            service_id,
            pool.id(),
            ip,
            port_range,
        );
        self.allocate_external_ip(opctx, data).await
    }

    /// Attempt to move a target external IP from detached to attaching,
    /// checking that its parent instance does not have too many addresses
    /// and is in a valid state.
    ///
    /// Returns the `ExternalIp` which was modified, where possible. This
    /// is only nullable when trying to double-attach ephemeral IPs.
    /// To better handle idempotent attachment, this method returns an
    /// additional bool:
    /// - true: EIP was detached or attaching. proceed with saga.
    /// - false: EIP was attached. No-op for remainder of saga.
    async fn begin_attach_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        instance_id: Uuid,
        kind: IpKind,
        creating_instance: bool,
    ) -> Result<Option<(ExternalIp, bool)>, Error> {
        use db::schema::external_ip::dsl;
        use db::schema::external_ip::table;
        use db::schema::instance::dsl as inst_dsl;
        use db::schema::instance::table as inst_table;
        use diesel::result::DatabaseErrorKind::UniqueViolation;
        use diesel::result::Error::DatabaseError;

        let safe_states = if creating_instance {
            &SAFE_TO_ATTACH_INSTANCE_STATES_CREATING[..]
        } else {
            &SAFE_TO_ATTACH_INSTANCE_STATES[..]
        };

        let query = Instance::attach_resource(
            instance_id,
            ip_id,
            inst_table
                .into_boxed()
                .filter(inst_dsl::state.eq_any(safe_states))
                .filter(inst_dsl::migration_id.is_null()),
            table
                .into_boxed()
                .filter(dsl::state.eq(IpAttachState::Detached))
                .filter(dsl::kind.eq(kind))
                .filter(dsl::parent_id.is_null()),
            MAX_EXTERNAL_IPS_PLUS_SNAT,
            diesel::update(dsl::external_ip).set((
                dsl::parent_id.eq(Some(instance_id)),
                dsl::time_modified.eq(Utc::now()),
                dsl::state.eq(IpAttachState::Attaching),
            )),
        );

        let mut do_saga = true;
        query.attach_and_get_result_async(&*self.pool_connection_authorized(opctx).await?)
        .await
        .map(|(_, resource)| Some(resource))
        .or_else(|e: AttachError<ExternalIp, _, _>| match e {
            AttachError::CollectionNotFound => {
                Err(Error::not_found_by_id(
                    ResourceType::Instance,
                    &instance_id,
                ))
            },
            AttachError::ResourceNotFound => {
                Err(if kind == IpKind::Ephemeral {
                    Error::internal_error("call-scoped ephemeral IP was lost")
                } else {
                    Error::not_found_by_id(
                        ResourceType::FloatingIp,
                        &ip_id,
                    )
                })
            },
            AttachError::NoUpdate { attached_count, resource, collection } => {
                match resource.state {
                    // Idempotent errors: is in progress or complete for same resource pair -- this is fine.
                    IpAttachState::Attaching if resource.parent_id == Some(instance_id) =>
                        return Ok(Some(resource)),
                    IpAttachState::Attached if resource.parent_id == Some(instance_id) => {
                        do_saga = false;
                        return Ok(Some(resource))
                    },
                    IpAttachState::Attached =>
                        return Err(Error::invalid_request(&format!(
                        "{kind} IP cannot be attached to one \
                         instance while still attached to another"
                    ))),
                    // User can reattempt depending on how the current saga unfolds.
                    // NB; only floating IP can return this case, eph will return
                    // a UniqueViolation.
                    IpAttachState::Attaching | IpAttachState::Detaching
                        => return Err(Error::unavail(&format!(
                        "tried to attach {kind} IP mid-attach/detach: \
                         attach will be safe to retry once operation on \
                         same IP resource completes"
                    ))),

                    IpAttachState::Detached => {},
                }

                if collection.runtime_state.migration_id.is_some() {
                    return Err(Error::unavail(&format!(
                        "tried to attach {kind} IP while instance was migrating: \
                         detach will be safe to retry once migrate completes"
                    )))
                }

                Err(match &collection.runtime_state.nexus_state {
                    state if SAFE_TRANSIENT_INSTANCE_STATES.contains(&state)
                        => Error::unavail(&format!(
                        "tried to attach {kind} IP while instance was changing state: \
                         attach will be safe to retry once start/stop completes"
                    )),
                    state if SAFE_TO_ATTACH_INSTANCE_STATES.contains(&state) => {
                        if attached_count >= MAX_EXTERNAL_IPS_PLUS_SNAT as i64 {
                            Error::invalid_request(&format!(
                                "an instance may not have more than \
                                {MAX_EXTERNAL_IPS_PER_INSTANCE} external IP addresses",
                            ))
                        } else {
                            Error::internal_error(&format!("failed to attach {kind} IP"))
                        }
                    },
                    state => Error::invalid_request(&format!(
                        "cannot attach {kind} IP to instance in {state} state"
                    )),
                })
            },
            // This case occurs for both currently attaching and attached ephemeral IPs:
            AttachError::DatabaseError(DatabaseError(UniqueViolation, ..))
                if kind == IpKind::Ephemeral => {
                Ok(None)
            },
            AttachError::DatabaseError(e) => {
                Err(public_error_from_diesel(e, ErrorHandler::Server))
            },
        })
        .map(|eip| eip.map(|v| (v, do_saga)))
    }

    /// Attempt to move a target external IP from attached to detaching,
    /// checking that its parent instance is in a valid state.
    ///
    /// Returns the `ExternalIp` which was modified, where possible. This
    /// is only nullable when trying to double-detach ephemeral IPs.
    /// To better handle idempotent attachment, this method returns an
    /// additional bool:
    /// - true: EIP was detached or attaching. proceed with saga.
    /// - false: EIP was attached. No-op for remainder of saga.
    async fn begin_detach_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        instance_id: Uuid,
        kind: IpKind,
        creating_instance: bool,
    ) -> UpdateResult<Option<(ExternalIp, bool)>> {
        use db::schema::external_ip::dsl;
        use db::schema::external_ip::table;
        use db::schema::instance::dsl as inst_dsl;
        use db::schema::instance::table as inst_table;

        let safe_states = if creating_instance {
            &SAFE_TO_ATTACH_INSTANCE_STATES_CREATING[..]
        } else {
            &SAFE_TO_ATTACH_INSTANCE_STATES[..]
        };

        let query = Instance::detach_resource(
            instance_id,
            ip_id,
            inst_table
                .into_boxed()
                .filter(inst_dsl::state.eq_any(safe_states))
                .filter(inst_dsl::migration_id.is_null()),
            table
                .into_boxed()
                .filter(dsl::state.eq(IpAttachState::Attached))
                .filter(dsl::kind.eq(kind)),
            diesel::update(dsl::external_ip).set((
                dsl::time_modified.eq(Utc::now()),
                dsl::state.eq(IpAttachState::Detaching),
            )),
        );

        let mut do_saga = true;
        query.detach_and_get_result_async(&*self.pool_connection_authorized(opctx).await?)
        .await
        .map(Some)
        .or_else(|e: DetachError<ExternalIp, _, _>| Err(match e {
            DetachError::CollectionNotFound => {
                Error::not_found_by_id(
                    ResourceType::Instance,
                    &instance_id,
                )
            },
            DetachError::ResourceNotFound => {
                if kind == IpKind::Ephemeral {
                    return Ok(None);
                } else {
                    Error::not_found_by_id(
                        ResourceType::FloatingIp,
                        &ip_id,
                    )
                }
            },
            DetachError::NoUpdate { resource, collection } => {
                let parent_match = resource.parent_id == Some(instance_id);
                match resource.state {
                    // Idempotent cases: already detached OR detaching from same instance.
                    IpAttachState::Detached => {
                        do_saga = false;
                        return Ok(Some(resource))
                    },
                    IpAttachState::Detaching if parent_match => return Ok(Some(resource)),
                    IpAttachState::Attached if !parent_match
                        => return Err(Error::invalid_request(&format!(
                        "{kind} IP is not attached to the target instance",
                    ))),
                    // User can reattempt depending on how the current saga unfolds.
                    IpAttachState::Attaching
                        | IpAttachState::Detaching => return Err(Error::unavail(&format!(
                        "tried to detach {kind} IP mid-attach/detach: \
                         detach will be safe to retry once operation on \
                         same IP resource completes"
                    ))),
                    IpAttachState::Attached => {},
                }

                if collection.runtime_state.migration_id.is_some() {
                    return Err(Error::unavail(&format!(
                        "tried to detach {kind} IP while instance was migrating: \
                         detach will be safe to retry once migrate completes"
                    )))
                }

                match collection.runtime_state.nexus_state {
                    state if SAFE_TRANSIENT_INSTANCE_STATES.contains(&state) => Error::unavail(&format!(
                        "tried to attach {kind} IP while instance was changing state: \
                         detach will be safe to retry once start/stop completes"
                    )),
                    state if SAFE_TO_ATTACH_INSTANCE_STATES.contains(&state) => {
                        Error::internal_error(&format!("failed to detach {kind} IP"))
                    },
                    state => Error::invalid_request(&format!(
                        "cannot detach {kind} IP from instance in {state} state"
                    )),
                }
            },
            DetachError::DatabaseError(e) => {
                public_error_from_diesel(e, ErrorHandler::Server)
            },

        }))
        .map(|eip| eip.map(|v| (v, do_saga)))
    }

    /// Deallocate the external IP address with the provided ID. This is a complete
    /// removal of the IP entry, in contrast with `begin_deallocate_ephemeral_ip`,
    /// and should only be used for SNAT entries or cleanup of short-lived ephemeral
    /// IPs on failure.
    ///
    /// To support idempotency, such as in saga operations, this method returns
    /// an extra boolean, rather than the usual `DeleteResult`. The meaning of
    /// return values are:
    /// - `Ok(true)`: The record was deleted during this call
    /// - `Ok(false)`: The record was already deleted, such as by a previous
    /// call
    /// - `Err(_)`: Any other condition, including a non-existent record.
    pub async fn deallocate_external_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
    ) -> Result<bool, Error> {
        use db::schema::external_ip::dsl;
        let now = Utc::now();
        diesel::update(dsl::external_ip)
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::id.eq(ip_id))
            .set(dsl::time_deleted.eq(now))
            .check_if_exists::<ExternalIp>(ip_id)
            .execute_and_check(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map(|r| match r.status {
                UpdateStatus::Updated => true,
                UpdateStatus::NotUpdatedButExists => false,
            })
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Moves an instance's ephemeral IP from 'Attached' to 'Detaching'.
    ///
    /// To support idempotency, this method will succeed if the instance
    /// has no ephemeral IP or one is actively being removed. As a result,
    /// information on an actual `ExternalIp` is best-effort.
    pub async fn begin_deallocate_ephemeral_ip(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<ExternalIp>, Error> {
        let _ = LookupPath::new(&opctx, self)
            .instance_id(instance_id)
            .lookup_for(authz::Action::Modify)
            .await?;

        self.begin_detach_ip(
            opctx,
            ip_id,
            instance_id,
            IpKind::Ephemeral,
            false,
        )
        .await
        .map(|res| res.map(|(ip, _do_saga)| ip))
    }

    /// Delete all non-floating IP addresses associated with the provided instance
    /// ID.
    ///
    /// This method returns the number of records deleted, rather than the usual
    /// `DeleteResult`. That's mostly useful for tests, but could be important
    /// if callers have some invariants they'd like to check.
    pub async fn deallocate_external_ip_by_instance_id(
        &self,
        opctx: &OpContext,
        instance_id: Uuid,
    ) -> Result<usize, Error> {
        use db::schema::external_ip::dsl;
        let now = Utc::now();
        diesel::update(dsl::external_ip)
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::is_service.eq(false))
            .filter(dsl::parent_id.eq(instance_id))
            .filter(dsl::kind.ne(IpKind::Floating))
            .set((
                dsl::time_deleted.eq(now),
                dsl::state.eq(IpAttachState::Detached),
            ))
            .execute_async(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Detach all Floating IP address from their parent instance.
    ///
    /// As in `deallocate_external_ip_by_instance_id`, this method returns the
    /// number of records altered, rather than an `UpdateResult`.
    ///
    /// This method ignores ongoing state transitions, and is only safely
    /// usable from within the instance_delete saga.
    pub async fn detach_floating_ips_by_instance_id(
        &self,
        opctx: &OpContext,
        instance_id: Uuid,
    ) -> Result<usize, Error> {
        use db::schema::external_ip::dsl;
        diesel::update(dsl::external_ip)
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::is_service.eq(false))
            .filter(dsl::parent_id.eq(instance_id))
            .filter(dsl::kind.eq(IpKind::Floating))
            .set((
                dsl::time_modified.eq(Utc::now()),
                dsl::parent_id.eq(Option::<Uuid>::None),
                dsl::state.eq(IpAttachState::Detached),
            ))
            .execute_async(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Fetch all external IP addresses of any kind for the provided instance
    /// in all attachment states.
    pub async fn instance_lookup_external_ips(
        &self,
        opctx: &OpContext,
        instance_id: Uuid,
    ) -> LookupResult<Vec<ExternalIp>> {
        use db::schema::external_ip::dsl;
        dsl::external_ip
            .filter(dsl::is_service.eq(false))
            .filter(dsl::parent_id.eq(instance_id))
            .filter(dsl::time_deleted.is_null())
            .select(ExternalIp::as_select())
            .get_results_async(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Fetch the ephmeral IP address assigned to the provided instance, if this
    /// has been configured.
    pub async fn instance_lookup_ephemeral_ip(
        &self,
        opctx: &OpContext,
        instance_id: Uuid,
    ) -> LookupResult<Option<ExternalIp>> {
        Ok(self
            .instance_lookup_external_ips(opctx, instance_id)
            .await?
            .into_iter()
            .find(|v| v.kind == IpKind::Ephemeral))
    }

    /// Fetch all Floating IP addresses for the provided project.
    pub async fn floating_ips_list(
        &self,
        opctx: &OpContext,
        authz_project: &authz::Project,
        pagparams: &PaginatedBy<'_>,
    ) -> ListResultVec<FloatingIp> {
        use db::schema::floating_ip::dsl;

        opctx.authorize(authz::Action::ListChildren, authz_project).await?;

        match pagparams {
            PaginatedBy::Id(pagparams) => {
                paginated(dsl::floating_ip, dsl::id, &pagparams)
            }
            PaginatedBy::Name(pagparams) => paginated(
                dsl::floating_ip,
                dsl::name,
                &pagparams.map_name(|n| Name::ref_cast(n)),
            ),
        }
        .filter(dsl::project_id.eq(authz_project.id()))
        .filter(dsl::time_deleted.is_null())
        .select(FloatingIp::as_select())
        .get_results_async(&*self.pool_connection_authorized(opctx).await?)
        .await
        .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }

    /// Delete a Floating IP, verifying first that it is not in use.
    pub async fn floating_ip_delete(
        &self,
        opctx: &OpContext,
        authz_fip: &authz::FloatingIp,
    ) -> DeleteResult {
        use db::schema::external_ip::dsl;

        opctx.authorize(authz::Action::Delete, authz_fip).await?;

        let now = Utc::now();
        let result = diesel::update(dsl::external_ip)
            .filter(dsl::id.eq(authz_fip.id()))
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::parent_id.is_null())
            .filter(dsl::state.eq(IpAttachState::Detached))
            .set(dsl::time_deleted.eq(now))
            .check_if_exists::<ExternalIp>(authz_fip.id())
            .execute_and_check(&*self.pool_connection_authorized(opctx).await?)
            .await
            .map_err(|e| {
                public_error_from_diesel(
                    e,
                    ErrorHandler::NotFoundByResource(authz_fip),
                )
            })?;

        match result.status {
            // Verify this FIP is not attached to any instances/services.
            UpdateStatus::NotUpdatedButExists if result.found.parent_id.is_some() => Err(Error::invalid_request(
                "Floating IP cannot be deleted while attached to an instance",
            )),
            // Only remaining cause of `NotUpdated` is earlier soft-deletion.
            // Return success in this case to maintain idempotency.
            UpdateStatus::Updated | UpdateStatus::NotUpdatedButExists => Ok(()),
        }
    }

    /// Attaches a Floating IP address to an instance.
    ///
    /// This moves a floating IP into the 'attaching' state. Callers are
    /// responsible for calling `external_ip_complete_op` to finalise the
    /// IP in 'attached' state at saga completion.
    ///
    /// To better handle idempotent attachment, this method returns an
    /// additional bool:
    /// - true: EIP was detached or attaching. proceed with saga.
    /// - false: EIP was attached. No-op for remainder of saga.
    pub async fn floating_ip_begin_attach(
        &self,
        opctx: &OpContext,
        authz_fip: &authz::FloatingIp,
        instance_id: Uuid,
        creating_instance: bool,
    ) -> UpdateResult<(ExternalIp, bool)> {
        let (.., authz_instance) = LookupPath::new(&opctx, self)
            .instance_id(instance_id)
            .lookup_for(authz::Action::Modify)
            .await?;

        opctx.authorize(authz::Action::Modify, authz_fip).await?;
        opctx.authorize(authz::Action::Modify, &authz_instance).await?;

        self.begin_attach_ip(
            opctx,
            authz_fip.id(),
            instance_id,
            IpKind::Floating,
            creating_instance,
        )
        .await
        .and_then(|v| {
            v.ok_or_else(|| {
                Error::internal_error(
                    "floating IP should never return `None` from begin_attach",
                )
            })
        })
    }

    /// Detaches a Floating IP address from an instance.
    ///
    /// This moves a floating IP into the 'detaching' state. Callers are
    /// responsible for calling `external_ip_complete_op` to finalise the
    /// IP in 'detached' state at saga completion.
    ///
    /// To better handle idempotent detachment, this method returns an
    /// additional bool:
    /// - true: EIP was attached or detaching. proceed with saga.
    /// - false: EIP was detached. No-op for remainder of saga.
    pub async fn floating_ip_begin_detach(
        &self,
        opctx: &OpContext,
        authz_fip: &authz::FloatingIp,
        instance_id: Uuid,
        creating_instance: bool,
    ) -> UpdateResult<(ExternalIp, bool)> {
        let (.., authz_instance) = LookupPath::new(&opctx, self)
            .instance_id(instance_id)
            .lookup_for(authz::Action::Modify)
            .await?;

        opctx.authorize(authz::Action::Modify, authz_fip).await?;
        opctx.authorize(authz::Action::Modify, &authz_instance).await?;

        self.begin_detach_ip(
            opctx,
            authz_fip.id(),
            instance_id,
            IpKind::Floating,
            creating_instance,
        )
        .await
        .and_then(|v| {
            v.ok_or_else(|| {
                Error::internal_error(
                    "floating IP should never return `None` from begin_detach",
                )
            })
        })
    }

    /// Move an external IP from a transitional state (attaching, detaching)
    /// to its intended end state.
    ///
    /// Returns the number of rows modified, this may be zero on:
    ///  - instance delete by another saga
    ///  - saga action rerun
    ///
    /// This is valid in both cases for idempotency.
    pub async fn external_ip_complete_op(
        &self,
        opctx: &OpContext,
        ip_id: Uuid,
        ip_kind: IpKind,
        expected_state: IpAttachState,
        target_state: IpAttachState,
    ) -> Result<usize, Error> {
        use db::schema::external_ip::dsl;

        if matches!(
            expected_state,
            IpAttachState::Attached | IpAttachState::Detached
        ) {
            return Err(Error::internal_error(&format!(
                "{expected_state:?} is not a valid transition state for attach/detach"
            )));
        }

        let part_out = diesel::update(dsl::external_ip)
            .filter(dsl::id.eq(ip_id))
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::state.eq(expected_state));

        let now = Utc::now();
        let conn = self.pool_connection_authorized(opctx).await?;
        match (ip_kind, expected_state, target_state) {
            (IpKind::SNat, _, _) => return Err(Error::internal_error(
                "SNAT should not be removed via `external_ip_complete_op`, \
                    use `deallocate_external_ip`",
            )),

            (IpKind::Ephemeral, _, IpAttachState::Detached) => {
                part_out
                    .set((
                        dsl::parent_id.eq(Option::<Uuid>::None),
                        dsl::time_modified.eq(now),
                        dsl::time_deleted.eq(now),
                        dsl::state.eq(target_state),
                    ))
                    .execute_async(&*conn)
                    .await
            }

            (IpKind::Floating, _, IpAttachState::Detached) => {
                part_out
                    .set((
                        dsl::parent_id.eq(Option::<Uuid>::None),
                        dsl::time_modified.eq(now),
                        dsl::state.eq(target_state),
                    ))
                    .execute_async(&*conn)
                    .await
            }

            // Attaching->Attached gets separate logic because we choose to fail
            // and unwind on instance delete. This covers two cases:
            // - External IP is deleted.
            // - Floating IP is suddenly `detached`.
            (_, IpAttachState::Attaching, IpAttachState::Attached) => {
                return part_out
                    .set((
                        dsl::time_modified.eq(Utc::now()),
                        dsl::state.eq(target_state),
                    ))
                    .check_if_exists::<ExternalIp>(ip_id)
                    .execute_and_check(
                        &*self.pool_connection_authorized(opctx).await?,
                    )
                    .await
                    .map_err(|e| {
                        public_error_from_diesel(e, ErrorHandler::Server)
                    })
                    .and_then(|r| match r.status {
                        UpdateStatus::Updated => Ok(1),
                        UpdateStatus::NotUpdatedButExists
                            if r.found.state == IpAttachState::Detached
                                || r.found.time_deleted.is_some() =>
                        {
                            Err(Error::internal_error(
                                "unwinding due to concurrent instance delete",
                            ))
                        }
                        UpdateStatus::NotUpdatedButExists => Ok(0),
                    })
            }

            // Unwind from failed detach.
            (_, _, IpAttachState::Attached) => {
                part_out
                    .set((
                        dsl::time_modified.eq(Utc::now()),
                        dsl::state.eq(target_state),
                    ))
                    .execute_async(&*conn)
                    .await
            }
            _ => return Err(Error::internal_error("unreachable")),
        }
        .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))
    }
}
