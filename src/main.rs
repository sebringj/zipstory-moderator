use actix_web::{post, web, HttpRequest, HttpResponse, HttpServer, Responder};
use base64::engine::general_purpose;
use base64::Engine;
use dotenv::dotenv;
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs as stdfs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::{NamedTempFile, TempDir};
use url::Url;
use tokio::time::sleep;

#[derive(Deserialize)]
struct RequestBody {
    url: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ModerationResult {
    description: String,
    rating: String, // "G", "PG", "PG-13", "R", or "Inappropriate"
}

#[derive(Serialize)]
struct FrameInfo {
    frame: String,
    status: String,
    moderation: ModerationResult,
}

fn extract_frame_number(path: &str) -> Option<u32> {
    let file_stem = Path::new(path).file_stem()?.to_string_lossy();
    let parts: Vec<&str> = file_stem.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    parts[1].parse().ok()
}

async fn get_frame_moderation(frame_path: &str) -> Result<ModerationResult, Box<dyn std::error::Error>> {
    let api_key = env::var("GROK_API_KEY")?;
    let file_bytes = tokio::fs::read(frame_path).await?;
    let base64_image = general_purpose::STANDARD.encode(&file_bytes);
    let data_url = format!("data:image/jpeg;base64,{}", base64_image);

    let messages = vec![
        json!({
            "role": "system",
            "content": "You are an image moderator. Analyze the image and return a JSON object with exactly two fields: 'description' (a concise analysis) and 'rating' (one of 'G', 'PG', 'PG-13', 'R', or 'Inappropriate'). Your response must be strictly valid JSON without any additional text."
        }),
        json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {
                    "url": data_url,
                    "detail": "high"
                }
            }]
        }),
    ];

    let payload = json!({
        "model": "grok-2-vision-latest",
        "messages": messages,
        "temperature": 0.7
    });

    println!("Grok API Payload:\n{}", serde_json::to_string_pretty(&payload)?);

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.x.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&payload)
        .send()
        .await?;

    let json_resp: serde_json::Value = resp.json().await?;
    println!("Grok API JSON response:\n{}", serde_json::to_string_pretty(&json_resp)?);

    let content = json_resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let cleaned = if content.starts_with("```") {
        content
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
            .to_string()
    } else {
        content.trim().to_string()
    };

    println!("Cleaned moderation response:\n{}", cleaned);

    let moderation: ModerationResult = serde_json::from_str(&cleaned)
        .unwrap_or(ModerationResult {
            description: format!("Parsing error in response: {}", cleaned),
            rating: "Inappropriate".to_string(),
        });

    Ok(moderation)
}

async fn get_frame_moderation_with_retry(frame_path: &str, retries: u32, delay_ms: u64) -> ModerationResult {
    for attempt in 0..=retries {
        match get_frame_moderation(frame_path).await {
            Ok(moderation) => return moderation,
            Err(e) => {
                if attempt < retries {
                    sleep(Duration::from_millis(delay_ms)).await;
                } else {
                    return ModerationResult {
                        description: format!("Error: {} (after {} attempts)", e, attempt + 1),
                        rating: "Inappropriate".to_string(),
                    };
                }
            }
        }
    }
    ModerationResult {
        description: "Unexpected error".to_string(),
        rating: "Inappropriate".to_string(),
    }
}

#[post("/moderate")]
async fn moderate(req: HttpRequest, body: web::Json<RequestBody>) -> impl Responder {
    let expected_token = env::var("ZIPSTORY_TOKEN").unwrap_or_default();
    let token = req.headers().get("zipstory-token").and_then(|v| v.to_str().ok());
    if token != Some(expected_token.as_str()) {
        return HttpResponse::Unauthorized().finish();
    }

    let parsed_url = match Url::parse(&body.url) {
        Ok(url) => url,
        Err(e) => return HttpResponse::BadRequest().body(format!("Invalid URL: {}", e)),
    };

    let response = match reqwest::get(parsed_url.clone()).await {
        Ok(resp) => resp,
        Err(e) => return HttpResponse::BadRequest().body(format!("Error downloading URL: {}", e)),
    };

    if !response.status().is_success() {
        return HttpResponse::BadRequest()
            .body(format!("Failed to download URL. HTTP Status: {}", response.status()));
    }

    let bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Error reading response: {}", e)),
    };

    let mut tmp_file = match NamedTempFile::new() {
        Ok(file) => file,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Error creating temp file: {}", e)),
    };
    if let Err(e) = tmp_file.write_all(&bytes) {
        return HttpResponse::InternalServerError().body(format!("Error writing to temp file: {}", e));
    }

    let temp_dir = match TempDir::new() {
        Ok(dir) => dir,
        Err(e) => return HttpResponse::InternalServerError().body(format!("Error creating temp dir: {}", e)),
    };

    let debug_mode = env::var("DEBUG").unwrap_or_default().to_lowercase() == "true";
    let thumbnails_dir = PathBuf::from("./thumbnails");
    if debug_mode {
        let _ = stdfs::create_dir_all(&thumbnails_dir);
    }

    let filter = r"fps=1,scale=w='if(gt(iw,ih),300,-2)':h='if(gt(iw,ih),-2,300)'";
    let output_pattern = format!("{}/frame_%03d.jpg", temp_dir.path().to_string_lossy());

    let ffmpeg_status = Command::new("ffmpeg")
        .args([
            "-y",
            "-nostdin",
            "-i",
            tmp_file.path().to_str().unwrap(),
            "-vf",
            filter,
            &output_pattern,
        ])
        .status();

    match ffmpeg_status {
        Ok(status) if status.success() => {
            let mut frames: Vec<FrameInfo> = Vec::new();
            if let Ok(entries) = stdfs::read_dir(temp_dir.path()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(ext) = path.extension() {
                        if ext == "jpg" {
                            if debug_mode {
                                if let Some(fname) = path.file_name() {
                                    let mut dest = thumbnails_dir.clone();
                                    dest.push(fname);
                                    if let Err(e) = stdfs::copy(&path, &dest) {
                                        println!("Failed to copy thumbnail: {}", e);
                                    }
                                }
                            }
                            frames.push(FrameInfo {
                                frame: path.to_string_lossy().into_owned(),
                                status: "extracted".to_string(),
                                moderation: ModerationResult {
                                    description: "".to_string(),
                                    rating: "".to_string(),
                                },
                            });
                        }
                    }
                }
            }

            frames.sort_by_key(|f| extract_frame_number(&f.frame).unwrap_or(0));

            // Sequentially moderate frames with a delay between requests.
            let mut moderated_frames = Vec::new();
            for frame in frames {
                let moderation = get_frame_moderation_with_retry(&frame.frame, 3, 200).await;
                let mut moderated_frame = frame;
                moderated_frame.moderation = moderation;
                moderated_frames.push(moderated_frame);
                sleep(Duration::from_millis(200)).await;
            }

            HttpResponse::Ok().json(json!({
                "message": "File processed successfully - all frames moderated sequentially",
                "frames": moderated_frames,
            }))
        }
        Ok(status) => HttpResponse::InternalServerError().body(format!("ffmpeg failed with status: {}", status)),
        Err(e) => HttpResponse::InternalServerError().body(format!("Failed to execute ffmpeg: {}", e)),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv().ok();
    HttpServer::new(|| actix_web::App::new().service(moderate))
        .bind("0.0.0.0:8080")?
        .run()
        .await
}