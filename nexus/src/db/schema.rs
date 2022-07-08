// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Describes the Diesel database schema.
//!
//! NOTE: Should be kept up-to-date with dbinit.sql.

table! {
    disk (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,
        project_id -> Uuid,
        volume_id -> Uuid,
        disk_state -> Text,
        attach_instance_id -> Nullable<Uuid>,
        state_generation -> Int8,
        time_state_updated -> Timestamptz,
        size_bytes -> Int8,
        block_size -> crate::db::model::BlockSizeEnum,
        origin_snapshot -> Nullable<Uuid>,
        origin_image -> Nullable<Uuid>,
    }
}

table! {
    image (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        project_id -> Uuid,
        volume_id -> Uuid,
        url -> Nullable<Text>,
        version -> Nullable<Text>,
        digest -> Nullable<Text>,
        block_size -> crate::db::model::BlockSizeEnum,
        size_bytes -> Int8,
    }
}

table! {
    global_image (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        volume_id -> Uuid,
        url -> Nullable<Text>,
        distribution -> Text,
        version -> Text,
        digest -> Nullable<Text>,
        block_size -> crate::db::model::BlockSizeEnum,
        size_bytes -> Int8,
    }
}

table! {
    snapshot (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        project_id -> Uuid,
        disk_id -> Uuid,
        volume_id -> Uuid,
        size_bytes -> Int8,
    }
}

table! {
    instance (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        project_id -> Uuid,
        user_data -> Binary,
        state -> crate::db::model::InstanceStateEnum,
        time_state_updated -> Timestamptz,
        state_generation -> Int8,
        active_server_id -> Uuid,
        active_propolis_id -> Uuid,
        active_propolis_ip -> Nullable<Inet>,
        target_propolis_id -> Nullable<Uuid>,
        migration_id -> Nullable<Uuid>,
        ncpus -> Int8,
        memory -> Int8,
        hostname -> Text,
    }
}

table! {
    metric_producer (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        ip -> Inet,
        port -> Int4,
        interval -> Float8,
        base_route -> Text,
        oximeter_id -> Uuid,
    }
}

table! {
    network_interface (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        instance_id -> Uuid,
        vpc_id -> Uuid,
        subnet_id -> Uuid,
        mac -> Int8,
        ip -> Inet,
        slot -> Int2,
        is_primary -> Bool,
    }
}

table! {
    ip_pool (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,
    }
}

table! {
    ip_pool_range (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        first_address -> Inet,
        last_address -> Inet,
        ip_pool_id -> Uuid,
        rcgen -> Int8,
    }
}

table! {
    instance_external_ip (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        ip_pool_id -> Uuid,
        ip_pool_range_id -> Uuid,
        instance_id -> Uuid,
        ip -> Inet,
        first_port -> Int4,
        last_port -> Int4,
    }
}

table! {
    silo (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        discoverable -> Bool,
        user_provision_type -> crate::db::model::UserProvisionTypeEnum,
        admin_group_name -> Nullable<Text>,

        rcgen -> Int8,
    }
}

table! {
    silo_user (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        silo_id -> Uuid,
        external_id -> Text,
    }
}

table! {
    silo_group (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        silo_id -> Uuid,
    }
}

table! {
    silo_group_membership (silo_group_id, silo_user_id) {
        silo_group_id -> Uuid,
        silo_user_id -> Uuid,
    }
}

allow_tables_to_appear_in_same_query!(silo_group, silo_group_membership);

table! {
    identity_provider (silo_id, id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        silo_id -> Uuid,
        provider_type -> crate::db::model::IdentityProviderTypeEnum,
    }
}

table! {
    saml_identity_provider (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,

        silo_id -> Uuid,

        idp_metadata_document_string -> Text,

        idp_entity_id -> Text,
        sp_client_id -> Text,
        acs_url -> Text,
        slo_url -> Text,
        technical_contact_email -> Text,
        public_cert -> Nullable<Text>,
        private_key -> Nullable<Text>,
        group_attribute_name -> Nullable<Text>,
    }
}

table! {
    ssh_key (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        silo_user_id -> Uuid,
        public_key -> Text,
    }
}

table! {
    organization (id) {
        id -> Uuid,
        silo_id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,
    }
}

table! {
    oximeter (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        ip -> Inet,
        port -> Int4,
    }
}

table! {
    project (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        organization_id -> Uuid,
    }
}

table! {
    saga (id) {
        id -> Uuid,
        creator -> Uuid,
        template_name -> Text,
        time_created -> Timestamptz,
        saga_params -> Jsonb,
        saga_state -> crate::db::saga_types::SagaCachedStateEnum,
        current_sec -> Nullable<Uuid>,
        adopt_generation -> Int8,
        adopt_time -> Timestamptz,
    }
}

table! {
    saga_node_event (saga_id, node_id, event_type) {
        saga_id -> Uuid,
        node_id -> Int8,
        event_type -> Text,
        data -> Nullable<Jsonb>,
        event_time -> Timestamptz,
        creator -> Uuid,
    }
}

table! {
    rack (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        initialized -> Bool,
        tuf_base_url -> Nullable<Text>,
    }
}

table! {
    console_session (token) {
        token -> Text,
        time_created -> Timestamptz,
        time_last_used -> Timestamptz,
        silo_user_id -> Uuid,
    }
}

table! {
    sled (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,

        rack_id -> Uuid,
        ip -> Inet,
        port -> Int4,
        last_used_address -> Inet,
    }
}

table! {
    service (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,

        sled_id -> Uuid,
        ip -> Inet,
        kind -> crate::db::model::ServiceKindEnum,
    }
}

table! {
    zpool (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,

        sled_id -> Uuid,

        total_size -> Int8,
    }
}

table! {
    dataset (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,

        pool_id -> Uuid,

        ip -> Inet,
        port -> Int4,

        kind -> crate::db::model::DatasetKindEnum,

        size_used -> Nullable<Int8>,
    }
}

table! {
    region (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,

        dataset_id -> Uuid,
        volume_id -> Uuid,

        block_size -> Int8,
        blocks_per_extent -> Int8,
        extent_count -> Int8,
    }
}

table! {
    volume (id) {
        id -> Uuid,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        rcgen -> Int8,

        data -> Text,
        /* TODO: some sort of refcount? */
    }
}

table! {
    vpc (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        project_id -> Uuid,
        system_router_id -> Uuid,
        vni -> Int4,
        ipv6_prefix -> Inet,
        dns_name -> Text,
        firewall_gen -> Int8,
    }
}

table! {
    vpc_subnet (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        vpc_id -> Uuid,
        ipv4_block -> Inet,
        ipv6_block -> Inet,
    }
}

table! {
    vpc_router (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        kind -> crate::db::model::VpcRouterKindEnum,
        vpc_id -> Uuid,
        rcgen -> Int8,
    }
}

table! {
    router_route (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        kind -> crate::db::model::RouterRouteKindEnum,
        vpc_router_id -> Uuid,
        target -> Text,
        destination -> Text,
    }
}

table! {
    use crate::db::model;
    use diesel::sql_types::*;

    vpc_firewall_rule (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
        vpc_id -> Uuid,
        status -> model::VpcFirewallRuleStatusEnum,
        direction -> model::VpcFirewallRuleDirectionEnum,
        targets -> Array<Text>,
        filter_hosts -> Nullable<Array<Text>>,
        filter_ports -> Nullable<Array<Text>>,
        filter_protocols -> Nullable<Array<model::VpcFirewallRuleProtocolEnum>>,
        action -> model::VpcFirewallRuleActionEnum,
        priority -> Int4,
    }
}

table! {
    user_builtin (id) {
        id -> Uuid,
        name -> Text,
        description -> Text,
        time_created -> Timestamptz,
        time_modified -> Timestamptz,
        time_deleted -> Nullable<Timestamptz>,
    }
}

table! {
    device_auth_request (client_id, device_code) {
        client_id -> Uuid,
        device_code -> Text,
        user_code -> Text,
        time_created -> Timestamptz,
        time_expires -> Timestamptz,
    }
}

table! {
    device_access_token (token) {
        token -> Text,
        client_id -> Uuid,
        device_code -> Text,
        silo_user_id -> Uuid,
        time_created -> Timestamptz,
    }
}

table! {
    role_builtin (resource_type, role_name) {
        resource_type -> Text,
        role_name -> Text,
        description -> Text,
    }
}

table! {
    role_assignment (
        identity_type,
        identity_id,
        resource_type,
        resource_id,
        role_name
    ) {
        identity_type -> crate::db::model::IdentityTypeEnum,
        identity_id -> Uuid,
        resource_type -> Text,
        role_name -> Text,
        resource_id -> Uuid,
    }
}

table! {
    update_available_artifact (name, version, kind) {
        name -> Text,
        version -> Int8,
        kind -> crate::db::model::UpdateArtifactKindEnum,
        targets_role_version -> Int8,
        valid_until -> Timestamptz,
        target_name -> Text,
        target_sha256 -> Text,
        target_length -> Int8,
    }
}

allow_tables_to_appear_in_same_query!(ip_pool_range, ip_pool);
joinable!(ip_pool_range -> ip_pool (ip_pool_id));

allow_tables_to_appear_in_same_query!(
    dataset,
    disk,
    instance,
    metric_producer,
    network_interface,
    organization,
    oximeter,
    project,
    rack,
    region,
    saga,
    saga_node_event,
    silo,
    identity_provider,
    console_session,
    service,
    sled,
    router_route,
    vpc,
    vpc_subnet,
    vpc_router,
    vpc_firewall_rule,
    user_builtin,
    role_builtin,
    role_assignment,
    zpool,
);
