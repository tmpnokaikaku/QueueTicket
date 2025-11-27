use axum::{
    extract::{Path, State, Request},
    http::{header::AUTHORIZATION, StatusCode, Method},  // 追加: Method
    middleware::{self, Next}, // ミドルウェア用に追加
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use askama::Template;
use base64::prelude::*;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::Deserialize;
use shuttle_runtime::SecretStore;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
use constant_time_eq::constant_time_eq;   // 追加

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    base_url: String,
    expected_auth_header: String, // 追加: 認証用の正解ヘッダー文字列
}

#[derive(FromRow, Clone)]
struct Ticket {
    id: Uuid,
    number: i32,
    group_size: i32,
    status: String,
}

// --- テンプレート定義 ---

#[derive(Template)]
#[template(path = "admin_index.html")]
struct AdminIndexTemplate;

#[derive(Template)]
#[template(path = "front.html")]
struct FrontTemplate {
    last_ticket: Option<Ticket>,
    qr_code: Option<String>,
}

#[derive(Template)]
#[template(path = "call.html")]
struct CallTemplate {
    tickets: Vec<Ticket>,
}

#[derive(Template)]
#[template(path = "guest.html")]
struct GuestTemplate {
    ticket: Ticket,
    waiting_count: i64,
}

#[derive(Template)]
#[template(path = "guest_content.html")]
struct GuestContentTemplate {
    ticket: Ticket,
    waiting_count: i64,
}

// --- ヘルパー ---
struct HtmlTemplate<T>(T);
impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to render template: {}", err),
            )
                .into_response(),
        }
    }
}

// QRコードSVG変換関数
fn to_svg_string(qr: &QrCode, border: i32) -> String {
    let mut res = String::new();
    let dim = qr.size();
    let brd = border;
    let width = dim + brd * 2;
    use std::fmt::Write;
    let _ = write!(res, "<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\" viewBox=\"0 0 {0} {0}\" stroke=\"none\">", width);
    res.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#FFFFFF\"/>");
    res.push_str("<path d=\"");
    for y in 0..dim {
        for x in 0..dim {
            if qr.get_module(x, y) {
                let _ = write!(res, "M{},{}h1v1h-1z ", x + brd, y + brd);
            }
        }
    }
    res.push_str("\" fill=\"#000000\"/></svg>");
    res
}

// --- Main ---
#[shuttle_runtime::main]
async fn main(
    #[shuttle_shared_db::Postgres] pool: PgPool,
    #[shuttle_runtime::Secrets] secret_store: SecretStore
) -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&pool).await.expect("Migrations failed");

    // 設定取得
    let base_url = secret_store
        .get("BASE_URL")
        .unwrap_or_else(|| "http://localhost:8000".to_string());

    let admin_password = secret_store
        .get("ADMIN_PASSWORD")
        .expect("ADMIN_PASSWORD must be set in Secrets.toml");

    // Basic認証のヘッダー値を作成 ("Basic " + Base64("admin:password"))
    let credentials = format!("admin:{}", admin_password);
    let encoded_credentials = BASE64_STANDARD.encode(credentials);
    let expected_auth_header = format!("Basic {}", encoded_credentials);

    // Stateの初期化
    let state = AppState { 
        pool, 
        base_url, 
        expected_auth_header // Stateに保存しておく
    };

    // --- ルーティングの構築 ---
    
    // 1. 公開エリア (ゲスト画面用) + ルートリダイレクト
    let public_routes = Router::new()
        .route("/", get(root_redirect))
        .route("/guest/{id}", get(guest_page))
        .route("/guest/{id}/content", get(guest_content));

    // 2. 管理者エリア (認証が必要)
    let admin_routes = Router::new()
        .route("/admin", get(admin_index))
        .route("/admin/reset", post(reset_db))
        .route("/admin/front", get(front_page))
        .route("/admin/front/tickets", post(create_ticket))
        .route("/admin/call", get(call_page))
        .route("/admin/call/update", post(update_status))
        // ここで認証ミドルウェアを適用
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    // 3. 全体をマージ
    let app = Router::new()
        .merge(public_routes)
        .merge(admin_routes)
        .with_state(state);

    Ok(app.into())
}

// --- 認証ミドルウェア (セキュリティ強化版) ---
async fn auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    // 1. Basic認証チェック (タイミング攻撃対策済み)
    let auth_header = req.headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.as_bytes().into()); // バイト列として取得

    let is_authorized = match auth_header {
        Some(auth) => constant_time_eq(auth, state.expected_auth_header.as_bytes()),
        None => false,
    };

    if !is_authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"Admin Area\"")],
            "Unauthorized: Access Denied",
        ).into_response();
    }

    // 2. CSRF対策 (簡易版: Origin/Refererチェック)
    // データを書き換えるメソッド(POST, DELETE等)の場合、リクエスト元を確認する
    if req.method() == Method::POST || req.method() == Method::PUT || req.method() == Method::DELETE {
        let headers = req.headers();
        
        // OriginまたはRefererヘッダーを取得
        let origin = headers.get("Origin")
            .and_then(|v| v.to_str().ok())
            .or_else(|| headers.get("Referer").and_then(|v| v.to_str().ok()));

        // 環境変数の BASE_URL と前方一致するか確認
        // 例: "https://my-app.shuttle.rs" からのリクエストか？
        let is_valid_origin = match origin {
            Some(o) => o.starts_with(&state.base_url),
            None => false, // OriginもRefererもないPOSTリクエストは拒否
        };

        if !is_valid_origin {
            return (
                StatusCode::FORBIDDEN,
                "Forbidden: CSRF Check Failed (Invalid Origin)",
            ).into_response();
        }
    }

    // すべてのチェックを通過
    next.run(req).await
}

// --- ハンドラ ---
async fn root_redirect() -> impl IntoResponse {
    Redirect::to("/admin")
}

async fn reset_db(State(state): State<AppState>) -> impl IntoResponse {
    sqlx::query("TRUNCATE TABLE tickets")
        .execute(&state.pool)
        .await
        .expect("Failed to reset table");
    Redirect::to("/admin")
}

async fn admin_index() -> impl IntoResponse {
    HtmlTemplate(AdminIndexTemplate)
}

async fn front_page() -> impl IntoResponse {
    HtmlTemplate(FrontTemplate {
        last_ticket: None,
        qr_code: None,
    })
}

#[derive(Deserialize)]
struct CreateTicketForm {
    group_size: i32,
}

async fn create_ticket(
    State(state): State<AppState>,
    Form(form): Form<CreateTicketForm>,
) -> impl IntoResponse {
    let next_number: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(number), 0) + 1 FROM tickets")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(1);
    let number = if next_number > 999 { 1 } else { next_number };

    let ticket = sqlx::query_as::<_, Ticket>(
        "INSERT INTO tickets (number, group_size, status) 
         VALUES ($1, $2, 'waiting') 
         RETURNING id, number, group_size, status",
    )
    .bind(number)
    .bind(form.group_size)
    .fetch_one(&state.pool)
    .await
    .expect("Failed to create ticket");

    let url = format!("{}/guest/{}", state.base_url, ticket.id);
    let qr = QrCode::encode_text(&url, QrCodeEcc::Medium).unwrap();
    let svg = to_svg_string(&qr, 4);

    HtmlTemplate(FrontTemplate {
        last_ticket: Some(ticket),
        qr_code: Some(svg),
    })
}

async fn call_page(State(state): State<AppState>) -> impl IntoResponse {
    let tickets = sqlx::query_as::<_, Ticket>(
        "SELECT id, number, group_size, status FROM tickets 
         WHERE status != 'completed' 
         ORDER BY number ASC"
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or(vec![]);

    HtmlTemplate(CallTemplate { tickets })
}

#[derive(Deserialize)]
struct UpdateStatusForm {
    id: Uuid,
    status: String,
}

async fn update_status(
    State(state): State<AppState>,
    Form(form): Form<UpdateStatusForm>,
) -> impl IntoResponse {
    sqlx::query("UPDATE tickets SET status = $1 WHERE id = $2")
        .bind(form.status)
        .bind(form.id)
        .execute(&state.pool)
        .await
        .expect("Failed to update status");

    Redirect::to("/admin/call")
}

async fn guest_page(Path(id): Path<Uuid>, State(state): State<AppState>) -> impl IntoResponse {
    let ticket = sqlx::query_as::<_, Ticket>("SELECT id, number, group_size, status FROM tickets WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .expect("Ticket not found");

    let waiting_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets WHERE status = 'waiting' AND number < $1")
        .bind(ticket.number)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);

    HtmlTemplate(GuestTemplate { ticket, waiting_count })
}

async fn guest_content(Path(id): Path<Uuid>, State(state): State<AppState>) -> impl IntoResponse {
    let ticket = sqlx::query_as::<_, Ticket>("SELECT id, number, group_size, status FROM tickets WHERE id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
        .expect("Ticket not found");

    let waiting_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tickets WHERE status = 'waiting' AND number < $1")
        .bind(ticket.number)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);

    HtmlTemplate(GuestContentTemplate { ticket, waiting_count })
}
