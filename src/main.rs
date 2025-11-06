#[macro_use]
extern crate rocket;

#[macro_use]
extern crate lazy_static;

mod background_optimization;
mod db;
mod encoding;
mod util;

use background_optimization::{optimize_image_and_update, optimize_images_from_database};
use base64::{engine::general_purpose, Engine as _};
use dotenv::dotenv;
use log::{error, info};
use rocket::data::ToByteUnit;
use rocket::form::Form;
use rocket::http::{ContentType, Header, Status};
use rocket::response::{status, status::Custom, Redirect};
use rocket::serde::{json::Json, Deserialize, Serialize};
use rocket::{Data, State};
use rocket_multipart_form_data::{
    mime, MultipartFormData, MultipartFormDataField, MultipartFormDataOptions,
};
use std::io::Cursor;
use image::GenericImageView;
use tokio::{join, task};
use util::ImageId;

lazy_static! {
    static ref HOST: String = std::env::var("HOST").unwrap_or("i.dishis.tech".to_string());
}

#[derive(FromForm)]
struct UrlencodedUpload {
    image: String,
}

#[derive(Deserialize)]
struct ApiUploadRequest {
    base64: Option<String>,
    url: Option<String>,
}

#[derive(Serialize)]
struct ApiImageVariant {
    filename: String,
    name: String,
    mime: String,
    extension: String,
    url: String,
}

#[derive(Serialize)]
struct ApiImageData {
    id: String,
    title: String,
    url_viewer: String,
    url: String,
    display_url: String,
    width: String,
    height: String,
    size: String,
    time: String,
    expiration: String,
    image: ApiImageVariant,
    thumb: ApiImageVariant,
    medium: ApiImageVariant,
    delete_url: String,
}

#[derive(Serialize)]
struct ApiResponse {
    data: ApiImageData,
    success: bool,
    status: u16,
}

#[derive(Serialize)]
struct ApiErrorResponse {
    error: String,
    success: bool,
    status: u16,
}

async fn download_image_from_url(url: &str) -> Result<(Vec<u8>, String), String> {
    info!("Downloading image from URL: {}", url);
    let response = reqwest::get(url)
        .await
        .map_err(|e| format!("Network error: {}", e))?;
    if !response.status().is_success() {
        return Err(format!(
            "Failed to download image: Server returned status {}",
            response.status()
        ));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let image_bytes = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
    info!(
        "Successfully downloaded {} bytes with content-type: {}",
        image_bytes.len(),
        content_type
    );
    Ok((image_bytes, content_type))
}

fn create_error(status: Status, message: &str) -> Custom<Json<ApiErrorResponse>> {
    Custom(
        status,
        Json(ApiErrorResponse {
            error: message.to_string(),
            success: false,
            status: status.code,
        }),
    )
}

fn mime_to_extension(mime_type: &str) -> &str {
    mime_type.split('/').last().unwrap_or("jpg")
}

async fn process_text_upload(
    mut text_value: String,
    images_collection: &mongodb::Collection<mongodb::bson::Document>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    text_value = text_value.trim().to_string();

    if text_value.starts_with("http://") || text_value.starts_with("https://") {
        let (image_bytes, ct) = download_image_from_url(&text_value)
            .await
            .map_err(|e| create_error(Status::BadRequest, &e))?;
        return process_and_respond(image_bytes, &ct, images_collection).await;
    }

    if let Some(idx) = text_value.find(',') {
        if text_value.starts_with("data:") {
            text_value = text_value[idx + 1..].to_string();
        }
    }
    let image_bytes = general_purpose::STANDARD
        .decode(&text_value)
        .map_err(|_| create_error(Status::BadRequest, "Invalid Base64 string"))?;
    let kind = infer::get(&image_bytes).ok_or_else(|| {
        create_error(
            Status::BadRequest,
            "Could not determine image type from Base64 data",
        )
    })?;

    process_and_respond(image_bytes, kind.mime_type(), images_collection).await
}

async fn process_and_respond(
    image_bytes: Vec<u8>,
    content_type_string: &str,
    images_collection: &mongodb::Collection<mongodb::bson::Document>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    if image_bytes.is_empty() {
        return Err(create_error(
            Status::BadRequest,
            "Image data cannot be empty.",
        ));
    }

    info!(
        "Processing {} bytes of image data with provided content-type: {}",
        image_bytes.len(),
        content_type_string
    );

    let decoded_image = image::load_from_memory(&image_bytes).map_err(|e| {
        create_error(
            Status::BadRequest,
            &format!("Failed to decode image: {}", e),
        )
    })?;
    let (width, height) = decoded_image.dimensions();

    let (encoded_thumbnail_result, image_id_result) = join!(
        encoding::from_image(
            decoded_image,
            encoding::FromImageOptions {
                max_size: Some(128),
                ..encoding::FromImageOptions::default()
            }
        ),
        db::generate_image_id(images_collection)
    );

    let encoded_thumbnail =
        encoded_thumbnail_result.map_err(|e| create_error(Status::InternalServerError, &e))?;
    let image_id =
        image_id_result.map_err(|e| create_error(Status::InternalServerError, &e.to_string()))?;
    
    let delete_token = util::generate_delete_token(16);

    let insert_result = db::insert_image(
        images_collection,
        &db::NewImage {
            id: &image_id,
            data: &image_bytes,
            content_type: content_type_string,
            thumbnail_data: &encoded_thumbnail.data,
            thumbnail_content_type: &encoded_thumbnail.content_type,
            size: (width, height),
            optim_level: 0,
            delete_token: &delete_token,
        },
    )
    .await;
    let inserted_doc = insert_result
        .map_err(|_| create_error(Status::InternalServerError, "DB insert failed"))?
        .ok_or_else(|| create_error(Status::InternalServerError, "DB did not return doc"))?;

    info!("Successfully uploaded image {}", &image_id);

    let doc_for_bg = inserted_doc.clone();
    let owned_images_collection = images_collection.clone();
    task::spawn(async move {
        optimize_image_and_update(&owned_images_collection, &doc_for_bg)
            .await
            .ok();
    });

    let id_str = image_id.to_string();
    let base_url = format!("https://{}", *HOST);
    let creation_time = inserted_doc
        .get_datetime("date")
        .unwrap()
        .timestamp_millis()
        / 1000;
    let image_ext = mime_to_extension(content_type_string);
    let thumb_ext = mime_to_extension(&encoded_thumbnail.content_type);
    let image_url = format!("{}/i/{}", base_url, id_str);
    let thumb_url = format!("{}/i/{}/thumb", base_url, id_str);
    let delete_url = format!("{}/delete/{}/{}", base_url, id_str, delete_token);

    Ok(Json(ApiResponse {
        data: ApiImageData {
            id: id_str.clone(),
            title: id_str.clone(),
            url_viewer: image_url.clone(),
            url: image_url.clone(),
            display_url: image_url.clone(),
            width: width.to_string(),
            height: height.to_string(),
            size: image_bytes.len().to_string(),
            time: creation_time.to_string(),
            expiration: "0".to_string(),
            delete_url,
            image: ApiImageVariant {
                filename: format!("{}.{}", id_str, image_ext),
                name: id_str.clone(),
                mime: content_type_string.to_string(),
                extension: image_ext.to_string(),
                url: image_url.clone(),
            },
            medium: ApiImageVariant {
                filename: format!("{}.{}", id_str, image_ext),
                name: id_str.clone(),
                mime: content_type_string.to_string(),
                extension: image_ext.to_string(),
                url: image_url.clone(),
            },
            thumb: ApiImageVariant {
                filename: format!("{}.{}", id_str, thumb_ext),
                name: id_str.clone(),
                mime: encoded_thumbnail.content_type.clone(),
                extension: thumb_ext.to_string(),
                url: thumb_url,
            },
        },
        success: true,
        status: 200,
    }))
}

#[derive(Responder)]
#[response(status = 200)]
struct HtmlResponder(&'static str, Header<'static>);

#[get("/")]
fn index() -> HtmlResponder {
    HtmlResponder(
        include_str!("../site/index.html"),
        Header::new("Content-Type", "text/html; charset=utf-8"),
    )
}

#[post("/api/upload", data = "<data>", format = "json", rank = 1, limits = "32 megabytes")]
async fn api_upload_json(
    data: Json<ApiUploadRequest>,
    collections: &State<db::Collections>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    let req = data.into_inner();
    if let Some(b64) = req.base64 {
        return process_text_upload(b64, &collections.images).await;
    }
    if let Some(url) = req.url {
        let (image_bytes, ct) = download_image_from_url(&url)
            .await
            .map_err(|e| create_error(Status::BadRequest, &e))?;
        return process_and_respond(image_bytes, &ct, &collections.images).await;
    }
    Err(create_error(
        Status::BadRequest,
        "Missing 'base64' or 'url' field in JSON.",
    ))
}

#[post("/api/upload", data = "<form>", format = "form", rank = 2, limits = "32 megabytes")]
async fn api_upload_form(
    form: Form<UrlencodedUpload>,
    collections: &State<db::Collections>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    process_text_upload(form.into_inner().image, &collections.images).await
}

#[post("/api/upload", data = "<data>", rank = 3, limits = "32 megabytes")]
async fn api_upload_fallback(
    content_type: &ContentType,
    data: Data<'_>,
    collections: &State<db::Collections>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    // --- CASE 1: Proper multipart/form-data ---
    if content_type.is_form_data() {
        let options = MultipartFormDataOptions::with_multipart_form_data_fields(vec![
            MultipartFormDataField::file("image")
                .content_type_by_string(Some(mime::STAR_STAR))
                .unwrap(),
            MultipartFormDataField::text("image"),
        ]);

        let form_data = MultipartFormData::parse(content_type, data, options)
            .await
            .map_err(|e| create_error(Status::BadRequest, &format!("Form parse error: {}", e)))?;

        if let Some(files) = form_data.files.get("image") {
            if let Some(file) = files.get(0) {
                let image_bytes = tokio::fs::read(&file.path).await.map_err(|_| {
                    create_error(Status::InternalServerError, "Could not read uploaded file")
                })?;
                let ct = file
                    .content_type
                    .as_ref()
                    .map(|ct| ct.to_string())
                    .unwrap_or_else(|| {
                        infer::get(&image_bytes)
                            .map(|k| k.mime_type().to_string())
                            .unwrap_or_else(|| "application/octet-stream".to_string())
                    });
                return process_and_respond(image_bytes, &ct, &collections.images).await;
            }
        }
        if let Some(texts) = form_data.texts.get("image") {
            if let Some(text_field) = texts.get(0) {
                return process_text_upload(text_field.text.clone(), &collections.images).await;
            }
        }
        return Err(create_error(
            Status::BadRequest,
            "Missing 'image' field in multipart form.",
        ));
    }

    // --- CASE 2: Custom raw boundary parsing ---
    let raw_body = data
        .open(32.megabytes())
        .into_bytes()
        .await
        .map_err(|_| create_error(Status::BadRequest, "Failed to read request body"))?
        .into_inner();

    let body_str = String::from_utf8_lossy(&raw_body);

    if let Some(start) = body_str.find("------") {
        let boundary_line = body_str.lines().next().unwrap_or("").trim().to_string();

        let boundary = boundary_line.trim();
        let parts: Vec<&str> = body_str.split(boundary).collect();

        for part in parts {
            if part.contains("Content-Disposition")
                && part.contains("filename=")
                && part.contains("Content-Type")
            {
                if let Some(idx) = part.find("\r\n\r\n") {
                    let file_data = &part[idx + 4..];
                    let file_bytes = file_data.as_bytes().to_vec();

                    let ct = if let Some(ct_idx) = part.find("Content-Type:") {
                        let line = part[ct_idx..].lines().next().unwrap_or("");
                        line.replace("Content-Type:", "").trim().to_string()
                    } else {
                        infer::get(&file_bytes)
                            .map(|k| k.mime_type().to_string())
                            .unwrap_or_else(|| "application/octet-stream".to_string())
                    };

                    return process_and_respond(file_bytes, &ct, &collections.images).await;
                }
            }
        }
    }

    // --- CASE 3: Raw binary ---
    if raw_body.is_empty() {
        return Err(create_error(Status::BadRequest, "No image data received."));
    }

    let ct = infer::get(&raw_body)
        .map(|kind| kind.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    process_and_respond(raw_body, &ct, &collections.images).await
}

#[derive(Responder)]
#[response(status = 200)]
struct ImageResponder(Vec<u8>, Header<'static>);

#[get("/i/<id>")]
async fn view_image_route(
    id: String,
    collections: &State<db::Collections>,
) -> Option<ImageResponder> {
    let doc = db::get_image(&collections.images, &id).await.ok()??;
    let data = doc.get_binary_generic("data").unwrap().clone();
    let ct = doc.get_str("content_type").unwrap().to_string();

    let images_collection = collections.images.clone();
    task::spawn(async move {
        db::update_last_seen(&images_collection, &ImageId(id))
            .await
            .ok();
    });

    Some(ImageResponder(data, Header::new("Content-Type", ct)))
}

#[get("/i/<id>/thumb")]
async fn view_thumbnail_route(
    id: String,
    collections: &State<db::Collections>,
) -> Option<ImageResponder> {
    let doc = db::get_image(&collections.images, &id).await.ok()??;
    let data = doc.get_binary_generic("thumbnail_data").unwrap().clone();
    let ct = doc.get_str("thumbnail_content_type").unwrap().to_string();
    Some(ImageResponder(data, Header::new("Content-Type", ct)))
}

#[get("/image/<id>")]
fn redirect_image_route(id: String) -> Redirect {
    Redirect::to(uri!(view_image_route(id)))
}

#[delete("/delete/<id>/<token>")]
async fn delete_image_route(
    id: String,
    token: String,
    collections: &State<db::Collections>,
) -> Result<status::NoContent, status::NotFound<String>> {
    match db::delete_image(&collections.images, &id, &token).await {
        Ok(result) => {
            if result.deleted_count == 1 {
                info!("Successfully deleted image {}", id);
                Ok(status::NoContent)
            } else {
                Err(status::NotFound("Image not found or token incorrect".to_string()))
            }
        }
        Err(e) => {
            error!("Error deleting image {}: {}", id, e);
            Err(status::NotFound("Error during deletion".to_string()))
        }
    }
}

#[launch]
async fn rocket() -> _ {
    dotenv().ok();
    env_logger::init();
    let images_collection = db::connect().await.unwrap();
    println!("Connected to database");

    let collections = db::Collections {
        images: images_collection.clone(),
    };
    tokio::spawn(async move {
        optimize_images_from_database(&images_collection)
            .await
            .expect("Failed optimizing images");
    });

    rocket::build().manage(collections).mount(
        "/",
        routes![
            index,
            api_upload_json,
            api_upload_form,
            api_upload_fallback,
            view_image_route,
            redirect_image_route,
            view_thumbnail_route,
            delete_image_route
        ],
    )
}