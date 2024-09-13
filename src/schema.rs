// @generated automatically by Diesel CLI.

diesel::table! {
    blobs (uuid) {
        uuid -> Text,
        data -> Binary,
    }
}

diesel::table! {
    manifests (id) {
        id -> Integer,
        name -> Text,
        reference -> Text,
        content -> Binary,
        created_at -> Timestamp,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    blobs,
    manifests,
);
