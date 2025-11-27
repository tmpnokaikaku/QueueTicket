use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response}, // Redirectを追加
    routing::{get, post},
    Form, Router,
};
use askama::Template;
use qrcodegen::{QrCode, QrCodeEcc};
use serde::Deserialize;
use shuttle_runtime::SecretStore;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    base_url: String,
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
#[template(path = "admin_index.html")] // 総合メニュー
struct AdminIndexTemplate;

#[derive(Template)]
#[template(path = "front.html")] // 発券画面
struct FrontTemplate {
    last_ticket: Option<Ticket>,
    qr_code: Option<String>,
}

#[derive(Template)]
#[template(path = "call.html")] // 呼び出し管理画面
struct CallTemplate {
    tickets: Vec<Ticket>, // リストで渡す
}

#[derive(Template)]
#[template(path = "guest.html")] // 来場者画面
struct GuestTemplate {
    ticket: Ticket,
    waiting_count: i64,
}

#[derive(Template)]
#[template(path = "guest_content.html")] // 来場者画面(部品)
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

// main
#[shuttle_runtime::main]
async fn main(
    #[shuttle_shared_db::Postgres] pool: PgPool,
    #[shuttle_runtime::Secrets] secret_store: SecretStore
) -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&pool).await.expect("Migrations failed");

    let base_url = secret_store
        .get("BASE_URL")
        .unwrap_or_else(|| "http://localhost:8000".to_string());

    let state = AppState { pool, base_url };

    let app = Router::new()
        // --- 管理者総合 ---
        .route("/admin", get(admin_index))
        // --- 発券画面 (Front) ---
        .route("/admin/front", get(front_page))
        .route("/admin/front/tickets", post(create_ticket))
        // --- 呼び出し管理 (Call) ---
        .route("/admin/call", get(call_page))
        .route("/admin/call/update", post(update_status))
        // --- 来場者画面 (Guest) ---
        .route("/guest/{id}", get(guest_page))
        .route("/guest/{id}/content", get(guest_content))
        .with_state(state);

    Ok(app.into())
}

// --- ハンドラ ---

// 1. 管理者メニュー
async fn admin_index() -> impl IntoResponse {
    HtmlTemplate(AdminIndexTemplate)
}

// 2. 発券画面表示
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

// 3. 発券処理
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

    // 修正: state.base_url を使ってURLを生成
    let url = format!("{}/guest/{}", state.base_url, ticket.id);
    
    let qr = QrCode::encode_text(&url, QrCodeEcc::Medium).unwrap();
    let svg = to_svg_string(&qr, 4);

    HtmlTemplate(FrontTemplate {
        last_ticket: Some(ticket),
        qr_code: Some(svg),
    })
}

// 4. 呼び出し管理画面表示
async fn call_page(State(state): State<AppState>) -> impl IntoResponse {
    // 完了していないチケットを番号順に取得
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

// 5. ステータス更新処理
async fn update_status(
    State(state): State<AppState>,
    Form(form): Form<UpdateStatusForm>,
) -> impl IntoResponse {
    // ステータスを更新
    sqlx::query("UPDATE tickets SET status = $1 WHERE id = $2")
        .bind(form.status)
        .bind(form.id)
        .execute(&state.pool)
        .await
        .expect("Failed to update status");

    // 処理が終わったらリスト画面にリダイレクト
    Redirect::to("/admin/call")
}

// 6. 来場者画面 (以前と同じ)
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

// 7. 来場者画面(自動更新用) (以前と同じ)
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
