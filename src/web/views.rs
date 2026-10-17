use axum::response::Html;

const BASE: &str = include_str!("../../templates/base.html");

fn render(title: &str, body: &str) -> Html<String> {
    Html(
        BASE.replace("{{title}}", title)
            .replace("{{content}}", body),
    )
}

pub async fn dashboard() -> Html<String> {
    render("Dashboard", include_str!("../../templates/dashboard.html"))
}

pub async fn settings() -> Html<String> {
    render("Settings", include_str!("../../templates/settings.html"))
}

pub async fn audit() -> Html<String> {
    render("GEDCOM audit", include_str!("../../templates/audit.html"))
}
