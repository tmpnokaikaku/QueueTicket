use axum::{
    extract::{Path, State}, // Pathを追加
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Form, Router,
};
use askama::Template;
use qrcodegen::{QrCode, QrCodeEcc}; // QRコード用
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid; // UUID用

#[derive(Clone)]
struct AppState {
    pool: PgPool,
}

// DBのデータをマッピングする構造体
#[derive(FromRow, Clone)]
struct Ticket {
    id: Uuid, // IDを追加
    number: i32,
    group_size: i32,
    status: String, // ステータスを追加
}

// --- テンプレート定義 ---

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminTemplate {
    last_ticket: Option<Ticket>,
    qr_code: Option<String>, // QRコードのSVGデータ
}

#[derive(Template)]
#[template(path = "guest.html")]
struct GuestTemplate {
    ticket: Ticket,
    waiting_count: i64, // 前に待っている組数
}

#[derive(Template)]
#[template(path = "guest_content.html")] // 部品用のHTMLを指定
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

#[shuttle_runtime::main]
async fn main(#[shuttle_shared_db::Postgres] pool: PgPool) -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&pool).await.expect("Migrations failed");

    let state = AppState { pool };

    let app = Router::new()
        .route("/admin", get(admin_page))
        .route("/admin/tickets", post(create_ticket))
        .route("/guest/{id}", get(guest_page)) // 来場者用のルーティングを追加
        .route("/guest/{id}/content", get(guest_content))
        .with_state(state);

    Ok(app.into())
}

// --- ハンドラ ---
async fn admin_page() -> impl IntoResponse {
    HtmlTemplate(AdminTemplate {
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
    // 番号決定ロジック
    let next_number: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(number), 0) + 1 FROM tickets")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(1);
    let number = if next_number > 999 { 1 } else { next_number };

    // DB保存 (returning id を追加)
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

    // QRコード生成
    // 開発中は localhost:8000、本番では実際のドメインになりますが、
    // ここでは簡易的に相対パスでも動くように工夫するか、一旦固定で生成します。
    // スマホで読み取るためには本来 http://192.168.x.x:8000 などが必要ですが、
    // PC画面上のQRをスマホで読むシミュレーションとして、ここではURL文字列を作ります。
    let url = format!("http://localhost:8000/guest/{}", ticket.id);
    
    // QRコードをSVG文字列に変換
    let qr = QrCode::encode_text(&url, QrCodeEcc::Medium).unwrap();
    let svg = to_svg_string(&qr, 4); // 4は枠の太さ

    HtmlTemplate(AdminTemplate {
        last_ticket: Some(ticket),
        qr_code: Some(svg),
    })
}

// 来場者用ページ
async fn guest_page(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // 1. 自分のチケット情報を取得
    let ticket = sqlx::query_as::<_, Ticket>(
        "SELECT id, number, group_size, status FROM tickets WHERE id = $1"
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .expect("Ticket not found"); // 本来はエラーハンドリングすべき

    // 2. 自分の前に待っている人の数をカウント
    // (ステータスがwaitingで、かつ自分より番号が小さい人)
    let waiting_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tickets WHERE status = 'waiting' AND number < $1"
    )
    .bind(ticket.number)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    HtmlTemplate(GuestTemplate {
        ticket,
        waiting_count,
    })
}

// 自動更新用：中身のHTMLだけを返す
async fn guest_content(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // DBから最新情報を取得
    let ticket = sqlx::query_as::<_, Ticket>(
        "SELECT id, number, group_size, status FROM tickets WHERE id = $1"
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .expect("Ticket not found");

    let waiting_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tickets WHERE status = 'waiting' AND number < $1"
    )
    .bind(ticket.number)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // 部品テンプレートを返す
    HtmlTemplate(GuestContentTemplate {
        ticket,
        waiting_count,
    })
}

// QRコードのデータをSVG文字列に変換
fn to_svg_string(qr: &QrCode, border: i32) -> String {
    let mut res = String::new();
    let dim = qr.size();
    let brd = border;
    let width = dim + brd * 2;
    
    // SVGヘッダー
    use std::fmt::Write; // 文字列書き込み用
    let _ = write!(res, "<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\" viewBox=\"0 0 {0} {0}\" stroke=\"none\">", width);
    
    // 背景（白）
    res.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#FFFFFF\"/>");
    
    // 黒いドット部分
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
