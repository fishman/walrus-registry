use actix_web::{middleware::from_fn, web, App, HttpResponse, HttpServer, Responder};
use core::str;
use diesel::{
    prelude::*,
    r2d2::{ConnectionManager, Pool, PooledConnection},
    sqlite::SqliteConnection,
    RunQueryDsl,
};
use dotenvy::dotenv;
use env_logger::Env;
use serde::Deserialize;
use std::{env, time::Duration};
use uuid::Uuid;
use walrus_registry::calculate_sha256_digest;

mod models;
mod schema;
use models::{Blob, Manifest, NewManifest};
use schema::{blobs::dsl::blobs, manifests::dsl::manifests};

mod logger;
use crate::logger::highlight_status;

mod sqlite;
use crate::sqlite::ConnectionOptions;

#[derive(Deserialize)]
struct UploadParams {
    digest: String,
}

#[derive(Clone)]
struct RegistryData {
    pool: Pool<ConnectionManager<SqliteConnection>>,
}

async fn readiness_check() -> impl Responder {
    HttpResponse::Ok().body("Service is ready")
}

fn establish_connection_pool() -> Pool<ConnectionManager<SqliteConnection>> {
    dotenv().ok();
    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL must be set, please check .env.example");
    let manager = ConnectionManager::<SqliteConnection>::new(database_url);

    let connection_options = ConnectionOptions {
        enable_wal: true,
        enable_foreign_keys: true,
        busy_timeout: Some(Duration::from_secs(5)),
    };

    let pool = Pool::builder()
        .max_size(10)
        .connection_customizer(Box::new(connection_options))
        .build(manager)
        .expect("Failed to create pool.");

    pool
}

async fn check_registry() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({ "message": "OCI Registry v2" }))
}

async fn start_blob_upload(
    name: web::Path<String>,
    data: web::Data<RegistryData>,
) -> impl Responder {
    let upload_uuid = Uuid::new_v4().to_string();

    let new_blob = Blob {
        uuid: upload_uuid.clone(),
        digest: None,
        content_length: None,
        name: name.clone(),
        data: vec![],
    };

    match data.pool.get() {
        Ok(mut conn) => {
            diesel::insert_into(blobs)
                .values(&new_blob)
                .execute(&mut conn)
                .expect("Error inserting new blob");

            HttpResponse::Accepted()
                .append_header((
                    "Location",
                    format!("/v2/{}/blobs/uploads/{}", name, upload_uuid),
                ))
                .append_header(("Docker-Upload-UUID", upload_uuid.clone()))
                .json(serde_json::json!({ "uuid": upload_uuid }))
        }
        Err(e) => {
            eprintln!("Error getting connection from pool: {:?}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

async fn complete_blob_upload(
    data: web::Data<RegistryData>,
    path: web::Path<(String, String)>,
    _query: web::Query<UploadParams>,
    body: web::Bytes,
) -> impl Responder {
    let (name, uuid) = path.into_inner();

    let target = blobs
        .filter(schema::blobs::uuid.eq(uuid.to_string()))
        .filter(schema::blobs::name.eq(name.to_string()));

    let conn_result: Result<
        PooledConnection<ConnectionManager<SqliteConnection>>,
        diesel::r2d2::PoolError,
    > = data.pool.get();

    match conn_result {
        Ok(mut conn) => {
            // Find the blob and update it with the uploaded data
            let digest = format!("sha256:{}", calculate_sha256_digest(&body));
            // body.len() will never exceed i32 unless we remove all limitation on
            // upload lizes
            let content_length: i32 = body.len().try_into().unwrap();

            let updated = diesel::update(target)
                .set((
                    schema::blobs::data.eq(body.to_vec()),
                    schema::blobs::digest.eq(digest.clone()),
                    schema::blobs::content_length.eq(content_length),
                ))
                .execute(&mut conn)
                .expect("Failed to update blob data");

            if updated == 1 {
                HttpResponse::Created()
                    .append_header(("Docker-Content-Digest", digest.clone()))
                    .json(serde_json::json!({
                            "uuid": uuid.to_string(),
                            "digest": digest,
                            "status": "completed"
                    }))
            } else {
                log::error!("Blob with UUID {} not found for update", uuid.to_string());
                HttpResponse::NotFound().json(serde_json::json!({ "error": "Blob not found" }))
            }
        }
        Err(e) => {
            eprintln!("Error getting connection from pool: {:?}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

async fn check_blob(
    data: web::Data<RegistryData>,
    path: web::Path<(String, String)>,
) -> impl Responder {
    let (name, digest) = path.into_inner();
    let mut conn = data.pool.get().expect("Failed to get DB connection");

    let blob = blobs
        .filter(schema::blobs::name.eq(name.trim()))
        .filter(schema::blobs::digest.eq(digest.trim()))
        .first::<Blob>(&mut conn);

    match blob {
        Ok(blob) => {
            log::info!("{}", blob.name);
            HttpResponse::Ok().body(blob.data)
        }
        Err(_) => {
            log::info!("couldn't find {} {}", name, digest);
            HttpResponse::NotFound().json(serde_json::json!({ "error": "Blob not found" }))
        }
    }
}

//async fn check_blob(
//    data: web::Data<RegistryData>,
//    path: web::Path<(String, String)>,
//) -> impl Responder {
//    let (name, checksum) = path.into_inner();
//    let mut conn = data.pool.get().expect("Failed to get DB connection");
//
//    let blob = blobs
//        .filter(schema::manifests::name.eq(name.as_str()))
//        .filter(schema::blobs::sha256digest.eq(checksum.as_str()))
//        .first::<Blob>(&mut conn);
//
//    match blob {
//        Ok(blob) => {
//            let digest = format!("sha256:{}", calculate_sha256_digest(&blob.content));
//            let content_length = manifest.content.len();
//
//            HttpResponse::Ok()
//                .append_header(("Docker-Content-Digest", digest))
//                .append_header(("Content-Length", content_length.to_string()))
//                .json(serde_json::json!({
//                    "status": "completed"
//                }))
//        }
//        Err(_) => {
//            HttpResponse::NotFound().json(serde_json::json!({ "error": "Manifest not found" }))
//        }
//    }
//}

async fn fetch_blob(
    data: web::Data<RegistryData>,
    path: web::Path<(String, String)>,
) -> impl Responder {
    let (name, digest) = path.into_inner();
    let mut conn = data.pool.get().expect("Failed to get DB connection");

    let blob = blobs
        .filter(schema::blobs::name.eq(name.trim()))
        .filter(schema::blobs::digest.eq(digest.trim()))
        .first::<Blob>(&mut conn);

    match blob {
        Ok(blob) => {
            log::info!("stuff");
            let digest = blob.digest.unwrap();
            let content_length = blob.content_length.unwrap();

            HttpResponse::Ok()
                .append_header(("Docker-Content-Digest", digest))
                .append_header(("Content-Length", content_length))
                .append_header(("Content-Type", content_length))
                .append_header(("Content-Type", "application/octet-stream"))
                .body(blob.data)
        }
        Err(_) => HttpResponse::NotFound().json(serde_json::json!({ "error": "Blob not found" })),
    }
}

async fn upload_manifest(
    path: web::Path<(String, String)>,
    data: web::Data<RegistryData>,
    body: web::Bytes,
) -> impl Responder {
    let (name, reference) = path.into_inner();
    let manifest = NewManifest {
        name: &name,
        reference: &reference,
        content: &body.to_vec(),
    };

    let conn_result: Result<
        PooledConnection<ConnectionManager<SqliteConnection>>,
        diesel::r2d2::PoolError,
    > = data.pool.get();

    match conn_result {
        Ok(mut conn) => {
            let result = diesel::insert_into(manifests)
                .values(&manifest)
                .on_conflict((schema::manifests::name, schema::manifests::reference))
                .do_update()
                .set((
                    schema::manifests::content.eq(&body.to_vec()),
                    schema::manifests::updated_at.eq(diesel::dsl::now),
                ))
                .execute(&mut conn);

            let digest = format!("sha256:{}", calculate_sha256_digest(&manifest.content));
            let content_length = manifest.content.len();

            match result {
                Ok(_) => {
                    HttpResponse::Ok()
                        .append_header(("Docker-Content-Digest", digest))
                        .append_header(("Content-Length", content_length.to_string()))
                        .json(serde_json::json!({ "name": name.to_string(), "reference": reference.to_string() }))
                },
                Err(e) => {
                    eprintln!("Error inserting or updating manifest: {:?}", e);
                    HttpResponse::InternalServerError().finish()
                }
            }
        }
        Err(e) => {
            eprintln!("Error getting connection from pool: {:?}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

async fn check_manifest(
    data: web::Data<RegistryData>,
    path: web::Path<(String, String)>,
) -> impl Responder {
    let (name, reference) = path.into_inner();
    let mut conn = data.pool.get().expect("Failed to get DB connection");

    let manifest = manifests
        .filter(schema::manifests::name.eq(name.as_str()))
        .filter(schema::manifests::reference.eq(reference.as_str()))
        .first::<Manifest>(&mut conn);

    match manifest {
        Ok(manifest) => {
            let digest = format!("sha256:{}", calculate_sha256_digest(&manifest.content));
            let content_length = manifest.content.len();

            HttpResponse::Ok()
                .append_header(("Docker-Content-Digest", digest))
                .append_header(("Content-Length", content_length.to_string()))
                .append_header(("Content-Type", "application/vnd.oci.image.index.v1+json"))
                .body(manifest.content)
        }
        Err(_) => {
            HttpResponse::NotFound().json(serde_json::json!({ "error": "Manifest not found" }))
        }
    }
}

async fn fetch_manifest(
    data: web::Data<RegistryData>,
    name: web::Path<String>,
    reference: web::Path<String>,
) -> impl Responder {
    let mut conn = data.pool.get().expect("Failed to get DB connection");

    let manifest = manifests
        .filter(schema::manifests::name.eq(name.as_str()))
        .filter(schema::manifests::reference.eq(reference.as_str()))
        .first::<Manifest>(&mut conn);

    match manifest {
        Ok(manifest) => HttpResponse::Ok().body(manifest.content),
        Err(_) => {
            HttpResponse::NotFound().json(serde_json::json!({ "error": "Manifest not found" }))
        }
    }
}

async fn get_tags(data: web::Data<RegistryData>, name: web::Path<String>) -> impl Responder {
    let mut conn = data.pool.get().expect("Failed to get DB connection");

    let tag_list: Vec<String> = manifests
        .filter(schema::manifests::name.eq(name.as_str()))
        .select(schema::manifests::reference)
        .load::<String>(&mut conn)
        .expect("Error loading tags");

    HttpResponse::Ok().json(serde_json::json!({
        "name": name.to_string(),
        "tags": tag_list
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();
    let bind_address =
        env::var("BIND_ADDRESS").expect("BIND_ADDRESS must be set, please check .env.example");
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    let pool = establish_connection_pool();

    let registry_data = web::Data::new(RegistryData { pool });

    log::info!("Starting server");
    HttpServer::new(move || {
        App::new()
            //.wrap(Logger::new("%s %r %Dms"))
            .wrap(from_fn(highlight_status))
            .app_data(registry_data.clone())
            .app_data(web::PayloadConfig::new(1000000 * 250))
            .route("/v2/", web::get().to(check_registry))
            .route(
                "/v2/{name}/blobs/uploads/",
                web::post().to(start_blob_upload),
            )
            .route(
                "/v2/{name}/blobs/uploads/{uuid}",
                web::put().to(complete_blob_upload),
            )
            .route("/v2/{name}/blobs/{digest}", web::head().to(check_blob))
            .route("/v2/{name}/blobs/{digest}", web::get().to(fetch_blob))
            .route(
                "/v2/{name}/manifests/{reference}",
                web::head().to(check_manifest),
            )
            .route(
                "/v2/{name}/manifests/{reference}",
                web::get().to(fetch_manifest),
            )
            .route(
                "/v2/{name}/manifests/{reference}",
                web::put().to(upload_manifest),
            )
            .route("/v2/{name}/tags/list", web::get().to(get_tags))
            .route("/healthz", web::get().to(readiness_check))
    })
    .bind(bind_address)?
    .run()
    .await
}
