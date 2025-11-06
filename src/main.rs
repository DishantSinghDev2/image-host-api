#[macro_use]
extern crate rocket;

mod background_optimization;
mod db;
mod encoding;
mod util;

use background_optimization::{optimize_image_and_update, optimize_images_from_database};
use base64::{engine::general_purpose, Engine as _};
use dotenv::dotenv;
use image::GenericImageView;
use log::{error, info};
use rocket::data::{Limits, ToByteUnit};
use rocket::form::Form;
use rocket::http::{ContentType, Header, Status};
use rocket::request::{self, FromRequest, Outcome, Request};
use rocket::response::{content::RawHtml, status, status::Custom, Redirect};
use rocket::serde::{json::Json, Deserialize, Serialize};
use rocket::{build, catch, catchers, get, launch, post, routes, Data, State};
use rocket_multipart_form_data::{
    mime, MultipartFormData, MultipartFormDataField, MultipartFormDataOptions,
};
use std::env;
use tokio::{join, task};
use util::ImageId;


// --- Config Struct ---
// A struct to hold our application's configuration.
pub struct Config {
    host: String,
    api_secret_key: String,
}

// --- API Key Guard ---
// It now gets the API key from the managed state instead of a global static.
struct ApiKeyGuard;

#[derive(Debug)]
enum ApiKeyError {
    Missing,
    Invalid,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ApiKeyGuard {
    type Error = ApiKeyError;

    async fn from_request(req: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let config = match req.guard::<&State<Config>>().await {
            Outcome::Success(config) => config,
            _ => return Outcome::Error((Status::InternalServerError, ApiKeyError::Missing)),
        };

        match req.headers().get_one("X-API-KEY") {
            None => Outcome::Error((Status::Unauthorized, ApiKeyError::Missing)),
            Some(key) => {
                if key == &config.api_secret_key {
                    Outcome::Success(ApiKeyGuard)
                } else {
                    Outcome::Error((Status::Forbidden, ApiKeyError::Invalid))
                }
            }
        }
    }
}


// --- API Structures ---
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


// --- Helper Functions ---
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


// --- Image Processing Logic ---
async fn process_text_upload(
    mut text_value: String,
    images_collection: &mongodb::Collection<mongodb::bson::Document>,
    config: &State<Config>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    text_value = text_value.trim().to_string();

    if text_value.starts_with("http://") || text_value.starts_with("https://") {
        let (image_bytes, ct) = download_image_from_url(&text_value)
            .await
            .map_err(|e| create_error(Status::BadRequest, &e))?;
        return process_and_respond(image_bytes, &ct, images_collection, config).await;
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

    process_and_respond(image_bytes, kind.mime_type(), images_collection, config).await
}

async fn process_and_respond(
    image_bytes: Vec<u8>,
    content_type_string: &str,
    images_collection: &mongodb::Collection<mongodb::bson::Document>,
    config: &State<Config>,
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
    let base_url = format!("https://{}", config.host);
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


// --- Routes ---

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

#[post("/api/upload", data = "<data>")]
async fn api_upload_unified(
    _api_key: ApiKeyGuard,
    content_type: &ContentType,
    data: Data<'_>,
    collections: &State<db::Collections>,
    config: &State<Config>,
) -> Result<Json<ApiResponse>, Custom<Json<ApiErrorResponse>>> {
    // --- 1. Handle JSON Body ---
    if content_type.is_json() {
        let string_data = data.open(32.megabytes()).into_string().await
            .map_err(|_| create_error(Status::BadRequest, "Failed to read request body as string"))?;
            
        if !string_data.is_complete() {
             return Err(create_error(Status::PayloadTooLarge, "JSON data is too large."));
        }

        let req: ApiUploadRequest = rocket::serde::json::from_str(string_data.into_inner().as_str())
            .map_err(|_| create_error(Status::BadRequest, "Invalid JSON format."))?;

        if let Some(b64) = req.base64 {
            return process_text_upload(b64, &collections.images, config).await;
        }
        if let Some(url) = req.url {
            let (image_bytes, ct) = download_image_from_url(&url).await
                .map_err(|e| create_error(Status::BadRequest, &e))?;
            return process_and_respond(image_bytes, &ct, &collections.images, config).await;
        }
        return Err(create_error(Status::BadRequest, "Missing 'base64' or 'url' in JSON."));
    }

    // --- 2. Handle Multipart Form Data ---
    if content_type.is_form_data() {
        let options = MultipartFormDataOptions::with_multipart_form_data_fields(vec![
            MultipartFormDataField::file("image").content_type_by_string(Some(mime::STAR_STAR)).unwrap(),
            MultipartFormDataField::text("image"),
        ]);

        match MultipartFormData::parse(content_type, data, options).await {
            Ok(form_data) => {
                // Prioritize the file field if it exists
                if let Some(files) = form_data.files.get("image") {
                    if let Some(file) = files.first() {
                        let image_bytes = tokio::fs::read(&file.path).await.map_err(|_| {
                            create_error(Status::InternalServerError, "Could not read uploaded file")
                        })?;
                        let ct = file.content_type.as_ref().map(|ct| ct.to_string())
                            .unwrap_or_else(|| {
                                infer::get(&image_bytes).map(|k| k.mime_type().to_string())
                                .unwrap_or_else(|| "application/octet-stream".to_string())
                            });
                        return process_and_respond(image_bytes, &ct, &collections.images, config).await;
                    }
                }
                // Fallback to a text field (for Base64/URL)
                if let Some(texts) = form_data.texts.get("image") {
                    if let Some(text_field) = texts.first() {
                        return process_text_upload(text_field.text.clone(), &collections.images, config).await;
                    }
                }
                return Err(create_error(Status::BadRequest, "Missing 'image' field in multipart form."));
            }
            Err(_) => {
                let error_message = "Failed to parse multipart/form-data. This often happens if the 'Content-Type' header is missing a boundary or is malformed.";
                return Err(create_error(Status::BadRequest, error_message));
            }
        };
    }

    // --- 3. Handle URL-Encoded Form (often used for Base64) ---
    if content_type.is_form() {
        let form_str = data.open(32.megabytes()).into_string().await
            .map_err(|_| create_error(Status::BadRequest, "Failed to read form body"))?;

        if !form_str.is_complete() {
            return Err(create_error(Status::PayloadTooLarge, "Form data is too large."));
        }

        // FIX: Bind the owned String to a variable to extend its lifetime.
        let form_body = form_str.into_inner();
        
        // Now, `form_result` borrows from `form_body`, which lives until the end of the `if` block.
        // We can pass a reference `&form_body` directly; Rust coerces `&String` to `&str`.
        let form_result: Result<UrlencodedUpload, _> = Form::parse(&form_body);
        
        return match form_result {
            Ok(form_content) => process_text_upload(form_content.image, &collections.images, config).await,
            Err(_) => Err(create_error(Status::BadRequest, "Failed to parse URL-encoded form."))
        }
    }
    
    // --- 4. ULTIMATE FALLBACK: Treat the entire body as raw image data ---
    let raw_body = data.open(32.megabytes()).into_bytes().await.map_err(|_| {
        create_error(Status::BadRequest, "Failed to read request body")
    })?.into_inner();

    if raw_body.is_empty() {
        return Err(create_error(Status::BadRequest, "No image data received."));
    }

    let ct = infer::get(&raw_body)
        .map(|kind| kind.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    process_and_respond(raw_body, &ct, &collections.images, config).await
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

#[get("/delete/<id>/<token>")]
async fn get_delete_confirmation_page(
    id: String,
    token: String,
    collections: &State<db::Collections>,
) -> Result<RawHtml<String>, status::NotFound<String>> {
    db::get_image_with_token(&collections.images, &id, &token)
        .await
        .map_err(|_| status::NotFound("Database error".to_string()))?
        .ok_or_else(|| status::NotFound("Image not found or token incorrect".to_string()))?;

    let html_content = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Confirm Deletion</title>
            <style>
                body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; text-align: center; margin-top: 50px; background-color: #f4f4f4; color: #333; }}
                .container {{ background-color: white; padding: 30px; border-radius: 8px; box-shadow: 0 4px 8px rgba(0,0,0,0.1); display: inline-block; }}
                h1 {{ color: #d9534f; }}
                img {{ max-width: 250px; max-height: 250px; border: 1px solid #ddd; margin: 20px 0; border-radius: 4px; }}
                .buttons {{ display: flex; justify-content: center; gap: 15px; }}
                button, .cancel-btn {{ padding: 12px 24px; font-size: 16px; cursor: pointer; border-radius: 5px; text-decoration: none; display: inline-block; font-weight: bold; }}
                .delete-btn {{ background-color: #d9534f; color: white; border: none; }}
                .cancel-btn {{ background-color: #6c757d; color: white; border: none; }}
            </style>
        </head>
        <body>
            <div class="container">
                <h1>Are you sure?</h1>
                <p>You are about to permanently delete this image:</p>
                <img src="/i/{id}/thumb" alt="Image thumbnail">
                <p>This action cannot be undone.</p>
                <div class="buttons">
                    <form action="/delete/{id}/{token}" method="post" style="margin:0;">
                        <button type="submit" class="delete-btn">Yes, Delete Image</button>
                    </form>
                    <a href="/i/{id}" class="cancel-btn">Cancel</a>
                </div>
            </div>
        </body>
        </html>
        "#,
        id = id,
        token = token
    );

    Ok(RawHtml(html_content))
}

#[post("/delete/<id>/<token>")]
async fn post_delete_image_route(
    id: String,
    token: String,
    collections: &State<db::Collections>,
) -> Result<RawHtml<&'static str>, status::NotFound<String>> {
    match db::delete_image(&collections.images, &id, &token).await {
        Ok(result) => {
            if result.deleted_count == 1 {
                info!("Successfully deleted image {}", id);
                Ok(RawHtml(
                    r#"
                    <!DOCTYPE html>
                    <html lang="en">
                    <head>
                        <meta charset="UTF-8">
                        <title>Image Deleted</title>
                        <style>
                            body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; text-align: center; margin-top: 50px; background-color: #f4f4f4; color: #333; }}
                            .container {{ background-color: white; padding: 30px; border-radius: 8px; box-shadow: 0 4px 8px rgba(0,0,0,0.1); display: inline-block; }}
                            a {{ color: #0275d8; text-decoration: none; font-weight: bold; }}
                        </style>
                    </head>
                    <body>
                        <div class="container">
                            <h1>Image Deleted Successfully</h1>
                        </div>
                    </body>
                    </html>
                "#,
                ))
            } else {
                Err(status::NotFound(
                    "Image not found or token incorrect".to_string(),
                ))
            }
        }
        Err(e) => {
            error!("Error deleting image {}: {}", id, e);
            Err(status::NotFound("Error during deletion".to_string()))
        }
    }
}

#[catch(401)]
fn unauthorized() -> Json<ApiErrorResponse> {
    Json(ApiErrorResponse {
        error: "Unauthorized. The X-API-KEY header is missing.".to_string(),
        success: false,
        status: 401,
    })
}

#[catch(403)]
fn forbidden() -> Json<ApiErrorResponse> {
    Json(ApiErrorResponse {
        error: "Forbidden. The provided API key is invalid.".to_string(),
        success: false,
        status: 403,
    })
}

#[launch]
async fn rocket() -> _ {
    dotenv().ok();
    env_logger::init();

    let api_secret_key = env::var("API_SECRET_KEY")
        .expect("FATAL: API_SECRET_KEY environment variable not set.");

    let config = Config {
        host: env::var("HOST").unwrap_or("i.dishis.tech".to_string()),
        api_secret_key,
    };

    let figment = rocket::Config::figment()
        .merge(("limits", Limits::new().limit("bytes", 32.mebibytes().into())));

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

    build()
        .configure(figment)
        .manage(collections)
        .manage(config)
        .mount(
            "/",
            routes![
                index,
                api_upload_unified,
                view_image_route,
                redirect_image_route,
                view_thumbnail_route,
                get_delete_confirmation_page,
                post_delete_image_route
            ],
        )
        .register("/", catchers![unauthorized, forbidden])
}