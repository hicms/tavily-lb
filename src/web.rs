use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Redirect, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

pub async fn redirect_to_index() -> Redirect {
    Redirect::permanent("/admin/")
}

pub async fn index() -> Response {
    serve_index()
}

pub async fn asset(Path(path): Path<String>) -> Response {
    if let Some(resp) = serve_file(&path) {
        return resp;
    }
    // Hashed bundle files must 404 when missing; any other path is a client-side route.
    if path.starts_with("assets/") || path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    serve_index()
}

fn serve_index() -> Response {
    serve_file("index.html").unwrap_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Admin UI is not built. Run `pnpm install && pnpm build` in web/ and rebuild.",
        )
            .into_response()
    })
}

fn serve_file(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (CONTENT_TYPE, mime.as_ref().to_string()),
                (CACHE_CONTROL, cache.to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
    )
}
