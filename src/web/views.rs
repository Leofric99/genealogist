use axum::response::Html;

const BASE: &str = include_str!("../../templates/base.html");
const NAVIGATION: &str = r#"<a href="/">Dashboard</a><a href="/errors">Errors</a><a href="/settings">Settings</a><a href="/audit">GEDCOM audit</a><button class="secondary" type="button" onclick="signOut()">Sign out</button>"#;

fn render(title: &str, body: &str, authenticated: bool) -> Html<String> {
    Html(
        BASE.replace("{{title}}", title)
            .replace("{{content}}", body)
            .replace(
                "{{navigation}}",
                if authenticated { NAVIGATION } else { "" },
            ),
    )
}

pub async fn dashboard() -> Html<String> {
    render(
        "Dashboard",
        include_str!("../../templates/dashboard.html"),
        true,
    )
}

pub async fn errors() -> Html<String> {
    render("Errors", include_str!("../../templates/errors.html"), true)
}

pub async fn login() -> Html<String> {
    render("Sign in", include_str!("../../templates/login.html"), false)
}

pub async fn settings() -> Html<String> {
    render(
        "Settings",
        include_str!("../../templates/settings.html"),
        true,
    )
}

pub async fn audit() -> Html<String> {
    render(
        "GEDCOM audit",
        include_str!("../../templates/audit.html"),
        true,
    )
}
