use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ORIGIN, RETRY_AFTER, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use moviebox_tui::providers::ProviderKind;
use moviebox_tui::providers::models::ProviderError;
use moviebox_tui::service::MovieBoxService;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const DEFAULT_RATE_LIMIT: u32 = 60;
const EXAMPLE_TOKEN: &str = "replace-with-a-long-random-secret";
const MAX_QUERY_CHARS: usize = 160;
const MAX_PAGE: usize = 100;
const MAX_CONNECTIONS: usize = 128;
const ALL_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::MovieBox,
    ProviderKind::FourKHdHub,
    ProviderKind::BdixCircleFtp,
    ProviderKind::BdixDhakaFlix,
    ProviderKind::Dramachi,
    ProviderKind::Addons,
];

type ApiBody = Full<Bytes>;

#[derive(Clone)]
struct ApiState {
    service: Arc<MovieBoxService>,
    token: Option<Arc<str>>,
    cors_origins: Arc<Vec<String>>,
    trust_proxy: bool,
    rate_limiter: Arc<RateLimiter>,
}

impl ApiState {
    fn from_env(service: Arc<MovieBoxService>) -> Result<Self, Box<dyn std::error::Error>> {
        let token = std::env::var("API_TOKEN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(Arc::<str>::from);
        if token.as_deref() == Some(EXAMPLE_TOKEN) {
            return Err(std::io::Error::other(
                "Replace the example API_TOKEN with a generated secret before starting.",
            )
            .into());
        }
        if token.is_none() {
            log::warn!("API_TOKEN is unset; API endpoints are unauthenticated");
        }

        let cors_origins = std::env::var("API_CORS_ORIGINS")
            .unwrap_or_else(|_| "*".to_owned())
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let cors_origins = if cors_origins.is_empty() {
            vec!["*".to_owned()]
        } else {
            cors_origins
        };

        let trust_proxy = std::env::var("API_TRUST_PROXY")
            .ok()
            .is_some_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            });

        let rate_limit = std::env::var("API_RATE_LIMIT_PER_MINUTE")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_RATE_LIMIT)
            .min(10_000);

        Ok(Self {
            service,
            token,
            cors_origins: Arc::new(cors_origins),
            trust_proxy,
            rate_limiter: Arc::new(RateLimiter::new(rate_limit)),
        })
    }

    fn allows_origin(&self, origin: &str) -> bool {
        self.cors_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    fn cors_origin_for<'a>(&self, origin: &'a str) -> Option<&'a str> {
        if self.cors_origins.iter().any(|allowed| allowed == "*") {
            Some("*")
        } else if self.cors_origins.iter().any(|allowed| allowed == origin) {
            Some(origin)
        } else {
            None
        }
    }
}

struct RateLimiter {
    per_minute: u32,
    clients: Mutex<HashMap<IpAddr, RateWindow>>,
}

struct RateWindow {
    started_at: Instant,
    requests: u32,
}

impl RateLimiter {
    fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn allows(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let Ok(mut clients) = self.clients.lock() else {
            return false;
        };

        if clients.len() >= 8_192 && !clients.contains_key(&address) {
            clients.retain(|_, window| now.duration_since(window.started_at) < RATE_WINDOW);
            if clients.len() >= 8_192 {
                return false;
            }
        }

        let window = clients.entry(address).or_insert(RateWindow {
            started_at: now,
            requests: 0,
        });
        if now.duration_since(window.started_at) >= RATE_WINDOW {
            window.started_at = now;
            window.requests = 0;
        }
        if window.requests >= self.per_minute {
            return false;
        }
        window.requests += 1;
        true
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|argument| argument == "--healthcheck") {
        return healthcheck();
    }

    moviebox_tui::logging::init();
    let address = listen_address()?;
    let service = Arc::new(MovieBoxService::new());
    let state = Arc::new(ApiState::from_env(service)?);
    let listener = TcpListener::bind(address).await?;
    let connection_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    log::info!("MovieBox read-only API listening on {address}");
    eprintln!("MovieBox API listening on {address}");
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log::info!("shutdown signal received");
                break;
            }
            accepted = listener.accept() => {
                let (stream, remote_address) = accepted?;
                let Ok(permit) = Arc::clone(&connection_slots).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, remote_address, state).await;
                });
            }
        }
    }

    moviebox_tui::logging::flush();
    Ok(())
}

fn listen_address() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let host = std::env::var("API_HOST").unwrap_or_else(|_| "0.0.0.0".to_owned());
    let ip = host.parse::<IpAddr>()?;
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_owned())
        .parse::<u16>()?;
    if port == 0 {
        return Err(std::io::Error::other("PORT must be between 1 and 65535").into());
    }
    Ok(SocketAddr::new(ip, port))
}

async fn serve_connection(stream: TcpStream, remote: SocketAddr, state: Arc<ApiState>) {
    let io = TokioIo::new(stream);
    let service = service_fn(move |request| {
        let state = Arc::clone(&state);
        async move {
            Ok::<_, Infallible>(handle_request(request, remote, state).await)
        }
    });

    let builder = http1::Builder::new();
    if let Err(error) = builder.keep_alive(false).serve_connection(io, service).await {
        log::debug!("HTTP connection closed: {error}");
    }
}

async fn handle_request(
    request: Request<Incoming>,
    remote: SocketAddr,
    state: Arc<ApiState>,
) -> Response<ApiBody> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let client_address = if state.trust_proxy {
        request
            .headers()
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
            .or_else(|| {
                request
                    .headers()
                    .get("x-real-ip")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<IpAddr>().ok())
            })
            .unwrap_or(remote.ip())
    } else {
        remote.ip()
    };
    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut response = if !state.rate_limiter.allows(client_address) {
        let mut response = error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Request limit reached; retry in a minute.",
        );
        response.headers_mut().insert(
            RETRY_AFTER,
            hyper::header::HeaderValue::from_static("60"),
        );
        response
    } else if origin
        .as_deref()
        .is_some_and(|request_origin| !state.allows_origin(request_origin))
    {
        error_response(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "This browser origin is not allowed.",
        )
    } else if method == Method::OPTIONS {
        empty_response(StatusCode::NO_CONTENT)
    } else if method != Method::GET {
        let mut response = error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Only GET and OPTIONS are supported.",
        );
        response.headers_mut().insert(
            hyper::header::ALLOW,
            hyper::header::HeaderValue::from_static("GET, OPTIONS"),
        );
        response
    } else if path == "/healthz" {
        route_request(&path, &query, &state).await
    } else if state.token.as_deref().is_some_and(|expected| {
        !authorization
            .as_deref()
            .and_then(bearer_token)
            .is_some_and(|received| constant_time_eq(received, expected))
    }) {
        error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid Bearer token is required.",
        )
    } else {
        route_request(&path, &query, &state).await
    };

    if let Some(origin) = origin.as_deref() {
        if let Some(allowed_origin) = state.cors_origin_for(origin) {
            if let Ok(value) = hyper::header::HeaderValue::from_bytes(allowed_origin.as_bytes()) {
                response
                    .headers_mut()
                    .insert(ACCESS_CONTROL_ALLOW_ORIGIN, value);
            }
            response
                .headers_mut()
                .insert(VARY, hyper::header::HeaderValue::from_static("Origin"));
            if method == Method::OPTIONS {
                response.headers_mut().insert(
                    ACCESS_CONTROL_ALLOW_METHODS,
                    hyper::header::HeaderValue::from_static("GET, OPTIONS"),
                );
                response.headers_mut().insert(
                    ACCESS_CONTROL_ALLOW_HEADERS,
                    hyper::header::HeaderValue::from_static("Authorization, Content-Type"),
                );
                response.headers_mut().insert(
                    ACCESS_CONTROL_MAX_AGE,
                    hyper::header::HeaderValue::from_static("600"),
                );
            }
        }
    }

    response
}

async fn route_request(path: &str, query: &str, state: &ApiState) -> Response<ApiBody> {
    if path == "/healthz" {
        return json_response(
            StatusCode::OK,
            json!({
                "status": "ok",
                "service": "moviebox-api",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    if path == "/api/v1/providers" {
        let providers = ALL_PROVIDERS
            .iter()
            .map(|provider| {
                json!({
                    "id": provider.cache_key(),
                    "name": provider.label(),
                    "capabilities": state.service.capabilities(*provider),
                })
            })
            .collect::<Vec<_>>();
        return json_response(StatusCode::OK, json!({"providers": providers}));
    }

    let params = parse_query(query);
    match path {
        "/api/v1/search" => search(&params, state).await,
        "/api/v1/suggest" => suggest(&params, state).await,
        "/api/v1/homepage" => homepage(&params, state).await,
        _ => {
            if let Some(resource) = path.strip_prefix("/api/v1/media/") {
                return media_details(resource, state).await;
            }
            error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                "No endpoint matches this path.",
            )
        }
    }
}

async fn search(params: &HashMap<String, String>, state: &ApiState) -> Response<ApiBody> {
    let query = match validated_query(params) {
        Ok(query) => query,
        Err(response) => return response,
    };
    let provider = match provider_parameter(params) {
        Ok(provider) => provider,
        Err(response) => return response,
    };
    let page = match page_parameter(params) {
        Ok(page) => page,
        Err(response) => return response,
    };

    match tokio::time::timeout(
        REQUEST_TIMEOUT,
        state.service.search_typed(provider, query, page),
    )
    .await
    {
        Ok(Ok(results)) => json_response(
            StatusCode::OK,
            json!({
                "provider": provider.cache_key(),
                "query": query,
                "page": page,
                "results": results,
            }),
        ),
        Ok(Err(error)) => provider_error_response(provider, error),
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The provider did not respond in time.",
        ),
    }
}

async fn suggest(params: &HashMap<String, String>, state: &ApiState) -> Response<ApiBody> {
    let query = match validated_query(params) {
        Ok(query) => query,
        Err(response) => return response,
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, state.service.suggest(query)).await {
        Ok(Ok(suggestions)) => json_response(
            StatusCode::OK,
            json!({"query": query, "suggestions": suggestions}),
        ),
        Ok(Err(_)) => error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "The provider could not complete the request.",
        ),
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The provider did not respond in time.",
        ),
    }
}

async fn homepage(params: &HashMap<String, String>, state: &ApiState) -> Response<ApiBody> {
    let page = match page_parameter(params) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let tab = params.get("tab").map(String::as_str).unwrap_or("2");
    if tab.is_empty()
        || tab.len() > 32
        || !tab
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_tab",
            "tab must contain 1 to 32 letters, numbers, dots, underscores, or hyphens.",
        );
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, state.service.homepage(tab, page)).await {
        Ok(Ok((results, metrics))) => json_response(
            StatusCode::OK,
            json!({"tab": tab, "page": page, "results": results, "metrics": metrics}),
        ),
        Ok(Err(_)) => error_response(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "The provider could not complete the request.",
        ),
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The provider did not respond in time.",
        ),
    }
}

async fn media_details(resource: &str, state: &ApiState) -> Response<ApiBody> {
    let mut segments = resource.split('/');
    let provider_name = segments.next().unwrap_or_default();
    let raw_id = segments.next().unwrap_or_default();
    if provider_name.is_empty() || raw_id.is_empty() || segments.next().is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_media_path",
            "Expected /api/v1/media/{provider}/{id}.",
        );
    }
    let Some(provider) = ProviderKind::parse(provider_name) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "Unknown provider. See /api/v1/providers.",
        );
    };
    let Ok(id) = percent_decode_str(raw_id).decode_utf8() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "The media id is not valid UTF-8.",
        );
    };
    if id.is_empty()
        || id.len() > 256
        || id.chars().any(char::is_control)
        || id.contains('/')
        || id.contains('\\')
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "The media id is invalid or too long.",
        );
    }

    match tokio::time::timeout(
        REQUEST_TIMEOUT,
        state.service.details_typed(provider, &id),
    )
    .await
    {
        Ok(Ok(details)) => json_response(StatusCode::OK, json!({"details": details})),
        Ok(Err(error)) => provider_error_response(provider, error),
        Err(_) => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The provider did not respond in time.",
        ),
    }
}

fn validated_query(params: &HashMap<String, String>) -> Result<&str, Response<ApiBody>> {
    let Some(query) = params.get("q").map(String::as_str) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "missing_query",
            "The q query parameter is required.",
        ));
    };
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "q must contain 1 to 160 characters.",
        ));
    }
    Ok(query)
}

fn provider_parameter(
    params: &HashMap<String, String>,
) -> Result<ProviderKind, Response<ApiBody>> {
    let provider_name = params
        .get("provider")
        .map(String::as_str)
        .unwrap_or("moviebox");
    ProviderKind::parse(provider_name).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "invalid_provider",
            "Unknown provider. See /api/v1/providers.",
        )
    })
}

fn page_parameter(params: &HashMap<String, String>) -> Result<usize, Response<ApiBody>> {
    let page = match params.get("page") {
        Some(value) => value.parse::<usize>().ok(),
        None => Some(1),
    };
    match page.filter(|page| (1..=MAX_PAGE).contains(page)) {
        Some(page) => Ok(page),
        None => Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_page",
            "page must be an integer between 1 and 100.",
        )),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn provider_error_response(provider: ProviderKind, error: ProviderError) -> Response<ApiBody> {
    let (status, code) = match &error {
        ProviderError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        ProviderError::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "provider_rate_limited"),
        ProviderError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "provider_unavailable"),
        ProviderError::Network(_) | ProviderError::Parsing(_) => {
            (StatusCode::BAD_GATEWAY, "upstream_error")
        }
    };
    error_response(status, code, &error.user_message(provider))
}

fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
        Some(token.trim())
    } else {
        None
    }
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn json_response(status: StatusCode, value: Value) -> Response<ApiBody> {
    let body = serde_json::to_vec(&value)
        .unwrap_or_else(|_| b"{\"error\":\"serialization_failed\"}".to_vec());
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        X_CONTENT_TYPE_OPTIONS,
        hyper::header::HeaderValue::from_static("nosniff"),
    );
    response
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response<ApiBody> {
    json_response(status, json!({"error": code, "message": message}))
}

fn empty_response(status: StatusCode) -> Response<ApiBody> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    response
}

fn healthcheck() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_owned())
        .parse::<u16>()?;
    let address = ("127.0.0.1", port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("could not resolve local healthcheck address"))?;
    let mut stream = StdTcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = BufReader::new(stream);
    let mut status_line = String::new();
    response.read_line(&mut status_line)?;
    if status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(std::io::Error::other("healthcheck endpoint did not return HTTP 200").into())
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_query_and_provider() {
        let params = parse_query("q=Le+Fabuleux+Destin&provider=moviebox&page=3");
        assert_eq!(params.get("q").map(String::as_str), Some("Le Fabuleux Destin"));
        assert!(matches!(provider_parameter(&params), Ok(ProviderKind::MovieBox)));
        assert_eq!(page_parameter(&params).ok(), Some(3));
    }

    #[test]
    fn rejects_invalid_pages() {
        let params = parse_query("page=0");
        assert!(page_parameter(&params).is_err());
    }

    #[test]
    fn requires_a_bearer_token() {
        assert_eq!(bearer_token("Bearer secret"), Some("secret"));
        assert_eq!(bearer_token("bearer secret"), Some("secret"));
        assert_eq!(bearer_token("Basic secret"), None);
        assert_eq!(bearer_token("Bearer "), None);
    }

    #[test]
    fn compares_tokens_without_content_early_exit() {
        assert!(constant_time_eq("secret-token", "secret-token"));
        assert!(!constant_time_eq("secret-token", "other-token"));
    }

    #[test]
    fn enforces_per_ip_rate_limit() {
        let limiter = RateLimiter::new(1);
        let address = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        assert!(limiter.allows(address));
        assert!(!limiter.allows(address));
    }
}
