//! Basic-auth middleware and the login help page.
use crate::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use subtle::ConstantTimeEq;

/// Require `username:password` Basic auth for every route except `/login`.
pub async fn basic_auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if req.uri().path() == "/login" {
        return Ok(next.run(req).await);
    }

    let expected = format!("{}:{}", state.config.username, state.config.password);

    if let Some(auth) = req.headers().get("authorization")
        && let Ok(auth_str) = auth.to_str()
        && let Some(base64_part) = auth_str.strip_prefix("Basic ")
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(base64_part)
        && bool::from(decoded.ct_eq(expected.as_bytes()))
    {
        return Ok(next.run(req).await);
    }

    Ok((
        StatusCode::UNAUTHORIZED,
        [("WWW-Authenticate", "Basic realm=\"Theia\"")],
    )
        .into_response())
}

/// Static page explaining how to authenticate.
pub async fn login_handler() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Theia</title></head>
<body>
<h1>Theia</h1>
<p>To access the library, visit <a href="/">/</a>. When prompted, sign in with the configured username and password (default: theia / theia).</p>
</body>
</html>"#;
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], html).into_response()
}
