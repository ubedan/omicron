// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rack management

use crate::authz;
use crate::context::OpContext;
use crate::db;
use crate::db::lookup::LookupPath;
use crate::internal_api::params::RackInitializationRequest;
use omicron_common::api::external::DataPageParams;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use uuid::Uuid;

impl super::Nexus {
    pub async fn racks_list(
        &self,
        opctx: &OpContext,
        pagparams: &DataPageParams<'_, Uuid>,
    ) -> ListResultVec<db::model::Rack> {
        self.db_datastore.rack_list(&opctx, pagparams).await
    }

    // TODO: Use this, instead of the manual one.
    //
    // Note that this will require insertion of the rack to occur
    // during the "populate" steps.
    /*
    pub async fn rack_lookup(
        &self,
        opctx: &OpContext,
        rack_id: &Uuid,
    ) -> LookupResult<db::model::Rack> {
        let (.., db_rack) = LookupPath::new(opctx, &self.db_datastore)
            .rack_id(*rack_id)
            .fetch()
            .await?;
        Ok(db_rack)
    }
    */

    pub async fn rack_lookup(
        &self,
        opctx: &OpContext,
        rack_id: &Uuid,
    ) -> LookupResult<db::model::Rack> {
        self.db_datastore.rack_lookup_manual(opctx, *rack_id).await
    }

    /// Ensures that a rack exists in the DB.
    ///
    /// If the rack already exists, this function is a no-op.
    pub async fn rack_insert(
        &self,
        opctx: &OpContext,
        rack_id: Uuid,
    ) -> Result<(), Error> {
        self.datastore()
            .rack_insert(opctx, &db::model::Rack::new(rack_id))
            .await?;
        Ok(())
    }

    /// Marks the rack as initialized with a set of services.
    ///
    /// This function is a no-op if the rack has already been initialized.
    pub async fn rack_initialize(
        &self,
        opctx: &OpContext,
        rack_id: Uuid,
        request: RackInitializationRequest,
    ) -> Result<(), Error> {
        opctx.authorize(authz::Action::Modify, &authz::FLEET).await?;

        // Convert from parameter -> DB type.
        let services: Vec<_> = request.services
            .into_iter()
            .map(|svc| {
                db::model::Service::new(
                    svc.service_id,
                    svc.sled_id,
                    svc.address,
                    svc.kind.into(),
                )
            })
            .collect();

        let datasets: Vec<_> = request.datasets
            .into_iter()
            .map(|dataset| {
                db::model::Dataset::new(
                    dataset.dataset_id,
                    dataset.zpool_id,
                    dataset.request.address,
                    dataset.request.kind.into(),
                )
            })
            .collect();
        self.db_datastore
            .rack_set_initialized(opctx, rack_id, services, datasets)
            .await?;

        Ok(())
    }
}
