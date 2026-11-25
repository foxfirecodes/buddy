//! An authenticated device reviews the short code. Only the requesting device
//! holds the secret needed to complete the sign-in.
use super::*;
use crate::device_code::{Claim, Issued, Review};
use axum::extract::ConnectInfo;

pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route("/v1/device-signins", post(start))
        .route("/v1/device-signins/review", post(review))
        .route("/v1/device-signins/approve", post(approve))
        .route("/v1/device-signins/poll", post(poll))
        .route("/v1/device-signins/finish", post(finish))
        .route("/v1/device-signins/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(2048))
        .layer(axum::middleware::map_response(async |mut r: Response| {
            r.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            r
        }))
}

fn gone() -> anyhow::Error {
    denied(
        StatusCode::GONE,
        "This device code expired or was cancelled. Get a new code on the device signing in.",
    )
}

async fn start(
    State(s): State<Store>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(device): Json<String>,
) -> ApiResult<Json<Issued>> {
    database(move || {
        ensure!(
            !device.trim().is_empty()
                && device.len() <= 120
                && !device.chars().any(char::is_control),
            "choose a device name of up to 120 bytes"
        );
        let mut db = s.connection()?;
        limit_attempts(&db, &format!("device-start/{}", peer.ip()), 10)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM device_signins WHERE expires<=?", [now()])?;
        let active: u32 = tx.query_row("SELECT count(*) FROM device_signins", [], |r| r.get(0))?;
        if active >= 256 {
            return Err(denied(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many devices are waiting. Try again in a few minutes.",
            ));
        }
        let secret = format!("{}{}", id(), id());
        let expires_in = 600;
        loop {
            let code = format!("{:08}", rand::random_range(0..100_000_000u32));
            let inserted = tx.execute(
                "INSERT INTO device_signins(request,secret_hash,code,device,expires)
                 VALUES(?,?,?,?,?) ON CONFLICT(code) DO NOTHING",
                params![
                    id(),
                    digest(secret.as_bytes()),
                    code,
                    device.trim(),
                    now() + expires_in
                ],
            )?;
            if inserted == 1 {
                tx.commit()?;
                return Ok(Json(Issued {
                    secret,
                    code,
                    expires_in,
                }));
            }
        }
    })
    .await
}

async fn review(
    State(s): State<Store>,
    headers: HeaderMap,
    Json(code): Json<String>,
) -> ApiResult<Json<Review>> {
    database(move || {
        let db = s.connection()?;
        let user = who(&db, &bearer(&headers))?;
        limit_attempts(&db, &format!("device-review/{}", user.account), 5)?;
        ensure!(code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit()), "enter all eight digits");
        db.query_row("SELECT request,device FROM device_signins WHERE code=? AND expires>? AND approver IS NULL",
            params![code, now()], |r| Ok(Json(Review { request: r.get(0)?, device: r.get(1)? })))
            .optional()?.ok_or_else(|| denied(StatusCode::NOT_FOUND, "No waiting device has that code. Check the digits or get a new code."))
    }).await
}

async fn approve(
    State(s): State<Store>,
    headers: HeaderMap,
    Json(request): Json<String>,
) -> ApiResult<StatusCode> {
    database(move || {
        let mut db = s.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let user = who(&tx, &bearer(&headers))?;
        if tx.execute("UPDATE device_signins SET approver=?1 WHERE request=?2 AND expires>?3 AND (approver IS NULL OR approver=?1)",
            params![user.device, request, now()])? != 1 { return Err(gone()) }
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    }).await
}

fn secret_hash(headers: &HeaderMap) -> Result<String> {
    let secret = bearer(headers);
    if !hash_ok(&secret) {
        return Err(gone());
    }
    Ok(digest(secret.as_bytes()))
}

async fn poll(State(s): State<Store>, headers: HeaderMap) -> ApiResult<Json<Option<String>>> {
    database(move || {
        s.connection()?.query_row("SELECT d.account FROM device_signins p LEFT JOIN devices d ON d.id=p.approver WHERE secret_hash=? AND expires>?",
            params![secret_hash(&headers)?, now()], |r| r.get(0)).optional()?.map(Json).ok_or_else(gone)
    }).await
}

async fn finish(
    State(s): State<Store>,
    headers: HeaderMap,
    Json(claim): Json<Claim>,
) -> ApiResult<StatusCode> {
    database(move || {
        ensure!(
            hash_ok(&claim.token_hash) && key(&claim.account),
            "invalid device credentials"
        );
        let mut db = s.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let device: String = tx
            .query_row(
                "DELETE FROM device_signins WHERE secret_hash=? AND expires>? AND approver IN
            (SELECT id FROM devices WHERE account=?) RETURNING device",
                params![secret_hash(&headers)?, now(), claim.account],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(gone)?;
        tx.execute(
            "INSERT INTO devices(id,account,name,token) VALUES(?,?,?,?)",
            params![id(), claim.account, device, claim.token_hash],
        )?;
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn cancel(State(s): State<Store>, headers: HeaderMap) -> ApiResult<StatusCode> {
    database(move || {
        s.connection()?.execute(
            "DELETE FROM device_signins WHERE secret_hash=?",
            [secret_hash(&headers)?],
        )?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
